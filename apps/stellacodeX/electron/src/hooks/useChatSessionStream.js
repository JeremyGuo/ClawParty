import { useEffect } from 'react';
import { conversationKey, foregroundSessions, loadMessages } from '../lib/api';
import { addUsageTotals, firstMessageIndexGap, isFinalAssistantMessage, mergeMessages, messageIndex, messageOrderFromId } from '../lib/messageUtils';
import {
  applyStreamErrorToMessages,
  applyStreamAttachmentManifest,
  createStreamBufferStore,
  createStreamIndexTracker,
  markQueuedUserMessage,
  normalizedStreamEvent,
  streamAssistantDeltaPatch,
  streamErrorPatch,
  streamEventIndex,
  streamEventType,
  streamMessageId,
  streamReasoningDeltaPatch,
  streamReasoningPartPatch,
  streamToolCallDeltaPatch,
  streamToolResultDonePatch,
  streamTurnCompletedPatch,
  streamTurnStartedPatch
} from '../lib/chatStreamDataPlane';
import { createChatStreamFrameQueue } from '../lib/chatStreamFrameQueue';
import { startChatSocketClient } from '../lib/chatSocketClient';
import { chatAckHistoryPlan, chatSnapshotProjection, incomingMessagesPatch, recentMessagesPatch } from '../lib/chatMessagePlane';
import { readMessageCache, removeMessageCache, writeMessageCache } from '../lib/chatMessageCache';
import {
  recordChatProtocolDiagnostic,
  shouldRecordChatProtocolDiagnostic,
  summarizeMessagesTail,
  summarizePayload
} from '../lib/chatProtocolDiagnostics';
import { chatSessionStateIsActive, chatSnapshotState, mergeProgressActivity, normalizeProgressFeedback, recentMessagePageParams } from '../lib/chatSessionState';
import {
  compactMessagesSummary,
  patchActionSummary,
  streamDeltaSummary,
  streamUiCategory,
  streamUiKind
} from '../lib/chatDebugSummaries';
import { patchConversationForegroundSession } from '../lib/conversationState';

function messageFromPayload(payload) {
  const message = payload?.message;
  if (!message || typeof message !== 'object') return null;
  return {
    ...message,
    index: message.index ?? payload.index,
    turn_id: message.turn_id ?? payload.turn_id,
    step_index: message.step_index ?? payload.step_index,
    message_part: message.message_part ?? payload.message_part
  };
}

function sortedFiniteMessages(messages) {
  return (Array.isArray(messages) ? messages : [])
    .filter((message) => {
      const index = messageIndex(message);
      return Number.isFinite(index) && index !== Number.MAX_SAFE_INTEGER;
    })
    .sort((left, right) => messageIndex(left) - messageIndex(right));
}

function recentContiguousSuffix(messages) {
  const sorted = sortedFiniteMessages(messages);
  if (!sorted.length) return [];
  let start = sorted.length - 1;
  while (start > 0 && messageIndex(sorted[start - 1]) + 1 === messageIndex(sorted[start])) {
    start -= 1;
  }
  return sorted.slice(start);
}

function cachedMessagesForRecentSession(messages, session) {
  const suffix = recentContiguousSuffix(messages);
  if (!suffix.length) return [];
  const latestCachedIndex = messageIndex(suffix.at(-1));
  const latestSessionIndex = messageOrderFromId(session?.last_message_id)
    ?? Number(session?.last_committed_message_index ?? session?.last_message_index);
  if (Number.isFinite(latestSessionIndex) && latestCachedIndex < latestSessionIndex - 240) {
    return [];
  }
  return suffix;
}

function rangesOverlapOrTouch(leftMessages, rightMessages) {
  const left = sortedFiniteMessages(leftMessages);
  const right = sortedFiniteMessages(rightMessages);
  if (!left.length || !right.length) return false;
  const leftStart = messageIndex(left[0]);
  const leftEnd = messageIndex(left.at(-1));
  const rightStart = messageIndex(right[0]);
  const rightEnd = messageIndex(right.at(-1));
  return leftStart <= rightEnd + 1 && rightStart <= leftEnd + 1;
}

