use std::{
    collections::VecDeque,
    fs,
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};

use ahash::AHashMap;
use tokenizers::{models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace, Tokenizer};

use crate::{
    huggingface::HuggingFaceFileResolver,
    model_config::{
        MediaInputConfig, MediaInputTransport, ModelCapability, MultimodalInputConfig,
        ProviderType, RetryMode, TokenEstimatorType,
    },
    providers::{Provider, ProviderError, ProviderRequest},
    session_actor::{
        builtin_tool_catalog, tool_result_text, BuiltinToolCatalogOptions, ChatRole, ContextItem,
        FileItem, HostToolScope, SessionMailboxKind, TaskPlanItemView, ToolBatchError,
        ToolBatchHandle, ToolCallItem, ToolResultContent, ToolResultItem, COMPRESSION_MARKER,
    },
    test_support::temp_cwd,
};

use super::*;

impl SessionActor {
    fn step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        self.drain_ready_events()?;
        self.process_ready_step()
    }

    fn process_ready_step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        if self.shutdown {
            return Ok(SessionActorStep::Shutdown);
        }

        if let Some(control) = self.pending_control.pop_front() {
            self.log_info(
                "control_request",
                serde_json::json!({"request": session_request_kind(&control)}),
            );
            self.handle_control(control)?;
            return Ok(if self.shutdown {
                SessionActorStep::Shutdown
            } else {
                SessionActorStep::ProcessedControl
            });
        }

        if self.active_provider_request.is_some() {
            if let Some(event) = self.pending_provider_events.pop_front() {
                return self.handle_provider_event(event);
            }
            if self.has_pending_user_message() {
                if self.provider_supersede_grace_remaining().is_none() {
                    self.cancel_active_provider_request(
                        "superseded_by_user_message".to_string(),
                        false,
                    )?;
                } else {
                    self.schedule_provider_supersede_grace_event_if_needed();
                    return Ok(SessionActorStep::WaitingProviderRequest);
                }
            } else {
                return Ok(SessionActorStep::WaitingProviderRequest);
            }
        } else if !self.pending_provider_events.is_empty() {
            self.discard_stale_provider_events();
        }

        if self.active_tool_batch.is_some() {
            if self.has_pending_user_message_interrupt() {
                self.request_active_tool_interrupt(
                    ToolBatchInterrupt::SupersededByUserMessage,
                    "newer user message arrived".to_string(),
                )?;
            }
            if let Some(progress) = self.pending_tool_progress.pop_front() {
                return self.handle_tool_progress_event(progress);
            }
            if let Some(completion) = self.pending_tool_completions.pop_front() {
                return self.handle_tool_completion_event(completion);
            }
            return Ok(SessionActorStep::WaitingToolBatch);
        }

        self.run_pending_data_if_idle(SessionActorStep::Idle)
    }

    fn drain_ready_events(&mut self) -> Result<(), SessionActorError> {
        while let Ok(request) = self.request_rx.try_recv() {
            self.enqueue_request(request);
        }
        while let Ok(completion) = self.tool_completion_rx.try_recv() {
            self.pending_tool_completions.push_back(completion);
        }
        while let Ok(progress) = self.tool_progress_rx.try_recv() {
            self.pending_tool_progress.push_back(progress);
        }
        while let Ok(event) = self.provider_event_rx.try_recv() {
            self.pending_provider_events.push_back(event);
        }
        while let Ok(event) = self.internal_event_rx.try_recv() {
            let _ = self.handle_internal_event(event)?;
        }
        Ok(())
    }

    fn enqueue_request(&mut self, request: SessionRequest) {
        match request.mailbox_kind() {
            SessionMailboxKind::Control => self.pending_control.push_back(request),
            SessionMailboxKind::Data => {
                let is_user_message = matches!(request, SessionRequest::EnqueueUserMessage { .. });
                self.pending_data.push_back(request);
                if is_user_message {
                    self.schedule_provider_supersede_grace_event_if_needed();
                }
            }
        }
    }

    fn has_pending_user_message_interrupt(&self) -> bool {
        self.active_tool_batch
            .as_ref()
            .is_some_and(|active| active.interrupt.is_none())
            && self.has_pending_user_message()
    }

    fn discard_stale_provider_events(&mut self) {
        let count = self.pending_provider_events.len();
        self.pending_provider_events.clear();
        if count > 0 {
            self.log_warn(
                "stale_provider_events_discarded",
                serde_json::json!({ "count": count }),
            );
        }
    }

    fn run_until_idle(&mut self, max_steps: usize) -> Result<SessionActorStep, SessionActorError> {
        let mut counted_steps = 0usize;
        let mut provider_wait_steps = 0usize;
        while counted_steps < max_steps {
            let step = self.step()?;
            if matches!(step, SessionActorStep::Idle | SessionActorStep::Shutdown) {
                return Ok(step);
            }
            if matches!(step, SessionActorStep::WaitingProviderRequest) {
                provider_wait_steps = provider_wait_steps.saturating_add(1);
                if provider_wait_steps > max_steps.saturating_mul(1_000).max(1_000) {
                    return Err(SessionActorError::StepLimitExceeded(max_steps));
                }
                thread::yield_now();
                continue;
            }
            counted_steps = counted_steps.saturating_add(1);
        }

        Err(SessionActorError::StepLimitExceeded(max_steps))
    }

    fn has_ready_work(&self) -> bool {
        self.shutdown
            || !self.pending_control.is_empty()
            || (!self.pending_data.is_empty()
                && self.active_provider_request.is_none()
                && self.active_tool_batch.is_none())
            || (self.active_provider_request.is_some() && !self.pending_provider_events.is_empty())
            || (self.active_provider_request.is_some()
                && self.has_pending_user_message()
                && self.provider_supersede_grace_remaining().is_none())
            || (self.active_provider_request.is_none() && !self.pending_provider_events.is_empty())
            || (self.active_tool_batch.is_some() && !self.pending_tool_progress.is_empty())
            || (self.active_tool_batch.is_some() && !self.pending_tool_completions.is_empty())
            || (self.active_tool_batch.is_some() && self.has_pending_user_message_interrupt())
    }
}

struct MemoryActorMailbox {
    sender: SessionActorRequestSender,
}

impl MemoryActorMailbox {
    fn append(&self, kind: SessionMailboxKind, request: SessionRequest) {
        assert_eq!(kind, request.mailbox_kind());
        self.sender
            .send(request)
            .expect("test request channel should be open");
    }
}

fn test_inbox() -> (SessionActorInbox, MemoryActorMailbox) {
    let (inbox, sender) = SessionActorInbox::channel();
    (inbox, MemoryActorMailbox { sender })
}

