use crate::channel::{AttachmentSource, Channel, IncomingMessage, PendingAttachment};
use crate::config::{BotCommandConfig, TelegramChannelConfig};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub struct TelegramChannel {
    id: String,
    bot_token: String,
    api_base_url: String,
    poll_timeout_seconds: u64,
    poll_interval_ms: u64,
    commands: Vec<BotCommandConfig>,
    client: Client,
}

impl TelegramChannel {
    pub fn from_config(config: TelegramChannelConfig) -> Result<Self> {
        let bot_token = match config.bot_token {
            Some(token) if !token.trim().is_empty() => token,
            _ => std::env::var(&config.bot_token_env).with_context(|| {
                format!(
                    "telegram channel {} requires bot_token or env {}",
                    config.id, config.bot_token_env
                )
            })?,
        };

        Ok(Self {
            id: config.id,
            bot_token,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            poll_timeout_seconds: config.poll_timeout_seconds,
            poll_interval_ms: config.poll_interval_ms,
            commands: config.commands,
            client: Client::new(),
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.bot_token, method)
    }

    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<T> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("telegram API call {} failed", method))?;
        let envelope: TelegramEnvelope<T> = response
            .json()
            .await
            .with_context(|| format!("telegram API {} returned invalid JSON", method))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                envelope
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {} returned no result", method))
    }

    async fn call_multipart(&self, method: &str, form: Form) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(self.method_url(method))
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("telegram multipart API call {} failed", method))?;
        let envelope: TelegramEnvelope<serde_json::Value> = response
            .json()
            .await
            .with_context(|| format!("telegram API {} returned invalid JSON", method))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                envelope
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {} returned no result", method))
    }

    async fn set_my_commands(&self) -> Result<()> {
        let commands = self
            .commands
            .iter()
            .map(|command| {
                json!({
                    "command": command.command,
                    "description": command.description,
                })
            })
            .collect::<Vec<_>>();
        self.call_api::<bool>(
            "setMyCommands",
            json!({
                "commands": commands,
            }),
        )
        .await?;
        Ok(())
    }

    fn build_address(&self, message: &TelegramMessage) -> ChannelAddress {
        let display_name = message.from.as_ref().map(|user| {
            let mut pieces = Vec::new();
            if !user.first_name.trim().is_empty() {
                pieces.push(user.first_name.trim());
            }
            if let Some(last_name) = user.last_name.as_deref() {
                if !last_name.trim().is_empty() {
                    pieces.push(last_name.trim());
                }
            }
            if pieces.is_empty() {
                user.username.clone().unwrap_or_else(|| user.id.to_string())
            } else {
                pieces.join(" ")
            }
        });

        ChannelAddress {
            channel_id: self.id.clone(),
            conversation_id: message.chat.id.to_string(),
            user_id: message.from.as_ref().map(|user| user.id.to_string()),
            display_name,
        }
    }

    fn collect_attachments(&self, message: &TelegramMessage) -> Vec<PendingAttachment> {
        let mut attachments = Vec::new();

        if let Some(photo) = message.photo.as_ref().and_then(|sizes| sizes.last()) {
            let file_name = format!("photo_{}.jpg", photo.file_unique_id);
            attachments.push(PendingAttachment::new(
                AttachmentKind::Image,
                Some(file_name),
                Some("image/jpeg".to_string()),
                photo.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    photo.file_id.clone(),
                )),
            ));
        }

        if let Some(document) = message.document.as_ref() {
            attachments.push(PendingAttachment::new(
                if document
                    .mime_type
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("image/")
                {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
                document.file_name.clone(),
                document.mime_type.clone(),
                document.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    document.file_id.clone(),
                )),
            ));
        }

        if let Some(video) = message.video.as_ref() {
            attachments.push(PendingAttachment::new(
                AttachmentKind::File,
                video
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("video_{}.bin", video.file_unique_id))),
                video.mime_type.clone(),
                video.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    video.file_id.clone(),
                )),
            ));
        }

        if let Some(audio) = message.audio.as_ref() {
            attachments.push(PendingAttachment::new(
                AttachmentKind::File,
                audio
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("audio_{}.bin", audio.file_unique_id))),
                audio.mime_type.clone(),
                audio.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    audio.file_id.clone(),
                )),
            ));
        }

        attachments
    }

    async fn send_photo(&self, chat_id: &str, attachment: OutgoingAttachment) -> Result<()> {
        let file_name = attachment
            .path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.bin".to_string());
        let bytes = fs::read(&attachment.path)
            .await
            .with_context(|| format!("failed to read image {}", attachment.path.display()))?;
        let form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", Part::bytes(bytes).file_name(file_name));
        let form = if let Some(caption) = attachment.caption {
            form.text("caption", caption)
        } else {
            form
        };
        self.call_multipart("sendPhoto", form).await?;
        Ok(())
    }

    async fn send_document(&self, chat_id: &str, attachment: OutgoingAttachment) -> Result<()> {
        let file_name = attachment
            .path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment.bin".to_string());
        let bytes = fs::read(&attachment.path)
            .await
            .with_context(|| format!("failed to read attachment {}", attachment.path.display()))?;
        let form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", Part::bytes(bytes).file_name(file_name));
        let form = if let Some(caption) = attachment.caption {
            form.text("caption", caption)
        } else {
            form
        };
        self.call_multipart("sendDocument", form).await?;
        Ok(())
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let mut offset = None::<i64>;
        self.set_my_commands().await?;
        info!(
            log_stream = "channel",
            log_key = %self.id,
            commands_count = self.commands.len() as u64,
            commands = ?self.commands,
            kind = "telegram_commands_registered",
            "telegram commands registered"
        );
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_polling_started",
            poll_timeout_seconds = self.poll_timeout_seconds,
            poll_interval_ms = self.poll_interval_ms,
            "telegram polling loop started"
        );
        loop {
            let payload = json!({
                "timeout": self.poll_timeout_seconds,
                "offset": offset,
                "allowed_updates": ["message"],
            });
            let updates: Vec<TelegramUpdate> = self.call_api("getUpdates", payload).await?;
            for update in updates {
                offset = Some(update.update_id + 1);
                let Some(message) = update.message else {
                    continue;
                };
                let text = message.text.clone().or_else(|| message.caption.clone());
                let attachments = self.collect_attachments(&message);
                let incoming = IncomingMessage {
                    remote_message_id: message.message_id.to_string(),
                    address: self.build_address(&message),
                    text,
                    attachments,
                };
                if sender.send(incoming).await.is_err() {
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_receiver_closed",
                        "telegram receiver closed; stopping polling loop"
                    );
                    return Ok(());
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(self.poll_interval_ms)).await;
        }
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_send",
            conversation_id = %address.conversation_id,
            has_text = message.text.is_some(),
            image_count = message.images.len() as u64,
            attachment_count = message.attachments.len() as u64,
            "sending message to telegram user"
        );
        if let Some(text) = message.text {
            self.call_api::<serde_json::Value>(
                "sendMessage",
                json!({
                    "chat_id": address.conversation_id,
                    "text": text,
                }),
            )
            .await?;
        }

        for image in message.images {
            self.send_photo(&address.conversation_id, image).await?;
        }

        for attachment in message.attachments {
            self.send_document(&address.conversation_id, attachment)
                .await?;
        }

        Ok(())
    }

    async fn set_processing(&self, address: &ChannelAddress, state: ProcessingState) -> Result<()> {
        if state == ProcessingState::Typing {
            info!(
                log_stream = "channel",
                log_key = %self.id,
                kind = "typing",
                conversation_id = %address.conversation_id,
                "telegram channel set to typing"
            );
            self.call_api::<serde_json::Value>(
                "sendChatAction",
                json!({
                    "chat_id": address.conversation_id,
                    "action": "typing",
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn processing_keepalive_interval(&self, state: ProcessingState) -> Option<Duration> {
        if state == ProcessingState::Typing {
            Some(Duration::from_secs(4))
        } else {
            None
        }
    }
}

struct TelegramAttachmentSource {
    client: Client,
    api_base_url: String,
    bot_token: String,
    file_id: String,
}

impl TelegramAttachmentSource {
    fn new(client: Client, api_base_url: String, bot_token: String, file_id: String) -> Self {
        Self {
            client,
            api_base_url,
            bot_token,
            file_id,
        }
    }
}

#[async_trait]
impl AttachmentSource for TelegramAttachmentSource {
    async fn save_to(&self, destination: &Path) -> Result<u64> {
        let response = self
            .client
            .post(format!(
                "{}/bot{}/getFile",
                self.api_base_url, self.bot_token
            ))
            .json(&json!({ "file_id": self.file_id }))
            .send()
            .await
            .context("telegram getFile request failed")?;
        let envelope: TelegramEnvelope<TelegramFile> = response
            .json()
            .await
            .context("telegram getFile response was not valid JSON")?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram getFile failed: {}",
                envelope
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        let file = envelope
            .result
            .ok_or_else(|| anyhow!("telegram getFile returned no file metadata"))?;
        let url = format!(
            "{}/file/bot{}/{}",
            self.api_base_url, self.bot_token, file.file_path
        );
        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .context("telegram file download failed")?
            .bytes()
            .await
            .context("telegram file payload read failed")?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(destination, &bytes).await?;
        Ok(bytes.len() as u64)
    }
}

#[derive(Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<TelegramPhotoSize>>,
    document: Option<TelegramMedia>,
    video: Option<TelegramMedia>,
    audio: Option<TelegramMedia>,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    last_name: Option<String>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_unique_id: String,
    file_size: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramMedia {
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramFile {
    file_path: String,
}
