use std::{
    collections::{BTreeSet, VecDeque},
    env,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossbeam_channel::{select, Receiver, Sender};
use serde::Deserialize;
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    huggingface::HuggingFaceFileResolver,
    model_config::ModelConfig,
    providers::{
        request_too_large_text, Provider, ProviderError, ProviderEvent, ProviderFailureKind,
        ProviderRequestOwned, ProviderSession, ProviderStreamEvent,
    },
};

#[cfg(not(test))]
use super::LocalToolBatchExecutor;
#[cfg(test)]
use super::ToolBatchExecutor;
use super::{
    logger::SessionActorLogger,
    normalize_messages_for_model,
    runtime_metadata::{
        remote_aliases_prompt_for_mode, RuntimeMetadataState, REMOTE_WORKSPACE_PROMPT_COMPONENT,
    },
    session_state::{SessionActorPersistedState, SessionStateStore},
    system_prompt_for_initial_with_common_prompt, ChatMessage, ChatMessageItem, ChatRole,
    CompressionError, CompressionReport, ContextItem, ConversationBridge,
    ConversationBridgeRequest, SessionCompressor, SessionErrorDetail, SessionEvent, SessionInitial,
    SessionMailbox, SessionMailboxKind, SessionMessageHistory, SessionMessageRecord,
    SessionRequest, TaskPlanItemStatus, TaskPlanView, TokenEstimate, TokenEstimator, ToolBatch,
    ToolBatchCompletion, ToolBatchItem, ToolBatchOperation, ToolBatchProgress, ToolCatalog,
    ToolResultContent,
};

const ACTIVE_COMPRESSION_THRESHOLD_RATIO: f64 = 0.9;
const REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS: usize = 8;
const DEFAULT_RETAIN_RECENT_PERCENT: u64 = 10;
const SESSION_PLAN_CONTEXT_MARKER: &str = "[StellaClaw Current Task Plan]";
const COMPRESSION_MEMORY_SCOPE_CANDIDATE_LIMIT: usize = 20;
const COMPRESSION_MEMORY_CONTEXT_MAX_TOKENS: u64 = 4_000;
const COMPRESSION_MEMORY_ENTRY_MAX_CHARS: usize = 640;
const PROVIDER_SUPERSEDE_GRACE: Duration = Duration::from_millis(200);

pub struct SessionActor {
    model_config: ModelConfig,
    provider: Arc<ProviderSession>,
    #[cfg(not(test))]
    tool_executor: Arc<LocalToolBatchExecutor>,
    #[cfg(test)]
    tool_executor: Arc<dyn ToolBatchExecutor + Send + Sync>,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    request_rx: Receiver<SessionRequest>,
    tool_completion_tx: Sender<ToolBatchCompletion>,
    tool_completion_rx: Receiver<ToolBatchCompletion>,
    tool_progress_tx: Sender<ToolBatchProgress>,
    tool_progress_rx: Receiver<ToolBatchProgress>,
    provider_event_rx: Receiver<ProviderEvent>,
    internal_event_tx: Sender<SessionActorInternalEvent>,
    internal_event_rx: Receiver<SessionActorInternalEvent>,
    pending_control: VecDeque<SessionRequest>,
    pending_data: VecDeque<SessionRequest>,
    pending_tool_completions: VecDeque<ToolBatchCompletion>,
    pending_tool_progress: VecDeque<ToolBatchProgress>,
    pending_provider_events: VecDeque<ProviderEvent>,
    event_sink: Arc<dyn SessionActorEventSink>,
    tool_catalog: ToolCatalog,
    history: Vec<ChatMessage>,
    all_messages: Vec<ChatMessage>,
    initial: Option<SessionInitial>,
    active_provider_request: Option<ActiveProviderRequest>,
    active_tool_batch: Option<ActiveToolBatch>,
    runtime_metadata_state: RuntimeMetadataState,
    next_turn_id: u64,
    next_batch_id: u64,
    shutdown: bool,
    logger: Option<SessionActorLogger>,
    state_store: Option<SessionStateStore>,
    compressor: Option<SessionCompressor>,
    token_estimator: Option<TokenEstimator>,
    pending_continuation: Option<PendingContinuation>,
    current_plan: Option<TaskPlanView>,
    last_provider_request_started_at: Option<Instant>,
    last_agent_returned_at: Option<Instant>,
    last_completed_turn_number: u64,
    next_provider_supersede_grace_timer_id: u64,
    active_provider_supersede_grace_timer_id: Option<u64>,
}

#[derive(Debug, Clone)]
struct ActiveToolBatch {
    turn_id: String,
    turn_number: u64,
    step_index: usize,
    handle: super::ToolBatchHandle,
    operations: Vec<ToolBatchOperation>,
    operation_summary: String,
    started_at_ms: u128,
    interrupt: Option<ToolBatchInterrupt>,
}

#[derive(Debug, Clone)]
struct ActiveProviderRequest {
    request_id: String,
    message_id: String,
    turn_id: String,
    turn_number: u64,
    step_index: usize,
    request_too_large_attempts: usize,
    started_at_ms: u128,
    next_stream_event_index: u64,
    last_activity_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBatchInterrupt {
    Cancel,
    SupersededByUserMessage,
}

#[derive(Debug, Clone)]
enum PendingContinuation {
    CurrentHistory,
    DataRequests(Vec<SessionRequest>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionActorInternalEvent {
    ProviderSupersedeGraceElapsed { timer_id: u64 },
}

#[derive(Debug, Deserialize)]
struct MemorySearchToolResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    results: Vec<MemorySearchToolResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct MemorySearchToolResult {
    id: String,
    scope: String,
    #[serde(default)]
    subject: Option<String>,
    text: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    score: f64,
}

pub struct SessionActorInbox {
    request_rx: Receiver<SessionRequest>,
    tool_completion_tx: Sender<ToolBatchCompletion>,
    tool_completion_rx: Receiver<ToolBatchCompletion>,
    tool_progress_tx: Sender<ToolBatchProgress>,
    tool_progress_rx: Receiver<ToolBatchProgress>,
}

#[derive(Clone)]
pub struct SessionActorRequestSender {
    request_tx: Sender<SessionRequest>,
}

impl SessionActorInbox {
    pub fn channel() -> (Self, SessionActorRequestSender) {
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (tool_completion_tx, tool_completion_rx) = crossbeam_channel::unbounded();
        let (tool_progress_tx, tool_progress_rx) = crossbeam_channel::unbounded();
        (
            Self {
                request_rx,
                tool_completion_tx,
                tool_completion_rx,
                tool_progress_tx,
                tool_progress_rx,
            },
            SessionActorRequestSender { request_tx },
        )
    }
}

impl SessionActorRequestSender {
    pub fn send(&self, request: SessionRequest) -> Result<(), String> {
        self.request_tx
            .send(request)
            .map_err(|_| "session actor request channel closed".to_string())
    }
}

impl SessionMailbox for SessionActorRequestSender {
    fn append(&self, kind: SessionMailboxKind, request: SessionRequest) -> Result<(), String> {
        if kind != request.mailbox_kind() {
            return Err(format!(
                "request kind mismatch: envelope={kind:?}, request={:?}",
                request.mailbox_kind()
            ));
        }
        self.send(request)
    }
}

impl SessionActor {
    pub fn new(
        model_config: ModelConfig,
        provider: Arc<dyn Provider + Send + Sync>,
        #[cfg(not(test))] tool_executor: Arc<LocalToolBatchExecutor>,
        #[cfg(test)] tool_executor: Arc<dyn ToolBatchExecutor + Send + Sync>,
        inbox: SessionActorInbox,
        event_sink: Arc<dyn SessionActorEventSink>,
        tool_catalog: ToolCatalog,
    ) -> Self {
        Self::new_with_provider_session(
            model_config,
            ProviderSession::new(provider),
            tool_executor,
            inbox,
            event_sink,
            tool_catalog,
        )
    }

    pub fn new_with_provider_session(
        model_config: ModelConfig,
        provider: ProviderSession,
        #[cfg(not(test))] tool_executor: Arc<LocalToolBatchExecutor>,
        #[cfg(test)] tool_executor: Arc<dyn ToolBatchExecutor + Send + Sync>,
        inbox: SessionActorInbox,
        event_sink: Arc<dyn SessionActorEventSink>,
        tool_catalog: ToolCatalog,
    ) -> Self {
        let tool_catalog = tool_catalog.filtered_for_model_config(&model_config);
        let provider = Arc::new(provider);
        let provider_event_rx = provider.event_rx();
        let (internal_event_tx, internal_event_rx) = crossbeam_channel::unbounded();
        Self {
            model_config,
            provider,
            tool_executor,
            conversation_bridge: None,
            request_rx: inbox.request_rx,
            tool_completion_tx: inbox.tool_completion_tx,
            tool_completion_rx: inbox.tool_completion_rx,
            tool_progress_tx: inbox.tool_progress_tx,
            tool_progress_rx: inbox.tool_progress_rx,
            provider_event_rx,
            internal_event_tx,
            internal_event_rx,
            pending_control: VecDeque::new(),
            pending_data: VecDeque::new(),
            pending_tool_completions: VecDeque::new(),
            pending_tool_progress: VecDeque::new(),
            pending_provider_events: VecDeque::new(),
            event_sink,
            tool_catalog,
            history: Vec::new(),
            all_messages: Vec::new(),
            initial: None,
            active_provider_request: None,
            active_tool_batch: None,
            runtime_metadata_state: RuntimeMetadataState::default(),
            next_turn_id: 1,
            next_batch_id: 1,
            shutdown: false,
            logger: None,
            state_store: None,
            compressor: None,
            token_estimator: None,
            pending_continuation: None,
            current_plan: None,
            last_provider_request_started_at: None,
            last_agent_returned_at: None,
            last_completed_turn_number: 0,
            next_provider_supersede_grace_timer_id: 1,
            active_provider_supersede_grace_timer_id: None,
        }
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn initial(&self) -> Option<&SessionInitial> {
        self.initial.as_ref()
    }

    pub fn tool_catalog(&self) -> &ToolCatalog {
        &self.tool_catalog
    }

    fn system_prompt_for_current_initial(&self) -> Result<Option<String>, SessionActorError> {
        let initial = match self.initial.as_ref() {
            Some(initial) => initial,
            None => return Ok(None),
        };
        let started_at = Instant::now();
        self.log_info(
            "system_prompt_build_started",
            serde_json::json!({
                "provider_type": &self.model_config.provider_type,
                "model_name": &self.model_config.model_name,
            }),
        );
        let provider_common_prompt = self
            .provider
            .system_prompt_for_model(&self.model_config)
            .map_err(SessionActorError::from_provider_error)?;
        let system_prompt = system_prompt_for_initial_with_common_prompt(
            initial,
            &self.runtime_metadata_state,
            provider_common_prompt.as_deref(),
        );
        self.log_info(
            "system_prompt_build_completed",
            serde_json::json!({
                "elapsed_ms": started_at.elapsed().as_millis(),
                "provider_common_prompt": provider_common_prompt.is_some(),
                "system_prompt_chars": system_prompt.len(),
            }),
        );
        Ok(Some(system_prompt))
    }

    fn provider_enabled_tool_names(&self) -> BTreeSet<String> {
        self.tool_catalog
            .iter()
            .map(|(_, tool)| tool.name.clone())
            .collect()
    }

    pub fn with_conversation_bridge(
        mut self,
        conversation_bridge: Arc<dyn ConversationBridge + Send + Sync>,
    ) -> Self {
        self.conversation_bridge = Some(conversation_bridge);
        self
    }

    pub fn recv_step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        let step = self.recv_one_event()?;
        self.run_pending_data_if_idle(step)
    }

    fn recv_one_event(&mut self) -> Result<SessionActorStep, SessionActorError> {
        select! {
            recv(self.request_rx) -> request => {
                request
                    .map_err(|_| SessionActorError::Mailbox("session actor request channel closed".to_string()))
                    .and_then(|request| self.handle_request_event(request))
            }
            recv(self.tool_completion_rx) -> completion => {
                completion
                    .map_err(|_| SessionActorError::Tool("tool completion channel disconnected".to_string()))
                    .and_then(|completion| self.handle_tool_completion_event(completion))
            }
            recv(self.tool_progress_rx) -> progress => {
                progress
                    .map_err(|_| SessionActorError::Tool("tool progress channel disconnected".to_string()))
                    .and_then(|progress| self.handle_tool_progress_event(progress))
            }
            recv(self.provider_event_rx) -> event => {
                event
                    .map_err(|_| {
                        SessionActorError::from_provider_error(ProviderError::Subprocess(
                            "provider event channel disconnected".to_string(),
                        ))
                    })
                    .and_then(|event| self.handle_provider_event(event))
            }
            recv(self.internal_event_rx) -> event => {
                event
                    .map_err(|_| SessionActorError::Mailbox("session actor internal event channel closed".to_string()))
                    .and_then(|event| self.handle_internal_event(event))
            }
        }
    }

    fn handle_request_event(
        &mut self,
        request: SessionRequest,
    ) -> Result<SessionActorStep, SessionActorError> {
        match request.mailbox_kind() {
            SessionMailboxKind::Control => {
                self.log_info(
                    "control_request",
                    serde_json::json!({"request": session_request_kind(&request)}),
                );
                self.handle_control(request)?;
                Ok(if self.shutdown {
                    SessionActorStep::Shutdown
                } else {
                    SessionActorStep::ProcessedControl
                })
            }
            SessionMailboxKind::Data => {
                let is_user_message = matches!(request, SessionRequest::EnqueueUserMessage { .. });
                self.pending_data.push_back(request);
                if is_user_message && self.active_tool_batch.is_some() {
                    self.request_active_tool_interrupt(
                        ToolBatchInterrupt::SupersededByUserMessage,
                        "newer user message arrived".to_string(),
                    )?;
                }
                if is_user_message && self.active_provider_request.is_some() {
                    self.schedule_provider_supersede_grace_event_if_needed();
                }
                Ok(if self.active_provider_request.is_some() {
                    SessionActorStep::WaitingProviderRequest
                } else if self.active_tool_batch.is_some() {
                    SessionActorStep::WaitingToolBatch
                } else {
                    SessionActorStep::ProcessedData
                })
            }
        }
    }

    fn run_pending_data_if_idle(
        &mut self,
        fallback: SessionActorStep,
    ) -> Result<SessionActorStep, SessionActorError> {
        if self.shutdown
            || self.active_provider_request.is_some()
            || self.active_tool_batch.is_some()
            || self.pending_data.is_empty()
        {
            return Ok(fallback);
        }

        let data_requests: Vec<SessionRequest> = self.pending_data.drain(..).collect();
        if data_requests.is_empty()
            || data_requests.iter().any(|request| {
                !matches!(
                    request,
                    SessionRequest::EnqueueUserMessage { .. }
                        | SessionRequest::EnqueueActorMessage { .. }
                )
            })
        {
            return Err(SessionActorError::UnexpectedDataRequest);
        }

        self.log_info(
            "data_requests",
            serde_json::json!({
                "requests": data_requests
                    .iter()
                    .map(session_request_kind)
                    .collect::<Vec<_>>(),
            }),
        );
        self.run_turn_from_data_requests(data_requests)?;
        Ok(if self.active_provider_request.is_some() {
            SessionActorStep::WaitingProviderRequest
        } else if self.active_tool_batch.is_some() {
            SessionActorStep::WaitingToolBatch
        } else {
            SessionActorStep::ProcessedData
        })
    }

    fn handle_control(&mut self, control: SessionRequest) -> Result<(), SessionActorError> {
        match control {
            SessionRequest::Initial { initial } => {
                if self.initial.is_some() {
                    self.log_warn(
                        "initial_rejected",
                        serde_json::json!({"reason": "session initial has already been applied"}),
                    );
                    return self.emit(SessionEvent::ControlRejected {
                        reason: "session initial has already been applied".to_string(),
                        payload: serde_json::to_value(initial).unwrap_or_else(
                            |_| serde_json::json!({"error": "serialize initial failed"}),
                        ),
                    });
                }

                let logger = SessionActorLogger::open_default(&initial.session_id)
                    .map_err(SessionActorError::Logging)?;
                let state_store = SessionStateStore::open_default(&initial.session_id)
                    .map_err(SessionActorError::Persistence)?;
                self.tool_catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
                    &self.model_config,
                    &initial,
                    self.provider.tool_set().as_deref(),
                )
                .map_err(|error| SessionActorError::ToolCatalog(error.to_string()))?;
                let active_compression_threshold_tokens =
                    active_compression_threshold_tokens(&self.model_config, &initial);
                let token_estimator = match build_session_token_estimator(&self.model_config) {
                    Ok(estimator) => Some(estimator),
                    Err(error) if active_compression_threshold_tokens.is_none() => {
                        logger.warn(
                            "token_estimator_unavailable",
                            serde_json::json!({
                                "error": error.to_string(),
                                "preflight_context_check": false,
                            }),
                        );
                        None
                    }
                    Err(error) => {
                        return Err(SessionActorError::Compression(error.to_string()));
                    }
                };
                let compressor = build_session_compressor(
                    active_compression_threshold_tokens,
                    &initial,
                    token_estimator.as_ref(),
                )?;
                let loaded = state_store.load().map_err(SessionActorError::Persistence)?;
                if let Some(saved) = loaded {
                    self.restore_persisted_state(saved, initial.clone())?;
                    logger.info(
                        "session_state_restored",
                        serde_json::json!({
                            "session_id": &initial.session_id,
                            "history_len": self.history.len(),
                            "all_messages_len": self.all_messages.len(),
                            "next_turn_id": self.next_turn_id,
                            "next_batch_id": self.next_batch_id,
                        }),
                    );
                } else {
                    self.runtime_metadata_state
                        .initialize_from_workspace(
                            &self
                                .workspace_root()
                                .map_err(SessionActorError::RuntimeMetadata)?,
                            &self
                                .data_root()
                                .map_err(SessionActorError::RuntimeMetadata)?,
                            remote_aliases_prompt_for_mode(&initial.tool_remote_mode),
                            initial
                                .remote_workspace_instructions
                                .clone()
                                .unwrap_or_default(),
                            initial.memory_enabled,
                        )
                        .map_err(SessionActorError::RuntimeMetadata)?;
                }
                logger.info(
                    "initial_applied",
                    serde_json::json!({
                        "session_id": &initial.session_id,
                        "session_type": &initial.session_type,
                        "tool_remote_mode": &initial.tool_remote_mode,
                        "compression_threshold_tokens": initial.compression_threshold_tokens,
                        "active_compression_threshold_tokens": active_compression_threshold_tokens,
                        "compression_retain_recent_tokens": initial.compression_retain_recent_tokens,
                        "tool_count": self.tool_catalog.len(),
                        "log_path": logger.path(),
                    }),
                );
                self.logger = Some(logger);
                self.state_store = Some(state_store);
                self.compressor = compressor;
                self.token_estimator = token_estimator;
                self.initial = Some(initial);
                self.persist_state()?;
                if restored_history_needs_continuation(&self.history) {
                    self.pending_continuation = Some(PendingContinuation::CurrentHistory);
                    self.log_warn(
                        "restored_unfinished_turn",
                        serde_json::json!({
                            "history_len": self.history.len(),
                            "all_messages_len": self.all_messages.len(),
                        }),
                    );
                    self.emit_turn_failed(
                        "session restored with an unfinished turn; ask the user whether to continue processing".to_string(),
                        SessionErrorDetail::new(
                            "session_actor.restore",
                            "unfinished_turn",
                            "session restored with an unfinished turn",
                        ),
                        true,
                    )?;
                }
                Ok(())
            }
            SessionRequest::Shutdown => {
                self.log_info("shutdown_requested", serde_json::json!({}));
                self.shutdown = true;
                Ok(())
            }
            SessionRequest::CancelTurn { reason } => self.handle_cancel_turn(reason),
            SessionRequest::ContinueTurn { reason } => self.handle_continue_turn(reason),
            SessionRequest::CompactNow => self.handle_compact_now(),
            SessionRequest::QuerySessionView { query_id, payload } => {
                self.handle_query_session_view(query_id, payload)
            }
            SessionRequest::QueryMessageHistory {
                request_id,
                offset,
                limit,
            } => self.handle_query_message_history(request_id, offset, limit),
            SessionRequest::QueryMessageDetail {
                request_id,
                message_id,
            } => self.handle_query_message_detail(request_id, message_id),
            other => self.emit(SessionEvent::ControlRejected {
                reason: "control command is not implemented by SessionActor yet".to_string(),
                payload: serde_json::to_value(other)
                    .unwrap_or_else(|_| serde_json::json!({"error": "serialize control failed"})),
            }),
        }
    }

    fn handle_compact_now(&mut self) -> Result<(), SessionActorError> {
        if self.active_tool_batch.is_some() || count_unclosed_tool_calls(&self.history) > 0 {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot compact while a tool batch or unfinished tool call is active"
                    .to_string(),
                payload: serde_json::json!({"type": "compact_now"}),
            });
        }

