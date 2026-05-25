import { chatRenderEntryKey } from './renderModel';
import { attachmentName, isHtmlFile, isImageAttachment, messageText } from '../../lib/fileUtils';
import { markerIndexes, messageItems, parseToolTextBlocks, splitMessageForDisplay } from '../../lib/messageUtils';

export const VIRTUALIZE_ENTRY_THRESHOLD = 80;
const VIRTUAL_OVERSCAN_MIN_PX = 360;
const VIRTUAL_OVERSCAN_MAX_PX = 760;

export function virtualHeightCacheScopeForViewport(clientWidth = 0) {
  const width = Number(clientWidth || 0);
  const bucket = Math.max(0, Math.round(width / 16) * 16);
  return `w:${bucket}`;
}

export function scopedVirtualHeightKey(scope, key) {
  return scope ? `${scope}:${key}` : key;
}

export function virtualWindowForEntries({ entries, keys, heightCache, heightCacheScope = '', viewport, activeIndex }) {
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
  const layout = markdownLayoutForViewport(viewport?.clientWidth);
  const heights = keys.map((key, index) => (
    heightCache.get(scopedVirtualHeightKey(heightCacheScope, key)) || estimateEntryHeight(entries[index], layout)
  ));
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

function estimateEntryHeight(entry, layout) {
  if (!entry) return 120;
  if (entry.type === 'assistantTurn') {
    return clampHeight(estimateToolProcessGroupHeight(entry.processGroup, layout) + estimateMessageHeight(entry.finalMessage, layout) + 30);
  }
  return estimateMessageHeight(entry.message, layout);
}

function estimateMessageHeight(message, layout) {
  if (!message) return 0;
  const role = String(message.role || '').toLowerCase();
  const base = role === 'user' ? 58 : 42;
  const multiplier = role === 'user' ? 0.72 : 1;
  const text = messageText(message);
  const attachments = messageAttachments(message);
  const inlineIndexes = markerIndexes(text);
  const unknownAttachments = Math.max(0, Number(message.attachment_count || 0) - attachments.length);
  const structuredItems = messageItems(message);
  const mainTextHeight = structuredItems.length > 0
    ? 0
    : estimateMarkdownHeight(text, multiplier, attachments, layout, role);
  const structuredAttachmentIndexes = new Set(
    structuredItems
      .filter((item) => item?.type === 'file' && (item.attachment_index !== undefined || item.index !== undefined))
      .map((item) => Number(item.attachment_index ?? item.index))
      .filter((index) => Number.isFinite(index))
  );
  const attachmentHeight = attachments.reduce((total, attachment, index) => {
    const inline = inlineIndexes.has(index) || inlineIndexes.has(Number(attachment?.index));
    if (inline) return total;
    if (!inline && structuredAttachmentIndexes.has(index)) return total;
    return total + estimateAttachmentHeight(attachment, inline);
  }, unknownAttachments * 82);
  return clampHeight(
    base
      + mainTextHeight
      + attachmentHeight
      + estimateStructuredItemsHeight(structuredItems, attachments, text, role, layout)
      + estimateMessageChromeHeight(message, role)
  );
}

function estimateToolProcessGroupHeight(group, layout) {
  const messages = Array.isArray(group?.messages) ? group.messages : [];
  if (!messages.length) return 0;
  const hasFinalMessage = Boolean(group?.nextMessage);
  const completeCollapsed = hasFinalMessage;
  let blocks = 0;
  let noteHeight = 0;
  let cardRows = 0;
  messages.forEach((message) => {
    const display = splitMessageForDisplay(message);
    const attachments = messageAttachments(display.textMessage);
    if (display.textMessage) {
      noteHeight += estimateMarkdownHeight(messageText(display.textMessage), 0.42, attachments, layout, 'assistant');
      noteHeight += estimateAttachmentsHeight(attachments, 0.7);
    }
    const segments = Array.isArray(display.segments) && display.segments.length
      ? display.segments
      : [{ notes: [], cards: display.toolCards || [] }];
    segments.forEach((segment) => {
      const notes = Array.isArray(segment?.notes) ? segment.notes : [];
      const cards = Array.isArray(segment?.cards) ? segment.cards : [];
      notes.forEach((note) => {
        noteHeight += note?.kind === 'reasoning'
          ? estimateReasoningHeight(note.text, completeCollapsed, layout)
          : estimateMarkdownHeight(note?.text || '', 0.42, attachments, layout, 'assistant');
      });
      if (cards.length) {
        blocks += 1;
        cardRows += cards.length;
      }
    });
  });
  if (completeCollapsed) {
    return 36 + Math.max(32, blocks * 32) + Math.min(180, noteHeight * 0.25);
  }
  return 40 + blocks * 36 + cardRows * 34 + noteHeight;
}

function estimateStructuredItemsHeight(items, attachments, fallbackText, role, layout) {
  const list = Array.isArray(items) ? items : [];
  if (!list.length) return 0;
  let total = 0;
  let hasText = false;
  let hasSelectionReference = false;
  list.forEach((item) => {
    if (typeof item === 'string') {
      hasText = true;
      total += estimateMarkdownHeight(item, 1, attachments, layout, role);
      return;
    }
    if (!item || typeof item !== 'object') return;
    if (item.type === 'text') {
      hasText = true;
      total += estimateMarkdownHeight(item.text_with_attachment_markers || item.text || item.content || '', 1, attachments, layout, role);
    } else if (item.type === 'file') {
      const attachmentIndex = Number(item.attachment_index ?? item.index);
      total += estimateAttachmentHeight(Number.isFinite(attachmentIndex) ? attachments[attachmentIndex] || item : item, false);
    } else if (item.type === 'selection_reference') {
      hasSelectionReference = true;
      total += estimateSelectionReferenceHeight(item.selection || item.payload || item);
    } else if (item.type === 'reasoning') {
      total += estimateReasoningHeight(item.text || item.summary || '', false, layout);
    } else if (item.type === 'tool_call' || item.type === 'tool_result') {
      total += estimateToolInlineCardHeight(item, layout);
    }
  });
  if (String(role || '').toLowerCase() === 'user' && hasSelectionReference && !hasText && String(fallbackText || '').trim()) {
    total += estimateMarkdownHeight(fallbackText, 0.72, attachments, layout, role) + 8;
  }
  return total;
}

function estimateMarkdownHeight(text, multiplier = 1, attachments = [], layout = markdownLayoutForViewport(), role = 'assistant') {
  const value = String(text || '');
  if (!value.trim()) return 0;
  return Math.ceil(estimateMarkdownBlockLayoutHeight(value, attachments, layout, role) * multiplier);
}

function estimateAttachmentsHeight(attachments, scale = 1) {
  return messageAttachments({ attachments }).reduce((total, attachment) => (
    total + estimateAttachmentHeight(attachment, false) * scale
  ), 0);
}

function estimateAttachmentHeight(attachment, inline) {
  if (!attachment) return inline ? 96 : 72;
  if (isHtmlAttachmentLike(attachment)) return inline ? 520 : 82;
  if (isImageAttachment(attachment)) return estimateImageAttachmentHeight(attachment);
  return inline ? 84 : 76;
}

function estimateImageAttachmentHeight(attachment) {
  const width = Number(attachment?.width || attachment?.image_width || attachment?.metadata?.width || 0);
  const height = Number(attachment?.height || attachment?.image_height || attachment?.metadata?.height || 0);
  if (width > 0 && height > 0) {
    const displayWidth = Math.min(340, Math.max(180, width));
    return Math.ceil(Math.min(240, Math.max(92, height * (displayWidth / width))) + 35);
  }
  return 275;
}

function estimateSelectionReferenceHeight(selection) {
  const text = String(selection?.selected_text || '').trim();
  if (!text) return 46;
  return Math.min(170, 50 + Math.ceil(Math.min(text.length, 240) / 58) * 20);
}

function estimateReasoningHeight(text, collapsed, layout) {
  const value = String(text || '').trim();
  if (!value) return 0;
  return collapsed ? 32 : 30 + estimateMarkdownHeight(value, 0.7, [], layout, 'assistant');
}

function estimateToolInlineCardHeight(item, layout) {
  const payload = item?.type === 'tool_result'
    ? (item.structured || item.context_with_attachment_markers || item.context || item.result || '')
    : (item?.arguments || item?.payload || '');
  const text = typeof payload === 'string' ? payload : stableJsonPreview(payload);
  return 34 + Math.min(360, estimateMarkdownHeight(text, 0.18, [], layout, 'assistant'));
}

function estimateMessageChromeHeight(message, role) {
  let height = 0;
  if (message?.pending || message?.queued || message?.error) height += 24;
  if (role === 'user' || (role === 'assistant' && !message?._streaming)) {
    height += messageText(message).trim() || Number(message?.token_usage?.total || message?.usage?.total || 0) > 0 ? 26 : 0;
  }
  return height;
}

function messageAttachments(message) {
  if (!message || typeof message !== 'object') return [];
  return [
    ...(Array.isArray(message.attachments) ? message.attachments : []),
    ...(Array.isArray(message.files) ? message.files : [])
  ];
}

function isHtmlAttachmentLike(attachment) {
  const mediaType = String(attachment?.media_type || attachment?.mime_type || attachment?.mime || '').toLowerCase();
  return mediaType.includes('html') || isHtmlFile(attachmentName(attachment)) || isHtmlFile(attachment?.path || '');
}

function estimateMarkdownBlockLayoutHeight(value, attachments, layout, role) {
  const lines = String(value || '').replace(/\r\n/g, '\n').split('\n');
  const textChars = role === 'user' ? layout.userChars : layout.assistantChars;
  let total = 0;
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    const trimmed = line.trim();
    if (!trimmed) {
      index += 1;
      continue;
    }
    if (/^```/.test(trimmed)) {
      const codeLines = [];
      index += 1;
      while (index < lines.length && !/^```/.test(lines[index].trim())) {
        codeLines.push(lines[index]);
        index += 1;
      }
      if (index < lines.length) index += 1;
      total += estimateCodeBlockHeight(codeLines, layout);
      continue;
    }
    if (/^\$\$\s*$/.test(trimmed) || /^\\\[\s*$/.test(trimmed)) {
      const closing = trimmed.startsWith('$$') ? /^\$\$\s*$/ : /^\\\]\s*$/;
      let rows = 1;
      index += 1;
      while (index < lines.length && !closing.test(lines[index].trim())) {
        rows += 1;
        index += 1;
      }
      if (index < lines.length) index += 1;
      total += 20 + rows * 28;
      continue;
    }
    if (/^\s{0,3}#{1,6}\s+/.test(line)) {
      const level = Math.min(6, (line.match(/^\s{0,3}(#{1,6})\s+/)?.[1] || '').length || 3);
      total += level <= 1 ? 43 : level === 2 ? 39 : 34;
      index += 1;
      continue;
    }
    if (/^\s{0,3}(?:[-*_]\s*){3,}$/.test(line)) {
      total += 17;
      index += 1;
      continue;
    }
    if (isMarkdownImageLine(trimmed)) {
      total += 280;
      index += 1;
      continue;
    }
    const markerOnlyIndex = attachmentMarkerOnlyIndex(trimmed);
    if (markerOnlyIndex !== null) {
      total += estimateAttachmentHeight(attachments[markerOnlyIndex], true);
      index += 1;
      continue;
    }
    if (looksLikeTableRow(line)) {
      const tableLines = [];
      while (index < lines.length && looksLikeTableRow(lines[index])) {
        tableLines.push(lines[index]);
        index += 1;
      }
      total += estimateTableHeight(tableLines, textChars);
      continue;
    }
    if (/^\s*(?:[-*+]|\d+[.)])\s+/.test(line)) {
      const listLines = [];
      while (
        index < lines.length
        && (lines[index].trim() === '' || /^\s*(?:[-*+]|\d+[.)])\s+/.test(lines[index]) || /^\s{2,}\S/.test(lines[index]))
      ) {
        if (lines[index].trim()) listLines.push(lines[index]);
        index += 1;
      }
      total += estimateListHeight(listLines, textChars);
      continue;
    }
    if (/^\s{0,3}>/.test(line)) {
      const quoteLines = [];
      while (index < lines.length && /^\s{0,3}>/.test(lines[index])) {
        quoteLines.push(lines[index].replace(/^\s{0,3}>\s?/, ''));
        index += 1;
      }
      total += 14 + estimateWrappedTextHeight(quoteLines.join(' '), Math.max(24, textChars - 4), 22);
      continue;
    }
    const paragraph = [];
    while (index < lines.length && lines[index].trim() && !isBlockBoundary(lines[index])) {
      paragraph.push(lines[index]);
      index += 1;
    }
    total += estimateParagraphHeight(paragraph, textChars, attachments);
  }
  const parsedToolBlocks = parseToolTextBlocks(value);
  if (parsedToolBlocks.length) total += parsedToolBlocks.length * 34;
  return total;
}

function markdownLayoutForViewport(clientWidth = 0) {
  const width = Number(clientWidth || 0);
  const columnWidth = Math.min(920, Math.max(320, width ? width - 48 : 872));
  const userWidth = Math.min(620, columnWidth);
  return {
    columnWidth,
    assistantChars: Math.max(34, Math.floor(columnWidth / 8.8)),
    userChars: Math.max(24, Math.floor((userWidth - 34) / 8.8)),
    codeChars: Math.max(28, Math.floor((columnWidth - 26) / 7.2))
  };
}

function estimateCodeBlockHeight(lines, layout) {
  const rows = Math.max(1, lines.reduce((total, line) => (
    total + Math.max(1, Math.ceil(String(line || '').length / layout.codeChars))
  ), 0));
  return 18 + 22 + rows * 18;
}

function estimateTableHeight(lines, charsPerLine) {
  const rows = lines.filter((line) => !/^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line));
  if (!rows.length) return 0;
  const columnCount = Math.max(1, rows[0].split('|').filter((cell) => cell.trim()).length);
  const charsPerCell = Math.max(10, Math.floor(charsPerLine / columnCount));
  const rowUnits = rows.reduce((total, row) => {
    const cells = row.split('|').map((cell) => cell.trim()).filter(Boolean);
    const wraps = cells.reduce((max, cell) => Math.max(max, Math.ceil(cell.length / charsPerCell)), 1);
    return total + wraps;
  }, 0);
  return 20 + rowUnits * 32;
}

