use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use reqwest::blocking::{multipart, Client};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatMessagePart, ChatRole, FileItem, FileState,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    conversation_host::ConversationHostRuntime,
    conversation_id_manager::ConversationIdManager,
    conversation_metadata::WorkdirLayout,
    logger::StellaclawLogger,
    service_protos::{
        channel::{ChannelEvent as KernelChannelEvent, ChannelIngress},
        workspace::{WorkspaceFileEncoding, WorkspaceRequest, WorkspaceResponse, WorkspaceTarget},
    },
};

use super::{
    types::{
        parse_conversation_control, ConversationControl, IncomingConversationMessage,
        IncomingDispatch, IncomingMessageDispatch, OutgoingAttachmentKind, OutgoingError,
        OutgoingMessageAppended, OutgoingOption, OutgoingOptions, OutgoingSessionStream,
        ProcessingState,
    },
    Channel,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum AuthorizationState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatAuthorization {
    state: AuthorizationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_user: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecurityState {
    #[serde(default)]
    admin_user_ids: Vec<i64>,
    #[serde(default)]
    chats: BTreeMap<String, ChatAuthorization>,
}

pub struct TelegramChannel {
    id: String,
    bot_token: String,
    api_base_url: String,
    poll_timeout_seconds: u64,
    poll_interval_ms: u64,
    client: Client,
    workdir: PathBuf,
    conversation_runtime: Option<Arc<ConversationHostRuntime>>,
    progress_panels: Mutex<BTreeMap<String, TelegramProgressPanel>>,
    model_aliases: Vec<String>,
    logger: Option<Arc<StellaclawLogger>>,
    security_path: PathBuf,
    security: Mutex<SecurityState>,
}

impl TelegramChannel {
    const MAX_MESSAGE_CHARS: usize = 4096;
    const MAX_OUTGOING_FILE_BYTES: usize = 48 * 1024 * 1024;
    const MIN_PROGRESS_EDIT_INTERVAL: Duration = Duration::from_millis(900);
    const DELIVERY_RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
    const DELIVERY_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);

    pub fn new(
        id: String,
        bot_token: String,
        api_base_url: String,
        poll_timeout_seconds: u64,
        poll_interval_ms: u64,
        admin_user_ids: Vec<i64>,
        workdir: &Path,
        conversation_runtime: Arc<ConversationHostRuntime>,
        model_aliases: Vec<String>,
        logger: Arc<StellaclawLogger>,
    ) -> Result<Self> {
        let dir = workdir.join(".stellaclaw").join("channels").join(&id);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let security_path = dir.join("security.json");
        let security = if security_path.exists() {
            let raw = fs::read_to_string(&security_path)
                .with_context(|| format!("failed to read {}", security_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", security_path.display()))?
        } else {
            SecurityState::default()
        };
        let mut security = security;
        for admin_user_id in &admin_user_ids {
            if !security.admin_user_ids.contains(admin_user_id) {
                security.admin_user_ids.push(*admin_user_id);
            }
        }
        security.admin_user_ids.sort_unstable();
        security.admin_user_ids.dedup();

        let request_timeout = Duration::from_secs(poll_timeout_seconds.saturating_add(15).max(30));
        let client = Client::builder()
            .timeout(request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build telegram HTTP client")?;

        let instance = Self {
            id,
            bot_token,
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
            poll_timeout_seconds,
            poll_interval_ms,
            client,
            workdir: workdir.to_path_buf(),
            conversation_runtime: Some(conversation_runtime),
            progress_panels: Mutex::new(BTreeMap::new()),
            model_aliases,
            logger: Some(logger),
            security_path,
            security: Mutex::new(security),
        };
        instance.save_security_state()?;
        Ok(instance)
    }

    fn run_loop(
        &self,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) -> Result<()> {
        let mut offset = 0_i64;
        loop {
            let payload = json!({
                "offset": offset,
                "timeout": self.poll_timeout_seconds,
                "allowed_updates": ["message", "callback_query"],
            });
            let updates: Vec<TelegramUpdate> = match self.call_api("getUpdates", &payload) {
                Ok(updates) => updates,
                Err(error) => {
                    logger.warn(
                        "telegram_poll_failed",
                        json!({"channel_id": self.id, "error": format!("{error:#}")}),
                    );
                    thread::sleep(Duration::from_millis(self.poll_interval_ms.max(1000)));
                    continue;
                }
            };
            for update in updates {
                offset = update.update_id.saturating_add(1);
                if let Some(message) = update.message {
                    if let Err(error) =
                        self.handle_message(message, &dispatch_tx, &id_manager, &logger)
                    {
                        logger.warn(
                            "telegram_update_failed",
                            json!({"channel_id": self.id, "update_id": update.update_id, "error": format!("{error:#}")}),
                        );
                    }
                } else if let Some(callback_query) = update.callback_query {
                    if let Err(error) = self.answer_callback_query(&callback_query.id) {
                        logger.warn(
                            "telegram_callback_query_ack_failed",
                            json!({
                                "channel_id": self.id,
                                "callback_query_id": callback_query.id,
                                "error": format!("{error:#}"),
                            }),
                        );
                    }
                    if let Err(error) = self.handle_callback_query(
                        callback_query,
                        &dispatch_tx,
                        &id_manager,
                        &logger,
                    ) {
                        logger.warn(
                            "telegram_update_failed",
                            json!({"channel_id": self.id, "update_id": update.update_id, "error": format!("{error:#}")}),
                        );
                    }
                }
            }
            thread::sleep(Duration::from_millis(self.poll_interval_ms));
        }
    }

    fn handle_message(
        &self,
        message: TelegramMessage,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
        logger: &Arc<StellaclawLogger>,
    ) -> Result<()> {
        let chat_id = message.chat.id.to_string();
        let from_user_id = message
            .from
            .as_ref()
            .map(|user| user.id)
            .unwrap_or_default();
        let text = message
            .text
            .clone()
            .or_else(|| message.caption.clone())
            .unwrap_or_default();
        self.bootstrap_first_private_admin(&message, from_user_id)?;

        if self.is_admin_private_chat(&message, from_user_id) && self.handle_admin_command(&text)? {
            return Ok(());
        }

        if !self.authorize_chat(&message, from_user_id, &text)? {
            return Ok(());
        }

        if matches!(
            parse_conversation_control(&text),
            Some(ConversationControl::ShowModel)
        ) {
            self.send_model_selection_panel(&chat_id)?;
            return Ok(());
        }

        let conversation_id = id_manager
            .lock()
            .map_err(|_| anyhow!("conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &chat_id)
            .map_err(anyhow::Error::msg)?;

        let control = parse_conversation_control(&text);
        let files = self.collect_incoming_files(&conversation_id, &message, logger)?;
        if control.is_none() && text.trim().is_empty() && files.is_empty() {
            return Ok(());
        }
        let incoming = IncomingDispatch::Message(IncomingMessageDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: chat_id,
            conversation_id,
            message: IncomingConversationMessage {
                remote_message_id: message.message_id.to_string(),
                user_name: message.from.as_ref().map(render_user),
                message_time: message.date.and_then(render_message_time),
                text: if control.is_some() || text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                selection_references: Vec::new(),
                files,
                control,
            },
        });
        dispatch_tx
            .send(incoming)
            .map_err(|_| anyhow!("dispatcher channel closed"))
    }

    fn handle_callback_query(
        &self,
        callback_query: TelegramCallbackQuery,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
        logger: &Arc<StellaclawLogger>,
    ) -> Result<()> {
        let Some(message) = callback_query.message else {
            return Ok(());
        };
        let text = callback_query
            .data
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_default();
        let chat_id = message.chat.id.to_string();
        let from_user_id = callback_query.from.id;
        self.bootstrap_first_private_admin(&message, from_user_id)?;

        if self.is_admin_private_chat(&message, from_user_id) && self.handle_admin_command(&text)? {
            return Ok(());
        }

        if !self.authorize_chat(&message, from_user_id, &text)? {
            return Ok(());
        }

        if matches!(
            parse_conversation_control(&text),
            Some(ConversationControl::ShowModel)
        ) {
            self.send_model_selection_panel(&chat_id)?;
            return Ok(());
        }

        let conversation_id = id_manager
            .lock()
            .map_err(|_| anyhow!("conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &chat_id)
            .map_err(anyhow::Error::msg)?;

        let control = parse_conversation_control(&text);
        let files = self.collect_incoming_files(&conversation_id, &message, logger)?;
        if control.is_none() && text.trim().is_empty() && files.is_empty() {
            return Ok(());
        }
        let incoming = IncomingDispatch::Message(IncomingMessageDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: chat_id,
            conversation_id,
            message: IncomingConversationMessage {
                remote_message_id: format!("callback:{}", callback_query.id),
                user_name: Some(render_user(&callback_query.from)),
                message_time: message.date.and_then(render_message_time),
                text: if control.is_some() || text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                selection_references: Vec::new(),
                files,
                control,
            },
        });
        dispatch_tx
            .send(incoming)
            .map_err(|_| anyhow!("dispatcher channel closed"))
    }

    fn handle_admin_command(&self, text: &str) -> Result<bool> {
        let command = text.trim();
        if command == "/admin_chat_list" {
            let security = self
                .security
                .lock()
                .map_err(|_| anyhow!("telegram security lock poisoned"))?;
            let mut lines = vec![format!("channel `{}` 当前 chat 审批状态:", self.id)];
            for (chat_id, state) in &security.chats {
                lines.push(format!(
                    "- {}: {:?} {} {}",
                    chat_id,
                    state.state,
                    state.last_title.as_deref().unwrap_or(""),
                    state.last_user.as_deref().unwrap_or("")
                ));
            }
            drop(security);
            self.send_text_to_admins(&lines.join("\n"))?;
            return Ok(true);
        }
        if let Some(chat_id) = command.strip_prefix("/admin_chat_approve ").map(str::trim) {
            self.update_chat_state(chat_id, AuthorizationState::Approved)?;
            self.send_text(chat_id, "此 chat 已批准，可以开始使用。", None)?;
            self.send_text_to_admins(&format!("已批准 chat {chat_id}"))?;
            return Ok(true);
        }
        if let Some(chat_id) = command.strip_prefix("/admin_chat_reject ").map(str::trim) {
            self.update_chat_state(chat_id, AuthorizationState::Rejected)?;
            self.send_text(chat_id, "此 chat 已被拒绝。", None)?;
            self.send_text_to_admins(&format!("已拒绝 chat {chat_id}"))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn authorize_chat(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
        text: &str,
    ) -> Result<bool> {
        if self.is_admin_private_chat(message, from_user_id) {
            return Ok(true);
        }
        let chat_id = message.chat.id.to_string();
        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let entry = security
            .chats
            .entry(chat_id.clone())
            .or_insert(ChatAuthorization {
                state: AuthorizationState::Pending,
                last_title: message.chat.title.clone(),
                last_user: message.from.as_ref().map(render_user),
            });
        entry.last_title = message.chat.title.clone();
        entry.last_user = message.from.as_ref().map(render_user);
        let state = entry.state.clone();
        drop(security);
        self.save_security_state()?;
        match state {
            AuthorizationState::Approved => Ok(true),
            AuthorizationState::Rejected => {
                self.send_text(&chat_id, "当前 chat 未获批准，无法使用。", None)?;
                Ok(false)
            }
            AuthorizationState::Pending => {
                self.send_text(
                    &chat_id,
                    "当前 chat 正在等待管理员批准，请联系管理员处理。",
                    None,
                )?;
                self.send_text_to_admins(&format!(
                    "待审批 chat: {}\n标题: {}\n用户: {}\n最近消息: {}",
                    chat_id,
                    message.chat.title.as_deref().unwrap_or(""),
                    message.from.as_ref().map(render_user).unwrap_or_default(),
                    text
                ))?;
                Ok(false)
            }
        }
    }

    fn collect_incoming_files(
        &self,
        conversation_id: &str,
        message: &TelegramMessage,
        logger: &StellaclawLogger,
    ) -> Result<Vec<FileItem>> {
        let attachments = self.collect_attachment_descriptors(message);
        if attachments.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self
            .workdir
            .join("conversations")
            .join(conversation_id)
            .join(".stellaclaw")
            .join("attachments")
            .join("incoming");
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

        let mut files = Vec::with_capacity(attachments.len());
        for attachment in attachments {
            let target = dir.join(attachment.file_name());
            match self.download_attachment(&attachment, &target) {
                Ok(file) => files.push(file),
                Err(error) => {
                    logger.warn(
                        "telegram_attachment_download_failed",
                        json!({
                            "channel_id": self.id,
                            "conversation_id": conversation_id,
                            "file_id": attachment.file_id,
                            "target": target.display().to_string(),
                            "error": format!("{error:#}"),
                        }),
                    );
                    files.push(FileItem {
                        uri: format!("file://{}", target.display()),
                        name: target
                            .file_name()
                            .and_then(|value| value.to_str())
                            .map(ToOwned::to_owned),
                        media_type: attachment.media_type.clone(),
                        width: None,
                        height: None,
                        state: Some(FileState::Crashed {
                            reason: format!("{error:#}"),
                        }),
                    });
                }
            }
        }
        Ok(files)
    }

    fn collect_attachment_descriptors(&self, message: &TelegramMessage) -> Vec<TelegramAttachment> {
        let mut attachments = Vec::new();
        if let Some(photo) = message.photo.as_ref().and_then(|items| items.last()) {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Image,
                file_id: photo.file_id.clone(),
                file_unique_id: photo.file_unique_id.clone(),
                file_name: None,
                media_type: Some("image/jpeg".to_string()),
            });
        }
        if let Some(document) = &message.document {
            attachments.push(TelegramAttachment {
                kind: classify_document_kind(document),
                file_id: document.file_id.clone(),
                file_unique_id: document.file_unique_id.clone(),
                file_name: document.file_name.clone(),
                media_type: document.mime_type.clone(),
            });
        }
        if let Some(audio) = &message.audio {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Audio,
                file_id: audio.file_id.clone(),
                file_unique_id: audio.file_unique_id.clone(),
                file_name: audio.file_name.clone(),
                media_type: audio.mime_type.clone(),
            });
        }
        if let Some(voice) = &message.voice {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Voice,
                file_id: voice.file_id.clone(),
                file_unique_id: voice.file_unique_id.clone(),
                file_name: voice.file_name.clone(),
                media_type: voice.mime_type.clone(),
            });
        }
        if let Some(video) = &message.video {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Video,
                file_id: video.file_id.clone(),
                file_unique_id: video.file_unique_id.clone(),
                file_name: video.file_name.clone(),
                media_type: video.mime_type.clone(),
            });
        }
        if let Some(animation) = &message.animation {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Animation,
                file_id: animation.file_id.clone(),
                file_unique_id: animation.file_unique_id.clone(),
                file_name: animation.file_name.clone(),
                media_type: animation.mime_type.clone(),
            });
        }
        attachments
    }

    fn download_attachment(
        &self,
        attachment: &TelegramAttachment,
        target: &Path,
    ) -> Result<FileItem> {
        let metadata: TelegramFile = self.call_api(
            "getFile",
            &json!({
                "file_id": attachment.file_id,
            }),
        )?;
        let file_path = metadata
            .file_path
            .context("telegram getFile returned no file_path")?;
        let url = format!(
            "{}/file/bot{}/{}",
            self.api_base_url, self.bot_token, file_path
        );
        let bytes = self
            .client
            .get(url)
            .send()
            .context("telegram attachment download request failed")?
            .bytes()
            .context("failed to read telegram attachment bytes")?;
        fs::write(target, &bytes)
            .with_context(|| format!("failed to write attachment {}", target.display()))?;
        Ok(FileItem {
            uri: format!("file://{}", target.display()),
            name: target
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned),
            media_type: attachment
                .media_type
                .clone()
                .or_else(|| infer_media_type(target)),
            width: None,
            height: None,
            state: None,
        })
    }

    fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        let _: serde_json::Value = self.call_api(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback_query_id,
            }),
        )?;
        Ok(())
    }

    fn send_text(
        &self,
        platform_chat_id: &str,
        text: &str,
        options: Option<&OutgoingOptions>,
    ) -> Result<()> {
        let chat_id = platform_chat_id.to_string();
        let chunks = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS);
        let last_index = chunks.len().saturating_sub(1);
        for (index, rendered) in chunks.into_iter().enumerate() {
            let payload = build_send_text_payload(
                &chat_id,
                rendered,
                (index == last_index).then_some(options).flatten(),
            )?;
            let _: serde_json::Value = self.call_api_delivery("sendMessage", &payload)?;
        }
        Ok(())
    }

    fn send_progress_panel(&self, platform_chat_id: &str, text: &str) -> Result<i64> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        let payload = build_send_text_payload(platform_chat_id, rendered, None)?;
        let message: TelegramSentMessage = self.call_api_delivery("sendMessage", &payload)?;
        Ok(message.message_id)
    }

    fn edit_progress_panel(
        &self,
        platform_chat_id: &str,
        message_id: i64,
        text: &str,
    ) -> Result<()> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        let mut payload = json!({
            "chat_id": platform_chat_id,
            "message_id": message_id,
            "text": rendered.text,
            "disable_web_page_preview": true,
        });
        if !rendered.entities.is_empty() {
            if let Some(object) = payload.as_object_mut() {
                object.insert(
                    "entities".to_string(),
                    serde_json::to_value(rendered.entities)
                        .context("failed to encode telegram entities")?,
                );
            }
        }
        let _: Value = self.call_api_delivery("editMessageText", &payload)?;
        Ok(())
    }

    fn handle_progress_stream(&self, stream: &OutgoingSessionStream) -> Result<()> {
        let event_type = stream.event.get("type").and_then(Value::as_str);
        match event_type {
            Some("turn_started") => self.start_progress_panel(stream),
            Some("plan_updated") => self.update_progress_panel(
                stream,
                TelegramProgressUpdate {
                    status: TelegramProgressStatus::Running,
                    activity: Some("计划已更新".to_string()),
                    plan: stream.event.get("plan").cloned(),
                    force_edit: true,
                    terminal: false,
                },
            ),
            Some("stream_assistant_message_delta") => self.update_progress_panel(
                stream,
                TelegramProgressUpdate {
                    status: TelegramProgressStatus::Running,
                    activity: Some("正在整理回复".to_string()),
                    plan: None,
                    force_edit: false,
                    terminal: false,
                },
            ),
            Some("stream_tool_call_delta") => self.update_progress_panel(
                stream,
                TelegramProgressUpdate {
                    status: TelegramProgressStatus::Running,
                    activity: Some(render_tool_activity(&stream.event, "正在准备工具")),
                    plan: None,
                    force_edit: false,
                    terminal: false,
                },
            ),
            Some("stream_tool_result_done") => self.update_progress_panel(
                stream,
                TelegramProgressUpdate {
                    status: TelegramProgressStatus::Running,
                    activity: Some(render_tool_result_activity(&stream.event)),
                    plan: None,
                    force_edit: true,
                    terminal: false,
                },
            ),
            Some("turn_completed") => self.update_progress_panel(
                stream,
                TelegramProgressUpdate {
                    status: TelegramProgressStatus::Completed,
                    activity: Some("结果已生成".to_string()),
                    plan: None,
                    force_edit: true,
                    terminal: true,
                },
            ),
            Some("stream_error") => {
                let terminal =
                    stream.event.get("scope").and_then(Value::as_str) == Some("turn_failed");
                self.update_progress_panel(
                    stream,
                    TelegramProgressUpdate {
                        status: if terminal {
                            TelegramProgressStatus::Failed
                        } else {
                            TelegramProgressStatus::Running
                        },
                        activity: stream
                            .event
                            .get("error")
                            .and_then(Value::as_str)
                            .map(|error| format!("遇到错误: {error}")),
                        plan: None,
                        force_edit: true,
                        terminal,
                    },
                )
            }
            _ => Ok(()),
        }
    }

    fn start_progress_panel(&self, stream: &OutgoingSessionStream) -> Result<()> {
        let Some(turn_id) = stream.event.get("turn_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let key = progress_panel_key(stream, turn_id);
        let mut panel = TelegramProgressPanel {
            message_id: 0,
            started_at: Instant::now(),
            last_edit_at: Instant::now(),
            status: TelegramProgressStatus::Running,
            activity: "开始处理当前 round".to_string(),
            plan: stream.event.get("plan").cloned(),
            last_rendered: String::new(),
        };
        let text = render_progress_panel(&panel);
        let message_id = self.send_progress_panel(&stream.platform_chat_id, &text)?;
        panel.message_id = message_id;
        panel.last_rendered = text;
        self.progress_panels
            .lock()
            .map_err(|_| anyhow!("telegram progress panel lock poisoned"))?
            .insert(key, panel);
        Ok(())
    }

    fn update_progress_panel(
        &self,
        stream: &OutgoingSessionStream,
        update: TelegramProgressUpdate,
    ) -> Result<()> {
        let mut panels = self
            .progress_panels
            .lock()
            .map_err(|_| anyhow!("telegram progress panel lock poisoned"))?;
        let Some(key) = progress_panel_key_for_event(stream).or_else(|| {
            let prefix = progress_panel_session_prefix(stream);
            panels.keys().find(|key| key.starts_with(&prefix)).cloned()
        }) else {
            return Ok(());
        };
        let Some(panel) = panels.get_mut(&key) else {
            return Ok(());
        };
        panel.status = update.status;
        if let Some(activity) = update.activity {
            if !activity.trim().is_empty() {
                panel.activity = activity;
            }
        }
        if update.plan.is_some() {
            panel.plan = update.plan;
        }
        let now = Instant::now();
        if !update.force_edit
            && now.duration_since(panel.last_edit_at) < Self::MIN_PROGRESS_EDIT_INTERVAL
        {
            return Ok(());
        }
        let text = render_progress_panel(panel);
        if text == panel.last_rendered {
            return Ok(());
        }
        let message_id = panel.message_id;
        panel.last_rendered = text.clone();
        panel.last_edit_at = now;
        if update.terminal {
            panels.remove(&key);
        }
        drop(panels);
        self.edit_progress_panel(&stream.platform_chat_id, message_id, &text)
    }

    fn send_attachment(
        &self,
        platform_chat_id: &str,
        attachment: TelegramOutgoingAttachment,
    ) -> Result<()> {
        let field = if attachment
            .media_type
            .as_deref()
            .is_some_and(|media_type| media_type.starts_with("image/"))
        {
            "photo"
        } else {
            "document"
        };
        let method = if field == "photo" {
            "sendPhoto"
        } else {
            "sendDocument"
        };
        let _: serde_json::Value = self.call_api_multipart_delivery(
            method,
            platform_chat_id,
            field,
            attachment.name,
            attachment.media_type,
            attachment.bytes,
        )?;
        Ok(())
    }

    fn collect_outgoing_attachments(
        &self,
        appended: &OutgoingMessageAppended,
    ) -> Vec<TelegramOutgoingAttachment> {
        let mut attachments = Vec::new();
        let mut seen = Vec::<String>::new();
        for item in &appended.message.data {
            match item {
                ChatMessageItem::Context(context) => {
                    for target in markdown_link_targets(&context.text) {
                        let key = format!("markdown:{target}");
                        if seen.iter().any(|value| value == &key) {
                            continue;
                        }
                        if let Some(attachment) =
                            self.workspace_attachment_from_markdown_target(appended, &target)
                        {
                            seen.push(key);
                            attachments.push(attachment);
                        }
                    }
                }
                ChatMessageItem::File(file) => {
                    let key = format!("file:{}", file.uri);
                    if seen.iter().any(|value| value == &key) {
                        continue;
                    }
                    if let Some(attachment) =
                        self.workspace_attachment_from_file_item(appended, file)
                    {
                        seen.push(key);
                        attachments.push(attachment);
                    }
                }
                ChatMessageItem::Compaction(_)
                | ChatMessageItem::SelectionReference(_)
                | ChatMessageItem::Reasoning(_)
                | ChatMessageItem::ToolCall(_)
                | ChatMessageItem::ToolResult(_) => {}
            }
        }
        attachments
    }

    fn workspace_attachment_from_markdown_target(
        &self,
        appended: &OutgoingMessageAppended,
        target: &str,
    ) -> Option<TelegramOutgoingAttachment> {
        let target = normalize_markdown_path(target);
        if target.is_empty() || target.starts_with("attachment://") || has_external_scheme(&target)
        {
            return None;
        }
        let target_path = Path::new(&target);
        if target_path.is_absolute() {
            return local_attachment_from_path(target_path, None, Self::MAX_OUTGOING_FILE_BYTES);
        }
        let relative_path = safe_relative_path(&target)?;
        let path = path_to_slash_string(&relative_path)?;
        self.read_workspace_attachment(
            &appended.conversation_id,
            &path,
            WorkspaceTarget::Auto,
            None,
        )
    }

    fn workspace_attachment_from_file_item(
        &self,
        appended: &OutgoingMessageAppended,
        file: &FileItem,
    ) -> Option<TelegramOutgoingAttachment> {
        if file.state.is_some() {
            return None;
        }
        let path = local_path_from_file_item_uri(&file.uri)?;
        if path.is_absolute() {
            return local_attachment_from_path(
                &path,
                file.media_type.clone(),
                Self::MAX_OUTGOING_FILE_BYTES,
            );
        }
        let root = WorkdirLayout::new(&self.workdir).conversation_root(&appended.conversation_id);
        let root = fs::canonicalize(root).ok()?;
        let path = fs::canonicalize(path).ok()?;
        if !path.starts_with(&root) {
            return None;
        }
        let relative_path = path.strip_prefix(&root).ok()?;
        let relative = path_to_slash_string(relative_path)?;
        let target = if is_local_overlay_path(relative_path) {
            WorkspaceTarget::LocalOverlay
        } else {
            WorkspaceTarget::Auto
        };
        self.read_workspace_attachment(
            &appended.conversation_id,
            &relative,
            target,
            file.media_type.clone(),
        )
    }

    fn read_workspace_attachment(
        &self,
        conversation_id: &str,
        path: &str,
        target: WorkspaceTarget,
        media_type: Option<String>,
    ) -> Option<TelegramOutgoingAttachment> {
        let response = self.workspace_response_value(
            conversation_id,
            WorkspaceRequest::ReadFile {
                path: path.to_string(),
                target,
                offset: None,
                limit_bytes: Some(Self::MAX_OUTGOING_FILE_BYTES.saturating_add(1)),
            },
        )?;
        let WorkspaceResponse::File {
            name,
            returned_bytes,
            truncated,
            encoding,
            data,
            ..
        } = response
        else {
            return None;
        };
        if truncated || returned_bytes > Self::MAX_OUTGOING_FILE_BYTES {
            return None;
        }
        let bytes = match encoding {
            WorkspaceFileEncoding::Utf8 => data.into_bytes(),
            WorkspaceFileEncoding::Base64 => general_purpose::STANDARD.decode(data).ok()?,
        };
        Some(TelegramOutgoingAttachment {
            media_type: media_type.or_else(|| infer_media_type(Path::new(&name))),
            name,
            bytes,
        })
    }

    fn workspace_response_value(
        &self,
        conversation_id: &str,
        request: WorkspaceRequest,
    ) -> Option<WorkspaceResponse> {
        let runtime = self.conversation_runtime.as_ref()?;
        runtime.ensure_conversation_started(conversation_id).ok()?;
        let request_id = telegram_request_id();
        let rx = runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::Workspace {
                    request_id: request_id.clone(),
                    request,
                },
            )
            .ok()?;
        wait_workspace_response(&rx, Duration::from_secs(30), &request_id).ok()
    }

    fn save_security_state(&self) -> Result<()> {
        let security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let raw = serde_json::to_string_pretty(&*security)
            .context("failed to serialize telegram security state")?;
        fs::write(&self.security_path, raw)
            .with_context(|| format!("failed to write {}", self.security_path.display()))
    }

    fn send_text_to_admins(&self, text: &str) -> Result<()> {
        for admin in self.effective_admin_user_ids()? {
            let _ = self.send_text(&admin.to_string(), text, None);
        }
        Ok(())
    }

    fn send_model_selection_panel(&self, platform_chat_id: &str) -> Result<()> {
        if self.model_aliases.is_empty() {
            return self.send_text(platform_chat_id, "当前没有可用的 agent model。", None);
        }
        let text = format!(
            "**选择模型**\n当前可用 agent model 共 {} 个。请选择这个 conversation 后续使用的模型：",
            self.model_aliases.len()
        );
        let options = model_selection_options(&self.model_aliases);
        self.send_text(platform_chat_id, &text, Some(&options))
    }

    fn update_chat_state(&self, chat_id: &str, new_state: AuthorizationState) -> Result<()> {
        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let entry = security
            .chats
            .entry(chat_id.to_string())
            .or_insert(ChatAuthorization {
                state: AuthorizationState::Pending,
                last_title: None,
                last_user: None,
            });
        entry.state = new_state;
        drop(security);
        self.save_security_state()
    }

    fn is_admin_private_chat(&self, message: &TelegramMessage, from_user_id: i64) -> bool {
        message.chat.chat_type == "private"
            && self
                .effective_admin_user_ids()
                .map(|admins| admins.contains(&from_user_id))
                .unwrap_or(false)
            && message.chat.id == from_user_id
    }

    fn effective_admin_user_ids(&self) -> Result<Vec<i64>> {
        let security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let mut admin_user_ids = security.admin_user_ids.clone();
        admin_user_ids.sort_unstable();
        admin_user_ids.dedup();
        Ok(admin_user_ids)
    }

    fn bootstrap_first_private_admin(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
    ) -> Result<()> {
        if self.bootstrap_first_private_admin_in_memory(message, from_user_id)? {
            self.save_security_state()?;
            self.send_text(
                &from_user_id.to_string(),
                "已将你注册为此 Telegram channel 的管理员。",
                None,
            )?;
        }
        Ok(())
    }

    fn bootstrap_first_private_admin_in_memory(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
    ) -> Result<bool> {
        if from_user_id == 0
            || message.chat.chat_type != "private"
            || message.chat.id != from_user_id
        {
            return Ok(false);
        }

        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        if !security.admin_user_ids.is_empty() {
            return Ok(false);
        }
        security.admin_user_ids.push(from_user_id);
        security.chats.insert(
            message.chat.id.to_string(),
            ChatAuthorization {
                state: AuthorizationState::Approved,
                last_title: message.chat.title.clone(),
                last_user: message.from.as_ref().map(render_user),
            },
        );
        Ok(true)
    }

    fn call_api<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        payload: &serde_json::Value,
    ) -> Result<T> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(payload)
            .send()
            .with_context(|| format!("telegram API call {method} failed"))?;
        let envelope = response
            .json::<TelegramEnvelope<T>>()
            .with_context(|| format!("telegram API {method} returned invalid JSON"))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                envelope
                    .description
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {} returned no result", method))
    }

    fn call_api_delivery<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        payload: &serde_json::Value,
    ) -> Result<T> {
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            match self.call_api_delivery_once(method, payload) {
                Ok(result) => return Ok(result),
                Err(error) if error.retryable => {
                    let delay = delivery_retry_delay(attempt, error.retry_after);
                    self.log_delivery_retry(method, attempt, delay, &error);
                    thread::sleep(delay);
                }
                Err(error) => return Err(anyhow!("{}", error.describe(method))),
            }
        }
    }

    fn call_api_delivery_once<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        payload: &serde_json::Value,
    ) -> std::result::Result<T, TelegramDeliveryError> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(payload)
            .send()
            .map_err(|error| TelegramDeliveryError::transport(error.to_string()))?;
        decode_telegram_delivery_response(method, response)
    }

    fn call_api_multipart_delivery<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        platform_chat_id: &str,
        field: &str,
        name: String,
        media_type: Option<String>,
        bytes: Vec<u8>,
    ) -> Result<T> {
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            let result = self.call_api_multipart_delivery_once(
                method,
                platform_chat_id,
                field,
                &name,
                media_type.as_deref(),
                bytes.clone(),
            );
            match result {
                Ok(result) => return Ok(result),
                Err(error) if error.retryable => {
                    let delay = delivery_retry_delay(attempt, error.retry_after);
                    self.log_delivery_retry(method, attempt, delay, &error);
                    thread::sleep(delay);
                }
                Err(error) => return Err(anyhow!("{}", error.describe(method))),
            }
        }
    }

    fn call_api_multipart_delivery_once<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        platform_chat_id: &str,
        field: &str,
        name: &str,
        media_type: Option<&str>,
        bytes: Vec<u8>,
    ) -> std::result::Result<T, TelegramDeliveryError> {
        let mut part = multipart::Part::bytes(bytes).file_name(name.to_string());
        if let Some(media_type) = media_type {
            part = part
                .mime_str(media_type)
                .map_err(|error| TelegramDeliveryError::non_retryable(error.to_string()))?;
        }
        let form = multipart::Form::new()
            .text("chat_id", platform_chat_id.to_string())
            .part(field.to_string(), part);
        let response = self
            .client
            .post(self.method_url(method))
            .multipart(form)
            .send()
            .map_err(|error| TelegramDeliveryError::transport(error.to_string()))?;
        decode_telegram_delivery_response(method, response)
    }

    fn log_delivery_retry(
        &self,
        method: &str,
        attempt: u32,
        delay: Duration,
        error: &TelegramDeliveryError,
    ) {
        if let Some(logger) = &self.logger {
            logger.warn(
                "telegram_delivery_retry",
                json!({
                    "channel_id": self.id,
                    "method": method,
                    "attempt": attempt,
                    "delay_ms": delay.as_millis(),
                    "http_status": error.http_status,
                    "error_code": error.error_code,
                    "error": error.description,
                }),
            );
        }
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.bot_token, method)
    }
}

