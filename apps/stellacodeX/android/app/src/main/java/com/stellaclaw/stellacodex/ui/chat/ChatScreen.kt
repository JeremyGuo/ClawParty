package com.stellaclaw.stellacodex.ui.chat

import android.Manifest
import android.app.Application
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.Image
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.AttachFile
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Code
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.Terminal
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageLocalState
import kotlinx.coroutines.delay
import org.json.JSONArray
import org.json.JSONObject
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val ChatBackground = Color(0xFFF4F4F7)
private val FrostedSurface = Color(0xF6FFFFFF)
private val FrostedBorder = Color(0x1A000000)
private val MutedText = Color(0xFF8E8E93)
private val UserBubble = Color(0xFF0A95FF)
private val UserBubbleText = Color.White
private val AssistantAccent = Color(0xFF7A45FF)
private val AssistantAccent2 = Color(0xFF2C7BFF)
private val CodeHeaderText = Color(0xFF8A8F98)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    conversationId: String,
    foregroundSessionId: String = "main",
    onBack: () -> Unit,
    onOpenWorkspace: (String) -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ChatViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ChatViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    val listState = rememberLazyListState()
    var initialBottomPlaced by remember(conversationId) { mutableStateOf(false) }
    var earlierLoadAnchor by remember(conversationId) { mutableStateOf<ScrollAnchor?>(null) }
    var showDetails by remember(conversationId) { mutableStateOf(false) }
    val visibleMessages = remember(state.messages) { state.messages.filterNot(ChatMessage::isRuntimeMetadataMessage) }
    val agentProcessing = remember(state.progressTitle, state.realtimeState) {
        isAgentProcessing(state.progressTitle, state.realtimeState)
    }
    val timeline = remember(visibleMessages, agentProcessing) { buildChatTimeline(visibleMessages, agentProcessing) }
    val timelineContentVersion = remember(visibleMessages) { visibleMessages.contentVersion() }
    val scopedPreviewPrefix = remember(conversationId, foregroundSessionId) { "$conversationId:$foregroundSessionId:" }
    val previews = remember(state.attachmentPreviews, scopedPreviewPrefix) {
        state.attachmentPreviews
            .filterKeys { it.startsWith(scopedPreviewPrefix) }
            .mapKeys { (key, _) -> key.removePrefix(scopedPreviewPrefix) }
    }
    val isNearBottom by remember(listState, timeline) {
        derivedStateOf {
            val lastVisible = listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: -1
            lastVisible >= timeline.lastIndex - 1
        }
    }
    val notificationPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }

    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !AgentNotificationCenter.canNotify(application)) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    LaunchedEffect(conversationId, foregroundSessionId) {
        initialBottomPlaced = false
        AgentNotificationCenter.dismissConversation(application, conversationId)
        viewModel.load(conversationId, foregroundSessionId)
    }

    LaunchedEffect(timeline.lastOrNull()?.key, timelineContentVersion) {
        if (timeline.isNotEmpty() && earlierLoadAnchor == null) {
            if (!initialBottomPlaced) {
                listState.scrollToItem(timeline.lastIndex)
                initialBottomPlaced = true
            } else if (isNearBottom) {
                listState.animateScrollToItem(timeline.lastIndex)
            }
        }
    }

    LaunchedEffect(timeline, state.isLoadingEarlier, earlierLoadAnchor) {
        val anchor = earlierLoadAnchor ?: return@LaunchedEffect
        if (!state.isLoadingEarlier && timeline.isNotEmpty()) {
            val index = anchor.messageId
                ?.let { messageId -> timeline.indexOfFirst { it.containsMessageId(messageId) } }
                ?.takeIf { it >= 0 }
                ?: timeline.indexOfFirst { it.key == anchor.key }
            if (index >= 0) {
                listState.scrollToItem(index, anchor.scrollOffset)
            }
            earlierLoadAnchor = null
        }
    }

    LaunchedEffect(visibleMessages) {
        visibleMessages.forEach { message ->
            if (message.attachments.isNotEmpty()) viewModel.previewAttachments(message.attachments)
            message.markdownImageTargets().forEach { target -> viewModel.previewMarkdownImage(message.id, target) }
        }
    }

    LaunchedEffect(
        listState.firstVisibleItemIndex,
        state.loadedOffset,
        state.isLoadingEarlier,
        state.isLoading,
        timeline.isNotEmpty(),
    ) {
        if (initialBottomPlaced &&
            timeline.isNotEmpty() &&
            listState.firstVisibleItemIndex == 0 &&
            state.loadedOffset > 0 &&
            !state.isLoadingEarlier &&
            !state.isLoading
        ) {
            val anchorItem = timeline.getOrNull(listState.firstVisibleItemIndex)
            if (anchorItem != null) {
                earlierLoadAnchor = ScrollAnchor(
                    key = anchorItem.key,
                    messageId = anchorItem.anchorMessageId(),
                    scrollOffset = listState.firstVisibleItemScrollOffset,
                )
            }
            viewModel.loadEarlier()
        }
    }

    Scaffold(
        containerColor = ChatBackground,
        topBar = {
            ChatHeader(
                title = state.displayName.ifBlank { conversationId.ifBlank { "Conversation" } },
                realtimeState = state.realtimeState,
                progressTitle = state.progressTitle,
                progressImportant = state.progressImportant,
                onBack = onBack,
                onOpenWorkspace = { onOpenWorkspace(conversationId) },
                onShowDetails = { showDetails = true },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .background(ChatBackground)
                .padding(horizontal = 12.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            state.error?.let { message ->
                Text(
                    text = message,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
            when {
                state.isLoading && state.messages.isEmpty() -> LoadingMessages()
                visibleMessages.isEmpty() -> EmptyMessages()
                else -> MessageList(
                    isLoadingEarlier = state.isLoadingEarlier,
                    timeline = timeline,
                    listState = listState,
                    previews = previews,
                    onPreviewMarkdownImage = { _, _ -> },
                    onPreviewAttachment = viewModel::previewAttachment,
                    onDownloadAttachment = viewModel::downloadAttachment,
                    onOpenAttachment = viewModel::openAttachment,
                    onRetrySend = viewModel::retrySend,
                    modifier = Modifier.weight(1f),
                )
            }

            Composer(
                draft = state.draft,
                pendingAttachments = state.pendingAttachments,
                selectionReferences = state.selectionReferences,
                isSending = state.isSending,
                onDraftChanged = viewModel::onDraftChanged,
                onAddAttachments = viewModel::addAttachments,
                onRemoveAttachment = viewModel::removeAttachment,
                onRemoveSelectionReference = viewModel::removeSelectionReference,
                onSend = viewModel::send,
            )
        }
    }
    if (showDetails) {
        ConversationDetailsDialog(
            conversationId = conversationId,
            totalMessages = state.totalMessages,
            realtimeState = state.realtimeState,
            summary = state.conversationSummary,
            onDismiss = { showDetails = false },
        )
    }
}

@Composable
private fun ChatHeader(
    title: String,
    realtimeState: String,
    progressTitle: String?,
    progressImportant: Boolean,
    onBack: () -> Unit,
    onOpenWorkspace: () -> Unit,
    onShowDetails: () -> Unit,
) {
    val statusText = listOfNotNull(progressTitle, realtimeState.takeIf { it.isNotBlank() }).joinToString(" · ")
    val hasError = statusText.contains("error", ignoreCase = true) ||
        statusText.contains("failed", ignoreCase = true) ||
        statusText.contains("unavailable", ignoreCase = true)
    val isActive = progressTitle?.let { title ->
        !title.equals("Done", ignoreCase = true) && !title.equals("Failed", ignoreCase = true)
    } == true || realtimeState.contains("active", ignoreCase = true)
    val detailsIcon = when {
        isActive -> Icons.Filled.Terminal
        else -> Icons.Filled.Info
    }
    val detailsTint = when {
        hasError || progressImportant -> MaterialTheme.colorScheme.error
        isActive -> MaterialTheme.colorScheme.primary
        else -> Color.Black
    }
    Surface(
        color = ChatBackground,
        tonalElevation = 0.dp,
        shadowElevation = 0.dp,
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .statusBarsPadding()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            GlassIconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Surface(
                modifier = Modifier.weight(1f),
                shape = RoundedCornerShape(28.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Column(
                    modifier = Modifier.padding(horizontal = 18.dp, vertical = 14.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text(
                        text = title,
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.Bold,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            Surface(
                shape = RoundedCornerShape(28.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Row(modifier = Modifier.padding(horizontal = 4.dp, vertical = 4.dp)) {
                    IconButton(onClick = onOpenWorkspace) {
                        Icon(Icons.Filled.Folder, contentDescription = "Files")
                    }
                    IconButton(onClick = onShowDetails) {
                        Icon(
                            detailsIcon,
                            contentDescription = "Conversation details",
                            tint = detailsTint,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun GlassIconButton(
    onClick: () -> Unit,
    content: @Composable () -> Unit,
) {
    Surface(
        modifier = Modifier.size(52.dp),
        shape = CircleShape,
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
        onClick = onClick,
    ) {
        Box(contentAlignment = Alignment.Center) { content() }
    }
}

@Composable
private fun ConversationDetailsDialog(
    conversationId: String,
    totalMessages: Int,
    realtimeState: String,
    summary: ConversationSummary?,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("关闭") }
        },
        title = { Text("会话详情", fontWeight = FontWeight.Bold) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                DetailLine("名称", summary?.displayName?.takeIf(String::isNotBlank) ?: conversationId)
                DetailLine("会话 ID", conversationId)
                DetailLine("Foreground session", summary?.foregroundSessionId?.takeIf(String::isNotBlank) ?: "main")
                DetailLine("模型", summary?.model?.takeIf(String::isNotBlank) ?: "未知")
                if (summary?.modelSelectionPending == true) {
                    DetailLine("模型状态", "等待选择")
                }
                DetailLine("消息", "${summary?.messageCount ?: totalMessages} msgs")
                DetailLine("实时", realtimeState.ifBlank { "Conversation stream" })
                summary?.reasoning?.takeIf(String::isNotBlank)?.let { DetailLine("Reasoning", it) }
                summary?.sandbox?.takeIf(String::isNotBlank)?.let { DetailLine("Sandbox", it) }
                summary?.remote?.takeIf(String::isNotBlank)?.let { DetailLine("Remote", it) }
                summary?.workspace?.takeIf(String::isNotBlank)?.let { DetailLine("Workspace", it) }
                summary?.let {
                    if (it.totalBackground > 0 || it.totalSubagents > 0) {
                        DetailLine("Sessions", "${it.totalBackground} background · ${it.totalSubagents} subagents")
                    }
                    it.lastMessageTime?.let { time -> DetailLine("最近消息", time) }
                }
            }
        },
        containerColor = FrostedSurface,
        shape = RoundedCornerShape(24.dp),
    )
}

@Composable
private fun DetailLine(label: String, value: String) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MutedText)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun RealtimeStatus(
    realtimeState: String,
    progressTitle: String?,
    progressDetail: String?,
    progressImportant: Boolean,
) {
    if (realtimeState.isBlank() && progressTitle == null) return
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        shape = RoundedCornerShape(18.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                Icons.Filled.Info,
                contentDescription = null,
                tint = MutedText,
                modifier = Modifier.size(22.dp),
            )
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = progressTitle?.let { if (progressImportant) "! $it" else it } ?: "Status",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.Bold,
                )
                val detail = listOfNotNull(
                    realtimeState.takeIf { it.isNotBlank() },
                    progressDetail?.takeIf { it.isNotBlank() },
                ).joinToString(" · ")
                if (detail.isNotBlank()) {
                    Text(
                        text = detail,
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (detail.contains("error", ignoreCase = true) ||
                            detail.contains("unavailable", ignoreCase = true)
                        ) {
                            MaterialTheme.colorScheme.error
                        } else {
                            MutedText
                        },
                    )
                }
            }
        }
    }
}

@Composable
private fun LoadingMessages() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator()
        Text("Loading messages...")
    }
}

@Composable
private fun EmptyMessages() {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("No messages yet.")
        Text(
            text = "Send a message to start this conversation.",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun MessageList(
    isLoadingEarlier: Boolean,
    timeline: List<ChatTimelineItem>,
    listState: LazyListState,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewMarkdownImage: (String, String) -> Unit,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onDownloadAttachment: (MessageAttachment) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
    onRetrySend: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyColumn(
        modifier = modifier.fillMaxWidth(),
        state = listState,
        contentPadding = PaddingValues(vertical = 10.dp),
        verticalArrangement = Arrangement.spacedBy(18.dp),
    ) {
        items(timeline, key = { it.key }) { item ->
            when (item) {
                is ChatTimelineItem.Message -> MessageCard(
                    message = item.message,
                    extraToolItems = item.extraToolItems,
                    processStartedAt = item.processStartedAt,
                    processEndedAt = item.processEndedAt,
                    previews = previews,
                    onPreviewMarkdownImage = onPreviewMarkdownImage,
                    onPreviewAttachment = onPreviewAttachment,
                    onDownloadAttachment = onDownloadAttachment,
                    onOpenAttachment = onOpenAttachment,
                    onRetrySend = onRetrySend,
                )
                is ChatTimelineItem.AgentRun -> AgentRunCard(
                    run = item,
                    previews = previews,
                    onPreviewMarkdownImage = onPreviewMarkdownImage,
                    onPreviewAttachment = onPreviewAttachment,
                    onDownloadAttachment = onDownloadAttachment,
                    onOpenAttachment = onOpenAttachment,
                    onRetrySend = onRetrySend,
                )
            }
        }
    }
}

private data class ScrollAnchor(
    val key: String,
    val messageId: String?,
    val scrollOffset: Int,
)

private sealed interface ChatTimelineItem {
    val key: String

    data class Message(
        val message: ChatMessage,
        val extraToolItems: List<MessageItem> = emptyList(),
        val processStartedAt: String? = null,
        val processEndedAt: String? = null,
    ) : ChatTimelineItem {
        override val key: String = "message:${message.id}"
    }

    data class AgentRun(
        val triggerKey: String,
        val messages: List<ChatMessage>,
        val closedByUser: Boolean = false,
        val forceRunning: Boolean = false,
    ) : ChatTimelineItem {
        val finalMessage: ChatMessage = messages.lastOrNull { !it.isToolOnlyMessage() } ?: messages.last()
        override val key: String = "agent:$triggerKey"
        val processMessages: List<ChatMessage> = messages.filter { it.id != finalMessage.id }
        val processItems: List<MessageItem> = messages.flatMap { it.items }.filter { it is MessageItem.ToolCall || it is MessageItem.ToolResult }
        val startedAt: String? = messages.firstOrNull()?.messageTime
        val endedAt: String? = finalMessage.messageTime
        val running: Boolean = !closedByUser && (forceRunning || messages.any { it.localState == MessageLocalState.Streaming })
    }
}

private fun buildChatTimeline(messages: List<ChatMessage>, latestAgentActive: Boolean): List<ChatTimelineItem> {
    val output = mutableListOf<ChatTimelineItem>()
    val pendingAgent = mutableListOf<ChatMessage>()
    var currentUserKey = "initial"

    fun flushAgent(closedByUser: Boolean = false, forceRunning: Boolean = false) {
        if (pendingAgent.isEmpty()) return
        output += ChatTimelineItem.AgentRun(
            triggerKey = currentUserKey,
            messages = pendingAgent.toList(),
            closedByUser = closedByUser,
            forceRunning = forceRunning,
        )
        pendingAgent.clear()
    }

    messages.forEach { message ->
        if (message.role.equals("user", ignoreCase = true)) {
            flushAgent(closedByUser = true)
            output += ChatTimelineItem.Message(message)
            currentUserKey = message.id.ifBlank { "user-${message.index}" }
        } else if (message.role.equals("assistant", ignoreCase = true)) {
            pendingAgent += message
        } else {
            flushAgent()
            output += ChatTimelineItem.Message(message)
        }
    }
    flushAgent(forceRunning = latestAgentActive)
    return output
}

private fun isAgentProcessing(progressTitle: String?, realtimeState: String): Boolean {
    val title = progressTitle.orEmpty()
    if (title.isNotBlank() && !title.equals("Done", ignoreCase = true) && !title.equals("Failed", ignoreCase = true)) {
        return true
    }
    return listOf(
        "agent running",
        "assistant streaming",
        "assistant reasoning",
        "preparing tool call",
        "tool result received",
    ).any { realtimeState.contains(it, ignoreCase = true) }
}

private fun ChatMessage.isRuntimeMetadataMessage(): Boolean {
    val body = text.ifBlank { preview }.trimStart()
    return body.startsWith("[Incoming User Metadata]") ||
        body.startsWith("[Incoming Assistant Metadata]") ||
        body.startsWith("[Incoming System Metadata]")
}

private fun ChatMessage.isToolOnlyMessage(): Boolean =
    role.equals("assistant", ignoreCase = true) &&
        text.isBlank() &&
        attachments.isEmpty() &&
        items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }

private fun ChatTimelineItem.anchorMessageId(): String? = when (this) {
    is ChatTimelineItem.Message -> message.id
    is ChatTimelineItem.AgentRun -> messages.firstOrNull()?.id
}

private fun ChatTimelineItem.containsMessageId(messageId: String): Boolean = when (this) {
    is ChatTimelineItem.Message -> message.id == messageId
    is ChatTimelineItem.AgentRun -> messages.any { it.id == messageId }
}

@Composable
private fun AgentRunCard(
    run: ChatTimelineItem.AgentRun,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewMarkdownImage: (String, String) -> Unit,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onDownloadAttachment: (MessageAttachment) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
    onRetrySend: (String) -> Unit,
) {
    val processMessages = run.processMessages.filterNot { it.isToolOnlyMessage() }
    val finalMessage = run.finalMessage
    val finalText = finalMessage.text.ifBlank { finalMessage.preview }
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.Start,
        verticalAlignment = Alignment.Top,
    ) {
        AssistantAvatar()
        Spacer(modifier = Modifier.size(10.dp))
        Column(
            modifier = Modifier.weight(1f),
            horizontalAlignment = Alignment.Start,
            verticalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            Text(
                text = "Assistant",
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.Bold,
                color = MutedText,
            )
            if (run.processMessages.isNotEmpty() || run.processItems.isNotEmpty()) {
                AgentProcessPanel(
                    runKey = run.key,
                    processMessages = processMessages,
                    items = run.processItems,
                    startedAt = run.startedAt,
                    endedAt = if (run.running) null else run.endedAt,
                    running = run.running,
                )
            }
            if (finalText.isNotBlank()) {
                SelectionContainer {
                    MessageBody(
                        messageId = finalMessage.id,
                        text = finalText,
                        attachments = finalMessage.attachments,
                        previews = previews,
                        onPreviewMarkdownImage = onPreviewMarkdownImage,
                        onOpenAttachment = onOpenAttachment,
                    )
                }
            } else {
                val textItems = finalMessage.items.filterIsInstance<MessageItem.Text>()
                if (textItems.isNotEmpty()) {
                    SelectionContainer {
                        MessageBody(
                            messageId = finalMessage.id,
                            text = textItems.joinToString("\n\n") { it.text },
                            attachments = finalMessage.attachments,
                            previews = previews,
                            onPreviewMarkdownImage = onPreviewMarkdownImage,
                            onOpenAttachment = onOpenAttachment,
                        )
                    }
                }
            }
            if (finalMessage.attachments.isNotEmpty()) {
                AttachmentList(
                    attachments = finalMessage.attachments,
                    previews = previews,
                    compact = false,
                    onPreviewAttachment = onPreviewAttachment,
                    onDownloadAttachment = onDownloadAttachment,
                    onOpenAttachment = onOpenAttachment,
                )
            }
            MessageMetaRow(
                message = finalMessage,
                alignEnd = false,
                onRetrySend = onRetrySend,
            )
        }
    }
}

@Composable
private fun MessageCard(
    message: ChatMessage,
    extraToolItems: List<MessageItem>,
    processStartedAt: String?,
    processEndedAt: String?,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewMarkdownImage: (String, String) -> Unit,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onDownloadAttachment: (MessageAttachment) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
    onRetrySend: (String) -> Unit,
) {
    val isUserMessage = message.role.equals("user", ignoreCase = true)
    val toolItems = message.items + extraToolItems
    val roleLabel = when (message.role.lowercase()) {
        "user" -> message.userName?.takeIf { it.isNotBlank() } ?: "User"
        "assistant" -> "Assistant"
        "system" -> "System"
        else -> message.role.ifBlank { "Message" }
    }
    val toolExplanations = toolItems
        .filterIsInstance<MessageItem.ToolCall>()
        .mapNotNull { it.explanation?.trim()?.takeIf(String::isNotEmpty) }
    val displayText = message.text.ifBlank {
        toolExplanations.joinToString("\n\n").ifBlank { message.preview }
    }
    val hasToolProcess = toolItems.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }
    val processRunning = hasToolProcess && message.localState == MessageLocalState.Streaming
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = if (isUserMessage) Arrangement.End else Arrangement.Start,
        verticalAlignment = Alignment.Top,
    ) {
        if (!isUserMessage) {
            AssistantAvatar()
            Spacer(modifier = Modifier.size(10.dp))
        }
        Column(
            modifier = if (isUserMessage) Modifier.fillMaxWidth(0.82f) else Modifier.weight(1f),
            horizontalAlignment = if (isUserMessage) Alignment.End else Alignment.Start,
            verticalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            if (isUserMessage) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = roleLabel,
                        style = MaterialTheme.typography.labelMedium,
                        color = MutedText,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Box(
                        modifier = Modifier
                            .size(6.dp)
                            .clip(CircleShape)
                            .background(MutedText),
                    )
                }
                Surface(
                    shape = RoundedCornerShape(22.dp),
                    color = UserBubble,
                ) {
                    SelectionContainer {
                        Text(
                            text = displayText.ifBlank { " " },
                            modifier = Modifier.padding(horizontal = 18.dp, vertical = 12.dp),
                            style = MaterialTheme.typography.titleMedium,
                            color = UserBubbleText,
                        )
                    }
                }
            } else {
                Text(
                    text = roleLabel,
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.Bold,
                    color = MutedText,
                )
                if (hasToolProcess) {
                    AgentProcessPanel(
                        runKey = "message:${message.id}:tools",
                        processMessages = emptyList(),
                        items = toolItems,
                        startedAt = processStartedAt ?: message.messageTime,
                        endedAt = processEndedAt,
                        running = processRunning,
                    )
                }
                if (displayText.isNotBlank()) {
                    SelectionContainer {
                        MessageBody(
                            messageId = message.id,
                            text = displayText,
                            attachments = message.attachments,
                            previews = previews,
                            onPreviewMarkdownImage = onPreviewMarkdownImage,
                            onOpenAttachment = onOpenAttachment,
                        )
                    }
                } else {
                    val textItems = message.items.filterIsInstance<MessageItem.Text>()
                    if (textItems.isNotEmpty()) {
                        SelectionContainer {
                            MessageBody(
                                messageId = message.id,
                                text = textItems.joinToString("\n\n") { it.text },
                                attachments = message.attachments,
                                previews = previews,
                                onPreviewMarkdownImage = onPreviewMarkdownImage,
                                onOpenAttachment = onOpenAttachment,
                            )
                        }
                    }
                }
            }
            if (message.attachments.isNotEmpty()) {
                AttachmentList(
                    attachments = message.attachments,
                    previews = previews,
                    compact = isUserMessage,
                    onPreviewAttachment = onPreviewAttachment,
                    onDownloadAttachment = onDownloadAttachment,
                    onOpenAttachment = onOpenAttachment,
                )
            }
            MessageMetaRow(
                message = message,
                alignEnd = isUserMessage,
                onRetrySend = onRetrySend,
            )
        }
    }
}

