mod patch;
mod visibility;

use std::sync::Arc;

use super::{BaseTool, ToolDefinition, ToolEntry, ToolRemoteMode};
pub(crate) use patch::ApplyPatchTool;
pub(crate) use visibility::ShellMakeVisibleTool;

pub fn file_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut tools = vec![ApplyPatchTool::new(remote_mode).definition()];
    if matches!(remote_mode, ToolRemoteMode::FixedSsh { .. }) {
        tools.extend([
            visibility::ShellMakeVisibleTool.definition(),
            visibility::AttachmentMakeVisibleTool.definition(),
        ]);
    }
    tools
}

pub(crate) fn file_tool_entries(remote_mode: &ToolRemoteMode) -> Vec<ToolEntry> {
    let mut entries = vec![ToolEntry::Base(Arc::new(patch::ApplyPatchTool::new(
        remote_mode,
    )))];
    if matches!(remote_mode, ToolRemoteMode::FixedSsh { .. }) {
        entries.extend([
            ToolEntry::Base(Arc::new(visibility::ShellMakeVisibleTool)),
            ToolEntry::Base(Arc::new(visibility::AttachmentMakeVisibleTool)),
        ]);
    }
    entries
}
