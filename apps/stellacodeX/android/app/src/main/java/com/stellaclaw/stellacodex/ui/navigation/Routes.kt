package com.stellaclaw.stellacodex.ui.navigation

import android.net.Uri

sealed class AppRoute(val route: String) {
    data object Connections : AppRoute("connections")
    data object Conversations : AppRoute("conversations")
    data object Chat : AppRoute("conversations/{conversationId}?foregroundSessionId={foregroundSessionId}") {
        fun create(conversationId: String, foregroundSessionId: String = "main"): String =
            "conversations/${Uri.encode(conversationId)}?foregroundSessionId=${Uri.encode(foregroundSessionId.ifBlank { "main" })}"
    }
    data object Workspace : AppRoute("conversations/{conversationId}/workspace?path={path}") {
        fun create(conversationId: String, path: String = ""): String =
            "conversations/${Uri.encode(conversationId)}/workspace?path=${Uri.encode(path)}"
    }
    data object Settings : AppRoute("settings")
    data object Logs : AppRoute("logs")
}