@Composable
private fun AssistantAvatar() {
    Box(
        modifier = Modifier
            .size(46.dp)
            .clip(CircleShape)
            .background(AssistantAccent),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(46.dp)
                .background(AssistantAccent2.copy(alpha = 0.45f)),
        )
        Icon(
            Icons.Filled.AutoAwesome,
            contentDescription = null,
            tint = Color.White,
            modifier = Modifier.size(26.dp),
        )
    }
}

@Composable
private fun MessageMetaRow(
    message: ChatMessage,
    alignEnd: Boolean,
    onRetrySend: (String) -> Unit,
) {
    val usage = message.tokenUsage
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = if (alignEnd) Arrangement.End else Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            message.messageTime?.let { time ->
                Text(text = formatLocalMinute(time), style = MaterialTheme.typography.labelSmall, color = MutedText)
            }
            when (message.localState) {
                MessageLocalState.Sending -> Text(
                    text = "sending...",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                MessageLocalState.Streaming -> Text(
                    text = "streaming...",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                MessageLocalState.Failed -> {
                    Text(
                        text = "send failed",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                    TextButton(onClick = { onRetrySend(message.id) }) {
                        Text("Retry")
                    }
                }
                MessageLocalState.Synced -> Unit
            }
        }
        if (!alignEnd && (usage != null || message.hasTokenUsage)) {
            Surface(
                shape = RoundedCornerShape(16.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Row(
                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 5.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(if (usage != null) Color(0xFF34C759) else Color(0xFFFF3B30)),
                    )
                    Text(
                        text = usage?.let { "${formatCompactNumber(it.total)} tokens" } ?: "usage",
                        style = MaterialTheme.typography.labelLarge,
                        color = MutedText,
                        fontWeight = FontWeight.Bold,
                    )
                }
            }
        }
    }
}

@Composable
private fun MessageBody(
    messageId: String,
    text: String,
    attachments: List<MessageAttachment>,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewMarkdownImage: (String, String) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
) {
    val blocks = markdownBlocks(text)
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        blocks.forEach { block ->
            when (block) {
                is MarkdownBlock.Code -> CodeBlock(block)
                is MarkdownBlock.Text -> MarkdownText(messageId, block.text, attachments, previews, onPreviewMarkdownImage, onOpenAttachment)
            }
        }
    }
}

