const IMPORTANT_TEXT_LIMIT = 900;
const RAW_TEXT_LIMIT = 12_000;

export function parseToolDetailPayload(payload, options = {}) {
  if (!payload) return {};
  if (typeof payload === 'object') return payload;
  const value = String(payload || '').trim();
  if (!value) return {};
  const maxJsonChars = Number(options.maxJsonChars ?? Number.POSITIVE_INFINITY);
  if (Number.isFinite(maxJsonChars) && value.length > maxJsonChars) {
    return { text: value };
  }
  try {
    return JSON.parse(value);
  } catch {
    return { text: value };
  }
}

export function payloadToRawText(payload, limit = RAW_TEXT_LIMIT) {
  const text = typeof payload === 'string'
    ? payload
    : JSON.stringify(payload ?? '', null, 2);
  return limitText(text, limit);
}

export function payloadToModelText(payload, fallback = '') {
  if (payload === undefined || payload === null || payload === '') {
    return toolResultModelText(parseToolDetailPayload(fallback));
  }
  if (typeof payload === 'string') return payload;
  if (typeof payload?.text === 'string') return payload.text;
  if (typeof payload?.context === 'string') return payload.context;
  if (typeof payload?.context?.text === 'string') return payload.context.text;
  return toolResultModelText(payload);
}

export function toolResultModelText(value) {
  if (value === undefined || value === null || value === '') return '';
  if (typeof value === 'string') return value;
  const kind = value?.kind;
  if (kind === 'shell_result') return shellResultModelText(value);
  if (kind === 'text_result') return String(value.text || '');
  if (kind === 'json_result') return prettyJson(value.value);
  return prettyJson(value);
}

export function importantToolFields(name, payload, mode = 'call') {
  const data = parseToolDetailPayload(payload);
  const kind = toolKind(name);
  if (kind === 'command') return commandFields(data, mode);
  if (kind === 'edit') return editFields(data, mode);
  if (kind === 'web') return webFields(data, mode);
  if (kind === 'search') return searchFields(data, mode);
  if (kind === 'read') return readFields(data, mode);
  if (kind === 'image') return imageFields(data, mode);
  if (kind === 'plan') return planFields(data, mode);
  if (kind === 'agent') return agentFields(data, mode);
  if (kind === 'skill') return skillFields(data, mode);
  if (kind === 'cron') return cronFields(data, mode);
  if (kind === 'memory') return memoryFields(data, mode);
  return genericFields(data);
}

export function toolKind(name) {
  const value = String(name || '').toLowerCase();
  if (value.includes('shell') || value.includes('command') || value.includes('terminal') || value.includes('stdin')) return 'command';
  if (value.includes('edit') || value.includes('write') || value.includes('patch')) return 'edit';
  if (value.includes('fetch') || value.includes('browser') || value.includes('open_url')) return 'web';
  if (value.includes('search') || value.includes('grep') || value === 'rg') return 'search';
  if (value.includes('file_read') || value.includes('read')) return 'read';
  if (value.includes('image') || value.includes('screenshot')) return 'image';
  if (value.includes('plan')) return 'plan';
  if (value.includes('agent') || value.includes('subagent')) return 'agent';
  if (value.includes('skill')) return 'skill';
  if (value.includes('cron') || value.includes('automation')) return 'cron';
  if (value.includes('memory')) return 'memory';
  return 'tool';
}

function commandFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['状态', firstValue(data, ['status', 'state'])],
      ['退出码', firstValue(data, ['exit_code', 'exitCode', 'code'])],
      ['耗时', firstValue(data, ['duration', 'elapsed', 'elapsed_ms'])],
      ['超时', firstValue(data, ['timed_out', 'timeout'])],
      ['输出摘要', shellOutputPreview(data)],
      ['错误', errorPreview(data)]
    ]);
  }
  return fields([
    ['命令', firstValue(data, ['cmd', 'command', 'text'])],
    ['工作目录', firstValue(data, ['workdir', 'cwd', 'directory'])],
    ['等待时间', firstValue(data, ['yield_time_ms', 'timeout_ms', 'timeout'])],
    ['最大输出', firstValue(data, ['max_output_tokens', 'max_output_chars', 'limit'])]
  ]);
}

function editFields(data, mode) {
  const patch = firstValue(data, ['patch', 'diff', 'text']);
  if (mode === 'result') {
    return fields([
      ['结果', firstValue(data, ['applied', 'success', 'status', 'ok'])],
      ['文件', firstValue(data, ['files', 'edited_files', 'changed_files', 'file', 'path', 'file_path'])],
      ['新增/删除', lineDelta(data)],
      ['错误', firstValue(data, ['error', 'message'])]
    ]);
  }
  return fields([
    ['文件', firstValue(data, ['file', 'path', 'file_path', 'target'])],
    ['操作', firstValue(data, ['action', 'operation', 'kind'])],
    ['Patch', patchSummary(patch)],
    ['最大输出', firstValue(data, ['max_output_chars', 'max_output_tokens'])]
  ]);
}

function webFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['标题', firstValue(data, ['title'])],
      ['URL', firstValue(data, ['url', 'final_url', 'href'])],
      ['状态', firstValue(data, ['status', 'status_code', 'code'])],
      ['内容摘要', firstValue(data, ['summary', 'description', 'text', 'content', 'markdown'])]
    ]);
  }
  return fields([
    ['URL', firstValue(data, ['url', 'href', 'uri', 'text'])],
    ['格式', firstValue(data, ['format', 'response_format'])],
    ['超时', firstValue(data, ['timeout_ms', 'timeout'])]
  ]);
}

function searchFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['结果数', resultCount(data)],
      ['命中', topResults(data)],
      ['摘要', firstValue(data, ['summary', 'text', 'content'])]
    ]);
  }
  return fields([
    ['查询', firstValue(data, ['query', 'q', 'pattern', 'text'])],
    ['路径/域名', firstValue(data, ['path', 'directory', 'cwd', 'domain', 'domains'])],
    ['数量', firstValue(data, ['count', 'limit', 'num_results'])]
  ]);
}

function readFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['文件', firstValue(data, ['path', 'file_path', 'file'])],
      ['行数/大小', firstValue(data, ['line_count', 'lines', 'size', 'bytes'])],
      ['摘要', firstValue(data, ['summary', 'text', 'content'])]
    ]);
  }
  return fields([
    ['文件', firstValue(data, ['path', 'file_path', 'file'])],
    ['范围', rangeValue(data)],
    ['最大输出', firstValue(data, ['max_output_chars', 'max_output_tokens', 'limit'])]
  ]);
}

function imageFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['图片', firstValue(data, ['path', 'file_path', 'file', 'url', 'uri', 'images', 'files'])],
      ['尺寸', imageSize(data)],
      ['摘要', firstValue(data, ['summary', 'text', 'content', 'description'])]
    ]);
  }
  return fields([
    ['图片/路径', firstValue(data, ['path', 'file_path', 'file', 'url', 'uri'])],
    ['Prompt', firstValue(data, ['prompt', 'text', 'description'])],
    ['尺寸', imageSize(data)]
  ]);
}

function planFields(data) {
  return fields([
    ['计划', firstValue(data, ['plan', 'steps', 'items'])],
    ['状态', firstValue(data, ['status', 'state'])],
    ['说明', firstValue(data, ['explanation', 'summary', 'text'])]
  ]);
}

function agentFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['Agent', firstValue(data, ['agent_id', 'id', 'target', 'name'])],
      ['状态', firstValue(data, ['status', 'state'])],
      ['结果', firstValue(data, ['result', 'output', 'summary', 'text'])],
      ['错误', firstValue(data, ['error'])]
    ]);
  }
  return fields([
    ['Agent', firstValue(data, ['agent_id', 'id', 'target', 'name'])],
    ['任务', firstValue(data, ['task', 'message', 'prompt', 'text'])],
    ['模型', firstValue(data, ['model', 'reasoning_effort'])]
  ]);
}

function skillFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['技能', firstValue(data, ['skill', 'name', 'skill_name'])],
      ['状态', firstValue(data, ['status', 'state'])],
      ['结果', firstValue(data, ['result', 'summary', 'text', 'output'])]
    ]);
  }
  return fields([
    ['技能', firstValue(data, ['skill', 'name', 'skill_name'])],
    ['操作', firstValue(data, ['action', 'operation'])],
    ['参数', firstValue(data, ['args', 'arguments', 'input'])]
  ]);
}

function cronFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['任务', firstValue(data, ['id', 'name', 'cron_id'])],
      ['状态', firstValue(data, ['status', 'state'])],
      ['结果', firstValue(data, ['result', 'summary', 'text'])]
    ]);
  }
  return fields([
    ['任务', firstValue(data, ['id', 'name', 'cron_id'])],
    ['操作', firstValue(data, ['action', 'operation'])],
    ['计划', firstValue(data, ['schedule', 'rrule', 'time'])]
  ]);
}

function memoryFields(data, mode) {
  if (mode === 'result') {
    return fields([
      ['命中数', resultCount(data)],
      ['内容', firstValue(data, ['results', 'memories', 'summary', 'text', 'content'])]
    ]);
  }
  return fields([
    ['查询', firstValue(data, ['query', 'q', 'text'])],
    ['操作', firstValue(data, ['action', 'operation'])],
    ['范围', firstValue(data, ['scope', 'namespace'])]
  ]);
}

