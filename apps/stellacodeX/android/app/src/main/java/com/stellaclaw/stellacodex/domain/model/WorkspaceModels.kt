package com.stellaclaw.stellacodex.domain.model

data class WorkspaceListing(
    val workspaceRoot: String,
    val path: String,
    val parent: String?,
    val totalEntries: Int,
    val returnedEntries: Int,
    val truncated: Boolean,
    val entries: List<WorkspaceEntry>,
)

data class WorkspaceEntry(
    val name: String,
    val path: String,
    val kind: String,
    val sizeBytes: Long?,
    val modifiedMs: Long?,
    val hidden: Boolean,
    val readonly: Boolean,
) {
    val isDirectory: Boolean = kind == "directory"
    val isFile: Boolean = kind == "file"
}