@Composable
private fun MarkdownText(
    messageId: String,
    text: String,
    attachments: List<MessageAttachment>,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewMarkdownImage: (String, String) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
        text.lines().forEach { rawLine ->
            val line = rawLine.trimEnd()
            val inlineImage = line.markdownImageAttachment(attachments)
            val inlineImageTarget = if (inlineImage == null) line.markdownImageTarget() else null
            when {
                inlineImage != null -> Box(modifier = Modifier.clickable { onOpenAttachment(inlineImage) }) {
                    AttachmentPreview(previews[inlineImage.previewKey()])
                }
                inlineImageTarget != null -> {
                    MarkdownImageReference(
                        path = inlineImageTarget,
                        onClick = { onOpenAttachment(inlineImageTarget.toMarkdownImageAttachment()) },
                    )
                }
                line.isBlank() -> Text("", style = MaterialTheme.typography.bodySmall)
                line.startsWith("### ") -> Text(
                    text = line.removePrefix("### "),
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("## ") -> Text(
                    text = line.removePrefix("## "),
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("# ") -> Text(
                    text = line.removePrefix("# "),
                    style = MaterialTheme.typography.titleLarge,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("- ") || line.startsWith("* ") -> Text(
                    text = "• ${line.drop(2)}",
                    style = MaterialTheme.typography.bodyMedium,
                )
                line.matches(Regex("\\d+\\.\\s+.*")) -> Text(
                    text = line,
                    style = MaterialTheme.typography.bodyMedium,
                )
                else -> Text(text = line, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

private fun String.markdownImageAttachment(attachments: List<MessageAttachment>): MessageAttachment? {
    val target = markdownImageTarget() ?: return null
    val normalizedTarget = target.normalizedAttachmentTarget()
    return attachments.firstOrNull { attachment ->
        attachment.kind == "image" && attachment.normalizedTargets().any { it == normalizedTarget }
    }
}

@Composable
private fun MarkdownImageReference(path: String, onClick: () -> Unit) {
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
        shape = RoundedCornerShape(10.dp),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(Icons.Filled.Folder, contentDescription = null, tint = CodeHeaderText)
            Text(
                text = path.substringBefore('?').substringBefore('#').substringAfterLast('/').ifBlank { path },
                style = MaterialTheme.typography.bodySmall,
                color = CodeHeaderText,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

private fun String.markdownImageTarget(): String? = MarkdownImageLinePattern.matchEntire(trim())
    ?.groupValues
    ?.getOrNull(2)
    ?.trim()
    ?.takeIf { it.isMarkdownImagePath() }

private fun String.toMarkdownImageAttachment(): MessageAttachment = MessageAttachment(
    index = -1,
    kind = "image",
    name = substringBefore('?').substringBefore('#').substringAfterLast('/').ifBlank { "image" },
    mediaType = markdownImageMediaType(),
    sizeBytes = null,
    url = takeIf { it.contains("://") }.orEmpty(),
    path = takeUnless { it.contains("://") }.orEmpty(),
)

private fun ChatMessage.markdownImageTargets(): List<String> = emptyList()

private fun List<ChatMessage>.contentVersion(): Int = fold(1) { acc, message ->
    31 * acc + message.contentSignature().hashCode()
}

private fun ChatMessage.contentSignature(): String = buildString {
    append(id).append('|')
    append(index).append('|')
    append(localState).append('|')
    append(text.length).append('|')
    append(preview.length).append('|')
    append(items.size).append('|')
    append(attachments.size)
}

private fun String.isMarkdownImagePath(): Boolean {
    if (isBlank() || startsWith("#") || startsWith("data:") || startsWith("blob:")) return false
    val extension = substringBefore('?').substringBefore('#').substringAfterLast('.', "").lowercase()
    return extension in setOf("png", "jpg", "jpeg", "gif", "webp", "svg")
}

private fun MessageAttachment.normalizedTargets(): List<String> = listOf(url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl)
    .filter { it.isNotBlank() }
    .map { it.normalizedAttachmentTarget() }

private fun String.normalizedAttachmentTarget(): String = trim()
    .substringBefore('?')
    .substringBefore('#')
    .removePrefix("file://")
    .replace('\\', '/')
    .trimStart('/')

private fun String.markdownImageMediaType(): String? = when (substringBefore('?').substringBefore('#').substringAfterLast('.', "").lowercase()) {
    "png" -> "image/png"
    "jpg", "jpeg" -> "image/jpeg"
    "gif" -> "image/gif"
    "webp" -> "image/webp"
    "svg" -> "image/svg+xml"
    else -> null
}

private val MarkdownImageLinePattern = Regex("!\\[([^\\]]*)]\\(([^)]+)\\)")

@Composable
private fun CodeBlock(block: MarkdownBlock.Code) {
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 2.dp),
        shape = RoundedCornerShape(14.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color.White.copy(alpha = 0.55f))
                    .padding(horizontal = 12.dp, vertical = 9.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        Icons.Filled.Code,
                        contentDescription = null,
                        tint = CodeHeaderText,
                        modifier = Modifier.size(20.dp),
                    )
                    Text(
                        text = block.language.ifBlank { "text" },
                        style = MaterialTheme.typography.titleSmall,
                        color = CodeHeaderText,
                        fontWeight = FontWeight.Bold,
                    )
                    Text(
                        text = "${block.code.lines().size} lines",
                        style = MaterialTheme.typography.bodySmall,
                        color = CodeHeaderText.copy(alpha = 0.65f),
                    )
                }
                Row {
                    Icon(Icons.Filled.ExpandLess, contentDescription = "Collapse", tint = CodeHeaderText)
                    Icon(Icons.Filled.ContentCopy, contentDescription = "Copy", tint = CodeHeaderText)
                }
            }
            Text(
                text = block.code.ifBlank { " " },
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color(0xFFF0F0F4))
                    .padding(12.dp),
                style = MaterialTheme.typography.bodyLarge,
                fontFamily = FontFamily.Monospace,
                color = Color(0xFF101014),
            )
        }
    }
}

@Composable
private fun ToolItemList(items: List<MessageItem>) {
    val toolItems = remember(items) { buildToolDisplayItems(items) }
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        toolItems.forEach { item ->
            ToolCard(
                title = item.title,
                body = item.body,
                isResult = item.completed,
            )
        }
    }
}

@Composable
private fun AgentProcessPanel(
    runKey: String,
    processMessages: List<ChatMessage>,
    items: List<MessageItem>,
    startedAt: String?,
    endedAt: String?,
    running: Boolean,
) {
    var expanded by remember(runKey) { mutableStateOf(running) }
    var wasRunning by remember(runKey) { mutableStateOf(running) }
    var now by remember { mutableStateOf(Instant.now()) }
    LaunchedEffect(running) {
        if (running) {
            expanded = true
        } else if (wasRunning) {
            expanded = false
        }
        wasRunning = running
        while (running) {
            now = Instant.now()
            delay(1_000)
        }
    }
    val start = startedAt?.let { runCatching { Instant.parse(it) }.getOrNull() }
    val end = endedAt?.let { runCatching { Instant.parse(it) }.getOrNull() }
    val elapsed = start?.let { formatElapsedDuration(it, if (running) now else end ?: now) }
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { expanded = !expanded },
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
        shape = RoundedCornerShape(14.dp),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp, vertical = 10.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(if (running) MaterialTheme.colorScheme.primary else MutedText),
                    )
                    Text(
                        text = listOfNotNull("已处理", elapsed).joinToString(" "),
                        style = MaterialTheme.typography.labelLarge,
                        color = CodeHeaderText,
                        fontWeight = FontWeight.Bold,
                    )
                }
                Icon(
                    if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = if (expanded) "Hide process" else "Show process",
                    tint = CodeHeaderText,
                )
            }
            if (expanded) {
                Column(
                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    processMessages.forEach { message ->
                        val body = message.text.ifBlank { message.preview }
                        if (body.isNotBlank()) {
                            Surface(
                                modifier = Modifier.fillMaxWidth(),
                                color = Color(0xFFF0F0F4),
                                shape = RoundedCornerShape(10.dp),
                            ) {
                                Text(
                                    text = body.take(2_000),
                                    modifier = Modifier.padding(10.dp),
                                    style = MaterialTheme.typography.bodySmall,
                                )
                            }
                        }
                    }
                    if (items.isNotEmpty()) {
                        ToolItemList(items = items)
                    }
                }
            }
        }
    }
}

