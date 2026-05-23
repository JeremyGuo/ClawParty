package com.stellaclaw.stellacodex.ui.chat

import android.app.Application
import android.content.ContentResolver
import android.content.ContentValues
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Base64
import androidx.core.content.FileProvider
import com.stellaclaw.stellacodex.core.result.AppError
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.MessagePage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.dto.ChatMessageDto
import com.stellaclaw.stellacodex.data.dto.MessagesResponseDto
import com.stellaclaw.stellacodex.data.dto.SendMessageFileDto
import com.stellaclaw.stellacodex.data.dto.SelectionReferenceDto
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.network.NetworkMonitor
import com.stellaclaw.stellacodex.data.network.NetworkState
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageLocalState
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.net.URLDecoder
import java.io.File
import java.time.Instant
import java.util.UUID
import kotlin.math.min
import kotlin.random.Random
import kotlin.text.Charsets

class ChatViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()
    private val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    private val mutableState = MutableStateFlow(ChatUiState())
    val state: StateFlow<ChatUiState> = mutableState.asStateFlow()
    private val coroutineErrorHandler = CoroutineExceptionHandler { _, throwable ->
        logDebug("coroutine failure ${throwable::class.java.simpleName}: ${throwable.message.orEmpty()}")
        mutableState.update { it.copy(isLoading = false, isSending = false, error = throwable.message ?: "Unexpected app error") }
    }
    private var webSocket: WebSocket? = null
    private var realtimeConversationId: String = ""
    private var reconnectJob: Job? = null
    private var realtimeSyncJob: Job? = null
    private var syncJob: Job? = null
    private var pendingSyncReason: SyncReason? = null
    private var realtimeSyncInFlight: Boolean = false
    private var reconnectAttempt: Int = 0
    private var reconnectEnabled: Boolean = false
    private var latestProfile: ConnectionProfile? = null
    private var loadRequestSeq: Long = 0
    private var sawActiveTurnProgress: Boolean = false
    private val pendingSeen = mutableMapOf<String, String>()
    private val pendingSends = linkedMapOf<String, PendingSend>()
    private val pendingStreamAttachments = mutableMapOf<String, List<MessageAttachment>>()

    init {
        AppLogStore.append(application, "chat", "ChatViewModel.init")
        NetworkMonitor.start(application)
        viewModelScope.launch(coroutineErrorHandler) {
            SelectionReferenceStore.references.collect { references ->
                var consumedConversationId: String? = null
                mutableState.update { current ->
                    val incoming = references.filter { it.conversationId == current.conversationId }
                    if (incoming.isEmpty()) return@update current
                    val merged = current.selectionReferences + incoming.filterNot { reference ->
                        current.selectionReferences.any { it.path == reference.path }
                    }
                    consumedConversationId = current.conversationId
                    current.copy(selectionReferences = merged)
                }
                consumedConversationId?.let(SelectionReferenceStore::consume)
            }
        }
        viewModelScope.launch(coroutineErrorHandler) {
            NetworkMonitor.state.collect { networkState ->
                handleNetworkState(networkState)
            }
        }
    }

    private fun handleNetworkState(networkState: NetworkState) {
        val conversationId = state.value.conversationId
        logDebug("network state=$networkState conversation=${conversationId.take(12)}")
        when (networkState) {
            NetworkState.Available -> {
                val profile = latestProfile
                if (profile?.connectionMode == ConnectionMode.SshProxy) {
                    api.invalidateTunnel()
                }
                if (conversationId.isNotBlank()) {
                    mutableState.update { it.copy(realtimeState = "Network restored; syncing...") }
                    viewModelScope.launch(coroutineErrorHandler) {
                        val activeProfile = profile ?: store.profile.first().also { latestProfile = it }
                        if (webSocket == null && reconnectEnabled) {
                            connectRealtime(activeProfile, conversationId, state.value.foregroundSessionId, forceRefreshTunnel = true)
                        }
                        requestSync(conversationId, activeProfile, SyncReason.NetworkRestored, updateStatusWhenIdle = true)
                        flushPendingSeen(activeProfile)
                        retryPendingSends(activeProfile)
                    }
                }
            }
            NetworkState.Lost, NetworkState.Unavailable -> {
                if (conversationId.isNotBlank()) {
                    mutableState.update { it.copy(realtimeState = "Offline; will sync when network returns") }
                }
            }
        }
    }

    fun load(conversationId: String, foregroundSessionId: String = "main") {
        if (conversationId.isBlank()) return
        val sessionId = foregroundSessionId.ifBlank { "main" }
        logDebug("load conversation=${conversationId.take(12)} foreground=$sessionId")
        if (state.value.conversationId == conversationId && state.value.foregroundSessionId == sessionId && webSocket != null) return
        val requestSeq = ++loadRequestSeq
        cacheCurrentConversation()
        closeRealtime(allowReconnect = false)
        mutableState.update {
            it.copy(
                conversationId = conversationId,
                foregroundSessionId = sessionId,
                displayName = conversationId,
                messages = emptyList(),
                loadedOffset = 0,
                totalMessages = 0,
                isLoading = true,
                realtimeState = "Connecting realtime...",
                progressTitle = null,
                progressDetail = null,
                attachmentPreviews = emptyMap(),
                selectionReferences = SelectionReferenceStore.consume(conversationId),
            )
        }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId || state.value.foregroundSessionId != sessionId) return@launch
            latestProfile = profile
            val cached = ConversationRuntimeCache.get(profile, conversationId, sessionId)
            if (cached != null) {
                mutableState.update {
                    it.copy(
                        displayName = cached.displayName.ifBlank { conversationId },
                        isLoading = false,
                        messages = cached.messages,
                        loadedOffset = cached.loadedOffset,
                        totalMessages = cached.totalMessages,
                        error = null,
                    )
                }
                connectRealtime(profile, conversationId, sessionId)
                markConversationSeen(profile, conversationId, sessionId, cached.totalMessages)
            } else {
                refresh(connectRealtimeAfterLoad = true)
            }
        }
    }

    fun onDraftChanged(value: String) {
        mutableState.update { it.copy(draft = value, error = null) }
    }

    fun addAttachments(uris: List<Uri>) {
        if (uris.isEmpty()) return
        val resolver = getApplication<Application>().contentResolver
        val incoming = uris.map { uri -> pendingAttachmentFromUri(uri, resolver) }
        mutableState.update { state ->
            val existingUris = state.pendingAttachments.map { it.uri }.toSet()
            val merged = state.pendingAttachments + incoming.filterNot { it.uri in existingUris }
            val totalBytes = merged.sumOf { it.sizeBytes ?: 0L }
            if (totalBytes > MaxAttachmentBytes) {
                state.copy(error = "Attachments are limited to ${formatBytes(MaxAttachmentBytes)} total")
            } else {
                state.copy(pendingAttachments = merged, error = null)
            }
        }
    }

    fun removeAttachment(uri: String) {
        mutableState.update {
            it.copy(pendingAttachments = it.pendingAttachments.filterNot { attachment -> attachment.uri == uri })
        }
    }

    fun removeSelectionReference(path: String) {
        mutableState.update {
            it.copy(selectionReferences = it.selectionReferences.filterNot { reference -> reference.path == path })
        }
    }

    fun previewAttachment(attachment: MessageAttachment) {
        previewAttachment(attachment, scopedPreviewKey(attachment.previewKey()))
    }

    private fun previewAttachment(attachment: MessageAttachment, key: String) {
        val current = state.value.attachmentPreviews[key]
        if (current?.isLoading == true || current?.hasContent == true || current?.error != null) return
        val conversationId = state.value.conversationId
        val foregroundSessionId = state.value.foregroundSessionId
        if (conversationId.isBlank()) return
        viewModelScope.launch(coroutineErrorHandler) {
            mutableState.update {
                if (it.conversationId != conversationId || it.foregroundSessionId != foregroundSessionId) return@update it
                it.copy(
                    attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(isLoading = true)),
                )
            }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = fetchAttachmentContent(profile, attachment, forDownload = false)) {
                is AppResult.Ok -> {
                    val mediaType = attachment.mediaType ?: result.value.mediaType.orEmpty()
                    val image = if (attachment.kind == "image" || mediaType.startsWith("image/")) {
                        BitmapFactory.decodeByteArray(result.value.bytes, 0, result.value.bytes.size)
                    } else {
                        null
                    }
                    val text = if (image == null && isTextAttachment(mediaType, attachment.name, result.value.bytes.size)) {
                        result.value.bytes.toString(Charsets.UTF_8).take(16_000)
                    } else {
                        null
                    }
                    mutableState.update {
                        if (it.conversationId != conversationId || it.foregroundSessionId != foregroundSessionId) return@update it
                        it.copy(
                            attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(
                                isLoading = false,
                                image = image,
                                text = text,
                                detail = if (image == null && text == null) "Loaded ${formatBytes(result.value.bytes.size.toLong())}" else null,
                            )),
                        )
                    }
                }
                is AppResult.Err -> mutableState.update {
                    if (it.conversationId != conversationId || it.foregroundSessionId != foregroundSessionId) return@update it
                    it.copy(
                        attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(
                            isLoading = false,
                            error = result.error.userMessage(),
                        )),
                    )
                }
            }
        }
    }

    fun previewAttachments(attachments: List<MessageAttachment>) {
        attachments.filter(::shouldAutoPreviewAttachment).forEach(::previewAttachment)
    }

    fun downloadAttachment(attachment: MessageAttachment) {
        viewModelScope.launch(coroutineErrorHandler) {
            mutableState.update { it.copy(realtimeState = "Downloading ${attachment.displayName()}...") }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = fetchAttachmentContent(profile, attachment, forDownload = true)) {
                is AppResult.Ok -> {
                    val saved = saveAttachmentToDownloads(attachment.displayName(), result.value.bytes, attachment.mediaType ?: result.value.mediaType ?: "application/octet-stream")
                    mutableState.update { it.copy(realtimeState = "Saved $saved") }
                }
                is AppResult.Err -> mutableState.update { it.copy(realtimeState = "Download failed: ${result.error.userMessage()}") }
            }
        }
    }

    fun openAttachment(attachment: MessageAttachment) {
        viewModelScope.launch(coroutineErrorHandler) {
            mutableState.update { it.copy(realtimeState = "Opening ${attachment.displayName()}...") }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = fetchAttachmentContent(profile, attachment, forDownload = true)) {
                is AppResult.Ok -> {
                    val app = getApplication<Application>()
                    val mediaType = attachment.mediaType ?: result.value.mediaType ?: "application/octet-stream"
                    val file = app.cacheDir.resolve("attachments").apply { mkdirs() }.resolve(attachment.displayName().safeFileName())
                    file.writeBytes(result.value.bytes)
                    val uri = FileProvider.getUriForFile(app, "${app.packageName}.fileprovider", file)
                    val intent = Intent(Intent.ACTION_VIEW)
                        .setDataAndType(uri, mediaType)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    runCatching { app.startActivity(intent) }
                        .onSuccess { mutableState.update { it.copy(realtimeState = "Opened ${attachment.displayName()}") } }
                        .onFailure { error -> mutableState.update { it.copy(realtimeState = "Open failed: ${error.message.orEmpty()}") } }
                }
                is AppResult.Err -> mutableState.update { it.copy(realtimeState = "Open failed: ${result.error.userMessage()}") }
            }
        }
    }

    private suspend fun fetchAttachmentContent(
        profile: ConnectionProfile,
        attachment: MessageAttachment,
        forDownload: Boolean,
    ): AppResult<com.stellaclaw.stellacodex.data.api.FetchedAttachment> {
        inlineAttachmentContent(attachment)?.let { return it }
        val url = attachment.loadUrl(forDownload)
        if (url.isBlank()) return AppResult.Err(AppError.Network("附件没有可预览内容"))
        return api.fetchAttachment(profile, url)
    }

    private fun inlineAttachmentContent(attachment: MessageAttachment): AppResult.Ok<com.stellaclaw.stellacodex.data.api.FetchedAttachment>? {
        val mediaType = attachment.mediaType ?: mediaTypeFromDataUrl(attachment.dataUrl)
        if (attachment.dataUrl.startsWith("data:")) {
            val comma = attachment.dataUrl.indexOf(',')
            if (comma > 0) {
                val header = attachment.dataUrl.substring(0, comma)
                val raw = attachment.dataUrl.substring(comma + 1)
                val bytes = if (header.contains(";base64", ignoreCase = true)) Base64.decode(raw, Base64.DEFAULT) else URLDecoder.decode(raw, Charsets.UTF_8.name()).toByteArray(Charsets.UTF_8)
                return AppResult.Ok(com.stellaclaw.stellacodex.data.api.FetchedAttachment(bytes, mediaType))
            }
        }
        attachment.dataBase64.takeIf { it.isNotBlank() }?.let {
            return AppResult.Ok(com.stellaclaw.stellacodex.data.api.FetchedAttachment(Base64.decode(it, Base64.DEFAULT), mediaType))
        }
        attachment.data.takeIf { it.isNotBlank() }?.let {
            val bytes = if (attachment.encoding.equals("base64", ignoreCase = true)) Base64.decode(it, Base64.DEFAULT) else it.toByteArray(Charsets.UTF_8)
            return AppResult.Ok(com.stellaclaw.stellacodex.data.api.FetchedAttachment(bytes, mediaType))
        }
        if (attachment.uri.startsWith("content://") || attachment.uri.startsWith("file://")) {
            val maxInlineBytes = 32L * 1024L * 1024L
            val size = attachment.sizeBytes ?: 0L
            if (size > maxInlineBytes) return null
            val app = getApplication<Application>()
            val bytes = app.contentResolver.openInputStream(Uri.parse(attachment.uri))?.use { stream ->
                stream.readBytesLimited(maxInlineBytes)
            }
            if (bytes != null) return AppResult.Ok(com.stellaclaw.stellacodex.data.api.FetchedAttachment(bytes, mediaType))
        }
        return null
    }

    private fun MessageAttachment.loadUrl(forDownload: Boolean): String = if (forDownload) {
        listOf(downloadUrl, previewUrl, url, uri, fileUri, src).firstOrNull { it.isFetchableAttachmentUrl() }.orEmpty()
    } else {
        listOf(previewUrl, url, uri, fileUri, src).firstOrNull { it.isFetchableAttachmentUrl() }.orEmpty()
    }

    private fun String.isFetchableAttachmentUrl(): Boolean {
        if (isBlank()) return false
        return startsWith("/") || startsWith("http://") || startsWith("https://")
    }

    private fun shouldAutoPreviewAttachment(attachment: MessageAttachment): Boolean {
        if (!attachment.hasPreviewSource()) return false
        val mediaType = attachment.mediaType.orEmpty()
        val size = attachment.sizeBytes ?: 0L
        return attachment.kind == "image" || mediaType.startsWith("image/") || isTextAttachment(mediaType, attachment.name, size.coerceAtMost(Int.MAX_VALUE.toLong()).toInt())
    }

    private fun MessageAttachment.hasPreviewSource(): Boolean =
        inlinePreviewSourceAvailable() || loadUrl(forDownload = false).isNotBlank()

    private fun MessageAttachment.inlinePreviewSourceAvailable(): Boolean =
        dataUrl.isNotBlank() || dataBase64.isNotBlank() || data.isNotBlank() || uri.startsWith("content://") || uri.startsWith("file://")

    private fun java.io.InputStream.readBytesLimited(maxBytes: Long): ByteArray? {
        val output = java.io.ByteArrayOutputStream()
        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
        var total = 0L
        while (true) {
            val read = read(buffer)
            if (read < 0) break
            total += read
            if (total > maxBytes) return null
            output.write(buffer, 0, read)
        }
        return output.toByteArray()
    }

    private fun mediaTypeFromDataUrl(value: String): String? = value
        .takeIf { it.startsWith("data:") }
        ?.substringAfter("data:")
        ?.substringBefore(';')
        ?.substringBefore(',')
        ?.takeIf { it.isNotBlank() }

    private fun saveAttachmentToDownloads(fileName: String, bytes: ByteArray, mediaType: String): String {
        val app = getApplication<Application>()
        val resolver = app.contentResolver
        val safeName = fileName.safeFileName()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, safeName)
                put(MediaStore.Downloads.MIME_TYPE, mediaType)
                put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/StellacodeX/attachments")
            }
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: error("Unable to create download")
            resolver.openOutputStream(uri)?.use { it.write(bytes) } ?: error("Unable to write download")
            return "Downloads/StellacodeX/attachments/$safeName"
        }
        val dir = File(app.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS), "StellacodeX/attachments").apply { mkdirs() }
        File(dir, safeName).writeBytes(bytes)
        return File(dir, safeName).absolutePath
    }

    private fun MessageAttachment.displayName(): String = name.ifBlank {
        listOf(path, filePath, workspacePath, relativePath, url, uri, fileUri, src)
            .firstOrNull { it.isNotBlank() }
            ?.substringBefore('?')
            ?.trimEnd('/')
            ?.substringAfterLast('/')
            ?.ifBlank { null }
            ?: "attachment-$index"
    }

    private fun String.safeFileName(): String = replace(Regex("[\\\\/:*?\"<>|]"), "_").ifBlank { "attachment" }

    fun refresh(connectRealtimeAfterLoad: Boolean = false, showLoading: Boolean = true) {
        val conversationId = state.value.conversationId
        if (conversationId.isBlank()) return
        val requestSeq = loadRequestSeq
        viewModelScope.launch(coroutineErrorHandler) {
            if (showLoading) {
                mutableState.update { it.copy(isLoading = true, error = null) }
            } else {
                mutableState.update { it.copy(error = null) }
            }
            val profile = store.profile.first()
            latestProfile = profile
            val foregroundSessionId = state.value.foregroundSessionId.ifBlank { "main" }
            updateConversationTitle(profile, conversationId)
            when (val result = loadLatestVisibleMessages(profile, conversationId, foregroundSessionId)) {
                is AppResult.Ok -> {
                    if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId) return@launch
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            messages = mergeMessages(it.messages, result.value.messages),
                            loadedOffset = mergedLoadedOffset(it.messages, it.loadedOffset, result.value),
                            totalMessages = result.value.total,
                            error = null,
                        )
                    }
                    markConversationSeen(profile, conversationId, foregroundSessionId, result.value.total)
                    cacheCurrentConversation()
                    if (connectRealtimeAfterLoad) {
                        connectRealtime(profile, conversationId, foregroundSessionId)
                    }
                }
                is AppResult.Err -> {
                    if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId) return@launch
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            error = result.error.userMessage(),
                            realtimeState = "Realtime unavailable; use Refresh",
                        )
                    }
                    if (connectRealtimeAfterLoad) {
                        connectRealtime(profile, conversationId, foregroundSessionId)
                    }
                }
            }
        }
    }

    private suspend fun loadLatestVisibleMessages(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String,
    ): AppResult<MessagePage> {
        val firstPage = when (val result = api.loadMessagePage(profile, conversationId, foregroundSessionId, offset = 0, limit = 1)) {
            is AppResult.Err -> return result
            is AppResult.Ok -> result.value
        }
        val total = firstPage.total
        if (total == 0) return AppResult.Ok(firstPage)

        var nextEnd = total
        var loadedOffset = total
        var loadedMessages = emptyList<ChatMessage>()
        while (nextEnd > 0 && loadedMessages.visibleTimelineItemCount() < VisibleTimelineItemTarget) {
            val nextOffset = (nextEnd - MessagePageFetchLimit).coerceAtLeast(0)
            val page = when (val result = api.loadMessagePage(
                profile = profile,
                conversationId = conversationId,
                foregroundSessionId = foregroundSessionId,
                offset = nextOffset,
                limit = nextEnd - nextOffset,
            )) {
                is AppResult.Err -> return result
                is AppResult.Ok -> result.value
            }
            loadedMessages = mergeMessages(page.messages, loadedMessages)
            loadedOffset = page.offset
            nextEnd = nextOffset
            if (page.messages.isEmpty()) break
        }
        return AppResult.Ok(
            MessagePage(
                offset = loadedOffset,
                limit = loadedMessages.size,
                total = total,
                messages = loadedMessages,
            ),
        )
    }

    private suspend fun loadEarlierVisibleMessages(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String,
        startOffset: Int,
    ): AppResult<MessagePage> {
        var nextEnd = startOffset
        var loadedOffset = startOffset
        var loadedMessages = emptyList<ChatMessage>()
        var total = state.value.totalMessages
        while (nextEnd > 0 && loadedMessages.visibleTimelineItemCount() < VisibleTimelineItemTarget) {
            val nextOffset = (nextEnd - MessagePageFetchLimit).coerceAtLeast(0)
            val page = when (val result = api.loadMessagePage(
                profile = profile,
                conversationId = conversationId,
                foregroundSessionId = foregroundSessionId,
                offset = nextOffset,
                limit = nextEnd - nextOffset,
            )) {
                is AppResult.Err -> return result
                is AppResult.Ok -> result.value
            }
            loadedMessages = mergeMessages(page.messages, loadedMessages)
            loadedOffset = page.offset
            total = page.total
            nextEnd = nextOffset
            if (page.messages.isEmpty()) break
        }
        return AppResult.Ok(
            MessagePage(
                offset = loadedOffset,
                limit = loadedMessages.size,
                total = total,
                messages = loadedMessages,
            ),
        )
    }

    fun loadEarlier() {
        val snapshot = state.value
        val conversationId = snapshot.conversationId
        if (conversationId.isBlank() || snapshot.loadedOffset <= 0 || snapshot.isLoadingEarlier) return
        viewModelScope.launch(coroutineErrorHandler) {
            mutableState.update { it.copy(isLoadingEarlier = true, error = null) }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = loadEarlierVisibleMessages(profile, conversationId, snapshot.foregroundSessionId, snapshot.loadedOffset)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isLoadingEarlier = false,
                            messages = mergeMessages(result.value.messages, it.messages),
                            loadedOffset = result.value.offset,
                            totalMessages = result.value.total,
                            error = null,
                        )
                    }
                    cacheCurrentConversation()
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(isLoadingEarlier = false, error = result.error.userMessage())
                }
            }
        }
    }

    fun send() {
        val current = state.value
        val text = current.draft.trim()
        val attachments = current.pendingAttachments
        val selectionReferences = current.selectionReferences
        logDebug("send requested conversation=${current.conversationId.take(12)} text_chars=${text.length} attachments=${attachments.size} selections=${selectionReferences.size}")
        if (current.conversationId.isBlank() || (text.isEmpty() && attachments.isEmpty() && selectionReferences.isEmpty()) || current.isSending) return
        val baselineIndex = lastServerMessageIndex(current.messages) ?: -1
        val remoteMessageId = "android-${UUID.randomUUID()}"
        val localId = remoteMessageId
        val clientMessageTime = Instant.now().toString()
        val senderName = latestProfile?.userName?.ifBlank { "workspace-user" } ?: "workspace-user"
        val optimistic = ChatMessage(
            id = localId,
            index = nextLocalIndex(current.messages),
            role = "user",
            text = text,
            preview = text,
            userName = senderName,
            messageTime = clientMessageTime,
            attachmentCount = attachments.size,
            attachments = attachments.toOptimisticAttachments(localId),
            items = emptyList(),
            hasAttachmentErrors = false,
            hasTokenUsage = false,
            localState = MessageLocalState.Sending,
            clientMessageId = remoteMessageId,
        )
        mutableState.update {
            it.copy(
                draft = "",
                pendingAttachments = emptyList(),
                selectionReferences = emptyList(),
                isSending = true,
                error = null,
                messages = it.messages + optimistic,
            )
        }
        cacheCurrentConversation()
        viewModelScope.launch(coroutineErrorHandler) {
            val profile: ConnectionProfile = store.profile.first()
            latestProfile = profile
            val files = try {
                attachments.map { it.toSendFile() }
            } catch (error: Exception) {
                mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        error = "Failed to read attachment: ${error.message.orEmpty()}",
                        pendingAttachments = attachments,
                        selectionReferences = selectionReferences,
                        messages = state.messages.filterNot { message -> message.id == localId },
                    )
                }
                return@launch
            }
            when (val result = api.sendMessage(
                profile = profile,
                conversationId = current.conversationId,
                foregroundSessionId = current.foregroundSessionId,
                text = text,
                files = files,
                selectionReferences = selectionReferences.map { it.toDto() },
                remoteMessageId = remoteMessageId,
                messageTime = clientMessageTime,
            )) {
                is AppResult.Ok -> {
                    pendingSends.remove(localId)
                    mutableState.update { state ->
                        state.copy(
                            isSending = false,
                            messages = state.messages.map { message ->
                                if (message.id == localId) {
                                    message.copy(localState = MessageLocalState.Sending)
                                } else {
                                    message
                                }
                            },
                        )
                    }
                    cacheCurrentConversation()
                    AgentCompletionService.watch(
                        context = getApplication<Application>(),
                        conversationId = current.conversationId,
                        baselineIndex = baselineIndex,
                    )
                    delay(350)
                    refresh()
                }
                is AppResult.Err -> {
                    pendingSends[localId] = PendingSend(
                        localId = localId,
                        remoteMessageId = remoteMessageId,
                        conversationId = current.conversationId,
                        foregroundSessionId = current.foregroundSessionId,
                        text = text,
                        files = files,
                        selectionReferences = selectionReferences.map { it.toDto() },
                        baselineIndex = baselineIndex,
                        messageTime = clientMessageTime,
                    )
                    mutableState.update { state ->
                        state.copy(
                            isSending = false,
                            error = result.error.userMessage(),
                            pendingAttachments = attachments,
                            messages = state.messages.map { message ->
                                if (message.id == localId) {
                                    message.copy(localState = MessageLocalState.Failed)
                                } else {
                                    message
                                }
                            },
                        )
                    }
                }
            }
        }
    }

    fun retrySend(localId: String) {
        val pending = pendingSends[localId] ?: return
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            sendPending(profile, pending)
        }
    }

    private suspend fun retryPendingSends(profile: ConnectionProfile) {
        pendingSends.values.toList().forEach { pending ->
            if (state.value.conversationId == pending.conversationId) {
                sendPending(profile, pending)
            }
        }
    }

    private suspend fun sendPending(profile: ConnectionProfile, pending: PendingSend) {
        mutableState.update { state ->
            state.copy(
                isSending = true,
                error = null,
                messages = state.messages.map { message ->
                    if (message.id == pending.localId) message.copy(localState = MessageLocalState.Sending) else message
                },
            )
        }
        when (val result = api.sendMessage(
            profile = profile,
            conversationId = pending.conversationId,
            foregroundSessionId = pending.foregroundSessionId,
            text = pending.text,
            files = pending.files,
            selectionReferences = pending.selectionReferences,
            remoteMessageId = pending.remoteMessageId,
            messageTime = pending.messageTime,
        )) {
            is AppResult.Ok -> {
                pendingSends.remove(pending.localId)
                mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        messages = state.messages.map { message ->
                            if (message.id == pending.localId) message.copy(localState = MessageLocalState.Sending) else message
                        },
                    )
                }
                cacheCurrentConversation()
                AgentCompletionService.watch(
                    context = getApplication<Application>(),
                    conversationId = pending.conversationId,
                    baselineIndex = pending.baselineIndex,
                )
                delay(350)
                refresh(showLoading = false)
            }
            is AppResult.Err -> {
                mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        error = result.error.userMessage(),
                        messages = state.messages.map { message ->
                            if (message.id == pending.localId) message.copy(localState = MessageLocalState.Failed) else message
                        },
                    )
                }
            }
        }
    }

    private suspend fun connectRealtime(
        profile: ConnectionProfile,
        conversationId: String,
        foregroundSessionId: String,
        forceRefreshTunnel: Boolean = false,
    ) {
        reconnectEnabled = true
        latestProfile = profile
        reconnectJob?.cancel()
        logRealtime("connect requested conversation=$conversationId foreground=$foregroundSessionId mode=${profile.connectionMode}")
        when (val result = api.foregroundWebSocketRequest(profile, conversationId, foregroundSessionId, forceRefreshTunnel = forceRefreshTunnel)) {
            is AppResult.Ok -> {
                realtimeConversationId = conversationId
                startRealtimeBackfill(conversationId)
                val previousSocket = webSocket
                webSocket = null
                previousSocket?.cancel()
                val request = result.value
                logRealtime("opening websocket conversation=$conversationId url=${request.url.scheme}://${request.url.host}:${request.url.port}${request.url.encodedPath}?token=<redacted>")
                webSocket = api.webSocketClient.newWebSocket(request, ChatWebSocketListener(conversationId))
            }
            is AppResult.Err -> {
                val message = result.error.userMessage()
                logRealtime("websocket request failed conversation=$conversationId error=$message")
                mutableState.update { it.copy(realtimeState = message) }
                scheduleReconnect(conversationId)
            }
        }
    }

    private fun scheduleReconnect(conversationId: String) {
        if (!reconnectEnabled || conversationId != realtimeConversationId && realtimeConversationId.isNotBlank()) return
        if (!NetworkMonitor.isAvailable()) {
            reconnectJob?.cancel()
            mutableState.update { it.copy(realtimeState = "Offline; will reconnect when network returns") }
            logRealtime("skip reconnect while offline conversation=$conversationId")
            return
        }
        reconnectJob?.cancel()
        val baseDelay = min(30_000L, 1_000L * (1 shl min(reconnectAttempt, 5)))
        val delayMillis = (baseDelay * Random.nextDouble(0.8, 1.2)).toLong().coerceAtLeast(500L)
        reconnectAttempt += 1
        logRealtime("schedule reconnect conversation=$conversationId delay=${delayMillis}ms attempt=$reconnectAttempt")
        mutableState.update { it.copy(realtimeState = "Realtime reconnecting in ${delayMillis / 1000}s") }
        reconnectJob = viewModelScope.launch {
            delay(delayMillis)
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            if (state.value.conversationId == conversationId && reconnectEnabled) {
                mutableState.update { it.copy(realtimeState = "Reconnecting realtime...") }
                connectRealtime(profile, conversationId, state.value.foregroundSessionId)
            }
        }
    }

    private fun closeRealtime(allowReconnect: Boolean) {
        reconnectEnabled = allowReconnect
        reconnectJob?.cancel()
        reconnectJob = null
        logRealtime("close websocket allowReconnect=$allowReconnect conversation=$realtimeConversationId")
        if (!allowReconnect) {
            realtimeSyncJob?.cancel()
            realtimeSyncJob = null
            syncJob?.cancel()
            syncJob = null
            pendingSyncReason = null
            realtimeSyncInFlight = false
            pendingStreamAttachments.clear()
        }
        webSocket?.close(1000, "conversation changed")
        webSocket = null
        if (!allowReconnect) {
            realtimeConversationId = ""
            reconnectAttempt = 0
        }
    }

    private fun applyIncomingMessages(incoming: List<ChatMessage>) {
        if (incoming.isEmpty()) return
        mutableState.update { current ->
            current.copy(messages = mergeMessages(removeStreamingMessagesForCanonical(current.messages, incoming), incoming))
        }
        cacheCurrentConversation()
    }

    private fun cacheCurrentConversation() {
        val profile = latestProfile ?: return
        val snapshot = state.value
        if (snapshot.conversationId.isBlank()) return
        ConversationRuntimeCache.put(
            profile = profile,
            conversationId = snapshot.conversationId,
            foregroundSessionId = snapshot.foregroundSessionId,
            snapshot = CachedChatSnapshot(
                displayName = snapshot.displayName,
                messages = snapshot.messages.filterNot { it.localState == MessageLocalState.Streaming },
                loadedOffset = snapshot.loadedOffset,
                totalMessages = snapshot.totalMessages,
            ),
        )
    }

    private fun List<ChatMessage>.visibleTimelineItemCount(): Int {
        var count = 0
        var pendingToolMessageCount = 0
        for (message in filterNot { it.isRuntimeMetadataMessage() }) {
            if (message.isToolOnlyMessage()) {
                pendingToolMessageCount += 1
            } else {
                if (pendingToolMessageCount > 0) {
                    count += 1
                    pendingToolMessageCount = 0
                }
                count += 1
            }
        }
        count += pendingToolMessageCount
        return count
    }

    private fun ChatMessage.isToolOnlyMessage(): Boolean =
        role.equals("assistant", ignoreCase = true) &&
            text.isBlank() &&
            attachments.isEmpty() &&
            items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }

    private fun ChatMessage.isRuntimeMetadataMessage(): Boolean {
        val body = text.ifBlank { preview }.trimStart()
        return body.startsWith("[Incoming User Metadata]") ||
            body.startsWith("[Incoming Assistant Metadata]") ||
            body.startsWith("[Incoming System Metadata]")
    }

    private fun mergeMessages(existing: List<ChatMessage>, incoming: List<ChatMessage>): List<ChatMessage> {
        val syncedIncoming = incoming.map { remote ->
            remote.copy(localState = MessageLocalState.Synced)
        }
        val byId = linkedMapOf<String, ChatMessage>()
        val sendingMatches = syncedIncoming.associateWith { remote ->
            existing.firstOrNull { local ->
                local.localState == MessageLocalState.Sending && shouldDropSendingForCanonical(local, remote)
            }
        }
        existing.filterNot { local ->
            local.localState == MessageLocalState.Streaming && syncedIncoming.any { remote -> remote.id == local.id || shouldDropStreamingForCanonical(local, remote) } ||
                local.localState == MessageLocalState.Sending && syncedIncoming.any { remote -> shouldDropSendingForCanonical(local, remote) }
        }.forEach { byId[it.id] = it }
        syncedIncoming.forEach { remote ->
            val local = sendingMatches[remote]
            val merged = if (local != null) remote.withOptimisticAttachmentFallback(local) else remote
            byId[merged.id] = merged
        }
        return byId.values.sortedWith(compareBy<ChatMessage> { it.index }.thenBy { it.id })
    }

    private fun shouldDropSendingForCanonical(local: ChatMessage, remote: ChatMessage): Boolean {
        if (local.id.isNotBlank() && local.id == remote.id) return true
        if (local.clientMessageId.isNotBlank() && local.clientMessageId == remote.clientMessageId) return true
        if (local.id.isNotBlank() && local.id == remote.clientMessageId) return true
        if (local.clientMessageId.isNotBlank() && local.clientMessageId == remote.id) return true
        return false
    }

    private fun ChatMessage.withOptimisticAttachmentFallback(local: ChatMessage): ChatMessage {
        if (local.attachments.isEmpty()) return this
        val canonical = attachments
        if (canonical.isEmpty()) {
            return copy(attachments = local.attachments, attachmentCount = maxOf(attachmentCount, local.attachments.size))
        }
        val merged = canonical.mapIndexed { index, attachment ->
            if (attachment.hasPreviewSource()) {
                attachment
            } else {
                val localAttachment = local.attachments.getOrNull(index)
                    ?: local.attachments.firstOrNull { it.name == attachment.name && it.mediaType == attachment.mediaType }
                attachment.withPreviewSourceFrom(localAttachment)
            }
        }
        return copy(attachments = merged, attachmentCount = maxOf(attachmentCount, merged.size))
    }

    private fun MessageAttachment.withPreviewSourceFrom(local: MessageAttachment?): MessageAttachment {
        if (local == null || !local.hasPreviewSource()) return this
        return copy(
            uri = uri.ifBlank { local.uri },
            fileUri = fileUri.ifBlank { local.fileUri },
            dataUrl = dataUrl.ifBlank { local.dataUrl },
            dataBase64 = dataBase64.ifBlank { local.dataBase64 },
            data = data.ifBlank { local.data },
            encoding = encoding.ifBlank { local.encoding },
        )
    }

    private fun List<PendingAttachmentUiState>.toOptimisticAttachments(messageId: String): List<MessageAttachment> = mapIndexed { index, attachment ->
        MessageAttachment(
            id = "$messageId-att-$index",
            index = index,
            kind = if (attachment.mediaType?.startsWith("image/") == true) "image" else "document",
            name = attachment.name.ifBlank { "attachment-${index + 1}" },
            mediaType = attachment.mediaType,
            sizeBytes = attachment.sizeBytes,
            url = "",
            uri = attachment.uri,
        )
    }

    private fun mergedLoadedOffset(currentMessages: List<ChatMessage>, currentOffset: Int, page: MessagePage): Int = when {
        page.messages.isEmpty() -> currentOffset
        currentMessages.isEmpty() -> page.offset
        else -> min(currentOffset, page.offset)
    }

    private suspend fun updateConversationTitle(profile: ConnectionProfile, conversationId: String) {
        when (val result = api.loadConversations(profile, limit = 200)) {
            is AppResult.Ok -> {
                val summary = result.value.firstOrNull { it.conversationId == conversationId }
                    ?.forForegroundSession(state.value.foregroundSessionId)
                val displayName = summary
                    ?.displayName
                    ?.takeIf(String::isNotBlank)
                    ?: conversationId
                mutableState.update { it.copy(displayName = displayName, conversationSummary = summary) }
            }
            is AppResult.Err -> Unit
        }
    }

    private fun ConversationSummary.forForegroundSession(foregroundSessionId: String): ConversationSummary {
        val sessionId = foregroundSessionId.ifBlank { "main" }
        val session = foregroundSessions.firstOrNull { it.id == sessionId } ?: return this
        return copy(
            foregroundSessionId = session.id,
            processingState = session.state,
            running = session.running,
            messageCount = session.messageCount,
            lastMessageId = session.lastMessageId,
            lastMessageTime = session.lastMessageTime,
            lastSeenMessageId = session.lastSeenMessageId,
            lastSeenAt = session.lastSeenAt,
        )
    }

    private fun markConversationSeen(profile: ConnectionProfile, conversationId: String, foregroundSessionId: String, totalMessages: Int) {
        val lastSeenMessageId = state.value.messages
            .filter { it.localState == MessageLocalState.Synced && it.id.isNotBlank() && it.index < totalMessages }
            .maxByOrNull { it.index }
            ?.id
            ?: return
        pendingSeen[seenKey(conversationId, foregroundSessionId)] = lastSeenMessageId
        viewModelScope.launch(coroutineErrorHandler) {
            flushPendingSeen(profile)
        }
    }

    private suspend fun flushPendingSeen(profile: ConnectionProfile) {
        val pending = pendingSeen.toMap()
        pending.forEach { (key, lastSeenMessageId) ->
            val (conversationId, foregroundSessionId) = splitSeenKey(key)
            when (val result = api.markConversationSeen(profile, conversationId, lastSeenMessageId, foregroundSessionId)) {
                is AppResult.Ok -> {
                    if (pendingSeen[key] == lastSeenMessageId) {
                        pendingSeen.remove(key)
                    }
                }
                is AppResult.Err -> logRealtime("mark seen failed conversation=$conversationId foreground=$foregroundSessionId error=${result.error.userMessage()}")
            }
        }
    }

    private fun seenKey(conversationId: String, foregroundSessionId: String): String = "$conversationId|${foregroundSessionId.ifBlank { "main" }}"

    private fun splitSeenKey(value: String): Pair<String, String> {
        val index = value.lastIndexOf('|')
        return if (index < 0) value to "main" else value.substring(0, index) to value.substring(index + 1).ifBlank { "main" }
    }

    private fun lastServerMessageIndex(messages: List<ChatMessage>): Int? = messages
        .asReversed()
        .firstOrNull { it.localState == MessageLocalState.Synced && it.index >= 0 }
        ?.index

    private fun requestSync(
        conversationId: String,
        profile: ConnectionProfile,
        reason: SyncReason,
        updateStatusWhenIdle: Boolean,
    ) {
        if (syncJob?.isActive == true) {
            pendingSyncReason = reason
            logDebug("sync queued conversation=${conversationId.take(12)} reason=$reason")
            return
        }
        logDebug("sync requested conversation=${conversationId.take(12)} reason=$reason")
        syncJob = viewModelScope.launch {
            syncMissingMessages(conversationId, profile, updateStatusWhenIdle = updateStatusWhenIdle)
            val nextReason = pendingSyncReason
            pendingSyncReason = null
            if (nextReason != null && state.value.conversationId == conversationId && reconnectEnabled) {
                syncMissingMessages(conversationId, profile, updateStatusWhenIdle = true)
            }
        }
    }

    private fun startRealtimeBackfill(conversationId: String) {
        realtimeSyncJob?.cancel()
        realtimeSyncJob = viewModelScope.launch {
            while (state.value.conversationId == conversationId && reconnectEnabled) {
                delay(2_000)
                val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                requestSync(conversationId, profile, SyncReason.PeriodicBackfill, updateStatusWhenIdle = false)
            }
        }
    }

    private suspend fun syncMissingMessages(
        conversationId: String,
        profile: ConnectionProfile,
        updateStatusWhenIdle: Boolean,
    ) {
        if (realtimeSyncInFlight || state.value.conversationId != conversationId) return
        realtimeSyncInFlight = true
        try {
            val snapshot = state.value
            val lastLocalId = lastServerMessageIndex(snapshot.messages)
            val request = if (lastLocalId == null) {
                0 to MessagePageFetchLimit
            } else {
                (lastLocalId + 1) to 200
            }
            when (val result = api.loadMessagePage(profile, conversationId, state.value.foregroundSessionId, offset = request.first, limit = request.second)) {
                is AppResult.Ok -> {
                    val page = result.value
                    val shouldLoadLatest = lastLocalId == null && page.total > page.messages.size
                    if (shouldLoadLatest) {
                        val latestOffset = (page.total - MessagePageFetchLimit).coerceAtLeast(0)
                        when (val latest = api.loadMessagePage(profile, conversationId, state.value.foregroundSessionId, offset = latestOffset, limit = MessagePageFetchLimit)) {
                            is AppResult.Ok -> mutableState.update {
                                it.copy(
                                    messages = mergeMessages(it.messages, latest.value.messages),
                                    loadedOffset = latest.value.offset,
                                    totalMessages = latest.value.total,
                                    error = null,
                                    realtimeState = "Realtime synced",
                                )
                            }
                            is AppResult.Err -> Unit
                        }
                    } else if (page.messages.isNotEmpty() || page.total != snapshot.totalMessages) {
                        mutableState.update {
                            it.copy(
                                messages = mergeMessages(it.messages, page.messages),
                                loadedOffset = if (page.messages.isNotEmpty()) min(it.loadedOffset, page.offset) else it.loadedOffset,
                                totalMessages = page.total,
                                error = null,
                                realtimeState = if (page.messages.isNotEmpty()) {
                                    "Realtime synced · ${page.offset + page.messages.size}/${page.total}"
                                } else {
                                    it.realtimeState
                                },
                            )
                        }
                    } else if (updateStatusWhenIdle) {
                        mutableState.update { it.copy(totalMessages = page.total, realtimeState = "Realtime synced") }
                    }
                    cacheCurrentConversation()
                }
                is AppResult.Err -> if (updateStatusWhenIdle) {
                    mutableState.update { it.copy(realtimeState = "Sync stale: ${result.error.userMessage()}") }
                }
            }
        } finally {
            realtimeSyncInFlight = false
        }
    }

    private fun handleSubscriptionAck(conversationId: String, payload: JsonObject) {
        val currentMessageId = (payload["current_message_id"] as? JsonPrimitive)?.content?.toIntOrNull()
        val nextMessageId = (payload["next_message_id"] as? JsonPrimitive)?.content?.toIntOrNull()
            ?: (payload["total"] as? JsonPrimitive)?.intOrNull
            ?: return
        viewModelScope.launch(coroutineErrorHandler) {
            if (state.value.conversationId != conversationId || conversationId != realtimeConversationId) return@launch
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            val lastLocalId = lastServerMessageIndex(state.value.messages)
            val request = when {
                lastLocalId == null -> {
                    val latestIndex = currentMessageId ?: nextMessageId.minus(1)
                    if (latestIndex < 0) null else {
                        val offset = (latestIndex - MessagePageFetchLimit + 1).coerceAtLeast(0)
                        offset to MessagePageFetchLimit
                    }
                }
                nextMessageId > lastLocalId + 1 -> {
                    val gap = (nextMessageId - lastLocalId - 1).coerceIn(1, 200)
                    (lastLocalId + 1) to gap
                }
                else -> null
            } ?: return@launch
            when (val result = api.loadMessagePage(profile, conversationId, state.value.foregroundSessionId, offset = request.first, limit = request.second)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            messages = mergeMessages(it.messages, result.value.messages),
                            loadedOffset = min(it.loadedOffset, result.value.offset),
                            totalMessages = result.value.total,
                            error = null,
                            realtimeState = "Realtime synced",
                        )
                    }
                    cacheCurrentConversation()
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(realtimeState = "Realtime sync gap failed: ${result.error.userMessage()}")
                }
            }
        }
    }

    private fun nextLocalIndex(messages: List<ChatMessage>): Int =
        (messages.maxOfOrNull { it.index } ?: -1) + 1

    private fun MessageAttachment.previewKey(): String = listOf(id, previewUrl, url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl)
        .firstOrNull { it.isNotBlank() }
        ?: "$index:$name"

    private fun scopedPreviewKey(key: String): String = "${state.value.conversationId}:${state.value.foregroundSessionId}:$key"

    private fun isTextAttachment(mediaType: String, name: String, byteCount: Int): Boolean {
        if (byteCount > 256 * 1024) return false
        if (mediaType.startsWith("text/")) return true
        if (mediaType.contains("json") || mediaType.contains("xml")) return true
        val lower = name.lowercase()
        return listOf(".txt", ".md", ".json", ".xml", ".log", ".csv", ".kt", ".rs", ".js", ".ts", ".py", ".toml", ".yaml", ".yml")
            .any { lower.endsWith(it) }
    }

    private fun PendingAttachmentUiState.toSendFile(): SendMessageFileDto {
        val resolver = getApplication<Application>().contentResolver
        val uriValue = Uri.parse(uri)
        val bytes = resolver.openInputStream(uriValue)?.use { it.readBytes() }
            ?: throw IllegalArgumentException("cannot open $name")
        if (bytes.size > MaxAttachmentBytes) {
            throw IllegalArgumentException("${name} is larger than ${formatBytes(MaxAttachmentBytes)}")
        }
        val base64 = Base64.encodeToString(bytes, Base64.NO_WRAP)
        val media = mediaType ?: resolver.getType(uriValue) ?: "application/octet-stream"
        return SendMessageFileDto(
            uri = "data:$media;base64,$base64",
            mediaType = media,
            name = name,
        )
    }

    private fun pendingAttachmentFromUri(uri: Uri, resolver: ContentResolver): PendingAttachmentUiState {
        var name = uri.lastPathSegment?.substringAfterLast('/')?.ifBlank { null } ?: "attachment"
        var size: Long? = null
        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst()) {
                val nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (nameIndex >= 0) {
                    name = cursor.getString(nameIndex)?.takeIf { it.isNotBlank() } ?: name
                }
                val sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE)
                if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) {
                    size = cursor.getLong(sizeIndex)
                }
            }
        }
        return PendingAttachmentUiState(
            uri = uri.toString(),
            name = name,
            mediaType = resolver.getType(uri),
            sizeBytes = size,
        )
    }

    private fun formatBytes(value: Long): String {
        val units = listOf("B", "KB", "MB", "GB")
        var size = value.toDouble()
        var unit = 0
        while (size >= 1024 && unit < units.lastIndex) {
            size /= 1024
            unit += 1
        }
        return if (unit == 0) "${value}B" else "${String.format("%.1f", size)}${units[unit]}"
    }

    private fun logRealtime(message: String) {
        logDebug("realtime: $message")
    }

    private fun logDebug(message: String) {
        AppLogStore.append(getApplication(), "chat", message)
    }

    private fun handleWebSocketText(conversationId: String, text: String) {
        if (conversationId != realtimeConversationId) return
        try {
            val payload = json.decodeFromString<JsonObject>(text)
            val payloadType = payload["type"]?.jsonPrimitive?.content.orEmpty()
            val streamType = payload.normalizedStreamEvent().streamType()
            when {
                payloadType == "chat.snapshot" -> {
                    logRealtime("chat snapshot conversation=$conversationId")
                    reconnectAttempt = 0
                    val turnActive = payload.hasActiveTurn()
                    mutableState.update {
                        it.copy(
                            realtimeState = if (turnActive) "Agent running..." else "Realtime connected",
                            progressTitle = if (turnActive) "Agent running" else it.progressTitle,
                        )
                    }
                    viewModelScope.launch(coroutineErrorHandler) {
                        val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                        requestSync(conversationId, profile, SyncReason.WebSocketAck, updateStatusWhenIdle = true)
                    }
                }
                payloadType == "chat.heartbeat" -> handleChatHeartbeat(payload)
                payloadType == "chat.message_appended" || payloadType == "chat.user_message_committed" -> {
                    val dto = payload["message"]?.let { json.decodeFromJsonElement<ChatMessageDto>(it) } ?: return
                    val message = dto.toDomain()
                    val index = payload["message_index"]?.jsonPrimitive?.intOrNull ?: message.index
                    applyIncomingMessages(listOf(message.copy(index = index)))
                    latestProfile?.let { profile -> markConversationSeen(profile, conversationId, state.value.foregroundSessionId, index + 1) }
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            totalMessages = maxOf(it.totalMessages, index + 1),
                            realtimeState = "Realtime connected · ${index + 1}/${maxOf(it.totalMessages, index + 1)}",
                        )
                    }
                    cacheCurrentConversation()
                }
                payloadType == "chat.user_message_queued" || payloadType == "chat.user_message_started" -> {
                    mutableState.update { it.copy(realtimeState = "Message accepted") }
                }
                payloadType == "chat.attachment_manifest" -> {
                    sawActiveTurnProgress = true
                    applyStreamAttachmentManifest(payload)
                }
                streamType == "stream_turn_start" -> {
                    sawActiveTurnProgress = true
                    pendingStreamAttachments.clear()
                    mutableState.update {
                        it.copy(
                            messages = removeStreamingMessages(it.messages, payload.normalizedStreamEvent().turnId()),
                            realtimeState = "Agent running...",
                            progressTitle = "Agent running",
                        )
                    }
                }
                streamType == "stream_turn_done" -> {
                    sawActiveTurnProgress = false
                    pendingStreamAttachments.clear()
                    mutableState.update {
                        it.copy(
                            messages = removeStreamingMessages(it.messages, payload.normalizedStreamEvent().turnId()),
                            realtimeState = "Turn completed",
                            progressTitle = null,
                            progressDetail = null,
                        )
                    }
                    viewModelScope.launch(coroutineErrorHandler) {
                        val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                        requestSync(conversationId, profile, SyncReason.CompletionFinalState, updateStatusWhenIdle = true)
                    }
                }
                streamType in StreamDeltaTypes -> {
                    sawActiveTurnProgress = true
                    applyStreamEvent(payload)
                }
                streamType == "plan_updated" || payloadType == "chat.plan_updated" -> {
                    mutableState.update { it.copy(realtimeState = "Plan updated") }
                }
                streamType == "stream_error" || payloadType == "chat.error" -> mutableState.update {
                    pendingStreamAttachments.clear()
                    it.copy(
                        realtimeState = payload["message"]?.jsonPrimitive?.content
                            ?: payload["error"]?.jsonPrimitive?.content
                            ?: "Realtime error",
                    )
                }
                payloadType == "subscription_ack" -> {
                    val reason = payload["reason"]?.jsonPrimitive?.content.orEmpty()
                    logRealtime("subscription_ack conversation=$conversationId reason=$reason")
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            realtimeState = if (reason == "session_changed") {
                                "Realtime synced; session changed"
                            } else {
                                "Realtime connected"
                            },
                        )
                    }
                    if (payload["turn_progress"] is JsonObject) {
                        handleProgress(payload["turn_progress"] as JsonObject)
                    }
                    handleSubscriptionAck(conversationId, payload)
                    viewModelScope.launch(coroutineErrorHandler) {
                        val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                        requestSync(conversationId, profile, SyncReason.WebSocketAck, updateStatusWhenIdle = true)
                    }
                }
                payloadType == "messages" -> {
                    val page = json.decodeFromString<MessagesResponseDto>(text)
                    val messages = page.messages.map { it.toDomain() }
                    logRealtime("messages frame conversation=$conversationId count=${messages.size} offset=${page.offset} total=${page.total}")
                    applyIncomingMessages(messages)
                    latestProfile?.let { profile -> markConversationSeen(profile, conversationId, state.value.foregroundSessionId, page.total) }
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            loadedOffset = mergedLoadedOffset(
                                currentMessages = it.messages,
                                currentOffset = it.loadedOffset,
                                page = MessagePage(page.offset, page.limit, page.total, messages),
                            ),
                            totalMessages = page.total,
                            realtimeState = "Realtime connected · ${page.offset + messages.size}/${page.total}",
                        )
                    }
                    cacheCurrentConversation()
                }
                payloadType == "turn_progress" -> handleProgress(payload)
                payloadType == "error" -> mutableState.update {
                    it.copy(
                        realtimeState = payload["message"]?.jsonPrimitive?.content
                            ?: payload["error"]?.jsonPrimitive?.content
                            ?: "Realtime error",
                    )
                }
            }
        } catch (error: SerializationException) {
            logRealtime("malformed frame conversation=$conversationId error=${error.message.orEmpty()} text=${text.take(240)}")
        } catch (error: IllegalArgumentException) {
            logRealtime("unexpected frame conversation=$conversationId error=${error.message.orEmpty()} text=${text.take(240)}")
        }
    }

    private fun applyStreamEvent(payload: JsonObject) {
        val event = payload.normalizedStreamEvent()
        val type = event.streamType().ifBlank { payload.string("type").orEmpty().removePrefix("chat.") }
        mutableState.update { current ->
            val messages = when (type) {
                "stream_assistant_message_delta" -> appendStreamingAssistantDelta(current.messages, event)
                "stream_reasoning_summary_delta" -> appendStreamingReasoningDelta(current.messages, event)
                "stream_tool_call_delta" -> appendStreamingToolCallDelta(current.messages, event)
                "stream_tool_result_done" -> appendStreamingToolResult(current.messages, event)
                else -> current.messages
            }
            current.copy(messages = messages, realtimeState = streamingStatus(type), progressTitle = "Agent running")
        }
    }

    private fun handleChatHeartbeat(payload: JsonObject) {
        val heartbeatConversationId = payload.string("conversation_id").orEmpty()
        val heartbeatSessionId = payload.string("foreground_session_id").orEmpty().ifBlank { "main" }
        val current = state.value
        if (heartbeatConversationId != current.conversationId || heartbeatSessionId != current.foregroundSessionId) return
        val serverState = payload.string("state").orEmpty()
        val active = payload.hasActiveTurn() || serverState == "running"
        mutableState.update {
            when {
                active -> it.copy(
                    realtimeState = "Agent running...",
                    progressTitle = "Agent running",
                    progressDetail = payload.string("active_turn_id")?.takeIf { id -> id.isNotBlank() },
                )
                serverState == "queued" -> it.copy(
                    realtimeState = "Message queued...",
                    progressTitle = "Queued",
                    progressDetail = null,
                )
                serverState == "idle" && it.progressTitle != null -> it.copy(
                    realtimeState = "Realtime connected",
                    progressTitle = null,
                    progressDetail = null,
                )
                else -> it.copy(realtimeState = "Realtime connected")
            }
        }
    }

    private fun appendStreamingAssistantDelta(messages: List<ChatMessage>, event: JsonObject): List<ChatMessage> {
        val id = event.streamMessageId().ifBlank { return messages }
        if (messages.any { it.localState != MessageLocalState.Streaming && it.id == id }) return messages
        val delta = event.streamDeltaText().ifBlank { return messages }
        val attachmentKey = event.streamAttachmentKey(id)
        val incomingAttachments = streamAttachments(event) + pendingStreamAttachments.remove(attachmentKey).orEmpty()
        return upsertStreamingMessage(messages, event, id) { existing ->
            val text = existing?.text.orEmpty() + delta
            val baseAttachments = existing?.attachments.orEmpty()
            val mergedAttachments = baseAttachments + incomingAttachments.filterNot { incoming ->
                baseAttachments.any { it.id == incoming.id || it.previewKey() == incoming.previewKey() }
            }
            (existing ?: newStreamingMessage(messages, event, id)).copy(
                text = text,
                preview = text.take(160),
                items = upsertTextItem(existing?.items.orEmpty(), text),
                attachments = mergedAttachments,
                attachmentCount = mergedAttachments.size,
            )
        }
    }

    private fun appendStreamingReasoningDelta(messages: List<ChatMessage>, event: JsonObject): List<ChatMessage> {
        val id = event.streamMessageId().ifBlank { return messages }
        if (messages.any { it.localState != MessageLocalState.Streaming && it.id == id }) return messages
        val delta = event.streamDeltaText().ifBlank { return messages }
        return upsertStreamingMessage(messages, event, id) { existing ->
            val base = existing ?: newStreamingMessage(messages, event, id)
            base.copy(items = appendReasoningItem(base.items, delta))
        }
    }

    private fun appendStreamingToolCallDelta(messages: List<ChatMessage>, event: JsonObject): List<ChatMessage> {
        val id = event.streamMessageId().ifBlank { return messages }
        if (messages.any { it.localState != MessageLocalState.Streaming && it.id == id }) return messages
        val delta = event.streamDeltaText().ifBlank { return messages }
        val callId = event.string("call_id") ?: event.string("callId") ?: event.string("item_id") ?: event.string("itemId") ?: return messages
        val toolName = event.string("tool_name") ?: event.string("toolName") ?: callId
        return upsertStreamingMessage(messages, event, id) { existing ->
            val base = existing ?: newStreamingMessage(messages, event, id)
            base.copy(items = appendToolCallDelta(base.items, callId, toolName, delta))
        }
    }

    private fun appendStreamingToolResult(messages: List<ChatMessage>, event: JsonObject): List<ChatMessage> {
        val resultObject = event.objectValue("tool_result") ?: event.objectValue("toolResult") ?: event
        val callId = resultObject.string("tool_call_id") ?: resultObject.string("toolCallId") ?: event.string("tool_call_id") ?: event.string("toolCallId") ?: return messages
        val toolName = resultObject.string("tool_name") ?: resultObject.string("toolName") ?: "tool"
        val result = resultObject.objectValue("result")
        val context = result?.objectValue("context")?.string("text") ?: result?.string("context")
        val files = streamToolResultFiles(result, resultObject)
        val turnId = event.turnId()
        val id = event.streamMessageId().ifBlank {
            messages.lastOrNull { message ->
                message.localState == MessageLocalState.Streaming &&
                    message.streamTurnId == turnId &&
                    message.items.any { it is MessageItem.ToolCall && it.toolCallId == callId }
            }?.id ?: "live-tool-result-${turnId.ifBlank { "turn" }}-$callId"
        }
        return upsertStreamingMessage(messages, event, id) { existing ->
            val base = existing ?: newStreamingMessage(messages, event, id)
            val mergedAttachments = base.attachments + files.filterNot { file ->
                base.attachments.any { it.previewKey() == file.previewKey() }
            }
            base.copy(
                items = upsertToolResult(base.items, callId, toolName, context),
                attachments = mergedAttachments,
                attachmentCount = mergedAttachments.size,
            )
        }
    }

    private fun applyStreamAttachmentManifest(event: JsonObject) {
        val attachments = streamAttachments(event)
        if (attachments.isEmpty()) return
        val id = event.streamMessageId().ifBlank { return }
        val attachmentKey = event.streamAttachmentKey(id)
        pendingStreamAttachments[attachmentKey] = pendingStreamAttachments[attachmentKey].orEmpty() + attachments.filterNot { incoming ->
            pendingStreamAttachments[attachmentKey].orEmpty().any { it.id == incoming.id || it.previewKey() == incoming.previewKey() }
        }
        mutableState.update { current ->
            current.copy(realtimeState = "Attachment ready", progressTitle = "Agent running")
        }
        previewAttachments(attachments)
    }

    private fun streamAttachments(event: JsonObject): List<MessageAttachment> {
        return event["attachments"]?.let { value ->
            runCatching {
                value.jsonArray.mapIndexedNotNull { index, element ->
                    (element as? JsonObject)?.toMessageAttachment(index)
                }
            }.getOrNull()
        }.orEmpty()
    }

    private fun streamToolResultFiles(result: JsonObject?, resultObject: JsonObject): List<MessageAttachment> {
        val files = result?.get("files") ?: resultObject.get("files") ?: return emptyList()
        return runCatching {
            files.jsonArray.mapIndexedNotNull { index, element ->
                (element as? JsonObject)?.toMessageAttachment(index)
            }
        }.getOrDefault(emptyList())
    }

    private fun JsonObject.toMessageAttachment(index: Int): MessageAttachment? {
        val url = string("url").orEmpty()
        val uri = string("uri").orEmpty()
        val fileUri = string("file_uri").orEmpty()
        val path = string("path").orEmpty()
        val filePath = string("file_path").orEmpty()
        val workspacePath = string("workspace_path").orEmpty()
        val relativePath = string("relative_path") ?: string("workspace_relative_path").orEmpty()
        val src = string("src").orEmpty()
        val dataUrl = string("data_url").orEmpty()
        val dataBase64 = string("data_base64") ?: string("base64").orEmpty()
        val data = string("data").orEmpty()
        val previewUrl = string("preview_url").orEmpty()
        val downloadUrl = string("download_url").orEmpty()
        val openInWorkspacePath = string("open_in_workspace_path").orEmpty()
        val target = listOf(previewUrl, downloadUrl, url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl, dataBase64, data).firstOrNull { it.isNotBlank() }.orEmpty()
        if (target.isBlank()) return null
        val name = string("name") ?: string("filename") ?: target.substringBefore('?').trimEnd('/').substringAfterLast('/').ifBlank { "attachment-$index" }
        val mediaType = string("media_type") ?: string("mime_type") ?: string("mime")
        return MessageAttachment(
            id = string("id").orEmpty(),
            index = index,
            kind = if (mediaType?.startsWith("image/") == true) "image" else "document",
            name = name,
            mediaType = mediaType,
            sizeBytes = longValue("size_bytes"),
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
            encoding = string("encoding").orEmpty(),
            previewUrl = previewUrl,
            downloadUrl = downloadUrl,
            openInWorkspacePath = openInWorkspacePath,
        )
    }

    private fun upsertStreamingMessage(messages: List<ChatMessage>, event: JsonObject, id: String, build: (ChatMessage?) -> ChatMessage): List<ChatMessage> {
        val position = messages.indexOfFirst { message ->
            message.localState == MessageLocalState.Streaming && message.id == id
        }
        val next = build(messages.getOrNull(position)).copy(localState = MessageLocalState.Streaming)
        return if (position >= 0) messages.toMutableList().also { it[position] = next } else messages + next
    }

    private fun newStreamingMessage(messages: List<ChatMessage>, event: JsonObject, id: String): ChatMessage = ChatMessage(
        id = id,
        index = event.messageIndex() ?: nextStreamingIndex(messages),
        role = "assistant",
        text = "",
        preview = "",
        userName = null,
        messageTime = Instant.now().toString(),
        attachmentCount = 0,
        hasTokenUsage = false,
        localState = MessageLocalState.Streaming,
        streamTurnId = event.turnId().takeIf { it.isNotBlank() },
        syntheticStream = id.startsWith("live-tool-result-"),
    )

    private fun nextStreamingIndex(messages: List<ChatMessage>): Int =
        (messages.filterNot { it.localState == MessageLocalState.Streaming }.maxOfOrNull { it.index } ?: -1) + 1

    private fun upsertTextItem(items: List<MessageItem>, text: String): List<MessageItem> {
        val index = items.indexOfFirst { it is MessageItem.Text }
        if (index < 0) return items + MessageItem.Text(items.size, text)
        return items.toMutableList().also { it[index] = MessageItem.Text(items[index].index, text) }
    }

    private fun appendReasoningItem(items: List<MessageItem>, delta: String): List<MessageItem> {
        val index = items.indexOfFirst { it is MessageItem.Text && it.text.startsWith("Thinking:\n") }
        val prefix = "Thinking:\n"
        if (index < 0) return items + MessageItem.Text(items.size, prefix + delta)
        val current = items[index] as MessageItem.Text
        return items.toMutableList().also { it[index] = current.copy(text = current.text + delta) }
    }

    private fun appendToolCallDelta(items: List<MessageItem>, callId: String, toolName: String, delta: String): List<MessageItem> {
        val index = items.indexOfFirst { it is MessageItem.ToolCall && it.toolCallId == callId }
        if (index < 0) return items + MessageItem.ToolCall(items.size, callId, toolName, delta, null)
        val current = items[index] as MessageItem.ToolCall
        return items.toMutableList().also { it[index] = current.copy(toolName = toolName, arguments = current.arguments + delta) }
    }

    private fun upsertToolResult(items: List<MessageItem>, callId: String, toolName: String, context: String?): List<MessageItem> {
        val index = items.indexOfFirst { it is MessageItem.ToolResult && it.toolCallId == callId }
        val item = MessageItem.ToolResult(if (index >= 0) items[index].index else items.size, callId, toolName, context, null)
        return if (index < 0) items + item else items.toMutableList().also { it[index] = item }
    }

    private fun removeStreamingMessages(messages: List<ChatMessage>, turnId: String): List<ChatMessage> =
        messages.filterNot { message ->
            message.localState == MessageLocalState.Streaming && (turnId.isBlank() || message.streamTurnId == turnId)
        }

    private fun removeStreamingMessagesForCanonical(messages: List<ChatMessage>, incoming: List<ChatMessage>): List<ChatMessage> {
        if (incoming.isEmpty()) return messages
        return messages.filterNot { local ->
            local.localState == MessageLocalState.Streaming && incoming.any { remote ->
                remote.id == local.id || shouldDropStreamingForCanonical(local, remote)
            }
        }
    }

    private fun shouldDropStreamingForCanonical(local: ChatMessage, remote: ChatMessage): Boolean {
        if (local.localState != MessageLocalState.Streaming || !remote.role.equals("assistant", ignoreCase = true)) return false
        if (local.streamTurnId != null && local.streamTurnId == remote.streamTurnId) return true
        if (local.syntheticStream && remote.index >= local.index) return true
        return local.index >= 0 && local.index == remote.index
    }

    private fun streamingStatus(type: String): String = when (type) {
        "stream_assistant_message_delta" -> "Assistant streaming..."
        "stream_reasoning_summary_delta", "stream_reasoning_summary_part_added" -> "Assistant reasoning..."
        "stream_tool_call_delta" -> "Preparing tool call..."
        "stream_tool_result_done" -> "Tool result received"
        else -> "Agent running..."
    }

    private fun handleProgress(payload: JsonObject) {
        val finalState = payload["final_state"]?.jsonPrimitive?.content
        val progress = payload["progress"] as? JsonObject
        val phase = payload["phase"]?.jsonPrimitive?.content
            ?: progress?.get("phase")?.jsonPrimitive?.content
        val activity = payload["activity"]?.jsonPrimitive?.content
            ?: progress?.get("activity")?.jsonPrimitive?.content
        val hint = payload["hint"]?.jsonPrimitive?.content
            ?: progress?.get("hint")?.jsonPrimitive?.content
        val important = payload["important"]?.jsonPrimitive?.booleanOrNull ?: false
        val title = when (finalState) {
            "done" -> "Done"
            "failed" -> "Failed"
            else -> phase?.replaceFirstChar { it.uppercase() } ?: "Working"
        }
        val detail = listOfNotNull(activity, hint).joinToString(" · ").ifBlank { null }
        if (finalState == null) {
            sawActiveTurnProgress = true
            AgentCompletionService.watch(
                context = getApplication<Application>(),
                conversationId = state.value.conversationId,
                baselineIndex = lastServerMessageIndex(state.value.messages) ?: -1,
            )
        }
        mutableState.update {
            it.copy(
                progressTitle = title,
                progressDetail = detail,
                progressImportant = important,
                realtimeState = if (finalState == null) "Realtime active" else "Realtime connected",
            )
        }
        if (finalState == "done" || finalState == "failed") {
            val shouldNotify = finalState == "done" && sawActiveTurnProgress
            sawActiveTurnProgress = false
            viewModelScope.launch(coroutineErrorHandler) {
                delay(500)
                val conversationId = state.value.conversationId
                val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                syncMissingMessages(conversationId, profile, updateStatusWhenIdle = true)
                if (shouldNotify) {
                    val snapshot = state.value
                    val latestAssistant = snapshot.messages.lastOrNull { message ->
                        message.role.equals("assistant", ignoreCase = true) &&
                            !message.isToolOnlyMessage() &&
                            !message.isRuntimeMetadataMessage()
                    }
                    val completionKey = latestAssistant?.let { "$conversationId:${it.index}" }
                    AgentNotificationCenter.notifyAgentDone(
                        context = getApplication<Application>(),
                        conversationId = snapshot.conversationId,
                        title = "Agent finished",
                        detail = latestAssistant?.text?.ifBlank { latestAssistant.preview }?.take(160),
                        completionKey = completionKey,
                    )
                    AgentCompletionService.stop(getApplication<Application>(), conversationId)
                }
                delay(1200)
                mutableState.update { it.copy(progressTitle = null, progressDetail = null, progressImportant = false) }
            }
        }
    }

    override fun onCleared() {
        cacheCurrentConversation()
        closeRealtime(allowReconnect = false)
        super.onCleared()
    }

    private fun websocketCloseMessage(prefix: String, code: Int, reason: String): String =
        if (reason.isBlank()) "$prefix: $code" else "$prefix: $code · $reason"

    private fun realtimeFailureMessage(error: Throwable, response: Response?): String {
        val errorName = error::class.simpleName ?: "Error"
        val message = error.message.orEmpty().ifBlank { "unknown" }
        val status = response?.let { " · HTTP ${it.code}${it.message.ifBlank { "" }.let { value -> if (value.isBlank()) "" else " ${value}" }}" }.orEmpty()
        return "$errorName: $message$status"
    }

    private inner class ChatWebSocketListener(
        private val conversationId: String,
    ) : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            if (!isActiveRealtimeSocket(webSocket)) return
            reconnectAttempt = 0
            logRealtime("onOpen conversation=$conversationId http=${response.code} ${response.message}")
            mutableState.update { it.copy(realtimeState = "Realtime connected") }
            viewModelScope.launch(coroutineErrorHandler) {
                val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                requestSync(conversationId, profile, SyncReason.Reconnected, updateStatusWhenIdle = true)
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            if (!isActiveRealtimeSocket(webSocket)) return
            handleWebSocketText(conversationId, text)
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            if (!isActiveRealtimeSocket(webSocket)) return
            val message = websocketCloseMessage("Realtime closing", code, reason)
            logRealtime("onClosing conversation=$conversationId $message")
            mutableState.update { it.copy(realtimeState = message) }
            webSocket.close(code, reason)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (!isActiveRealtimeSocket(webSocket)) return
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                val message = websocketCloseMessage("Realtime closed", code, reason)
                logRealtime("onClosed conversation=$conversationId $message")
                mutableState.update { it.copy(realtimeState = message) }
                scheduleReconnect(conversationId)
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (!isActiveRealtimeSocket(webSocket)) return
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                val message = realtimeFailureMessage(t, response)
                logRealtime("onFailure conversation=$conversationId $message")
                mutableState.update {
                    it.copy(realtimeState = "Realtime error: $message")
                }
                scheduleReconnect(conversationId)
            }
        }
    }

    private fun isActiveRealtimeSocket(socket: WebSocket): Boolean = socket == webSocket

    private companion object {
        const val VisibleTimelineItemTarget = 20
        const val MessagePageFetchLimit = 20
        const val MaxAttachmentBytes = 8L * 1024L * 1024L
        const val AttachmentPreviewLimitBytes = 2_000_000
        const val AttachmentDownloadLimitBytes = 100_000_000
    }
}