        let Some(compressor) = self.compressor.clone() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "context compression is not available for this session".to_string(),
                payload: serde_json::json!({"type": "compact_now"}),
            });
        };

        self.log_info(
            "manual_compaction_started",
            serde_json::json!({
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
            }),
        );
        let system_prompt = self.system_prompt_for_current_initial()?;
        let compression_context = self.compression_memory_context(&self.history, None);

        let report = match compressor.compact_now_with_tools(
            &mut self.history,
            self.provider.as_ref(),
            &self.model_config,
            system_prompt.as_deref(),
            compression_context.as_deref(),
            self.tool_catalog
                .iter()
                .map(|(_, tool)| tool)
                .collect::<Vec<_>>(),
        ) {
            Ok(report) => report,
            Err(error) => {
                let reason = format!("manual context compression failed: {error}");
                self.log_error(
                    "manual_compaction_failed",
                    serde_json::json!({
                        "error": reason,
                        "history_len": self.history.len(),
                    }),
                );
                return self.emit_compact_failed("manual_compaction", reason);
            }
        };

        self.log_compression_report("manual", &report);
        if report.compressed {
            self.runtime_metadata_state
                .promote_notified_components_to_system_snapshot();
            self.persist_state_if_history_closed("manual_compaction")?;
        }
        self.log_info(
            "manual_compaction_finished",
            serde_json::json!({
                "compressed": report.compressed,
                "estimated_tokens_before": report.estimated_tokens_before,
                "estimated_tokens_after": report.estimated_tokens_after,
                "threshold_tokens": report.threshold_tokens,
                "retained_message_count": report.retained_message_count,
                "compressed_message_count": report.compressed_message_count,
                "history_len": self.history.len(),
            }),
        );
        self.emit(SessionEvent::CompactCompleted {
            compressed: report.compressed,
            estimated_tokens_before: report.estimated_tokens_before,
            estimated_tokens_after: report.estimated_tokens_after,
            threshold_tokens: report.threshold_tokens,
            retained_message_count: report.retained_message_count,
            compressed_message_count: report.compressed_message_count,
        })
    }

    fn handle_cancel_turn(&mut self, reason: Option<String>) -> Result<(), SessionActorError> {
        if self.active_provider_request.is_some() {
            return self.cancel_active_provider_request(
                reason.unwrap_or_else(|| "user_cancelled".to_string()),
                true,
            );
        }
        self.request_active_tool_interrupt(
            ToolBatchInterrupt::Cancel,
            reason.unwrap_or_else(|| "user_cancelled".to_string()),
        )
    }

    fn cancel_active_provider_request(
        &mut self,
        reason: String,
        emit_failure: bool,
    ) -> Result<(), SessionActorError> {
        let Some(active) = self.active_provider_request.take() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "no active interruptible turn".to_string(),
                payload: serde_json::json!({"command": "cancel_turn"}),
            });
        };
        self.disarm_provider_supersede_grace_event();
        self.provider
            .abort()
            .map_err(SessionActorError::from_provider_error)?;
        self.log_info(
            "provider_request_cancelled",
            serde_json::json!({
                "turn_id": active.turn_id,
                "request_id": active.request_id,
                "step_index": active.step_index,
                "reason": reason,
            }),
        );
        self.mark_turn_returned(active.turn_number);
        self.emit(SessionEvent::StreamError {
            message_id: active.message_id.clone(),
            turn_id: active.turn_id.clone(),
            in_message_index: active.next_stream_event_index,
            item_id: None,
            message_index: None,
            error: reason.clone(),
            error_detail: SessionErrorDetail::new("session_actor.provider", "cancelled", reason),
        })?;
        if emit_failure {
            self.emit_turn_failed(
                "provider request cancelled".to_string(),
                SessionErrorDetail::new(
                    "session_actor.provider",
                    "cancelled",
                    "provider request cancelled",
                ),
                false,
            )?;
        }
        Ok(())
    }

    fn request_active_tool_interrupt(
        &mut self,
        interrupt: ToolBatchInterrupt,
        reason: String,
    ) -> Result<(), SessionActorError> {
        let Some(active) = self.active_tool_batch.as_mut() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "no active interruptible turn".to_string(),
                payload: serde_json::json!({"command": "cancel_turn"}),
            });
        };
        if active.interrupt.is_some() {
            return Ok(());
        }

        self.tool_executor
            .interrupt(&active.handle)
            .map_err(|error| SessionActorError::Tool(error.to_string()))?;
        active.interrupt = Some(interrupt);
        let turn_id = active.turn_id.clone();
        let batch_id = active.handle.batch_id.clone();
        self.log_info(
            "tool_batch_interrupt_requested",
            serde_json::json!({
                "turn_id": turn_id,
                "batch_id": batch_id,
                "reason": match interrupt {
                    ToolBatchInterrupt::Cancel => "cancel",
                    ToolBatchInterrupt::SupersededByUserMessage => "superseded_by_user_message",
                },
                "detail": reason,
            }),
        );
        if interrupt == ToolBatchInterrupt::SupersededByUserMessage {
            return Ok(());
        }

        self.emit(SessionEvent::Progress {
            message: format!("interrupt requested for tool batch {batch_id}"),
            plan: self.current_plan.clone(),
        })
    }

    fn has_pending_user_message(&self) -> bool {
        self.pending_data
            .iter()
            .any(|request| matches!(request, SessionRequest::EnqueueUserMessage { .. }))
    }

    fn provider_supersede_grace_remaining(&self) -> Option<Duration> {
        if !self.has_pending_user_message() {
            return None;
        }
        let active = self.active_provider_request.as_ref()?;
        let elapsed = active.last_activity_at.elapsed();
        if elapsed >= PROVIDER_SUPERSEDE_GRACE {
            None
        } else {
            Some(PROVIDER_SUPERSEDE_GRACE.saturating_sub(elapsed))
        }
    }

    fn schedule_provider_supersede_grace_event_if_needed(&mut self) {
        if self.active_provider_supersede_grace_timer_id.is_some() {
            return;
        }
        let Some(delay) = self.provider_supersede_grace_remaining() else {
            return;
        };
        let timer_id = self.next_provider_supersede_grace_timer_id;
        self.next_provider_supersede_grace_timer_id = self
            .next_provider_supersede_grace_timer_id
            .saturating_add(1);
        self.active_provider_supersede_grace_timer_id = Some(timer_id);
        let internal_event_tx = self.internal_event_tx.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = internal_event_tx
                .send(SessionActorInternalEvent::ProviderSupersedeGraceElapsed { timer_id });
        });
    }

    fn wake_or_schedule_provider_supersede_if_needed(&mut self) -> Result<(), SessionActorError> {
        if self.active_provider_request.is_none() || !self.has_pending_user_message() {
            return Ok(());
        }
        if self.provider_supersede_grace_remaining().is_none() {
            self.cancel_active_provider_request("superseded_by_user_message".to_string(), false)?;
        } else {
            self.schedule_provider_supersede_grace_event_if_needed();
        }
        Ok(())
    }

    fn disarm_provider_supersede_grace_event(&mut self) {
        self.active_provider_supersede_grace_timer_id = None;
    }

    fn handle_internal_event(
        &mut self,
        event: SessionActorInternalEvent,
    ) -> Result<SessionActorStep, SessionActorError> {
        match event {
            SessionActorInternalEvent::ProviderSupersedeGraceElapsed { timer_id } => {
                if self.active_provider_supersede_grace_timer_id != Some(timer_id) {
                    return Ok(SessionActorStep::Idle);
                }
                self.active_provider_supersede_grace_timer_id = None;

                if let Ok(provider_event) = self.provider_event_rx.try_recv() {
                    return self.handle_provider_event(provider_event);
                }

                if self.active_provider_request.is_some() && self.has_pending_user_message() {
                    if self.provider_supersede_grace_remaining().is_none() {
                        self.cancel_active_provider_request(
                            "superseded_by_user_message".to_string(),
                            false,
                        )?;
                    } else {
                        self.schedule_provider_supersede_grace_event_if_needed();
                    }
                }
            }
        }
        Ok(if self.active_provider_request.is_some() {
            SessionActorStep::WaitingProviderRequest
        } else if self.active_tool_batch.is_some() {
            SessionActorStep::WaitingToolBatch
        } else {
            SessionActorStep::Idle
        })
    }

    fn handle_query_session_view(
        &self,
        query_id: String,
        payload: serde_json::Value,
    ) -> Result<(), SessionActorError> {
        self.emit(SessionEvent::SessionViewResult {
            query_id,
            payload: self.session_view_payload(payload),
        })
    }

    fn handle_query_message_history(
        &self,
        request_id: String,
        offset: usize,
        limit: usize,
    ) -> Result<(), SessionActorError> {
        let total = self.all_messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit.min(500)).min(total);
        let messages = self.all_messages[start..end]
            .iter()
            .cloned()
            .enumerate()
            .map(|(relative_index, message)| SessionMessageRecord {
                index: start + relative_index,
                message,
            })
            .collect::<Vec<_>>();
        let last_message = self
            .all_messages
            .last()
            .cloned()
            .map(|message| SessionMessageRecord {
                index: total.saturating_sub(1),
                message,
            });
        self.emit(SessionEvent::MessageHistoryResult {
            history: SessionMessageHistory {
                request_id,
                offset,
                limit,
                total,
                last_message,
                messages,
            },
        })
    }

    fn handle_query_message_detail(
        &self,
        request_id: String,
        message_id: String,
    ) -> Result<(), SessionActorError> {
        let requested_index = message_id.parse::<usize>().ok();
        let record = self
            .all_messages
            .iter()
            .cloned()
            .enumerate()
            .find(|(index, message)| {
                message.message_id == message_id
                    || requested_index.is_some_and(|value| value == *index)
            })
            .map(|(index, message)| SessionMessageRecord { index, message });
        self.emit(SessionEvent::MessageDetailResult { request_id, record })
    }

    fn session_view_payload(&self, payload: serde_json::Value) -> serde_json::Value {
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("transcript_page") => self.transcript_page_payload(&payload),
            Some("message_detail") => self.message_detail_payload(&payload),
            Some("live_state") => self.live_state_payload(),
            Some(query_type) => serde_json::json!({
                "type": query_type,
                "error": format!("unsupported session view query type {query_type}"),
            }),
            None => serde_json::json!({
                "type": "error",
                "error": "missing session view query type",
                "query": payload,
            }),
        }
    }

    fn transcript_page_payload(&self, payload: &serde_json::Value) -> serde_json::Value {
        let (source, messages) = self.messages_for_view_payload(payload);
        let Some(messages) = messages else {
            return invalid_source_payload(source);
        };
        let offset = usize_field(payload, "offset", 0);
        let limit = usize_field(payload, "limit", 50).min(200);
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        serde_json::json!({
            "type": "transcript_page",
            "source": source,
            "offset": offset,
            "limit": limit,
            "total": total,
            "messages": messages[start..end],
        })
    }

    fn message_detail_payload(&self, payload: &serde_json::Value) -> serde_json::Value {
        let (source, messages) = self.messages_for_view_payload(payload);
        let Some(messages) = messages else {
            return invalid_source_payload(source);
        };
        let Some(index) = payload
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
        else {
            return serde_json::json!({
                "type": "message_detail",
                "source": source,
                "error": "missing numeric message index",
                "total": messages.len(),
            });
        };
        let Some(message) = messages.get(index) else {
            return serde_json::json!({
                "type": "message_detail",
                "source": source,
                "index": index,
                "error": "message index out of range",
                "total": messages.len(),
            });
        };
        serde_json::json!({
            "type": "message_detail",
            "source": source,
            "index": index,
            "message": message,
        })
    }

    fn live_state_payload(&self) -> serde_json::Value {
        let initial = self.initial.as_ref();
        serde_json::json!({
            "type": "live_state",
            "initialized": initial.is_some(),
            "session_id": initial.map(|initial| initial.session_id.as_str()),
            "session_type": initial.map(|initial| initial.session_type),
            "shutdown": self.shutdown,
            "history_len": self.history.len(),
            "all_messages_len": self.all_messages.len(),
            "pending_control_len": self.pending_control.len(),
            "pending_data_len": self.pending_data.len(),
            "pending_provider_event_len": self.pending_provider_events.len(),
            "pending_tool_completion_len": self.pending_tool_completions.len(),
            "pending_tool_progress_len": self.pending_tool_progress.len(),
            "active_provider_request": self.active_provider_request.as_ref().map(|active| serde_json::json!({
                "turn_id": active.turn_id,
                "request_id": active.request_id,
                "step_index": active.step_index,
                "started_at_ms": active.started_at_ms,
            })),
            "active_tool_batch": self.active_tool_batch.as_ref().map(|active| serde_json::json!({
                "turn_id": active.turn_id,
                "batch_id": active.handle.batch_id,
                "step_index": active.step_index,
                "operation_summary": active.operation_summary,
                "started_at_ms": active.started_at_ms,
            })),
            "can_continue": self.pending_continuation.is_some(),
        })
    }

    fn messages_for_view_payload<'a, 'b>(
        &'a self,
        payload: &'b serde_json::Value,
    ) -> (&'b str, Option<&'a [ChatMessage]>) {
        let source = payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("current");
        let messages = match source {
            "current" => Some(self.history.as_slice()),
            "all" => Some(self.all_messages.as_slice()),
            _ => None,
        };
        (source, messages)
    }

    fn handle_continue_turn(&mut self, reason: Option<String>) -> Result<(), SessionActorError> {
        if self.initial.is_none() {
            return Err(SessionActorError::MissingInitial);
        }
        if self.active_provider_request.is_some() {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot continue while a provider request is running".to_string(),
                payload: serde_json::json!({"command": "continue_turn", "reason": reason}),
            });
        }
        if self.active_tool_batch.is_some() {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot continue while a tool batch is running".to_string(),
                payload: serde_json::json!({"command": "continue_turn", "reason": reason}),
            });
        }

        let Some(continuation) = self.pending_continuation.take() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "no recoverable failed turn is pending".to_string(),
                payload: serde_json::json!({"command": "continue_turn", "reason": reason}),
            });
        };

        self.log_info(
            "continue_turn_requested",
            serde_json::json!({
                "reason": reason,
                "mode": pending_continuation_kind(&continuation),
            }),
        );

        match continuation {
            PendingContinuation::CurrentHistory => self.continue_turn_from_history(),
            PendingContinuation::DataRequests(requests) => {
                self.run_turn_from_data_requests(requests)
            }
        }
    }

    fn run_turn_from_data_requests(
        &mut self,
        requests: Vec<SessionRequest>,
    ) -> Result<(), SessionActorError> {
        if self.initial.is_none() {
            self.log_error(
                "turn_rejected",
                serde_json::json!({"reason": "missing_initial"}),
            );
            return Err(SessionActorError::MissingInitial);
        }

        self.pending_continuation = None;
        let retry_requests = requests.clone();
        let appended_message_start = self.all_messages.len();
        let mut input_roles = Vec::with_capacity(requests.len());
        let mut input_items = 0usize;

        let turn_id = self.allocate_turn_id();
        let turn_number = self.next_turn_id.saturating_sub(1);
        for request in requests {
            let input_message = match request {
                SessionRequest::EnqueueUserMessage { message } => {
                    if let Err(error) = self.append_runtime_synthetic_messages(&message) {
                        return self.finish_turn_error(
                            &turn_id,
                            error,
                            Some(PendingContinuation::DataRequests(retry_requests.clone())),
                        );
                    }
                    message
                }
                SessionRequest::EnqueueActorMessage { message } => message,
                _ => return Err(SessionActorError::UnexpectedDataRequest),
            };
            input_roles.push(input_message.role.clone());
            input_items = input_items.saturating_add(input_message.data.len());
            if let Err(error) = self.append_history_message("input", input_message) {
                return self.finish_turn_error(
                    &turn_id,
                    error,
                    Some(PendingContinuation::DataRequests(retry_requests.clone())),
                );
            }
        }
        self.log_info(
            "turn_started",
            serde_json::json!({
                "turn_id": &turn_id,
                "input_roles": input_roles,
                "input_messages": retry_requests.len(),
                "input_items": input_items,
            }),
        );
        for (relative, message) in self.all_messages[appended_message_start..]
            .iter()
            .cloned()
            .enumerate()
        {
            self.emit(SessionEvent::MessageAppended {
                index: appended_message_start + relative,
                message,
            })?;
        }
        self.emit(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            plan: self.current_plan.clone(),
        })?;

        if let Err(error) = self.start_provider_request(turn_id.clone(), turn_number, 0, 0) {
            return self.finish_turn_error(
                &turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            );
        }

        Ok(())
    }

    fn continue_turn_from_history(&mut self) -> Result<(), SessionActorError> {
        if self.history.is_empty() {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot continue without existing session history".to_string(),
                payload: serde_json::json!({"command": "continue_turn"}),
            });
        }

        let turn_id = self.allocate_turn_id();
        let turn_number = self.next_turn_id.saturating_sub(1);
        self.log_info(
            "turn_continued",
            serde_json::json!({
                "turn_id": &turn_id,
                "history_len": self.history.len(),
            }),
        );
        self.emit(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            plan: self.current_plan.clone(),
        })?;

        if let Err(error) = self.start_provider_request(turn_id.clone(), turn_number, 0, 0) {
            return self.finish_turn_error(
                &turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            );
        }

        Ok(())
    }

    fn finish_turn_error(
        &mut self,
        turn_id: &str,
        error: SessionActorError,
        continuation: Option<PendingContinuation>,
    ) -> Result<(), SessionActorError> {
        self.log_error(
            "turn_failed",
            serde_json::json!({"turn_id": turn_id, "error": error.to_string()}),
        );
        let can_continue = continuation.is_some() && error.is_recoverable_turn_error();
        if can_continue {
            self.pending_continuation = continuation;
        }
        self.emit_turn_failed(error.to_string(), error.detail(), can_continue)?;
        if can_continue {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn start_provider_request(
        &mut self,
        turn_id: String,
        turn_number: u64,
        step_index: usize,
        mut request_too_large_attempts: usize,
    ) -> Result<(), SessionActorError> {
        if self.active_provider_request.is_some() {
            return Err(SessionActorError::from_provider_error(
                ProviderError::Subprocess("provider request is already running".to_string()),
            ));
        }
        loop {
            self.log_info(
                "provider_request_started",
                serde_json::json!({
                    "turn_id": &turn_id,
                    "step_index": step_index,
                    "history_len": self.history.len(),
                    "provider_type": &self.model_config.provider_type,
                    "model_name": &self.model_config.model_name,
                }),
            );
            self.repair_unclosed_tool_calls_before_provider_request(&turn_id, step_index)?;
            let system_prompt = self
                .system_prompt_for_current_initial()?
                .ok_or(SessionActorError::MissingInitial)?;
            let normalized_history =
                normalize_messages_for_model(&self.history, &self.model_config);
            let provider_history = self
                .provider
                .normalize_messages_for_provider(&normalized_history);
            let estimate = self.provider_request_token_estimate(&provider_history)?;
            if let Some(estimate) = estimate.as_ref() {
                self.log_info(
                    "provider_request_token_estimate",
                    serde_json::json!({
                        "turn_id": &turn_id,
                        "step_index": step_index,
                        "text_tokens": estimate.text_tokens,
                        "multimodal_tokens": estimate.multimodal_tokens,
                        "reasoning_tokens": estimate.reasoning_tokens,
                        "total_tokens": estimate.total_tokens,
                        "token_max_context": self.model_config.token_max_context,
                    }),
                );
            }
            if let Some(estimated_tokens) = estimate
                .as_ref()
                .filter(|estimate| estimate.total_tokens >= self.model_config.token_max_context)
                .map(|estimate| estimate.total_tokens)
            {
                if request_too_large_attempts >= REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS {
                    return Err(SessionActorError::provider_preflight(format!(
                        "estimated provider request tokens {estimated_tokens} exceed model context {} after {REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS} prune attempts",
                        self.model_config.token_max_context
                    )));
                }
                request_too_large_attempts += 1;
                let error = format!(
                    "estimated provider request tokens {estimated_tokens} exceed model context {}",
                    self.model_config.token_max_context
                );
                if self.compact_history_after_request_too_large(
                    "provider_request_preflight",
                    Some(&turn_id),
                    Some(step_index),
                    &error,
                )? {
                    continue;
                }
                return Err(SessionActorError::provider_preflight(error));
            }

            let request_id = format!(
                "{}_provider_{}_{}",
                turn_id, step_index, request_too_large_attempts
            );
            let message_id = message_id_for_all_messages_index(self.all_messages.len());
            let request = ProviderRequestOwned {
                system_prompt: Some(system_prompt),
                messages: provider_history,
                tools: self
                    .tool_catalog
                    .iter()
                    .map(|(_, tool)| tool.clone())
                    .collect(),
                image_edit_mask: None,
                image_size: None,
            };
            self.provider
                .start(request_id.clone(), request)
                .map_err(SessionActorError::from_provider_error)?;
            let now = Instant::now();
            self.last_provider_request_started_at = Some(now);
            self.active_provider_request = Some(ActiveProviderRequest {
                request_id,
                message_id,
                turn_id,
                turn_number,
                step_index,
                request_too_large_attempts,
                started_at_ms: current_time_millis(),
                next_stream_event_index: 0,
                last_activity_at: now,
            });
            self.schedule_provider_supersede_grace_event_if_needed();
            return Ok(());
        }
    }

    fn process_provider_message(
        &mut self,
        active: ActiveProviderRequest,
        mut model_message: ChatMessage,
    ) -> Result<(), SessionActorError> {
        model_message.message_id = active.message_id.clone();
        stamp_assistant_message_time(&mut model_message);

        let tool_calls = collect_tool_calls(&model_message);
        self.log_info(
            "provider_response_received",
            serde_json::json!({
                "turn_id": active.turn_id,
                "step_index": active.step_index,
                "message_items": model_message.data.len(),
                "tool_calls": tool_calls.iter().map(|tool| &tool.tool_name).collect::<Vec<_>>(),
            }),
        );
        let model_message_index =
            self.append_history_message("model_response", model_message.clone())?;
        self.emit(SessionEvent::MessageAppended {
            index: model_message_index,
            message: model_message.clone(),
        })?;

        if tool_calls.is_empty() {
            self.log_info(
                "turn_completed",
                serde_json::json!({
                    "turn_id": active.turn_id,
                    "final_items": model_message.data.len(),
                }),
            );
            self.emit(SessionEvent::TurnCompleted {
                message: model_message,
            })?;
            self.mark_turn_returned(active.turn_number);
            self.log_info(
                "turn_completed_emitted",
                serde_json::json!({
                    "turn_id": active.turn_id,
                }),
            );
            if let Err(error) = self.clear_current_plan() {
                self.log_error(
                    "clear_current_plan_after_turn_completed_failed",
                    serde_json::json!({
                        "turn_id": active.turn_id,
                        "error": error.to_string(),
                    }),
                );
            }
            return Ok(());
        }

        let batch = self.build_tool_batch(&active.turn_id, tool_calls)?;
        let batch_progress = batch.progress_summary();
        self.log_info(
            "tool_batch_started",
            serde_json::json!({
                "turn_id": active.turn_id,
                "batch_id": &batch.batch_id,
                "operations": batch.operations.len(),
                "operation_summary": &batch_progress,
            }),
        );
        self.emit(SessionEvent::Progress {
            message: format!("running tool batch {}: {}", batch.batch_id, batch_progress),
            plan: self.current_plan.clone(),
        })?;

        let operations = batch.operations.clone();
        let handle = match self.tool_executor.start(
            batch,
            self.tool_completion_tx.clone(),
            self.tool_progress_tx.clone(),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                let mut tool_message = tool_error_message_for_operations(
                    &operations,
                    format!("tool batch failed to start: {error}"),
                );
                stamp_assistant_message_time(&mut tool_message);
                let tool_message_index =
                    self.append_history_message("tool_result", tool_message.clone())?;
                self.emit(SessionEvent::MessageAppended {
                    index: tool_message_index,
                    message: tool_message,
                })?;
                return self.start_provider_request(
                    active.turn_id,
                    active.turn_number,
                    active.step_index.saturating_add(1),
                    0,
                );
            }
        };
        self.active_tool_batch = Some(ActiveToolBatch {
            turn_id: active.turn_id,
            turn_number: active.turn_number,
            step_index: active.step_index,
            handle,
            operations,
            operation_summary: batch_progress,
            started_at_ms: current_time_millis(),
            interrupt: None,
        });
        Ok(())
    }

    fn emit_provider_stream_event(
        &mut self,
        message_id: &str,
        turn_id: &str,
        in_message_index: u64,
        event: ProviderStreamEvent,
    ) -> Result<(), SessionActorError> {
        let model_message_index = self.history.len();
        match event {
            ProviderStreamEvent::KeepAlive | ProviderStreamEvent::RawJson { .. } => {}
            ProviderStreamEvent::OutputTextDelta { item_id, delta } => {
                self.emit(SessionEvent::StreamAssistantMessageDelta {
                    message_id: message_id.to_string(),
                    turn_id: turn_id.to_string(),
                    in_message_index,
                    item_id,
                    delta,
                    message_index: Some(model_message_index),
                })?;
            }
            ProviderStreamEvent::ToolCallInputDelta {
                item_id,
                call_id,
                tool_name,
                delta,
            } => {
                self.emit(SessionEvent::StreamToolCallDelta {
                    message_id: message_id.to_string(),
                    turn_id: turn_id.to_string(),
                    in_message_index,
                    item_id,
                    call_id,
                    tool_name,
                    delta,
                })?;
            }
            ProviderStreamEvent::ReasoningSummaryDelta {
                item_id,
                delta,
                summary_index,
            } => {
                self.emit(SessionEvent::StreamReasoningSummaryDelta {
                    message_id: message_id.to_string(),
                    turn_id: turn_id.to_string(),
                    in_message_index,
                    item_id,
                    summary_index,
                    delta,
                })?;
            }
            ProviderStreamEvent::ReasoningSummaryPartAdded {
                item_id,
                summary_index,
            } => {
                self.emit(SessionEvent::StreamReasoningSummaryPartAdded {
                    message_id: message_id.to_string(),
                    turn_id: turn_id.to_string(),
                    in_message_index,
                    item_id,
                    summary_index,
                })?;
            }
        }
        Ok(())
    }

    fn build_tool_batch(
        &mut self,
        turn_id: &str,
        tool_calls: Vec<super::ToolCallItem>,
    ) -> Result<ToolBatch, SessionActorError> {
        let batch_id = self.allocate_batch_id(turn_id);
        let mut operations = Vec::with_capacity(tool_calls.len());
        let provider_enabled_tool_names = self.provider_enabled_tool_names();

        for tool_call in tool_calls {
            if !provider_enabled_tool_names.contains(&tool_call.tool_name) {
                operations.push(
                    ToolBatchItem::UnsupportedTool {
                        reason: format!("{} is disabled for this provider", tool_call.tool_name),
                        tool_call,
                    }
                    .into(),
                );
                continue;
            }
            let scheduled = match self.tool_catalog.get(&tool_call.tool_name) {
                Some(definition) => {
                    let operation = if self
                        .tool_catalog
                        .should_execute_registered(&tool_call.tool_name)
                    {
                        ToolBatchItem::RegisteredTool(tool_call)
                    } else {
                        ToolBatchItem::UnsupportedTool {
                            reason: format!(
                                "{} is not executable through the local tool batch executor",
                                tool_call.tool_name
                            ),
                            tool_call,
                        }
                    };
                    ToolBatchOperation::new(operation, definition.concurrency)
                }
                None => ToolBatchItem::UnsupportedTool {
                    reason: format!("{} is not registered in this session", tool_call.tool_name),
                    tool_call,
                }
                .into(),
            };
            operations.push(scheduled);
        }

        if operations.is_empty() {
            self.log_error(
                "empty_tool_batch_built",
                serde_json::json!({
                    "turn_id": turn_id,
                    "batch_id": batch_id,
                }),
            );
            return Err(SessionActorError::Tool(
                "tool calls produced an empty tool batch".to_string(),
            ));
        }

        Ok(ToolBatch::new_scheduled(batch_id, operations))
    }

    fn repair_unclosed_tool_calls_before_provider_request(
        &mut self,
        turn_id: &str,
        step_index: usize,
    ) -> Result<(), SessionActorError> {
        let unclosed_tool_calls = collect_unclosed_tool_calls(&self.history);
        if unclosed_tool_calls.is_empty() {
            return Ok(());
        }

        let reason = "previous tool call did not receive a tool result before the session continued; closing it locally so the provider can accept the next request";
        self.log_warn(
            "unclosed_tool_calls_repaired",
            serde_json::json!({
                "turn_id": turn_id,
                "step_index": step_index,
                "tool_call_count": unclosed_tool_calls.len(),
                "tool_calls": unclosed_tool_calls
                    .iter()
                    .map(|tool_call| serde_json::json!({
                        "tool_call_id": tool_call.tool_call_id,
                        "tool_name": tool_call.tool_name,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
        let mut tool_message = ChatMessage::new(
            ChatRole::Assistant,
            unclosed_tool_calls
                .iter()
                .map(|tool_call| {
                    ChatMessageItem::ToolResult(tool_error_result_for_tool_call(tool_call, reason))
                })
                .collect(),
        );
        stamp_assistant_message_time(&mut tool_message);
        let tool_message_index =
            self.append_history_message("tool_result_repair", tool_message.clone())?;
        self.emit(SessionEvent::MessageAppended {
            index: tool_message_index,
            message: tool_message,
        })
    }

    fn clear_current_plan(&mut self) -> Result<(), SessionActorError> {
        self.history
            .retain(|message| !is_task_plan_context_message(message));
        if self.current_plan.take().is_some() {
            self.emit(SessionEvent::PlanUpdated { plan: None })?;
        }
        self.persist_state_if_history_closed("clear_plan")?;
        Ok(())
    }

    fn handle_provider_event(
        &mut self,
        event: ProviderEvent,
    ) -> Result<SessionActorStep, SessionActorError> {
        let Some(active_request_id) = self
            .active_provider_request
            .as_ref()
            .map(|active| active.request_id.clone())
        else {
            return Ok(SessionActorStep::Idle);
        };
        match event {
            ProviderEvent::Stream { request_id, event } => {
                if request_id == active_request_id && provider_stream_event_is_renderable(&event) {
                    let Some(active) = self.active_provider_request.as_mut() else {
                        return Ok(SessionActorStep::Idle);
                    };
                    let turn_id = active.turn_id.clone();
                    let message_id = active.message_id.clone();
                    let in_message_index = active.next_stream_event_index;
                    active.next_stream_event_index =
                        active.next_stream_event_index.saturating_add(1);
                    active.last_activity_at = Instant::now();
                    self.emit_provider_stream_event(
                        &message_id,
                        &turn_id,
                        in_message_index,
                        event,
                    )?;
                    self.wake_or_schedule_provider_supersede_if_needed()?;
                }
                Ok(SessionActorStep::WaitingProviderRequest)
            }
            ProviderEvent::Retry {
                request_id,
                retry,
                max_retries,
                delay_ms,
                error,
            } => {
                if request_id == active_request_id {
                    let Some(active) = self.active_provider_request.as_ref() else {
                        return Ok(SessionActorStep::Idle);
                    };
                    self.log_info(
                        "provider_request_retrying",
                        serde_json::json!({
                            "turn_id": active.turn_id,
                            "step_index": active.step_index,
                            "retry": retry,
                            "max_retries": max_retries,
                            "delay_ms": delay_ms,
                            "error": error,
                        }),
                    );
                }
                self.wake_or_schedule_provider_supersede_if_needed()?;
                Ok(SessionActorStep::WaitingProviderRequest)
            }
            ProviderEvent::Result { request_id, result } => {
                if request_id != active_request_id {
                    self.log_warn(
                        "stale_provider_request_completion_ignored",
                        serde_json::json!({
                            "request_id": request_id,
                            "active_request_id": active_request_id,
                        }),
                    );
                    return Ok(SessionActorStep::WaitingProviderRequest);
                }
                let active = self
                    .active_provider_request
                    .take()
                    .expect("active provider request should still exist");
                self.disarm_provider_supersede_grace_event();
                match result {
                    Ok(message) => {
                        self.process_provider_message(active, message)?;
                    }
                    Err(error)
                        if error.is_request_too_large()
                            && active.request_too_large_attempts
                                < REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS =>
                    {
                        let next_attempt = active.request_too_large_attempts.saturating_add(1);
                        if self.compact_history_after_request_too_large(
                            "provider_request",
                            Some(&active.turn_id),
                            Some(active.step_index),
                            &error.to_string(),
                        )? {
                            self.start_provider_request(
                                active.turn_id,
                                active.turn_number,
                                active.step_index,
                                next_attempt,
                            )?;
                        } else {
                            self.finish_turn_error(
                                &active.turn_id,
                                SessionActorError::from_provider_error(error),
                                Some(PendingContinuation::CurrentHistory),
                            )?;
                        }
                    }
                    Err(error) => {
                        let actor_error = SessionActorError::from_provider_error(error);
                        let error_detail = actor_error.detail();
                        let error_text = error_detail.reason.clone();
                        self.emit(SessionEvent::StreamError {
                            message_id: active.message_id.clone(),
                            turn_id: active.turn_id.clone(),
                            in_message_index: active.next_stream_event_index,
                            item_id: None,
                            message_index: None,
                            error: error_text.clone(),
                            error_detail,
                        })?;
                        self.finish_turn_error(
                            &active.turn_id,
                            actor_error,
                            Some(PendingContinuation::CurrentHistory),
                        )?;
                    }
                }
                Ok(if self.active_provider_request.is_some() {
                    SessionActorStep::WaitingProviderRequest
                } else if self.active_tool_batch.is_some() {
                    SessionActorStep::WaitingToolBatch
                } else {
                    SessionActorStep::ProcessedData
                })
            }
        }
    }

    fn handle_tool_progress_event(
        &mut self,
        progress: ToolBatchProgress,
    ) -> Result<SessionActorStep, SessionActorError> {
        let Some(active) = self.active_tool_batch.as_ref() else {
            return Ok(SessionActorStep::Idle);
        };
        if progress.batch_id != active.handle.batch_id {
            self.log_warn(
                "stale_tool_batch_progress_ignored",
                serde_json::json!({
                    "batch_id": progress.batch_id,
                    "active_batch_id": active.handle.batch_id,
                }),
            );
            return Ok(SessionActorStep::WaitingToolBatch);
        }
        self.emit(SessionEvent::StreamToolResultDone {
            turn_id: active.turn_id.clone(),
            batch_id: progress.batch_id,
            tool_result: progress.result,
        })?;
        Ok(SessionActorStep::WaitingToolBatch)
    }

    fn handle_tool_completion_event(
        &mut self,
        completion: ToolBatchCompletion,
    ) -> Result<SessionActorStep, SessionActorError> {
        while let Ok(progress) = self.tool_progress_rx.try_recv() {
            self.handle_tool_progress_event(progress)?;
        }
        let Some(active) = self.active_tool_batch.as_ref() else {
            return Ok(SessionActorStep::Idle);
        };
        if completion.batch_id != active.handle.batch_id {
            return Err(SessionActorError::Tool(format!(
                "unexpected tool batch completion {}, expected {}",
                completion.batch_id, active.handle.batch_id
            )));
        }

        let active = self
            .active_tool_batch
            .take()
            .expect("active tool batch should still exist");
        self.tool_executor
            .finish(&active.handle.batch_id)
            .map_err(|error| SessionActorError::Tool(error.to_string()))?;
        let mut tool_message = match completion.result {
            Ok(message) => message,
            Err(error) => {
                self.log_warn(
                    "tool_batch_completion_failed",
                    serde_json::json!({
                        "turn_id": &active.turn_id,
                        "batch_id": &active.handle.batch_id,
                        "error": error,
                        "synthetic_tool_results": active.operations.len(),
                    }),
                );
                tool_error_message_for_operations(
                    &active.operations,
                    format!("tool batch failed before returning results: {error}"),
                )
            }
        };
        stamp_assistant_message_time(&mut tool_message);
        self.log_info(
            "tool_batch_completed",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "result_items": tool_message.data.len(),
            }),
        );
        let mark_started_at = Instant::now();
        self.log_info(
            "tool_result_mark_skills_started",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "turn_number": active.turn_number,
                "result_items": tool_message.data.len(),
            }),
        );
        let marked_skill_count =
            self.mark_loaded_skills_from_message(&tool_message, active.turn_number)?;
        self.log_info(
            "tool_result_mark_skills_completed",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "marked_skill_count": marked_skill_count,
                "elapsed_ms": mark_started_at.elapsed().as_millis(),
            }),
        );
        let append_started_at = Instant::now();
        self.log_info(
            "tool_result_append_started",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
            }),
        );
        let tool_message_index =
            self.append_history_message("tool_result", tool_message.clone())?;
        self.log_info(
            "tool_result_append_completed",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "message_index": tool_message_index,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "elapsed_ms": append_started_at.elapsed().as_millis(),
            }),
        );
        let emit_started_at = Instant::now();
        self.log_info(
            "tool_result_emit_started",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "message_index": tool_message_index,
            }),
        );
        self.emit(SessionEvent::MessageAppended {
            index: tool_message_index,
            message: tool_message,
        })?;
        self.log_info(
            "tool_result_emit_completed",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "message_index": tool_message_index,
                "elapsed_ms": emit_started_at.elapsed().as_millis(),
            }),
        );
        if active.interrupt == Some(ToolBatchInterrupt::SupersededByUserMessage) {
            self.log_info(
                "tool_batch_superseded_by_user_message",
                serde_json::json!({
                    "turn_id": &active.turn_id,
                    "batch_id": &active.handle.batch_id,
                }),
            );
            self.mark_turn_returned(active.turn_number);
            return Ok(SessionActorStep::ProcessedData);
        }
        if let Err(error) = self.start_provider_request(
            active.turn_id.clone(),
            active.turn_number,
            active.step_index + 1,
            0,
        ) {
            self.finish_turn_error(
                &active.turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            )?;
        }
        Ok(if self.active_tool_batch.is_some() {
            SessionActorStep::WaitingToolBatch
        } else if self.active_provider_request.is_some() {
            SessionActorStep::WaitingProviderRequest
        } else {
            SessionActorStep::ProcessedData
        })
    }

    fn emit(&self, event: SessionEvent) -> Result<(), SessionActorError> {
        self.log_info("event_emitted", session_event_summary(&event));
        self.event_sink
            .emit(event)
            .map_err(SessionActorError::Event)
    }

    fn emit_turn_failed(
        &self,
        error: String,
        error_detail: SessionErrorDetail,
        can_continue: bool,
    ) -> Result<(), SessionActorError> {
        self.emit(SessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        })
    }

    fn emit_compact_failed(
        &self,
        phase: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<(), SessionActorError> {
        self.emit(SessionEvent::CompactFailed {
            phase: phase.into(),
            reason: reason.into(),
        })
    }

    fn append_history_message(
        &mut self,
        phase: &str,
        mut message: ChatMessage,
    ) -> Result<usize, SessionActorError> {
        let append_started_at = Instant::now();
        let index = self.all_messages.len();
        if !message_id_has_all_messages_index(&message.message_id) {
            message.message_id = message_id_for_all_messages_index(index);
        }
        self.log_info(
            "append_history_message_started",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "message_role": message.role,
                "message_items": message.data.len(),
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "has_compressor": self.compressor.is_some(),
            }),
        );
        let Some(compressor) = self.compressor.clone() else {
            self.all_messages.push(message.clone());
            self.history.push(message);
            self.persist_state_if_history_closed(phase)?;
            self.log_info(
                "append_history_message_completed",
                serde_json::json!({
                    "phase": phase,
                    "index": index,
                    "mode": "no_compressor",
                    "history_len": self.history.len(),
                    "all_messages_len": self.all_messages.len(),
                    "elapsed_ms": append_started_at.elapsed().as_millis(),
                }),
            );
            return Ok(index);
        };

        self.all_messages.push(message.clone());
        if append_phase_should_defer_compression(phase) {
            self.log_info(
                "append_history_message_compression_deferred",
                serde_json::json!({
                    "phase": phase,
                    "index": index,
                    "history_len": self.history.len(),
                    "all_messages_len": self.all_messages.len(),
                }),
            );
            self.history.push(message);
            self.persist_state_if_history_closed(phase)?;
            self.log_info(
                "append_history_message_completed",
                serde_json::json!({
                    "phase": phase,
                    "index": index,
                    "mode": "compression_deferred",
                    "history_len": self.history.len(),
                    "all_messages_len": self.all_messages.len(),
                    "elapsed_ms": append_started_at.elapsed().as_millis(),
                }),
            );
            return Ok(index);
        }
        let system_prompt_started_at = Instant::now();
        self.log_info(
            "append_history_system_prompt_started",
            serde_json::json!({
                "phase": phase,
                "index": index,
            }),
        );
        let system_prompt = self.system_prompt_for_current_initial()?;
        self.log_info(
            "append_history_system_prompt_completed",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "has_system_prompt": system_prompt.is_some(),
                "elapsed_ms": system_prompt_started_at.elapsed().as_millis(),
            }),
        );
        let would_compress_started_at = Instant::now();
        let would_compress = compressor
            .would_compress_with_next(&self.history, &message)
            .unwrap_or(false);
        self.log_info(
            "append_history_would_compress_checked",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "would_compress": would_compress,
                "elapsed_ms": would_compress_started_at.elapsed().as_millis(),
            }),
        );
        if would_compress {
            self.flush_all_messages_before_compression(phase)?;
        }
        let compression_started_at = Instant::now();
        self.log_info(
            "append_history_compression_append_started",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
            }),
        );
        let report = {
            let mut request_too_large_attempts = 0usize;
            loop {
                let compression_context =
                    self.compression_memory_context_for_append(&compressor, &message);
                match compressor.append_with_compression_with_tools(
                    &mut self.history,
                    message.clone(),
                    self.provider.as_ref(),
                    &self.model_config,
                    system_prompt.as_deref(),
                    compression_context.as_deref(),
                    self.tool_catalog
                        .iter()
                        .map(|(_, tool)| tool)
                        .collect::<Vec<_>>(),
                ) {
                    Ok(report) => break report,
                    Err(error)
                        if compression_error_is_request_too_large(&error)
                            && request_too_large_attempts
                                < REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS =>
                    {
                        request_too_large_attempts += 1;
                        if self.prune_history_after_request_too_large(
                            phase,
                            None,
                            None,
                            &error.to_string(),
                        )? {
                            continue;
                        }
                        let reason = error.to_string();
                        self.emit_compact_failed(phase, reason.clone())?;
                        return Err(SessionActorError::Compression(reason));
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        self.emit_compact_failed(phase, reason.clone())?;
                        return Err(SessionActorError::Compression(reason));
                    }
                }
            }
        };
        self.log_info(
            "append_history_compression_append_completed",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "compressed": report.compressed,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "elapsed_ms": compression_started_at.elapsed().as_millis(),
            }),
        );
        self.log_compression_report(phase, &report);
        if report.compressed {
            self.runtime_metadata_state
                .promote_notified_components_to_system_snapshot();
        }
        self.persist_state_if_history_closed(phase)?;
        self.log_info(
            "append_history_message_completed",
            serde_json::json!({
                "phase": phase,
                "index": index,
                "mode": "compression_checked",
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "elapsed_ms": append_started_at.elapsed().as_millis(),
            }),
        );
        Ok(index)
    }

    fn compression_memory_context_for_append(
        &self,
        compressor: &SessionCompressor,
        next_message: &ChatMessage,
    ) -> Option<String> {
        if !compressor
            .would_compress_with_next(&self.history, next_message)
            .unwrap_or(false)
        {
            return None;
        }
        self.compression_memory_context(&self.history, Some(next_message))
    }

    fn compression_memory_context(
        &self,
        messages: &[ChatMessage],
        next_message: Option<&ChatMessage>,
    ) -> Option<String> {
        if !self.initial.as_ref()?.memory_enabled {
            return None;
        }
        let bridge = self.conversation_bridge.as_ref()?;
        let query =
            build_compression_memory_query(messages, next_message, self.current_plan.as_ref());
        if query.trim().is_empty() {
            return None;
        }
        let mut results = Vec::new();
        results.extend(self.search_memory_scope_for_compression(bridge, "conversation", &query));
        results.extend(self.search_memory_scope_for_compression(bridge, "public", &query));
        render_compression_memory_context(results, self.compression_memory_budget_tokens())
    }

    fn search_memory_scope_for_compression(
        &self,
        bridge: &Arc<dyn ConversationBridge + Send + Sync>,
        scope: &str,
        query: &str,
    ) -> Vec<MemorySearchToolResult> {
        let request = ConversationBridgeRequest {
            request_id: format!("compression_memory_{scope}"),
            tool_call_id: format!("compression_memory_{scope}"),
            tool_name: "memory_search".to_string(),
            action: "memory_search".to_string(),
            payload: serde_json::json!({
                "query": query,
                "limit": COMPRESSION_MEMORY_SCOPE_CANDIDATE_LIMIT,
                "scopes": [scope],
            }),
        };
        let response = match bridge.call(request) {
            Ok(response) => response,
            Err(error) => {
                self.log_info(
                    "compression_memory_lookup_failed",
                    serde_json::json!({"scope": scope, "error": error.to_string()}),
                );
                return Vec::new();
            }
        };
        let rendered = crate::session_actor::tool_result_text(&response.result);
        if rendered.trim().is_empty() {
            return Vec::new();
        }
        let Ok(search) = serde_json::from_str::<MemorySearchToolResponse>(&rendered) else {
            self.log_info(
                "compression_memory_parse_failed",
                serde_json::json!({"scope": scope}),
            );
            return Vec::new();
        };
        if search.status.as_deref() != Some("success") {
            return Vec::new();
        }
        search.results
    }

    fn compression_memory_budget_tokens(&self) -> usize {
        let ratio_budget = (self.model_config.token_max_context.saturating_mul(3) / 100).max(1);
        ratio_budget.min(COMPRESSION_MEMORY_CONTEXT_MAX_TOKENS) as usize
    }

    fn append_runtime_synthetic_messages(
        &mut self,
        input_message: &ChatMessage,
    ) -> Result<(), SessionActorError> {
        let notices = self
            .runtime_metadata_state
            .observe_for_user_turn_from_workspace(
                &self
                    .workspace_root()
                    .map_err(SessionActorError::RuntimeMetadata)?,
                &self
                    .data_root()
                    .map_err(SessionActorError::RuntimeMetadata)?,
                self.initial
                    .as_ref()
                    .map(|initial| remote_aliases_prompt_for_mode(&initial.tool_remote_mode))
                    .unwrap_or_default(),
                self.initial
                    .as_ref()
                    .and_then(|initial| initial.remote_workspace_instructions.clone())
                    .unwrap_or_default(),
                self.initial
                    .as_ref()
                    .is_some_and(|initial| initial.memory_enabled),
            )
            .map_err(SessionActorError::RuntimeMetadata)?;
        for notice in notices {
            self.append_history_message(
                "runtime_synthetic",
                ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text: notice })],
                ),
            )?;
        }
        if let Some(notice) = render_incoming_user_metadata_notice(input_message) {
            self.append_history_message(
                "runtime_synthetic",
                ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text: notice })],
                ),
            )?;
        }
        Ok(())
    }

    fn mark_loaded_skills_from_message(
        &mut self,
        message: &ChatMessage,
        turn_number: u64,
    ) -> Result<usize, SessionActorError> {
        let skill_names = loaded_skill_names_from_message(message);
        if skill_names.is_empty() {
            return Ok(0);
        }
        let marked_skill_count = skill_names.len();
        self.runtime_metadata_state
            .mark_loaded_skills(&skill_names, turn_number);
        Ok(marked_skill_count)
    }

    fn restore_persisted_state(
        &mut self,
        saved: SessionActorPersistedState,
        incoming_initial: SessionInitial,
    ) -> Result<(), SessionActorError> {
        self.history = saved.current_messages;
        self.all_messages = saved.all_messages;
        stamp_missing_assistant_message_times(&mut self.history);
        stamp_missing_assistant_message_times(&mut self.all_messages);
        self.next_turn_id = saved.next_turn_id.max(1);
        self.next_batch_id = saved.next_batch_id.max(1);
        self.runtime_metadata_state = saved.runtime_metadata_state;
        if self.runtime_metadata_state.prompt_components.is_empty() {
            self.runtime_metadata_state
                .initialize_from_workspace(
                    &self
                        .workspace_root()
                        .map_err(SessionActorError::RuntimeMetadata)?,
                    &self
                        .data_root()
                        .map_err(SessionActorError::RuntimeMetadata)?,
                    remote_aliases_prompt_for_mode(&incoming_initial.tool_remote_mode),
                    incoming_initial
                        .remote_workspace_instructions
                        .clone()
                        .unwrap_or_default(),
                    incoming_initial.memory_enabled,
                )
                .map_err(SessionActorError::RuntimeMetadata)?;
        } else {
            self.runtime_metadata_state.initialize_missing_component(
                REMOTE_WORKSPACE_PROMPT_COMPONENT,
                incoming_initial
                    .remote_workspace_instructions
                    .clone()
                    .unwrap_or_default(),
            );
        }
        Ok(())
    }

    fn persist_state(&self) -> Result<(), SessionActorError> {
        let (Some(store), Some(initial)) = (&self.state_store, &self.initial) else {
            return Ok(());
        };
        store
            .save(&SessionActorPersistedState {
                version: 1,
                initial: initial.clone(),
                all_messages: self.all_messages.clone(),
                current_messages: self.history.clone(),
                next_turn_id: self.next_turn_id,
                next_batch_id: self.next_batch_id,
                runtime_metadata_state: self.runtime_metadata_state.clone(),
            })
            .map_err(SessionActorError::Persistence)
    }

    fn flush_all_messages_before_compression(&self, phase: &str) -> Result<(), SessionActorError> {
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        store
            .save_all_messages_jsonl(&self.all_messages)
            .map_err(SessionActorError::Persistence)?;
        self.log_info(
            "all_messages_flushed_before_compression",
            serde_json::json!({
                "phase": phase,
                "all_messages_len": self.all_messages.len(),
            }),
        );
        Ok(())
    }

    fn persist_state_if_history_closed(&self, phase: &str) -> Result<(), SessionActorError> {
        let open_tool_call_count = count_unclosed_tool_calls(&self.history);
        if open_tool_call_count > 0 {
            self.log_info(
                "session_state_persist_skipped",
                serde_json::json!({
                    "phase": phase,
                    "reason": "unclosed_tool_calls",
                    "open_tool_call_count": open_tool_call_count,
                    "history_len": self.history.len(),
                    "all_messages_len": self.all_messages.len(),
                }),
            );
            return Ok(());
        }
        let started_at = Instant::now();
        self.log_info(
            "session_state_persist_started",
            serde_json::json!({
                "phase": phase,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
            }),
        );
        match self.persist_state() {
            Ok(()) => {
                self.log_info(
                    "session_state_persist_completed",
                    serde_json::json!({
                        "phase": phase,
                        "history_len": self.history.len(),
                        "all_messages_len": self.all_messages.len(),
                        "elapsed_ms": started_at.elapsed().as_millis(),
                    }),
                );
                Ok(())
            }
            Err(error) => {
                self.log_error(
                    "session_state_persist_failed",
                    serde_json::json!({
                        "phase": phase,
                        "history_len": self.history.len(),
                        "all_messages_len": self.all_messages.len(),
                        "elapsed_ms": started_at.elapsed().as_millis(),
                        "error": error.to_string(),
                    }),
                );
                Err(error)
            }
        }
    }

    fn log_compression_report(&self, phase: &str, report: &CompressionReport) {
        if !report.compressed {
            return;
        }

        self.log_info(
            "history_compressed",
            serde_json::json!({
                "phase": phase,
                "estimated_tokens_before": report.estimated_tokens_before,
                "estimated_tokens_after": report.estimated_tokens_after,
                "threshold_tokens": report.threshold_tokens,
                "retained_message_count": report.retained_message_count,
                "compressed_message_count": report.compressed_message_count,
                "history_len": self.history.len(),
            }),
        );
    }

    fn mark_turn_returned(&mut self, turn_number: u64) {
        self.last_agent_returned_at = Some(Instant::now());
        self.last_completed_turn_number = self.last_completed_turn_number.max(turn_number);
    }

    fn prune_history_after_request_too_large(
        &mut self,
        phase: &str,
        turn_id: Option<&str>,
        step_index: Option<usize>,
        error: &str,
    ) -> Result<bool, SessionActorError> {
        let before_len = self.history.len();
        let Some(prune_start) = request_too_large_prune_start(&self.history) else {
            self.log_warn(
                "request_too_large_history_prune_failed",
                serde_json::json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "history_len": before_len,
                    "error": error,
                }),
            );
            return Ok(false);
        };

        self.history.drain(..prune_start);
        let retained_len = self.history.len();
        self.log_warn(
            "request_too_large_history_pruned",
            serde_json::json!({
                "phase": phase,
                "turn_id": turn_id,
                "step_index": step_index,
                "dropped_message_count": prune_start,
                "retained_message_count": retained_len,
                "history_len_before": before_len,
                "history_len_after": retained_len,
                "data_loss": true,
                "bug": true,
                "error": error,
            }),
        );
        self.emit(SessionEvent::Progress {
            message: format!(
                "警告：上游拒绝了本轮请求，原因是请求体过大。系统已从当前上下文中丢弃较早的 {prune_start} 条消息并自动重试；这代表发生了上下文数据丢失，应按 bug 处理并排查。"
            ),
            plan: self.current_plan.clone(),
        })?;
        self.persist_state_if_history_closed("request_too_large_prune")?;
        Ok(true)
    }

    fn compact_history_after_request_too_large(
        &mut self,
        phase: &str,
        turn_id: Option<&str>,
        step_index: Option<usize>,
        error: &str,
    ) -> Result<bool, SessionActorError> {
        if count_unclosed_tool_calls(&self.history) > 0 {
            self.log_warn(
                "request_too_large_compaction_skipped",
                serde_json::json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "reason": "unclosed_tool_calls",
                    "history_len": self.history.len(),
                    "error": error,
                }),
            );
            return Ok(false);
        }
        let Some(compressor) = self.compressor.clone() else {
            self.log_warn(
                "request_too_large_compaction_skipped",
                serde_json::json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "reason": "compressor_unavailable",
                    "history_len": self.history.len(),
                    "error": error,
                }),
            );
            return Ok(false);
        };

        self.log_warn(
            "request_too_large_compaction_started",
            serde_json::json!({
                "phase": phase,
                "turn_id": turn_id,
                "step_index": step_index,
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "error": error,
            }),
        );
        let system_prompt = self.system_prompt_for_current_initial()?;
        let compression_context = self.compression_memory_context(&self.history, None);
        let started_at = Instant::now();
        match compressor.compact_now_with_tools(
            &mut self.history,
            self.provider.as_ref(),
            &self.model_config,
            system_prompt.as_deref(),
            compression_context.as_deref(),
            self.tool_catalog
                .iter()
                .map(|(_, tool)| tool)
                .collect::<Vec<_>>(),
        ) {
            Ok(report) => {
                self.log_compression_report("request_too_large", &report);
                self.log_warn(
                    "request_too_large_compaction_completed",
                    serde_json::json!({
                        "phase": phase,
                        "turn_id": turn_id,
                        "step_index": step_index,
                        "compressed": report.compressed,
                        "estimated_tokens_before": report.estimated_tokens_before,
                        "estimated_tokens_after": report.estimated_tokens_after,
                        "threshold_tokens": report.threshold_tokens,
                        "retained_message_count": report.retained_message_count,
                        "compressed_message_count": report.compressed_message_count,
                        "history_len": self.history.len(),
                        "elapsed_ms": started_at.elapsed().as_millis(),
                    }),
                );
                if report.compressed {
                    self.runtime_metadata_state
                        .promote_notified_components_to_system_snapshot();
                    self.persist_state_if_history_closed("request_too_large_compaction")?;
                }
                self.emit(SessionEvent::CompactCompleted {
                    compressed: report.compressed,
                    estimated_tokens_before: report.estimated_tokens_before,
                    estimated_tokens_after: report.estimated_tokens_after,
                    threshold_tokens: report.threshold_tokens,
                    retained_message_count: report.retained_message_count,
                    compressed_message_count: report.compressed_message_count,
                })?;
                Ok(report.compressed)
            }
            Err(error) => {
                self.log_error(
                    "request_too_large_compaction_failed",
                    serde_json::json!({
                        "phase": phase,
                        "turn_id": turn_id,
                        "step_index": step_index,
                        "error": error.to_string(),
                        "history_len": self.history.len(),
                        "elapsed_ms": started_at.elapsed().as_millis(),
                    }),
                );
                self.emit_compact_failed(
                    "request_too_large_compaction",
                    format!("context compression before provider retry failed: {error}"),
                )?;
                Ok(false)
            }
        }
    }

    fn provider_request_token_estimate(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Option<TokenEstimate>, SessionActorError> {
        if self.model_config.token_max_context == 0 {
            return Ok(None);
        }
        let Some(estimator) = self.token_estimator.as_ref() else {
            return Ok(None);
        };
        let estimate = estimator
            .estimate(messages)
            .map_err(|error| SessionActorError::Compression(error.to_string()))?;
        Ok(Some(estimate))
    }

    fn log_info(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.info(event, data);
        }
    }

    fn log_warn(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.warn(event, data);
        }
    }

    fn log_error(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.error(event, data);
        }
    }

    fn allocate_turn_id(&mut self) -> String {
        let id = format!("turn_{}", self.next_turn_id);
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        id
    }

    fn allocate_batch_id(&mut self, turn_id: &str) -> String {
        let id = format!("{}_batch_{}", turn_id, self.next_batch_id);
        self.next_batch_id = self.next_batch_id.saturating_add(1);
        id
    }

    fn workspace_root(&self) -> Result<PathBuf, String> {
        env::current_dir().map_err(|error| format!("failed to resolve cwd: {error}"))
    }

    fn data_root(&self) -> Result<PathBuf, String> {
        match env::var_os("STELLACLAW_DATA_ROOT") {
            Some(value) => Ok(PathBuf::from(value)),
            None => self.workspace_root(),
        }
    }
}