fn step_until(actor: &mut SessionActor, expected: SessionActorStep, max_steps: usize, label: &str) {
    for _ in 0..max_steps {
        let step = actor.step().expect(label);
        if step == expected {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{label}: did not reach {expected:?}");
}

#[derive(Default)]
struct MemoryEventSink {
    events: Mutex<Vec<SessionEvent>>,
}

impl SessionActorEventSink for MemoryEventSink {
    fn emit(&self, event: SessionEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

struct ScriptedProvider {
    model_config: ModelConfig,
    provider_system_prompt: Option<String>,
    responses: Mutex<VecDeque<ChatMessage>>,
    seen_requests: Mutex<Vec<ProviderRequestSnapshot>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatMessage>) -> Self {
        Self {
            model_config: test_model_config(),
            provider_system_prompt: None,
            responses: Mutex::new(VecDeque::from(responses)),
            seen_requests: Mutex::new(Vec::new()),
        }
    }

    fn with_model_config(mut self, model_config: ModelConfig) -> Self {
        self.model_config = model_config;
        self
    }

    fn with_provider_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.provider_system_prompt = Some(system_prompt.into());
        self
    }
}

#[derive(Debug, Clone)]
struct ProviderRequestSnapshot {
    system_prompt: Option<String>,
    tool_names: Vec<String>,
    message_count: usize,
}

impl Provider for ScriptedProvider {
    fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    fn system_prompt_for_model(
        &self,
        _model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        Ok(self.provider_system_prompt.clone())
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        self.seen_requests
            .lock()
            .unwrap()
            .push(ProviderRequestSnapshot {
                system_prompt: request.system_prompt.map(str::to_string),
                tool_names: request.tools.iter().map(|tool| tool.name.clone()).collect(),
                message_count: request.messages.len(),
            });
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::EmptyChoices)
    }
}

fn compression_response(summary: &str) -> String {
    serde_json::json!({
        "summary": summary,
        "current_state": "",
        "plan": "",
        "preserved_tool_call_ids": [],
    })
    .to_string()
}

struct RequestTooLargeThenOkProvider {
    model_config: ModelConfig,
    calls: Mutex<usize>,
    seen_message_counts: Mutex<Vec<usize>>,
}

impl RequestTooLargeThenOkProvider {
    fn new() -> Self {
        Self {
            model_config: test_model_config(),
            calls: Mutex::new(0),
            seen_message_counts: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for RequestTooLargeThenOkProvider {
    fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        self.seen_message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            return Err(ProviderError::HttpStatus {
                url: "https://example.invalid/chat".to_string(),
                status: 413,
                body: r#"{"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#.to_string(),
            });
        }
        if *calls == 2 {
            return Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: compression_response("compressed after provider request too large"),
                })],
            ));
        }
        Ok(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "recovered".to_string(),
            })],
        ))
    }
}

struct RepeatedRequestTooLargeProvider {
    model_config: ModelConfig,
    normal_calls: Mutex<usize>,
    compression_calls: Mutex<usize>,
    normal_message_counts: Mutex<Vec<usize>>,
}

impl RepeatedRequestTooLargeProvider {
    fn new() -> Self {
        Self {
            model_config: test_model_config(),
            normal_calls: Mutex::new(0),
            compression_calls: Mutex::new(0),
            normal_message_counts: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for RepeatedRequestTooLargeProvider {
    fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        if provider_request_contains_text(&request, "Return strict JSON only") {
            *self.compression_calls.lock().unwrap() += 1;
            return Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: compression_response("compressed once"),
                })],
            ));
        }

        self.normal_message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        let mut normal_calls = self.normal_calls.lock().unwrap();
        *normal_calls += 1;
        if *normal_calls <= 2 {
            return Err(ProviderError::HttpStatus {
                url: "https://example.invalid/chat".to_string(),
                status: 413,
                body: r#"{"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#
                    .to_string(),
            });
        }

        Ok(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "recovered after pruning".to_string(),
            })],
        ))
    }
}

fn provider_request_contains_text(request: &ProviderRequest<'_>, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message.data.iter().any(|item| match item {
            ChatMessageItem::Context(context) => context.text.contains(needle),
            ChatMessageItem::ToolCall(tool_call) => tool_call.arguments.text.contains(needle),
            ChatMessageItem::ToolResult(tool_result) => {
                tool_result_text(tool_result).contains(needle)
            }
            ChatMessageItem::Compaction(compaction) => compaction
                .generic_summary_text()
                .is_some_and(|text| text.contains(needle)),
            ChatMessageItem::File(_)
            | ChatMessageItem::Reasoning(_)
            | ChatMessageItem::SelectionReference(_) => false,
        })
    })
}

struct TransientThenOkProvider {
    model_config: ModelConfig,
    failures_remaining: Mutex<usize>,
    calls: Mutex<usize>,
}

impl TransientThenOkProvider {
    fn new(failures: usize, max_retries: u64) -> Self {
        let mut model_config = test_model_config();
        model_config.retry_mode = RetryMode::RandomInterval {
            max_interval_secs: 1,
            max_retries,
        };
        Self {
            model_config,
            failures_remaining: Mutex::new(failures),
            calls: Mutex::new(0),
        }
    }
}

impl Provider for TransientThenOkProvider {
    fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    fn send(&self, _request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        let mut failures_remaining = self.failures_remaining.lock().unwrap();
        if *failures_remaining > 0 {
            *failures_remaining -= 1;
            return Err(ProviderError::WebSocket(
                "temporary websocket failure".to_string(),
            ));
        }
        Ok(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "recovered after transient errors".to_string(),
            })],
        ))
    }
}

struct EchoToolExecutor {
    batches: Mutex<Vec<ToolBatch>>,
}

impl EchoToolExecutor {
    fn new() -> Self {
        Self {
            batches: Mutex::new(Vec::new()),
        }
    }
}

impl ToolBatchExecutor for EchoToolExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        _progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        let handle = ToolBatchHandle::new(batch.batch_id.clone());
        let results = batch
            .operations
            .iter()
            .map(|operation| {
                let (tool_call_id, tool_name) = match &operation.item {
                    ToolBatchItem::RegisteredTool(tool_call)
                    | ToolBatchItem::UnsupportedTool { tool_call, .. } => {
                        (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
                    }
                };
                ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id,
                    tool_name,
                    result: ToolResultContent::from_text(format!(
                        "tool batch {} done",
                        handle.batch_id
                    )),
                })
            })
            .collect();
        self.batches.lock().unwrap().push(batch);
        let message = ChatMessage::new(ChatRole::Assistant, results);
        let _ = completion_tx.send(ToolBatchCompletion {
            batch_id: handle.batch_id.clone(),
            result: Ok(message),
        });
        Ok(handle)
    }

    fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        Ok(())
    }

    fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
        Ok(())
    }
}

struct ProgressEchoToolExecutor;

impl ToolBatchExecutor for ProgressEchoToolExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        let handle = ToolBatchHandle::new(batch.batch_id.clone());
        let results: Vec<ChatMessageItem> = batch
            .operations
            .iter()
            .map(|operation| {
                let (tool_call_id, tool_name) = match &operation.item {
                    ToolBatchItem::RegisteredTool(tool_call)
                    | ToolBatchItem::UnsupportedTool { tool_call, .. } => {
                        (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
                    }
                };
                let result = ToolResultItem {
                    tool_call_id,
                    tool_name,
                    result: ToolResultContent::from_text(format!(
                        "tool batch {} done",
                        handle.batch_id
                    )),
                };
                let _ = progress_tx.send(ToolBatchProgress {
                    batch_id: handle.batch_id.clone(),
                    result: result.clone(),
                });
                ChatMessageItem::ToolResult(result)
            })
            .collect();
        let message = ChatMessage::new(ChatRole::Assistant, results);
        let _ = completion_tx.send(ToolBatchCompletion {
            batch_id: handle.batch_id.clone(),
            result: Ok(message),
        });
        Ok(handle)
    }

    fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        Ok(())
    }

    fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
        Ok(())
    }
}