data class ChatUiState(
    val conversationId: String = "",
    val foregroundSessionId: String = "main",
    val displayName: String = "",
    val isLoading: Boolean = false,
    val isLoadingEarlier: Boolean = false,
    val isSending: Boolean = false,
    val messages: List<ChatMessage> = emptyList(),
    val loadedOffset: Int = 0,
    val totalMessages: Int = 0,
    val pendingAttachments: List<PendingAttachmentUiState> = emptyList(),
    val selectionReferences: List<SelectionReferenceUiState> = emptyList(),
    val draft: String = "",
    val error: String? = null,
    val realtimeState: String = "",
    val progressTitle: String? = null,
    val progressDetail: String? = null,
    val progressImportant: Boolean = false,
    val attachmentPreviews: Map<String, AttachmentPreviewUiState> = emptyMap(),
    val conversationSummary: ConversationSummary? = null,
)

data class PendingAttachmentUiState(
    val uri: String,
    val name: String,
    val mediaType: String? = null,
    val sizeBytes: Long? = null,
)

data class AttachmentPreviewUiState(
    val isLoading: Boolean = false,
    val error: String? = null,
    val image: Bitmap? = null,
    val text: String? = null,
    val detail: String? = null,
) {
    val hasContent: Boolean = image != null || text != null || detail != null
}

