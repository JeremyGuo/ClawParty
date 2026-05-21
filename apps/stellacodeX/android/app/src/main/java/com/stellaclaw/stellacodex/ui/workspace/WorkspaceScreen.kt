package com.stellaclaw.stellacodex.ui.workspace

import android.app.Application
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Download
import androidx.compose.material.icons.filled.DriveFileRenameOutline
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.InsertDriveFile
import androidx.compose.material.icons.filled.UploadFile
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.WorkspaceEntry
import com.stellaclaw.stellacodex.ui.chat.SelectionReferenceStore
import com.stellaclaw.stellacodex.ui.chat.SelectionReferenceUiState

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun WorkspaceScreen(
    conversationId: String,
    path: String,
    onBack: () -> Unit,
    onOpenPath: (String) -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val resolver = LocalContext.current.contentResolver
    val viewModel: WorkspaceViewModel = viewModel(
        factory = viewModelFactory { initializer { WorkspaceViewModel(application) } },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    val uploadLauncher = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) viewModel.uploadArchive(uri, resolver)
    }
    var movingEntry by remember { mutableStateOf<WorkspaceEntry?>(null) }
    var moveTarget by remember { mutableStateOf("") }

    LaunchedEffect(conversationId, path) {
        viewModel.load(conversationId, path)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(if (state.path.isBlank()) "Workspace" else state.path, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
                actions = {
                    IconButton(onClick = { uploadLauncher.launch(arrayOf("application/gzip", "application/x-gtar", "application/x-tar", "application/octet-stream")) }, enabled = !state.isWorking) {
                        Icon(Icons.Filled.UploadFile, contentDescription = "Upload tar.gz archive")
                    }
                },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            state.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            state.status?.let { Text(it, color = MaterialTheme.colorScheme.primary) }
            state.listing?.let { listing ->
                Text(
                    text = listing.workspaceRoot.ifBlank { conversationId },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (state.isLoading) {
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp), verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator()
                    Text("Loading workspace...")
                }
            } else {
                LazyColumn(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    state.listing?.parent?.let { parent ->
                        item {
                            WorkspacePathRow(name = "..", detail = parent.ifBlank { "/" }, isDirectory = true, onClick = { onOpenPath(parent) })
                        }
                    }
                    items(state.listing?.entries.orEmpty(), key = { it.path }) { entry ->
                        WorkspaceEntryRow(
                            entry = entry,
                            enabled = !state.isWorking,
                            onOpen = { if (entry.isDirectory) onOpenPath(entry.path) else viewModel.previewFile(entry) },
                            onDownload = { viewModel.download(entry) },
                            onMove = {
                                movingEntry = entry
                                moveTarget = entry.path
                            },
                            onDelete = { viewModel.delete(entry) },
                        )
                    }
                }
            }
            state.preview?.let { preview ->
                Surface(
                    modifier = Modifier.fillMaxWidth(),
                    shape = RoundedCornerShape(8.dp),
                    border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
                ) {
                    Column(modifier = Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                            Text(preview.name, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            Row {
                                TextButton(onClick = {
                                    SelectionReferenceStore.add(
                                        SelectionReferenceUiState(
                                            conversationId = conversationId,
                                            path = preview.path,
                                            label = preview.name,
                                            mediaType = preview.mediaType,
                                            selectedText = preview.text ?: "[workspace file: ${preview.path}]",
                                        ),
                                    )
                                    onBack()
                                }) { Text("Reference") }
                                IconButton(onClick = viewModel::closePreview) { Icon(Icons.Filled.Close, contentDescription = "Close preview") }
                            }
                        }
                        if (preview.isLoading) {
                            CircularProgressIndicator()
                        } else if (preview.text != null) {
                            Text(
                                text = preview.text,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .height(220.dp)
                                    .verticalScroll(rememberScrollState()),
                                fontFamily = FontFamily.Monospace,
                                style = MaterialTheme.typography.bodySmall,
                            )
                        } else {
                            Text(preview.detail ?: preview.mediaType ?: "Binary file")
                        }
                    }
                }
            }
        }
    }

    movingEntry?.let { entry ->
        AlertDialog(
            onDismissRequest = { movingEntry = null },
            title = { Text("Move or rename") },
            text = {
                OutlinedTextField(
                    value = moveTarget,
                    onValueChange = { moveTarget = it },
                    label = { Text("Workspace path") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        viewModel.move(entry.path, moveTarget)
                        movingEntry = null
                    },
                    enabled = moveTarget.isNotBlank() && moveTarget != entry.path,
                ) { Text("Move") }
            },
            dismissButton = { TextButton(onClick = { movingEntry = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun WorkspaceEntryRow(
    entry: WorkspaceEntry,
    enabled: Boolean,
    onOpen: () -> Unit,
    onDownload: () -> Unit,
    onMove: () -> Unit,
    onDelete: () -> Unit,
) {
    Surface(shape = RoundedCornerShape(8.dp), tonalElevation = 1.dp) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .clickable(enabled = enabled, onClick = onOpen)
                .padding(horizontal = 12.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(if (entry.isDirectory) Icons.Filled.Folder else Icons.Filled.InsertDriveFile, contentDescription = null)
            Column(modifier = Modifier.weight(1f)) {
                Text(entry.name, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(entryDetail(entry), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            IconButton(onClick = onDownload, enabled = enabled) { Icon(Icons.Filled.Download, contentDescription = "Download") }
            IconButton(onClick = onMove, enabled = enabled && !entry.readonly) { Icon(Icons.Filled.DriveFileRenameOutline, contentDescription = "Move") }
            IconButton(onClick = onDelete, enabled = enabled && !entry.readonly) { Icon(Icons.Filled.Delete, contentDescription = "Delete") }
        }
    }
}

@Composable
private fun WorkspacePathRow(name: String, detail: String, isDirectory: Boolean, onClick: () -> Unit) {
    Surface(shape = RoundedCornerShape(8.dp), tonalElevation = 1.dp) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .clickable(onClick = onClick)
                .padding(horizontal = 12.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(if (isDirectory) Icons.Filled.Folder else Icons.Filled.InsertDriveFile, contentDescription = null)
            Column(modifier = Modifier.weight(1f)) {
                Text(name)
                Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

private fun entryDetail(entry: WorkspaceEntry): String {
    val size = entry.sizeBytes?.let { formatWorkspaceBytes(it) } ?: entry.kind
    return if (entry.readonly) "$size · readonly" else size
}

private fun formatWorkspaceBytes(value: Long): String = when {
    value >= 1024L * 1024L -> "%.1f MB".format(value / 1024.0 / 1024.0)
    value >= 1024L -> "%.1f KB".format(value / 1024.0)
    else -> "$value B"
}