struct MediaFileToolExecutor;

impl ToolBatchExecutor for MediaFileToolExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        _progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        let handle = ToolBatchHandle::new(batch.batch_id);
        let _ = completion_tx.send(ToolBatchCompletion {
            batch_id: handle.batch_id.clone(),
            result: Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "image_view".to_string(),
                    result: ToolResultContent::from_text("loaded image".to_string()).with_file(
                        FileItem {
                            uri: "file:///tmp/test.png".to_string(),
                            name: Some("test.png".to_string()),
                            media_type: Some("image/png".to_string()),
                            width: None,
                            height: None,
                            state: None,
                        },
                    ),
                })],
            )),
        });
        Ok(handle)
    }

    fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        Ok(())
    }

    fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
        Ok(())
    }
}

struct BlockingToolExecutor {
    started_tx: Mutex<Option<mpsc::Sender<()>>>,
    release_rx: Mutex<Option<mpsc::Receiver<()>>>,
    interrupt_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl BlockingToolExecutor {
    fn new(started_tx: mpsc::Sender<()>, release_rx: mpsc::Receiver<()>) -> Self {
        Self::with_interrupt_tx(started_tx, release_rx, None)
    }

    fn with_interrupt_tx(
        started_tx: mpsc::Sender<()>,
        release_rx: mpsc::Receiver<()>,
        interrupt_tx: Option<mpsc::Sender<()>>,
    ) -> Self {
        Self {
            started_tx: Mutex::new(Some(started_tx)),
            release_rx: Mutex::new(Some(release_rx)),
            interrupt_tx: Mutex::new(interrupt_tx),
        }
    }
}

impl ToolBatchExecutor for BlockingToolExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        _progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        let handle = ToolBatchHandle::new(batch.batch_id);
        if let Some(started_tx) = self.started_tx.lock().unwrap().take() {
            let _ = started_tx.send(());
        }
        let release_rx = self.release_rx.lock().unwrap().take().ok_or_else(|| {
            ToolBatchError::Start("blocking executor already started".to_string())
        })?;
        let batch_id = handle.batch_id.clone();
        std::thread::spawn(move || {
            if release_rx.recv().is_ok() {
                let _ = completion_tx.send(ToolBatchCompletion {
                    batch_id,
                    result: Ok(ChatMessage::new(
                        ChatRole::Assistant,
                        vec![ChatMessageItem::ToolResult(ToolResultItem {
                            tool_call_id: "call_1".to_string(),
                            tool_name: "cron_tasks_list".to_string(),
                            result: ToolResultContent::from_text("tool result".to_string()),
                        })],
                    )),
                });
            }
        });
        Ok(handle)
    }

    fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        if let Some(interrupt_tx) = self.interrupt_tx.lock().unwrap().take() {
            let _ = interrupt_tx.send(());
        }
        Ok(())
    }

    fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
        Ok(())
    }
}

struct FailingCompletionToolExecutor;

impl ToolBatchExecutor for FailingCompletionToolExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        _progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        let handle = ToolBatchHandle::new(batch.batch_id);
        let _ = completion_tx.send(ToolBatchCompletion {
            batch_id: handle.batch_id.clone(),
            result: Err("simulated tool batch failure".to_string()),
        });
        Ok(handle)
    }

    fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        Ok(())
    }

    fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
        Ok(())
    }
}

fn test_session_id(prefix: &str) -> String {
    format!(
        "{}_{}_{}",
        prefix,
        std::process::id(),
        rand::random::<u64>()
    )
}

fn test_model_config() -> ModelConfig {
    ModelConfig {
        provider_type: ProviderType::OpenRouterCompletion,
        model_name: "openai/gpt-4o-mini".to_string(),
        url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
        api_key_env: "OPENROUTER_API_KEY".to_string(),
        capabilities: vec![ModelCapability::Chat],
        token_max_context: 128_000,
        max_tokens: 0,
        cache_timeout: 300,
        conn_timeout: 10,
        request_timeout: 600,
        max_request_size: 30 * 1024 * 1024,
        retry_mode: RetryMode::Once,
        reasoning: None,
        token_estimator_type: TokenEstimatorType::Local,
        multimodal_estimator: None,
        multimodal_input: None,
        token_estimator_url: None,
    }
}

fn test_model_config_with_tokenizer() -> (ModelConfig, std::path::PathBuf) {
    let mut vocab = AHashMap::new();
    vocab.insert("[UNK]".to_string(), 0);
    vocab.insert("user".to_string(), 1);
    vocab.insert("assistant".to_string(), 2);
    vocab.insert("old".to_string(), 3);
    vocab.insert("first".to_string(), 4);
    vocab.insert("second".to_string(), 5);
    vocab.insert("final".to_string(), 6);
    vocab.insert("summary".to_string(), 7);

    let model = WordLevel::builder()
        .vocab(vocab)
        .unk_token("[UNK]".to_string())
        .build()
        .expect("word level should build");
    let mut tokenizer = Tokenizer::new(model);
    tokenizer.with_pre_tokenizer(Some(Whitespace));

    let directory = std::env::temp_dir().join(format!(
        "stellaclaw-actor-compression-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    fs::create_dir_all(&directory).expect("directory should exist");
    tokenizer
        .save(directory.join("tokenizer.json"), false)
        .expect("tokenizer should save");
    fs::write(
        directory.join("tokenizer_config.json"),
        r#"{
            "chat_template": "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            "bos_token": "<s>",
            "eos_token": "</s>"
        }"#,
    )
    .expect("tokenizer config should save");

    let mut config = test_model_config();
    config.token_estimator_type = TokenEstimatorType::HuggingFace;
    config.token_estimator_url = Some(
        directory
            .join("tokenizer_config.json")
            .to_string_lossy()
            .to_string(),
    );

    let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
    TokenEstimator::from_model_config(&config, &resolver).expect("tokenizer should load");

    (config, directory)
}

#[test]
fn active_compression_threshold_is_capped_by_model_context() {
    let mut model_config = test_model_config();
    model_config.token_max_context = 200_000;
    let mut initial = SessionInitial::new(
        test_session_id("session_compression_threshold"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(235_929);

    assert_eq!(
        active_compression_threshold_tokens(&model_config, &initial),
        Some(180_000)
    );

    initial.compression_threshold_tokens = Some(120_000);
    assert_eq!(
        active_compression_threshold_tokens(&model_config, &initial),
        Some(120_000)
    );

    initial.compression_threshold_tokens = None;
    assert_eq!(
        active_compression_threshold_tokens(&model_config, &initial),
        None
    );
}

#[test]
fn default_retain_recent_tokens_uses_clawparty_ratio() {
    assert_eq!(default_retain_recent_tokens(235_929), 23_592);
    assert_eq!(default_retain_recent_tokens(200_000), 20_000);
    assert_eq!(default_retain_recent_tokens(1_000), 512);
    assert_eq!(default_retain_recent_tokens(2), 1);
}

#[test]
fn request_too_large_prune_start_preserves_tool_call_result_pairs() {
    let messages = vec![
        text_message(ChatRole::User, "old user"),
        text_message(ChatRole::Assistant, "old assistant"),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "shell_exec".to_string(),
                arguments: ContextItem {
                    text: r#"{"path":"README.md"}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "shell_exec".to_string(),
                result: ToolResultContent::from_text("contents".to_string()),
            })],
        ),
        text_message(ChatRole::Assistant, "after tool"),
        text_message(ChatRole::User, "new user"),
    ];

    assert_eq!(request_too_large_prune_start(&messages), Some(4));
}