private enum class SyncReason {
    InitialLoad,
    WebSocketAck,
    WebSocketMessages,
    Reconnected,
    PeriodicBackfill,
    NetworkRestored,
    ManualRefresh,
    CompletionFinalState,
}

private data class PendingSend(
    val localId: String,
    val remoteMessageId: String,
    val conversationId: String,
    val foregroundSessionId: String,
    val text: String,
    val files: List<SendMessageFileDto>,
    val selectionReferences: List<SelectionReferenceDto>,
    val baselineIndex: Int,
    val messageTime: String,
)

private val StreamDeltaTypes = setOf(
    "stream_assistant_message_delta",
    "stream_assistant_message_flush",
    "stream_tool_call_delta",
    "stream_tool_result_done",
    "stream_reasoning_summary_delta",
    "stream_reasoning_summary_part_added",
)

private fun SelectionReferenceUiState.toDto(): SelectionReferenceDto = SelectionReferenceDto(
    filePath = path,
    fileName = label,
    mediaType = mediaType,
    selectedText = selectedText,
)

private fun JsonObject.normalizedStreamEvent(): JsonObject =
    objectValue("event") ?: objectValue("session_event") ?: objectValue("stream_event") ?: this

private fun JsonObject.hasActiveTurn(): Boolean =
    objectValue("current_turn_state") != null ||
        string("active_turn_id")?.isNotBlank() == true ||
        get("running")?.jsonPrimitive?.booleanOrNull == true