impl Channel for TelegramChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        if state == ProcessingState::Typing {
            let _: serde_json::Value = self.call_api(
                "sendChatAction",
                &json!({
                    "chat_id": platform_chat_id,
                    "action": "typing",
                }),
            )?;
        }
        Ok(())
    }

    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        let mut text = error.message.clone();
        if let Some(action) = error
            .suggested_action
            .as_deref()
            .filter(|action| !action.trim().is_empty())
        {
            text.push('\n');
            text.push_str(action);
        }
        self.send_text(&error.platform_chat_id, &text, None)
    }

    fn message_appended(&self, appended: &OutgoingMessageAppended) -> Result<()> {
        if !is_visible_telegram_assistant_message(appended) {
            return Ok(());
        }
        let text = render_chat_message(&appended.message);
        let attachments = self.collect_outgoing_attachments(appended);
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(());
        }
        if !text.trim().is_empty() {
            self.send_text(&appended.platform_chat_id, &text, None)?;
        }
        for attachment in attachments {
            self.send_attachment(&appended.platform_chat_id, attachment)?;
        }
        Ok(())
    }

    fn session_stream(&self, stream: &OutgoingSessionStream) -> Result<()> {
        self.handle_progress_stream(stream)
    }

    fn spawn_ingress(
        self: Arc<Self>,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) where
        Self: Sized,
    {
        thread::spawn(move || {
            if let Err(error) = self.run_loop(dispatch_tx, id_manager, logger.clone()) {
                logger.error(
                    "telegram_channel_failed",
                    json!({"channel_id": self.id, "error": format!("{error:#}")}),
                );
            }
        });
    }
}