function estimateListHeight(lines, charsPerLine) {
  const rows = lines.reduce((total, line) => {
    const text = line.replace(/^\s*(?:[-*+]|\d+[.)])\s+/, '').trim();
    return total + Math.max(1, Math.ceil(text.length / Math.max(18, charsPerLine - 6)));
  }, 0);
  return 14 + rows * 25;
}

function estimateParagraphHeight(lines, charsPerLine, attachments) {
  const text = lines.join(' ').trim();
  if (!text) return 0;
  const markerHeight = Array.from(markerIndexes(text)).reduce((total, markerIndex) => (
    total + estimateAttachmentHeight(attachments[markerIndex], true)
  ), 0);
  const imageHeight = (text.match(/!\[[^\]]*]\([^)]+\)/g) || []).length * 280;
  const cleaned = text
    .replace(/\[\[attachment:\d+]]/g, '')
    .replace(/!\[[^\]]*]\([^)]+\)/g, '')
    .trim();
  return estimateWrappedTextHeight(cleaned, charsPerLine, 26) + markerHeight + imageHeight;
}

function estimateWrappedTextHeight(text, charsPerLine, rowHeight) {
  const length = String(text || '').trim().length;
  if (!length) return 0;
  return Math.max(1, Math.ceil(length / Math.max(1, charsPerLine))) * rowHeight + 8;
}

