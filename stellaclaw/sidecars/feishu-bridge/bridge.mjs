#!/usr/bin/env node
import * as Lark from '@larksuiteoapi/node-sdk';
import readline from 'node:readline';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const originalConsole = { ...console };
for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
  console[level] = (...args) => {
    process.stderr.write(`${args.map(formatConsoleArg).join(' ')}\n`);
  };
}

const appId = readRequiredEnv('STELLACLAW_FEISHU_APP_ID');
const appSecret = readRequiredEnv('STELLACLAW_FEISHU_APP_SECRET');
const domainName = process.env.STELLACLAW_FEISHU_DOMAIN || 'feishu';
const encryptKey = process.env.STELLACLAW_FEISHU_ENCRYPT_KEY || '';
const verificationToken = process.env.STELLACLAW_FEISHU_VERIFICATION_TOKEN || '';
const domain = resolveDomain(domainName);
const silentLogger = {
  trace() {},
  debug() {},
  info() {},
  warn() {},
  error() {},
  fatal() {},
};

if (Lark.defaultHttpInstance?.defaults) {
  Lark.defaultHttpInstance.defaults.proxy = false;
}

const client = new Lark.Client({
  appId,
  appSecret,
  appType: Lark.AppType.SelfBuild,
  domain,
  logger: silentLogger,
  loggerLevel: Lark.LoggerLevel.error,
});

let botOpenId = undefined;
let botName = undefined;
let wsClient = undefined;

try {
  await probeBotInfo();
  await startWebSocket();
  startCommandLoop();
  emit({ type: 'ready', bot_open_id: botOpenId, bot_name: botName });
} catch (error) {
  emitError(error, 'feishu bridge startup failed');
  process.exitCode = 1;
}

async function startWebSocket() {
  const dispatcher = new Lark.EventDispatcher({
    encryptKey,
    verificationToken,
  });
  dispatcher.register({
    'im.message.receive_v1': async (data) => {
      try {
        await handleMessageEvent(data);
      } catch (error) {
        emitError(error, 'failed to handle feishu message event');
      }
    },
  });

  wsClient = new Lark.WSClient({
    appId,
    appSecret,
    domain,
    logger: silentLogger,
    loggerLevel: Lark.LoggerLevel.error,
  });
  void wsClient.start({ eventDispatcher: dispatcher });
}

async function handleMessageEvent(data) {
  const event = data?.event ?? data;
  const message = event?.message ?? {};
  const sender = event?.sender ?? {};
  const senderId = extractSenderId(sender);
  if (isBotSender(senderId)) {
    return;
  }

  await emitMessage(message, sender, { source: 'websocket' });
}

function startCommandLoop() {
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  rl.on('line', (line) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    void handleCommandLine(trimmed).catch((error) => emitError(error, 'failed to handle feishu bridge command'));
  });
}

async function handleCommandLine(line) {
  const command = JSON.parse(line);
  switch (command.type) {
    case 'send_text':
      await sendText(command.chat_id, command.text);
      break;
    case 'set_typing':
      break;
    default:
      emit({ type: 'log', level: 'warn', message: 'unknown feishu bridge command', detail: { type: command.type } });
  }
}

async function sendText(chatId, text) {
  if (!chatId || !String(chatId).trim()) {
    throw new Error('send_text requires chat_id');
  }
  const chunks = splitText(String(text || ''), 30000);
  for (const chunk of chunks) {
    const response = await client.im.message.create({
      params: { receive_id_type: 'chat_id' },
      data: {
        receive_id: chatId,
        msg_type: 'text',
        content: JSON.stringify({ text: chunk }),
      },
    });
    emit({
      type: 'delivery',
      chat_id: chatId,
      message_id: response?.data?.message_id,
    });
  }
}