#[derive(Debug, Clone)]
struct TelegramAttachment {
    kind: OutgoingAttachmentKind,
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
    media_type: Option<String>,
}

struct TelegramOutgoingAttachment {
    name: String,
    media_type: Option<String>,
    bytes: Vec<u8>,
}

struct TelegramProgressPanel {
    message_id: i64,
    started_at: Instant,
    last_edit_at: Instant,
    status: TelegramProgressStatus,
    activity: String,
    plan: Option<Value>,
    last_rendered: String,
}

struct TelegramProgressUpdate {
    status: TelegramProgressStatus,
    activity: Option<String>,
    plan: Option<Value>,
    force_edit: bool,
    terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramProgressStatus {
    Running,
    Completed,
    Failed,
}

impl TelegramAttachment {
    fn file_name(&self) -> String {
        if let Some(name) = self
            .file_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            return sanitize_file_name(name);
        }
        let ext = infer_extension(self.media_type.as_deref(), self.kind);
        format!(
            "telegram-{}-{}.{}",
            kind_label(self.kind),
            self.file_unique_id,
            ext
        )
    }
}

#[derive(Debug, Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

#[derive(Debug)]
struct TelegramDeliveryError {
    description: String,
    http_status: Option<u16>,
    error_code: Option<i64>,
    retry_after: Option<u64>,
    retryable: bool,
}

impl TelegramDeliveryError {
    fn transport(description: String) -> Self {
        Self {
            description,
            http_status: None,
            error_code: None,
            retry_after: None,
            retryable: true,
        }
    }

    fn non_retryable(description: String) -> Self {
        Self {
            description,
            http_status: None,
            error_code: None,
            retry_after: None,
            retryable: false,
        }
    }

    fn describe(&self, method: &str) -> String {
        let mut text = format!("telegram API {method} failed: {}", self.description);
        if let Some(status) = self.http_status {
            text.push_str(&format!(" (http_status={status})"));
        }
        if let Some(code) = self.error_code {
            text.push_str(&format!(" (error_code={code})"));
        }
        text
    }
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    date: Option<i64>,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    document: Option<TelegramMedia>,
    #[serde(default)]
    audio: Option<TelegramMedia>,
    #[serde(default)]
    voice: Option<TelegramMedia>,
    #[serde(default)]
    video: Option<TelegramMedia>,
    #[serde(default)]
    animation: Option<TelegramMedia>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_unique_id: String,
}