function genericFields(data) {
  const entries = Object.entries(data || {})
    .filter(([, value]) => value !== undefined && value !== null && value !== '')
    .filter(([key, value]) => !isLargeField(key, value))
    .slice(0, 8)
    .map(([key, value]) => [key, value]);
  if (entries.length) return fields(entries);
  if (typeof data?.text === 'string') return fields([['内容', data.text]]);
  return [];
}

function fields(entries) {
  return entries
    .map(([label, value]) => ({ label, value: normalizeImportantValue(value) }))
    .filter((entry) => entry.value !== '');
}

function firstValue(data, keys) {
  for (const key of keys) {
    const value = getDeep(data, key);
    if (!isEmptyImportantValue(value)) return value;
  }
  return '';
}

function getDeep(data, key) {
  if (!data || typeof data !== 'object') return undefined;
  if (Object.prototype.hasOwnProperty.call(data, key)) return data[key];
  const parts = String(key).split('.');
  let current = data;
  for (const part of parts) {
    if (!current || typeof current !== 'object' || !Object.prototype.hasOwnProperty.call(current, part)) {
      return undefined;
    }
    current = current[part];
  }
  return current;
}

function normalizeImportantValue(value) {
  if (isEmptyImportantValue(value)) return '';
  if (typeof value === 'boolean') return value ? '是' : '否';
  if (typeof value === 'number') return String(value);
  if (typeof value === 'string') return limitText(value, IMPORTANT_TEXT_LIMIT);
  if (Array.isArray(value)) {
    if (!value.length) return '';
    if (value.every((item) => ['string', 'number', 'boolean'].includes(typeof item))) {
      return limitText(value.join('\n'), IMPORTANT_TEXT_LIMIT);
    }
    return limitText(value.slice(0, 5).map((item) => summarizeObject(item)).join('\n'), IMPORTANT_TEXT_LIMIT);
  }
  return limitText(summarizeObject(value), IMPORTANT_TEXT_LIMIT);
}

function summarizeObject(value) {
  if (!value || typeof value !== 'object') return String(value ?? '');
  const explicitText = firstValue(value, ['summary', 'text', 'content', 'description', 'message', 'error']);
  if (explicitText) return String(explicitText);
  const truncation = truncationSummary(value);
  if (truncation) return truncation;
  const title = firstValue(value, ['title', 'name', 'path', 'file', 'url', 'uri', 'id']);
  const detail = firstValue(value, ['status', 'state', 'kind', 'type']);
  if (title || detail) return [title, detail].filter(Boolean).join(' - ');
  const primitives = Object.entries(value)
    .filter(([, item]) => ['string', 'number', 'boolean'].includes(typeof item))
    .filter(([, item]) => !isEmptyImportantValue(item))
    .slice(0, 6)
    .map(([key, item]) => `${key}: ${typeof item === 'boolean' ? (item ? '是' : '否') : item}`);
  return primitives.join('\n');
}

function shellOutputPreview(data) {
  const output = firstValue(data, ['output.text', 'output', 'stdout', 'stderr', 'text']);
  if (typeof output === 'object') {
    return [output.stdout, output.stderr, output.text].filter(Boolean).join('\n');
  }
  const lines = String(output || '')
    .split(/\r?\n/)
    .map((line) => line.trimEnd())
    .filter(Boolean);
  return lines.slice(-8).join('\n');
}

function shellResultModelText(value) {
  const parts = [];
  const running = Boolean(value?.running);
  const timedOut = Boolean(value?.timed_out);
  const exitCode = value?.exit_code;
  const wallTimeSeconds = Number(value?.wall_time_seconds ?? (Number(value?.duration_ms || 0) / 1000)) || 0;
  if (running) {
    parts.push(`Process running with session ID ${value?.session_id || value?.process_id || ''}`);
  } else if (timedOut) {
    parts.push('Process timed out');
  } else if (exitCode !== undefined && exitCode !== null) {
    parts.push(`Process exited with code ${exitCode}`);
  } else {
    parts.push('Process exited');
  }
  parts.push(`Wall time: ${wallTimeSeconds.toFixed(4)} seconds`);
  const originalTokenCount = value?.output?.original_token_count;
  if (originalTokenCount !== undefined && originalTokenCount !== null) {
    parts.push(`Original token count: ${originalTokenCount}`);
  }
  if (value?.tty) {
    pushShellStreamText(parts, 'Output', value.output);
  } else {
    pushShellStreamText(parts, 'Stdout', value.stdout);
    pushShellStreamText(parts, 'Stderr', value.stderr);
    if (!hasStreamText(value.stdout) && !hasStreamText(value.stderr)) {
      pushShellStreamText(parts, 'Output', value.output);
    }
  }
  if (value?.terminal_snapshot && typeof value.terminal_snapshot === 'object') {
    const snapshot = value.terminal_snapshot;
    parts.push(`Terminal snapshot: alternate_screen=${Boolean(snapshot.alternate_screen)}, saw_alternate_screen=${Boolean(snapshot.saw_alternate_screen)}, truncated=${Boolean(snapshot.truncated)}\n${snapshot.visible_text || ''}`);
  }
  return parts.join('\n');
}