#[test]
fn runs_user_turn_without_tools() {
    let _cwd = temp_cwd("actor-runs-user-turn");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_runs_user_turn");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id.clone(), super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "hi".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    let step = actor.run_until_idle(4).expect("actor should run");

    assert_eq!(step, SessionActorStep::Idle);
    assert_eq!(actor.initial().unwrap().session_id, session_id);
    assert_eq!(actor.history().len(), 2);
    assert!(actor.history()[1].message_time.is_some());
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
    let seen_requests = provider.seen_requests.lock().unwrap();
    assert_eq!(seen_requests.len(), 1);
    assert!(seen_requests[0]
        .system_prompt
        .as_ref()
        .unwrap()
        .contains("Session kind: foreground"));
    assert!(seen_requests[0]
        .tool_names
        .contains(&"shell_exec".to_string()));
    assert_eq!(seen_requests[0].message_count, 1);
}

#[test]
fn starts_turn_with_all_current_pending_user_messages() {
    let _cwd = temp_cwd("actor-batches-pending-user-messages");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_batches_pending_user_messages"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: text_message(ChatRole::User, "first"),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: text_message(ChatRole::User, "second"),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![text_message(
        ChatRole::Assistant,
        "done",
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events,
        catalog,
    );

    let step = actor.run_until_idle(4).expect("actor should run");

    assert_eq!(step, SessionActorStep::Idle);
    assert_eq!(actor.history().len(), 3);
    assert_eq!(actor.history()[0].role, ChatRole::User);
    assert_eq!(actor.history()[1].role, ChatRole::User);
    assert_eq!(message_text_for_test(&actor.history()[0]), "first");
    assert_eq!(message_text_for_test(&actor.history()[1]), "second");
    let seen_requests = provider.seen_requests.lock().unwrap();
    assert_eq!(seen_requests.len(), 1);
    assert_eq!(seen_requests[0].message_count, 2);
}

#[test]
fn provider_system_prompt_replaces_common_prompt_section() {
    let _cwd = temp_cwd("actor-provider-system-prompt");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_provider_system_prompt"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(
        ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hi".to_string(),
            })],
        )])
        .with_provider_system_prompt("provider native instructions"),
    );
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events,
        catalog,
    );

    let step = actor.run_until_idle(4).expect("actor should run");

    assert_eq!(step, SessionActorStep::Idle);
    let seen_requests = provider.seen_requests.lock().unwrap();
    let system_prompt = seen_requests[0].system_prompt.as_ref().unwrap();
    assert!(system_prompt.starts_with("provider native instructions"));
    assert!(system_prompt.contains("Session kind: foreground"));
    assert!(!system_prompt.contains("You are StellaClaw"));
}

#[test]
fn transient_provider_errors_retry_using_model_retry_mode() {
    let _cwd = temp_cwd("actor-transient-provider-retry-recovers");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_transient_retry_recovers"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(TransientThenOkProvider::new(2, 2));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    actor.run_until_idle(8).expect("actor should recover");

    assert_eq!(*provider.calls.lock().unwrap(), 3);
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
    assert_eq!(
        message_text_for_test(actor.history().last().unwrap()),
        "recovered after transient errors"
    );
}

#[test]
fn transient_provider_errors_stop_after_retry_budget() {
    let _cwd = temp_cwd("actor-transient-provider-retry-fails");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_transient_retry_fails"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(TransientThenOkProvider::new(3, 2));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    actor
        .run_until_idle(8)
        .expect("actor should stay alive after recoverable failure");

    assert_eq!(*provider.calls.lock().unwrap(), 3);
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnFailed {
            can_continue: true,
            ..
        })
    ));
}

#[test]
fn provider_error_keeps_actor_alive_and_continue_retries_current_history() {
    let _cwd = temp_cwd("actor-continue-after-provider-error");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_continue_error"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "retry me".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    let step = actor
        .run_until_idle(4)
        .expect("recoverable error should not crash actor");

    assert_eq!(step, SessionActorStep::Idle);
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnFailed {
            can_continue: true,
            ..
        })
    ));
    assert_eq!(actor.history().len(), 1);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::QuerySessionView {
            query_id: "live_state".to_string(),
            payload: serde_json::json!({"type": "live_state"}),
        },
    );
    actor.step().expect("live state query should run");
    let live_state = session_view_payload_for_test(&events, "live_state");
    assert_eq!(live_state["type"], "live_state");
    assert_eq!(live_state["initialized"], true);
    assert_eq!(live_state["history_len"], 1);
    assert_eq!(live_state["can_continue"], true);

    provider
        .responses
        .lock()
        .unwrap()
        .push_back(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "continued".to_string(),
            })],
        ));
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::ContinueTurn {
            reason: Some("user confirmed".to_string()),
        },
    );

    actor
        .run_until_idle(4)
        .expect("continue should retry current history");

    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
    assert_eq!(actor.history().len(), 2);
    assert_eq!(provider.seen_requests.lock().unwrap().len(), 2);
}