export function useChatSessionStream({
  selectedServerId,
  selectedConversationId,
  selectedSessionId,
  conversationsRef,
  websocketKeyRef,
  messagesRef,
  seenUsageMessagesRef,
  chatSessionStateRef,
  setMessages,
  setMessagesReady,
  setSessionActivity,
  setChatSessionState,
  setRunningActivities,
  setStatusDeltas,
  setConversations,
  updateRunningActivities,
  showCommandNotice,
  markConversationRead
}) {
  useEffect(() => {
    if (!selectedServerId || !selectedConversationId) return undefined;
    const serverId = selectedServerId;
    const conversationId = selectedConversationId;
    const sessionId = selectedSessionId;
    const key = conversationKey(serverId, conversationId, sessionId);
    let disposed = false;
    let socketClient = null;
    let streamFrameQueue = null;

    const cacheMessages = (next) => {
      writeMessageCache(serverId, conversationId, sessionId, next);
    };

    const clearMessageCache = () => {
      removeMessageCache(serverId, conversationId, sessionId);
    };

    const selectedSessionSummary = () => {
      const conversation = conversationsRef.current.find((item) => item.conversation_id === conversationId);
      return foregroundSessions(conversation).find((item) => String(item?.id || 'main') === sessionId) || conversation;
    };

    const recordProtocol = (kind, details = {}) => {
      const category = typeof details === 'function' ? '' : details?.category;
      if (!shouldRecordChatProtocolDiagnostic(kind, category)) return;
      const resolvedDetails = typeof details === 'function' ? details() : details;
      recordChatProtocolDiagnostic(kind, {
        scopeKey: key,
        serverId,
        conversationId,
        foregroundSessionId: sessionId,
        ...resolvedDetails
      });
    };

    const shouldRecordProtocol = (kind, category = '') => (
      shouldRecordChatProtocolDiagnostic(kind, category)
    );

    const updateSelectedSessionSummary = (latestMessage, latestId, latestIndex) => {
      if (!latestId || !Number.isFinite(latestIndex)) return;
      setConversations((current) => {
        const next = current.map((conversation) => {
          if (conversation.conversation_id !== conversationId) return conversation;
          const session = foregroundSessions(conversation).find((item) => (
            String(item?.id || 'main') === sessionId
          ));
          const currentCount = Number(session?.message_count || conversation?.message_count || 0);
          return patchConversationForegroundSession(conversation, sessionId, {
            last_message_id: String(latestId),
            last_message_time: latestMessage?.message_time || new Date().toISOString(),
            message_count: Math.max(currentCount, latestIndex + 1)
          });
        });
        conversationsRef.current = next;
        return next;
      });
      markConversationRead(serverId, conversationId, sessionId, latestId);
    };

    const applyIncomingMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
      streamFrameQueue?.flushNow?.();
      const patch = incomingMessagesPatch(messagesRef.current, incoming, key, seenUsageMessagesRef.current);
      if (!patch) return;
      if (patch.protocolMismatches.length > 0) {
        recordProtocol('chat.stream_commit_mismatch', {
          mismatches: patch.protocolMismatches,
          incoming: summarizeMessagesTail(incoming, 12),
          beforeTail: summarizeMessagesTail(messagesRef.current, 12),
          afterTail: summarizeMessagesTail(patch.messages, 12)
        });
        console.warn('stream provisional message differed from durable commit', patch.protocolMismatches);
        setSessionActivity('流式消息和落盘消息不一致，已使用落盘消息');
      }
      if (patch.usageDelta.totalTokens > 0 || patch.usageDelta.cost > 0) {
        setStatusDeltas((current) => {
          const next = new Map(current);
          next.set(key, addUsageTotals(next.get(key), patch.usageDelta));
          return next;
        });
      }
      messagesRef.current = patch.messages;
      cacheMessages(patch.messages);
      setMessages(patch.messages);
      recordProtocol('chat.message_arrived', {
        category: 'replace_ui_element',
        action: 'merge durable messages / refresh messages',
        incoming: compactMessagesSummary(incoming, 5),
        afterTail: compactMessagesSummary(patch.messages, 5)
      });
      updateSelectedSessionSummary(patch.latestMessage, patch.latestId, patch.latestIndex);
      if (patch.activity) setSessionActivity(patch.activity);
      if (patch.finalizedActivities.size > 0) {
        updateRunningActivities((current) => current.filter((item) => !patch.finalizedActivities.has(item.id)));
      }
      if (patch.hasFinalAssistant) {
        setChatSessionState({ scopeKey: key, state: 'idle' });
        chatSessionStateRef.current = { scopeKey: key, state: 'idle' };
        setSessionActivity('已完成');
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
            setSessionActivity('');
          }
        }, 700);
      }
    };

    const replaceWithRecentMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
      streamFrameQueue?.flushNow?.();
      const patch = recentMessagesPatch(incoming);
      if (!patch) return;
      messagesRef.current = patch.messages;
      cacheMessages(patch.messages);
      setMessages(patch.messages);
      recordProtocol('chat.message_arrived', {
        category: 'replace_ui_element',
        action: 'replace with recent durable messages',
        incoming: compactMessagesSummary(incoming, 5),
        afterTail: compactMessagesSummary(patch.messages, 5)
      });
      updateSelectedSessionSummary(patch.latestMessage, patch.latestId, patch.latestIndex);
      if (patch.activity) setSessionActivity(patch.activity);
      if (isFinalAssistantMessage(patch.latestMessage)) {
        setChatSessionState({ scopeKey: key, state: 'idle' });
        chatSessionStateRef.current = { scopeKey: key, state: 'idle' };
        setRunningActivities([]);
        setSessionActivity('');
      }
    };

    const mergeOrReplaceRecentMessages = (current, incoming) => (
      current.length && rangesOverlapOrTouch(current, incoming)
        ? mergeMessages(current, incoming)
        : incoming
    );

    const fillFirstMessageGap = async () => {
      let gap = firstMessageIndexGap(messagesRef.current);
      let previousOffset = -1;
      while (!disposed && websocketKeyRef.current === key && gap) {
        if (gap.offset === previousOffset) return;
        previousOffset = gap.offset;
        const missing = await loadMessages(serverId, conversationId, {
          ...gap,
          foregroundSessionId: sessionId
        });
        if (!missing.length) return;
        applyIncomingMessages(missing);
        gap = firstMessageIndexGap(messagesRef.current);
      }
    };

    const streamBuffers = createStreamBufferStore();
    const streamTracker = createStreamIndexTracker(key);
    let lastStreamAuxUpdateAt = 0;

    const expectedStreamIndexFromMessages = (event) => {
      const id = streamMessageId(event);
      if (!id) return undefined;
      const message = [...messagesRef.current].reverse().find((item) => (
        item?._streaming
        && String(item?.role || '').toLowerCase() === 'assistant'
        && String(item?.id ?? item?.message_id ?? '').trim() === id
      ));
      if (!message) return undefined;
      const lastIndex = Number(message._lastStreamEventIndex);
      return Number.isFinite(lastIndex) ? lastIndex + 1 : undefined;
    };

    const acceptStreamEvent = (event) => (
      streamTracker.accept(event, (expected, received) => {
        recordProtocol('chat.stream_index_gap', {
          expected,
          received,
          firstObserved: streamEventIndex(event),
          event: summarizePayload({ type: streamEventType(event), event }),
          messagesTail: summarizeMessagesTail(messagesRef.current, 12)
        });
        setMessages((current) => {
          const next = applyStreamErrorToMessages(current, {
            ...event,
            error: `non-contiguous stream event: expected index ${expected}, received ${received}`
          });
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('流式消息不连续，已撤销当前临时消息');
      }, expectedStreamIndexFromMessages(event))
    );

    const scopedChatState = (state) => ({ scopeKey: key, ...state });
    const turnIdFromState = (state) => String(
      state?.activeTurnId
      || state?.active_turn_id
      || state?.currentTurnState?.turn_id
      || state?.currentTurnState?.turnId
      || ''
    ).trim();
    const keepOrSetRunningState = (current, eventPayload) => (
      chatSessionStateIsActive(current) && current.scopeKey === key
        ? current
        : scopedChatState({ state: 'running', currentTurnState: eventPayload })
    );
    const applyStreamPatch = (patch, event, type) => {
      if (!patch) {
        recordProtocol('chat.stream', () => ({
          category: 'stream',
          action: 'stream arrived, no render change',
          ...streamDeltaSummary(event)
        }));
        return;
      }
      const category = patch.messages ? streamUiCategory(type, patch) : 'stream';
      const protocolKind = patch.messages ? streamUiKind(category) : 'chat.stream';
      const shouldLogPatch = shouldRecordProtocol(protocolKind, category);
      const beforeTail = patch.messages && shouldLogPatch ? compactMessagesSummary(messagesRef.current, 3) : undefined;
      if (patch.resetStreamState) {
        streamTracker.reset();
        streamBuffers.reset();
        streamFrameQueue?.reset?.();
      }
      if (patch.chatState) {
        const previousState = chatSessionStateRef.current;
        if (patch.forceChatState && patch.chatState.state === 'running') {
          const previousTurnId = turnIdFromState(previousState);
          const nextTurnId = turnIdFromState(patch.chatState);
          if (
            previousState?.scopeKey === key
            && chatSessionStateIsActive(previousState)
            && previousTurnId
            && nextTurnId
            && previousTurnId !== nextTurnId
          ) {
            recordProtocol('chat.turn_start_overlap_warning', {
              previousTurnId,
              nextTurnId,
              event: summarizePayload({ type, event }),
              messagesTail: summarizeMessagesTail(messagesRef.current, 12)
            });
            console.warn('chat protocol warning: stream_turn_start received before previous turn completed', {
              scopeKey: key,
              previousTurnId,
              nextTurnId
            });
          }
        }
        setChatSessionState((current) => {
          const nextState = patch.chatState.state === 'running' && !patch.forceChatState
            ? keepOrSetRunningState(current, patch.chatState.currentTurnState || event)
            : scopedChatState(patch.chatState);
          chatSessionStateRef.current = nextState;
          return nextState;
        });
      }
      if (patch.messages) {
        messagesRef.current = patch.messages;
        if (patch.shouldCache) cacheMessages(patch.messages);
        setMessages(patch.messages);
        recordProtocol(protocolKind, () => ({
          category,
          action: patchActionSummary(type, patch),
          ...streamDeltaSummary(event),
          messageCount: patch.messages.length,
          beforeTail,
          afterTail: compactMessagesSummary(patch.messages, 3)
        }));
      } else {
        recordProtocol(protocolKind, () => ({
          category: 'stream',
          action: patchActionSummary(type, patch),
          ...streamDeltaSummary(event),
          activity: patch.activity,
          chatState: patch.chatState?.state
        }));
      }
      const isHighFrequencyStreamPatch = category === 'append_stream_to_ui';
      const nowMs = Date.now();
      const shouldUpdateAuxState = !isHighFrequencyStreamPatch || nowMs - lastStreamAuxUpdateAt > 200;
      if (shouldUpdateAuxState) lastStreamAuxUpdateAt = nowMs;
      if (patch.activity && shouldUpdateAuxState) setSessionActivity(patch.activity);
      if (patch.runningActivity && shouldUpdateAuxState) {
        const removeIds = new Set(patch.removeActivityIds || []);
        updateRunningActivities((current) => [
          ...current.filter((item) => !removeIds.has(item.id)),
          mergeProgressActivity(current, patch.runningActivity)
        ]);
      }
      if (patch.clearRunningActivitiesDelay) {
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, patch.clearRunningActivitiesDelay);
      }
    };

    streamFrameQueue = createChatStreamFrameQueue({
      onFlush: (entries) => {
        if (disposed || websocketKeyRef.current !== key) return;
        entries.forEach(({ kind, event }) => {
          if (kind === 'assistant') {
            applyStreamPatch(
              streamAssistantDeltaPatch(messagesRef.current, event, key, streamBuffers),
              event,
              'stream_assistant_message_delta'
            );
          } else if (kind === 'reasoning') {
            applyStreamPatch(
              streamReasoningDeltaPatch(messagesRef.current, event, key, streamBuffers),
              event,
              'stream_reasoning_summary_delta'
            );
          } else if (kind === 'tool') {
            applyStreamPatch(
              streamToolCallDeltaPatch(messagesRef.current, event),
              event,
              'stream_tool_call_delta'
            );
          }
        });
      }
    });

    const drainStreamFrameQueue = (callback) => {
      if (streamFrameQueue?.drainBefore) {
        streamFrameQueue.drainBefore(callback);
      } else if (typeof callback === 'function') {
        callback();
      }
    };

    const applySessionStream = (rawEvent) => {
      const event = normalizedStreamEvent(rawEvent);
      const type = streamEventType(event);
      if (!type || disposed || websocketKeyRef.current !== key) return;
      recordProtocol('chat.stream', () => ({
        category: 'stream',
        action: 'stream arrived',
        ...streamDeltaSummary(event)
      }));

      if (type === 'turn_started' || type === 'stream_turn_start') {
        setChatSessionState((current) => (
          current?.scopeKey === key
            ? { ...current, compressionError: null }
            : current
        ));
        if (chatSessionStateRef.current?.scopeKey === key) {
          chatSessionStateRef.current = {
            ...chatSessionStateRef.current,
            compressionError: null
          };
        }
        applyStreamPatch(streamTurnStartedPatch(event), event, type);
        return;
      }

      if (type === 'turn_completed' || type === 'stream_turn_done') {
        drainStreamFrameQueue(() => {
          applyStreamPatch(streamTurnCompletedPatch(messagesRef.current, event), event, type);
        });
        return;
      }

      if (type === 'plan_updated') {
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const progress = normalizeProgressFeedback({ type: 'turn_progress', progress: event });
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== progress.id && item.id !== 'thinking'),
          mergeProgressActivity(current, progress)
        ]);
        setSessionActivity(progress.detail || progress.title || '已更新计划');
        return;
      }

      if (type === 'compact_started') {
        setChatSessionState((current) => ({
          ...(current?.scopeKey === key ? current : { scopeKey: key, state: 'idle' }),
          compressionActive: true,
          compressionError: null,
          currentCompressionState: {
            phase: event.phase || '',
            startedAt: event.started_at || event.startedAt || new Date().toISOString()
          }
        }));
        chatSessionStateRef.current = {
          ...(chatSessionStateRef.current?.scopeKey === key ? chatSessionStateRef.current : { scopeKey: key, state: 'idle' }),
          compressionActive: true,
          compressionError: null,
          currentCompressionState: {
            phase: event.phase || '',
            startedAt: event.started_at || event.startedAt || new Date().toISOString()
          }
        };
        setSessionActivity('正在压缩上下文');
        return;
      }

      if (type === 'compact_completed' || type === 'compact_failed') {
        const compressionError = type === 'compact_failed'
          ? {
            reason: event.reason || event.message || '上下文压缩失败',
            phase: event.phase || '',
            failedAt: event.failed_at || event.failedAt || new Date().toISOString()
          }
          : null;
        setChatSessionState((current) => (
          current?.scopeKey === key
            ? { ...current, compressionActive: false, currentCompressionState: null, compressionError }
            : current
        ));
        if (chatSessionStateRef.current?.scopeKey === key) {
          chatSessionStateRef.current = {
            ...chatSessionStateRef.current,
            compressionActive: false,
            currentCompressionState: null,
            compressionError
          };
        }
        if (type === 'compact_failed') {
          setSessionActivity(compressionError.reason);
          showCommandNotice?.({
            title: '上下文压缩失败',
            detail: compressionError.reason,
            state: 'failed'
          }, 3600);
        }
        return;
      }

      if (type === 'stream_assistant_message_delta') {
        if (!acceptStreamEvent(event)) return;
        streamFrameQueue?.enqueue('assistant', event);
        return;
      }

      if (type === 'stream_reasoning_summary_part_added') {
        if (!acceptStreamEvent(event)) return;
        applyStreamPatch(streamReasoningPartPatch(event), event, type);
        return;
      }

      if (type === 'stream_reasoning_summary_delta') {
        if (!acceptStreamEvent(event)) return;
        streamFrameQueue?.enqueue('reasoning', event);
        return;
      }

      if (type === 'stream_tool_call_delta') {
        if (!acceptStreamEvent(event)) return;
        streamFrameQueue?.enqueue('tool', event);
        return;
      }

      if (type === 'stream_tool_result_done') {
        if (!acceptStreamEvent(event)) return;
        drainStreamFrameQueue(() => {
          applyStreamPatch(streamToolResultDonePatch(messagesRef.current, event), event, type);
        });
        return;
      }

      if (type === 'stream_error') {
        drainStreamFrameQueue(() => {
          streamTracker.clearForEvent(event);
          applyStreamPatch(streamErrorPatch(messagesRef.current, event), event, type);
        });
      }
    };

    const loadInitialMessagePage = async () => {
      const session = selectedSessionSummary();
      const initial = await loadMessages(
        serverId,
        conversationId,
        { ...recentMessagePageParams(session), foregroundSessionId: sessionId }
      );
      if (disposed || websocketKeyRef.current !== key) return;
      streamFrameQueue?.flushNow?.();
      setMessages((current) => {
        const next = mergeOrReplaceRecentMessages(current, initial);
        messagesRef.current = next;
        cacheMessages(next);
        return next;
      });
      setMessagesReady(true);
      fillFirstMessageGap().catch(() => {});
    };

    const reconcileAck = async (ack) => {
      const plan = chatAckHistoryPlan(messagesRef.current, ack);
      if (plan.kind === 'none' || disposed || websocketKeyRef.current !== key) return;
      if (plan.kind === 'clear') {
        messagesRef.current = [];
        clearMessageCache();
        setMessages([]);
        setMessagesReady(true);
        return;
      }
      let missing = await loadMessages(serverId, conversationId, {
        ...plan.params,
        foregroundSessionId: sessionId
      });
      let nextOffset = Number(plan.params?.offset || 0) + missing.length;
      const total = Number(plan.total);
      while (!disposed && websocketKeyRef.current === key && Number.isFinite(total) && nextOffset < total && missing.length > 0) {
        const chunk = await loadMessages(serverId, conversationId, {
          offset: nextOffset,
          limit: Math.min(200, total - nextOffset),
          foregroundSessionId: sessionId
        });
        if (!chunk.length) break;
        missing = missing.concat(chunk);
        nextOffset += chunk.length;
      }
      if (plan.replace) {
        replaceWithRecentMessages(missing);
      } else {
        applyIncomingMessages(missing);
      }
      if (!disposed && websocketKeyRef.current === key) {
        setMessagesReady(true);
      }
    };

    const applyChatSnapshotLiveProjection = (snapshot) => {
      if (!snapshot || disposed || websocketKeyRef.current !== key) return;
      const projection = chatSnapshotProjection(messagesRef.current, snapshot);
      if (!projection) return;
      if (projection.changed) {
        streamFrameQueue?.flushNow?.();
        messagesRef.current = projection.messages;
        if (projection.shouldCache) cacheMessages(projection.messages);
        setMessages(projection.messages);
      }
      if (projection.runningActivities?.length > 0) {
        updateRunningActivities((current) => [
          ...current.filter((item) => !projection.runningActivities.some((activity) => activity.id === item.id) && item.id !== 'thinking'),
          ...projection.runningActivities
        ]);
      }
      if (projection.clearRunningActivities) {
        setRunningActivities([]);
      }
      if (projection.activity) setSessionActivity(projection.activity);
    };

    const applyChatSocketPayload = (payload) => {
      if (disposed || websocketKeyRef.current !== key) return;
      const payloadType = String(payload?.type || '');
      const nestedStreamType = streamEventType(normalizedStreamEvent(payload));
      const streamLikePayload = payloadType.startsWith('chat.stream_')
        || nestedStreamType.startsWith('stream_')
        || nestedStreamType === 'turn_started'
        || nestedStreamType === 'turn_completed'
        || nestedStreamType === 'plan_updated';
      recordProtocol('chat.socket_payload', () => ({
        category: streamLikePayload ? 'stream' : 'replace_ui_element',
        action: 'socket payload received',
        payloadType,
        nestedType: nestedStreamType,
        keys: Object.keys(payload || {}).slice(0, 16),
        payload: summarizePayload(payload)
      }));
      if (payloadType === 'chat.snapshot') {
        const snapshotState = chatSnapshotState(payload);
        setChatSessionState({ scopeKey: key, ...snapshotState });
        chatSessionStateRef.current = { scopeKey: key, ...snapshotState };
        setSessionActivity(payload.reason === 'session_changed' ? 'Session 已切换' : '实时连接已同步');
        reconcileAck(payload).catch(() => {});
        applyChatSnapshotLiveProjection(payload);
      } else if (payloadType === 'chat.user_message_queued') {
        setChatSessionState({ scopeKey: key, state: 'queued' });
        setMessages((current) => {
          const next = markQueuedUserMessage(current, payload.client_message_id || payload.clientMessageId);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('消息已排队');
      } else if (payloadType === 'chat.user_message_started') {
        setChatSessionState((current) => chatSessionStateIsActive(current) && current.scopeKey === key ? current : { scopeKey: key, state: 'queued' });
        setSessionActivity('开始处理');
      } else if (payloadType === 'chat.user_message_committed') {
        setChatSessionState((current) => chatSessionStateIsActive(current) && current.scopeKey === key ? current : { scopeKey: key, state: 'queued' });
        {
          const message = messageFromPayload(payload);
          applyIncomingMessages(message ? [message] : []);
        }
        setSessionActivity('用户消息已落盘');
      } else if (payloadType === 'chat.message_appended') {
        {
          const message = messageFromPayload(payload);
          applyIncomingMessages(message ? [message] : []);
        }
      } else if (payloadType === 'chat.attachment_manifest') {
        setMessages((current) => {
          const next = applyStreamAttachmentManifest(current, payload);
          messagesRef.current = next;
          return next;
        });
      } else if (
        payloadType.startsWith('chat.stream_')
        || nestedStreamType.startsWith('stream_')
        || nestedStreamType === 'turn_started'
        || nestedStreamType === 'turn_completed'
        || nestedStreamType === 'plan_updated'
        || nestedStreamType === 'compact_started'
        || nestedStreamType === 'compact_completed'
        || nestedStreamType === 'compact_failed'
        || payloadType === 'chat.compact_started'
        || payloadType === 'chat.compact_completed'
        || payloadType === 'chat.compact_failed'
        || payloadType === 'chat.plan_updated'
      ) {
        applySessionStream(payload);
      } else if (payloadType === 'chat.error' || payloadType === 'error') {
        setChatSessionState({ scopeKey: key, state: 'failed', lastError: payload.message || payload.error || '实时连接错误' });
        setSessionActivity(payload.message || payload.error || '实时连接错误');
      }
    };

    const loadFallbackMessagePage = () => {
      const session = selectedSessionSummary();
      loadMessages(serverId, conversationId, {
        ...recentMessagePageParams(session),
        foregroundSessionId: sessionId
      })
        .then((initial) => {
          if (disposed || websocketKeyRef.current !== key) return;
          streamFrameQueue?.flushNow?.();
          setMessages((current) => {
            const next = mergeOrReplaceRecentMessages(current, initial);
            messagesRef.current = next;
            cacheMessages(next);
            return next;
          });
          setMessagesReady(true);
          fillFirstMessageGap().catch(() => {});
        })
        .catch(() => {
          if (!disposed) {
            setMessages([]);
            setMessagesReady(true);
          }
        });
    };

    socketClient?.close();
    websocketKeyRef.current = key;
    const cachedMessages = cachedMessagesForRecentSession(
      readMessageCache(serverId, conversationId, sessionId),
      selectedSessionSummary()
    );
    messagesRef.current = cachedMessages;
    setMessages(cachedMessages);
    setMessagesReady(cachedMessages.length > 0);
    setSessionActivity('');
    setChatSessionState({ scopeKey: key, state: 'idle' });
    setRunningActivities([]);
    loadInitialMessagePage().catch(() => {
      if (!disposed && websocketKeyRef.current === key && messagesRef.current.length === 0) {
        setMessages([]);
        setMessagesReady(true);
      }
    });
    socketClient = startChatSocketClient({
      serverId,
      conversationId,
      foregroundSessionId: sessionId,
      isCurrent: () => !disposed && websocketKeyRef.current === key,
      onPayload: applyChatSocketPayload,
      onStatus: (status) => {
        if (status === 'reconnecting') setSessionActivity('实时连接异常，正在重连');
        else if (status === 'error') setSessionActivity('实时连接异常');
        else if (status === 'unavailable') setSessionActivity('实时连接不可用，使用刷新兜底');
      },
      onFallback: loadFallbackMessagePage
    });

    return () => {
      disposed = true;
      if (websocketKeyRef.current === key) websocketKeyRef.current = '';
      streamFrameQueue?.dispose?.();
      socketClient?.close();
    };
  }, [
    selectedServerId,
    selectedConversationId,
    selectedSessionId,
    conversationsRef,
    websocketKeyRef,
    messagesRef,
    seenUsageMessagesRef,
    chatSessionStateRef,
    setMessages,
    setMessagesReady,
    setSessionActivity,
    setChatSessionState,
    setRunningActivities,
    setStatusDeltas,
    setConversations,
    updateRunningActivities,
    showCommandNotice,
    markConversationRead
  ]);
}
