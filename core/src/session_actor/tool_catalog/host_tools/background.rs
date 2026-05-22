use serde_json::{json, Value};

use super::{bridge_tool, call_bridge_tool};
use crate::session_actor::{
    tool_catalog::{
        schema::{object_schema, properties},
        BaseTool, ToolCallContext, ToolDefinition,
    },
    tool_runtime::LocalToolError,
    ToolResultContent,
};

pub(super) struct BackgroundAgentStartTool;

impl BaseTool for BackgroundAgentStartTool {
    fn definition(&self) -> ToolDefinition {
        background_agent_start_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(background_agent_start_tool_definition(), ctx, args)
    }
}

pub(super) struct TerminateTool;

impl BaseTool for TerminateTool {
    fn definition(&self) -> ToolDefinition {
        terminate_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(terminate_tool_definition(), ctx, args)
    }
}

pub(super) struct BackgroundAgentsListTool;

impl BaseTool for BackgroundAgentsListTool {
    fn definition(&self) -> ToolDefinition {
        background_agents_list_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(background_agents_list_tool_definition(), ctx, args)
    }
}

pub(super) fn background_agent_start_tool_definition() -> ToolDefinition {
    bridge_tool(
        "background_agent_start",
        "Start a main background agent. Requires task. The background agent inherits this conversation's current model. The final user-facing reply is delivered to the current foreground conversation and inserted into the main foreground context.",
        object_schema(properties([("task", json!({"type": "string"}))]), &["task"]),    )
}

pub(super) fn terminate_tool_definition() -> ToolDefinition {
    bridge_tool(
        "terminate",
        "Terminate this main background agent silently. Use this when the task should stop without sending any user-facing reply or inserting anything into the main foreground context.",
        object_schema(properties([]), &[]),    )
}

pub(super) fn background_agents_list_tool_definition() -> ToolDefinition {
    bridge_tool(
        "background_agents_list",
        "List tracked background agents and subagents with status, task, latest message, and latest error.",
        object_schema(properties([]), &[]),    )
}
