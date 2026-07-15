use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::Sender;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatMessagePart, ChatRole, FileItem, FileState,
};

use crate::{
    config::FeishuChannelConfig, conversation_id_manager::ConversationIdManager,
    logger::StellaclawLogger,
};

use super::{
    types::{
        parse_conversation_control, IncomingConversationMessage, IncomingDispatch,
        IncomingMessageDispatch, OutgoingError, OutgoingMessageAppended, ProcessingState,
    },
    Channel,
};

pub struct FeishuChannel {
    id: String,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<Option<ChildStdout>>,
    child: Mutex<Option<Child>>,
    workdir: PathBuf,
    allowed_chat_ids: BTreeSet<String>,
    allowed_user_ids: BTreeSet<String>,
    last_remote_message_by_chat: Mutex<BTreeMap<String, String>>,
}

impl FeishuChannel {
    pub fn new(
        config: &FeishuChannelConfig,
        workdir: impl AsRef<Path>,
        logger: Arc<StellaclawLogger>,
    ) -> Result<Self> {
        let bridge_script = resolve_bridge_script(config)?;
        let mut command = Command::new(&config.bridge_command);
        command
            .arg(&bridge_script)
            .env(
                "STELLACLAW_FEISHU_APP_ID",
                config.resolve_app_id().map_err(anyhow::Error::msg)?,
            )
            .env(
                "STELLACLAW_FEISHU_APP_SECRET",
                config.resolve_app_secret().map_err(anyhow::Error::msg)?,
            )
            .env("STELLACLAW_FEISHU_DOMAIN", &config.domain)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(value) = config.resolve_encrypt_key() {
            command.env("STELLACLAW_FEISHU_ENCRYPT_KEY", value);
        }
        if let Some(value) = config.resolve_verification_token() {
            command.env("STELLACLAW_FEISHU_VERIFICATION_TOKEN", value);
        }

        let mut child = command.spawn().with_context(|| {
            format!("failed to start feishu bridge {}", bridge_script.display())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("feishu bridge stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("feishu bridge stdout unavailable"))?;
        if let Some(stderr) = child.stderr.take() {
            let channel_id = config.id.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    logger.warn(
                        "feishu_bridge_stderr",
                        json!({"channel_id": channel_id, "line": line}),
                    );
                }
            });
        }

        Ok(Self {
            id: config.id.clone(),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(Some(stdout)),
            child: Mutex::new(Some(child)),
            workdir: workdir.as_ref().to_path_buf(),
            allowed_chat_ids: config.allowed_chat_ids.iter().cloned().collect(),
            allowed_user_ids: config.allowed_user_ids.iter().cloned().collect(),
            last_remote_message_by_chat: Mutex::new(BTreeMap::new()),
        })
    }

