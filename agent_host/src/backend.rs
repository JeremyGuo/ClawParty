use agent_frame::compaction::ContextCompactionReport;
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::{
    SessionExecutionControl, SessionRunReport, Tool,
    compact_session_messages_with_report as frame_compact_session_messages_with_report,
    run_session_with_report_controlled as frame_run_session_with_report_controlled,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    #[default]
    AgentFrame,
}

pub fn backend_supports_native_multimodal_input(_kind: AgentBackendKind) -> bool {
    true
}

pub fn run_session_with_report_controlled(
    _backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    frame_run_session_with_report_controlled(
        previous_messages,
        prompt,
        config,
        extra_tools,
        control,
    )
}

pub fn compact_session_messages_with_report(
    _backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    frame_compact_session_messages_with_report(previous_messages, config, extra_tools)
}