pub trait SessionActorEventSink: Send + Sync + 'static {
    fn emit(&self, event: SessionEvent) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActorStep {
    Idle,
    ProcessedControl,
    ProcessedData,
    WaitingProviderRequest,
    WaitingToolBatch,
    Shutdown,
}

#[derive(Debug, Error)]
pub enum SessionActorError {
    #[error("session actor mailbox failed: {0}")]
    Mailbox(String),
    #[error("session actor event sink failed: {0}")]
    Event(String),
    #[error("provider request failed in {module}: {reason}")]
    ProviderPreflight {
        module: &'static str,
        reason: String,
    },
    #[error("provider request failed in {module}: {reason}")]
    Provider {
        module: &'static str,
        kind: &'static str,
        reason: String,
        #[source]
        source: ProviderError,
    },
    #[error("tool batch failed: {0}")]
    Tool(String),
    #[error("tool catalog failed: {0}")]
    ToolCatalog(String),
    #[error("session actor logging failed: {0}")]
    Logging(String),
    #[error("session actor compression failed: {0}")]
    Compression(String),
    #[error("session actor persistence failed: {0}")]
    Persistence(String),
    #[error("session actor runtime metadata failed: {0}")]
    RuntimeMetadata(String),
    #[error("data mailbox head changed while starting a turn")]
    DataHeadChanged,
    #[error("unexpected request in data mailbox")]
    UnexpectedDataRequest,
    #[error("session initial message has not been applied")]
    MissingInitial,
    #[error("session actor exceeded step limit {0}")]
    StepLimitExceeded(usize),
}