#[derive(Debug, Deserialize)]
struct TelegramMedia {
    file_id: String,
    file_unique_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    #[serde(default)]
    file_path: Option<String>,
}

fn classify_document_kind(document: &TelegramMedia) -> OutgoingAttachmentKind {
    match document.mime_type.as_deref() {
        Some(value) if value.starts_with("image/") => OutgoingAttachmentKind::Image,
        Some(value) if value.starts_with("audio/") => OutgoingAttachmentKind::Audio,
        Some(value) if value.starts_with("video/") => OutgoingAttachmentKind::Video,
        _ => OutgoingAttachmentKind::Document,
    }
}

fn render_user(user: &TelegramUser) -> String {
    user.username.clone().unwrap_or_else(|| {
        format!(
            "{} {}",
            user.first_name,
            user.last_name.clone().unwrap_or_default()
        )
        .trim()
        .to_string()
    })
}

fn render_message_time(unix_timestamp: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(unix_timestamp)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn render_chat_message(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => parts.push(context.text.clone()),
            ChatMessageItem::File(file) => parts.push(render_file_item(file)),
            ChatMessageItem::Compaction(_)
            | ChatMessageItem::SelectionReference(_)
            | ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::ToolCall(_) | ChatMessageItem::ToolResult(_) => {}
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n\n")
    }
}

fn is_visible_telegram_assistant_message(appended: &OutgoingMessageAppended) -> bool {
    if appended.message.role != ChatRole::Assistant {
        return false;
    }
    let message_part = appended
        .message_part
        .as_ref()
        .or(appended.message.message_part.as_ref());
    message_part == Some(&ChatMessagePart::FinalResponse)
}

fn model_selection_options(model_aliases: &[String]) -> OutgoingOptions {
    OutgoingOptions {
        options: model_aliases
            .iter()
            .map(|alias| OutgoingOption {
                label: alias.clone(),
                value: format!("/model {alias}"),
            })
            .collect(),
    }
}

fn decode_telegram_delivery_response<T: serde::de::DeserializeOwned>(
    method: &str,
    response: reqwest::blocking::Response,
) -> std::result::Result<T, TelegramDeliveryError> {
    let status = response.status();
    let retryable_status = is_retryable_telegram_status(status);
    let envelope =
        response
            .json::<TelegramEnvelope<T>>()
            .map_err(|error| TelegramDeliveryError {
                description: format!("invalid JSON response: {error}"),
                http_status: Some(status.as_u16()),
                error_code: None,
                retry_after: None,
                retryable: retryable_status,
            })?;
    if !envelope.ok || !status.is_success() {
        let error_code = envelope.error_code;
        let retryable = retryable_status || is_retryable_telegram_error_code(error_code);
        return Err(TelegramDeliveryError {
            description: envelope
                .description
                .unwrap_or_else(|| "unknown".to_string()),
            http_status: Some(status.as_u16()),
            error_code,
            retry_after: envelope
                .parameters
                .and_then(|parameters| parameters.retry_after),
            retryable,
        });
    }
    envelope.result.ok_or_else(|| TelegramDeliveryError {
        description: format!("telegram API {method} returned no result"),
        http_status: Some(status.as_u16()),
        error_code: None,
        retry_after: None,
        retryable: false,
    })
}

fn is_retryable_telegram_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_retryable_telegram_error_code(error_code: Option<i64>) -> bool {
    matches!(error_code, Some(429) | Some(500..=599))
}

fn delivery_retry_delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(seconds) = retry_after {
        return Duration::from_secs(seconds.max(1));
    }
    let shift = attempt.saturating_sub(1).min(5);
    let multiplier = 1_u64 << shift;
    TelegramChannel::DELIVERY_RETRY_INITIAL_DELAY
        .saturating_mul(u32::try_from(multiplier).unwrap_or(u32::MAX))
        .min(TelegramChannel::DELIVERY_RETRY_MAX_DELAY)
}

fn progress_panel_key(stream: &OutgoingSessionStream, turn_id: &str) -> String {
    format!(
        "{}:{}:{}",
        stream.conversation_id, stream.session_id, turn_id
    )
}

fn progress_panel_session_prefix(stream: &OutgoingSessionStream) -> String {
    format!("{}:{}:", stream.conversation_id, stream.session_id)
}

fn progress_panel_key_for_event(stream: &OutgoingSessionStream) -> Option<String> {
    if let Some(turn_id) = stream.event.get("turn_id").and_then(Value::as_str) {
        return Some(progress_panel_key(stream, turn_id));
    }
    None
}

fn render_progress_panel(panel: &TelegramProgressPanel) -> String {
    let mut lines = Vec::new();
    match panel.status {
        TelegramProgressStatus::Running => lines.push("**Stellaclaw 正在处理**".to_string()),
        TelegramProgressStatus::Completed => lines.push("**Stellaclaw 已完成**".to_string()),
        TelegramProgressStatus::Failed => lines.push("**Stellaclaw 处理失败**".to_string()),
    }
    lines.push(format!("状态: {}", progress_status_label(panel.status)));
    lines.push(format!(
        "耗时: {}",
        format_elapsed(panel.started_at.elapsed())
    ));
    if !panel.activity.trim().is_empty() {
        lines.push(format!("当前: {}", panel.activity.trim()));
    }
    if let Some(plan) = panel.plan.as_ref().and_then(render_plan_lines) {
        lines.push(String::new());
        lines.push("**计划**".to_string());
        lines.extend(plan);
    }
    lines.join("\n")
}

fn progress_status_label(status: TelegramProgressStatus) -> &'static str {
    match status {
        TelegramProgressStatus::Running => "运行中",
        TelegramProgressStatus::Completed => "完成",
        TelegramProgressStatus::Failed => "失败",
    }
}

fn format_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes:02}m")
}

fn render_plan_lines(plan: &Value) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    if let Some(explanation) = plan
        .get("explanation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.push(explanation.to_string());
    }
    let items = plan.get("plan").and_then(Value::as_array)?;
    for item in items.iter().take(8) {
        let step = item
            .get("step")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("未命名步骤");
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        lines.push(format!("{} {}", plan_status_marker(status), step));
    }
    if items.len() > 8 {
        lines.push(format!("... 还有 {} 步", items.len().saturating_sub(8)));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

fn plan_status_marker(status: &str) -> &'static str {
    match status {
        "completed" => "[x]",
        "in_progress" => "[>]",
        _ => "[ ]",
    }
}

fn render_tool_activity(event: &Value, prefix: &str) -> String {
    event
        .get("tool_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|tool| format!("{prefix}: {tool}"))
        .unwrap_or_else(|| prefix.to_string())
}

fn render_tool_result_activity(event: &Value) -> String {
    event
        .get("tool_result")
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|tool| format!("工具已返回: {tool}"))
        .unwrap_or_else(|| "工具已返回".to_string())
}

fn wait_workspace_response(
    rx: &Receiver<KernelChannelEvent>,
    timeout: Duration,
    request_id: &str,
) -> Result<WorkspaceResponse> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow!("workspace request timed out"));
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(KernelChannelEvent::Workspace {
                request_id: id,
                response,
            }) if id == request_id => return Ok(response),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return Err(anyhow!("workspace request timed out")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("conversation event stream closed"));
            }
        }
    }
}

fn telegram_request_id() -> String {
    static NEXT_TELEGRAM_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
    let counter = NEXT_TELEGRAM_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("telegram-workspace-{nanos:032x}{counter:016x}")
}

fn markdown_link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        targets.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    targets
}