#[test]
fn request_too_large_provider_error_compacts_history_and_retries() {
    let _cwd = temp_cwd("actor-request-too-large-retry");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_request_too_large"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(32);
    initial.compression_retain_recent_tokens = Some(4);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(RequestTooLargeThenOkProvider::new());
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );
    actor.run_until_idle(2).expect("initial should apply");
    actor.history = vec![
        text_message(ChatRole::User, "old 1"),
        text_message(ChatRole::Assistant, "old 2"),
        text_message(ChatRole::User, "old 3"),
        text_message(ChatRole::Assistant, "new 1"),
        text_message(ChatRole::User, "new 2"),
        text_message(ChatRole::Assistant, "new 3"),
    ];
    actor.all_messages = actor.history.clone();

    actor
        .start_provider_request("turn_retry".to_string(), 1, 0, 0)
        .expect("request too large should start provider request");
    actor
        .run_until_idle(4)
        .expect("request too large should recover by compacting history");

    assert_eq!(*provider.calls.lock().unwrap(), 3);
    assert!(actor
        .history()
        .iter()
        .any(|message| matches!(message.data.first(), Some(ChatMessageItem::Compaction(_)))));
    assert!(events.events.lock().unwrap().iter().any(|event| matches!(
        event,
        SessionEvent::CompactCompleted { compressed, .. } if *compressed
    )));
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn repeated_request_too_large_prunes_after_one_compaction_attempt() {
    let _cwd = temp_cwd("actor-request-too-large-prune-after-compact");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_request_too_large_prune"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(32);
    initial.compression_retain_recent_tokens = Some(4);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(RepeatedRequestTooLargeProvider::new());
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );
    actor.run_until_idle(2).expect("initial should apply");
    actor.history = (0..12)
        .map(|index| {
            let role = if index % 2 == 0 {
                ChatRole::User
            } else {
                ChatRole::Assistant
            };
            text_message(role, &format!("message {index}"))
        })
        .collect();
    actor.all_messages = actor.history.clone();
    let original_len = actor.history.len();

    actor
        .start_provider_request("turn_retry".to_string(), 1, 0, 0)
        .expect("request too large should start provider request");
    actor
        .run_until_idle(8)
        .expect("repeated request too large should recover by pruning");

    assert_eq!(*provider.compression_calls.lock().unwrap(), 1);
    assert_eq!(*provider.normal_calls.lock().unwrap(), 3);
    assert!(
        actor.history().len() < original_len,
        "history should be pruned after compression does not make the request acceptable"
    );
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn preflight_compacts_when_estimate_already_exceeds_context() {
    let _cwd = temp_cwd("actor-request-too-large-preflight");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_preflight_too_large"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(1_000);
    initial.compression_retain_recent_tokens = Some(1);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_message(
            ChatRole::Assistant,
            &compression_response("compressed before preflight retry"),
        ),
        text_message(ChatRole::Assistant, "recovered after preflight compact"),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut model_config = test_model_config();
    model_config.capabilities.push(ModelCapability::ImageIn);
    model_config.multimodal_input = Some(MultimodalInputConfig {
        image: Some(MediaInputConfig {
            transport: MediaInputTransport::FileReference,
            supported_media_types: vec!["image/png".to_string()],
            max_width: None,
            max_height: None,
        }),
        pdf: None,
        audio: None,
    });
    model_config.token_max_context = 1_000;
    let mut actor = SessionActor::new(
        model_config,
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );
    actor.run_until_idle(2).expect("initial should apply");
    let image_message = || {
        ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/large.png".to_string(),
                name: Some("large.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: Some(1024),
                height: Some(1024),
                state: None,
            })],
        )
    };
    actor.history = vec![image_message(), image_message(), image_message()];
    actor.all_messages = actor.history.clone();

    actor
        .start_provider_request("turn_preflight".to_string(), 1, 0, 0)
        .expect("oversized local estimate should start provider request");
    actor
        .run_until_idle(4)
        .expect("oversized local estimate should recover by compacting history before send");

    let seen_requests = provider.seen_requests.lock().unwrap();
    assert_eq!(seen_requests.len(), 2);
    assert_eq!(seen_requests[0].message_count, 3);
    assert!(seen_requests[1].message_count < seen_requests[0].message_count);
    assert!(events.events.lock().unwrap().iter().any(|event| matches!(
        event,
        SessionEvent::CompactCompleted { compressed, .. } if *compressed
    )));
}

#[test]
fn query_session_view_returns_transcript_page_and_message_detail() {
    let _cwd = temp_cwd("actor-query-session-view");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_query_view"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "show transcript".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "transcript response".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        tools.clone(),
        inbox,
        events.clone(),
        catalog,
    );
    actor.run_until_idle(4).expect("actor should run");

    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::QuerySessionView {
            query_id: "page".to_string(),
            payload: serde_json::json!({
                "type": "transcript_page",
                "source": "current",
                "offset": 0,
                "limit": 1,
            }),
        },
    );
    actor.step().expect("page query should run");
    let page = session_view_payload_for_test(&events, "page");
    assert_eq!(page["type"], "transcript_page");
    assert_eq!(page["source"], "current");
    let total = page["total"]
        .as_u64()
        .expect("transcript total should be numeric");
    assert!(total >= 2);
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);

    let assistant_index = total - 1;
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::QuerySessionView {
            query_id: "detail".to_string(),
            payload: serde_json::json!({
                "type": "message_detail",
                "source": "current",
                "index": assistant_index,
            }),
        },
    );
    actor.step().expect("detail query should run");
    let detail = session_view_payload_for_test(&events, "detail");
    assert_eq!(detail["type"], "message_detail");
    assert_eq!(detail["index"], assistant_index);
    assert_eq!(detail["message"]["role"], "assistant");

    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::QuerySessionView {
            query_id: "missing".to_string(),
            payload: serde_json::json!({
                "type": "message_detail",
                "source": "all",
                "index": 99,
            }),
        },
    );
    actor.step().expect("missing detail query should run");
    let missing = session_view_payload_for_test(&events, "missing");
    assert_eq!(missing["type"], "message_detail");
    assert_eq!(missing["error"], "message index out of range");
    assert_eq!(missing["total"], total);
}

#[test]
fn runs_model_tool_model_loop_and_routes_bridge_tools() {
    let _cwd = temp_cwd("actor-tool-loop");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_tool_loop");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "tell user".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: r#"{}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        tools.clone(),
        inbox,
        events,
        catalog,
    );

    actor.run_until_idle(4).expect("actor should run");

    assert_eq!(actor.history().len(), 4);
    let batches = tools.batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert!(matches!(
        batches[0].operations[0].item,
        ToolBatchItem::RegisteredTool(_)
    ));
}

#[test]
fn tool_results_emit_stream_event_before_durable_message() {
    let _cwd = temp_cwd("actor-tool-result-stream-event");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_tool_result_stream_event");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "run tool".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: r#"{}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        ),
    ]));
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        Arc::new(ProgressEchoToolExecutor),
        inbox,
        events.clone(),
        catalog,
    );

    actor.run_until_idle(8).expect("actor should run");

    let captured = events.events.lock().unwrap();
    let progress_index = captured
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::StreamToolResultDone { tool_result, .. }
                    if tool_result.tool_call_id == "call_1"
            )
        })
        .expect("tool result stream event should be emitted");
    let durable_index = captured
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::MessageAppended { message, .. }
                    if message.data.iter().any(|item| matches!(
                        item,
                        ChatMessageItem::ToolResult(result)
                            if result.tool_call_id == "call_1"
                    ))
            )
        })
        .expect("durable tool result message should be emitted");
    assert!(progress_index < durable_index);
}

#[test]
fn provider_disabled_tools_are_not_executed_from_filtered_catalog() {
    let _cwd = temp_cwd("actor-provider-filtered-tools");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_provider_filtered_tool");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "tell user".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let mut model_config = test_model_config();
    model_config.provider_type = ProviderType::CodexSubscription;
    model_config.url = "wss://codex.example.invalid".to_string();
    model_config.api_key_env = "CODEX_AUTH".to_string();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    item_id: None,
                    tool_call_id: "call_1".to_string(),
                    tool_name: "not_a_catalog_tool".to_string(),
                    arguments: ContextItem {
                        text: r#"{}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "done".to_string(),
                })],
            ),
        ])
        .with_model_config(model_config.clone()),
    );
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = ToolCatalog::from_model_config_and_session_type(
        &model_config,
        super::super::SessionType::Foreground,
    )
    .expect("catalog should build");
    assert!(!catalog.contains("not_a_catalog_tool"));
    let mut actor = SessionActor::new(
        model_config,
        provider,
        tools.clone(),
        inbox,
        events,
        catalog,
    );

    actor.run_until_idle(4).expect("actor should run");

    let batches = tools.batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert!(matches!(
        batches[0].operations[0].item,
        ToolBatchItem::UnsupportedTool { .. }
    ));
}

