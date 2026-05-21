use std::sync::Arc;

use serde_json::{json, Value};

use super::{bridge_tool, call_bridge_tool};
use crate::session_actor::{
    tool_catalog::{
        schema::{object_schema, properties},
        BaseTool, ToolCallContext, ToolDefinition, ToolEntry, ToolExecutionMode,
    },
    tool_runtime::LocalToolError,
    ToolResultContent,
};

pub(super) fn subagent_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        subagent_start_tool_definition(),
        subagent_kill_tool_definition(),
        subagent_join_tool_definition(),
    ]
}

pub(super) fn subagent_tool_entries() -> Vec<ToolEntry> {
    vec![
        ToolEntry::Base(Arc::new(SubagentStartTool)),
        ToolEntry::Base(Arc::new(SubagentKillTool)),
        ToolEntry::Base(Arc::new(SubagentJoinTool)),
    ]
}

pub(super) struct SubagentStartTool;

impl BaseTool for SubagentStartTool {
    fn definition(&self) -> ToolDefinition {
        subagent_start_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(subagent_start_tool_definition(), ctx, args)
    }
}

pub(super) struct SubagentKillTool;

impl BaseTool for SubagentKillTool {
    fn definition(&self) -> ToolDefinition {
        subagent_kill_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(subagent_kill_tool_definition(), ctx, args)
    }
}

pub(super) struct SubagentJoinTool;

impl BaseTool for SubagentJoinTool {
    fn definition(&self) -> ToolDefinition {
        subagent_join_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(subagent_join_tool_definition(), ctx, args)
    }
}

fn subagent_start_tool_definition() -> ToolDefinition {
    bridge_tool(
        "subagent_start",
        "Start a session-bound subagent for a small delegated task. For multi-step tasks that require more than 3 sequential tool operations and can be clearly scoped, such as exploring a codebase module, running benchmarks, or setting up a dependency, prefer this tool to keep the main conversation context lean. Do not batch tool calls that could cause irreversible damage if an earlier step produces unexpected results, such as destructive shell commands, production deploys, or database mutations; use this tool for those instead so intermediate results can be inspected. Requires description. The subagent always inherits this conversation's current model.",
        object_schema(
            properties([("description", json!({"type": "string"}))]),
            &["description"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn subagent_kill_tool_definition() -> ToolDefinition {
    bridge_tool(
        "subagent_kill",
        "Kill a running subagent and clean up its state.",
        object_schema(
            properties([("agent_id", json!({"type": "string"}))]),
            &["agent_id"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn subagent_join_tool_definition() -> ToolDefinition {
    bridge_tool(
        "subagent_join",
        "Wait until a subagent finishes or fails. Supports an optional timeout_seconds; timing out returns a still-running result without killing the subagent. Finished or failed subagents are destroyed immediately after join returns them.",
        object_schema(
            properties([
                ("agent_id", json!({"type": "string"})),
                ("timeout_seconds", json!({"type": "number"})),
            ]),
            &["agent_id"],
        ),
        ToolExecutionMode::Interruptible,
    )
}