fn normalize_markdown_path(value: &str) -> String {
    let value = value
        .split('#')
        .next()
        .unwrap_or(value)
        .split('?')
        .next()
        .unwrap_or(value)
        .trim();
    percent_decode(value)
        .replace('\\', "/")
        .trim_start_matches("./")
        .replace("//", "/")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn has_external_scheme(value: &str) -> bool {
    let Some(index) = value.find(':') else {
        return false;
    };
    value[..index]
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn safe_relative_path(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn path_to_slash_string(path: &Path) -> Option<String> {
    Some(path.to_string_lossy().replace('\\', "/")).filter(|value| !value.trim().is_empty())
}

fn is_local_overlay_path(path: &Path) -> bool {
    path.components().next().is_some_and(
        |component| matches!(component, Component::Normal(value) if value == ".stellaclaw"),
    )
}

fn local_path_from_file_item_uri(uri: &str) -> Option<PathBuf> {
    if let Some(path) = uri.strip_prefix("file://") {
        return Some(PathBuf::from(percent_decode(path)));
    }
    let path = PathBuf::from(percent_decode(uri));
    path.is_absolute().then_some(path)
}

fn local_attachment_from_path(
    path: &Path,
    media_type: Option<String>,
    max_bytes: usize,
) -> Option<TelegramOutgoingAttachment> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("attachment.bin")
        .to_string();
    Some(TelegramOutgoingAttachment {
        media_type: media_type.or_else(|| infer_media_type(path)),
        name,
        bytes,
    })
}

fn render_file_item(file: &FileItem) -> String {
    match &file.name {
        Some(name) => format!("[file] {name} ({})", file.uri),
        None => format!("[file] {}", file.uri),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TelegramRenderedText {
    text: String,
    entities: Vec<TelegramMessageEntity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct TelegramMessageEntity {
    #[serde(rename = "type")]
    kind: String,
    offset: usize,
    length: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RichDocument {
    blocks: Vec<RichBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RichBlock {
    Paragraph(Vec<RichInline>),
    Heading(Vec<RichInline>),
    BlockQuote(Vec<RichBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<RichBlock>>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Table(RichTable),
    ThematicBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RichTable {
    rows: Vec<Vec<String>>,
    header_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RichInline {
    Text(String),
    Emphasis(Vec<RichInline>),
    Strong(Vec<RichInline>),
    Strikethrough(Vec<RichInline>),
    Link {
        url: String,
        content: Vec<RichInline>,
    },
    Code(String),
    LineBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingBlockKind {
    Paragraph,
    Heading,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineStyle {
    Emphasis,
    Strong,
    Strikethrough,
    Link,
}

#[derive(Clone, Debug)]
enum InlineWrapper {
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
}

#[derive(Clone, Debug)]
struct PendingInlineBlock {
    kind: PendingBlockKind,
    inlines: Vec<RichInline>,
}

#[derive(Clone, Debug)]
struct PendingInlineContainer {
    style: InlineStyle,
    url: Option<String>,
    inlines: Vec<RichInline>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PendingTable {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    header_rows: usize,
}

#[derive(Clone, Debug)]
enum BlockContainer {
    Root(Vec<RichBlock>),
    BlockQuote(Vec<RichBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<RichBlock>>,
    },
    ListItem(Vec<RichBlock>),
}

#[derive(Clone, Copy, Debug)]
struct EntityCursor {
    byte: usize,
    utf16: usize,
}

#[derive(Default)]
struct TelegramEntityBuilder {
    text: String,
    entities: Vec<TelegramMessageEntity>,
}

impl TelegramEntityBuilder {
    fn cursor(&self) -> EntityCursor {
        EntityCursor {
            byte: self.text.len(),
            utf16: utf16_len(&self.text),
        }
    }

    fn push_text(&mut self, value: &str) {
        self.text.push_str(value);
    }

    fn push_entity_trimmed(
        &mut self,
        start: EntityCursor,
        kind: &str,
        url: Option<String>,
        language: Option<String>,
    ) {
        let end = self.cursor();
        let slice = &self.text[start.byte..end.byte];
        let leading_utf16 =
            utf16_len(slice.trim_start_matches(char::is_whitespace)).abs_diff(utf16_len(slice));
        let trailing_utf16 =
            utf16_len(slice.trim_end_matches(char::is_whitespace)).abs_diff(utf16_len(slice));
        let full_length = end.utf16.saturating_sub(start.utf16);
        let length = full_length
            .saturating_sub(leading_utf16)
            .saturating_sub(trailing_utf16);
        if length == 0 {
            return;
        }
        self.entities.push(TelegramMessageEntity {
            kind: kind.to_string(),
            offset: start.utf16 + leading_utf16,
            length,
            url,
            language,
        });
    }

    fn has_entity_of_kinds_in_range(
        &self,
        start_utf16: usize,
        end_utf16: usize,
        kinds: &[&str],
    ) -> bool {
        self.entities.iter().any(|entity| {
            entity.offset >= start_utf16
                && entity.offset + entity.length <= end_utf16
                && kinds.contains(&entity.kind.as_str())
        })
    }

    fn has_any_entity_in_range(&self, start_utf16: usize, end_utf16: usize) -> bool {
        self.entities.iter().any(|entity| {
            entity.offset >= start_utf16 && entity.offset + entity.length <= end_utf16
        })
    }
}

fn build_inline_keyboard_markup(options: &OutgoingOptions) -> serde_json::Value {
    let mut rows = Vec::new();
    for chunk in options.options.chunks(2) {
        let row = chunk
            .iter()
            .map(|option| {
                json!({
                    "text": option.label,
                    "callback_data": option.value,
                })
            })
            .collect::<Vec<_>>();
        rows.push(row);
    }
    json!({
        "inline_keyboard": rows,
    })
}

fn build_send_text_payload(
    chat_id: &str,
    rendered: TelegramRenderedText,
    options: Option<&OutgoingOptions>,
) -> Result<serde_json::Value> {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": rendered.text,
        "disable_web_page_preview": true,
    });
    if !rendered.entities.is_empty() {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "entities".to_string(),
                serde_json::to_value(rendered.entities)
                    .context("failed to encode telegram entities")?,
            );
        }
    }
    if let Some(options) = options.filter(|value| !value.options.is_empty()) {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "reply_markup".to_string(),
                build_inline_keyboard_markup(options),
            );
        }
    }
    Ok(payload)
}

fn parse_markdown_to_rich_document(input: &str) -> RichDocument {
    let parser = Parser::new_ext(input, Options::all());
    let mut block_stack = vec![BlockContainer::Root(Vec::new())];
    let mut pending_inline_block: Option<PendingInlineBlock> = None;
    let mut inline_stack: Vec<PendingInlineContainer> = Vec::new();
    let mut code_block_language: Option<String> = None;
    let mut code_block_buffer: Option<String> = None;
    let mut pending_table: Option<PendingTable> = None;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Table(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_table = Some(PendingTable::default());
                }
                Tag::TableHead => {}
                Tag::TableRow => {
                    if let Some(table) = pending_table.as_mut() {
                        table.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = pending_table.as_mut() {
                        table.current_cell.clear();
                    }
                }
                Tag::Paragraph => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_inline_block = Some(PendingInlineBlock {
                        kind: PendingBlockKind::Paragraph,
                        inlines: Vec::new(),
                    });
                }
                Tag::Heading { .. } => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_inline_block = Some(PendingInlineBlock {
                        kind: PendingBlockKind::Heading,
                        inlines: Vec::new(),
                    });
                }
                Tag::BlockQuote(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::BlockQuote(Vec::new()));
                }
                Tag::List(start) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::List {
                        start,
                        items: Vec::new(),
                    });
                }
                Tag::Item => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::ListItem(Vec::new()));
                }
                Tag::Emphasis => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Emphasis,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Strong => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Strong,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Strikethrough => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Strikethrough,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Link { dest_url, .. } => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Link,
                        url: Some(dest_url.to_string()),
                        inlines: Vec::new(),
                    });
                }
                Tag::CodeBlock(kind) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    code_block_language = match kind {
                        CodeBlockKind::Indented => None,
                        CodeBlockKind::Fenced(language) => {
                            let trimmed = language.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_string())
                            }
                        }
                    };
                    code_block_buffer = Some(String::new());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Table => {
                    if let Some(table) = pending_table.take() {
                        push_block(
                            &mut block_stack,
                            RichBlock::Table(RichTable {
                                rows: table.rows,
                                header_rows: table.header_rows,
                            }),
                        );
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = pending_table.as_mut() {
                        if !table.current_row.is_empty() {
                            table.rows.push(std::mem::take(&mut table.current_row));
                        }
                        table.header_rows = table.rows.len();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = pending_table.as_mut() {
                        if !table.current_row.is_empty() {
                            table.rows.push(std::mem::take(&mut table.current_row));
                        }
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = pending_table.as_mut() {
                        table
                            .current_row
                            .push(normalize_table_cell(&table.current_cell));
                        table.current_cell.clear();
                    }
                }
                TagEnd::Paragraph | TagEnd::Heading(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::BlockQuote(blocks)) = block_stack.pop() {
                        push_block(&mut block_stack, RichBlock::BlockQuote(blocks));
                    }
                }
                TagEnd::List(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::List { start, items }) = block_stack.pop() {
                        push_block(&mut block_stack, RichBlock::List { start, items });
                    }
                }
                TagEnd::Item => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::ListItem(blocks)) = block_stack.pop() {
                        if let Some(BlockContainer::List { items, .. }) = block_stack.last_mut() {
                            items.push(blocks);
                        }
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    if let Some(container) = inline_stack.pop() {
                        let inline = match container.style {
                            InlineStyle::Emphasis => RichInline::Emphasis(container.inlines),
                            InlineStyle::Strong => RichInline::Strong(container.inlines),
                            InlineStyle::Strikethrough => {
                                RichInline::Strikethrough(container.inlines)
                            }
                            InlineStyle::Link => RichInline::Link {
                                url: container.url.unwrap_or_default(),
                                content: container.inlines,
                            },
                        };
                        push_inline(&mut pending_inline_block, &mut inline_stack, inline);
                    }
                }
                TagEnd::CodeBlock => {
                    let code = code_block_buffer.take().unwrap_or_default();
                    let language = code_block_language.take();
                    push_block(&mut block_stack, RichBlock::CodeBlock { language, code });
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some(table) = pending_table.as_mut() {
                    table.current_cell.push_str(&text);
                } else if let Some(buffer) = code_block_buffer.as_mut() {
                    buffer.push_str(&text);
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::Text(text.to_string()),
                    );
                }
            }
            Event::Code(code) => {
                if let Some(table) = pending_table.as_mut() {
                    table.current_cell.push_str(&code);
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::Code(code.to_string()),
                    );
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = pending_table.as_mut() {
                    if !table.current_cell.ends_with(' ') {
                        table.current_cell.push(' ');
                    }
                } else if let Some(buffer) = code_block_buffer.as_mut() {
                    buffer.push('\n');
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::LineBreak,
                    );
                }
            }
            Event::Rule => {
                flush_inline_block(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    &mut block_stack,
                );
                push_block(&mut block_stack, RichBlock::ThematicBreak);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(html.to_string()),
                );
            }
            Event::InlineMath(math) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Code(math.to_string()),
                );
            }
            Event::DisplayMath(math) => {
                flush_inline_block(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    &mut block_stack,
                );
                push_block(
                    &mut block_stack,
                    RichBlock::CodeBlock {
                        language: None,
                        code: math.to_string(),
                    },
                );
            }
            Event::FootnoteReference(text) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(format!("[{}]", text)),
                );
            }
            Event::TaskListMarker(checked) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(if checked {
                        "☑ ".to_string()
                    } else {
                        "☐ ".to_string()
                    }),
                );
            }
        }
    }

    flush_inline_block(
        &mut pending_inline_block,
        &mut inline_stack,
        &mut block_stack,
    );

    let blocks = match block_stack.pop() {
        Some(BlockContainer::Root(blocks)) => blocks,
        _ => Vec::new(),
    };
    RichDocument { blocks }
}

fn ensure_inline_block(pending_inline_block: &mut Option<PendingInlineBlock>) {
    if pending_inline_block.is_none() {
        *pending_inline_block = Some(PendingInlineBlock {
            kind: PendingBlockKind::Paragraph,
            inlines: Vec::new(),
        });
    }
}

fn push_inline(
    pending_inline_block: &mut Option<PendingInlineBlock>,
    inline_stack: &mut Vec<PendingInlineContainer>,
    inline: RichInline,
) {
    ensure_inline_block(pending_inline_block);
    if let Some(container) = inline_stack.last_mut() {
        container.inlines.push(inline);
    } else if let Some(block) = pending_inline_block.as_mut() {
        block.inlines.push(inline);
    }
}

fn push_block(block_stack: &mut [BlockContainer], block: RichBlock) {
    if let Some(container) = block_stack.last_mut() {
        match container {
            BlockContainer::Root(blocks)
            | BlockContainer::BlockQuote(blocks)
            | BlockContainer::ListItem(blocks) => blocks.push(block),
            BlockContainer::List { .. } => {}
        }
    }
}

fn flush_inline_block(
    pending_inline_block: &mut Option<PendingInlineBlock>,
    inline_stack: &mut Vec<PendingInlineContainer>,
    block_stack: &mut [BlockContainer],
) {
    while let Some(container) = inline_stack.pop() {
        let inline = match container.style {
            InlineStyle::Emphasis => RichInline::Emphasis(container.inlines),
            InlineStyle::Strong => RichInline::Strong(container.inlines),
            InlineStyle::Strikethrough => RichInline::Strikethrough(container.inlines),
            InlineStyle::Link => RichInline::Link {
                url: container.url.unwrap_or_default(),
                content: container.inlines,
            },
        };
        push_inline(pending_inline_block, inline_stack, inline);
    }

    let Some(block) = pending_inline_block.take() else {
        return;
    };
    if block.inlines.is_empty() {
        return;
    }
    let rich_block = match block.kind {
        PendingBlockKind::Paragraph => RichBlock::Paragraph(block.inlines),
        PendingBlockKind::Heading => RichBlock::Heading(block.inlines),
    };
    push_block(block_stack, rich_block);
}

fn render_markdown_chunks_to_telegram_entities(
    input: &str,
    max_chars: usize,
) -> Vec<TelegramRenderedText> {
    split_markdown_for_telegram_documents(input, max_chars)
        .into_iter()
        .map(|document| render_rich_document_to_telegram_entities(&document))
        .collect()
}

fn render_rich_document_to_telegram_entities(document: &RichDocument) -> TelegramRenderedText {
    let mut builder = TelegramEntityBuilder::default();
    let mut need_paragraph_break = false;
    render_blocks_to_telegram_entities(
        &document.blocks,
        &mut builder,
        &mut need_paragraph_break,
        0,
    );
    builder.entities.sort_by(|left, right| {
        left.offset
            .cmp(&right.offset)
            .then(right.length.cmp(&left.length))
    });
    TelegramRenderedText {
        text: builder.text,
        entities: builder.entities,
    }
}

fn render_blocks_to_telegram_entities(
    blocks: &[RichBlock],
    builder: &mut TelegramEntityBuilder,
    need_paragraph_break: &mut bool,
    quote_depth: usize,
) {
    for block in blocks {
        match block {
            RichBlock::Paragraph(inlines) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                render_inlines_to_telegram_entities(inlines, builder, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::Heading(inlines) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let start = builder.cursor();
                render_inlines_to_telegram_entities(inlines, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "bold", None, None);
                *need_paragraph_break = true;
            }
            RichBlock::BlockQuote(inner) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                let start = builder.cursor();
                render_blocks_to_telegram_entities(
                    inner,
                    builder,
                    need_paragraph_break,
                    quote_depth + 1,
                );
                if quote_depth == 0 {
                    builder.push_entity_trimmed(
                        start,
                        classify_blockquote_entity(&builder.text[start.byte..builder.text.len()]),
                        None,
                        None,
                    );
                }
            }
            RichBlock::List { start, items } => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                render_list_to_telegram_entities(*start, items, builder, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::CodeBlock { language, code } => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let collapse = quote_depth == 0 && should_collapse_code_block(code);
                let outer_start = builder.cursor();
                let start = builder.cursor();
                builder.push_text(code);
                builder.push_entity_trimmed(start, "pre", None, language.clone());
                if collapse {
                    builder.push_entity_trimmed(outer_start, "expandable_blockquote", None, None);
                }
                *need_paragraph_break = true;
            }
            RichBlock::Table(table) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let table_text = render_table_text(table);
                let collapse = quote_depth == 0
                    && (table_text.lines().count() >= 15 || table_text.chars().count() >= 600);
                let outer_start = builder.cursor();
                let start = builder.cursor();
                builder.push_text(&table_text);
                builder.push_entity_trimmed(start, "pre", None, None);
                if collapse {
                    builder.push_entity_trimmed(outer_start, "expandable_blockquote", None, None);
                }
                *need_paragraph_break = true;
            }
            RichBlock::ThematicBreak => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                builder.push_text("──────────");
                *need_paragraph_break = true;
            }
        }
    }
}

