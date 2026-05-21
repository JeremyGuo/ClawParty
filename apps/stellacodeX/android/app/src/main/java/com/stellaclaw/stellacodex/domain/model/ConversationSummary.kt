package com.stellaclaw.stellacodex.domain.model

data class ConversationSummary(
    val conversationId: String,
    val platformChatId: String,
    val displayName: String,
    val model: String,
    val modelSelectionPending: Boolean,
    val reasoning: String,
    val sandbox: String,
    val sandboxSource: String,
    val remote: String,
    val workspace: String,
    val foregroundSessionId: String,
    val totalBackground: Int,
    val totalSubagents: Int,
    val processingState: String,
    val running: Boolean,
    val messageCount: Int,
    val lastMessageId: String?,
    val lastMessageTime: String?,
    val lastSeenMessageId: String?,
    val lastSeenAt: String?,
    val foregroundSessions: List<ForegroundSessionSummary> = emptyList(),
) {
    val hasUnread: Boolean = lastMessageId != null && lastMessageId != lastSeenMessageId
}

data class ForegroundSessionSummary(
    val id: String,
    val displayName: String,
    val state: String,
    val running: Boolean,
    val messageCount: Int,
    val lastMessageId: String?,
    val lastMessageTime: String?,
    val lastSeenMessageId: String?,
    val lastSeenAt: String?,
) {
    val hasUnread: Boolean = lastMessageId != null && lastMessageId != lastSeenMessageId
}