impl SessionActorError {
    fn provider_preflight(reason: impl Into<String>) -> Self {
        Self::ProviderPreflight {
            module: "session_actor.provider_preflight",
            reason: reason.into(),
        }
    }

    fn from_provider_error(error: ProviderError) -> Self {
        let (module, kind, reason) = provider_error_parts(&error);
        Self::Provider {
            module,
            kind,
            reason,
            source: error,
        }
    }

    fn is_recoverable_turn_error(&self) -> bool {
        matches!(
            self,
            Self::Provider { .. }
                | Self::ProviderPreflight { .. }
                | Self::Compression(_)
                | Self::Tool(_)
                | Self::RuntimeMetadata(_)
        )
    }

    fn detail(&self) -> SessionErrorDetail {
        match self {
            Self::Mailbox(reason) => {
                SessionErrorDetail::new("session_actor.mailbox", "mailbox", reason.clone())
            }
            Self::Event(reason) => {
                SessionErrorDetail::new("session_actor.event_sink", "event_sink", reason.clone())
            }
            Self::ProviderPreflight { module, reason } => {
                SessionErrorDetail::new(*module, "request_too_large_preflight", reason.clone())
            }
            Self::Provider {
                module,
                kind,
                source,
                ..
            } => SessionErrorDetail::new(*module, *kind, source.to_string()),
            Self::Tool(reason) => {
                SessionErrorDetail::new("session_actor.tool_batch", "tool_batch", reason.clone())
            }
            Self::ToolCatalog(reason) => SessionErrorDetail::new(
                "session_actor.tool_catalog",
                "tool_catalog",
                reason.clone(),
            ),
            Self::Logging(reason) => {
                SessionErrorDetail::new("session_actor.logger", "logging", reason.clone())
            }
            Self::Compression(reason) => {
                SessionErrorDetail::new("session_actor.compression", "compression", reason.clone())
            }
            Self::Persistence(reason) => {
                SessionErrorDetail::new("session_actor.persistence", "persistence", reason.clone())
            }
            Self::RuntimeMetadata(reason) => SessionErrorDetail::new(
                "session_actor.runtime_metadata",
                "runtime_metadata",
                reason.clone(),
            ),
            Self::DataHeadChanged => SessionErrorDetail::new(
                "session_actor.mailbox",
                "data_head_changed",
                "data mailbox head changed while starting a turn",
            ),
            Self::UnexpectedDataRequest => SessionErrorDetail::new(
                "session_actor.mailbox",
                "unexpected_data_request",
                "unexpected request in data mailbox",
            ),
            Self::MissingInitial => SessionErrorDetail::new(
                "session_actor.initialization",
                "missing_initial",
                "session initial message has not been applied",
            ),
            Self::StepLimitExceeded(max_steps) => SessionErrorDetail::new(
                "session_actor.loop",
                "step_limit_exceeded",
                format!("session actor exceeded step limit {max_steps}"),
            ),
        }
    }
}