private data class ToolDisplayItem(
    val id: String,
    val title: String,
    val body: String,
    val completed: Boolean,
)

private fun buildToolDisplayItems(items: List<MessageItem>): List<ToolDisplayItem> {
    val calls = linkedMapOf<String, MessageItem.ToolCall>()
    val results = linkedMapOf<String, MutableList<MessageItem.ToolResult>>()
    items.forEach { item ->
        when (item) {
            is MessageItem.ToolCall -> calls[item.toolCallId.ifBlank { "tool-${item.index}" }] = item
            is MessageItem.ToolResult -> results.getOrPut(item.toolCallId.ifBlank { "tool-${item.index}" }) { mutableListOf() } += item
            else -> Unit
        }
    }
    val display = mutableListOf<ToolDisplayItem>()
    calls.forEach { (id, call) ->
        val callResults = results.remove(id).orEmpty().ifEmpty {
            val name = call.toolName.ifBlank { call.toolCallId }
            val matchingKey = results.entries.singleOrNull { (_, values) ->
                values.any { it.toolName == name && name.isNotBlank() }
            }?.key
            if (matchingKey == null) emptyList() else results.remove(matchingKey).orEmpty()
        }
        display += call.toDisplayItem(id, callResults)
    }
    results.forEach { (id, orphanResults) ->
        display += orphanResults.first().toDisplayItem(id, orphanResults)
    }
    return display
}