fn render_list_to_telegram_entities(
    start: Option<u64>,
    items: &[Vec<RichBlock>],
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    let mut next_number = start.unwrap_or(1);
    for (index, item) in items.iter().enumerate() {
        if index > 0 && !builder.text.ends_with('\n') {
            builder.push_text("\n");
        }
        maybe_render_nested_quote_prefix(builder, quote_depth);
        if start.is_some() {
            builder.push_text(&format!("{}. ", next_number));
            next_number += 1;
        } else {
            builder.push_text("• ");
        }
        if let Some((first, rest)) = item.split_first() {
            render_first_list_block_to_telegram_entities(first, builder, quote_depth);
            if !rest.is_empty() {
                let mut nested_break = true;
                render_blocks_to_telegram_entities(rest, builder, &mut nested_break, quote_depth);
            }
        }
    }
}

fn render_first_list_block_to_telegram_entities(
    block: &RichBlock,
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    match block {
        RichBlock::Paragraph(inlines) => {
            render_inlines_to_telegram_entities(inlines, builder, quote_depth)
        }
        RichBlock::Heading(inlines) => {
            let start = builder.cursor();
            render_inlines_to_telegram_entities(inlines, builder, quote_depth);
            maybe_push_wrapping_entity(builder, start, "bold", None, None);
        }
        RichBlock::CodeBlock { language, code } => {
            builder.push_text("\n");
            maybe_render_nested_quote_prefix(builder, quote_depth);
            let start = builder.cursor();
            builder.push_text(code);
            builder.push_entity_trimmed(start, "pre", None, language.clone());
        }
        RichBlock::Table(table) => {
            builder.push_text("\n");
            maybe_render_nested_quote_prefix(builder, quote_depth);
            let start = builder.cursor();
            builder.push_text(&render_table_text(table));
            builder.push_entity_trimmed(start, "pre", None, None);
        }
        RichBlock::ThematicBreak => builder.push_text("──────────"),
        RichBlock::BlockQuote(inner) => {
            let mut nested_break = false;
            render_blocks_to_telegram_entities(inner, builder, &mut nested_break, quote_depth + 1);
        }
        RichBlock::List { start, items } => {
            builder.push_text("\n");
            render_list_to_telegram_entities(*start, items, builder, quote_depth);
        }
    }
}

fn render_inlines_to_telegram_entities(
    inlines: &[RichInline],
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    for inline in inlines {
        match inline {
            RichInline::Text(text) => render_text_to_telegram_entities(text, builder, quote_depth),
            RichInline::Emphasis(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "italic", None, None);
            }
            RichInline::Strong(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "bold", None, None);
            }
            RichInline::Strikethrough(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "strikethrough", None, None);
            }
            RichInline::Link { url, content } => {
                if is_telegram_text_link_url(url) {
                    let start = builder.cursor();
                    render_inlines_to_telegram_entities(content, builder, quote_depth);
                    let end = builder.cursor();
                    if !builder.has_any_entity_in_range(start.utf16, end.utf16) {
                        builder.push_entity_trimmed(start, "text_link", Some(url.clone()), None);
                    }
                } else if is_local_markdown_attachment_target(url) {
                    render_text_to_telegram_entities(
                        &telegram_file_link_placeholder(url, content),
                        builder,
                        quote_depth,
                    );
                } else {
                    let start = builder.cursor();
                    render_inlines_to_telegram_entities(content, builder, quote_depth);
                    let end = builder.cursor();
                    let label = builder.text[start.byte..end.byte].trim().to_string();
                    let target = url.trim();
                    if !target.is_empty() && label != target {
                        builder.push_text(" (");
                        builder.push_text(target);
                        builder.push_text(")");
                    }
                }
            }
            RichInline::Code(code) => {
                let start = builder.cursor();
                builder.push_text(code);
                builder.push_entity_trimmed(start, "code", None, None);
            }
            RichInline::LineBreak => builder.push_text("\n"),
        }
    }
}

fn is_telegram_text_link_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return false;
    }
    trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty())
}

fn is_local_markdown_attachment_target(url: &str) -> bool {
    let target = normalize_markdown_path(url);
    !target.is_empty() && !target.starts_with("attachment://") && !has_external_scheme(&target)
}

fn telegram_file_link_placeholder(url: &str, content: &[RichInline]) -> String {
    let label = plain_text_from_inlines(content).trim().to_string();
    let target = normalize_markdown_path(url);
    let fallback = target
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(target.trim())
        .trim();
    let display = if !label.is_empty() && label != url.trim() {
        label.as_str()
    } else if !fallback.is_empty() {
        fallback
    } else if !label.is_empty() {
        label.as_str()
    } else {
        "文件"
    };
    format!("📎 {display}")
}

fn plain_text_from_inlines(inlines: &[RichInline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            RichInline::Text(text) | RichInline::Code(text) => out.push_str(text),
            RichInline::LineBreak => out.push('\n'),
            RichInline::Emphasis(children)
            | RichInline::Strong(children)
            | RichInline::Strikethrough(children)
            | RichInline::Link {
                content: children, ..
            } => out.push_str(&plain_text_from_inlines(children)),
        }
    }
    out
}

fn render_text_to_telegram_entities(
    text: &str,
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    for segment in text.split_inclusive('\n') {
        maybe_render_nested_quote_prefix(builder, quote_depth);
        builder.push_text(segment);
    }
}

fn maybe_render_nested_quote_prefix(builder: &mut TelegramEntityBuilder, quote_depth: usize) {
    if quote_depth > 1 && (builder.text.is_empty() || builder.text.ends_with('\n')) {
        builder.push_text(&"> ".repeat(quote_depth - 1));
    }
}

fn classify_blockquote_entity(text: &str) -> &'static str {
    let line_count = text.lines().count();
    let char_count = text.chars().count();
    if line_count >= 6 || char_count >= 360 {
        "expandable_blockquote"
    } else {
        "blockquote"
    }
}

fn should_collapse_code_block(code: &str) -> bool {
    let line_count = code.lines().count();
    let char_count = code.chars().count();
    line_count >= 15 || char_count >= 600
}

fn maybe_push_wrapping_entity(
    builder: &mut TelegramEntityBuilder,
    start: EntityCursor,
    kind: &str,
    url: Option<String>,
    language: Option<String>,
) {
    let end = builder.cursor();
    if matches!(kind, "bold" | "italic" | "strikethrough")
        && builder.has_entity_of_kinds_in_range(start.utf16, end.utf16, &["code", "pre"])
    {
        return;
    }
    builder.push_entity_trimmed(start, kind, url, language);
}

fn split_markdown_for_telegram_documents(input: &str, max_chars: usize) -> Vec<RichDocument> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let document = parse_markdown_to_rich_document(trimmed);
    if rendered_length_for_document(&document) <= max_chars {
        return vec![document];
    }

    match split_blocks_to_fit(&document.blocks, max_chars) {
        Some(chunks) => chunks
            .into_iter()
            .map(|blocks| RichDocument { blocks })
            .collect(),
        None => split_markdown_message_legacy(trimmed, max_chars)
            .into_iter()
            .map(|chunk| parse_markdown_to_rich_document(&chunk))
            .collect(),
    }
}

fn rendered_length_for_document(document: &RichDocument) -> usize {
    telegram_text_len(&render_rich_document_to_telegram_entities(document).text)
}

fn rendered_length_for_blocks(blocks: &[RichBlock]) -> usize {
    rendered_length_for_document(&RichDocument {
        blocks: blocks.to_vec(),
    })
}

fn rendered_length_for_block(block: &RichBlock) -> usize {
    rendered_length_for_document(&RichDocument {
        blocks: vec![block.clone()],
    })
}

fn split_blocks_to_fit(blocks: &[RichBlock], max_chars: usize) -> Option<Vec<Vec<RichBlock>>> {
    let mut chunks = Vec::new();
    let mut current_blocks = Vec::new();

    for block in blocks {
        let split_parts = split_block_to_fit(block, max_chars)?;
        for part in split_parts {
            let mut candidate_blocks = current_blocks.clone();
            candidate_blocks.extend(part.clone());
            if rendered_length_for_blocks(&candidate_blocks) <= max_chars {
                current_blocks = candidate_blocks;
                continue;
            }
            if !current_blocks.is_empty() {
                chunks.push(std::mem::take(&mut current_blocks));
            }
            if rendered_length_for_blocks(&part) > max_chars {
                return None;
            }
            current_blocks = part;
        }
    }

    if !current_blocks.is_empty() {
        chunks.push(current_blocks);
    }
    Some(chunks)
}

fn split_block_to_fit(block: &RichBlock, max_chars: usize) -> Option<Vec<Vec<RichBlock>>> {
    if rendered_length_for_block(block) <= max_chars {
        return Some(vec![vec![block.clone()]]);
    }

    match block {
        RichBlock::Paragraph(inlines) => {
            split_inline_block_to_fit(PendingBlockKind::Paragraph, inlines, max_chars)
        }
        RichBlock::Heading(inlines) => {
            split_inline_block_to_fit(PendingBlockKind::Heading, inlines, max_chars)
        }
        RichBlock::BlockQuote(blocks) => {
            let chunks = split_blocks_to_fit(blocks, max_chars)?;
            Some(
                chunks
                    .into_iter()
                    .map(|chunk| vec![RichBlock::BlockQuote(chunk)])
                    .collect(),
            )
        }
        RichBlock::List { start, items } => split_list_block_to_fit(*start, items, max_chars),
        RichBlock::CodeBlock { language, code } => {
            split_code_block_to_fit(language.clone(), code, max_chars)
        }
        RichBlock::Table(_) => None,
        RichBlock::ThematicBreak => Some(vec![vec![RichBlock::ThematicBreak]]),
    }
}

fn split_inline_block_to_fit(
    kind: PendingBlockKind,
    inlines: &[RichInline],
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();

    for inline in inlines {
        let split_parts = split_inline_to_fit(kind, inline, max_chars)?;
        for part in split_parts {
            let mut candidate = current.clone();
            candidate.push(part.clone());
            if rendered_length_for_inline_block(kind, &candidate) <= max_chars {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                chunks.push(vec![inline_block_from_parts(
                    kind,
                    std::mem::take(&mut current),
                )]);
            }
            if rendered_length_for_inline_block(kind, std::slice::from_ref(&part)) > max_chars {
                return None;
            }
            current.push(part);
        }
    }

    if !current.is_empty() {
        chunks.push(vec![inline_block_from_parts(kind, current)]);
    }
    Some(chunks)
}

fn split_inline_to_fit(
    block_kind: PendingBlockKind,
    inline: &RichInline,
    max_chars: usize,
) -> Option<Vec<RichInline>> {
    if rendered_length_for_inline_block(block_kind, std::slice::from_ref(inline)) <= max_chars {
        return Some(vec![inline.clone()]);
    }

    match inline {
        RichInline::Text(text) => {
            split_leaf_inline_text_to_fit(block_kind, text, max_chars, |value| {
                RichInline::Text(value)
            })
        }
        RichInline::Code(code) => {
            split_leaf_inline_text_to_fit(block_kind, code, max_chars, RichInline::Code)
        }
        RichInline::LineBreak => Some(vec![RichInline::LineBreak]),
        RichInline::Emphasis(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Emphasis)
        }
        RichInline::Strong(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Strong)
        }
        RichInline::Strikethrough(children) => split_wrapped_inline_to_fit(
            block_kind,
            children,
            max_chars,
            &InlineWrapper::Strikethrough,
        ),
        RichInline::Link { url, content } => split_wrapped_inline_to_fit(
            block_kind,
            content,
            max_chars,
            &InlineWrapper::Link(url.clone()),
        ),
    }
}

fn split_leaf_inline_text_to_fit<F>(
    block_kind: PendingBlockKind,
    text: &str,
    max_chars: usize,
    make_inline: F,
) -> Option<Vec<RichInline>>
where
    F: Fn(String) -> RichInline,
{
    split_text_to_fit(
        text,
        |candidate| {
            rendered_length_for_inline_block(block_kind, &[make_inline(candidate.to_string())])
        },
        max_chars,
    )
    .map(|parts| parts.into_iter().map(make_inline).collect::<Vec<_>>())
    .filter(|parts| {
        !parts.is_empty()
            && parts.iter().all(|inline| {
                rendered_length_for_inline_block(block_kind, std::slice::from_ref(inline))
                    <= max_chars
            })
    })
}