private fun JsonObject.streamType(): String =
    (string("type") ?: string("event_type") ?: string("kind")).orEmpty().removePrefix("chat.")

private fun JsonObject.streamMessageId(): String = listOf(
    string("message_id"),
    string("messageId"),
    string("next_message_id"),
    string("nextMessageId"),
    string("stream_id"),
    string("streamId"),
).firstOrNull { !it.isNullOrBlank() }.orEmpty().trim()

private fun JsonObject.streamDeltaText(): String =
    string("delta") ?: string("text_delta") ?: string("textDelta") ?: ""

private fun JsonObject.turnId(): String =
    (string("turn_id") ?: string("turnId")).orEmpty().trim()

private fun JsonObject.streamAttachmentKey(messageId: String = streamMessageId()): String = listOf(
    string("conversation_id").orEmpty().trim(),
    string("foreground_session_id").orEmpty().trim(),
    turnId(),
    messageId.trim(),
).joinToString(":")

private fun JsonObject.messageIndex(): Int? =
    intValue("message_index") ?: intValue("messageIndex") ?: intValue("index")

private fun JsonObject.objectValue(name: String): JsonObject? = get(name) as? JsonObject

private fun JsonObject.string(name: String): String? = get(name)?.jsonPrimitive?.contentOrNull

private fun JsonObject.intValue(name: String): Int? = get(name)?.jsonPrimitive?.intOrNull

private fun JsonObject.longValue(name: String): Long? = get(name)?.jsonPrimitive?.longOrNull
