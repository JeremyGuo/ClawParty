mod background;
mod cron;
mod memory;
mod plan;
mod subagent;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    execute_bridge_tool, ToolBackend, ToolCallContext, ToolConcurrency, ToolDefinition, ToolEntry,
};
use crate::session_actor::{tool_runtime::LocalToolError, ToolResultContent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostToolScope {
    MainForeground,
    MainBackground,
    SubAgent,
}

pub fn host_tool_definitions(
    scope: HostToolScope,
    enable_memory_tools: bool,
) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if matches!(
        scope,
        HostToolScope::MainForeground | HostToolScope::MainBackground
    ) {
        tools.extend(cron::cron_tool_definitions());
        tools.push(background::background_agents_list_tool_definition());
    }

    tools.push(plan::update_plan_tool_definition());
    if enable_memory_tools {
        tools.extend(memory::memory_tool_definitions());
    }
    tools.extend(subagent::subagent_tool_definitions());

    match scope {
        HostToolScope::MainForeground => {
            tools.push(background::background_agent_start_tool_definition());
        }
        HostToolScope::MainBackground => {
            tools.push(background::terminate_tool_definition());
        }
        HostToolScope::SubAgent => {}
    }

    tools
}

pub(crate) fn host_tool_entries(scope: HostToolScope, enable_memory_tools: bool) -> Vec<ToolEntry> {
    let mut entries = Vec::new();

    if matches!(
        scope,
        HostToolScope::MainForeground | HostToolScope::MainBackground
    ) {
        entries.extend(cron::cron_tool_entries());
        entries.push(ToolEntry::Base(Arc::new(
            background::BackgroundAgentsListTool,
        )));
    }

    entries.push(ToolEntry::Base(Arc::new(plan::UpdatePlanTool)));
    if enable_memory_tools {
        entries.extend(memory::memory_tool_entries());
    }
    entries.extend(subagent::subagent_tool_entries());

    match scope {
        HostToolScope::MainForeground => {
            entries.push(ToolEntry::Base(Arc::new(
                background::BackgroundAgentStartTool,
            )));
        }
        HostToolScope::MainBackground => {
            entries.push(ToolEntry::Base(Arc::new(background::TerminateTool)));
        }
        HostToolScope::SubAgent => {}
    }

    entries
}

fn bridge_tool(name: &'static str, description: &'static str, parameters: Value) -> ToolDefinition {
    ToolDefinition::new(
        name,
        description,
        parameters,
        ToolBackend::ConversationBridge {
            action: name.to_string(),
        },
    )
    .with_concurrency(ToolConcurrency::Serial)
}

fn call_bridge_tool(
    definition: ToolDefinition,
    ctx: &ToolCallContext<'_>,
    args: Value,
) -> Result<ToolResultContent, LocalToolError> {
    execute_bridge_tool(
        &definition.name,
        &definition.name,
        &definition.parameters,
        ctx,
        args,
    )
}
