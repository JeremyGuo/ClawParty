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

pub(super) struct UpdatePlanTool;

impl BaseTool for UpdatePlanTool {
    fn definition(&self) -> ToolDefinition {
        update_plan_tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        call_bridge_tool(update_plan_tool_definition(), ctx, args)
    }
}

pub(super) fn update_plan_tool_definition() -> ToolDefinition {
    bridge_tool(
        "update_plan",
        "Replace the current task plan shown to the user.",
        object_schema(
            properties([
                ("explanation", json!({"type": "string"})),
                (
                    "plan",
                    json!({
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {"type": "string"},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }),
                ),
            ]),
            &["plan"],
        ),
    )
}