fn provider_error_parts(error: &ProviderError) -> (&'static str, &'static str, String) {
    match error {
        ProviderError::MissingApiKeyEnv(env) => (
            "provider.config",
            "missing_api_key_env",
            format!("missing api key in environment variable {env}"),
        ),
        ProviderError::BuildHttpClient(error) => (
            "provider.http_client",
            "build_http_client",
            error.to_string(),
        ),
        ProviderError::Request(message) => (
            "provider.http_transport",
            "request",
            concise_error_reason(message),
        ),
        ProviderError::HttpStatus { url, status, body } => (
            "provider.http_status",
            "http_status",
            format!(
                "request to {url} returned HTTP {status}: {}",
                concise_error_reason(body)
            ),
        ),
        ProviderError::DecodeResponse(error) => (
            "provider.response_body",
            "decode_response",
            error.to_string(),
        ),
        ProviderError::DecodeJson(error) => {
            ("provider.response_json", "decode_json", error.to_string())
        }
        ProviderError::InvalidResponse(message) => (
            "provider.response_validation",
            "invalid_response",
            concise_error_reason(message),
        ),
        ProviderError::ProviderFailure { kind, message, .. } => (
            "provider.failure",
            provider_failure_kind_label(*kind),
            concise_error_reason(message),
        ),
        ProviderError::WebSocket(message) => (
            "provider.websocket",
            "websocket",
            concise_error_reason(message),
        ),
        ProviderError::PersistOutput(error) => (
            "provider.output_persistor",
            "persist_output",
            error.to_string(),
        ),
        ProviderError::EmptyChoices => (
            "provider.response_validation",
            "empty_choices",
            "provider response did not include any completion choices".to_string(),
        ),
        ProviderError::Subprocess(message) => (
            "provider.runtime",
            "isolation",
            concise_error_reason(message),
        ),
    }
}

