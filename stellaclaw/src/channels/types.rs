use serde::Serialize;
use serde_json::Value;
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessagePart, FileItem, SelectionReferenceItem,
};

use crate::config::SandboxMode;

#[derive(Debug, Clone)]
pub struct IncomingConversationMessage {
    pub remote_message_id: String,
    pub user_name: Option<String>,
    pub message_time: Option<String>,
    pub text: Option<String>,
    pub selection_references: Vec<SelectionReferenceItem>,
    pub files: Vec<FileItem>,
    pub control: Option<ConversationControl>,
}

#[derive(Debug, Clone)]
pub enum ConversationControl {
    Continue,
    Cancel,
    Compact,
    ShowStatus,
    ShowModel,
    SwitchModel { model_name: String },
    ShowReasoning,
    SetReasoning { effort: Option<String> },
    InvalidReasoning { reason: String },
    ShowRemote,
    SetRemote { host: String, path: String },
    DisableRemote,
    InvalidRemote { reason: String },
    ShowSandbox,
    SetSandbox { mode: Option<SandboxMode> },
    InvalidSandbox { reason: String },
}

pub(crate) fn parse_conversation_control(text: &str) -> Option<ConversationControl> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let argument = parts.next().map(str::trim).unwrap_or("");
    let command = first.split_once('@').map_or(first, |(base, _)| base);

    match command {
        "/continue" if argument.is_empty() => Some(ConversationControl::Continue),
        "/cancel" if argument.is_empty() => Some(ConversationControl::Cancel),
        "/compact" if argument.is_empty() => Some(ConversationControl::Compact),
        "/status" if argument.is_empty() => Some(ConversationControl::ShowStatus),
        "/model" if argument.is_empty() => Some(ConversationControl::ShowModel),
        "/model" => Some(ConversationControl::SwitchModel {
            model_name: argument.to_string(),
        }),
        "/reasoning" => Some(parse_reasoning_control_argument(argument)),
        "/remote" if argument.is_empty() => Some(ConversationControl::ShowRemote),
        "/remote" if argument.eq_ignore_ascii_case("off") => {
            Some(ConversationControl::DisableRemote)
        }
        "/remote" => parse_remote_control(argument),
        "/sandbox" if argument.is_empty() => Some(ConversationControl::ShowSandbox),
        "/sandbox" => parse_sandbox_control(argument),
        _ => None,
    }
}

pub(crate) fn parse_reasoning_control_argument(argument: &str) -> ConversationControl {
    let argument = argument.trim();
    if argument.is_empty() {
        return ConversationControl::ShowReasoning;
    }
    match argument.to_ascii_lowercase().as_str() {
        "default" | "model" | "model_default" | "model-default" | "global" => {
            ConversationControl::SetReasoning { effort: None }
        }
        "minimal" | "low" | "medium" | "high" | "xhigh" => ConversationControl::SetReasoning {
            effort: Some(argument.to_ascii_lowercase()),
        },
        _ => ConversationControl::InvalidReasoning {
            reason: format!("未知 reasoning effort `{argument}`。"),
        },
    }
}

fn parse_remote_control(argument: &str) -> Option<ConversationControl> {
    let mut parts = argument.trim().splitn(2, char::is_whitespace);
    let host = parts.next().unwrap_or_default().trim();
    let path = parts.next().map(str::trim).unwrap_or_default();
    if host.is_empty() || path.is_empty() {
        return Some(ConversationControl::InvalidRemote {
            reason: "remote 命令缺少 host 或 path。".to_string(),
        });
    }
    Some(ConversationControl::SetRemote {
        host: host.to_string(),
        path: path.to_string(),
    })
}

fn parse_sandbox_control(argument: &str) -> Option<ConversationControl> {
    let argument = argument.trim();
    if argument.eq_ignore_ascii_case("default") || argument.eq_ignore_ascii_case("global") {
        return Some(ConversationControl::SetSandbox { mode: None });
    }
    if argument.eq_ignore_ascii_case("subprocess")
        || argument.eq_ignore_ascii_case("off")
        || argument.eq_ignore_ascii_case("none")
        || argument.eq_ignore_ascii_case("disabled")
    {
        return Some(ConversationControl::SetSandbox {
            mode: Some(crate::config::SandboxMode::Subprocess),
        });
    }
    if argument.eq_ignore_ascii_case("bubblewrap") || argument.eq_ignore_ascii_case("bwrap") {
        return Some(ConversationControl::SetSandbox {
            mode: Some(crate::config::SandboxMode::Bubblewrap),
        });
    }
    Some(ConversationControl::InvalidSandbox {
        reason: format!("未知 sandbox 模式 `{argument}`。"),
    })
}

#[derive(Debug, Clone)]
pub enum IncomingDispatch {
    Message(IncomingMessageDispatch),
}

#[derive(Debug, Clone)]
pub struct IncomingMessageDispatch {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub message: IncomingConversationMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingAttachmentKind {
    Image,
    Audio,
    Voice,
    Video,
    Animation,
    Document,
}

#[derive(Debug, Clone)]
pub struct OutgoingOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct OutgoingOptions {
    pub options: Vec<OutgoingOption>,
}

#[derive(Debug, Clone)]
pub struct OutgoingMessageAppended {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub index: usize,
    pub turn_id: Option<String>,
    pub step_index: Option<usize>,
    pub message_part: Option<ChatMessagePart>,
    pub message: ChatMessage,
}

#[derive(Debug, Clone)]
pub struct OutgoingSessionStream {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub event: Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingErrorScope {
    Turn,
    Runtime,
    Control,
    Configuration,
    RemoteWorkspace,
    Sandbox,
    Attachment,
    Delivery,
    BackgroundSession,
    Subagent,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingErrorSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingError {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub scope: OutgoingErrorScope,
    pub severity: OutgoingErrorSeverity,
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
    pub can_continue: bool,
    pub suggested_action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingState {
    Idle,
    Typing,
}

#[derive(Debug, Clone)]
pub struct OutgoingProcessing {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub state: ProcessingState,
}

#[derive(Debug, Clone)]
pub struct OutgoingHomeEvent {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum ChannelEvent {
    Home(OutgoingHomeEvent),
    MessageAppended(OutgoingMessageAppended),
    SessionStream(OutgoingSessionStream),
    Processing(OutgoingProcessing),
    Error(OutgoingError),
}

impl ChannelEvent {
    pub fn channel_id(&self) -> &str {
        match self {
            ChannelEvent::Home(home) => &home.channel_id,
            ChannelEvent::MessageAppended(appended) => &appended.channel_id,
            ChannelEvent::SessionStream(stream) => &stream.channel_id,
            ChannelEvent::Processing(processing) => &processing.channel_id,
            ChannelEvent::Error(error) => &error.channel_id,
        }
    }

    pub fn platform_chat_id(&self) -> &str {
        match self {
            ChannelEvent::Home(home) => &home.platform_chat_id,
            ChannelEvent::MessageAppended(appended) => &appended.platform_chat_id,
            ChannelEvent::SessionStream(stream) => &stream.platform_chat_id,
            ChannelEvent::Processing(processing) => &processing.platform_chat_id,
            ChannelEvent::Error(error) => &error.platform_chat_id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OutgoingDispatch {
    Event(ChannelEvent),
}
