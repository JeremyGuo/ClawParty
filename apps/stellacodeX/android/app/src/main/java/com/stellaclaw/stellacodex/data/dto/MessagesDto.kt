package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonObject

@Serializable
data class MessagesResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "",
    val offset: Int = 0,
    val limit: Int = 0,
    val total: Int = 0,
    val messages: List<ChatMessageDto> = emptyList(),
)

@Serializable
data class MessageDetailResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "main",
    val message: ChatMessageDto = ChatMessageDto(),
)

@Serializable
data class ChatMessageDto(
    val id: String = "",
    @SerialName("message_id") val messageId: String = "",
    val index: Int = 0,
    val role: String = "",
    val text: String = "",
    @SerialName("rendered_text") val renderedText: String = "",
    @SerialName("text_with_attachment_markers") val textWithAttachmentMarkers: String = "",
    val preview: String = "",
    val items: List<JsonObject> = emptyList(),
    val data: List<JsonObject> = emptyList(),
    val attachments: List<MessageAttachmentDto> = emptyList(),
    @SerialName("has_attachment_errors") val hasAttachmentErrors: Boolean = false,
    @SerialName("user_name") val userName: String? = null,
    @SerialName("message_time") val messageTime: String? = null,
    @SerialName("attachment_count") val attachmentCount: Int = 0,
    @SerialName("has_token_usage") val hasTokenUsage: Boolean = false,
    @SerialName("token_usage") val tokenUsage: MessageTokenUsageDto? = null,
)

@Serializable
data class MessageAttachmentDto(
    val id: String = "",
    val index: Int = 0,
    val kind: String = "document",
    val name: String = "",
    val filename: String = "",
    @SerialName("media_type") val mediaType: String? = null,
    @SerialName("mime_type") val mimeType: String? = null,
    val mime: String? = null,
    @SerialName("size_bytes") val sizeBytes: Long? = null,
    val url: String = "",
    val uri: String = "",
    @SerialName("file_uri") val fileUri: String = "",
    val path: String = "",
    @SerialName("file_path") val filePath: String = "",
    @SerialName("workspace_path") val workspacePath: String = "",
    @SerialName("relative_path") val relativePath: String = "",
    @SerialName("workspace_relative_path") val workspaceRelativePath: String = "",
    val src: String = "",
    @SerialName("data_url") val dataUrl: String = "",
    @SerialName("data_base64") val dataBase64: String = "",
    val base64: String = "",
    val data: String = "",
    val encoding: String = "",
    @SerialName("preview_url") val previewUrl: String = "",
    @SerialName("download_url") val downloadUrl: String = "",
    @SerialName("open_in_workspace_path") val openInWorkspacePath: String? = null,
)

@Serializable
data class MessageTokenUsageDto(
    @SerialName("cache_read") val cacheRead: Long = 0,
    @SerialName("cache_write") val cacheWrite: Long = 0,
    @SerialName("uncache_input") val uncacheInput: Long = 0,
    val input: Long = 0,
    val output: Long = 0,
    val total: Long = 0,
)

@Serializable
data class MarkConversationSeenRequestDto(
    @SerialName("last_seen_message_id") val lastSeenMessageId: String,
    @SerialName("foreground_session_id") val foregroundSessionId: String = "main",
)

@Serializable
data class SendMessageRequestDto(
    @SerialName("client_message_id") val clientMessageId: String? = null,
    @SerialName("user_name") val userName: String,
    @SerialName("message_time") val messageTime: String? = null,
    val text: String,
    val files: List<SendMessageFileDto> = emptyList(),
    @SerialName("selection_references") val selectionReferences: List<SelectionReferenceDto> = emptyList(),
)

@Serializable
data class SelectionReferenceDto(
    @SerialName("file_path") val filePath: String,
    @SerialName("file_name") val fileName: String? = null,
    @SerialName("media_type") val mediaType: String? = null,
    @SerialName("source_kind") val sourceKind: String = "workspace_file",
    @SerialName("selected_text") val selectedText: String,
)

@Serializable
data class SendMessageFileDto(
    val uri: String,
    @SerialName("media_type") val mediaType: String? = null,
    val name: String? = null,
)

@Serializable
data class SendMessageResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "main",
    @SerialName("client_message_id") val clientMessageId: String = "",
    val accepted: Boolean = false,
)
