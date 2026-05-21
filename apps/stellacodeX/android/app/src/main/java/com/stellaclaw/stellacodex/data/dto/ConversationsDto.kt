package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class ConversationsResponseDto(
    @SerialName("channel_id") val channelId: String = "",
    val offset: Int = 0,
    val limit: Int = 0,
    val total: Int = 0,
    val conversations: List<ConversationSummaryDto> = emptyList(),
)

@Serializable
data class HomeSnapshotDto(
    val type: String = "",
    val seq: Long = 0,
    @SerialName("server_time") val serverTime: String = "",
    val conversations: List<ConversationSummaryDto> = emptyList(),
)

@Serializable
data class ForegroundSessionSummaryDto(
    val id: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "",
    @SerialName("session_id") val sessionId: String = "",
    val nickname: String? = null,
    @SerialName("session_name") val sessionName: String? = null,
    val state: String = "idle",
    @SerialName("active_turn_id") val activeTurnId: String? = null,
    @SerialName("is_main") val isMain: Boolean = false,
    @SerialName("message_count") val messageCount: Int = 0,
    @SerialName("last_message_id") val lastMessageId: String? = null,
    @SerialName("last_message_time") val lastMessageTime: String? = null,
    @SerialName("last_committed_message_id") val lastCommittedMessageId: String? = null,
    @SerialName("last_committed_message_index") val lastCommittedMessageIndex: Int? = null,
    @SerialName("last_activity_at") val lastActivityAt: String? = null,
    @SerialName("last_seen_message_id") val lastSeenMessageId: String? = null,
    @SerialName("last_seen_at") val lastSeenAt: String? = null,
)

@Serializable
data class ConversationSummaryDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("conversation_name") val conversationName: String? = null,
    @SerialName("platform_chat_id") val platformChatId: String = "",
    val nickname: String? = null,
    val model: String = "",
    @SerialName("model_selection_pending") val modelSelectionPending: Boolean = false,
    val reasoning: String = "",
    val sandbox: String = "",
    @SerialName("sandbox_source") val sandboxSource: String = "",
    val remote: String = "",
    val workspace: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "",
    @SerialName("total_background") val totalBackground: Int = 0,
    @SerialName("total_subagents") val totalSubagents: Int = 0,
    @SerialName("processing_state") val processingState: String = "idle",
    val running: Boolean = false,
    @SerialName("message_count") val messageCount: Int = 0,
    @SerialName("last_message_id") val lastMessageId: String? = null,
    @SerialName("last_message_time") val lastMessageTime: String? = null,
    @SerialName("last_committed_message_id") val lastCommittedMessageId: String? = null,
    @SerialName("last_committed_message_index") val lastCommittedMessageIndex: Int? = null,
    @SerialName("updated_at") val updatedAt: String? = null,
    @SerialName("last_seen_message_id") val lastSeenMessageId: String? = null,
    @SerialName("last_seen_at") val lastSeenAt: String? = null,
    @SerialName("foreground_sessions") val foregroundSessions: List<ForegroundSessionSummaryDto> = emptyList(),
)
