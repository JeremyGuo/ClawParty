// ClawParty Web — minimal client
(function () {
  'use strict';

  const AUTH_KEY = 'clawparty_auth_token';
  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input');
  const formEl = document.getElementById('input-form');
  const statusEl = document.getElementById('connection-status');

  let ws = null;
  let token = localStorage.getItem(AUTH_KEY) || '';

  // ── Auth ──────────────────────────────────────────────────
  function ensureToken() {
    if (!token) {
      token = prompt('Enter auth token (or leave empty for open channels):') || '';
      if (token) localStorage.setItem(AUTH_KEY, token);
    }
    return token;
  }

  function authHeaders() {
    const h = { 'Content-Type': 'application/json' };
    if (token) h['Authorization'] = 'Bearer ' + token;
    return h;
  }

  // ── Messaging ─────────────────────────────────────────────
  function appendMessage(role, text, meta) {
    const div = document.createElement('div');
    div.className = 'msg ' + role;
    if (meta) {
      const metaEl = document.createElement('div');
      metaEl.className = 'meta';
      metaEl.textContent = meta;
      div.appendChild(metaEl);
    }
    const body = document.createElement('span');
    body.textContent = text;
    div.appendChild(body);
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  async function sendMessage(text) {
    appendMessage('user', text);
    try {
      const resp = await fetch('/api/send', {
        method: 'POST',
        headers: authHeaders(),
        body: JSON.stringify({ text }),
      });
      if (!resp.ok) {
        const err = await resp.text();
        appendMessage('event', '⚠ Send failed: ' + resp.status + ' ' + err);
      }
    } catch (e) {
      appendMessage('event', '⚠ Network error: ' + e.message);
    }
  }

  // ── WebSocket ─────────────────────────────────────────────
  function connectWS() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = proto + '//' + location.host + '/ws';
    ws = new WebSocket(url);

    ws.onopen = function () {
      statusEl.className = 'connected';
      statusEl.title = 'Connected';
      // If auth is needed, send it as first message.
      if (token) {
        ws.send(JSON.stringify({ type: 'auth', token: token }));
      }
    };
    ws.onclose = function () {
      statusEl.className = 'disconnected';
      statusEl.title = 'Disconnected';
      setTimeout(connectWS, 3000);
    };
    ws.onerror = function () {
      ws.close();
    };
    ws.onmessage = function (evt) {
      try {
        const data = JSON.parse(evt.data);
        handleEvent(data);
      } catch (e) {
        console.warn('bad ws message', evt.data);
      }
    };
  }

  function handleEvent(data) {
    switch (data.type) {
      case 'outgoing_message':
        appendMessage('assistant', data.text);
        break;
      case 'session_event':
        appendMessage('event', data.event_summary);
        break;
      case 'processing':
        appendMessage('event', '⏳ ' + data.state);
        break;
      case 'media_group':
        appendMessage('event', '🖼 media group (' + data.count + ' items)');
        break;
      default:
        appendMessage('event', JSON.stringify(data));
    }
  }

  // ── Init ──────────────────────────────────────────────────
  formEl.addEventListener('submit', function (e) {
    e.preventDefault();
    const text = inputEl.value.trim();
    if (!text) return;
    inputEl.value = '';
    sendMessage(text);
  });

  inputEl.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event('submit'));
    }
  });

  ensureToken();
  connectWS();
})();