async function probeBotInfo() {
  try {
    const response = await client.request({
      method: 'GET',
      url: '/open-apis/bot/v3/info',
    });
    if (response?.code === 0) {
      botOpenId = response?.data?.open_id;
      botName = response?.data?.app_name || response?.data?.name;
    } else {
      emit({
        type: 'log',
        level: 'warn',
        message: 'failed to probe feishu bot info',
        detail: response,
      });
    }
  } catch (error) {
    emitError(error, 'failed to probe feishu bot info');
  }
}

async function emitMessage(message, sender, meta = {}) {
  const senderId = extractSenderId(sender);
  if (isBotSender(senderId)) {
    return false;
  }

  const messageId = message.message_id || '';
  const chatId = message.chat_id || '';
  const text = extractMessageText(message);
  if (!messageId || !chatId) {
    emit({
      type: 'log',
      level: 'warn',
      message: 'discarding feishu message without message_id or chat_id',
      detail: { message_id: messageId, chat_id: chatId, source: meta.source },
    });
    return false;
  }

  const attachments = await collectAttachments(messageId, message);
  emit({
    type: 'message',
    message_id: messageId,
    chat_id: chatId,
    chat_type: message.chat_type,
    create_time: renderFeishuTime(message.create_time),
    text,
    attachments,
    sender: {
      open_id: senderId.open_id,
      user_id: senderId.user_id,
      union_id: senderId.union_id,
      name: sender?.sender_name || sender?.sender_type || sender?.name,
    },
  });
  return true;
}

function extractSenderId(sender) {
  const id = sender?.sender_id ?? {};
  const idType = sender?.id_type;
  const rawId = sender?.id;
  return {
    open_id: id.open_id || (idType === 'open_id' ? rawId : undefined),
    user_id: id.user_id || (idType === 'user_id' ? rawId : undefined),
    union_id: id.union_id || (idType === 'union_id' ? rawId : undefined),
  };
}

function isBotSender(senderId) {
  return Boolean(botOpenId && senderId?.open_id && senderId.open_id === botOpenId);
}

