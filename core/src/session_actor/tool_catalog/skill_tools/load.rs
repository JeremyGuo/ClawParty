use serde_json::Value;

use crate::session_actor::{
    tool_catalog::{
        execute_bridge_tool,
        schema::{object_schema, properties},
        BaseTool, ToolBackend, ToolCallContext, ToolDefinition, ToolExecutionMode,
    },
    tool_runtime::LocalToolError,
    ToolResultContent,
};

pub(super) struct SkillLoadTool;

impl BaseTool for SkillLoadTool {
    fn definition(&self) -> ToolDefinition {
        skill_load_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let definition = skill_load_tool_definition();
        execute_bridge_tool(
            &definition.name,
            "skill_load",
            &definition.parameters,
            ctx,
            args,
        )
    }
}

pub(super) fn skill_load_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "skill_load",
        "Load the SKILL.md instructions for a named skill from the current workspace .stellaclaw/skill directory. Use exact skill names that currently exist under .stellaclaw/skill/.",
        object_schema(
            properties([("skill_name", serde_json::json!({"type": "string"}))]),
            &["skill_name"],
        ),
        ToolExecutionMode::Immediate,
        ToolBackend::ConversationBridge {
            action: "skill_load".to_string(),
        },
    )
}
