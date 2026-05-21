package com.stellaclaw.stellacodex.data.api

import com.stellaclaw.stellacodex.core.result.AppError
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.data.dto.CreateConversationRequestDto
import com.stellaclaw.stellacodex.data.dto.CreateConversationResponseDto
import com.stellaclaw.stellacodex.data.dto.CreateForegroundSessionRequestDto
import com.stellaclaw.stellacodex.data.dto.ForegroundSessionResponseDto
import com.stellaclaw.stellacodex.data.dto.HomeSnapshotDto
import com.stellaclaw.stellacodex.data.dto.MarkConversationSeenRequestDto
import com.stellaclaw.stellacodex.data.dto.MessageDetailResponseDto
import com.stellaclaw.stellacodex.data.dto.MessagesResponseDto
import com.stellaclaw.stellacodex.data.dto.ModelsResponseDto
import com.stellaclaw.stellacodex.data.dto.MoveWorkspacePathRequestDto
import com.stellaclaw.stellacodex.data.dto.RenameConversationRequestDto
import com.stellaclaw.stellacodex.data.dto.RenameConversationResponseDto
import com.stellaclaw.stellacodex.data.dto.RenameForegroundSessionRequestDto
import com.stellaclaw.stellacodex.data.dto.SendMessageFileDto
import com.stellaclaw.stellacodex.data.dto.SendMessageRequestDto
import com.stellaclaw.stellacodex.data.dto.SendMessageResponseDto
import com.stellaclaw.stellacodex.data.dto.SelectionReferenceDto
import com.stellaclaw.stellacodex.data.dto.WorkspaceListingDto
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.ssh.SshTunnelManager
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.ModelInfo
import com.stellaclaw.stellacodex.domain.model.WorkspaceListing
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.io.IOException
import java.time.Instant
import java.util.Base64
import java.util.concurrent.TimeUnit
import kotlin.math.min
import kotlin.random.Random

data class MessagePage(
    val offset: Int,
    val limit: Int,
    val total: Int,
    val messages: List<ChatMessage>,
)

data class FetchedAttachment(
    val bytes: ByteArray,
    val mediaType: String?,
)

data class WorkspaceFileContent(
    val bytes: ByteArray,
    val mediaType: String?,
    val name: String? = null,
    val path: String? = null,
)

