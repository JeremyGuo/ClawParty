use serde_json::Value;

use crate::session_actor::{
    tool_catalog::{
        execute_bridge_tool,
        schema::{object_schema, properties},
        BaseTool, ToolBackend, ToolCallContext, ToolConcurrency, ToolDefinition, ToolExecutionMode,
    },
    tool_runtime::LocalToolError,
    ToolResultContent,
};

pub(super) struct SkillUpdateTool;

impl BaseTool for SkillUpdateTool {
    fn definition(&self) -> ToolDefinition {
        skill_update_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let definition = skill_update_tool_definition();
        execute_bridge_tool(
            &definition.name,
            "skill_update",
            &definition.parameters,
            ctx,
            args,
        )
    }
}

pub(super) fn skill_update_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "skill_update",
        "Persist a staged skill directory from .stellaclaw/skill/<skill_name>/ in the current workspace into the runtime skills store as an update to an existing skill. Validate SKILL.md and fail with the validation reason if invalid.",
        object_schema(
            properties([("skill_name", serde_json::json!({"type": "string"}))]),
            &["skill_name"],
        ),
        ToolExecutionMode::Immediate,
        ToolBackend::ConversationBridge {
            action: "skill_update".to_string(),
        },
    )
    .with_concurrency(ToolConcurrency::Serial)
}
