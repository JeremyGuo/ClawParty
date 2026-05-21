package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.ChatMessageDto
import com.stellaclaw.stellacodex.data.dto.MessageAttachmentDto
import com.stellaclaw.stellacodex.data.dto.MessageTokenUsageDto
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageTokenUsage
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonPrimitive

fun ChatMessageDto.toDomain(): ChatMessage {
    val canonicalItems = data.ifEmpty { items }
    val derivedAttachments = canonicalItems.flatMapIndexed { index, item -> item.toAttachments(index) }
    val allAttachments = attachments.map { it.toDomain() } + derivedAttachments
    val mappedItems = if (data.isNotEmpty()) {
        var fileIndex = attachments.size
        canonicalItems.mapIndexedNotNull { index, item ->
            val messageItem = item.toMessageItem(index, fileIndex)
            if (item.typeName() == "file") fileIndex += 1
            messageItem
        }
    } else {
        items.mapNotNull { it.toMessageItem() }
    }
    val displayText = text.takeIf { it.isNotBlank() }
        ?: textWithAttachmentMarkers.takeIf { it.isNotBlank() }
        ?: renderedText.takeIf { it.isNotBlank() }
        ?: canonicalItems.contextText()
    return ChatMessage(
        id = id.ifBlank { messageId },
        index = index,
        role = role,
        text = displayText,
        preview = preview.ifBlank { displayText.take(160) },
        userName = userName,
        messageTime = messageTime,
        attachmentCount = if (attachmentCount > 0) attachmentCount else allAttachments.size,
        attachments = allAttachments,
        items = mappedItems,
        hasAttachmentErrors = hasAttachmentErrors,
        hasTokenUsage = hasTokenUsage || tokenUsage != null,
        tokenUsage = tokenUsage?.toDomain(),
    )
}

private fun MessageAttachmentDto.toDomain(): MessageAttachment {
    val resolvedName = name.ifBlank { filename }
        .ifBlank { fileNameFromPath(path.ifBlank { filePath }.ifBlank { url.ifBlank { uri.ifBlank { fileUri.ifBlank { src } } } }) }
        .ifBlank { "attachment" }
    val resolvedMediaType = mediaType ?: mimeType ?: mime ?: guessMediaType(resolvedName)
    return MessageAttachment(
        index = index,
        kind = kind,
        name = resolvedName,
        mediaType = resolvedMediaType,
        sizeBytes = sizeBytes,
        url = url,
        uri = uri,
        fileUri = fileUri,
        path = path,
        filePath = filePath,
        workspacePath = workspacePath,
        relativePath = relativePath.ifBlank { workspaceRelativePath },
        src = src,
        dataUrl = dataUrl,
        dataBase64 = dataBase64.ifBlank { base64 },
        data = data,
        encoding = encoding,
    )
}

private fun MessageTokenUsageDto.toDomain(): MessageTokenUsage = MessageTokenUsage(
    input = input.takeIf { it > 0 } ?: (cacheRead + cacheWrite + uncacheInput),
    output = output,
    total = total.takeIf { it > 0 } ?: (cacheRead + cacheWrite + uncacheInput + output),
)

private fun JsonObject.toMessageItem(indexOverride: Int? = null, fileAttachmentIndex: Int? = null): MessageItem? {
    val type = typeName()
    val payload = payloadObject()
    val index = indexOverride ?: int("index") ?: 0
    return when (type) {
        "context" -> MessageItem.Text(
            index = index,
            text = payload.string("text").orEmpty(),
        )
        "reasoning" -> MessageItem.Text(
            index = index,
            text = payload.string("text").orEmpty(),
        )
        "text" -> MessageItem.Text(
            index = index,
            text = payload.string("text").orEmpty(),
        )
        "file" -> MessageItem.File(
            index = index,
            attachmentIndex = fileAttachmentIndex ?: payload.int("attachment_index") ?: int("attachment_index") ?: -1,
        )
        "tool_call" -> MessageItem.ToolCall(
            index = index,
            toolCallId = payload.toolCallId(),
            toolName = payload.toolName(),
            arguments = payload["arguments"]?.asObject()?.string("text")
                ?: payload["arguments"]?.compactJson().orEmpty(),
            explanation = payload.string("explanation") ?: (payload["arguments"] as? JsonObject)?.string("explanation"),
        )
        "tool_result" -> MessageItem.ToolResult(
            index = index,
            toolCallId = payload.toolCallId(),
            toolName = payload.toolName(),
            context = payload["result"]?.asObject()?.get("structured")?.compactJson()
                ?: payload.string("context"),
            fileAttachmentIndex = payload.int("file_attachment_index"),
        )
        else -> null
    }
}

