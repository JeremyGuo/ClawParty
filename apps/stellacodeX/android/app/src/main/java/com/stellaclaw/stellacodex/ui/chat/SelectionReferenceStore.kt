package com.stellaclaw.stellacodex.ui.chat

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

object SelectionReferenceStore {
    private val mutableReferences = MutableStateFlow<List<SelectionReferenceUiState>>(emptyList())
    val references = mutableReferences.asStateFlow()

    fun add(reference: SelectionReferenceUiState) {
        mutableReferences.update { current ->
            if (current.any { it.conversationId == reference.conversationId && it.path == reference.path }) current else current + reference
        }
    }

    fun consume(conversationId: String): List<SelectionReferenceUiState> {
        val matching = mutableReferences.value.filter { it.conversationId == conversationId }
        if (matching.isNotEmpty()) {
            mutableReferences.update { current -> current.filterNot { it.conversationId == conversationId } }
        }
        return matching
    }
}

data class SelectionReferenceUiState(
    val conversationId: String,
    val path: String,
    val label: String? = null,
    val mediaType: String? = null,
    val selectedText: String,
)
