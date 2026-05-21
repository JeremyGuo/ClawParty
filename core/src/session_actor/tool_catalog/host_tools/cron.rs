use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::{bridge_tool, call_bridge_tool};
use crate::session_actor::{
    tool_catalog::{
        schema::{object_schema, properties},
        BaseTool, ToolCallContext, ToolDefinition, ToolEntry, ToolExecutionMode,
    },
    tool_runtime::LocalToolError,
    ToolResultContent,
};

pub(super) fn cron_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        cron_tasks_list_tool_definition(),
        cron_task_get_tool_definition(),
        cron_task_create_tool_definition(),
        cron_task_update_tool_definition(),
        cron_task_remove_tool_definition(),
    ]
}

pub(super) fn cron_tool_entries() -> Vec<ToolEntry> {
    vec![
        ToolEntry::Base(Arc::new(CronTasksListTool)),
        ToolEntry::Base(Arc::new(CronTaskGetTool)),
        ToolEntry::Base(Arc::new(CronTaskCreateTool)),
        ToolEntry::Base(Arc::new(CronTaskUpdateTool)),
        ToolEntry::Base(Arc::new(CronTaskRemoveTool)),
    ]
}

macro_rules! impl_cron_tool {
    ($type_name:ident, $definition_fn:ident) => {
        pub(super) struct $type_name;

        impl BaseTool for $type_name {
            fn definition(&self) -> ToolDefinition {
                $definition_fn()
            }

            fn call(
                &self,
                ctx: &ToolCallContext<'_>,
                args: Value,
            ) -> Result<ToolResultContent, LocalToolError> {
                call_bridge_tool($definition_fn(), ctx, args)
            }
        }
    };
}

impl_cron_tool!(CronTasksListTool, cron_tasks_list_tool_definition);
impl_cron_tool!(CronTaskGetTool, cron_task_get_tool_definition);
impl_cron_tool!(CronTaskCreateTool, cron_task_create_tool_definition);
impl_cron_tool!(CronTaskUpdateTool, cron_task_update_tool_definition);
impl_cron_tool!(CronTaskRemoveTool, cron_task_remove_tool_definition);

fn cron_tasks_list_tool_definition() -> ToolDefinition {
    bridge_tool(
        "cron_tasks_list",
        "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
        object_schema(properties([]), &[]),
        ToolExecutionMode::Immediate,
    )
}

fn cron_task_get_tool_definition() -> ToolDefinition {
    bridge_tool(
        "cron_task_get",
        "Get full details for a cron task by id.",
        object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
        ToolExecutionMode::Immediate,
    )
}

fn cron_task_create_tool_definition() -> ToolDefinition {
    bridge_tool(
        "cron_task_create",
        "Create a persisted cron task owned by this session. Provide each cron time field as a named argument; the host builds a seconds-first cron expression in the task timezone. task launches a background agent with that prompt.",
        cron_create_schema(),
        ToolExecutionMode::Immediate,
    )
}

fn cron_task_update_tool_definition() -> ToolDefinition {
    bridge_tool(
        "cron_task_update",
        "Update a cron task owned by this session. To change timing, provide all named cron fields together: cron_second, cron_minute, cron_hour, cron_day_of_month, cron_month, cron_day_of_week, plus optional cron_year. Use timezone to change the IANA timezone and enabled to pause or resume it. Setting task changes the background-agent prompt.",
        cron_update_schema(),
        ToolExecutionMode::Immediate,
    )
}

fn cron_task_remove_tool_definition() -> ToolDefinition {
    bridge_tool(
        "cron_task_remove",
        "Remove a cron task permanently.",
        object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
        ToolExecutionMode::Immediate,
    )
}

fn cron_create_schema() -> Value {
    object_schema(
        cron_common_properties(),
        &[
            "name",
            "description",
            "cron_second",
            "cron_minute",
            "cron_hour",
            "cron_day_of_month",
            "cron_month",
            "cron_day_of_week",
            "task",
        ],
    )
}

fn cron_update_schema() -> Value {
    let mut schema_properties = cron_common_properties();
    schema_properties.insert("id".to_string(), json!({"type": "string"}));
    schema_properties.insert("clear_task".to_string(), json!({"type": "boolean"}));
    object_schema(schema_properties, &["id"])
}

fn cron_common_properties() -> Map<String, Value> {
    properties([
        ("name", json!({"type": "string"})),
        ("description", json!({"type": "string"})),
        (
            "cron_second",
            json!({"type": "string", "description": "Seconds field. Examples: '0', '*/30', '*'."}),
        ),
        (
            "cron_minute",
            json!({"type": "string", "description": "Minutes field. Examples: '0', '*/5', '*'."}),
        ),
        (
            "cron_hour",
            json!({"type": "string", "description": "Hours field in the task timezone. Examples: '13', '9-17', '*'."}),
        ),
        (
            "cron_day_of_month",
            json!({"type": "string", "description": "Day-of-month field in the task timezone. Examples: '17', '1,15', '*'."}),
        ),
        (
            "cron_month",
            json!({"type": "string", "description": "Month field in the task timezone. Examples: '4', '1-12', '*'."}),
        ),
        (
            "cron_day_of_week",
            json!({"type": "string", "description": "Day-of-week field in the task timezone. Examples: '*', 'Mon-Fri', '0'."}),
        ),
        (
            "cron_year",
            json!({"type": "string", "description": "Optional year field in the task timezone. Example: '2026'."}),
        ),
        (
            "timezone",
            json!({"type": "string", "description": "IANA timezone for these cron fields, e.g. 'Asia/Shanghai'. Defaults to 'Asia/Shanghai'."}),
        ),
        (
            "task",
            json!({"type": "string", "description": "Prompt for a background agent."}),
        ),
        ("enabled", json!({"type": "boolean"})),
    ])
}
