package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.ConversationSummaryDto
import com.stellaclaw.stellacodex.data.dto.ForegroundSessionSummaryDto
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.ForegroundSessionSummary

fun ConversationSummaryDto.toDomain(): ConversationSummary = ConversationSummary(
    conversationId = conversationId,
    platformChatId = platformChatId,
    displayName = nickname?.trim().takeUnless { it.isNullOrBlank() }
        ?: conversationName?.trim().takeUnless { it.isNullOrBlank() }
        ?: platformChatId.takeIf { it.isNotBlank() }
        ?: conversationId,
    model = model,
    modelSelectionPending = modelSelectionPending,
    reasoning = reasoning,
    sandbox = sandbox,
    sandboxSource = sandboxSource,
    remote = remote,
    workspace = workspace,
    foregroundSessionId = mainForegroundSession().foregroundSessionId.takeIf { it.isNotBlank() }
        ?: foregroundSessionId.takeIf { it.isNotBlank() }
        ?: "main",
    totalBackground = totalBackground,
    totalSubagents = totalSubagents,
    processingState = mainForegroundSession().state.takeIf { it.isNotBlank() } ?: processingState,
    running = running || mainForegroundSession().state in setOf("running", "queued"),
    messageCount = mainForegroundSession().messageCount.takeIf { it > 0 } ?: messageCount,
    lastMessageId = mainForegroundSession().lastCommittedMessageId ?: mainForegroundSession().lastMessageId ?: lastCommittedMessageId ?: lastMessageId,
    lastMessageTime = mainForegroundSession().lastActivityAt ?: mainForegroundSession().lastMessageTime ?: updatedAt ?: lastMessageTime,
    lastSeenMessageId = mainForegroundSession().lastSeenMessageId ?: lastSeenMessageId,
    lastSeenAt = mainForegroundSession().lastSeenAt ?: lastSeenAt,
    foregroundSessions = foregroundSessions.ifEmpty { listOf(mainForegroundSession()) }.map { it.toDomain() },
)

private fun ConversationSummaryDto.mainForegroundSession() = foregroundSessions.firstOrNull { it.foregroundSessionId == "main" || it.id == "main" || it.isMain }
    ?: foregroundSessions.firstOrNull()
    ?: com.stellaclaw.stellacodex.data.dto.ForegroundSessionSummaryDto(foregroundSessionId = "main")

private fun ForegroundSessionSummaryDto.toDomain(): ForegroundSessionSummary {
    val resolvedId = foregroundSessionId.ifBlank { id.ifBlank { "main" } }
    val resolvedState = state.ifBlank { "idle" }
    val lastId = lastCommittedMessageId ?: lastMessageId
    return ForegroundSessionSummary(
        id = resolvedId,
        displayName = sessionName?.takeIf { it.isNotBlank() } ?: nickname?.takeIf { it.isNotBlank() } ?: resolvedId,
        state = resolvedState,
        running = resolvedState == "running" || resolvedState == "queued",
        messageCount = messageCount,
        lastMessageId = lastId,
        lastMessageTime = lastActivityAt ?: lastMessageTime,
        lastSeenMessageId = lastSeenMessageId,
        lastSeenAt = lastSeenAt,
    )
}