    fn send_command(&self, command: &FeishuBridgeCommand) -> Result<()> {
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| anyhow!("feishu bridge stdin lock poisoned"))?;
        serde_json::to_writer(&mut *stdin, command).context("failed to encode feishu command")?;
        stdin
            .write_all(b"\n")
            .context("failed to write feishu command")?;
        stdin.flush().context("failed to flush feishu command")?;
        Ok(())
    }

    fn handle_bridge_line(
        &self,
        line: &str,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
        logger: &Arc<StellaclawLogger>,
    ) -> Result<()> {
        let event: FeishuBridgeEvent =
            serde_json::from_str(line).context("failed to decode feishu bridge event")?;
        match event {
            FeishuBridgeEvent::Ready {
                bot_open_id,
                bot_name,
            } => {
                logger.info(
                    "feishu_bridge_ready",
                    json!({
                        "channel_id": self.id,
                        "bot_open_id": bot_open_id,
                        "bot_name": bot_name,
                    }),
                );
                Ok(())
            }
            FeishuBridgeEvent::Log {
                level,
                message,
                detail,
            } => {
                let payload = json!({
                    "channel_id": self.id,
                    "message": message,
                    "detail": detail,
                });
                match level.as_deref() {
                    Some("error") => logger.error("feishu_bridge_log", payload),
                    Some("warn") | Some("warning") => logger.warn("feishu_bridge_log", payload),
                    _ => logger.info("feishu_bridge_log", payload),
                }
                Ok(())
            }
            FeishuBridgeEvent::Error { message, detail } => {
                logger.error(
                    "feishu_bridge_error",
                    json!({"channel_id": self.id, "message": message, "detail": detail}),
                );
                Ok(())
            }
            FeishuBridgeEvent::Delivery {
                chat_id,
                message_id,
            } => {
                if let Some(message_id) = message_id {
                    self.last_remote_message_by_chat
                        .lock()
                        .map_err(|_| anyhow!("feishu delivery lock poisoned"))?
                        .insert(chat_id, message_id);
                }
                Ok(())
            }
            FeishuBridgeEvent::Message(message) => {
                self.handle_message(message, dispatch_tx, id_manager)
            }
        }
    }

    fn handle_message(
        &self,
        message: FeishuBridgeMessage,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
    ) -> Result<()> {
        if !self.allowed_chat_ids.is_empty() && !self.allowed_chat_ids.contains(&message.chat_id) {
            return Ok(());
        }
        if !self.allowed_user_ids.is_empty() && !message.sender.matches_any(&self.allowed_user_ids)
        {
            return Ok(());
        }

        self.last_remote_message_by_chat
            .lock()
            .map_err(|_| anyhow!("feishu delivery lock poisoned"))?
            .insert(message.chat_id.clone(), message.message_id.clone());

        let conversation_id = id_manager
            .lock()
            .map_err(|_| anyhow!("conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &message.chat_id)
            .map_err(anyhow::Error::msg)?;
        let files = self.collect_incoming_files(&conversation_id, &message)?;
        let control = parse_conversation_control(&message.text);
        if control.is_none() && message.text.trim().is_empty() && files.is_empty() {
            return Ok(());
        }
        dispatch_tx
            .send(IncomingDispatch::Message(IncomingMessageDispatch {
                channel_id: self.id.clone(),
                platform_chat_id: message.chat_id,
                conversation_id,
                message: IncomingConversationMessage {
                    remote_message_id: message.message_id,
                    user_name: message.sender.name.or(message.sender.open_id),
                    message_time: message.create_time,
                    text: if control.is_some() {
                        None
                    } else {
                        Some(message.text)
                    },
                    selection_references: Vec::new(),
                    files,
                    control,
                },
            }))
            .map_err(|_| anyhow!("dispatcher channel closed"))
    }

    fn collect_incoming_files(
        &self,
        conversation_id: &str,
        message: &FeishuBridgeMessage,
    ) -> Result<Vec<FileItem>> {
        if message.attachments.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self
            .workdir
            .join("conversations")
            .join(conversation_id)
            .join(".stellaclaw")
            .join("attachments")
            .join("incoming")
            .join(safe_path_component(&message.message_id));
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

        Ok(message
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| self.materialize_attachment(&dir, index, attachment))
            .collect())
    }

    fn materialize_attachment(
        &self,
        dir: &Path,
        index: usize,
        attachment: &FeishuBridgeAttachment,
    ) -> FileItem {
        let file_name = attachment_file_name(index, attachment);
        let target = dir.join(&file_name);
        let uri = format!("file://{}", target.display());
        let media_type = attachment
            .media_type
            .clone()
            .or_else(|| infer_media_type(&target));

        if let Some(error) = attachment.error.as_ref() {
            return FileItem {
                uri,
                name: Some(file_name),
                media_type,
                width: None,
                height: None,
                state: Some(FileState::Crashed {
                    reason: error.clone(),
                }),
            };
        }

        let result = (|| -> Result<(Option<u32>, Option<u32>)> {
            let encoded = attachment
                .data_base64
                .as_deref()
                .context("feishu attachment had no data_base64")?;
            let bytes = general_purpose::STANDARD
                .decode(encoded)
                .context("failed to decode feishu attachment base64")?;
            fs::write(&target, &bytes)
                .with_context(|| format!("failed to write attachment {}", target.display()))?;
            let dimensions = image_dimensions(media_type.as_deref(), &bytes);
            Ok(match dimensions {
                Some((width, height)) => (Some(width), Some(height)),
                None => (None, None),
            })
        })();

        match result {
            Ok((width, height)) => FileItem {
                uri,
                name: Some(file_name),
                media_type,
                width,
                height,
                state: None,
            },
            Err(error) => FileItem {
                uri,
                name: Some(file_name),
                media_type,
                width: None,
                height: None,
                state: Some(FileState::Crashed {
                    reason: format!("{error:#}"),
                }),
            },
        }
    }
}

impl Drop for FeishuChannel {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Channel for FeishuChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        self.send_command(&FeishuBridgeCommand::SetTyping {
            chat_id: platform_chat_id.to_string(),
            typing: state == ProcessingState::Typing,
        })
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
        self.send_command(&FeishuBridgeCommand::SendText {
            chat_id: error.platform_chat_id.clone(),
            text,
        })
    }

    fn message_appended(&self, appended: &OutgoingMessageAppended) -> Result<()> {
        if !is_visible_feishu_assistant_message(appended) {
            return Ok(());
        }
        let text = render_chat_message(&appended.message);
        if text.trim().is_empty() {
            return Ok(());
        }
        self.send_command(&FeishuBridgeCommand::SendText {
            chat_id: appended.platform_chat_id.clone(),
            text,
        })
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
            let stdout = match self.stdout.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => None,
            };
            let Some(stdout) = stdout else {
                logger.error(
                    "feishu_channel_failed",
                    json!({"channel_id": self.id, "error": "bridge stdout unavailable"}),
                );
                return;
            };
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        if let Err(error) =
                            self.handle_bridge_line(&line, &dispatch_tx, &id_manager, &logger)
                        {
                            logger.warn(
                                "feishu_bridge_event_failed",
                                json!({
                                    "channel_id": self.id,
                                    "line": line,
                                    "error": format!("{error:#}"),
                                }),
                            );
                        }
                    }
                    Err(error) => {
                        logger.error(
                            "feishu_channel_failed",
                            json!({"channel_id": self.id, "error": error.to_string()}),
                        );
                        break;
                    }
                }
            }
        });
    }
}

