import { messageIndex } from './messageUtils';
import { readLocalCache, removeLocalCache, writeLocalCache } from './localCache';

const MESSAGE_CACHE_LIMIT = 240;
const MESSAGE_CACHE_TARGET_BYTES = 2_100_000;

function cachePayloadBytes(messages) {
  try {
    return JSON.stringify(messages || []).length;
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

function durableMessagesForCache(messages) {
  const durable = (Array.isArray(messages) ? messages : [])
    .filter((message) => (
      message
      && !message._streaming
      && !message._optimistic
      && !message.pending
      && !message.queued
      && !message._userMessageStarted
    ))
    .sort((left, right) => messageIndex(left) - messageIndex(right))
    .slice(-MESSAGE_CACHE_LIMIT);
  const result = [];
  for (let index = durable.length - 1; index >= 0; index -= 1) {
    const next = [durable[index], ...result];
    if (result.length > 0 && cachePayloadBytes(next) > MESSAGE_CACHE_TARGET_BYTES) break;
    result.unshift(durable[index]);
  }
  return result;
}

export function readMessageCache(serverId, conversationId, foregroundSessionId) {
  return readLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main']) || [];
}

export function writeMessageCache(serverId, conversationId, foregroundSessionId, messages) {
  const durable = durableMessagesForCache(messages);
  if (durable.length > 0) {
    const written = writeLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main'], durable);
    if (!written) removeMessageCache(serverId, conversationId, foregroundSessionId);
  } else {
    removeMessageCache(serverId, conversationId, foregroundSessionId);
  }
}

export function removeMessageCache(serverId, conversationId, foregroundSessionId) {
  removeLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main']);
}
