import { common, createLowlight } from 'lowlight';

const DEFAULT_PREFIX = 'hljs-';
const CACHE_LIMIT = 360;
const lowlight = createLowlight(common);
const highlightCache = new Map();

export function highlightCodeText(language, code, prefix = DEFAULT_PREFIX) {
  const normalizedLanguage = String(language || '').trim();
  const highlighted = normalizedLanguage
    ? cachedHighlight(normalizedLanguage, String(code || ''), prefix)
    : cachedAutoHighlight(String(code || ''), prefix);
  return highlighted || { children: [{ type: 'text', value: String(code || '') }], language: '' };
}

export function cachedRehypeHighlight(options = {}) {
  const prefix = typeof options.prefix === 'string' ? options.prefix : DEFAULT_PREFIX;
  const plainText = Array.isArray(options.plainText) ? options.plainText : [];
  const className = highlightRootClass(prefix);
  return function transform(tree) {
    visitElements(tree, (node, parent) => {
      if (
        node.tagName !== 'code'
        || !parent
        || parent.type !== 'element'
        || parent.tagName !== 'pre'
      ) {
        return;
      }
      const language = codeLanguage(node);
      if (language && plainText.includes(language)) return;

      const classes = ensureClassList(node);
      if (!classes.includes(className)) classes.unshift(className);

      const code = hastText(node);
      const highlighted = language
        ? cachedHighlight(language, code, prefix)
        : cachedAutoHighlight(code, prefix);
      if (!highlighted) return;
      if (!language && highlighted.language && !classes.includes(`language-${highlighted.language}`)) {
        classes.push(`language-${highlighted.language}`);
      }
      node.children = cloneHastChildren(highlighted.children);
    });
  };
}

function cachedHighlight(language, code, prefix) {
  const key = `${language}\u0000${prefix}\u0000${code.length}\u0000${hashText(code)}`;
  const cached = highlightCache.get(key);
  if (cached) {
    highlightCache.delete(key);
    highlightCache.set(key, cached);
    return cached;
  }
  let result;
  try {
    result = lowlight.highlight(language, code, { prefix });
  } catch {
    return null;
  }
  const cachedResult = { children: cloneHastChildren(result.children || []) };
  highlightCache.set(key, cachedResult);
  while (highlightCache.size > CACHE_LIMIT) {
    highlightCache.delete(highlightCache.keys().next().value);
  }
  return cachedResult;
}

function cachedAutoHighlight(code, prefix) {
  const key = `auto\u0000${prefix}\u0000${code.length}\u0000${hashText(code)}`;
  const cached = highlightCache.get(key);
  if (cached) {
    highlightCache.delete(key);
    highlightCache.set(key, cached);
    return cached;
  }
  let result;
  try {
    result = lowlight.highlightAuto(code, { prefix });
  } catch {
    return null;
  }
  const cachedResult = {
    children: cloneHastChildren(result.children || []),
    language: typeof result.data?.language === 'string' ? result.data.language : ''
  };
  highlightCache.set(key, cachedResult);
  while (highlightCache.size > CACHE_LIMIT) {
    highlightCache.delete(highlightCache.keys().next().value);
  }
  return cachedResult;
}

function visitElements(node, visitor, parent = null) {
  if (!node || typeof node !== 'object') return;
  if (node.type === 'element') visitor(node, parent);
  const children = Array.isArray(node.children) ? node.children : [];
  children.forEach((child) => visitElements(child, visitor, node));
}

function codeLanguage(node) {
  const classes = Array.isArray(node.properties?.className) ? node.properties.className : [];
  for (const item of classes) {
    const value = String(item || '');
    if (value === 'no-highlight' || value === 'nohighlight') return '';
    if (value.startsWith('language-')) return value.slice(9);
    if (value.startsWith('lang-')) return value.slice(5);
  }
  return '';
}

function ensureClassList(node) {
  if (!node.properties || typeof node.properties !== 'object') node.properties = {};
  if (Array.isArray(node.properties.className)) return node.properties.className;
  if (node.properties.className) {
    node.properties.className = String(node.properties.className).split(/\s+/).filter(Boolean);
  } else {
    node.properties.className = [];
  }
  return node.properties.className;
}

function highlightRootClass(prefix) {
  const index = prefix.indexOf('-');
  return index < 0 ? prefix : prefix.slice(0, index);
}

function hastText(node) {
  if (!node || typeof node !== 'object') return '';
  if (node.type === 'text') return String(node.value || '');
  return (Array.isArray(node.children) ? node.children : []).map(hastText).join('');
}

function cloneHastChildren(children) {
  return children.map((child) => cloneHastNode(child));
}

function cloneHastNode(node) {
  if (!node || typeof node !== 'object') return node;
  const clone = { ...node };
  if (node.properties && typeof node.properties === 'object') {
    clone.properties = { ...node.properties };
    if (Array.isArray(node.properties.className)) {
      clone.properties.className = [...node.properties.className];
    }
  }
  if (Array.isArray(node.children)) {
    clone.children = node.children.map((child) => cloneHastNode(child));
  }
  return clone;
}

function hashText(text) {
  const value = String(text || '');
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
}