fn split_wrapped_inline_to_fit(
    block_kind: PendingBlockKind,
    children: &[RichInline],
    max_chars: usize,
    wrapper: &InlineWrapper,
) -> Option<Vec<RichInline>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();

    for child in children {
        let split_parts = split_child_for_wrapped_chunk(block_kind, child, max_chars, wrapper)?;
        for part in split_parts {
            let mut candidate = current.clone();
            candidate.push(part.clone());
            if rendered_length_for_inline_block(
                block_kind,
                &[apply_inline_wrapper(wrapper, candidate.clone())],
            ) <= max_chars
            {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                chunks.push(apply_inline_wrapper(wrapper, std::mem::take(&mut current)));
            }
            if rendered_length_for_inline_block(
                block_kind,
                &[apply_inline_wrapper(wrapper, vec![part.clone()])],
            ) > max_chars
            {
                return None;
            }
            current.push(part);
        }
    }

    if !current.is_empty() {
        chunks.push(apply_inline_wrapper(wrapper, current));
    }

    Some(chunks)
}

fn split_child_for_wrapped_chunk(
    block_kind: PendingBlockKind,
    child: &RichInline,
    max_chars: usize,
    wrapper: &InlineWrapper,
) -> Option<Vec<RichInline>> {
    if rendered_length_for_inline_block(
        block_kind,
        &[apply_inline_wrapper(wrapper, vec![child.clone()])],
    ) <= max_chars
    {
        return Some(vec![child.clone()]);
    }

    match child {
        RichInline::Text(text) => split_text_to_fit(
            text,
            |candidate| {
                rendered_length_for_inline_block(
                    block_kind,
                    &[apply_inline_wrapper(
                        wrapper,
                        vec![RichInline::Text(candidate.to_string())],
                    )],
                )
            },
            max_chars,
        )
        .map(|parts| parts.into_iter().map(RichInline::Text).collect()),
        RichInline::Code(code) => split_text_to_fit(
            code,
            |candidate| {
                rendered_length_for_inline_block(
                    block_kind,
                    &[apply_inline_wrapper(
                        wrapper,
                        vec![RichInline::Code(candidate.to_string())],
                    )],
                )
            },
            max_chars,
        )
        .map(|parts| parts.into_iter().map(RichInline::Code).collect()),
        RichInline::LineBreak => Some(vec![RichInline::LineBreak]),
        RichInline::Emphasis(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Emphasis)
        }
        RichInline::Strong(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Strong)
        }
        RichInline::Strikethrough(children) => split_wrapped_inline_to_fit(
            block_kind,
            children,
            max_chars,
            &InlineWrapper::Strikethrough,
        ),
        RichInline::Link { url, content } => split_wrapped_inline_to_fit(
            block_kind,
            content,
            max_chars,
            &InlineWrapper::Link(url.clone()),
        ),
    }
}

fn apply_inline_wrapper(wrapper: &InlineWrapper, children: Vec<RichInline>) -> RichInline {
    match wrapper {
        InlineWrapper::Emphasis => RichInline::Emphasis(children),
        InlineWrapper::Strong => RichInline::Strong(children),
        InlineWrapper::Strikethrough => RichInline::Strikethrough(children),
        InlineWrapper::Link(url) => RichInline::Link {
            url: url.clone(),
            content: children,
        },
    }
}

fn split_list_block_to_fit(
    start: Option<u64>,
    items: &[Vec<RichBlock>],
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let mut normalized_items = Vec::new();
    for item in items {
        if rendered_length_for_single_item_list(start, item) <= max_chars {
            normalized_items.push(item.clone());
            continue;
        }
        let item_chunks = split_blocks_to_fit(item, max_chars)?;
        for chunk in item_chunks {
            if rendered_length_for_single_item_list(start, &chunk) > max_chars {
                return None;
            }
            normalized_items.push(chunk);
        }
    }

    let mut chunks = Vec::new();
    let mut current_items = Vec::new();
    let mut current_start = start;
    let mut next_number = start.unwrap_or(1);

    for item in normalized_items {
        let candidate_items = {
            let mut items = current_items.clone();
            items.push(item.clone());
            items
        };
        let candidate_block = RichBlock::List {
            start: current_start,
            items: candidate_items.clone(),
        };
        if rendered_length_for_block(&candidate_block) <= max_chars {
            current_items = candidate_items;
        } else {
            if !current_items.is_empty() {
                chunks.push(vec![RichBlock::List {
                    start: current_start,
                    items: std::mem::take(&mut current_items),
                }]);
                current_start = start.map(|_| next_number);
            }
            current_items.push(item.clone());
            let single_block = RichBlock::List {
                start: current_start,
                items: current_items.clone(),
            };
            if rendered_length_for_block(&single_block) > max_chars {
                return None;
            }
        }
        if start.is_some() {
            next_number += 1;
        }
    }

    if !current_items.is_empty() {
        chunks.push(vec![RichBlock::List {
            start: current_start,
            items: current_items,
        }]);
    }

    Some(chunks)
}

fn split_code_block_to_fit(
    language: Option<String>,
    code: &str,
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let parts = split_text_to_fit(
        code,
        |candidate| {
            rendered_length_for_block(&RichBlock::CodeBlock {
                language: language.clone(),
                code: candidate.to_string(),
            })
        },
        max_chars,
    )?;

    let chunks = parts
        .into_iter()
        .map(|part| {
            vec![RichBlock::CodeBlock {
                language: language.clone(),
                code: part,
            }]
        })
        .collect::<Vec<_>>();

    if chunks
        .iter()
        .all(|chunk| rendered_length_for_blocks(chunk) <= max_chars)
    {
        Some(chunks)
    } else {
        None
    }
}

fn split_text_to_fit<F>(text: &str, measure: F, max_chars: usize) -> Option<Vec<String>>
where
    F: Fn(&str) -> usize,
{
    if text.is_empty() {
        return Some(Vec::new());
    }

    let chars: Vec<char> = text.chars().collect();
    let mut cursor = 0usize;
    let mut chunks = Vec::new();

    while cursor < chars.len() {
        let remaining = chars.len() - cursor;
        let mut low = 1usize;
        let mut high = remaining;
        let mut best = 0usize;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate: String = chars[cursor..cursor + mid].iter().collect();
            if measure(&candidate) <= max_chars {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }
        if best == 0 {
            return None;
        }

        let mut end = cursor + best;
        if end < chars.len() {
            if let Some(adjusted) = prefer_split_boundary_with_measure(
                &chars[cursor..end],
                best / 2,
                &measure,
                max_chars,
            ) {
                end = cursor + adjusted;
            }
        }

        let chunk: String = chars[cursor..end].iter().collect();
        if chunk.is_empty() {
            return None;
        }
        chunks.push(chunk);
        cursor = end;
    }

    Some(chunks)
}

fn prefer_split_boundary_with_measure<F>(
    chars: &[char],
    minimum_index: usize,
    measure: &F,
    max_chars: usize,
) -> Option<usize>
where
    F: Fn(&str) -> usize,
{
    let text: String = chars.iter().collect();
    for needle in ["\n\n", "\n", " "] {
        if let Some(index) = text.rfind(needle) {
            let split_index = index + needle.len();
            if split_index >= minimum_index {
                let candidate = &text[..split_index];
                if measure(candidate) <= max_chars {
                    return Some(candidate.chars().count());
                }
            }
        }
    }
    None
}

fn inline_block_from_parts(kind: PendingBlockKind, inlines: Vec<RichInline>) -> RichBlock {
    match kind {
        PendingBlockKind::Paragraph => RichBlock::Paragraph(inlines),
        PendingBlockKind::Heading => RichBlock::Heading(inlines),
    }
}

fn rendered_length_for_inline_block(kind: PendingBlockKind, inlines: &[RichInline]) -> usize {
    rendered_length_for_block(&inline_block_from_parts(kind, inlines.to_vec()))
}

fn rendered_length_for_single_item_list(start: Option<u64>, item: &[RichBlock]) -> usize {
    rendered_length_for_block(&RichBlock::List {
        start,
        items: vec![item.to_vec()],
    })
}

fn split_markdown_message_legacy(input: &str, max_chars: usize) -> Vec<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = trimmed.chars().collect();
    let mut cursor = 0usize;
    let mut chunks = Vec::new();
    while cursor < chars.len() {
        let remaining = chars.len() - cursor;
        let mut low = 1usize;
        let mut high = remaining;
        let mut best = 1usize;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate: String = chars[cursor..cursor + mid].iter().collect();
            let translated_len = telegram_text_len(
                &render_rich_document_to_telegram_entities(&parse_markdown_to_rich_document(
                    &candidate,
                ))
                .text,
            );
            if translated_len <= max_chars {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }

        let mut end = cursor + best;
        if end < chars.len() {
            if let Some(adjusted) = prefer_split_boundary(&chars[cursor..end], best / 2) {
                end = cursor + adjusted;
            }
        }

        let chunk: String = chars[cursor..end].iter().collect();
        let chunk = chunk.trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        cursor = end;
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
    }
    chunks
}

fn prefer_split_boundary(chars: &[char], minimum_index: usize) -> Option<usize> {
    let text: String = chars.iter().collect();
    for needle in ["\n\n", "\n", " "] {
        if let Some(index) = text.rfind(needle) {
            let split_index = index + needle.len();
            if split_index >= minimum_index {
                return Some(text[..split_index].chars().count());
            }
        }
    }
    None
}

