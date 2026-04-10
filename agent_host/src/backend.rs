use agent_frame::compaction::ContextCompactionReport;
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::{
    Tool, compact_session_messages_with_report as frame_compact_session_messages_with_report,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::zgent::subagent::ZgentSubagentModel;
use crate::zgent::zgent_runtime_available;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    #[default]
    AgentFrame,
    Zgent,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendExecutionOptions {
    #[serde(default)]
    pub zgent_allowed_subagent_models: Vec<ZgentSubagentModel>,
}

pub fn backend_supports_native_multimodal_input(kind: AgentBackendKind) -> bool {
    matches!(kind, AgentBackendKind::AgentFrame)
}

pub fn compact_session_messages_with_report(
    backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    match backend {
        AgentBackendKind::AgentFrame => {
            frame_compact_session_messages_with_report(previous_messages, config, extra_tools)
        }
        AgentBackendKind::Zgent => {
            if !zgent_runtime_available() {
                return Err(anyhow!(
                    "the zgent backend is unavailable because the local ./zgent runtime directory is unavailable"
                ));
            }
            crate::zgent::compaction::compact_session_messages_with_report(
                previous_messages,
                &config,
                &extra_tools,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentBackendKind, backend_supports_native_multimodal_input};

    #[test]
    fn only_agent_frame_backend_supports_native_multimodal_input() {
        assert!(backend_supports_native_multimodal_input(
            AgentBackendKind::AgentFrame
        ));
        assert!(!backend_supports_native_multimodal_input(
            AgentBackendKind::Zgent
        ));
    }
}