fn provider_failure_kind_label(kind: ProviderFailureKind) -> &'static str {
    match kind {
        ProviderFailureKind::RequestTooLarge => "request_too_large",
        ProviderFailureKind::RateLimited => "rate_limited",
        ProviderFailureKind::Authentication => "authentication",
        ProviderFailureKind::Permission => "permission",
        ProviderFailureKind::CyberPolicy => "cyber_policy",
        ProviderFailureKind::ProviderUnavailable => "provider_unavailable",
        ProviderFailureKind::Unknown => "unknown",
    }
}

fn tool_error_message_for_operations(
    operations: &[ToolBatchOperation],
    error: String,
) -> ChatMessage {
    ChatMessage::new(
        ChatRole::Assistant,
        operations
            .iter()
            .map(|operation| {
                ChatMessageItem::ToolResult(tool_error_result_for_operation(operation, &error))
            })
            .collect(),
    )
}

fn tool_error_result_for_operation(
    operation: &ToolBatchOperation,
    error: &str,
) -> super::ToolResultItem {
    let (tool_call_id, tool_name) = match &operation.item {
        ToolBatchItem::RegisteredTool(tool_call)
        | ToolBatchItem::UnsupportedTool { tool_call, .. } => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
    };
    super::ToolResultItem {
        tool_call_id,
        tool_name,
        result: tool_error_content(error),
    }
}