fn ensure_block_break_text(output: &mut String, need_paragraph_break: &mut bool) {
    if output.is_empty() {
        *need_paragraph_break = false;
        return;
    }
    if *need_paragraph_break {
        if !output.ends_with("\n\n") {
            if output.ends_with('\n') {
                output.push('\n');
            } else {
                output.push_str("\n\n");
            }
        }
        *need_paragraph_break = false;
    } else if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn normalize_table_cell(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pad_table_cell(value: &str, width: usize) -> String {
    let cell_width = value.chars().count();
    if cell_width >= width {
        value.to_string()
    } else {
        format!("{}{}", value, " ".repeat(width - cell_width))
    }
}

fn render_table_text(table: &RichTable) -> String {
    if table.rows.is_empty() {
        return String::new();
    }

    let column_count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if column_count == 0 {
        return String::new();
    }

    let widths = (0..column_count)
        .map(|column| {
            table
                .rows
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();

    let mut lines = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        let rendered = (0..column_count)
            .map(|column| {
                let value = row.get(column).cloned().unwrap_or_default();
                pad_table_cell(&value, widths[column])
            })
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(rendered);
        if table.header_rows > 0 && index + 1 == table.header_rows {
            let separator = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("─┼─");
            lines.push(separator);
        }
    }

    lines.join("\n")
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn telegram_text_len(value: &str) -> usize {
    utf16_len(value)
}

fn sanitize_file_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized = "attachment.bin".to_string();
    }
    sanitized
}

fn infer_extension(media_type: Option<&str>, kind: OutgoingAttachmentKind) -> &'static str {
    match (media_type.unwrap_or_default(), kind) {
        ("image/png", _) => "png",
        ("image/webp", _) => "webp",
        ("image/gif", _) => "gif",
        ("audio/mpeg", _) => "mp3",
        ("audio/ogg", _) => "ogg",
        ("audio/wav", _) => "wav",
        ("video/mp4", _) => "mp4",
        ("application/pdf", _) => "pdf",
        (_, OutgoingAttachmentKind::Image) => "jpg",
        (_, OutgoingAttachmentKind::Audio) => "mp3",
        (_, OutgoingAttachmentKind::Voice) => "ogg",
        (_, OutgoingAttachmentKind::Video) => "mp4",
        (_, OutgoingAttachmentKind::Animation) => "gif",
        (_, OutgoingAttachmentKind::Document) => "bin",
    }
}

fn infer_media_type(path: &Path) -> Option<String> {
    match path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "webp" => Some("image/webp".to_string()),
        "gif" => Some("image/gif".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "ogg" => Some("audio/ogg".to_string()),
        "wav" => Some("audio/wav".to_string()),
        "mp4" => Some("video/mp4".to_string()),
        _ => None,
    }
}

fn kind_label(kind: OutgoingAttachmentKind) -> &'static str {
    match kind {
        OutgoingAttachmentKind::Image => "image",
        OutgoingAttachmentKind::Audio => "audio",
        OutgoingAttachmentKind::Voice => "voice",
        OutgoingAttachmentKind::Video => "video",
        OutgoingAttachmentKind::Animation => "animation",
        OutgoingAttachmentKind::Document => "document",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_send_text_payload, delivery_retry_delay, is_retryable_telegram_error_code,
        is_retryable_telegram_status, is_visible_telegram_assistant_message,
        local_attachment_from_path, local_path_from_file_item_uri, markdown_link_targets,
        model_selection_options, normalize_markdown_path, parse_conversation_control,
        parse_markdown_to_rich_document, render_chat_message,
        render_markdown_chunks_to_telegram_entities, render_progress_panel,
        render_rich_document_to_telegram_entities, safe_relative_path, telegram_text_len,
        ChatAuthorization, SecurityState, TelegramChannel, TelegramChat, TelegramMessage,
        TelegramMessageEntity, TelegramProgressPanel, TelegramProgressStatus, TelegramRenderedText,
        TelegramUser,
    };
    use crate::channels::types::{
        ConversationControl, OutgoingMessageAppended, OutgoingOption, OutgoingOptions,
    };
    use crate::config::SandboxMode;
    use reqwest::StatusCode;
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::Mutex,
        time::{Duration, Instant},
    };
    use stellaclaw_core::session_actor::{
        ChatMessage, ChatMessageItem, ChatMessagePart, ChatRole, ContextItem, ToolCallItem,
        ToolResultContent, ToolResultItem,
    };

    fn appended(message: ChatMessage) -> OutgoingMessageAppended {
        OutgoingMessageAppended {
            channel_id: "telegram-main".to_string(),
            platform_chat_id: "42".to_string(),
            conversation_id: "conversation".to_string(),
            session_id: "main".to_string(),
            index: 0,
            turn_id: message.turn_id.clone(),
            step_index: message.step_index,
            message_part: message.message_part.clone(),
            message,
        }
    }

    #[test]
    fn parses_model_control_commands() {
        assert!(matches!(
            parse_conversation_control("/model"),
            Some(ConversationControl::ShowModel)
        ));
        assert!(matches!(
            parse_conversation_control("/model gpt54"),
            Some(ConversationControl::SwitchModel { model_name }) if model_name == "gpt54"
        ));
        assert!(matches!(
            parse_conversation_control("/model@stellaclaw_bot gpt54"),
            Some(ConversationControl::SwitchModel { model_name }) if model_name == "gpt54"
        ));
        assert!(matches!(
            parse_conversation_control("/remote"),
            Some(ConversationControl::ShowRemote)
        ));
        assert!(matches!(
            parse_conversation_control("/remote demo-host ~/repo"),
            Some(ConversationControl::SetRemote { host, path }) if host == "demo-host" && path == "~/repo"
        ));
        assert!(matches!(
            parse_conversation_control("/remote off"),
            Some(ConversationControl::DisableRemote)
        ));
        assert!(matches!(
            parse_conversation_control("/status"),
            Some(ConversationControl::ShowStatus)
        ));
        assert!(matches!(
            parse_conversation_control("/compact"),
            Some(ConversationControl::Compact)
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning"),
            Some(ConversationControl::ShowReasoning)
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning high"),
            Some(ConversationControl::SetReasoning { effort: Some(effort) }) if effort == "high"
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning default"),
            Some(ConversationControl::SetReasoning { effort: None })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox"),
            Some(ConversationControl::ShowSandbox)
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox bubblewrap"),
            Some(ConversationControl::SetSandbox {
                mode: Some(SandboxMode::Bubblewrap)
            })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox subprocess"),
            Some(ConversationControl::SetSandbox {
                mode: Some(SandboxMode::Subprocess)
            })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox default"),
            Some(ConversationControl::SetSandbox { mode: None })
        ));
    }

    #[test]
    fn first_private_user_bootstraps_as_admin_in_security_state() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            client: reqwest::blocking::Client::new(),
            workdir: PathBuf::from("."),
            conversation_runtime: None,
            progress_panels: Mutex::new(BTreeMap::new()),
            model_aliases: Vec::new(),
            logger: None,
            security_path: PathBuf::from("/tmp/unused-security.json"),
            security: Mutex::new(SecurityState {
                admin_user_ids: Vec::new(),
                chats: BTreeMap::<String, ChatAuthorization>::new(),
            }),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
            chat: TelegramChat {
                id: 42,
                chat_type: "private".to_string(),
                title: None,
            },
            from: Some(TelegramUser {
                id: 42,
                first_name: "Alice".to_string(),
                last_name: None,
                username: Some("alice".to_string()),
            }),
            text: Some("/start".to_string()),
            caption: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            animation: None,
        };

        let bootstrapped = channel
            .bootstrap_first_private_admin_in_memory(&message, 42)
            .expect("bootstrap should work");

        assert!(bootstrapped);
        assert_eq!(channel.effective_admin_user_ids().unwrap(), vec![42]);
        assert!(channel.is_admin_private_chat(&message, 42));
    }

    #[test]
    fn telegram_only_displays_final_assistant_messages() {
        let final_message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        )
        .with_turn_metadata("turn_1", 1, ChatMessagePart::FinalResponse);
        let preamble_message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Context(ContextItem {
                    text: "I will inspect the project first.".to_string(),
                }),
                ChatMessageItem::ToolCall(ToolCallItem {
                    item_id: None,
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell_exec".to_string(),
                    arguments: ContextItem {
                        text: "{\"command\":\"rg foo\"}".to_string(),
                    },
                }),
            ],
        )
        .with_turn_metadata("turn_1", 0, ChatMessagePart::ModelResponse);

        assert!(is_visible_telegram_assistant_message(&appended(
            final_message
        )));
        assert!(!is_visible_telegram_assistant_message(&appended(
            preamble_message
        )));
    }

    #[test]
    fn telegram_render_omits_internal_tool_items() {
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Context(ContextItem {
                    text: "final text".to_string(),
                }),
                ChatMessageItem::ToolCall(ToolCallItem {
                    item_id: None,
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell_exec".to_string(),
                    arguments: ContextItem {
                        text: "{\"command\":\"pwd\"}".to_string(),
                    },
                }),
                ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell_exec".to_string(),
                    result: ToolResultContent {
                        structured: Some(serde_json::json!({
                            "kind": "text_result",
                            "text": "/tmp/work",
                        })),
                        files: Vec::new(),
                    },
                }),
            ],
        );

        assert_eq!(render_chat_message(&message), "final text");
    }

    #[test]
    fn telegram_extracts_local_markdown_attachment_targets() {
        let targets = markdown_link_targets(
            "see [report](./reports/final%20report.pdf) and ![plot](images/chart.png?raw=1)",
        );

        assert_eq!(
            targets,
            vec!["./reports/final%20report.pdf", "images/chart.png?raw=1"]
        );
        assert_eq!(
            normalize_markdown_path(&targets[0]),
            "reports/final report.pdf"
        );
        assert_eq!(normalize_markdown_path(&targets[1]), "images/chart.png");
        assert!(safe_relative_path(&normalize_markdown_path(&targets[0])).is_some());
        assert!(safe_relative_path("../secret.txt").is_none());
    }

    #[test]
    fn telegram_reads_absolute_local_attachment_paths() {
        let root =
            std::env::temp_dir().join(format!("telegram-local-attachment-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp dir should be created");
        let path = root.join("deck.pptx");
        fs::write(&path, b"pptx bytes").expect("temp file should be written");

        let attachment =
            local_attachment_from_path(&path, None, TelegramChannel::MAX_OUTGOING_FILE_BYTES)
                .expect("absolute local file should be readable");

        assert_eq!(attachment.name, "deck.pptx");
        assert_eq!(attachment.bytes, b"pptx bytes");
        assert_eq!(
            local_path_from_file_item_uri(path.to_str().unwrap()),
            Some(path)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn telegram_progress_panel_renders_status_elapsed_and_plan() {
        let panel = TelegramProgressPanel {
            message_id: 1,
            started_at: Instant::now() - Duration::from_secs(75),
            last_edit_at: Instant::now(),
            status: TelegramProgressStatus::Running,
            activity: "正在准备工具: shell_exec".to_string(),
            plan: Some(serde_json::json!({
                "explanation": "先确认当前状态。",
                "plan": [
                    {"step": "Inspect event flow", "status": "completed"},
                    {"step": "Restore Telegram progress panel", "status": "in_progress"},
                    {"step": "Run focused tests", "status": "pending"}
                ]
            })),
            last_rendered: String::new(),
        };

        let rendered = render_progress_panel(&panel);

        assert!(rendered.contains("**Stellaclaw 正在处理**"));
        assert!(rendered.contains("耗时: 1m"));
        assert!(rendered.contains("当前: 正在准备工具: shell_exec"));
        assert!(rendered.contains("[x] Inspect event flow"));
        assert!(rendered.contains("[>] Restore Telegram progress panel"));
        assert!(rendered.contains("[ ] Run focused tests"));
    }

    #[test]
    fn telegram_delivery_retry_policy_retries_only_transient_failures() {
        assert!(is_retryable_telegram_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_telegram_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_telegram_status(StatusCode::BAD_REQUEST));
        assert!(is_retryable_telegram_error_code(Some(429)));
        assert!(is_retryable_telegram_error_code(Some(503)));
        assert!(!is_retryable_telegram_error_code(Some(400)));

        assert_eq!(delivery_retry_delay(1, None), Duration::from_secs(2));
        assert_eq!(delivery_retry_delay(3, None), Duration::from_secs(8));
        assert_eq!(delivery_retry_delay(20, None), Duration::from_secs(60));
        assert_eq!(delivery_retry_delay(1, Some(7)), Duration::from_secs(7));
    }

    #[test]
    fn telegram_model_selection_options_use_model_callback_commands() {
        let options = model_selection_options(&["fast".to_string(), "deep".to_string()]);

        assert_eq!(options.options.len(), 2);
        assert_eq!(options.options[0].label, "fast");
        assert_eq!(options.options[0].value, "/model fast");
        assert_eq!(options.options[1].label, "deep");
        assert_eq!(options.options[1].value, "/model deep");
    }

    #[test]
    fn renders_basic_entities_for_telegram() {
        let document = parse_markdown_to_rich_document(
            "# Title\n\n**bold** and *italic* with [link](https://example.com).\n\n```rust\nlet x = 1;\n```",
        );
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("Title"));
        assert!(rendered.text.contains("bold"));
        assert!(rendered.text.contains("italic"));
        assert!(rendered.text.contains("let x = 1;"));
        assert!(rendered.entities.iter().any(|entity| entity.kind == "bold"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "italic"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "text_link"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "pre" && entity.language.as_deref() == Some("rust")));
    }

    #[test]
    fn renders_local_markdown_links_as_file_placeholders_for_telegram() {
        let document = parse_markdown_to_rich_document(
            "see [文件1](paper-library/data/papers.json) and [paper-library/data/papers.json](paper-library/data/papers.json)",
        );
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("📎 文件1"));
        assert!(rendered.text.contains("📎 papers.json"));
        assert!(!rendered.entities.iter().any(|entity| {
            entity.kind == "text_link"
                && entity
                    .url
                    .as_deref()
                    .is_some_and(|url| url.contains("paper-library/data/papers.json"))
        }));
    }

    #[test]
    fn renders_blockquote_entities_for_telegram() {
        let document = parse_markdown_to_rich_document("> quoted line\n>\n> second line");
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("quoted line"));
        assert!(rendered.text.contains("second line"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "blockquote"));
    }

    #[test]
    fn build_send_text_payload_includes_entities_and_inline_keyboard() {
        let payload = build_send_text_payload(
            "123",
            TelegramRenderedText {
                text: "hello".to_string(),
                entities: vec![TelegramMessageEntity {
                    kind: "bold".to_string(),
                    offset: 0,
                    length: 5,
                    url: None,
                    language: None,
                }],
            },
            Some(&OutgoingOptions {
                options: vec![
                    OutgoingOption {
                        label: "One".to_string(),
                        value: "/one".to_string(),
                    },
                    OutgoingOption {
                        label: "Two".to_string(),
                        value: "/two".to_string(),
                    },
                    OutgoingOption {
                        label: "Three".to_string(),
                        value: "/three".to_string(),
                    },
                ],
            }),
        )
        .unwrap();

        assert_eq!(payload["chat_id"], "123");
        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["entities"][0]["type"], "bold");
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][1]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["text"],
            "One"
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "/one"
        );
    }

    #[test]
    fn splits_long_markdown_messages_into_multiple_chunks() {
        let input = format!(
            "{}\n\n{}\n\n{}",
            "a".repeat(2200),
            "b".repeat(2200),
            "c".repeat(2200)
        );
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 4096);
        assert!(chunks.len() >= 2);
        assert!(chunks
            .iter()
            .all(|chunk| telegram_text_len(&chunk.text) <= 4096));
    }

    #[test]
    fn splits_large_code_block_into_multiple_pre_blocks() {
        let input = format!("```rust\n{}\n```", "let x = 42;\n".repeat(300));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(chunks
            .iter()
            .all(|chunk| telegram_text_len(&chunk.text) <= 1024));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.entities.iter().any(|entity| entity.kind == "pre")));
    }
}
