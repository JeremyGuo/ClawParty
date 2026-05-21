package com.stellaclaw.stellacodex.ui.conversations

import android.Manifest
import android.app.Application
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Article
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.ForegroundSessionSummary
import com.stellaclaw.stellacodex.ui.chat.AgentNotificationCenter
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val ListBackground = Color(0xFFF4F4F7)
private val ListSurface = Color(0xF6FFFFFF)
private val ListBorder = Color(0x1A000000)
private val ListMutedText = Color(0xFF8E8E93)
private val OnlineGreen = Color(0xFF30D681)
private val UnreadRed = Color(0xFFFF453A)
private val AvatarBlue = Color(0xFF0A95FF)
private val AvatarPurple = Color(0xFF7A45FF)

@Composable
fun ConversationListScreen(
    onOpenConversation: (String, String) -> Unit,
    onOpenSettings: () -> Unit,
    onOpenLogs: () -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ConversationListViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ConversationListViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    val lifecycleOwner = LocalLifecycleOwner.current
    var renameConversation by remember { mutableStateOf<ConversationSummary?>(null) }
    var deleteConversation by remember { mutableStateOf<ConversationSummary?>(null) }
    var renameSession by remember { mutableStateOf<Pair<ConversationSummary, ForegroundSessionSummary>?>(null) }
    var deleteSession by remember { mutableStateOf<Pair<ConversationSummary, ForegroundSessionSummary>?>(null) }
    val notificationPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }

    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !AgentNotificationCenter.canNotify(application)) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                viewModel.refreshOnResume()
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    LaunchedEffect(state.pendingOpenConversationId) {
        val conversationId = state.pendingOpenConversationId ?: return@LaunchedEffect
        val foregroundSessionId = state.pendingOpenForegroundSessionId ?: "main"
        viewModel.consumePendingOpenConversation()
        onOpenConversation(conversationId, foregroundSessionId)
    }

    Scaffold(containerColor = ListBackground) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .background(ListBackground),
        ) {
            ConversationListHeader(
                connectionName = state.activeConnectionName,
                isCreating = state.isCreating,
                onCreate = viewModel::createConversation,
                onOpenSettings = onOpenSettings,
                onOpenLogs = onOpenLogs,
            )
            when {
                state.isLoading -> LoadingState()
                state.error != null -> ErrorState(
                    message = state.error.orEmpty(),
                    onRetry = viewModel::refresh,
                )
                state.conversations.isEmpty() -> EmptyState(
                    isCreating = state.isCreating,
                    onCreate = viewModel::createConversation,
                    onRetry = viewModel::refresh,
                )
                else -> ConversationList(
                    conversations = state.conversations,
                    onOpenConversation = onOpenConversation,
                    onRenameConversation = { renameConversation = it },
                    onDeleteConversation = { deleteConversation = it },
                    onCreateSession = viewModel::createForegroundSession,
                    onRenameSession = { conversation, session -> renameSession = conversation to session },
                    onDeleteSession = { conversation, session -> deleteSession = conversation to session },
                )
            }
        }
    }

    renameConversation?.let { conversation ->
        RenameDialog(
            title = "Rename conversation",
            initial = conversation.displayName,
            onDismiss = { renameConversation = null },
            onConfirm = { name ->
                viewModel.renameConversation(conversation.conversationId, name)
                renameConversation = null
            },
        )
    }
    deleteConversation?.let { conversation ->
        ConfirmDialog(
            title = "Delete conversation",
            text = "Delete ${conversation.displayName.ifBlank { conversation.conversationId }}?",
            onDismiss = { deleteConversation = null },
            onConfirm = {
                viewModel.deleteConversation(conversation.conversationId)
                deleteConversation = null
            },
        )
    }
    renameSession?.let { (conversation, session) ->
        RenameDialog(
            title = "Rename session",
            initial = session.displayName,
            onDismiss = { renameSession = null },
            onConfirm = { name ->
                viewModel.renameForegroundSession(conversation.conversationId, session.id, name)
                renameSession = null
            },
        )
    }
    deleteSession?.let { (conversation, session) ->
        ConfirmDialog(
            title = "Delete session",
            text = "Delete ${session.displayName.ifBlank { session.id }}?",
            onDismiss = { deleteSession = null },
            onConfirm = {
                viewModel.deleteForegroundSession(conversation.conversationId, session.id)
                deleteSession = null
            },
        )
    }
}