fn tool_error_result_for_tool_call(
    tool_call: &super::ToolCallItem,
    error: &str,
) -> super::ToolResultItem {
    super::ToolResultItem {
        tool_call_id: tool_call.tool_call_id.clone(),
        tool_name: tool_call.tool_name.clone(),
        result: tool_error_content(error),
    }
}

fn tool_error_content(error: &str) -> ToolResultContent {
    ToolResultContent::from_json(serde_json::json!({ "error": error }))
}

fn concise_error_reason(message: &str) -> String {
    const MAX_REASON_CHARS: usize = 600;
    let message = message.trim();
    let mut reason = String::new();
    for (index, ch) in message.chars().enumerate() {
        if index >= MAX_REASON_CHARS {
            reason.push_str("...");
            return reason;
        }
        reason.push(ch);
    }
    reason
}

fn build_session_token_estimator(model_config: &ModelConfig) -> Result<TokenEstimator, String> {
    let file_resolver = HuggingFaceFileResolver::new().map_err(|error| error.to_string())?;
    TokenEstimator::from_model_config(model_config, &file_resolver)
        .map_err(|error| error.to_string())
}

fn build_session_compressor(
    threshold_tokens: Option<u64>,
    initial: &SessionInitial,
    token_estimator: Option<&TokenEstimator>,
) -> Result<Option<SessionCompressor>, SessionActorError> {
    let Some(threshold_tokens) = threshold_tokens else {
        return Ok(None);
    };

    let retain_recent_tokens = initial
        .compression_retain_recent_tokens
        .unwrap_or_else(|| default_retain_recent_tokens(threshold_tokens));
    let estimator = token_estimator.cloned().ok_or_else(|| {
        SessionActorError::Compression("token estimator is unavailable".to_string())
    })?;
    let compressor = SessionCompressor::new(estimator, threshold_tokens, retain_recent_tokens)
        .map_err(|error| SessionActorError::Compression(error.to_string()))?;
    Ok(Some(compressor))
}

fn active_compression_threshold_tokens(
    model_config: &ModelConfig,
    initial: &SessionInitial,
) -> Option<u64> {
    let configured_threshold = initial.compression_threshold_tokens?;
    Some(
        configured_threshold
            .min(model_context_ratio_threshold(
                model_config,
                ACTIVE_COMPRESSION_THRESHOLD_RATIO,
            ))
            .max(1),
    )
}

fn model_context_ratio_threshold(model_config: &ModelConfig, ratio: f64) -> u64 {
    ((model_config.token_max_context as f64) * ratio)
        .floor()
        .max(1.0) as u64
}

fn default_retain_recent_tokens(threshold_tokens: u64) -> u64 {
    if threshold_tokens <= 2 {
        return 1;
    }
    ((threshold_tokens.saturating_mul(DEFAULT_RETAIN_RECENT_PERCENT)) / 100)
        .max(512)
        .min(threshold_tokens - 1)
}

fn collect_tool_calls(message: &ChatMessage) -> Vec<super::ToolCallItem> {
    message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

fn loaded_skill_names_from_message(message: &ChatMessage) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for item in &message.data {
        let ChatMessageItem::ToolResult(result) = item else {
            continue;
        };
        if result.tool_name != "skill_load" {
            continue;
        }
        let rendered = crate::session_actor::tool_result_text(result);
        if rendered.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&rendered) {
            if let Some(name) = value
                .get("name")
                .or_else(|| value.get("skill_name"))
                .and_then(serde_json::Value::as_str)
            {
                names.insert(name.to_string());
            }
        }
    }
    names.into_iter().collect()
}

fn count_unclosed_tool_calls(messages: &[ChatMessage]) -> usize {
    collect_unclosed_tool_calls(messages).len()
}

fn collect_unclosed_tool_calls(messages: &[ChatMessage]) -> Vec<super::ToolCallItem> {
    let mut open = std::collections::BTreeMap::new();
    let mut order = Vec::new();
    for message in messages {
        for item in &message.data {
            match item {
                ChatMessageItem::ToolCall(tool_call) => {
                    if !open.contains_key(&tool_call.tool_call_id) {
                        order.push(tool_call.tool_call_id.clone());
                    }
                    open.insert(tool_call.tool_call_id.clone(), tool_call.clone());
                }
                ChatMessageItem::ToolResult(tool_result) => {
                    open.remove(&tool_result.tool_call_id);
                }
                _ => {}
            }
        }
    }
    order
        .into_iter()
        .filter_map(|tool_call_id| open.remove(&tool_call_id))
        .collect()
}

fn build_compression_memory_query(
    messages: &[ChatMessage],
    next_message: Option<&ChatMessage>,
    current_plan: Option<&TaskPlanView>,
) -> String {
    let mut parts = Vec::new();
    if let Some(plan) = current_plan.and_then(render_task_plan_context) {
        parts.push(plan);
    }
    for message in messages.iter().rev().take(6).rev() {
        let text = message_text_for_memory_query(message);
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    if let Some(message) = next_message {
        let text = message_text_for_memory_query(message);
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    truncate_chars(&parts.join("\n\n"), 4_000)
}

fn message_text_for_memory_query(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => parts.push(context.text.trim().to_string()),
            ChatMessageItem::Compaction(compaction) => {
                if let Some(text) = compaction.generic_summary_text() {
                    parts.push(text.trim().to_string());
                }
            }
            ChatMessageItem::SelectionReference(selection) => {
                parts.push(selection.to_prompt_text());
            }
            ChatMessageItem::ToolResult(tool_result) => {
                let rendered = crate::session_actor::tool_result_text(tool_result);
                if !rendered.trim().is_empty() {
                    parts.push(rendered.trim().to_string());
                }
            }
            ChatMessageItem::ToolCall(tool_call) => {
                parts.push(format!(
                    "tool_call {} {}",
                    tool_call.tool_name, tool_call.arguments.text
                ));
            }
            ChatMessageItem::File(file) => {
                let name = file.name.as_deref().unwrap_or(file.uri.as_str());
                parts.push(format!("file {name} {}", file.uri));
            }
            ChatMessageItem::Reasoning(_) => {}
        }
    }
    truncate_chars(&parts.join("\n"), 1_200)
}