private fun List<MessageItem>.hasOpenToolCall(): Boolean {
    val resultIds = filterIsInstance<MessageItem.ToolResult>()
        .map { it.toolCallId }
        .filter { it.isNotBlank() }
        .toSet()
    return filterIsInstance<MessageItem.ToolCall>().any { call ->
        val id = call.toolCallId
        id.isBlank() || id !in resultIds
    }
}

private fun MessageItem.ToolCall.toDisplayItem(id: String, results: List<MessageItem.ToolResult>): ToolDisplayItem {
    val name = toolName.ifBlank { toolCallId.ifBlank { "tool" } }
    val completed = results.isNotEmpty()
    val body = buildString {
        appendToolSection("参数", arguments)
        results.forEachIndexed { index, result ->
            if (isNotBlank()) append("\n\n")
            appendToolSection(if (results.size == 1) "结果" else "结果 ${index + 1}", result.context?.takeIf { it.isNotBlank() } ?: "[no textual result]")
            result.fileAttachmentIndex?.let { append("\n文件: #$it") }
        }
    }.ifBlank { "[no tool detail]" }
    return ToolDisplayItem(
        id = id,
        title = if (completed) "已运行 $name" else "正在运行 $name",
        body = body,
        completed = completed,
    )
}

private fun MessageItem.ToolResult.toDisplayItem(id: String, results: List<MessageItem.ToolResult>): ToolDisplayItem {
    val name = toolName.ifBlank { toolCallId.ifBlank { "tool" } }
    val body = buildString {
        results.forEachIndexed { index, result ->
            if (isNotBlank()) append("\n\n")
            appendToolSection(if (results.size == 1) "结果" else "结果 ${index + 1}", result.context?.takeIf { it.isNotBlank() } ?: "[no textual result]")
            result.fileAttachmentIndex?.let { append("\n文件: #$it") }
        }
    }.ifBlank { "[no textual result]" }
    return ToolDisplayItem(id = id, title = "已运行 $name", body = body, completed = true)
}