@Composable
private fun ConversationListHeader(
    connectionName: String,
    isCreating: Boolean,
    onCreate: () -> Unit,
    onOpenSettings: () -> Unit,
    onOpenLogs: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .statusBarsPadding()
            .padding(horizontal = 22.dp, vertical = 18.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = AvatarPurple,
            ) {
                Box(contentAlignment = Alignment.Center) {
                    Icon(Icons.Filled.AutoAwesome, contentDescription = null, tint = Color.White)
                }
            }
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    text = "StellaClaw",
                    style = MaterialTheme.typography.headlineMedium,
                    fontWeight = FontWeight.Bold,
                )
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(7.dp)) {
                    Box(
                        modifier = Modifier
                            .size(12.dp)
                            .clip(CircleShape)
                            .background(OnlineGreen),
                    )
                    Text("在线 - $connectionName ›", style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onOpenLogs) {
                Icon(Icons.Filled.Article, contentDescription = "Logs")
            }
            IconButton(onClick = onOpenSettings) {
                Icon(Icons.Filled.Settings, contentDescription = "Settings")
            }
            IconButton(onClick = onCreate, enabled = !isCreating) {
                Icon(
                    Icons.Filled.Add,
                    contentDescription = if (isCreating) "Creating" else "New conversation",
                    modifier = Modifier.size(36.dp),
                    tint = Color(0xFF111111),
                )
            }
        }
    }
}