fn resolve_bridge_script(config: &FeishuChannelConfig) -> Result<PathBuf> {
    match config.bridge_script.as_deref() {
        Some(path) if !path.trim().is_empty() => Ok(PathBuf::from(path)),
        _ => Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("sidecars")
            .join("feishu-bridge")
            .join("bridge.mjs")),
    }
}

fn render_chat_message(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => parts.push(context.text.clone()),
            ChatMessageItem::File(file) => parts.push(render_file_item(file)),
            ChatMessageItem::Compaction(_)
            | ChatMessageItem::SelectionReference(_)
            | ChatMessageItem::Reasoning(_)
            | ChatMessageItem::ToolCall(_)
            | ChatMessageItem::ToolResult(_) => {}
        }
    }
    parts.join("\n\n")
}

fn render_file_item(file: &FileItem) -> String {
    match &file.name {
        Some(name) => format!("[file] {name} ({})", file.uri),
        None => format!("[file] {}", file.uri),
    }
}

fn is_visible_feishu_assistant_message(appended: &OutgoingMessageAppended) -> bool {
    if appended.message.role != ChatRole::Assistant {
        return false;
    }
    let message_part = appended
        .message_part
        .as_ref()
        .or(appended.message.message_part.as_ref());
    message_part == Some(&ChatMessagePart::FinalResponse)
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FeishuBridgeCommand {
    SendText { chat_id: String, text: String },
    SetTyping { chat_id: String, typing: bool },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FeishuBridgeEvent {
    Ready {
        #[serde(default)]
        bot_open_id: Option<String>,
        #[serde(default)]
        bot_name: Option<String>,
    },
    Message(FeishuBridgeMessage),
    Delivery {
        chat_id: String,
        #[serde(default)]
        message_id: Option<String>,
    },
    Log {
        #[serde(default)]
        level: Option<String>,
        message: String,
        #[serde(default)]
        detail: Option<Value>,
    },
    Error {
        message: String,
        #[serde(default)]
        detail: Option<Value>,
    },
}

#[derive(Debug, Deserialize)]
struct FeishuBridgeMessage {
    message_id: String,
    chat_id: String,
    #[serde(default)]
    create_time: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    sender: FeishuBridgeSender,
    #[serde(default)]
    attachments: Vec<FeishuBridgeAttachment>,
}

#[derive(Debug, Default, Deserialize)]
struct FeishuBridgeAttachment {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    file_key: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    data_base64: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FeishuBridgeSender {
    #[serde(default)]
    open_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    union_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl FeishuBridgeSender {
    fn matches_any(&self, allowed: &BTreeSet<String>) -> bool {
        [
            self.open_id.as_deref(),
            self.user_id.as_deref(),
            self.union_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| allowed.contains(id))
    }
}

fn attachment_file_name(index: usize, attachment: &FeishuBridgeAttachment) -> String {
    if let Some(name) = attachment
        .name
        .as_deref()
        .map(safe_file_name)
        .filter(|name| !name.is_empty())
    {
        return name;
    }

    let kind = attachment
        .kind
        .as_deref()
        .map(safe_path_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "file".to_string());
    let key = attachment
        .file_key
        .as_deref()
        .map(safe_path_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| index.to_string());
    let extension = attachment
        .media_type
        .as_deref()
        .and_then(extension_for_media_type)
        .unwrap_or("bin");
    format!("{kind}-{key}.{extension}")
}

fn safe_file_name(value: &str) -> String {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value);
    safe_path_component(name)
}

fn safe_path_component(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    output.trim_matches('.').to_string()
}

fn image_dimensions(media_type: Option<&str>, bytes: &[u8]) -> Option<(u32, u32)> {
    let media_type = media_type?;
    if !media_type.starts_with("image/") {
        return None;
    }
    image::load_from_memory(bytes)
        .ok()
        .map(|image| image.dimensions())
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
        "txt" => Some("text/plain".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "ogg" => Some("audio/ogg".to_string()),
        "wav" => Some("audio/wav".to_string()),
        "mp4" => Some("video/mp4".to_string()),
        _ => None,
    }
}

fn extension_for_media_type(media_type: &str) -> Option<&'static str> {
    match media_type
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "audio/mpeg" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        "audio/wav" => Some("wav"),
        "video/mp4" => Some("mp4"),
        _ => None,
    }
}