private fun StringBuilder.appendToolSection(title: String, raw: String) {
    append(title)
    append(":\n")
    append(formatToolContent(raw))
}

private fun formatToolContent(raw: String): String {
    val text = raw.trim().ifBlank { return "[empty]" }
    return runCatching {
        when {
            text.startsWith("{") -> JSONObject(text).toString(2)
            text.startsWith("[") -> JSONArray(text).toString(2)
            else -> text
        }
    }.getOrElse { text }
}

@Composable
private fun ToolCard(
    title: String,
    body: String,
    isResult: Boolean,
) {
    var expanded by remember(title, body) { mutableStateOf(false) }
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { expanded = !expanded },
        shape = RoundedCornerShape(14.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp, vertical = 10.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        if (isResult) Icons.Filled.Code else Icons.Filled.Terminal,
                        contentDescription = null,
                        tint = CodeHeaderText,
                    )
                    Text(
                        text = title,
                        style = MaterialTheme.typography.labelLarge,
                        fontWeight = FontWeight.Bold,
                        color = CodeHeaderText,
                    )
                }
                Icon(
                    if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = if (expanded) "Hide" else "Show",
                    tint = CodeHeaderText,
                )
            }
            if (expanded) {
                SelectionContainer {
                    Text(
                        text = body.take(8_000),
                        modifier = Modifier
                            .fillMaxWidth()
                            .background(Color(0xFFF0F0F4))
                            .padding(12.dp),
                        style = MaterialTheme.typography.bodySmall,
                        fontFamily = FontFamily.Monospace,
                    )
                }
            }
        }
    }
}

