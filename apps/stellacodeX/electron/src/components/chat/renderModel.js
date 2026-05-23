import { displayMessages, isFinalAssistantMessage, messageKey } from '../../lib/messageUtils';
import { measureChatPerf } from '../../lib/chatPerfMetrics';

export function buildChatRenderModel({
  messages,
  currentActivity,
  sending = false,
  processing = false,
  modelSelectionPending = false
} = {}) {
  const renderedMessages = measureChatPerf('chat.render_model.display_messages', () => displayMessages(messages || []), { messages: messages?.length || 0 });
  const renderEntries = measureChatPerf('chat.render_model.assistant_turn_entries', () => assistantTurnEntries(renderedMessages), { renderedMessages: renderedMessages.length });
  const entryKeys = measureChatPerf('chat.render_model.entry_keys', () => renderEntries.map((entry, index) => chatRenderEntryKey(entry, index)), { entries: renderEntries.length });
  const latestAssistantTurnIndex = latestAssistantTurnEntryIndex(renderEntries);
  const activeAssistantTurnVisible = measureChatPerf('chat.render_model.active_assistant_turn', () => hasAssistantTurnAfterLastUser(renderEntries), { entries: renderEntries.length });
  const pendingAssistantVisible = measureChatPerf('chat.render_model.pending_assistant', () => shouldShowPendingAssistant(renderEntries, currentActivity, sending, processing), { entries: renderEntries.length });
  const responseSpacerVisible = Boolean(pendingAssistantVisible && renderedMessages.length > 0 && !modelSelectionPending);
  return {
    renderedMessages,
    renderEntries,
    entryKeys,
    latestAssistantTurnIndex,
    activeAssistantTurnVisible,
    pendingAssistantVisible,
    responseSpacerVisible
  };
}

export function assistantTurnEntries(renderedMessages) {
  const entries = [];
  for (let index = 0; index < renderedMessages.length; index += 1) {
    const message = renderedMessages[index];
    if (message?.type !== 'toolGroup') {
      entries.push({ type: 'message', id: messageKey(message, index), message });
      continue;
    }
    const nextMessage = renderedMessages[index + 1];
    const finalMessage = isFinalAssistantMessage(nextMessage) ? nextMessage : null;
    entries.push({
      type: 'assistantTurn',
      id: `turn-${message.id || messageKey(message.messages?.[0], index)}`,
      processGroup: message,
      finalMessage
    });
    if (finalMessage) index += 1;
  }
  return entries;
}

export function chatRenderEntryKey(entry, index) {
  if (entry?.type === 'assistantTurn') return entry.id || `assistant-turn-${index}`;
  return messageKey(entry?.message, index);
}

export function latestAssistantTurnEntryIndex(entries) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (entries[index]?.type === 'assistantTurn') return index;
  }
  return -1;
}

export function hasAssistantTurnAfterLastUser(entries) {
  const lastUserIndex = findLastUserEntryIndex(entries);
  if (lastUserIndex < 0) return false;
  return entries.slice(lastUserIndex + 1).some((entry) => entry?.type === 'assistantTurn');
}

export function shouldShowPendingAssistant(entries, currentActivity, sending, processing) {
  const state = String(currentActivity?.state || '').toLowerCase();
  const activityId = String(currentActivity?.id || '').trim();
  const active = Boolean(sending || processing || (currentActivity && state !== 'done' && state !== 'failed'));
  if (!active) return false;
  if (activityId.startsWith('stream-assistant-') || activityId.startsWith('stream-reasoning-') || activityId.startsWith('stream-tool-')) return false;
  const lastUserIndex = findLastUserEntryIndex(entries);
  if (lastUserIndex < 0) return false;
  const hasAssistantAfterUser = entries.slice(lastUserIndex + 1).some((entry) => {
    if (entry?.type === 'assistantTurn') return true;
    if (entry?.type !== 'message') return false;
    return String(entry.message?.role || '').toLowerCase() === 'assistant';
  });
  return !hasAssistantAfterUser;
}

function findLastUserEntryIndex(entries) {
  return findLastEntryIndex(entries, (entry) => (
    entry?.type === 'message' && String(entry.message?.role || '').toLowerCase() === 'user'
  ));
}

function findLastEntryIndex(entries, predicate) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (predicate(entries[index])) return index;
  }
  return -1;
}