async function collectAttachments(messageId, message) {
  const resources = extractMessageResources(message);
  const attachments = [];
  for (const resource of resources) {
    try {
      const response = await client.im.messageResource.get({
        path: {
          message_id: messageId,
          file_key: resource.file_key,
        },
        params: {
          type: resource.download_type,
        },
      });
      const { buffer, contentType, fileName } = await extractBufferFromResponse(response);
      attachments.push({
        kind: resource.kind,
        file_key: resource.file_key,
        name: resource.name || fileName || defaultResourceName(resource, contentType),
        media_type: normalizeContentType(contentType) || resource.media_type,
        data_base64: buffer.toString('base64'),
      });
    } catch (error) {
      attachments.push({
        kind: resource.kind,
        file_key: resource.file_key,
        name: resource.name || defaultResourceName(resource),
        media_type: resource.media_type,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }
  return attachments;
}

function extractMessageResources(message) {
  const messageType = String(message?.message_type || '').toLowerCase();
  const parsed = parseContent(message?.content);
  const resources = [];
  const seen = new Set();
  const push = (resource) => {
    if (!resource?.file_key) return;
    const key = `${resource.download_type}:${resource.file_key}`;
    if (seen.has(key)) return;
    seen.add(key);
    resources.push(resource);
  };

  if (messageType === 'image') {
    push({
      kind: 'image',
      download_type: 'image',
      file_key: parsed?.image_key,
      media_type: 'image/png',
    });
  } else if (messageType === 'file') {
    push({
      kind: 'file',
      download_type: 'file',
      file_key: parsed?.file_key,
      name: parsed?.file_name,
    });
  } else if (['audio', 'media', 'video'].includes(messageType)) {
    push({
      kind: messageType,
      download_type: 'file',
      file_key: parsed?.file_key,
      name: parsed?.file_name,
    });
  }

  collectInlineResources(parsed, push);
  return resources;
}

function collectInlineResources(value, push) {
  const visit = (node) => {
    if (Array.isArray(node)) {
      for (const child of node) visit(child);
      return;
    }
    if (!node || typeof node !== 'object') return;
    if (typeof node.image_key === 'string') {
      push({
        kind: 'image',
        download_type: 'image',
        file_key: node.image_key,
        media_type: 'image/png',
      });
    }
    if (typeof node.file_key === 'string') {
      push({
        kind: node.tag === 'media' || node.tag === 'video' ? 'video' : 'file',
        download_type: 'file',
        file_key: node.file_key,
        name: typeof node.file_name === 'string' ? node.file_name : undefined,
      });
    }
    if (Array.isArray(node.content)) visit(node.content);
  };
  visit(value);
}

function extractMessageText(message) {
  const raw = message?.content;
  if (!raw) return '';
  const parsed = parseContent(raw);
  if (!parsed || typeof parsed !== 'object') return String(raw);
  if (typeof parsed?.text === 'string') return parsed.text;
  if (typeof parsed?.title === 'string' && Array.isArray(parsed?.content)) {
    const body = flattenPostContent(parsed.content).trim();
    return body ? `${parsed.title}\n${body}` : parsed.title;
  }
  if (Array.isArray(parsed?.content)) return flattenPostContent(parsed.content);
  return '';
}

function parseContent(raw) {
  if (!raw) return undefined;
  if (typeof raw !== 'string') return raw;
  try {
    return JSON.parse(raw);
  } catch {
    return undefined;
  }
}

function flattenPostContent(value) {
  const parts = [];
  const visit = (node) => {
    if (Array.isArray(node)) {
      for (const child of node) visit(child);
      return;
    }
    if (!node || typeof node !== 'object') return;
    if (typeof node.text === 'string') parts.push(node.text);
    if (typeof node.name === 'string' && node.tag === 'at') parts.push(`@${node.name}`);
    if (Array.isArray(node.content)) visit(node.content);
  };
  visit(value);
  return parts.join('');
}

async function extractBufferFromResponse(response) {
  if (Buffer.isBuffer(response)) {
    return { buffer: response };
  }
  if (response instanceof ArrayBuffer) {
    return { buffer: Buffer.from(response) };
  }
  if (response == null) {
    throw new Error('received empty resource response');
  }

  const resp = response;
  const contentType = resp.headers?.['content-type'] || resp.headers?.['Content-Type'] || resp.contentType;
  const fileName = contentDispositionFileName(
    resp.headers?.['content-disposition'] || resp.headers?.['Content-Disposition'],
  );

  if (resp.data != null) {
    if (Buffer.isBuffer(resp.data)) return { buffer: resp.data, contentType, fileName };
    if (resp.data instanceof ArrayBuffer) return { buffer: Buffer.from(resp.data), contentType, fileName };
    if (typeof resp.data.pipe === 'function') {
      return { buffer: await streamToBuffer(resp.data), contentType, fileName };
    }
  }
  if (typeof resp.getReadableStream === 'function') {
    return { buffer: await streamToBuffer(resp.getReadableStream()), contentType, fileName };
  }
  if (typeof resp.writeFile === 'function') {
    const tmpFile = path.join(os.tmpdir(), `stellaclaw-feishu-${Date.now()}-${Math.random().toString(16).slice(2)}`);
    try {
      await resp.writeFile(tmpFile);
      return { buffer: fs.readFileSync(tmpFile), contentType, fileName };
    } finally {
      try {
        fs.unlinkSync(tmpFile);
      } catch {
        // Best-effort cleanup only.
      }
    }
  }
  if (typeof resp[Symbol.asyncIterator] === 'function' || typeof resp.next === 'function') {
    const chunks = [];
    const iterable = typeof resp[Symbol.asyncIterator] === 'function' ? resp : asyncIteratorToIterable(resp);
    for await (const chunk of iterable) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    }
    return { buffer: Buffer.concat(chunks), contentType, fileName };
  }
  if (typeof resp.pipe === 'function') {
    return { buffer: await streamToBuffer(resp), contentType, fileName };
  }

  throw new Error('unable to extract resource bytes from Feishu response');
}

function streamToBuffer(stream) {
  if (typeof stream?.[Symbol.asyncIterator] === 'function') {
    return asyncIterableToBuffer(stream);
  }
  if (typeof stream?.getReader === 'function') {
    return webStreamToBuffer(stream);
  }
  return new Promise((resolve, reject) => {
    const chunks = [];
    stream.on('data', (chunk) => chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)));
    stream.on('end', () => resolve(Buffer.concat(chunks)));
    stream.on('error', reject);
  });
}

