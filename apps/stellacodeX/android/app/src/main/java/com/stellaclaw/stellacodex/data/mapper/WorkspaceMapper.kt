package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.WorkspaceEntryDto
import com.stellaclaw.stellacodex.data.dto.WorkspaceListingDto
import com.stellaclaw.stellacodex.domain.model.WorkspaceEntry
import com.stellaclaw.stellacodex.domain.model.WorkspaceListing

fun WorkspaceListingDto.toDomain(): WorkspaceListing = WorkspaceListing(
    workspaceRoot = workspaceRoot,
    path = path,
    parent = parent,
    totalEntries = totalEntries,
    returnedEntries = returnedEntries,
    truncated = truncated,
    entries = entries.map { it.toDomain() },
)

private fun WorkspaceEntryDto.toDomain(): WorkspaceEntry = WorkspaceEntry(
    name = name,
    path = path,
    kind = kind,
    sizeBytes = sizeBytes,
    modifiedMs = modifiedMs,
    hidden = hidden,
    readonly = readonly,
)