private fun List<JsonObject>.contextText(): String = mapNotNull { item ->
    val payload = item.payloadObject()
    when (item.typeName()) {
        "context" -> payload.string("text")
        "text" -> payload.string("text")
        else -> null
    }
}.filter { it.isNotBlank() }.joinToString("\n")

private fun JsonObject.toAttachments(index: Int): List<MessageAttachment> {
    if (typeName() == "tool_result") {
        val files = payloadObject().objectValue("result")?.get("files") ?: get("files")
        return runCatching { files?.jsonArray?.mapIndexedNotNull { offset, file -> file.asObject()?.toAttachment(index + offset) } }.getOrNull().orEmpty()
    }
    if (typeName() != "file") return emptyList()
    return listOfNotNull(toAttachment(index))
}

private fun JsonObject.toAttachment(index: Int): MessageAttachment? {
    val payload = payloadObject()
    val url = payload.string("url").orEmpty()
    val uri = payload.string("uri").orEmpty()
    val fileUri = payload.string("file_uri").orEmpty()
    val path = payload.string("path").orEmpty()
    val filePath = payload.string("file_path").orEmpty()
    val workspacePath = payload.string("workspace_path").orEmpty()
    val relativePath = payload.string("relative_path") ?: payload.string("workspace_relative_path").orEmpty()
    val src = payload.string("src").orEmpty()
    val dataUrl = payload.string("data_url").orEmpty()
    val dataBase64 = payload.string("data_base64") ?: payload.string("base64").orEmpty()
    val data = payload.string("data").orEmpty()
    val target = listOf(url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl, dataBase64, data).firstOrNull { it.isNotBlank() }.orEmpty()
    if (target.isBlank()) return null
    val name = payload.string("name") ?: payload.string("filename") ?: fileNameFromPath(path.ifBlank { filePath }.ifBlank { target }).ifBlank { "attachment" }
    val mediaType = payload.string("media_type") ?: payload.string("mime_type") ?: payload.string("mime") ?: guessMediaType(name)
    return MessageAttachment(
        index = index,
        kind = if (mediaType?.startsWith("image/") == true) "image" else "document",
        name = name,
        mediaType = mediaType,
        sizeBytes = payload.long("size_bytes"),
        url = url,
        uri = uri,
        fileUri = fileUri,
        path = path,
        filePath = filePath,
        workspacePath = workspacePath,
        relativePath = relativePath,
        src = src,
        dataUrl = dataUrl,
        dataBase64 = dataBase64,
        data = data,
        encoding = payload.string("encoding").orEmpty(),
    )
}

private fun JsonObject.typeName(): String = string("type").orEmpty()

private fun JsonObject.payloadObject(): JsonObject = get("payload") as? JsonObject ?: this

private fun JsonElement.asObject(): JsonObject? = this as? JsonObject

private fun JsonObject.objectValue(name: String): JsonObject? = get(name) as? JsonObject

private fun JsonObject.string(name: String): String? = get(name)?.let { value ->
    if (value is JsonPrimitive && value.isString) value.content else null
}

private fun JsonObject.int(name: String): Int? = get(name)?.jsonPrimitive?.intOrNull

private fun JsonObject.long(name: String): Long? = get(name)?.jsonPrimitive?.longOrNull

private fun JsonObject.toolCallId(): String = listOf(
    string("tool_call_id"),
    string("toolCallId"),
    string("call_id"),
    string("callId"),
    string("item_id"),
    string("itemId"),
    string("id"),
).firstOrNull { !it.isNullOrBlank() }.orEmpty()

private fun JsonObject.toolName(): String = listOf(
    string("tool_name"),
    string("toolName"),
    string("name"),
).firstOrNull { !it.isNullOrBlank() }.orEmpty()

private fun JsonElement.compactJson(): String = when (this) {
    JsonNull -> "null"
    is JsonPrimitive -> content
    else -> toString()
}

private fun fileNameFromPath(value: String): String = value
    .substringBefore('?')
    .substringBefore('#')
    .trimEnd('/')
    .substringAfterLast('/')
    .substringAfterLast('\\')

private fun guessMediaType(name: String): String? = when (name.substringAfterLast('.', "").lowercase()) {
    "png" -> "image/png"
    "jpg", "jpeg" -> "image/jpeg"
    "gif" -> "image/gif"
    "webp" -> "image/webp"
    "svg" -> "image/svg+xml"
    "txt", "md", "log", "kt", "java", "rs", "js", "ts", "tsx", "jsx", "py", "toml", "yaml", "yml", "gradle", "kts", "css", "html" -> "text/plain"
    "json" -> "application/json"
    "xml" -> "application/xml"
    "pdf" -> "application/pdf"
    else -> null
}