@Composable
private fun AttachmentList(
    attachments: List<MessageAttachment>,
    previews: Map<String, AttachmentPreviewUiState>,
    compact: Boolean,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onDownloadAttachment: (MessageAttachment) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        attachments.forEach { attachment ->
            AttachmentCard(
                attachment = attachment,
                preview = previews[attachment.previewKey()],
                compact = compact,
                onPreviewAttachment = onPreviewAttachment,
                onDownloadAttachment = onDownloadAttachment,
                onOpenAttachment = onOpenAttachment,
            )
        }
    }
}

@Composable
private fun AttachmentCard(
    attachment: MessageAttachment,
    preview: AttachmentPreviewUiState?,
    compact: Boolean,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onDownloadAttachment: (MessageAttachment) -> Unit,
    onOpenAttachment: (MessageAttachment) -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = attachment.hasLoadTarget()) {
                if (compact) onOpenAttachment(attachment) else onPreviewAttachment(attachment)
            },
    ) {
        Column(
            modifier = Modifier.padding(10.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            if (!compact) {
                Text(
                    text = "${attachment.kind.ifBlank { "file" }} · ${attachment.name.ifBlank { "attachment-${attachment.index}" }}",
                    style = MaterialTheme.typography.labelMedium,
                    fontWeight = FontWeight.SemiBold,
                )
                Text(
                    text = listOfNotNull(
                        attachment.mediaType,
                        attachment.sizeBytes?.let(::formatBytes),
                    ).joinToString(" · ").ifBlank { "Tap to preview" },
                    style = MaterialTheme.typography.bodySmall,
                )
            } else if (preview?.image == null) {
                Text(
                    text = attachment.name.ifBlank { "attachment-${attachment.index}" },
                    style = MaterialTheme.typography.labelMedium,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            AttachmentPreview(preview)
            if (attachment.hasLoadTarget() && !compact) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                    TextButton(onClick = { onPreviewAttachment(attachment) }) { Text("Preview") }
                    TextButton(onClick = { onDownloadAttachment(attachment) }) { Text("Download") }
                    TextButton(onClick = { onOpenAttachment(attachment) }) { Text("Open") }
                }
            }
        }
    }
}

