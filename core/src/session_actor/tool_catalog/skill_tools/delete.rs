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

pub(super) struct SkillDeleteTool;

impl BaseTool for SkillDeleteTool {
    fn definition(&self) -> ToolDefinition {
        skill_delete_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let definition = skill_delete_tool_definition();
        execute_bridge_tool(
            &definition.name,
            "skill_delete",
            &definition.parameters,
            ctx,
            args,
        )
    }
}

pub(super) fn skill_delete_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "skill_delete",
        "Persist deletion of an existing skill by removing .stellaclaw/skill/<skill_name>/ from the runtime skills store and active local workspaces.",
        object_schema(
            properties([("skill_name", serde_json::json!({"type": "string"}))]),
            &["skill_name"],
        ),
        ToolExecutionMode::Immediate,
        ToolBackend::ConversationBridge {
            action: "skill_delete".to_string(),
        },
    )
    .with_concurrency(ToolConcurrency::Serial)
}