#[test]
fn tool_result_file_does_not_add_synthetic_user_media_context() {
    let _cwd = temp_cwd("actor-media-context");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_media_context");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "load image".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "image_view".to_string(),
                arguments: ContextItem {
                    text: r#"{"path":"test.png"}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "saw image".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(MediaFileToolExecutor);
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        enable_native_image_view: true,
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events,
        catalog,
    );

    actor.run_until_idle(6).expect("actor should run");

    assert_eq!(actor.history().len(), 4);
    assert!(matches!(
        actor.history()[2].data[0],
        ChatMessageItem::ToolResult(_)
    ));
    assert_eq!(actor.history()[3].role, ChatRole::Assistant);
    let seen_requests = provider.seen_requests.lock().unwrap();
    assert_eq!(seen_requests.len(), 2);
    assert_eq!(seen_requests[1].message_count, 3);
}

#[test]
fn rejects_data_before_initial_message() {
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    let error = actor.step().expect_err("data before initial should fail");

    assert!(matches!(error, SessionActorError::MissingInitial));
}

#[test]
fn initial_enables_remote_tools_for_actor_catalog() {
    let _cwd = temp_cwd("actor-remote-tools");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_remote_tools"),
        super::super::SessionType::Foreground,
    );
    initial.tool_remote_mode = super::super::ToolRemoteMode::Selectable;
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    actor.step().expect("initial should apply");

    let remote =
        &actor.tool_catalog().get("shell_exec").unwrap().parameters["properties"]["remote"];
    assert_eq!(remote["type"], "string");
}

#[test]
fn injects_runtime_metadata_updates_before_user_message() {
    let _cwd = temp_cwd("actor-runtime-meta");
    fs::create_dir_all(".stellaclaw").expect("metadata dir should exist");
    fs::create_dir_all(".stellaclaw/skill/demo").expect("skill dir should exist");
    fs::write(".stellaclaw/USER.md", "tier: old").expect("user metadata should seed");
    fs::write(
        ".stellaclaw/skill/demo/SKILL.md",
        "# Demo\n\nold desc\n\nold body",
    )
    .expect("skill should seed");
    let (inbox, mailbox) = test_inbox();
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "done".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_runtime_meta"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    actor.step().expect("initial should apply");

    fs::write(".stellaclaw/USER.md", "tier: new").expect("user metadata should update");
    fs::write(
        ".stellaclaw/skill/demo/SKILL.md",
        "# Demo\n\nnew desc\n\nold body",
    )
    .expect("skill should update");
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "real user request".to_string(),
                })],
            ),
        },
    );

    actor.run_until_idle(4).expect("actor should run");

    assert!(message_text_for_test(&actor.history()[0]).contains("[Runtime Prompt Updates]"));
    assert!(message_text_for_test(&actor.history()[0]).contains("USER.md metadata changed"));
    assert!(message_text_for_test(&actor.history()[0]).contains(".stellaclaw/USER.md"));
    assert!(!message_text_for_test(&actor.history()[0]).contains("tier: new"));
    assert!(message_text_for_test(&actor.history()[1]).contains("[Runtime Skill Updates]"));
    assert!(message_text_for_test(&actor.history()[2]).contains("real user request"));
}

#[test]
fn user_message_metadata_inserts_synthetic_notice_before_user_input() {
    let _cwd = temp_cwd("actor-user-message-metadata");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(
                test_session_id("session_user_message_metadata"),
                super::super::SessionType::Foreground,
            ),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello".to_string(),
                })],
            )
            .with_user_name("alice")
            .with_message_time("2026-04-23T10:20:30Z"),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "done".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    actor.run_until_idle(4).expect("actor should run");

    assert!(message_text_for_test(&actor.history()[0]).contains("[Incoming User Metadata]"));
    assert!(message_text_for_test(&actor.history()[0]).contains("Speaker: alice"));
    assert!(
        message_text_for_test(&actor.history()[0]).contains("Message time: 2026-04-23T10:20:30Z")
    );
    assert_eq!(actor.history()[1].user_name.as_deref(), Some("alice"));
    assert_eq!(
        actor.history()[1].message_time.as_deref(),
        Some("2026-04-23T10:20:30Z")
    );
}

#[test]
fn restores_session_state_on_initial() {
    let _cwd = temp_cwd("actor-restore");
    let session_id = test_session_id("session_restore");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id.clone(), super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "persist me".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "persisted reply".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
    actor.run_until_idle(4).expect("first actor should run");

    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut restored =
        SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    restored.step().expect("initial should restore state");

    assert_eq!(restored.history().len(), 2);
    assert!(message_text_for_test(&restored.history()[0]).contains("persist me"));
    assert!(message_text_for_test(&restored.history()[1]).contains("persisted reply"));
}

#[test]
fn restored_unfinished_history_requests_continue_confirmation() {
    let _cwd = temp_cwd("actor-restore-unfinished");
    let session_id = test_session_id("session_restore_unfinished");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id.clone(), super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "unfinished request".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
    actor
        .run_until_idle(4)
        .expect("provider error should leave recoverable state");

    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let mut restored = SessionActor::new(
        test_model_config(),
        provider,
        tools.clone(),
        inbox,
        events.clone(),
        catalog,
    );

    restored
        .step()
        .expect("initial should restore unfinished state");

    assert_eq!(restored.history().len(), 1);
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnFailed {
            can_continue: true,
            ..
        })
    ));
}

#[test]
fn does_not_persist_history_with_unclosed_tool_call() {
    let _cwd = temp_cwd("actor-unclosed-tool");
    let session_id = test_session_id("session_unclosed_tool");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id.clone(), super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "run a tool".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: r#"{}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        ),
    ]));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let tools = Arc::new(BlockingToolExecutor::new(started_tx, release_rx));
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

    assert_eq!(
        actor.step().expect("initial should apply"),
        SessionActorStep::ProcessedControl
    );
    step_until(
        &mut actor,
        SessionActorStep::WaitingToolBatch,
        20,
        "tool batch should start",
    );
    started_rx
        .recv()
        .expect("tool batch should start after model tool call");

    let state_path = std::env::current_dir()
        .unwrap()
        .join(".stellaclaw")
        .join("log")
        .join(&session_id)
        .join("session.json");
    let state_before_release: SessionActorPersistedState =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("safe state should exist"))
            .expect("safe state should parse");
    assert_eq!(state_before_release.current_messages.len(), 1);
    assert_eq!(
        count_unclosed_tool_calls(&state_before_release.current_messages),
        0
    );

    release_tx.send(()).expect("tool wait should release");
    let mut reached_idle = false;
    for _ in 0..20 {
        if actor.step().expect("actor should advance") == SessionActorStep::Idle {
            reached_idle = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        reached_idle,
        "actor should reach idle after tool completion"
    );

    let state_after_release: SessionActorPersistedState =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("closed state should exist"))
            .expect("closed state should parse");
    assert!(state_after_release.current_messages.len() >= 4);
    assert_eq!(
        count_unclosed_tool_calls(&state_after_release.current_messages),
        0
    );
    assert!(state_after_release.current_messages.iter().any(|message| {
        message
            .data
            .iter()
            .any(|item| matches!(item, ChatMessageItem::ToolCall(_)))
    }));
    assert!(state_after_release.current_messages.iter().any(|message| {
        message
            .data
            .iter()
            .any(|item| matches!(item, ChatMessageItem::ToolResult(_)))
    }));
}