function looksLikeTableRow(line) {
  const value = String(line || '').trim();
  return value.includes('|') && value.split('|').length >= 3;
}

function isMarkdownImageLine(value) {
  return /^!\[[^\]]*]\([^)]+\)\s*$/.test(String(value || '').trim());
}

function attachmentMarkerOnlyIndex(value) {
  const match = String(value || '').trim().match(/^\[\[attachment:(\d+)]]$/);
  return match ? Number(match[1]) : null;
}

function isBlockBoundary(line) {
  const value = String(line || '');
  const trimmed = value.trim();
  return /^```/.test(trimmed)
    || /^\$\$\s*$/.test(trimmed)
    || /^\\\[\s*$/.test(trimmed)
    || /^\s{0,3}#{1,6}\s+/.test(value)
    || /^\s{0,3}(?:[-*_]\s*){3,}$/.test(value)
    || isMarkdownImageLine(trimmed)
    || attachmentMarkerOnlyIndex(trimmed) !== null
    || looksLikeTableRow(value)
    || /^\s*(?:[-*+]|\d+[.)])\s+/.test(value)
    || /^\s{0,3}>/.test(value);
}

function stableJsonPreview(value) {
  try {
    return JSON.stringify(value ?? '');
  } catch {
    return String(value || '');
  }
}

function clampHeight(value) {
  return Math.max(80, Math.min(18000, Math.ceil(Number(value) || 120)));
}