@Composable
private fun AttachmentPreview(preview: AttachmentPreviewUiState?) {
    when {
        preview == null -> Unit
        preview.isLoading -> Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CircularProgressIndicator(modifier = Modifier.widthIn(max = 20.dp))
            Text("Loading preview...", style = MaterialTheme.typography.bodySmall)
        }
        preview.error != null -> Text(
            text = preview.error,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.error,
        )
        preview.image != null -> Image(
            bitmap = preview.image.asImageBitmap(),
            contentDescription = "attachment preview",
            modifier = Modifier
                .fillMaxWidth()
                .widthIn(max = 420.dp),
            contentScale = ContentScale.FillWidth,
        )
        preview.text != null -> Text(
            text = preview.text,
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceVariant)
                .padding(8.dp),
        )
        preview.detail != null -> Text(
            text = preview.detail,
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

private fun MessageAttachment.previewKey(): String = listOf(url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl)
    .firstOrNull { it.isNotBlank() }
    ?: "$index:$name"

private fun MessageAttachment.hasLoadTarget(): Boolean = listOf(url, uri, fileUri, path, filePath, workspacePath, relativePath, src, dataUrl, dataBase64, data)
    .any { it.isNotBlank() }

private sealed interface MarkdownBlock {
    data class Text(val text: String) : MarkdownBlock
    data class Code(val language: String, val code: String) : MarkdownBlock
}

private fun markdownBlocks(text: String): List<MarkdownBlock> {
    if (text.isBlank()) return listOf(MarkdownBlock.Text(""))
    val blocks = mutableListOf<MarkdownBlock>()
    val pendingText = StringBuilder()
    val pendingCode = StringBuilder()
    var inCode = false
    var language = ""
    text.lines().forEach { line ->
        if (line.startsWith("```")) {
            if (inCode) {
                blocks += MarkdownBlock.Code(language, pendingCode.toString().trimEnd())
                pendingCode.clear()
                language = ""
                inCode = false
            } else {
                if (pendingText.isNotEmpty()) {
                    blocks += MarkdownBlock.Text(pendingText.toString().trimEnd())
                    pendingText.clear()
                }
                language = line.removePrefix("```").trim()
                inCode = true
            }
        } else if (inCode) {
            pendingCode.appendLine(line)
        } else {
            pendingText.appendLine(line)
        }
    }
    if (inCode) {
        blocks += MarkdownBlock.Code(language, pendingCode.toString().trimEnd())
    }
    if (pendingText.isNotEmpty()) {
        blocks += MarkdownBlock.Text(pendingText.toString().trimEnd())
    }
    return blocks.ifEmpty { listOf(MarkdownBlock.Text(text)) }
}

private fun formatBytes(value: Long): String {
    val units = listOf("B", "KB", "MB", "GB")
    var size = value.toDouble()
    var unit = 0
    while (size >= 1024 && unit < units.lastIndex) {
        size /= 1024
        unit += 1
    }
    return if (unit == 0) {
        "${value}B"
    } else {
        "${String.format("%.1f", size)}${units[unit]}"
    }
}

private fun formatElapsedDuration(start: Instant, end: Instant): String {
    val seconds = java.time.Duration.between(start, end).seconds.coerceAtLeast(0)
    val hours = seconds / 3600
    val minutes = (seconds % 3600) / 60
    val remainingSeconds = seconds % 60
    return when {
        hours > 0 -> "${hours}h ${minutes}m ${remainingSeconds}s"
        minutes > 0 -> "${minutes}m ${remainingSeconds}s"
        else -> "${remainingSeconds}s"
    }
}

private fun formatCompactNumber(value: Long): String = when {
    value >= 1_000_000 -> "${String.format("%.1f", value / 1_000_000.0)}M"
    value >= 1_000 -> {
        val rounded = value / 1_000.0
        if (rounded >= 100) "${(rounded).toInt()}K" else "${String.format("%.1f", rounded)}K"
    }
    else -> value.toString()
}

private fun formatLocalMinute(value: String): String = runCatching {
    Instant.parse(value)
        .atZone(ZoneId.systemDefault())
        .format(LocalMinuteFormatter)
}.getOrElse { value.take(16) }

private val LocalMinuteFormatter: DateTimeFormatter = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")

@Composable
private fun Composer(
    draft: String,
    pendingAttachments: List<PendingAttachmentUiState>,
    selectionReferences: List<SelectionReferenceUiState>,
    isSending: Boolean,
    onDraftChanged: (String) -> Unit,
    onAddAttachments: (List<android.net.Uri>) -> Unit,
    onRemoveAttachment: (String) -> Unit,
    onRemoveSelectionReference: (String) -> Unit,
    onSend: () -> Unit,
) {
    val attachmentLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.GetMultipleContents(),
    ) { uris -> onAddAttachments(uris) }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (pendingAttachments.isNotEmpty()) {
            Surface(
                shape = RoundedCornerShape(18.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
            Column(modifier = Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                pendingAttachments.forEach { attachment ->
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(modifier = Modifier.weight(1f)) {
                            Text(text = attachment.name, style = MaterialTheme.typography.labelMedium)
                            Text(
                                text = listOfNotNull(
                                    attachment.mediaType,
                                    attachment.sizeBytes?.let(::formatBytes),
                                ).joinToString(" · ").ifBlank { "attachment" },
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                        IconButton(onClick = { onRemoveAttachment(attachment.uri) }) {
                            Icon(Icons.Filled.Close, contentDescription = "Remove attachment")
                        }
                    }
                }
            }
            }
        }
        if (selectionReferences.isNotEmpty()) {
            Surface(
                shape = RoundedCornerShape(18.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Column(modifier = Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    selectionReferences.forEach { reference ->
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column(modifier = Modifier.weight(1f)) {
                                Text(text = reference.label ?: reference.path, style = MaterialTheme.typography.labelMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                Text(text = reference.path, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            }
                            IconButton(onClick = { onRemoveSelectionReference(reference.path) }) {
                                Icon(Icons.Filled.Close, contentDescription = "Remove reference")
                            }
                        }
                    }
                }
            }
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                IconButton(
                    onClick = { attachmentLauncher.launch("*/*") },
                    enabled = !isSending,
                ) {
                    Icon(Icons.Filled.AttachFile, contentDescription = "Attach files", tint = Color(0xFF111111))
                }
            }
            OutlinedTextField(
                value = draft,
                onValueChange = onDraftChanged,
                modifier = Modifier.weight(1f),
                placeholder = { Text("消息", color = MutedText) },
                minLines = 1,
                maxLines = 4,
                shape = RoundedCornerShape(28.dp),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedContainerColor = FrostedSurface,
                    unfocusedContainerColor = FrostedSurface,
                    disabledContainerColor = FrostedSurface,
                    focusedBorderColor = FrostedBorder,
                    unfocusedBorderColor = FrostedBorder,
                ),
            )
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = if ((draft.isNotBlank() || pendingAttachments.isNotEmpty() || selectionReferences.isNotEmpty()) && !isSending) {
                    Color(0xFF7A7A7A)
                } else {
                    Color(0x33808080)
                },
            ) {
                IconButton(
                    onClick = onSend,
                    enabled = (draft.isNotBlank() || pendingAttachments.isNotEmpty() || selectionReferences.isNotEmpty()) && !isSending,
                ) {
                    Icon(
                        Icons.AutoMirrored.Filled.Send,
                        contentDescription = if (isSending) "Sending" else "Send",
                        tint = Color.White,
                    )
                }
            }
        }
        Spacer(modifier = Modifier.height(6.dp))
    }
}