fn render_compression_memory_context(
    mut results: Vec<MemorySearchToolResult>,
    budget_tokens: usize,
) -> Option<String> {
    if results.is_empty() || budget_tokens == 0 {
        return None;
    }
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                right
                    .updated_at
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(left.updated_at.as_deref().unwrap_or_default())
            })
    });

    let mut selected = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut used_chars = 0usize;
    let budget_chars = budget_tokens.saturating_mul(4).max(1);
    for result in results {
        if !seen.insert(result.id.clone()) {
            continue;
        }
        let text = truncate_chars(
            &result.text.replace('\n', " "),
            COMPRESSION_MEMORY_ENTRY_MAX_CHARS,
        );
        let subject = result
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" ({value})"))
            .unwrap_or_default();
        let updated = result
            .updated_at
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" updated {value}"))
            .unwrap_or_default();
        let line = format!(
            "* [{}:{}]{}{} {}",
            result.scope, result.id, subject, updated, text
        );
        let line_len = line.len() + 1;
        if used_chars + line_len > budget_chars {
            break;
        }
        used_chars += line_len;
        selected.push(line);
    }
    if selected.is_empty() {
        return None;
    }
    Some(selected.join("\n"))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.trim().chars().enumerate() {
        if index >= max_chars {
            output.push_str("...");
            return output;
        }
        output.push(ch);
    }
    output
}

fn compression_error_is_request_too_large(error: &CompressionError) -> bool {
    request_too_large_text(&error.to_string())
}

fn append_phase_should_defer_compression(phase: &str) -> bool {
    phase.starts_with("tool_result")
}

fn request_too_large_prune_start(messages: &[ChatMessage]) -> Option<usize> {
    if messages.len() <= 1 {
        return None;
    }

    let target = (messages.len() / 2).max(1);
    (target..messages.len()).find(|&start| is_tool_protocol_closed_suffix(&messages[start..]))
}

fn current_time_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn message_id_for_all_messages_index(index: usize) -> String {
    format!("msg_{index:020}_{:016x}", rand::random::<u64>())
}

fn message_id_has_all_messages_index(message_id: &str) -> bool {
    let Some(rest) = message_id.strip_prefix("msg_") else {
        return false;
    };
    let Some((index, _suffix)) = rest.split_once('_') else {
        return false;
    };
    index.len() == 20 && index.bytes().all(|byte| byte.is_ascii_digit())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn stamp_assistant_message_time(message: &mut ChatMessage) {
    if message.role == ChatRole::Assistant && message.message_time.is_none() {
        message.message_time = Some(now_rfc3339());
    }
}

fn stamp_missing_assistant_message_times(messages: &mut [ChatMessage]) {
    for message in messages {
        stamp_assistant_message_time(message);
    }
}

fn is_tool_protocol_closed_suffix(messages: &[ChatMessage]) -> bool {
    let mut open = std::collections::BTreeSet::new();
    for message in messages {
        for item in &message.data {
            match item {
                ChatMessageItem::ToolCall(tool_call) => {
                    open.insert(tool_call.tool_call_id.clone());
                }
                ChatMessageItem::ToolResult(tool_result) => {
                    if !open.remove(&tool_result.tool_call_id) {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    open.is_empty()
}

fn render_incoming_user_metadata_notice(message: &ChatMessage) -> Option<String> {
    if message.role != ChatRole::User {
        return None;
    }
    let mut lines = vec!["[Incoming User Metadata]".to_string()];
    let mut has_metadata = false;
    if let Some(user_name) = message.user_name.as_deref().map(str::trim) {
        if !user_name.is_empty() {
            lines.push(format!("Speaker: {user_name}"));
            has_metadata = true;
        }
    }
    if let Some(message_time) = message.message_time.as_deref().map(str::trim) {
        if !message_time.is_empty() {
            lines.push(format!("Message time: {message_time}"));
            has_metadata = true;
        }
    }
    if !has_metadata {
        return None;
    }
    lines.push("For the next user message only.".to_string());
    Some(lines.join("\n"))
}

fn session_request_kind(request: &SessionRequest) -> &'static str {
    match request {
        SessionRequest::Initial { .. } => "initial",
        SessionRequest::EnqueueUserMessage { .. } => "enqueue_user_message",
        SessionRequest::EnqueueActorMessage { .. } => "enqueue_actor_message",
        SessionRequest::CancelTurn { .. } => "cancel_turn",
        SessionRequest::ContinueTurn { .. } => "continue_turn",
        SessionRequest::CompactNow => "compact_now",
        SessionRequest::ResolveHostCoordination { .. } => "resolve_host_coordination",
        SessionRequest::QuerySessionView { .. } => "query_session_view",
        SessionRequest::QueryMessageHistory { .. } => "query_message_history",
        SessionRequest::QueryMessageDetail { .. } => "query_message_detail",
        SessionRequest::Shutdown => "shutdown",
    }
}

fn provider_stream_event_is_renderable(event: &ProviderStreamEvent) -> bool {
    matches!(
        event,
        ProviderStreamEvent::OutputTextDelta { .. }
            | ProviderStreamEvent::ToolCallInputDelta { .. }
            | ProviderStreamEvent::ReasoningSummaryDelta { .. }
            | ProviderStreamEvent::ReasoningSummaryPartAdded { .. }
    )
}

fn session_event_summary(event: &SessionEvent) -> serde_json::Value {
    match event {
        SessionEvent::MessageAppended { index, message } => serde_json::json!({
            "event": "message_appended",
            "index": index,
            "message_role": message.role,
            "message_items": message.data.len(),
        }),
        SessionEvent::TurnStarted { turn_id, plan } => {
            serde_json::json!({
                "event": "turn_started",
                "turn_id": turn_id,
                "plan_items": plan.as_ref().map(|plan| plan.plan.len()).unwrap_or(0),
            })
        }
        SessionEvent::Progress { message, plan } => {
            serde_json::json!({
                "event": "progress",
                "message": message,
                "plan_items": plan.as_ref().map(|plan| plan.plan.len()).unwrap_or(0),
            })
        }
        SessionEvent::PlanUpdated { plan } => {
            serde_json::json!({
                "event": "plan_updated",
                "plan_items": plan.as_ref().map(|plan| plan.plan.len()).unwrap_or(0),
            })
        }
        SessionEvent::StreamAssistantMessageDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            delta,
            message_index,
        } => serde_json::json!({
            "event": "stream_assistant_message_delta",
            "message_id": message_id,
            "turn_id": turn_id,
            "in_message_index": in_message_index,
            "item_id": item_id,
            "message_index": message_index,
            "chars": delta.chars().count(),
        }),
        SessionEvent::StreamToolCallDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            call_id,
            tool_name,
            delta,
        } => serde_json::json!({
            "event": "stream_tool_call_delta",
            "message_id": message_id,
            "turn_id": turn_id,
            "in_message_index": in_message_index,
            "item_id": item_id,
            "call_id": call_id,
            "tool_name": tool_name,
            "chars": delta.chars().count(),
        }),
        SessionEvent::StreamReasoningSummaryDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
            delta,
        } => serde_json::json!({
            "event": "stream_reasoning_summary_delta",
            "message_id": message_id,
            "turn_id": turn_id,
            "in_message_index": in_message_index,
            "item_id": item_id,
            "summary_index": summary_index,
            "chars": delta.chars().count(),
        }),
        SessionEvent::StreamReasoningSummaryPartAdded {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
        } => serde_json::json!({
            "event": "stream_reasoning_summary_part_added",
            "message_id": message_id,
            "turn_id": turn_id,
            "in_message_index": in_message_index,
            "item_id": item_id,
            "summary_index": summary_index,
        }),
        SessionEvent::StreamError {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            message_index,
            error,
            error_detail,
        } => serde_json::json!({
            "event": "stream_error",
            "message_id": message_id,
            "turn_id": turn_id,
            "in_message_index": in_message_index,
            "item_id": item_id,
            "message_index": message_index,
            "error": error,
            "error_detail": error_detail,
        }),
        SessionEvent::StreamToolResultDone {
            turn_id,
            batch_id,
            tool_result,
        } => serde_json::json!({
            "event": "stream_tool_result_done",
            "turn_id": turn_id,
            "batch_id": batch_id,
            "tool_name": tool_result.tool_name,
            "tool_call_id": tool_result.tool_call_id,
        }),
        SessionEvent::TurnCompleted { message } => serde_json::json!({
            "event": "turn_completed",
            "message_items": message.data.len(),
        }),
        SessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        } => {
            serde_json::json!({
                "event": "turn_failed",
                "error": error,
                "error_detail": error_detail,
                "can_continue": can_continue,
            })
        }
        SessionEvent::HostCoordinationRequested { request } => serde_json::json!({
            "event": "host_coordination_requested",
            "request_id": request.request_id,
            "tool_call_id": request.tool_call_id,
            "tool_name": request.tool_name,
            "action": request.action,
        }),
        SessionEvent::InteractiveOutputRequested { payload } => serde_json::json!({
            "event": "interactive_output_requested",
            "payload": payload,
        }),
        SessionEvent::SessionViewResult { query_id, payload } => serde_json::json!({
            "event": "session_view_result",
            "query_id": query_id,
            "payload": payload,
        }),
        SessionEvent::MessageHistoryResult { history } => serde_json::json!({
            "event": "message_history_result",
            "request_id": history.request_id,
            "offset": history.offset,
            "limit": history.limit,
            "total": history.total,
            "messages": history.messages.len(),
        }),
        SessionEvent::MessageDetailResult { request_id, record } => serde_json::json!({
            "event": "message_detail_result",
            "request_id": request_id,
            "found": record.is_some(),
        }),
        SessionEvent::CompactCompleted {
            compressed,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens,
            ..
        } => serde_json::json!({
            "event": "compact_completed",
            "compressed": compressed,
            "estimated_tokens_before": estimated_tokens_before,
            "estimated_tokens_after": estimated_tokens_after,
            "threshold_tokens": threshold_tokens,
        }),
        SessionEvent::CompactFailed { phase, reason } => serde_json::json!({
            "event": "compact_failed",
            "phase": phase,
            "reason": reason,
        }),
        SessionEvent::ControlRejected { reason, payload } => serde_json::json!({
            "event": "control_rejected",
            "reason": reason,
            "payload": payload,
        }),
        SessionEvent::RuntimeCrashed {
            error,
            error_detail,
        } => {
            serde_json::json!({"event": "runtime_crashed", "error": error, "error_detail": error_detail})
        }
    }
}

fn render_task_plan_context(plan: &TaskPlanView) -> Option<String> {
    if plan.explanation.is_none() && plan.plan.is_empty() {
        return None;
    }
    let mut text = String::from(SESSION_PLAN_CONTEXT_MARKER);
    if let Some(explanation) = plan.explanation.as_deref() {
        text.push_str("\n\n");
        text.push_str(explanation);
    }
    if !plan.plan.is_empty() {
        text.push_str("\n\n");
        for item in &plan.plan {
            text.push_str("- [");
            text.push_str(match item.status {
                TaskPlanItemStatus::Pending => "pending",
                TaskPlanItemStatus::InProgress => "in_progress",
                TaskPlanItemStatus::Completed => "completed",
            });
            text.push_str("] ");
            text.push_str(item.step.trim());
            text.push('\n');
        }
    }
    Some(text.trim_end().to_string())
}

fn is_task_plan_context_message(message: &ChatMessage) -> bool {
    message.role == ChatRole::Assistant
        && message.data.iter().any(|item| {
            matches!(
                item,
                ChatMessageItem::Context(context)
                    if context.text.starts_with(SESSION_PLAN_CONTEXT_MARKER)
            )
        })
}

fn restored_history_needs_continuation(history: &[ChatMessage]) -> bool {
    let Some(last) = history.last() else {
        return false;
    };
    if matches!(last.role, ChatRole::User) {
        return true;
    }
    last.data.iter().any(|item| {
        matches!(
            item,
            ChatMessageItem::ToolCall(_) | ChatMessageItem::ToolResult(_)
        )
    })
}

fn pending_continuation_kind(continuation: &PendingContinuation) -> &'static str {
    match continuation {
        PendingContinuation::CurrentHistory => "current_history",
        PendingContinuation::DataRequests(_) => "data_requests",
    }
}

fn usize_field(payload: &serde_json::Value, key: &str, default: usize) -> usize {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn invalid_source_payload(source: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "source": source,
        "error": "source must be current or all",
    })
}

#[cfg(test)]
#[path = "actor_test.rs"]
mod actor_test;
