mod create;
mod delete;
mod load;
mod update;

use std::sync::Arc;

use super::{ToolDefinition, ToolEntry};

pub fn skill_tool_definitions(
    _skill_names: &[String],
    enable_skill_persistence_tools: bool,
) -> Vec<ToolDefinition> {
    let mut tools = vec![load::skill_load_tool_definition()];

    if enable_skill_persistence_tools {
        tools.extend([
            create::skill_create_tool_definition(),
            update::skill_update_tool_definition(),
            delete::skill_delete_tool_definition(),
        ]);
    }

    tools
}

pub(crate) fn skill_tool_entries(
    _skill_names: &[String],
    enable_skill_persistence_tools: bool,
) -> Vec<ToolEntry> {
    let mut entries = vec![ToolEntry::Base(Arc::new(load::SkillLoadTool))];

    if enable_skill_persistence_tools {
        entries.extend([
            ToolEntry::Base(Arc::new(create::SkillCreateTool)),
            ToolEntry::Base(Arc::new(update::SkillUpdateTool)),
            ToolEntry::Base(Arc::new(delete::SkillDeleteTool)),
        ]);
    }

    entries
}
