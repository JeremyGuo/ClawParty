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

pub(super) fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        memory_search_tool_definition(),
        memory_write_tool_definition(),
        memory_update_tool_definition(),
        memory_delete_tool_definition(),
    ]
}

pub(super) fn memory_tool_entries() -> Vec<ToolEntry> {
    vec![
        ToolEntry::Base(Arc::new(MemorySearchTool)),
        ToolEntry::Base(Arc::new(MemoryWriteTool)),
        ToolEntry::Base(Arc::new(MemoryUpdateTool)),
        ToolEntry::Base(Arc::new(MemoryDeleteTool)),
    ]
}

macro_rules! impl_memory_tool {
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

impl_memory_tool!(MemorySearchTool, memory_search_tool_definition);
impl_memory_tool!(MemoryWriteTool, memory_write_tool_definition);
impl_memory_tool!(MemoryUpdateTool, memory_update_tool_definition);
impl_memory_tool!(MemoryDeleteTool, memory_delete_tool_definition);

fn memory_search_tool_definition() -> ToolDefinition {
    bridge_tool(
        "memory_search",
        "Search long memory for durable facts from conversation and public scopes.",
        object_schema(
            properties([
                (
                    "query",
                    json!({"type": "string", "description": "Natural language search query."}),
                ),
                (
                    "limit",
                    json!({"type": "number", "description": "Optional maximum result count. Defaults to 5 and is capped by the host."}),
                ),
                (
                    "scopes",
                    json!({
                        "type": "array",
                        "items": {"type": "string", "enum": ["conversation", "public"]},
                        "description": "Optional scopes to search. Defaults to conversation and public."
                    }),
                ),
            ]),
            &["query"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn memory_write_tool_definition() -> ToolDefinition {
    bridge_tool(
        "memory_write",
        "Persist one concise long-memory entry. The host may deduplicate or merge conflicting entries and returns success or failure.",
        object_schema(
            properties([
                (
                    "scope",
                    json!({
                        "type": "string",
                        "enum": ["user", "public", "conversation"],
                        "description": "Memory scope: user, conversation, or public."
                    }),
                ),
                ("subject", json!({"type": "string", "description": "Optional short subject or entity name."})),
                ("text", json!({"type": "string", "description": "Compact durable memory text. About 1KB maximum."})),
                ("tags", json!({"type": "array", "items": {"type": "string"}, "description": "Optional compact tags."})),
            ]),
            &["scope", "text"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn memory_update_tool_definition() -> ToolDefinition {
    bridge_tool(
        "memory_update",
        "Replace a memory entry by id.",
        object_schema(
            properties([
                ("memory_id", json!({"type": "string"})),
                ("text", json!({"type": "string"})),
            ]),
            &["memory_id", "text"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn memory_delete_tool_definition() -> ToolDefinition {
    bridge_tool(
        "memory_delete",
        "Delete or tombstone a memory entry by id.",
        object_schema(
            properties([("memory_id", json!({"type": "string"}))]),
            &["memory_id"],
        ),
        ToolExecutionMode::Immediate,
    )
}
