package com.stellaclaw.stellacodex.ui.workspace

import android.app.Application
import android.content.ContentResolver
import android.content.ContentValues
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.WorkspaceEntry
import com.stellaclaw.stellacodex.domain.model.WorkspaceListing
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import java.io.File

class WorkspaceViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()
    private val mutableState = MutableStateFlow(WorkspaceUiState())
    val state: StateFlow<WorkspaceUiState> = mutableState.asStateFlow()
    private val coroutineErrorHandler = CoroutineExceptionHandler { _, throwable ->
        mutableState.update { it.copy(isLoading = false, isWorking = false, error = throwable.message ?: "Unexpected workspace error") }
    }

    fun load(conversationId: String, path: String) {
        val normalized = normalizePath(path)
        mutableState.update { it.copy(conversationId = conversationId, path = normalized, isLoading = true, error = null, status = null) }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            when (val result = api.loadWorkspace(profile, conversationId, normalized, limit = 500)) {
                is AppResult.Ok -> mutableState.update { it.copy(isLoading = false, listing = result.value, path = result.value.path, preview = null) }
                is AppResult.Err -> mutableState.update { it.copy(isLoading = false, error = result.error.userMessage()) }
            }
        }
    }

    fun previewFile(entry: WorkspaceEntry) {
        val conversationId = state.value.conversationId
        mutableState.update { it.copy(isWorking = true, error = null, preview = WorkspacePreview(path = entry.path, name = entry.name, isLoading = true)) }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            when (val result = api.fetchWorkspaceFile(profile, conversationId, entry.path, limitBytes = 2_000_000)) {
                is AppResult.Ok -> {
                    val text = if (isText(entry.name, result.value.mediaType, result.value.bytes.size)) {
                        result.value.bytes.toString(Charsets.UTF_8).take(32_000)
                    } else null
                    mutableState.update {
                        it.copy(
                            isWorking = false,
                            preview = WorkspacePreview(
                                path = entry.path,
                                name = entry.name,
                                mediaType = result.value.mediaType,
                                text = text,
                                detail = if (text == null) "${formatBytes(result.value.bytes.size.toLong())} loaded; use download to save." else null,
                            ),
                        )
                    }
                }
                is AppResult.Err -> mutableState.update { it.copy(isWorking = false, preview = null, error = result.error.userMessage()) }
            }
        }
    }

    fun download(entry: WorkspaceEntry) {
        val conversationId = state.value.conversationId
        mutableState.update { it.copy(isWorking = true, status = "Downloading ${entry.name}...", error = null) }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            val result = if (entry.isDirectory) {
                api.downloadWorkspaceArchive(profile, conversationId, entry.path)
            } else {
                api.fetchWorkspaceFile(profile, conversationId, entry.path, limitBytes = 50_000_000)
            }
            when (result) {
                is AppResult.Ok -> {
                    val fileName = if (entry.isDirectory) "${entry.name.ifBlank { "workspace" }}.tar.gz" else entry.name.ifBlank { "workspace-file" }
                    val saved = saveToDownloads(fileName.safeFileName(), result.value.bytes, result.value.mediaType ?: "application/octet-stream")
                    mutableState.update { it.copy(isWorking = false, status = "Saved $saved") }
                }
                is AppResult.Err -> mutableState.update { it.copy(isWorking = false, error = result.error.userMessage(), status = null) }
            }
        }
    }

    fun delete(entry: WorkspaceEntry) {
        val conversationId = state.value.conversationId
        mutableState.update { it.copy(isWorking = true, error = null, status = "Deleting ${entry.name}...") }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            when (val result = api.deleteWorkspacePath(profile, conversationId, entry.path)) {
                is AppResult.Ok -> {
                    mutableState.update { it.copy(isWorking = false, status = "Deleted ${entry.name}", preview = null) }
                    load(conversationId, state.value.path)
                }
                is AppResult.Err -> mutableState.update { it.copy(isWorking = false, error = result.error.userMessage(), status = null) }
            }
        }
    }

    fun move(path: String, newPath: String) {
        val conversationId = state.value.conversationId
        mutableState.update { it.copy(isWorking = true, error = null, status = "Moving...") }
        viewModelScope.launch(coroutineErrorHandler) {
            val profile = store.profile.first()
            when (val result = api.moveWorkspacePath(profile, conversationId, path, newPath)) {
                is AppResult.Ok -> {
                    mutableState.update { it.copy(isWorking = false, status = "Moved to ${normalizePath(newPath)}", preview = null) }
                    load(conversationId, parentPath(normalizePath(newPath)))
                }
                is AppResult.Err -> mutableState.update { it.copy(isWorking = false, error = result.error.userMessage(), status = null) }
            }
        }
    }

    fun uploadArchive(uri: Uri, resolver: ContentResolver) {
        val snapshot = state.value
        mutableState.update { it.copy(isWorking = true, status = "Uploading archive...", error = null) }
        viewModelScope.launch(coroutineErrorHandler) {
            val name = displayName(uri, resolver)
            if (!name.endsWith(".tar.gz", ignoreCase = true) && !name.endsWith(".tgz", ignoreCase = true)) {
                mutableState.update { it.copy(isWorking = false, status = null, error = "Workspace upload expects a .tar.gz or .tgz archive.") }
                return@launch
            }
            val bytes = resolver.openInputStream(uri)?.use { it.readBytes() } ?: ByteArray(0)
            val profile = store.profile.first()
            when (val result = api.uploadWorkspaceArchive(profile, snapshot.conversationId, snapshot.path, bytes)) {
                is AppResult.Ok -> {
                    mutableState.update { it.copy(isWorking = false, status = "Archive uploaded") }
                    load(snapshot.conversationId, snapshot.path)
                }
                is AppResult.Err -> mutableState.update { it.copy(isWorking = false, error = result.error.userMessage(), status = null) }
            }
        }
    }

    fun closePreview() {
        mutableState.update { it.copy(preview = null) }
    }

    private fun saveToDownloads(fileName: String, bytes: ByteArray, mediaType: String): String {
        val resolver = getApplication<Application>().contentResolver
        val safeName = fileName.safeFileName()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, safeName)
                put(MediaStore.Downloads.MIME_TYPE, mediaType)
                put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/StellacodeX")
            }
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: error("Unable to create download")
            resolver.openOutputStream(uri)?.use { it.write(bytes) } ?: error("Unable to write download")
            return "Downloads/StellacodeX/$safeName"
        }
        val dir = File(getApplication<Application>().getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS), "StellacodeX").apply { mkdirs() }
        File(dir, safeName).writeBytes(bytes)
        return File(dir, safeName).absolutePath
    }

    private fun String.safeFileName(): String {
        val cleaned = replace(Regex("[\\\\/:*?\"<>|]"), "_").replace(Regex("\\s+"), " ").trim().ifBlank { "workspace-file" }
        if (cleaned.length <= 120) return cleaned
        val extension = cleaned.substringAfterLast('.', missingDelimiterValue = "").takeIf { it.length in 1..12 }
        val stemLimit = if (extension == null) 120 else 119 - extension.length
        val stem = cleaned.substringBeforeLast('.', cleaned).take(stemLimit).trimEnd('.', ' ')
        return if (extension == null) stem else "$stem.$extension"
    }

    private fun displayName(uri: Uri, resolver: ContentResolver): String {
        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
            val index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (index >= 0 && cursor.moveToFirst()) return cursor.getString(index).orEmpty()
        }
        return uri.lastPathSegment.orEmpty()
    }
}

