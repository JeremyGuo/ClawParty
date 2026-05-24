import { chatRenderEntryKey } from './renderModel';
import { messageText } from '../../lib/fileUtils';

export const VIRTUALIZE_ENTRY_THRESHOLD = 80;
const VIRTUAL_OVERSCAN_MIN_PX = 360;
const VIRTUAL_OVERSCAN_MAX_PX = 760;

export function virtualWindowForEntries({ entries, keys, heightCache, viewport, activeIndex }) {
  const count = entries.length;
  if (count <= VIRTUALIZE_ENTRY_THRESHOLD) {
    return {
      virtualized: false,
      start: 0,
      end: Math.max(0, count - 1),
      topPadding: 0,
      bottomPadding: 0,
      items: entries.map((entry, index) => ({ entry, index, key: keys[index] || chatRenderEntryKey(entry, index) }))
    };
  }
  const heights = keys.map((key, index) => heightCache.get(key) || estimateEntryHeight(entries[index]));
  const offsets = new Array(count + 1);
  offsets[0] = 0;
  for (let index = 0; index < count; index += 1) {
    offsets[index + 1] = offsets[index] + heights[index];
  }
  const scrollTop = Number(viewport.scrollTop || 0);
  const clientHeight = Number(viewport.clientHeight || 0);
  const overscan = Math.min(
    VIRTUAL_OVERSCAN_MAX_PX,
    Math.max(VIRTUAL_OVERSCAN_MIN_PX, clientHeight * 0.55)
  );
  if (viewport.stickToBottom) {
    let start = Math.max(0, count - 1);
    const tailTop = Math.max(0, offsets[count] - clientHeight - overscan);
    while (start > 0 && offsets[start] > tailTop) start -= 1;
    return {
      virtualized: true,
      start,
      end: count - 1,
      topPadding: offsets[start],
      bottomPadding: 0,
      items: entries.slice(start).map((entry, offset) => {
        const index = start + offset;
        return { entry, index, key: keys[index] || chatRenderEntryKey(entry, index) };
      })
    };
  }
  const top = Math.max(0, scrollTop - overscan);
  const bottom = Math.max(top, scrollTop + clientHeight + overscan);
  let start = 0;
  while (start < count - 1 && offsets[start + 1] < top) start += 1;
  let end = start;
  while (end < count - 1 && offsets[end] < bottom) end += 1;
  if (Number.isFinite(activeIndex) && activeIndex >= 0) {
    start = Math.min(start, Math.max(0, activeIndex - 2));
    end = Math.max(end, Math.min(count - 1, activeIndex + 2));
  }
  return {
    virtualized: true,
    start,
    end,
    topPadding: offsets[start],
    bottomPadding: Math.max(0, offsets[count] - offsets[end + 1]),
    items: entries.slice(start, end + 1).map((entry, offset) => {
      const index = start + offset;
      return { entry, index, key: keys[index] || chatRenderEntryKey(entry, index) };
    })
  };
}

function estimateEntryHeight(entry) {
  if (!entry) return 120;
  if (entry.type === 'assistantTurn') {
    const toolMessages = Array.isArray(entry.processGroup?.messages) ? entry.processGroup.messages : [];
    const toolRows = Math.max(1, Math.min(12, toolMessages.length || 1));
    const toolText = toolMessages.map((message) => messageText(message)).join('\n');
    return clampHeight(44 + toolRows * 38 + estimateTextHeight(toolText, 0.35) + estimateMessageHeight(entry.finalMessage));
  }
  return estimateMessageHeight(entry.message);
}

function estimateMessageHeight(message) {
  if (!message) return 0;
  const role = String(message.role || '').toLowerCase();
  const base = role === 'user' ? 58 : 42;
  const multiplier = role === 'user' ? 0.72 : 1;
  const attachments = Number(message.attachment_count || 0)
    || (Array.isArray(message.attachments) ? message.attachments.length : 0)
    || (Array.isArray(message.files) ? message.files.length : 0);
  return clampHeight(base + estimateTextHeight(messageText(message), multiplier) + attachments * 72);
}

function estimateTextHeight(text, multiplier = 1) {
  const value = String(text || '');
  if (!value.trim()) return 0;
  const chars = value.length;
  const hardLines = value.split('\n').length;
  const wrappedLines = Math.ceil(chars / 58);
  const fencedBlocks = (value.match(/```/g) || []).length / 2;
  return Math.ceil((hardLines + wrappedLines) * 22 * multiplier + fencedBlocks * 18);
}

function clampHeight(value) {
  return Math.max(80, Math.min(5200, Math.ceil(Number(value) || 120)));
}
