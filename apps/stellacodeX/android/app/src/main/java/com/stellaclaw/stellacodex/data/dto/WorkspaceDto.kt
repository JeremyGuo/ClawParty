package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class WorkspaceListingDto(
    val type: String = "",
    val target: String = "auto",
    @SerialName("workspace_root") val workspaceRoot: String = "",
    val path: String = "",
    val parent: String? = null,
    @SerialName("total_entries") val totalEntries: Int = 0,
    @SerialName("returned_entries") val returnedEntries: Int = 0,
    val truncated: Boolean = false,
    val entries: List<WorkspaceEntryDto> = emptyList(),
    val message: String? = null,
)

@Serializable
data class WorkspaceEntryDto(
    val name: String = "",
    val path: String = "",
    val kind: String = "other",
    @SerialName("size_bytes") val sizeBytes: Long? = null,
    @SerialName("modified_ms") val modifiedMs: Long? = null,
    val hidden: Boolean = false,
    val readonly: Boolean = false,
)

@Serializable
data class MoveWorkspacePathRequestDto(
    val path: String,
    @SerialName("new_path") val newPath: String,
)