#[test]
fn newer_user_message_interrupts_active_tool_batch_and_runs_next_turn() {
    let _cwd = temp_cwd("actor-user-interrupt");
    let session_id = test_session_id("session_user_interrupt");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "run a slow tool".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: r#"{}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "handled newer message".to_string(),
            })],
        ),
    ]));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (interrupt_tx, interrupt_rx) = mpsc::channel();
    let tools = Arc::new(BlockingToolExecutor::with_interrupt_tx(
        started_tx,
        release_rx,
        Some(interrupt_tx),
    ));
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        tools.clone(),
        inbox,
        events.clone(),
        catalog,
    );

    assert_eq!(
        actor.step().expect("initial should apply"),
        SessionActorStep::ProcessedControl
    );
    step_until(
        &mut actor,
        SessionActorStep::WaitingToolBatch,
        20,
        "tool batch should start",
    );
    started_rx
        .recv()
        .expect("tool batch should start after model tool call");

    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "new instruction".to_string(),
                })],
            ),
        },
    );
    assert_eq!(
        actor
            .step()
            .expect("new user message should request interrupt"),
        SessionActorStep::WaitingToolBatch
    );
    interrupt_rx
        .recv()
        .expect("new user message should interrupt active tool batch");

    release_tx.send(()).expect("tool wait should release");
    let mut yielded = false;
    for _ in 0..20 {
        if actor.step().expect("interrupted batch should yield") == SessionActorStep::ProcessedData
        {
            yielded = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(yielded, "interrupted batch should yield after completion");
    step_until(
        &mut actor,
        SessionActorStep::ProcessedData,
        20,
        "new user message should run next",
    );

    let completed = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnCompleted { message, .. } => Some(message_text_for_test(message)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed, vec!["handled newer message".to_string()]);
    let progress = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Progress { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(progress
        .iter()
        .all(|message| !message.contains("newer user message")));
}

#[test]
fn provider_supersede_grace_after_window_does_not_underflow() {
    let _cwd = temp_cwd("actor-provider-supersede-grace-underflow");
    let (inbox, _mailbox) = test_inbox();
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = ToolCatalog::new();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
    actor.active_provider_request = Some(ActiveProviderRequest {
        request_id: "request_1".to_string(),
        message_id: "message_1".to_string(),
        turn_id: "turn_1".to_string(),
        turn_number: 1,
        step_index: 0,
        request_too_large_attempts: 0,
        started_at_ms: 0,
        next_stream_event_index: 0,
        last_activity_at: Instant::now() - PROVIDER_SUPERSEDE_GRACE - Duration::from_millis(1),
    });
    actor
        .pending_data
        .push_back(SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "interrupt after grace".to_string(),
                })],
            ),
        });

    assert_eq!(actor.provider_supersede_grace_remaining(), None);
    assert!(actor.has_ready_work());
}

#[test]
fn provider_supersede_grace_timer_is_armed_once_for_burst_user_messages() {
    let _cwd = temp_cwd("actor-provider-supersede-grace-single-timer");
    let (inbox, _mailbox) = test_inbox();
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = ToolCatalog::new();
    let mut actor = SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
    actor.active_provider_request = Some(ActiveProviderRequest {
        request_id: "request_1".to_string(),
        message_id: "message_1".to_string(),
        turn_id: "turn_1".to_string(),
        turn_number: 1,
        step_index: 0,
        request_too_large_attempts: 0,
        started_at_ms: 0,
        next_stream_event_index: 0,
        last_activity_at: Instant::now(),
    });

    actor.enqueue_request(SessionRequest::EnqueueUserMessage {
        message: ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "first interrupt".to_string(),
            })],
        ),
    });
    let active_timer = actor.active_provider_supersede_grace_timer_id;
    let next_timer_id = actor.next_provider_supersede_grace_timer_id;

    actor.enqueue_request(SessionRequest::EnqueueUserMessage {
        message: ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "second interrupt".to_string(),
            })],
        ),
    });

    assert_eq!(actor.active_provider_supersede_grace_timer_id, active_timer);
    assert_eq!(actor.next_provider_supersede_grace_timer_id, next_timer_id);
}

#[test]
fn failed_tool_batch_closes_tool_calls_before_continuing_model_loop() {
    let _cwd = temp_cwd("actor-tool-batch-failure-closes-calls");
    let session_id = test_session_id("session_tool_batch_failure_closes_calls");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "run a tool that fails".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: r#"{}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "recovered after tool failure".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(FailingCompletionToolExecutor);
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        tools.clone(),
        inbox,
        events.clone(),
        catalog,
    );

    actor.run_until_idle(10).expect("actor should recover");

    assert_eq!(count_unclosed_tool_calls(actor.history()), 0);
    let has_failure_result = actor.history().iter().any(|message| {
        message.data.iter().any(|item| {
            matches!(
                item,
                ChatMessageItem::ToolResult(result)
                    if result.tool_call_id == "call_1"
                        && tool_result_text(result).contains("simulated tool batch failure")
            )
        })
    });
    assert!(has_failure_result);
    let completed = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnCompleted { message, .. } => Some(message_text_for_test(message)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed, vec!["recovered after tool failure".to_string()]);
}

#[test]
fn repairs_preexisting_unclosed_tool_call_before_provider_request() {
    let _cwd = temp_cwd("actor-repairs-unclosed-tool-call");
    let (inbox, mailbox) = test_inbox();
    let session_id = test_session_id("session_repair_unclosed_tool_call");
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::Context(ContextItem {
            text: "continued after repair".to_string(),
        })],
    )]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider.clone(),
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    assert_eq!(
        actor.step().expect("initial should process"),
        SessionActorStep::ProcessedControl
    );
    actor.history.push(ChatMessage::new(
        ChatRole::User,
        vec![ChatMessageItem::Context(ContextItem {
            text: "continue".to_string(),
        })],
    ));
    actor.history.push(ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::ToolCall(ToolCallItem {
            item_id: None,
            tool_call_id: "call_orphan".to_string(),
            tool_name: "cron_tasks_list".to_string(),
            arguments: ContextItem {
                text: r#"{}"#.to_string(),
            },
        })],
    ));

    actor
        .start_provider_request("turn_repair".to_string(), 1, 0, 0)
        .expect("unclosed tool call should start provider request");
    actor
        .run_until_idle(4)
        .expect("unclosed tool call should be repaired");

    assert_eq!(count_unclosed_tool_calls(actor.history()), 0);
    let repaired = actor.history().iter().any(|message| {
        message.data.iter().any(|item| {
            matches!(
                item,
                ChatMessageItem::ToolResult(result)
                    if result.tool_call_id == "call_orphan"
                        && tool_result_text(result).contains("did not receive a tool result")
            )
        })
    });
    assert!(repaired);
    let message_counts = provider
        .seen_requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.message_count)
        .collect::<Vec<_>>();
    assert_eq!(message_counts, vec![3]);
}