function pushShellStreamText(parts, label, value) {
  if (!value || typeof value !== 'object') return;
  const text = String(value.text || '');
  const truncated = Boolean(value.truncated);
  if (!text && !truncated) return;
  const header = truncated ? `${label} (truncated)` : label;
  parts.push(`${header}:\n${text || '<empty>'}`);
}

function hasStreamText(value) {
  return Boolean(value && typeof value === 'object' && String(value.text || ''));
}

function prettyJson(value) {
  if (value === undefined || value === null) return '';
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function errorPreview(data) {
  const error = firstValue(data, ['error', 'message']);
  if (error) return error;
  const stderr = firstValue(data, ['stderr', 'output.stderr']);
  return stderr || '';
}

function lineDelta(data) {
  const added = firstValue(data, ['added', 'additions', 'lines_added']);
  const removed = firstValue(data, ['removed', 'deletions', 'lines_removed']);
  if (added === '' && removed === '') return '';
  return `+${Number(added || 0)} / -${Number(removed || 0)}`;
}

function patchSummary(patch) {
  const text = String(patch || '');
  if (!text.trim()) return '';
  const files = Array.from(text.matchAll(/^\*\*\* (?:Update|Add|Delete) File:\s+(.+)$/gm)).map((match) => match[1].trim());
  if (files.length) return files.slice(0, 8).join('\n');
  const diffFiles = Array.from(text.matchAll(/^diff --git a\/.+? b\/(.+)$/gm)).map((match) => match[1].trim());
  if (diffFiles.length) return diffFiles.slice(0, 8).join('\n');
  return `${text.split(/\r?\n/).length} 行 patch`;
}

function resultCount(data) {
  const direct = firstValue(data, ['count', 'total', 'result_count']);
  if (direct !== '') return direct;
  const results = firstValue(data, ['results', 'items']);
  return Array.isArray(results) ? results.length : '';
}

function topResults(data) {
  const results = firstValue(data, ['results', 'items']);
  if (!Array.isArray(results)) return '';
  return results.slice(0, 5).map((item) => summarizeObject(item)).join('\n');
}

function rangeValue(data) {
  const start = firstValue(data, ['start', 'line_start', 'offset']);
  const end = firstValue(data, ['end', 'line_end', 'limit']);
  if (start === '' && end === '') return '';
  return [start, end].filter((value) => value !== '').join(' - ');
}

function imageSize(data) {
  const width = firstValue(data, ['width', 'pixel_width']);
  const height = firstValue(data, ['height', 'pixel_height']);
  if (width && height) return `${width} x ${height}`;
  return '';
}

function isLargeField(key, value) {
  const lower = String(key || '').toLowerCase();
  if (['patch', 'diff', 'stdout', 'stderr', 'output', 'content', 'text', 'context'].includes(lower)) {
    return String(typeof value === 'string' ? value : JSON.stringify(value)).length > 600;
  }
  return false;
}

function isEmptyImportantValue(value) {
  if (value === undefined || value === null || value === '') return true;
  if (typeof value === 'string') return value.trim() === '';
  if (Array.isArray(value)) return value.length === 0;
  if (typeof value !== 'object') return false;
  const text = value.text ?? value.content ?? value.summary ?? value.message ?? value.error;
  if (typeof text === 'string' && text.trim()) return false;
  if (value.truncated === true) return false;
  if (
    Object.prototype.hasOwnProperty.call(value, 'original_chars')
    || Object.prototype.hasOwnProperty.call(value, 'original_token_count')
  ) {
    return true;
  }
  return Object.keys(value).length === 0;
}

function truncationSummary(value) {
  if (!value || typeof value !== 'object' || value.truncated !== true) return '';
  const chars = Number(value.original_chars || 0);
  const tokens = Number(value.original_token_count || 0);
  const parts = ['已截断'];
  if (chars > 0) parts.push(`${chars} chars`);
  if (tokens > 0) parts.push(`${tokens} tokens`);
  return parts.join(' · ');
}

function limitText(text, limit) {
  const value = String(text || '');
  if (value.length <= limit) return value;
  return `${value.slice(0, limit - 1)}...`;
}