@Composable
private fun RenameDialog(
    title: String,
    initial: String,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    var value by remember(initial) { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = value,
                onValueChange = { value = it },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = { TextButton(onClick = { onConfirm(value) }, enabled = value.isNotBlank()) { Text("Save") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ConfirmDialog(
    title: String,
    text: String,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(text) },
        confirmButton = { TextButton(onClick = onConfirm) { Text("Delete") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ConversationList(
    conversations: List<ConversationSummary>,
    onOpenConversation: (String, String) -> Unit,
    onRenameConversation: (ConversationSummary) -> Unit,
    onDeleteConversation: (ConversationSummary) -> Unit,
    onCreateSession: (String) -> Unit,
    onRenameSession: (ConversationSummary, ForegroundSessionSummary) -> Unit,
    onDeleteSession: (ConversationSummary, ForegroundSessionSummary) -> Unit,
) {
    var expandedConversationIds by remember { mutableStateOf(setOf<String>()) }
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(horizontal = 18.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(18.dp),
    ) {
        items(conversations, key = { it.conversationId }) { conversation ->
            ConversationRow(
                conversation = conversation,
                expanded = expandedConversationIds.contains(conversation.conversationId),
                onToggleExpanded = {
                    expandedConversationIds = if (expandedConversationIds.contains(conversation.conversationId)) {
                        expandedConversationIds - conversation.conversationId
                    } else {
                        expandedConversationIds + conversation.conversationId
                    }
                },
                onClick = { onOpenConversation(conversation.conversationId, conversation.foregroundSessionId.ifBlank { "main" }) },
                onOpenSession = { sessionId -> onOpenConversation(conversation.conversationId, sessionId) },
                onRenameConversation = { onRenameConversation(conversation) },
                onDeleteConversation = { onDeleteConversation(conversation) },
                onCreateSession = { onCreateSession(conversation.conversationId) },
                onRenameSession = { session -> onRenameSession(conversation, session) },
                onDeleteSession = { session -> onDeleteSession(conversation, session) },
            )
        }
    }
}

@Composable
private fun LoadingState() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator()
        Text("Loading conversations...")
    }
}

@Composable
private fun ErrorState(message: String, onRetry: () -> Unit) {
    Column(
        modifier = Modifier.padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = message,
            color = MaterialTheme.colorScheme.error,
        )
        Button(onClick = onRetry) { Text("Retry") }
    }
}

@Composable
private fun EmptyState(
    isCreating: Boolean,
    onCreate: () -> Unit,
    onRetry: () -> Unit,
) {
    Column(
        modifier = Modifier.padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("No conversations yet.")
        Button(onClick = onCreate, enabled = !isCreating) { Text(if (isCreating) "Creating..." else "Create conversation") }
        Button(onClick = onRetry) { Text("Refresh") }
    }
}

@Composable
private fun ConversationRow(
    conversation: ConversationSummary,
    expanded: Boolean,
    onToggleExpanded: () -> Unit,
    onClick: () -> Unit,
    onOpenSession: (String) -> Unit,
    onRenameConversation: () -> Unit,
    onDeleteConversation: () -> Unit,
    onCreateSession: () -> Unit,
    onRenameSession: (ForegroundSessionSummary) -> Unit,
    onDeleteSession: (ForegroundSessionSummary) -> Unit,
) {
    var menuOpen by remember { mutableStateOf(false) }
    val sessions = conversation.foregroundSessions
    val hasSessionDropdown = sessions.size > 1
    Column(modifier = Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .clickable(onClick = { if (hasSessionDropdown) onToggleExpanded() else onClick() }),
            horizontalArrangement = Arrangement.spacedBy(16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ConversationAvatar(conversation)
            Column(
                modifier = Modifier.weight(1f),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = conversation.displayName.ifBlank { conversation.conversationId },
                        modifier = Modifier.weight(1f),
                        style = MaterialTheme.typography.headlineSmall,
                        fontWeight = FontWeight.Bold,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = formatConversationTime(conversation.lastMessageTime),
                        style = MaterialTheme.typography.bodyMedium,
                        color = ListMutedText.copy(alpha = 0.65f),
                        maxLines = 1,
                    )
                    if (hasSessionDropdown) {
                        IconButton(onClick = onToggleExpanded) {
                            Icon(
                                if (expanded) Icons.Filled.KeyboardArrowUp else Icons.Filled.KeyboardArrowDown,
                                contentDescription = if (expanded) "Collapse sessions" else "Expand sessions",
                            )
                        }
                    }
                    Box {
                        IconButton(onClick = { menuOpen = true }) { Icon(Icons.Filled.MoreVert, contentDescription = "Conversation actions") }
                        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                            DropdownMenuItem(text = { Text("Open active session") }, onClick = { menuOpen = false; onClick() })
                            DropdownMenuItem(text = { Text("Rename") }, onClick = { menuOpen = false; onRenameConversation() })
                            DropdownMenuItem(text = { Text("New session") }, onClick = { menuOpen = false; onCreateSession() })
                            DropdownMenuItem(text = { Text("Delete") }, onClick = { menuOpen = false; onDeleteConversation() })
                        }
                    }
                }
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = if (hasSessionDropdown) sessionDropdownPreview(conversation) else conversationPreview(conversation),
                        modifier = Modifier.weight(1f),
                        style = MaterialTheme.typography.titleMedium,
                        color = ListMutedText,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    ConversationStatusIndicator(conversation = conversation)
                }
            }
        }
        if (hasSessionDropdown && expanded) {
            Surface(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 90.dp),
                shape = RoundedCornerShape(14.dp),
                color = ListSurface,
                border = BorderStroke(1.dp, ListBorder),
            ) {
                Column(modifier = Modifier.padding(vertical = 6.dp)) {
                    sessions.forEach { session ->
                        ForegroundSessionRow(
                            session = session,
                            active = session.id == conversation.foregroundSessionId,
                            onOpen = { onOpenSession(session.id) },
                            onRename = { onRenameSession(session) },
                            onDelete = { onDeleteSession(session) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun ForegroundSessionRow(
    session: ForegroundSessionSummary,
    active: Boolean,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onDelete: () -> Unit,
) {
    var menuOpen by remember { mutableStateOf(false) }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onOpen)
            .padding(start = 12.dp, end = 4.dp, top = 8.dp, bottom = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(if (session.running) 12.dp else 9.dp)
                .background(
                    when {
                        session.running -> OnlineGreen
                        session.hasUnread -> UnreadRed
                        active -> AvatarBlue
                        else -> ListMutedText.copy(alpha = 0.45f)
                    },
                    CircleShape,
                ),
        )
        Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(
                text = session.displayName.ifBlank { session.id },
                style = MaterialTheme.typography.titleMedium,
                fontWeight = if (active) FontWeight.Bold else FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                text = foregroundSessionPreview(session),
                style = MaterialTheme.typography.bodySmall,
                color = ListMutedText,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Text(
            text = formatConversationTime(session.lastMessageTime),
            style = MaterialTheme.typography.bodySmall,
            color = ListMutedText.copy(alpha = 0.65f),
            maxLines = 1,
        )
        Box {
            IconButton(onClick = { menuOpen = true }) { Icon(Icons.Filled.MoreVert, contentDescription = "Session actions") }
            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                DropdownMenuItem(text = { Text("Open") }, onClick = { menuOpen = false; onOpen() })
                DropdownMenuItem(text = { Text("Rename") }, onClick = { menuOpen = false; onRename() })
                DropdownMenuItem(
                    text = { Text("Delete") },
                    onClick = { menuOpen = false; onDelete() },
                    enabled = !session.id.equals("main", ignoreCase = true),
                )
            }
        }
    }
}

@Composable
private fun ConversationAvatar(conversation: ConversationSummary) {
    Box(modifier = Modifier.size(74.dp), contentAlignment = Alignment.TopEnd) {
        Surface(
            modifier = Modifier
                .size(68.dp)
                .align(Alignment.CenterStart),
            shape = RoundedCornerShape(20.dp),
            color = avatarColor(conversation.conversationId),
            border = BorderStroke(1.dp, Color.White.copy(alpha = 0.7f)),
        ) {
            Box(contentAlignment = Alignment.Center) {
                Text(
                    text = avatarText(conversation.displayName.ifBlank { conversation.conversationId }),
                    style = MaterialTheme.typography.headlineMedium,
                    fontWeight = FontWeight.Bold,
                    color = Color.White,
                )
            }
        }
        if (conversation.hasUnread) {
            Surface(
                modifier = Modifier.size(26.dp),
                shape = CircleShape,
                color = UnreadRed,
            ) {
                Box(contentAlignment = Alignment.Center) {
                    Text("•", color = Color.White, fontWeight = FontWeight.Bold)
                }
            }
        }
    }
}

@Composable
private fun ConversationStatusIndicator(conversation: ConversationSummary) {
    Box(
        modifier = Modifier.size(26.dp),
        contentAlignment = Alignment.Center,
    ) {
        when {
            conversation.running -> CircularProgressIndicator(
                modifier = Modifier.size(18.dp),
                strokeWidth = 2.dp,
                color = OnlineGreen,
            )
            conversation.hasUnread -> Box(
                modifier = Modifier
                    .size(10.dp)
                    .background(UnreadRed, CircleShape),
            )
        }
    }
}

private fun conversationPreview(conversation: ConversationSummary): String = when {
    conversation.running -> "Assistant 正在处理 · ${conversation.processingState}"
    conversation.modelSelectionPending -> "请选择模型后继续"
    conversation.messageCount > 0 -> "${conversation.messageCount} 条消息 · ${conversation.model.ifBlank { "default model" }}"
    else -> "新的会话，点击开始聊天"
}

private fun sessionDropdownPreview(conversation: ConversationSummary): String {
    val sessions = conversation.foregroundSessions
    val activeSessionId = conversation.foregroundSessionId.ifBlank { "main" }
    val active = sessions.firstOrNull { it.id == activeSessionId }
    val unreadCount = sessions.count { it.hasUnread }
    val runningCount = sessions.count { it.running }
    val parts = mutableListOf("${sessions.size} sessions")
    active?.let { parts += "active: ${it.displayName.ifBlank { it.id }}" }
    if (runningCount > 0) parts += "$runningCount running"
    if (unreadCount > 0) parts += "$unreadCount unread"
    return parts.joinToString(" · ")
}

private fun foregroundSessionPreview(session: ForegroundSessionSummary): String = when {
    session.running -> "Assistant 正在处理 · ${session.state}"
    session.messageCount > 0 -> "${session.messageCount} 条消息"
    else -> "新的 session"
}

private fun avatarText(value: String): String = value.trim().take(1).ifBlank { "S" }.uppercase()

private fun avatarColor(seed: String): Color {
    val colors = listOf(AvatarBlue, AvatarPurple, Color(0xFF20C997), Color(0xFFFF9F0A), Color(0xFF5856D6))
    val index = seed.fold(0) { acc, c -> acc + c.code }.let { kotlin.math.abs(it) % colors.size }
    return colors[index]
}

private fun formatConversationTime(value: String?): String {
    if (value.isNullOrBlank()) return ""
    return runCatching {
        Instant.parse(value)
            .atZone(ZoneId.systemDefault())
            .format(ConversationTimeFormatter)
    }.getOrElse { value.take(16) }
}

private val ConversationTimeFormatter: DateTimeFormatter = DateTimeFormatter.ofPattern("HH:mm")