async function asyncIterableToBuffer(iterable) {
  const chunks = [];
  for await (const chunk of iterable) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}

async function webStreamToBuffer(stream) {
  const reader = stream.getReader();
  const chunks = [];
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(Buffer.isBuffer(value) ? value : Buffer.from(value));
  }
  return Buffer.concat(chunks);
}

async function* asyncIteratorToIterable(iterator) {
  while (true) {
    const { value, done } = await iterator.next();
    if (done) break;
    yield value;
  }
}

function contentDispositionFileName(value) {
  if (typeof value !== 'string') return undefined;
  const match = value.match(/filename[*]?=(?:UTF-8'')?["']?([^"';\n]+)/i);
  if (!match) return undefined;
  try {
    return decodeURIComponent(match[1].trim());
  } catch {
    return match[1].trim();
  }
}

function normalizeContentType(value) {
  if (typeof value !== 'string') return undefined;
  const clean = value.split(';')[0].trim().toLowerCase();
  return clean || undefined;
}

function defaultResourceName(resource, contentType) {
  const extension = extensionForContentType(contentType) || (resource.download_type === 'image' ? 'png' : 'bin');
  return `${resource.kind || resource.download_type}-${resource.file_key}.${extension}`;
}

function extensionForContentType(value) {
  switch (normalizeContentType(value)) {
    case 'image/png':
      return 'png';
    case 'image/jpeg':
      return 'jpg';
    case 'image/webp':
      return 'webp';
    case 'image/gif':
      return 'gif';
    case 'application/pdf':
      return 'pdf';
    case 'text/plain':
      return 'txt';
    case 'audio/mpeg':
      return 'mp3';
    case 'audio/ogg':
      return 'ogg';
    case 'video/mp4':
      return 'mp4';
    default:
      return undefined;
  }
}

function splitText(text, maxLength) {
  if (!text) return [''];
  const chunks = [];
  for (let index = 0; index < text.length; index += maxLength) {
    chunks.push(text.slice(index, index + maxLength));
  }
  return chunks;
}

function renderFeishuTime(value) {
  if (!value) return undefined;
  const numeric = Number(value);
  if (Number.isFinite(numeric)) {
    const millis = numeric > 100000000000 ? numeric : numeric * 1000;
    return new Date(millis).toISOString();
  }
  return String(value);
}

function resolveDomain(value) {
  if (value === 'feishu') return Lark.Domain.Feishu;
  if (value === 'lark') return Lark.Domain.Lark;
  return value.replace(/\/+$/, '');
}

function readRequiredEnv(name) {
  const value = process.env[name];
  if (!value || !value.trim()) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function emit(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

function emitError(error, message) {
  emit({
    type: 'error',
    message,
    detail: feishuErrorDetail(error),
  });
}

function feishuErrorDetail(error) {
  return {
    error: error instanceof Error ? error.message : String(error),
    stack: error instanceof Error ? error.stack : undefined,
    status: error?.response?.status,
    response: error?.response?.data,
    code: error?.code,
  };
}

function formatConsoleArg(value) {
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

process.on('SIGTERM', closeAndExit);
process.on('SIGINT', closeAndExit);

function closeAndExit() {
  try {
    wsClient?.close?.({ force: true });
  } catch (error) {
    originalConsole.error(error);
  }
  process.exit(0);
}