#[test]
fn update_plan_routes_through_conversation_bridge() {
    let _cwd = temp_cwd("actor-update-plan-bridge");
    let session_id = test_session_id("session_update_plan_bridge");
    let (inbox, mailbox) = test_inbox();
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial {
            initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
        },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "do planned work".to_string(),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(ToolCallItem {
                item_id: None,
                tool_call_id: "call_plan".to_string(),
                tool_name: "update_plan".to_string(),
                arguments: ContextItem {
                    text: r#"{"plan":[{"step":"Inspect state","status":"in_progress"},{"step":"Report result","status":"pending"}]}"#.to_string(),
                },
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
        host_tool_scope: Some(HostToolScope::MainForeground),
        ..BuiltinToolCatalogOptions::default()
    })
    .unwrap();
    let mut actor = SessionActor::new(
        test_model_config(),
        provider,
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    assert_eq!(
        actor.step().expect("initial should apply"),
        SessionActorStep::ProcessedControl
    );
    step_until(
        &mut actor,
        SessionActorStep::ProcessedData,
        20,
        "plan tool call should close and finish",
    );
    assert_eq!(count_unclosed_tool_calls(actor.history()), 0);
    assert!(actor.history().iter().any(|message| {
        message.data.iter().any(|item| {
            matches!(
                item,
                ChatMessageItem::ToolResult(result)
                    if result.tool_call_id == "call_plan"
                        && result.tool_name == "update_plan"
            )
        })
    }));
    assert!(!events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, SessionEvent::PlanUpdated { .. })));
    assert!(matches!(
        events.events.lock().unwrap().last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn compresses_history_before_appending_next_data_message_when_threshold_is_exceeded() {
    let _cwd = temp_cwd("actor-compression");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_compression"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(32);
    initial.compression_retain_recent_tokens = Some(12);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(50),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "first final".to_string(),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: compression_response("summary"),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "second final".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let (model_config, tokenizer_dir) = test_model_config_with_tokenizer();
    let mut actor = SessionActor::new(model_config, provider, tools, inbox, events, catalog);

    actor.run_until_idle(4).expect("first turn should run");
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "second request".to_string(),
                })],
            ),
        },
    );
    actor.run_until_idle(8).expect("actor should run");

    assert!(message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));
    assert!(message_text_for_test(&actor.history()[0]).contains("summary"));
    assert!(actor
        .history()
        .iter()
        .any(|message| message_text_for_test(message).contains("second final")));

    fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
}

#[test]
fn model_response_with_tool_call_defers_compression_until_protocol_closes() {
    let message = ChatMessage::new(
        ChatRole::Assistant,
        vec![ChatMessageItem::ToolCall(ToolCallItem {
            item_id: None,
            tool_call_id: "call_1".to_string(),
            tool_name: "attachment_make_visible".to_string(),
            arguments: ContextItem {
                text: r#"{"path":"plot.svg"}"#.to_string(),
            },
        })],
    );

    assert!(append_message_should_defer_compression(
        "model_response",
        &message
    ));
}

#[test]
fn compression_does_not_append_runtime_update_plan_context() {
    let _cwd = temp_cwd("actor-compression-no-runtime-plan");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_compression_no_runtime_plan"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(32);
    initial.compression_retain_recent_tokens = Some(12);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(50),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "first final".to_string(),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: compression_response("summary"),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "second final".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let (model_config, tokenizer_dir) = test_model_config_with_tokenizer();
    let mut actor = SessionActor::new(model_config, provider, tools, inbox, events, catalog);
    actor.current_plan = Some(TaskPlanView {
        explanation: None,
        plan: vec![TaskPlanItemView {
            step: "Do not append this as a separate compacted message".to_string(),
            status: TaskPlanItemStatus::InProgress,
        }],
    });

    actor.run_until_idle(4).expect("first turn should run");
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "second request".to_string(),
                })],
            ),
        },
    );
    actor.run_until_idle(8).expect("actor should run");

    assert!(actor
        .history()
        .iter()
        .all(|message| !message_text_for_test(message).contains(SESSION_PLAN_CONTEXT_MARKER)));
    assert!(message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));

    fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
}

#[test]
fn manual_compact_now_forces_context_compression() {
    let _cwd = temp_cwd("actor-manual-compression");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_manual_compression"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(1_000);
    initial.compression_retain_recent_tokens = Some(12);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(50),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "first final".to_string(),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: compression_response("manual summary"),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let (model_config, tokenizer_dir) = test_model_config_with_tokenizer();
    let mut actor = SessionActor::new(
        model_config,
        provider,
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    actor.run_until_idle(4).expect("initial turn should run");
    mailbox.append(SessionMailboxKind::Control, SessionRequest::CompactNow);
    actor.run_until_idle(4).expect("manual compact should run");

    assert!(message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));
    assert!(message_text_for_test(&actor.history()[0]).contains("manual summary"));
    let completed = events.events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            SessionEvent::CompactCompleted {
                compressed: true,
                ..
            }
        )
    });
    assert!(completed);

    fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
}

#[test]
fn manual_compact_failure_emits_compact_failed_event() {
    let _cwd = temp_cwd("actor-manual-compression-failed");
    let (inbox, mailbox) = test_inbox();
    let mut initial = SessionInitial::new(
        test_session_id("session_manual_compression_failed"),
        super::super::SessionType::Foreground,
    );
    initial.compression_threshold_tokens = Some(1_000);
    initial.compression_retain_recent_tokens = Some(12);
    mailbox.append(
        SessionMailboxKind::Control,
        SessionRequest::Initial { initial },
    );
    mailbox.append(
        SessionMailboxKind::Data,
        SessionRequest::EnqueueUserMessage {
            message: ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(50),
                })],
            ),
        },
    );
    let events = Arc::new(MemoryEventSink::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "first final".to_string(),
            })],
        ),
        ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "not json".to_string(),
            })],
        ),
    ]));
    let tools = Arc::new(EchoToolExecutor::new());
    let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
    let (model_config, tokenizer_dir) = test_model_config_with_tokenizer();
    let mut actor = SessionActor::new(
        model_config,
        provider,
        tools,
        inbox,
        events.clone(),
        catalog,
    );

    actor.run_until_idle(4).expect("initial turn should run");
    mailbox.append(SessionMailboxKind::Control, SessionRequest::CompactNow);
    actor
        .run_until_idle(4)
        .expect("compact failure should be reported");

    let failed = events.events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            SessionEvent::CompactFailed { phase, reason }
                if phase == "manual_compaction"
                    && reason.contains("compression summary was not valid JSON")
        )
    });
    assert!(failed);
    assert!(!message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));

    fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
}

fn message_text_for_test(message: &ChatMessage) -> String {
    message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::Context(context) => Some(context.text.as_str()),
            ChatMessageItem::Compaction(compaction) => compaction.generic_summary_text(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_message(role: ChatRole, text: &str) -> ChatMessage {
    ChatMessage::new(
        role,
        vec![ChatMessageItem::Context(ContextItem {
            text: text.to_string(),
        })],
    )
}

fn session_view_payload_for_test(
    events: &MemoryEventSink,
    expected_query_id: &str,
) -> serde_json::Value {
    events
        .events
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::SessionViewResult { query_id, payload }
                if query_id == expected_query_id =>
            {
                Some(payload.clone())
            }
            _ => None,
        })
        .expect("session view result should exist")
}
