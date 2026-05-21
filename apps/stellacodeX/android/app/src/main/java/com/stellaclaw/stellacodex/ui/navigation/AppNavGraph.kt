package com.stellaclaw.stellacodex.ui.navigation

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import com.stellaclaw.stellacodex.ui.chat.ChatScreen
import com.stellaclaw.stellacodex.ui.connections.ConnectionsScreen
import com.stellaclaw.stellacodex.ui.conversations.ConversationListScreen
import com.stellaclaw.stellacodex.ui.logs.LogsScreen
import com.stellaclaw.stellacodex.ui.settings.SettingsScreen
import com.stellaclaw.stellacodex.ui.workspace.WorkspaceScreen

@Composable
fun AppNavGraph(requestedConversationId: String? = null) {
    val navController = rememberNavController()

    LaunchedEffect(requestedConversationId) {
        val conversationId = requestedConversationId?.takeIf { it.isNotBlank() } ?: return@LaunchedEffect
        navController.navigate(AppRoute.Chat.create(conversationId)) {
            launchSingleTop = true
            popUpTo(AppRoute.Connections.route) { inclusive = false }
        }
    }

    NavHost(
        navController = navController,
        startDestination = AppRoute.Connections.route,
    ) {
        composable(AppRoute.Connections.route) {
            ConnectionsScreen(
                onContinue = { navController.navigate(AppRoute.Conversations.route) },
            )
        }
        composable(AppRoute.Conversations.route) {
            ConversationListScreen(
                onOpenConversation = { id, foregroundSessionId -> navController.navigate(AppRoute.Chat.create(id, foregroundSessionId)) },
                onOpenSettings = { navController.navigate(AppRoute.Settings.route) },
                onOpenLogs = { navController.navigate(AppRoute.Logs.route) },
            )
        }
        composable(AppRoute.Chat.route) { backStackEntry ->
            ChatScreen(
                conversationId = backStackEntry.arguments?.getString("conversationId").orEmpty(),
                foregroundSessionId = backStackEntry.arguments?.getString("foregroundSessionId") ?: "main",
                onBack = { navController.popBackStack() },
                onOpenWorkspace = { conversationId ->
                    navController.navigate(AppRoute.Workspace.create(conversationId))
                },
            )
        }
        composable(AppRoute.Workspace.route) { backStackEntry ->
            WorkspaceScreen(
                conversationId = backStackEntry.arguments?.getString("conversationId").orEmpty(),
                path = backStackEntry.arguments?.getString("path") ?: "/",
                onBack = { navController.popBackStack() },
                onOpenPath = { path -> navController.navigate(AppRoute.Workspace.create(backStackEntry.arguments?.getString("conversationId").orEmpty(), path)) },
            )
        }
        composable(AppRoute.Settings.route) {
            SettingsScreen(onBack = { navController.popBackStack() })
        }
        composable(AppRoute.Logs.route) {
            LogsScreen(onBack = { navController.popBackStack() })
        }
    }
}