class StellaclawApi(
    private val httpClient: OkHttpClient = defaultHttpClient(),
    val webSocketClient: OkHttpClient = defaultWebSocketClient(),
    private val tunnelManager: SshTunnelManager = SshTunnelManager(),
    private val json: Json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    },
) {
    suspend fun loadModels(profile: ConnectionProfile): AppResult<List<ModelInfo>> = get(profile, "/api/models") { body ->
        json.decodeFromString<ModelsResponseDto>(body).models.map { it.toDomain() }
    }

    suspend fun loadConversations(
        profile: ConnectionProfile,
        limit: Int = 80,
    ): AppResult<List<ConversationSummary>> = when (val result = loadHomeSnapshot(profile)) {
        is AppResult.Ok -> AppResult.Ok(result.value.conversations.map { it.toDomain() }.take(limit))
        is AppResult.Err -> result
    }

    suspend fun loadHomeSnapshot(profile: ConnectionProfile): AppResult<HomeSnapshotDto> {
        val request = when (val result = homeWebSocketRequest(profile)) {
            is AppResult.Ok -> result.value
            is AppResult.Err -> return result
        }
        return withContext(Dispatchers.IO) {
            val deferred = CompletableDeferred<AppResult<HomeSnapshotDto>>()
            val socket = webSocketClient.newWebSocket(request, object : WebSocketListener() {
                override fun onMessage(webSocket: WebSocket, text: String) {
                    try {
                        val snapshot = json.decodeFromString<HomeSnapshotDto>(text)
                        if (snapshot.type == "home.snapshot") {
                            deferred.complete(AppResult.Ok(snapshot))
                            webSocket.close(1000, "snapshot received")
                        }
                    } catch (error: SerializationException) {
                        deferred.complete(AppResult.Err(AppError.Decode(error.message.orEmpty())))
                        webSocket.close(1002, "snapshot decode failed")
                    }
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    deferred.complete(AppResult.Err(AppError.Network(t.message.orEmpty())))
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    if (!deferred.isCompleted) deferred.complete(AppResult.Err(AppError.Network("Home snapshot websocket closed")))
                }
            })
            try {
                withTimeout(5_000L) { deferred.await() }
            } catch (error: Exception) {
                socket.close(1000, "snapshot timeout")
                AppResult.Err(AppError.Network(error.message ?: "Home snapshot timeout"))
            }
        }
    }

    suspend fun createConversation(
        profile: ConnectionProfile,
        nickname: String? = null,
    ): AppResult<String> = post(
        profile = profile,
        path = "/api/conversations",
        body = json.encodeToString(
            CreateConversationRequestDto(
                nickname = nickname?.trim()?.takeIf { it.isNotEmpty() },
            ),
        ),
        retryPolicy = RetryPolicy.NoRetry,
    ) { responseBody ->
        json.decodeFromString<CreateConversationResponseDto>(responseBody).conversationId
    }

    suspend fun renameConversation(
        profile: ConnectionProfile,
        conversationId: String,
        nickname: String,
    ): AppResult<ConversationSummary?> = patch(
        profile = profile,
        path = "/api/conversations/$conversationId",
        body = json.encodeToString(RenameConversationRequestDto(nickname = nickname)),
        retryPolicy = RetryPolicy.Default,
    ) { body -> json.decodeFromString<RenameConversationResponseDto>(body).conversation?.toDomain() }

    suspend fun deleteConversation(profile: ConnectionProfile, conversationId: String): AppResult<Unit> = delete(
        profile = profile,
        path = "/api/conversations/$conversationId",
    ) { Unit }

    suspend fun createForegroundSession(
        profile: ConnectionProfile,
        conversationId: String,
        sessionId: String? = null,
        nickname: String? = null,
    ): AppResult<Unit> = post(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions",
        body = json.encodeToString(CreateForegroundSessionRequestDto(sessionId = sessionId, nickname = nickname)),
        retryPolicy = RetryPolicy.Default,
    ) { json.decodeFromString<ForegroundSessionResponseDto>(it); Unit }

    suspend fun renameForegroundSession(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String,
        nickname: String,
    ): AppResult<Unit> = patch(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}",
        body = json.encodeToString(RenameForegroundSessionRequestDto(nickname = nickname)),
        retryPolicy = RetryPolicy.Default,
    ) { json.decodeFromString<ForegroundSessionResponseDto>(it); Unit }

    suspend fun deleteForegroundSession(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String,
    ): AppResult<Unit> = delete(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}",
    ) { Unit }

    suspend fun loadMessagePage(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String = "main",
        offset: Int = 0,
        limit: Int = 80,
    ): AppResult<MessagePage> = get(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}/messages?offset=$offset&limit=$limit",
    ) { body ->
        val page = json.decodeFromString<MessagesResponseDto>(body)
        MessagePage(
            offset = page.offset,
            limit = page.limit,
            total = page.total,
            messages = page.messages.map { it.toDomain() },
        )
    }

    suspend fun loadLatestMessages(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String = "main",
        limit: Int = 30,
    ): AppResult<MessagePage> = when (val firstPage = loadMessagePage(profile, conversationId, foregroundSessionId, offset = 0, limit = 1)) {
        is AppResult.Err -> firstPage
        is AppResult.Ok -> {
            val total = firstPage.value.total
            val latestOffset = (total - limit).coerceAtLeast(0)
            loadMessagePage(profile, conversationId, foregroundSessionId, offset = latestOffset, limit = limit)
        }
    }

    suspend fun loadMessageDetail(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String = "main",
        messageId: String,
    ): AppResult<ChatMessage> = get(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}/messages/${urlEncode(messageId)}",
    ) { body -> json.decodeFromString<MessageDetailResponseDto>(body).message.toDomain() }

    suspend fun markConversationSeen(
        profile: ConnectionProfile,
        conversationId: String,
        lastSeenMessageId: String,
        foregroundSessionId: String = "main",
    ): AppResult<Unit> = post(
        profile = profile,
        path = "/api/conversations/$conversationId/seen",
        body = json.encodeToString(MarkConversationSeenRequestDto(lastSeenMessageId = lastSeenMessageId, foregroundSessionId = foregroundSessionId)),
        retryPolicy = RetryPolicy.Default,
    ) { Unit }

    suspend fun sendMessage(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String = "main",
        text: String,
        files: List<SendMessageFileDto> = emptyList(),
        selectionReferences: List<SelectionReferenceDto> = emptyList(),
        remoteMessageId: String? = null,
        messageTime: String = Instant.now().toString(),
    ): AppResult<Unit> = post(
        profile = profile,
        path = "/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}/messages",
        body = json.encodeToString(
            SendMessageRequestDto(
                clientMessageId = remoteMessageId,
                userName = profile.userName.ifBlank { "workspace-user" },
                messageTime = messageTime,
                text = text,
                files = files,
                selectionReferences = selectionReferences,
            ),
        ),
        retryPolicy = if (remoteMessageId.isNullOrBlank()) RetryPolicy.NoRetry else RetryPolicy.Default,
    ) { responseBody ->
        json.decodeFromString<SendMessageResponseDto>(responseBody)
        Unit
    }

    suspend fun fetchAttachment(
        profile: ConnectionProfile,
        attachmentUrl: String,
    ): AppResult<FetchedAttachment> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        if (attachmentUrl.isBlank()) return AppResult.Err(AppError.Network("Attachment URL is empty"))
        return fetchAttachmentBytes(profile, attachmentUrl)
    }

    suspend fun fetchWorkspaceFile(
        profile: ConnectionProfile,
        conversationId: String,
        path: String,
        limitBytes: Int = 2_000_000,
    ): AppResult<WorkspaceFileContent> = get(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace/file?path=${urlEncode(path)}&offset=0&limit_bytes=$limitBytes",
    ) { body ->
        val payload = json.decodeFromString<JsonObject>(body)
        val encoding = payload["encoding"]?.jsonPrimitive?.content.orEmpty()
        val data = payload["data"]?.jsonPrimitive?.content.orEmpty()
        val bytes = if (encoding == "base64") Base64.getDecoder().decode(data) else data.toByteArray(Charsets.UTF_8)
        val responseName = payload["name"]?.jsonPrimitive?.content
        val responsePath = payload["path"]?.jsonPrimitive?.content
        WorkspaceFileContent(
            bytes = bytes,
            mediaType = guessMediaType(responseName ?: responsePath ?: path),
            name = responseName,
            path = responsePath,
        )
    }

    suspend fun loadWorkspace(
        profile: ConnectionProfile,
        conversationId: String,
        path: String = "",
        limit: Int = 300,
    ): AppResult<WorkspaceListing> = get(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace?path=${urlEncode(path.trimStart('/'))}&limit=$limit",
    ) { body -> json.decodeFromString<WorkspaceListingDto>(body).toDomain() }

    suspend fun deleteWorkspacePath(
        profile: ConnectionProfile,
        conversationId: String,
        path: String,
    ): AppResult<Unit> = delete(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace?path=${urlEncode(path.trimStart('/'))}",
    ) { Unit }

    suspend fun moveWorkspacePath(
        profile: ConnectionProfile,
        conversationId: String,
        path: String,
        newPath: String,
    ): AppResult<Unit> = patch(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace",
        body = json.encodeToString(MoveWorkspacePathRequestDto(path = path.trimStart('/'), newPath = newPath.trimStart('/'))),
        retryPolicy = RetryPolicy.Default,
    ) { Unit }

    suspend fun downloadWorkspaceArchive(
        profile: ConnectionProfile,
        conversationId: String,
        path: String,
    ): AppResult<WorkspaceFileContent> = requestBytes(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace/download?path=${urlEncode(path.trimStart('/'))}",
        method = "GET",
        body = null,
        retryPolicy = RetryPolicy.Default,
    ) { bytes, mediaType -> WorkspaceFileContent(bytes, mediaType ?: "application/gzip") }

    suspend fun uploadWorkspaceArchive(
        profile: ConnectionProfile,
        conversationId: String,
        path: String,
        bytes: ByteArray,
    ): AppResult<Unit> = requestBytes(
        profile = profile,
        path = "/api/conversations/$conversationId/workspace/upload?path=${urlEncode(path.trimStart('/'))}",
        method = "POST",
        body = bytes,
        retryPolicy = RetryPolicy.Default,
    ) { _, _ -> Unit }

    private suspend fun fetchAttachmentBytes(
        profile: ConnectionProfile,
        attachmentUrl: String,
    ): AppResult<FetchedAttachment> = withContext(Dispatchers.IO) {
        var attempt = 0
        var forceRefreshTunnel = false
        while (true) {
            try {
                val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
                val urlText = if (attachmentUrl.startsWith("http://") || attachmentUrl.startsWith("https://")) {
                    attachmentUrl
                } else {
                    baseUrl.plus(if (attachmentUrl.startsWith('/')) attachmentUrl else "/$attachmentUrl")
                }
                val url = urlText.toHttpUrlOrNull()
                    ?: return@withContext AppResult.Err(AppError.Network("Invalid attachment URL"))
                val request = Request.Builder()
                    .url(url)
                    .header("Authorization", "Bearer ${profile.token.trim()}")
                    .get()
                    .build()
                httpClient.newCall(request).execute().use { response ->
                    val bytes = response.body?.bytes() ?: ByteArray(0)
                    val result = when {
                        response.code == 401 -> AppResult.Err(AppError.Unauthorized())
                        !response.isSuccessful -> AppResult.Err(AppError.Server(response.code, response.message))
                        else -> AppResult.Ok(FetchedAttachment(bytes = bytes, mediaType = response.header("content-type")))
                    }
                    if (!result.shouldRetry(attempt)) return@withContext result
                }
            } catch (error: IOException) {
                if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
                if (attempt >= RetryPolicy.Default.maxRetries) {
                    return@withContext AppResult.Err(AppError.Network(error.message.orEmpty()))
                }
            } catch (error: Exception) {
                return@withContext AppResult.Err(AppError.Unknown(error.message.orEmpty()))
            }
            attempt += 1
            forceRefreshTunnel = profile.connectionMode == ConnectionMode.SshProxy
            delay(retryDelayMillis(attempt))
        }
        AppResult.Err(AppError.Network("attachment fetch retry exhausted"))
    }

    suspend fun conversationStreamRequest(
        profile: ConnectionProfile,
        forceRefreshTunnel: Boolean = false,
    ): AppResult<Request> = homeWebSocketRequest(profile, forceRefreshTunnel)

    suspend fun homeWebSocketRequest(
        profile: ConnectionProfile,
        forceRefreshTunnel: Boolean = false,
    ): AppResult<Request> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        return withContext(Dispatchers.IO) {
            try {
                val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
                val httpUrl = baseUrl
                    .plus("/api/ws/home")
                    .toHttpUrlOrNull()
                    ?: return@withContext AppResult.Err(AppError.Network("Invalid conversation stream URL"))
                val httpUrlWithToken = httpUrl.newBuilder()
                    .addQueryParameter("token", profile.token.trim())
                    .build()
                val wsUrl = when (httpUrlWithToken.scheme) {
                    "https" -> httpUrlWithToken.toString().replaceFirst("https://", "wss://")
                    "http" -> httpUrlWithToken.toString().replaceFirst("http://", "ws://")
                    else -> return@withContext AppResult.Err(AppError.Network("Unsupported WebSocket scheme"))
                }
                AppResult.Ok(
                    Request.Builder()
                        .url(wsUrl)
                        .header("Authorization", "Bearer ${profile.token.trim()}")
                        .build(),
                )
            } catch (error: IOException) {
                if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
                AppResult.Err(AppError.Network(error.message.orEmpty()))
            } catch (error: Exception) {
                AppResult.Err(AppError.Unknown(error.message.orEmpty()))
            }
        }
    }

    suspend fun foregroundWebSocketRequest(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String = "main",
        forceRefreshTunnel: Boolean = false,
    ): AppResult<Request> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        return withContext(Dispatchers.IO) {
            try {
                val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
                val httpUrl = baseUrl
                    .plus("/api/conversations/$conversationId/foreground_sessions/${urlEncode(foregroundSessionId)}/ws")
                    .toHttpUrlOrNull()
                    ?: return@withContext AppResult.Err(AppError.Network("Invalid WebSocket URL"))
                val httpUrlWithToken = httpUrl.newBuilder()
                    .addQueryParameter("token", profile.token.trim())
                    .build()
                val wsUrl = when (httpUrlWithToken.scheme) {
                    "https" -> httpUrlWithToken.toString().replaceFirst("https://", "wss://")
                    "http" -> httpUrlWithToken.toString().replaceFirst("http://", "ws://")
                    else -> return@withContext AppResult.Err(AppError.Network("Unsupported WebSocket scheme"))
                }
                AppResult.Ok(
                    Request.Builder()
                        .url(wsUrl)
                        .header("Authorization", "Bearer ${profile.token.trim()}")
                        .build(),
                )
            } catch (error: IOException) {
                if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
                AppResult.Err(AppError.Network(error.message.orEmpty()))
            } catch (error: Exception) {
                AppResult.Err(AppError.Unknown(error.message.orEmpty()))
            }
        }
    }

    fun invalidateTunnel() {
        tunnelManager.invalidate()
    }

    private suspend fun <T> get(
        profile: ConnectionProfile,
        path: String,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "GET", body = null, retryPolicy = RetryPolicy.Default, decode = decode)

    private suspend fun <T> post(
        profile: ConnectionProfile,
        path: String,
        body: String,
        retryPolicy: RetryPolicy,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "POST", body = body, retryPolicy = retryPolicy, decode = decode)

    private suspend fun <T> patch(
        profile: ConnectionProfile,
        path: String,
        body: String,
        retryPolicy: RetryPolicy,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "PATCH", body = body, retryPolicy = retryPolicy, decode = decode)

    private suspend fun <T> delete(
        profile: ConnectionProfile,
        path: String,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "DELETE", body = null, retryPolicy = RetryPolicy.Default, decode = decode)

    private suspend fun <T> requestBytes(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: ByteArray?,
        retryPolicy: RetryPolicy,
        decode: (ByteArray, String?) -> T,
    ): AppResult<T> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        var attempt = 0
        var forceRefreshTunnel = false
        while (true) {
            val result = executeBytesRequest(profile, path, method, body, forceRefreshTunnel, decode)
            if (!result.shouldRetry(attempt) || attempt >= retryPolicy.maxRetries) return result
            if (profile.connectionMode == ConnectionMode.SshProxy) {
                tunnelManager.invalidate()
                forceRefreshTunnel = true
            }
            attempt += 1
            delay(retryDelayMillis(attempt))
        }
    }

    private suspend fun <T> request(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: String?,
        retryPolicy: RetryPolicy,
        absoluteOrRelativePath: Boolean = false,
        decode: (String) -> T,
    ): AppResult<T> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)

        var attempt = 0
        var forceRefreshTunnel = false
        while (true) {
            val result = executeRequest(profile, path, method, body, forceRefreshTunnel, absoluteOrRelativePath, decode)
            if (!result.shouldRetry(attempt) || attempt >= retryPolicy.maxRetries) return result
            if (profile.connectionMode == ConnectionMode.SshProxy) {
                tunnelManager.invalidate()
                forceRefreshTunnel = true
            }
            attempt += 1
            delay(retryDelayMillis(attempt))
        }
    }

    private suspend fun <T> executeRequest(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: String?,
        forceRefreshTunnel: Boolean,
        absoluteOrRelativePath: Boolean,
        decode: (String) -> T,
    ): AppResult<T> = withContext(Dispatchers.IO) {
        try {
            val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
            val urlText = if (absoluteOrRelativePath && (path.startsWith("http://") || path.startsWith("https://"))) {
                path
            } else {
                baseUrl.plus(if (path.startsWith('/')) path else "/$path")
            }
            val url = urlText.toHttpUrlOrNull()
                ?: return@withContext AppResult.Err(AppError.Network("Invalid server URL"))
            val builder = Request.Builder()
                .url(url)
                .header("Authorization", "Bearer ${profile.token.trim()}")
            when (method) {
                "POST" -> builder.post((body ?: "{}").toRequestBody(JsonMediaType))
                "PATCH" -> builder.patch((body ?: "{}").toRequestBody(JsonMediaType))
                "DELETE" -> builder.delete()
                else -> builder.get()
            }
            httpClient.newCall(builder.build()).execute().use { response ->
                val responseBody = response.body?.string().orEmpty()
                when {
                    response.code == 401 -> AppResult.Err(AppError.Unauthorized())
                    !response.isSuccessful -> AppResult.Err(AppError.Server(response.code, responseBody.ifBlank { response.message }))
                    else -> AppResult.Ok(decode(responseBody))
                }
            }
        } catch (error: SerializationException) {
            AppResult.Err(AppError.Decode(error.message.orEmpty()))
        } catch (error: IOException) {
            if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
            AppResult.Err(AppError.Network(error.message.orEmpty()))
        } catch (error: Exception) {
            AppResult.Err(AppError.Unknown(error.message.orEmpty()))
        }
    }

    private fun resolveBaseUrl(profile: ConnectionProfile, forceRefreshTunnel: Boolean = false): String = when (profile.connectionMode) {
        ConnectionMode.Direct -> profile.baseUrl.trim().trimEnd('/')
        ConnectionMode.SshProxy -> tunnelManager.resolveBaseUrl(profile, forceRefresh = forceRefreshTunnel).trimEnd('/')
    }

    private suspend fun <T> executeBytesRequest(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: ByteArray?,
        forceRefreshTunnel: Boolean,
        decode: (ByteArray, String?) -> T,
    ): AppResult<T> = withContext(Dispatchers.IO) {
        try {
            val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
            val url = baseUrl.plus(if (path.startsWith('/')) path else "/$path").toHttpUrlOrNull()
                ?: return@withContext AppResult.Err(AppError.Network("Invalid server URL"))
            val builder = Request.Builder()
                .url(url)
                .header("Authorization", "Bearer ${profile.token.trim()}")
            when (method) {
                "POST" -> builder.post((body ?: ByteArray(0)).toRequestBody("application/octet-stream".toMediaType()))
                else -> builder.get()
            }
            httpClient.newCall(builder.build()).execute().use { response ->
                val bytes = response.body?.bytes() ?: ByteArray(0)
                when {
                    response.code == 401 -> AppResult.Err(AppError.Unauthorized())
                    !response.isSuccessful -> AppResult.Err(AppError.Server(response.code, bytes.toString(Charsets.UTF_8).ifBlank { response.message }))
                    else -> AppResult.Ok(decode(bytes, response.header("content-type")))
                }
            }
        } catch (error: IOException) {
            if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
            AppResult.Err(AppError.Network(error.message.orEmpty()))
        } catch (error: Exception) {
            AppResult.Err(AppError.Unknown(error.message.orEmpty()))
        }
    }

    private fun urlEncode(value: String): String = java.net.URLEncoder.encode(value, Charsets.UTF_8.name())

    private fun guessMediaType(name: String): String? = when (name.substringAfterLast('.', "").lowercase()) {
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        "html", "htm" -> "text/html"
        "md", "txt", "log", "csv", "json", "xml", "kt", "rs", "js", "ts", "py", "toml", "yaml", "yml" -> "text/plain"
        else -> null
    }

    private fun <T> AppResult<T>.shouldRetry(attempt: Int): Boolean {
        if (attempt >= RetryPolicy.Default.maxRetries) return false
        return when (this) {
            is AppResult.Ok -> false
            is AppResult.Err -> when (val error = error) {
                is AppError.Network -> true
                is AppError.Server -> error.code in RetryableStatusCodes
                else -> false
            }
        }
    }

    private fun retryDelayMillis(attempt: Int): Long {
        val base = min(4_000L, 500L * (1L shl (attempt - 1).coerceAtLeast(0)))
        val jitter = Random.nextDouble(0.8, 1.2)
        return (base * jitter).toLong().coerceAtLeast(250L)
    }

    private data class RetryPolicy(val maxRetries: Int) {
        companion object {
            val Default = RetryPolicy(maxRetries = 3)
            val NoRetry = RetryPolicy(maxRetries = 0)
        }
    }

    private companion object {
        val JsonMediaType = "application/json; charset=utf-8".toMediaType()
        val RetryableStatusCodes = setOf(408, 429, 500, 502, 503, 504)

        fun defaultHttpClient(): OkHttpClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(30, TimeUnit.SECONDS)
            .writeTimeout(30, TimeUnit.SECONDS)
            .callTimeout(45, TimeUnit.SECONDS)
            .build()

        fun defaultWebSocketClient(): OkHttpClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.SECONDS)
            .writeTimeout(20, TimeUnit.SECONDS)
            .pingInterval(15, TimeUnit.SECONDS)
            .build()
    }
}