data class WorkspaceUiState(
    val conversationId: String = "",
    val path: String = "",
    val isLoading: Boolean = false,
    val isWorking: Boolean = false,
    val listing: WorkspaceListing? = null,
    val preview: WorkspacePreview? = null,
    val status: String? = null,
    val error: String? = null,
)

data class WorkspacePreview(
    val path: String,
    val name: String,
    val isLoading: Boolean = false,
    val mediaType: String? = null,
    val text: String? = null,
    val detail: String? = null,
)

fun normalizePath(path: String): String = path.trim().trimStart('/').trimEnd('/')

fun parentPath(path: String): String = normalizePath(path).substringBeforeLast('/', missingDelimiterValue = "")

private fun isText(name: String, mediaType: String?, size: Int): Boolean {
    if (size > 2_000_000) return false
    if (mediaType?.startsWith("text/") == true || mediaType == "application/json") return true
    return name.substringAfterLast('.', "").lowercase() in setOf("md", "txt", "log", "json", "xml", "kt", "java", "rs", "js", "ts", "tsx", "jsx", "py", "toml", "yaml", "yml", "gradle", "kts", "css", "html")
}

private fun formatBytes(value: Long): String = when {
    value >= 1024L * 1024L -> "%.1f MB".format(value / 1024.0 / 1024.0)
    value >= 1024L -> "%.1f KB".format(value / 1024.0)
    else -> "$value B"
}
