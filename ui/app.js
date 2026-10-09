'use strict';

// ---------------------------------------------------------------- state

const $ = (sel, el = document) => el.querySelector(sel);
const view = $('#view');

const S = {
  key: localStorage.getItem('cliproxyapi-rust.key') || '',
  locked: null, // null | 'key' | 'remote'
  route: 'overview',
  overview: null,
  accounts: null,
  requests: [],
  models: [],
  live: 'connecting',
  paused: false,
  filter: '',
  panel: null, // 'claude' | 'codex' | 'key'
  login: null, // { state, provider, url, callback, status, message }
  keyProvider: 'claude',
  snippet: localStorage.getItem('cliproxyapi-rust.snippet') || 'claude',
  setup: localStorage.getItem('cliproxyapi-rust.setup'), // 'open' | 'closed' | null (auto)
  private: localStorage.getItem('cliproxyapi-rust.private') === '1', // hide emails and keys
  quotaDisplay: localStorage.getItem('cliproxyapi-rust.quota-display') === 'remaining' ? 'remaining' : 'used',
  confirm: null,
  resets: {}, // account-specific confirmations and errors
  resetModal: null,
  notifications: { status: null, loading: false, error: null, busy: null, testMsg: null, filter: '', credentialEditor: null, credentialRemove: null, credentialMsg: null },
  config: { values: null, saved: null, defaults: {}, revision: '', path: '', ignored: [], restart_fields: [],
    msg: null, busy: false, loading: false, section: 'server', provider: 'claude', oauthProvider: 'claude',
    errors: {}, opens: {}, secrets: {}, reloadConfirm: false, reveal: false, raw: { text: null, saved: null, loading: false } },
};

const PROVIDER = {
  claude: 'Claude', codex: 'Codex', gemini: 'Gemini', vertex: 'Vertex AI', antigravity: 'Antigravity',
  kimi: 'Kimi', xai: 'Grok', meta: 'Meta', devin: 'Devin', 'openai-compat': 'Compatible',
};
// Accounts you can sign in to: [id, name, what it connects].
const SIGNIN = [
  ['claude', 'Claude', 'Pro or Max subscription'],
  ['codex', 'ChatGPT', 'Plus, Pro or Team, for Codex models'],
  ['antigravity', 'Antigravity', 'Google account, Gemini and Claude models'],
  ['xai', 'Grok', 'SuperGrok or X Premium'],
  ['kimi', 'Kimi', 'Kimi Code membership'],
  ['meta', 'Meta', 'Muse Spark'],
  ['devin', 'Devin', 'Devin or Windsurf account'],
  ['vertex', 'Vertex AI', 'Google Cloud service account key'],
];
const LOGIN = {
  claude: { name: 'Claude', intro: 'Connect a Claude Pro or Max subscription.', port: 54545, example: 'http://localhost:54545/callback?code=…&state=…' },
  codex: { name: 'ChatGPT', intro: 'Connect a ChatGPT Plus, Pro or Team subscription for Codex models.', port: 1455, example: 'http://localhost:1455/auth/callback?code=…&state=…' },
  antigravity: { name: 'Antigravity', intro: 'Connect a Google account with Antigravity access for Gemini and Claude models.', port: 51121, example: 'http://localhost:51121/oauth-callback?code=…&state=…' },
  devin: { name: 'Devin', intro: 'Connect a Devin or Windsurf account.', example: 'http://127.0.0.1:…/callback?code=…, or a session token' },
  kimi: { name: 'Kimi', intro: 'Connect a Kimi Code membership.' },
  xai: { name: 'Grok', intro: 'Connect a SuperGrok or X Premium subscription.' },
  meta: { name: 'Meta', intro: 'Connect a Meta account for Muse models.' },
};
const CLIENT = { openai: 'OpenAI', responses: 'Responses', claude: 'Anthropic', gemini: 'Gemini' };

// Real provider logos live in the inline sprite (ui/logos.svg). OpenAI-compatible
// groups get their vendor's logo when the name gives it away.
const LOGOS = new Set(['claude', 'codex', 'gemini', 'vertex', 'antigravity', 'xai', 'kimi', 'meta', 'devin']);
const COMPAT_LOGOS = [
  ['openrouter', 'openrouter'], ['ollama', 'ollama'], ['lmstudio', 'lmstudio'], ['deepseek', 'deepseek'], ['groq', 'groq'],
  ['mistral', 'mistral'], ['qwen', 'qwen'], ['dashscope', 'qwen'], ['moonshot', 'kimi'], ['kimi', 'kimi'], ['grok', 'xai'],
  ['xai', 'xai'], ['gemini', 'gemini'], ['anthropic', 'claude'], ['claude', 'claude'], ['openai', 'codex'],
];

function logo(provider, group, kind) {
  let id = LOGOS.has(provider) ? provider : 'compat';
  if (provider === 'xai' && kind === 'api-key') id = 'xai-api'; // xAI console keys; Grok is the subscription
  if (provider === 'openai-compat' || provider === 'compat') {
    const g = String(group || '').toLowerCase().replace(/[^a-z]/g, '');
    id = (COMPAT_LOGOS.find(([k]) => g.includes(k)) || [, 'compat'])[1];
  }
  return `<svg class="logo logo-${id}" aria-hidden="true" focusable="false"><use href="#logo-${id}"/></svg>`;
}

const ICON = {
  copy: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5V3.5A1 1 0 0 0 9.5 2.5h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2"/></svg>',
  check: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><path d="m3.5 8.5 3 3 6-7" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  refresh: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" stroke-linecap="round"/><path d="M13.5 2.5v3h-3" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  trash: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M2.5 4.5h11M6.5 4.5v-2h3v2M4 4.5l.7 9h6.6l.7-9" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  chevron: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="m4 6 4 4 4-4" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  eye: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M1.5 8S3.9 3.5 8 3.5 14.5 8 14.5 8 12.1 12.5 8 12.5 1.5 8 1.5 8Z" stroke-linejoin="round"/><circle cx="8" cy="8" r="2"/></svg>',
  eyeOff: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M6.4 3.7A6 6 0 0 1 8 3.5c4.1 0 6.5 4.5 6.5 4.5a11.5 11.5 0 0 1-1.6 2.2M10.3 12a5.7 5.7 0 0 1-2.3.5C3.9 12.5 1.5 8 1.5 8a11.6 11.6 0 0 1 2.6-3.1M6.6 6.6a2 2 0 0 0 2.8 2.8M2.5 2.5l11 11" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  external: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M9.5 2.5h4v4M13.5 2.5 7 9M11.5 9.5v3a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1h3" stroke-linecap="round" stroke-linejoin="round"/></svg>',
};

// ---------------------------------------------------------------- helpers

const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

// Privacy: with the toggle on, emails and the visible ends of API keys are
// replaced before anything reaches the page. Copy buttons still copy the real value.
const EMAIL = /[^\s@<>()"',;:]+@[^\s@<>()"',;:]+\.[a-z]{2,}/gi;
const KEY_ENDS = /\S*…\S*/g;
const HIDDEN = '••••••••';
const hideEmails = (text) => (S.private ? String(text ?? '').replace(EMAIL, '••••••@••••••') : text);
// An account label: an email, a masked key ("sk-ant…f3e2") or a group and key.
const who = (text) => (S.private ? String(hideEmails(text) ?? '').replace(KEY_ENDS, '••••…••••') : text);
const secret = (key) => (S.private ? HIDDEN : key);
// Signed-in accounts without an email (a Devin username, a file name) are hidden whole.
const acctLabel = (a) => (S.private && a.kind !== 'api-key' && !a.label.includes('@') ? HIDDEN : who(a.label));
// Home directories name the person: /Users/maya/... reads ~/...
const home = (path) => (S.private ? String(path ?? '').replace(/^(\/Users|\/home)\/[^/]+/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+/, '~') : path);

function fmt(n) {
  n = Number(n) || 0;
  if (n < 1000) return n.toLocaleString('en-US');
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e5 ? 1 : 0).replace(/\.0$/, '') + 'k';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 2 : n < 1e8 ? 1 : 0).replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '') + 'M';
  return (n / 1e9).toFixed(2).replace(/\.?0+$/, '') + 'B';
}

function ms(v) {
  if (v == null) return '—';
  if (v < 1000) return `${v}ms`;
  if (v < 60000) return `${(v / 1000).toFixed(v < 10000 ? 1 : 0)}s`;
  return `${Math.floor(v / 60000)}m ${Math.round((v % 60000) / 1000)}s`;
}

function ago(iso) {
  if (!iso) return 'never';
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 10) return 'just now';
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function until(iso) {
  const s = Math.max(0, Math.round((Date.parse(iso) - Date.now()) / 1000));
  if (s < 3600) return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

function span(secs) {
  if (secs < 60) return `${Math.floor(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

const clock = (iso) => new Date(iso).toLocaleTimeString('en-GB', { hour12: false });

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

async function api(path, opts = {}) {
  const headers = { 'content-type': 'application/json' };
  if (S.key) headers.authorization = `Bearer ${S.key}`;
  const res = await fetch(`/api${path}`, { ...opts, headers });
  if (res.status === 401 || res.status === 403) {
    const lock = res.status === 401 ? 'key' : 'remote';
    if (S.locked !== lock) { S.locked = lock; render(); }
    throw new ApiError(res.status, 'locked');
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(res.status, data.error || res.statusText);
  return data;
}

// ---------------------------------------------------------------- data

async function loadAll() {
  const [overview, accounts, requests, models] = await Promise.all([
    api('/overview'), api('/accounts'), api('/requests'), api('/models'),
  ]);
  Object.assign(S, { overview, accounts, requests, models, locked: null });
}

let accountsTimer = 0;
function refreshAccounts() {
  clearTimeout(accountsTimer);
  accountsTimer = setTimeout(async () => {
    try {
      const [accounts, overview, models] = await Promise.all([api('/accounts'), api('/overview'), api('/models')]);
      Object.assign(S, { accounts, overview, models });
      patch('ov-accounts', ovAccountsHTML);
      patch('acct-list', accountListHTML);
      patch('acct-head', accountHeadHTML);
      patch('endpoint', endpointHTML);
      syncResetModal();
    } catch {}
  }, 250);
}

// ---------------------------------------------------------------- live

let ws = null;
let wsDelay = 1000;

function connectLive() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const q = S.key ? `?key=${encodeURIComponent(S.key)}` : '';
  ws = new WebSocket(`${proto}://${location.host}/api/live${q}`);
  ws.onopen = () => { wsDelay = 1000; setLive('live'); };
  ws.onclose = () => {
    setLive('offline');
    if (!S.locked) setTimeout(connectLive, wsDelay);
    wsDelay = Math.min(wsDelay * 2, 15000);
  };
  ws.onmessage = (e) => {
    try { onLive(JSON.parse(e.data)); } catch {}
  };
}

function setLive(state) {
  S.live = state;
  renderStatus();
}

function onLive(msg) {
  if (msg.type === 'request') return onRequest(msg.data);
  if (msg.type === 'accounts') return refreshAccounts();
  if (msg.type === 'notifications' && S.route === 'config' && S.config.section === 'notifications') return loadNotifications();
  if (msg.type === 'login') return pollLogin();
  if (msg.type === 'tick' && S.overview) {
    S.overview.totals = msg.data.totals;
    S.overview.active = msg.data.active;
    patch('figures', figuresHTML);
  }
}

function onRequest(log) {
  S.requests.unshift(log);
  if (S.requests.length > 300) S.requests.length = 300;
  const o = S.overview;
  if (o) {
    const t = o.totals;
    t.requests += 1;
    if (log.status < 400) t.ok += 1; else t.failed += 1;
    t.input_tokens += log.input_tokens;
    t.output_tokens += log.output_tokens;
    t.cache_tokens += log.cache_tokens;
    const minute = Math.floor(Date.parse(log.ts) / 60000);
    let b = o.series[o.series.length - 1];
    if (!b || b.minute !== minute) {
      o.series.push((b = { minute, requests: 0, failed: 0, tokens: 0 }));
      if (o.series.length > 60) o.series.shift();
    }
    b.requests += 1;
    if (log.status >= 400) b.failed += 1;
    b.tokens += log.input_tokens + log.output_tokens + log.cache_tokens;
  }
  if (S.route === 'overview') {
    patch('figures', figuresHTML);
    patch('bars', barsHTML);
    patch('recent', recentHTML, true);
  } else if (S.route === 'requests' && !S.paused && matches(log)) {
    const body = $('#req-body');
    if (body) {
      $('#req-empty')?.remove();
      body.insertAdjacentHTML('afterbegin', requestRowHTML(log, true));
      while (body.children.length > 300) body.lastElementChild.remove();
      patch('req-count', reqCountHTML);
    }
  }
}

// ---------------------------------------------------------------- render

function patch(id, fn, lit = false) {
  const el = document.getElementById(id);
  if (!el) return;
  const active = document.activeElement;
  const resetAct = el.contains(active) && active.dataset?.act?.startsWith('banked-') ? active.dataset.act : null;
  const resetResolution = resetAct ? active.dataset.resolution : null;
  const resetId = resetAct ? active.dataset.id : el.contains(active) ? active.dataset?.resetGrant : null;
  el.innerHTML = fn(lit);
  if (resetId) {
    const target = resetAct ? `[data-act="${CSS.escape(resetAct)}"][data-id="${CSS.escape(resetId)}"]${resetResolution ? `[data-resolution="${CSS.escape(resetResolution)}"]` : ''}` : `[data-reset-grant="${CSS.escape(resetId)}"]`;
    el.querySelector(target)?.focus({ preventScroll: true });
  }
}

// Silent while connected; only a lost connection is worth showing.
function renderStatus() {
  const el = $('#live-status');
  if (!el) return;
  el.innerHTML = S.live === 'offline'
    ? '<span class="dot" style="background:var(--err)" title="Reconnecting"></span><span class="live-word">Reconnecting</span>'
    : '';
}

function renderPrivacy() {
  const btn = $('#privacy');
  if (!btn) return;
  const label = S.private ? 'Show emails and keys' : 'Hide emails and keys';
  btn.innerHTML = S.private ? ICON.eyeOff : ICON.eye;
  btn.setAttribute('aria-pressed', String(S.private));
  btn.setAttribute('aria-label', label);
  btn.title = label;
}

function render() {
  renderPrivacy();
  for (const a of document.querySelectorAll('.tabs a')) {
    if (a.dataset.tab === S.route && !S.locked) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  renderStatus();
  if (S.locked) { closeResetModal(); view.innerHTML = lockHTML(); bindLock(); return; }
  if (!S.overview) { view.innerHTML = skeletonHTML(); return; }
  const pages = { overview: overviewHTML, accounts: accountsHTML, requests: requestsHTML, config: configHTML };
  view.innerHTML = (pages[S.route] || overviewHTML)();
  if (S.route === 'config') bindConfig();
  if (S.route === 'requests') bindRequests();
  syncResetModal();
}

function skeletonHTML() {
  return `<div class="skel-rows" aria-busy="true" aria-label="Loading">${'<div class="skel"></div>'.repeat(6)}</div>`;
}

// overview -------------------------------------------------------------

function overviewHTML() {
  return `
    <section class="endpoint" id="endpoint" aria-label="Connect a client">${endpointHTML()}</section>
    <section class="section">
      <div class="traffic-head">
        <div class="section-head" style="margin:0"><h2>Traffic</h2><span class="meta">since start</span></div>
        <dl class="figures" id="figures">${figuresHTML()}</dl>
      </div>
      <div class="bars" id="bars">${barsHTML()}</div>
      <div class="axis"><span>60 min ago</span><span>now</span></div>
    </section>
    <section class="section" id="ov-accounts">${ovAccountsHTML()}</section>
    <section class="section">
      <div class="section-head"><h2>Latest requests</h2><a class="link" href="#/requests">All requests</a></div>
      <div id="recent">${recentHTML()}</div>
    </section>`;
}

function figuresHTML() {
  const o = S.overview;
  const t = o.totals;
  const rate = t.requests ? `${((t.ok / t.requests) * 100).toFixed(t.ok === t.requests ? 0 : 1)}%` : '—';
  const items = [
    ['Requests', fmt(t.requests)],
    ['Success', rate],
    ['Tokens in', fmt(t.input_tokens)],
    ['Tokens out', fmt(t.output_tokens)],
    ['Cached', fmt(t.cache_tokens)],
    ['In flight', fmt(o.active)],
  ];
  return items.map(([k, v]) => `<div><dt>${k}</dt><dd>${v}</dd></div>`).join('');
}

function barsHTML() {
  const series = S.overview.series;
  const max = Math.max(4, ...series.map((b) => b.requests));
  const now = Math.floor(Date.now() / 60000);
  const total = series.reduce((a, b) => a + b.requests, 0);
  const bars = series.map((b) => {
    const label = `${new Date(b.minute * 60000).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' })} · ${b.requests} request${b.requests === 1 ? '' : 's'}${b.failed ? `, ${b.failed} failed` : ''} · ${fmt(b.tokens)} tokens`;
    if (!b.requests) return `<div class="b empty${b.minute === now ? ' now' : ''}" title="${esc(label)}"><i class="base"></i></div>`;
    const okH = ((b.requests - b.failed) / max) * 100;
    const fH = (b.failed / max) * 100;
    return `<div class="b${b.minute === now ? ' now' : ''}" title="${esc(label)}">${b.failed ? `<i class="f" style="height:${fH}%"></i>` : ''}<i style="height:${Math.max(okH, b.requests > b.failed ? 4 : 0)}%"></i></div>`;
  });
  return `<span class="sr-only">${total} requests in the last hour</span>${bars.join('')}`;
}

function acctStatus(a, withScope = true) {
  if (a.disabled) return { cls: 'disabled', html: 'Disabled' };
  const cds = Object.entries(a.cooldowns || {}).sort((x, y) => Date.parse(y[1]) - Date.parse(x[1]));
  if (cds.length) {
    const [model, t] = cds[0];
    const scope = model === '*' || !withScope ? '' : ` <span class="dim">${esc(model)}</span>`;
    return { cls: 'cooling', html: `Cooling <span data-until="${esc(t)}">${until(t)}</span>${scope}`, scope: model === '*' ? 'all models' : model };
  }
  if (a.last_error) return { cls: 'error', html: 'Error' };
  return { cls: 'ready', html: 'Ready' };
}

function acctSub(a) {
  const parts = [a.provider === 'openai-compat' ? (a.group || 'Compatible') : PROVIDER[a.provider]];
  if (a.kind === 'service-account') {
    parts.push('Service account');
  } else if (a.kind === 'oauth') {
    parts.push('OAuth');
    if (a.expires_at) {
      const left = (Date.parse(a.expires_at) - Date.now()) / 1000;
      parts.push(left > 0 ? `token valid ${span(left)}` : 'token expired');
    }
  } else {
    parts.push('API key');
  }
  return parts.join(' · ');
}

// Quota colors always describe capacity left, whichever percentage is displayed.
function quotaOf(w, now = Date.now()) {
  if (!w || typeof w.used !== 'number' || !Number.isFinite(w.used)) return null;
  if (w.resets_at && !(Date.parse(w.resets_at) > now)) return null;
  const used = Math.max(0, Math.min(100, w.used));
  return { used, remaining: 100 - used, cls: used >= 95 ? 'err' : used >= 75 ? 'warn' : '', exhausted: used === 100 };
}

// Whole percentages; the ends never round onto 0% or 100% unless they are.
function quotaPercent(value) {
  const rounded = Math.round(value);
  if (value > 0 && rounded === 0) return '<1%';
  if (value < 100 && rounded === 100) return '>99%';
  return `${rounded}%`;
}

// "used" or "left", for headings and labels.
const quotaWord = () => (S.quotaDisplay === 'used' ? 'used' : 'left');

function quotaView(q) {
  const mode = S.quotaDisplay;
  const value = q[mode];
  const text = `${quotaPercent(value)} ${quotaWord()}`;
  const status = q.exhausted ? 'Exhausted' : q.cls === 'err' ? 'Almost exhausted' : q.cls === 'warn' ? 'Low quota' : 'Healthy';
  return { mode, value, text, status };
}

function quotaControlsHTML() {
  return `<div class="quota-controls"><span class="meta">Quota</span>
    <div class="seg" role="group" aria-label="Quota display" title="Display preference saved in this browser">
      ${['used', 'remaining'].map((mode) => `<button type="button" data-act="quota-display" data-id="${mode}" aria-pressed="${S.quotaDisplay === mode}">${mode === 'used' ? 'Used' : 'Remaining'}</button>`).join('')}
    </div></div>`;
}

function setQuotaDisplay(mode, persist = true) {
  S.quotaDisplay = mode === 'remaining' ? 'remaining' : 'used';
  if (persist) {
    try { localStorage.setItem('cliproxyapi-rust.quota-display', S.quotaDisplay); } catch {}
  }
  patch('ov-accounts', ovAccountsHTML);
  patch('acct-list', accountListHTML);
}

// Subscriptions that can report limits get meters (a dash until they do); API keys have none.
const metered = (a) => (a.kind === 'oauth' && ['claude', 'codex'].includes(a.provider))
  || (a.quota?.windows || []).some((w) => !w.model);

// Subscription usage windows (Claude 5h / week, ChatGPT), tightest first.
function limitsHTML(a, max = 2) {
  const now = Date.now();
  const ws = ((a.quota && a.quota.windows) || [])
    .filter((w) => !w.model && quotaOf(w, now))
    .sort((x, y) => y.used - x.used)
    .slice(0, max);
  if (!ws.length) return metered(a) ? '<span class="limits none" title="Quota not reported"><span aria-label="Quota not reported">–</span></span>' : '';
  return `<span class="limits">${ws.map((w) => {
    const q = quotaOf(w, now);
    const v = quotaView(q);
    const resets = w.resets_at ? `, resets in ${until(w.resets_at)}` : '';
    return `<span class="limit ${q.cls}${q.exhausted ? ' exhausted' : ''}"${w.resets_at ? ` data-quota-reset="${esc(w.resets_at)}"` : ''} title="${esc(w.name)}: ${esc(v.text)} · ${v.status}${resets}">
      <span>${esc(w.name)}</span><span class="track" role="meter" aria-label="${esc(w.name)} quota ${v.mode}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${v.value}" aria-valuetext="${esc(v.text)} · ${v.status}"><i style="width:${v.value}%"></i></span>
      <span class="pct">${esc(v.text)}</span>${q.exhausted ? '<span class="quota-exhausted">Exhausted</span>' : ''}</span>`;
  }).join('')}</span>`;
}

function statusHTML(a, withScope = true) {
  const st = acctStatus(a, withScope);
  return `<span class="status ${st.cls}"><span class="dot"></span><span>${st.html}</span></span>`;
}

// The tightest live usage window of a kind: short (5-hour) or long (weekly).
function windowOf(a, short) {
  const now = Date.now();
  return ((a.quota && a.quota.windows) || [])
    .filter((w) => !w.model && quotaOf(w, now))
    .filter((w) => /^\d+h$/.test(w.name) === short)
    .sort((x, y) => y.used - x.used)[0];
}

const hasLimits = (a) => !!(windowOf(a, true) || windowOf(a, false));

function meterHTML(w, label) {
  const q = quotaOf(w);
  if (!q) return `<span class="meter none"><span class="m-lab">${label}</span><span aria-label="${label} quota not reported" title="Quota not reported">–</span></span>`;
  const v = quotaView(q);
  const reset = w.resets_at
    ? `<span class="reset">${q.exhausted ? '<span class="quota-exhausted">Exhausted</span> · ' : ''}Resets in <span data-until="${esc(w.resets_at)}">${until(w.resets_at)}</span></span>`
    : q.exhausted ? '<span class="reset"><span class="quota-exhausted">Exhausted</span></span>' : '';
  return `<div class="meter ${q.cls}${q.exhausted ? ' exhausted' : ''}"${w.resets_at ? ` data-quota-reset="${esc(w.resets_at)}"` : ''}>
    <div class="m-top"><span class="m-lab">${label} ${quotaWord()}</span><span class="track" role="meter" aria-label="${label} quota ${v.mode}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${v.value}" aria-valuetext="${esc(v.text)} · ${v.status}" title="${v.status}"><i style="width:${v.value}%"></i></span>
      <span class="pct">${esc(quotaPercent(v.value))}</span></div>${reset}
  </div>`;
}

const ROUTING = {
  'smart-quota': 'New sessions balance weekly resets, 5-hour quota and account load',
  'least-used': 'New sessions use the account with the most quota left',
  'round-robin': 'New sessions take turns across accounts',
  'fill-first': 'New sessions use the first available account',
};

function ovAccountsHTML() {
  const list = S.accounts || [];
  const routing = (ROUTING[S.overview.routing] || '').replace('New sessions', S.overview.session_affinity === false ? 'Requests' : 'New sessions');
  const head = `<div class="section-head quota-head"><div class="head-l"><h2>Accounts</h2>${list.length ? `<span class="meta hide-sm">${routing}</span>` : ''}</div>
    <div class="quota-actions">${quotaControlsHTML()}<a class="link" href="#/accounts">Manage</a></div></div>`;
  if (!list.length) {
    return `${head}<div class="empty list">
      <h3>No accounts connected</h3>
      <p>Sign in with a subscription or add an API key. From a terminal you can also run <code>cliproxyapi-rust login claude</code>.</p>
      <div class="actions">
        <button class="btn" data-act="start-login" data-provider="claude">${logo('claude')}Sign in with Claude</button>
        <button class="btn" data-act="start-login" data-provider="codex">${logo('codex')}Sign in with ChatGPT</button>
        <button class="btn" data-act="open-panel" data-panel="connect">Other accounts</button>
        <button class="btn" data-act="open-panel" data-panel="key">Add API key</button>
      </div></div>`;
  }
  // Subscriptions that report their limits lead; the rest follow in pool order.
  const sorted = [...list.filter(hasLimits), ...list.filter((a) => !hasLimits(a))];
  const shown = sorted.slice(0, 10);
  const limits = list.some(metered);
  const name = accountNameHTML;
  const req = (a) => `<span class="num"><b>${fmt(a.counters.requests)}</b> req</span>`;
  const more = list.length > shown.length ? `<p class="note"><a class="link" href="#/accounts">${list.length - shown.length} more</a></p>` : '';
  if (!limits) {
    const rows = shown.map((a) => `<div class="row acct-row">${name(a)}${statusHTML(a, false)}<span class="hide-sm">${req(a)}</span></div>`).join('');
    return `${head}<div class="list">${rows}</div>${more}`;
  }
  const rows = shown.map((a) => {
    return `<div class="row lim-row">
      ${name(a)}
      <div class="lim-5h">${metered(a) ? meterHTML(windowOf(a, true), '5h') : ''}</div>
      <div class="lim-wk">${metered(a) ? meterHTML(windowOf(a, false), 'Week') : ''}</div>
      <div class="lim-status">${statusHTML(a, false)}</div>
      <div class="lim-req hide-md">${req(a)}</div>
    </div>`;
  }).join('');
  return `${head}<div class="lim-table">
    <div class="row lim-row lim-head" aria-hidden="true"><span>Account</span><span>5-hour limit <span class="dim">· ${quotaWord()}</span></span><span>Weekly limit <span class="dim">· ${quotaWord()}</span></span><span>Status</span><span class="hide-md r">Requests</span></div>
    ${rows}
  </div>${more}`;
}

function snippet(kind) {
  const origin = location.origin;
  const key = S.overview.client_keys[0];
  const token = key || 'cliproxyapi-rust';
  const shown = key ? secret(key) : token; // what the page shows; copy gets the real token
  const pick = (prefix, fallback) => (S.models.find((m) => m.id.startsWith(prefix)) || {}).id || fallback;
  const any = (S.models[0] || {}).id || 'claude-sonnet-5-5';
  const k = (s) => `<span class="k">${esc(s)}</span>`;
  const v = (s) => `<span class="v">${esc(s)}</span>`;
  switch (kind) {
    case 'codex':
      return {
        text: `# ~/.codex/config.toml\nmodel = "${pick('gpt-', 'gpt-6-astra')}"\nmodel_provider = "cliproxyapi-rust"\n\n[model_providers.cliproxyapi-rust]\nname = "CLIProxyAPI-Rust"\nbase_url = "${origin}/v1"\nwire_api = "responses"${key ? '\nenv_key = "CLIPROXYAPI_RUST_KEY"' : ''}`,
        html: `${k('# ~/.codex/config.toml')}\nmodel = ${v(`"${pick('gpt-', 'gpt-6-astra')}"`)}\nmodel_provider = ${v('"cliproxyapi-rust"')}\n\n[model_providers.cliproxyapi-rust]\nname = ${v('"CLIProxyAPI-Rust"')}\nbase_url = ${v(`"${origin}/v1"`)}\nwire_api = ${v('"responses"')}${key ? `\nenv_key = ${v('"CLIPROXYAPI_RUST_KEY"')}` : ''}`,
        note: `${key ? 'Then export CLIPROXYAPI_RUST_KEY with your key. ' : ''}Both HTTP and websocket transports work, and any model your accounts serve can be used.`,
      };
    case 'sdk':
      return {
        text: `from openai import OpenAI\n\nclient = OpenAI(base_url="${origin}/v1", api_key="${token}")\nreply = client.chat.completions.create(\n    model="${any}",\n    messages=[{"role": "user", "content": "Hello"}],\n)`,
        html: `from openai import OpenAI\n\nclient = OpenAI(base_url=${v(`"${origin}/v1"`)}, api_key=${v(`"${shown}"`)})\nreply = client.chat.completions.create(\n    model=${v(`"${any}"`)},\n    messages=[{"role": "user", "content": "Hello"}],\n)`,
        note: 'Any model works with any client format; CLIProxyAPI-Rust translates between OpenAI, Anthropic and Gemini.',
      };
    case 'curl':
      return {
        text: `curl ${origin}/v1/chat/completions \\\n  -H "Authorization: Bearer ${token}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model": "${any}", "messages": [{"role": "user", "content": "Hello"}]}'`,
        html: `curl ${v(`${origin}/v1/chat/completions`)} \\\n  -H ${v(`"Authorization: Bearer ${shown}"`)} \\\n  -H "Content-Type: application/json" \\\n  -d '{"model": ${v(`"${any}"`)}, "messages": [{"role": "user", "content": "Hello"}]}'`,
        note: 'Also available: /v1/messages, /v1/responses (HTTP and websocket) and /v1beta/models.',
      };
    default:
      return {
        text: `export ANTHROPIC_BASE_URL=${origin}\nexport ANTHROPIC_AUTH_TOKEN=${token}\nclaude`,
        html: `export ANTHROPIC_BASE_URL=${v(origin)}\nexport ANTHROPIC_AUTH_TOKEN=${v(shown)}\nclaude`,
        note: 'Claude Code requests pass through untouched. Set ANTHROPIC_MODEL to use a GPT or Gemini model instead.',
      };
  }
}

function setupOpen() {
  if (S.setup) return S.setup === 'open';
  // Until the first request arrives, show how to connect.
  return !S.overview.totals.requests;
}

function endpointHTML() {
  const o = S.overview;
  const key = o.client_keys[0];
  const open = setupOpen();
  const copyBtn = (text, label) => `<button class="btn ghost small" data-act="copy" data-text="${esc(text)}" aria-label="${label}" title="${label}">${ICON.copy}</button>`;
  return `<div class="ep-row">
      <div class="ep-item"><span class="ep-label">Endpoint</span><span class="ep-val mono" title="${esc(location.origin)}">${esc(location.origin)}</span>${copyBtn(location.origin, 'Copy endpoint')}</div>
      <div class="ep-item">${key
        ? `<span class="ep-label">Key</span><span class="ep-val mono">${esc(secret(key))}</span>${copyBtn(key, 'Copy API key')}`
        : `<span class="ep-label">Key</span><span class="ep-val">None required</span>`}</div>
      <div class="ep-item hide-sm"><span class="ep-label">Models</span><span class="ep-val">${o.models}</span></div>
      <button class="btn ghost small ep-toggle" data-act="toggle-setup" aria-expanded="${open}" aria-controls="ep-setup">Set up a client${ICON.chevron}</button>
    </div>${open ? `<div class="ep-setup" id="ep-setup">${setupHTML()}</div>` : ''}`;
}

function setupHTML() {
  const tabs = [['claude', 'Claude Code'], ['codex', 'Codex'], ['sdk', 'OpenAI SDK'], ['curl', 'curl']];
  const sn = snippet(S.snippet);
  return `<div class="snip-head">
      <div class="seg" role="group" aria-label="Client">${tabs.map(([id, label]) => `<button data-act="snippet" data-id="${id}" aria-pressed="${S.snippet === id}">${label}</button>`).join('')}</div>
      <button class="btn ghost small" data-act="copy" data-text="${esc(sn.text)}" aria-label="Copy snippet">${ICON.copy}<span>Copy</span></button>
    </div>
    <pre class="code">${sn.html}</pre>
    <p class="note">${esc(sn.note)}</p>`;
}

const accountOf = (r) => (S.accounts || []).find((a) => a.provider === r.provider && a.label === r.account);

function routeHTML(r, tags = false) {
  const acct = accountOf(r);
  const provider = acct && acct.group ? acct.group : PROVIDER[r.provider] || (r.provider ? r.provider : '—');
  const kind = { ws: 'ws', images: 'image', video: 'video' }[r.transport];
  const extra = tags ? [kind, r.attempts > 1 ? `${r.attempts} tries` : null].filter(Boolean) : [];
  return `<span class="route"><span>${esc(CLIENT[r.client] || r.client)}</span><span class="arrow">→</span>${r.provider ? logo(r.provider, acct ? acct.group : r.account, acct && acct.kind) : ''}<span>${esc(provider)}</span>${extra.map((t) => `<span class="tag">${t}</span>`).join('')}</span>`;
}

function codeClass(s) {
  if (s === 499) return 'code-499';
  if (s >= 500) return 'code-5xx';
  if (s >= 400) return 'code-4xx';
  return 'code-200';
}

const ROUTING_LABEL = { 'least-used': 'Least-used', 'smart-quota': 'Smart quota balancing', 'round-robin': 'Round-robin', 'fill-first': 'Fill-first' };
const ROUTING_REASON = {
  new_session: 'New session',
  session_reused: 'Same session',
  quota_exhausted: 'Moved: quota used up',
  account_disabled: 'Moved: account disabled',
  account_removed: 'Moved: account removed',
  model_unavailable: 'Moved: model not served',
  temporary_detour: 'Detour: account busy',
  missing_session: 'No session assignment',
  affinity_disabled: 'Affinity disabled',
  retry_same: 'Retried same account',
};
const ROUTING_WARNING = {
  missing_session_id: ['No session ID', 'The client supplied no stable session identifier. Later requests may use another account and lose cache reuse.'],
  connection_only: ['Connection only', 'This assignment lasts for the WebSocket connection. A reconnect without a stable session identifier may use another account.'],
  response_id_only: ['Response ID only', 'This assignment relies on a previous response ID held in memory. A stable session identifier is needed to preserve it across server restarts.'],
  affinity_disabled: ['Affinity off', 'Session affinity is disabled. Requests from this session may use different accounts.'],
};
const SESSION_SOURCE = {
  previous_response_id: 'a previous response ID',
  websocket_connection: 'this WebSocket connection',
  generated_response: 'a generated response ID',
  prompt_cache_key: 'the prompt cache key',
};

function routingReason(reason) {
  return ROUTING_REASON[reason] || (reason || '').replaceAll('_', ' ');
}

function requestAccountHTML(r) {
  const reason = routingReason(r.routing_reason);
  const strategy = ROUTING_LABEL[r.routing_strategy] || r.routing_strategy;
  // Attempts name accounts by id; show their (privacy-aware) labels instead.
  const named = (id) => { const a = (S.accounts || []).find((x) => x.id === id); return a ? acctLabel(a) : 'another account'; };
  const attempts = (r.routing_attempts || []).map((a) => {
    const from = a.previous_account ? `${named(a.previous_account)} → ` : '';
    return `${from}${named(a.account_id) || 'Unknown account'}: ${routingReason(a.reason)}`;
  });
  const detail = [strategy && `Routing: ${strategy}`, ...attempts].filter(Boolean).join('\n');
  // One line under the account: why it was chosen, then the session it belongs to.
  const moved = /^(quota_exhausted|account_|model_unavailable|temporary_detour)/.test(r.routing_reason);
  const why = reason ? `<span class="${moved ? 'warn' : ''}" title="${esc(detail)}">${esc(reason)}</span>` : '';
  const session = requestSessionHTML(r);
  const label = (accountOf(r) ? acctLabel(accountOf(r)) : who(r.account)) || '—';
  return `<span class="request-account" title="${esc(label)}">${esc(label)}</span>${why || session ? `<span class="request-detail">${[why, session].filter(Boolean).join(' · ')}</span>` : ''}`;
}

function requestSessionHTML(r) {
  const warning = ROUTING_WARNING[r.routing_warning];
  const source = SESSION_SOURCE[r.session_source] || (r.session_source ? `the client’s ${r.session_source}` : 'the client');
  const title = `Session fingerprint: ${r.session_id}\nIdentified by ${source}. Click to show this session’s requests.`;
  const session = r.session_id
    ? `<button class="linkbtn mono session-link" data-act="filter-session" data-id="${esc(r.session_id)}" title="${esc(title)}" aria-label="Show requests for session ${esc(r.session_id.slice(0, 8))}">${esc(r.session_id.slice(0, 8))}</button>`
    : '';
  return [session, warning ? `<span class="warn" title="${esc(warning[1])}">${esc(warning[0])}</span>` : ''].filter(Boolean).join(' · ');
}

function requestTokensHTML(r, field) {
  if (r.status === 499 && !r.input_tokens && !r.output_tokens && !r.cache_tokens) {
    return '<span class="dim" title="No token usage was reported before this request closed. Usage is unknown.">—</span>';
  }
  return `<span title="${Number(r[field] || 0).toLocaleString('en-US')} tokens">${fmt(r[field])}</span>`;
}

function requestRowHTML(r, lit = false, full = true) {
  const status = r.status === 499 ? 'closed' : r.status;
  const error = hideEmails(r.error);
  const err = r.error && r.status >= 400 && r.status !== 499 ? `<span class="errline" title="${esc(error)}">${esc(error)}</span>` : '';
  return `<tr class="${lit ? 'lit' : ''}">
    <td class="mono" title="${esc(r.ts)}">${clock(r.ts)}</td>
    <td>${routeHTML(r, full)}</td>
    <td><span class="model mono">${esc(r.model)}</span>${err}</td>
    <td>${requestAccountHTML(r)}</td>
    <td class="mono ${codeClass(r.status)}">${status}</td>
    ${full ? `<td class="r mono hide-sm">${ms(r.ttft_ms)}</td>` : ''}
    <td class="r mono">${ms(r.latency_ms)}</td>
    <td class="r mono">${requestTokensHTML(r, 'input_tokens')}</td>
    <td class="r mono">${requestTokensHTML(r, 'output_tokens')}</td>
    <td class="r mono">${requestTokensHTML(r, 'cache_tokens')}</td>
  </tr>`;
}

function recentHTML(lit = false) {
  const rows = S.requests.slice(0, 8);
  if (!rows.length) {
    return `<div class="empty"><h3>No requests yet</h3><p>Point a client at the endpoint above and requests will show up here as they happen.</p></div>`;
  }
  return `<div class="table-wrap"><table>
    <thead><tr><th>Time</th><th>Route</th><th>Model</th><th>Account</th><th>Status</th><th class="r">Latency</th><th class="r">In</th><th class="r">Out</th><th class="r">Cached</th></tr></thead>
    <tbody>${rows.map((r, i) => requestRowHTML(r, lit && i === 0, false)).join('')}</tbody></table></div>`;
}

// accounts --------------------------------------------------------------

function accountsHTML() {
  return `
    <div id="acct-head">${accountHeadHTML()}</div>
    <div id="acct-panel">${panelHTML()}</div>
    <div id="acct-list">${accountListHTML()}</div>`;
}

function accountHeadHTML() {
  const n = (S.accounts || []).length;
  const connecting = S.panel === 'connect' || !!LOGIN[S.panel] || S.panel === 'vertex';
  return `<div class="page-head">
    <div><h1>Accounts</h1><p>${n ? `${n} connected · stored in <span class="mono">${esc(home(S.overview.auth_dir))}</span> and config.yaml` : 'Nothing connected yet'}</p></div>
    <div class="actions">
      <button class="btn" data-act="open-panel" data-panel="connect" aria-expanded="${connecting}">Connect account</button>
      <button class="btn" data-act="open-panel" data-panel="key" aria-expanded="${S.panel === 'key'}">Add API key</button>
    </div></div>`;
}

function panelHTML() {
  if (S.panel === 'key') return keyPanelHTML();
  if (S.panel === 'connect') return connectPanelHTML();
  if (S.panel === 'vertex') return vertexPanelHTML();
  if (S.panel === 'vertex-done') return doneHTML('Vertex AI', S.login);
  if (LOGIN[S.panel]) return S.login && S.login.kind === 'device' ? devicePanelHTML() : loginPanelHTML();
  return '';
}

function connectPanelHTML() {
  return `<div class="panel" role="region" aria-label="Connect an account">
    <h3>Connect an account</h3>
    <p>Sign in with a subscription. Credentials are stored in the auth directory on this machine.</p>
    <div class="choices">${SIGNIN.map(([id, name, sub]) => `
      <button class="choice" data-act="start-login" data-provider="${id}">
        ${logo(id)}<span class="who"><span class="label">${name}</span><span class="sub">${sub}</span></span>
      </button>`).join('')}
    </div>
    <div class="actions" style="margin-top:16px"><button class="btn ghost" data-act="close-panel">Cancel</button></div>
  </div>`;
}

function loginStatus(L, port) {
  if (!L || L.status === 'starting') return `<span class="wait"><span class="pulse"></span>Opening the sign-in page…</span>`;
  if (L.status === 'done') return `<span class="ok">Connected ${esc(who(L.message) || '')}</span>`;
  if (L.status === 'error') return `<span class="err">${esc(hideEmails(L.message) || 'Sign-in failed')}</span>`;
  if (L.kind === 'device') return `<span class="wait"><span class="pulse"></span>Waiting for you to approve…</span>`;
  if (L.callback) return `<span class="wait"><span class="pulse"></span>Waiting for you to approve in the browser…</span>`;
  return `<span class="warn">This server can't receive the redirect${port ? ` (port ${port} is busy)` : ''}. Paste the URL below.</span>`;
}

function doneHTML(name, L) {
  return `<div class="panel" role="region" aria-label="Sign in with ${name}">
    <h3>Signed in</h3><p>${esc(who(L.message) || '')} is ready to serve requests.</p>
    <div class="actions"><button class="btn" data-act="close-panel">Done</button></div></div>`;
}

const reopen = (L, text) => L && L.url ? ` <a class="link" href="${esc(L.url)}" target="_blank" rel="noopener">${text} ${ICON.external.replace('<svg', '<svg style="width:12px;height:12px;vertical-align:-1px"')}</a>` : '';

function loginPanelHTML() {
  const L = S.login;
  const info = LOGIN[S.panel];
  if (L && L.status === 'done') return doneHTML(info.name, L);
  return `<div class="panel" role="region" aria-label="Sign in with ${info.name}">
    <h3>Sign in with ${info.name}</h3>
    <p>${info.intro} Credentials stay on this machine.</p>
    <ol class="steps">
      <li><span class="n">1</span><div class="t"><b>Approve access</b> in the tab that opened.${reopen(L, 'Open sign-in page again')}</div></li>
      <li><span class="n">2</span><div class="t" aria-live="polite">${loginStatus(L, info.port)}</div></li>
    </ol>
    <div class="divider"></div>
    <form class="field" data-form="paste">
      <label for="paste-url"><span class="dim" style="font-size:12.5px;font-weight:500">Signed in from another device? Paste the address the browser was sent to</span></label>
      <div class="inline">
        <input id="paste-url" class="mono" type="text" name="input" placeholder="${esc(info.example)}" autocomplete="off" spellcheck="false" ${L && L.state ? '' : 'disabled'}>
        <button class="btn" type="submit" ${L && L.state ? '' : 'disabled'}>Connect</button>
      </div>
      <small>After you approve, that localhost page won't load when the browser runs elsewhere. Copy its full address from the address bar.</small>
      ${L && L.error ? `<p class="msg err" role="alert">${esc(L.error)}</p>` : ''}
    </form>
    <div class="actions" style="margin-top:18px"><button class="btn ghost" data-act="close-panel">Cancel</button></div>
  </div>`;
}

function devicePanelHTML() {
  const L = S.login;
  const info = LOGIN[S.panel];
  if (L.status === 'done') return doneHTML(info.name, L);
  let host = '';
  try { host = new URL(L.url).host; } catch {}
  return `<div class="panel" role="region" aria-label="Sign in with ${info.name}">
    <h3>Sign in with ${info.name}</h3>
    <p>${info.intro} Credentials stay on this machine.</p>
    <ol class="steps">
      <li><span class="n">1</span><div class="t"><b>Open ${esc(host || 'the sign-in page')}</b> in the tab that opened.${reopen(L, 'Open it again')}</div></li>
      <li><span class="n">2</span><div class="t"><b>Check the code matches</b> <span class="code mono">${esc(L.user_code || '')}</span> <button class="linkbtn" data-act="copy" data-text="${esc(L.user_code || '')}">Copy</button></div></li>
      <li><span class="n">3</span><div class="t" aria-live="polite">${loginStatus(L)}</div></li>
    </ol>
    <div class="actions" style="margin-top:18px"><button class="btn ghost" data-act="close-panel">Cancel</button></div>
  </div>`;
}

function vertexPanelHTML() {
  return `<form class="panel" data-form="vertex" aria-label="Add a Vertex AI service account">
    <h3>Add a Vertex AI service account</h3>
    <p>Paste a Google Cloud service account key (JSON) with the Vertex AI User role. It is saved to the auth directory.</p>
    <div class="grid">
      <label class="field wide"><span>Service account key</span><textarea class="mono" name="json" rows="6" spellcheck="false" placeholder='{ "type": "service_account", "project_id": "…", "private_key": "…", "client_email": "…" }' required></textarea></label>
      <label class="field"><span>Region</span><input class="mono" type="text" name="location" placeholder="us-central1" autocomplete="off" spellcheck="false"><small>Use global for the newest models.</small></label>
    </div>
    <div class="actions"><button class="btn primary" type="submit">Add service account</button><button class="btn ghost" type="button" data-act="close-panel">Cancel</button></div>
    <p class="msg" id="vertex-msg" aria-live="polite"></p>
  </form>`;
}

function keyPanelHTML() {
  const p = S.keyProvider;
  const opts = [['claude', 'Claude'], ['codex', 'OpenAI'], ['gemini', 'Gemini'], ['vertex', 'Vertex AI'], ['kimi', 'Kimi'], ['xai', 'xAI'], ['meta', 'Meta'], ['compat', 'OpenAI-compatible']];
  const base = {
    claude: 'https://api.anthropic.com', codex: 'https://api.openai.com/v1', gemini: 'https://generativelanguage.googleapis.com',
    vertex: 'https://aiplatform.googleapis.com', kimi: 'https://api.kimi.com/coding', xai: 'https://api.x.ai/v1',
    meta: 'https://api.meta.ai/v1', compat: 'https://openrouter.ai/api/v1',
  }[p];
  const compat = p === 'compat';
  return `<form class="panel" data-form="key" aria-label="Add an API key">
    <h3>Add an API key</h3>
    <p>Keys are saved to <span class="mono">config.yaml</span> and used alongside your signed-in accounts.</p>
    <div class="seg" role="group" aria-label="Provider">${opts.map(([id, label]) => `<button type="button" data-act="key-provider" data-id="${id}" aria-pressed="${p === id}">${logo(id, null, 'api-key')}${label}</button>`).join('')}</div>
    <div class="grid">
      <label class="field wide"><span>API key${compat ? ' (optional for local servers)' : ''}</span><input class="mono" type="password" name="api_key" autocomplete="off" spellcheck="false" ${compat ? '' : 'required'}></label>
      <label class="field ${compat ? '' : 'wide'}"><span>Base URL${compat ? '' : ' (optional)'}</span><input class="mono" type="url" name="base_url" placeholder="${esc(base)}" ${compat ? 'required' : ''}></label>
      ${compat ? `<label class="field"><span>Name</span><input type="text" name="name" placeholder="openrouter"></label>
      <label class="field wide"><span>Models</span><input class="mono" type="text" name="models" placeholder="moonshotai/kimi-k3, kimi=moonshotai/kimi-k3" required><small>Comma separated. Write alias=upstream-name to expose a model under a shorter name.</small></label>` : ''}
    </div>
    <div class="actions"><button class="btn primary" type="submit">Add key</button><button class="btn ghost" type="button" data-act="close-panel">Cancel</button></div>
    <p class="msg" id="key-msg" aria-live="polite"></p>
  </form>`;
}

function accountListHTML() {
  const list = S.accounts || [];
  if (!list.length) {
    return `<div class="empty" style="border-top:1px solid var(--line)"><h3>No accounts yet</h3>
      <p>Connect a subscription or add an API key above, or run <code>cliproxyapi-rust login &lt;provider&gt;</code> on the server. Existing CLIProxyAPI credentials in the auth directory are picked up automatically.</p></div>`;
  }
  const head = `<div class="account-quota-controls">${quotaControlsHTML()}</div>
    <div class="row acct-columns" style="min-height:36px;color:var(--fg-3);font-size:12px;font-weight:500"><span>Account</span><span>Status / quota</span><span class="hide-md">Requests / tokens</span><span class="hide-md">Last used</span><span></span></div>`;
  const rows = list.map((a) => {
    const confirming = S.confirm === a.id;
    const cooling = Object.keys(a.cooldowns || {}).length > 0;
    const actions = confirming
      ? `<button class="btn small danger" data-act="delete" data-id="${esc(a.id)}" aria-label="Confirm removing ${esc(acctLabel(a))}">Remove</button><button class="btn ghost small" data-act="cancel-delete">Keep</button>`
      : `${a.kind === 'oauth' ? `<button class="btn ghost small" data-act="refresh" data-id="${esc(a.id)}" aria-label="Refresh token for ${esc(acctLabel(a))}" title="Refresh token">${ICON.refresh}</button>` : ''}
         <button class="switch" role="switch" aria-checked="${!a.disabled}" aria-label="${a.disabled ? 'Enable' : 'Disable'} ${esc(acctLabel(a))}" title="${a.disabled ? 'Disabled' : 'Enabled'}" data-act="toggle" data-id="${esc(a.id)}"></button>
         <button class="btn ghost small" data-act="confirm-delete" data-id="${esc(a.id)}" aria-label="Remove ${esc(acctLabel(a))}" title="Remove">${ICON.trash}</button>`;
    const c = a.counters;
    return `<div class="row">
      ${accountNameHTML(a)}
      <div class="stack">${statusHTML(a, false)}${cooling ? `<span class="sub">${esc(acctStatus(a).scope)} · <button class="linkbtn" data-act="reset" data-id="${esc(a.id)}" title="Clear local cooldowns; refresh quota to verify provider limits">Clear cooldowns</button></span>` : ''}<span class="sub quota-sub">${limitsHTML(a)}</span></div>
      <div class="stack hide-md"><span class="main"><b>${fmt(c.requests)}</b> ${c.requests === 1 ? 'request' : 'requests'}</span><span class="sub">${fmt(c.input_tokens)} in · ${fmt(c.output_tokens)} out${c.failures ? ` · <span class="err">${fmt(c.failures)} failed</span>` : ''}</span></div>
      <span class="num hide-md" style="text-align:left" data-ago="${esc(a.last_used || '')}">${ago(a.last_used)}</span>
      <div class="row-actions">${actions}</div>
      ${a.last_error ? `<div class="acct-err">${esc(hideEmails(a.last_error))}</div>` : ''}
    </div>`;
  }).join('');
  return `<div class="acct-table list">${head}${rows}</div>`;
}

// banked resets ----------------------------------------------------------

// Opt-in (banked-resets in config.yaml): it relies on unofficial provider endpoints.
function hasBankedResets(a) { return !!S.overview?.banked_resets && a.kind === 'oauth' && ['claude', 'codex'].includes(a.provider); }
function accountNameHTML(a) {
  return `<div class="acct-name">${logo(a.provider, a.group, a.kind)}<span class="who"><span class="acct-title"><span class="label" title="${esc(acctLabel(a))}">${esc(acctLabel(a))}</span>${bankedSummaryHTML(a)}</span><span class="sub">${esc(acctSub(a))}</span></span></div>`;
}
function bankedLabel(a) {
  const r = a.banked_resets;
  if (['pending', 'unknown'].includes(r?.operation?.status)) return 'Reset needs review';
  const count = !r?.error ? r?.inventory?.available : null;
  return count == null ? 'Resets unavailable' : `${count} reset${count === 1 ? '' : 's'} available`;
}
function bankedSummaryHTML(a) {
  if (!hasBankedResets(a)) return '';
  const r = a.banked_resets, review = ['pending', 'unknown'].includes(r?.operation?.status);
  const count = !r?.error ? r?.inventory?.available : null;
  // Only resets you have, or one that needs a decision, earn a badge.
  if (!review && !count) return '';
  const label = review ? 'Review reset' : count == null ? 'Resets unavailable' : `${count} reset${count === 1 ? '' : 's'}`;
  const now = Date.now();
  const expiring = !review && !r?.error && (r?.inventory?.grants || []).some((g) =>
    g.remaining > 0 && Date.parse(g.expires_at) > now && Date.parse(g.expires_at) <= now + 24 * 60 * 60 * 1000);
  const hint = expiring ? ' A reset expires within 24 hours.' : '';
  return `<button type="button" class="reset-badge${review ? ' warn' : ''}" data-act="banked-details" data-id="${esc(a.id)}" aria-haspopup="dialog" aria-controls="banked-reset-modal" aria-label="${esc(bankedLabel(a))} for ${esc(acctLabel(a))}.${hint}" title="View saved resets and expiry dates.${hint}">${ICON.refresh}<span>${esc(label)}</span>${expiring ? '<span class="reset-expiry-dot" aria-hidden="true"></span>' : ''}</button>`;
}
function resetButton(a, act, label, disabled = false, extra = '') {
  return `<button type="button" class="btn small ${['banked-open', 'banked-confirm'].includes(act) ? 'primary' : 'ghost'}" data-act="${act}" data-id="${esc(a.id)}" ${disabled ? 'disabled' : ''} ${extra}>${label}</button>`;
}
function resetDate(value) {
  return value ? new Date(value).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' }) : 'No expiry reported';
}
function grantDetail(g) {
  return `${g.expires_at ? `Expires ${resetDate(g.expires_at)}` : 'No expiry reported'} · Clears ${g.clears.join(', ') || 'provider limits'}`;
}
function bankedResetsHTML(a) {
  const local = S.resets[a.id] || {}, r = a.banked_resets, inv = r?.inventory, op = r?.operation, d = local.dialog;
  const uncertain = ['pending', 'unknown'].includes(op?.status);
  const stale = !r || !(Date.parse(r.checked_at) > Date.now() - 5 * 60 * 1000);
  const expiryChanged = inv?.grants?.some((g) => g.usable && ((g.expires_at && Date.parse(g.expires_at) <= Date.now()) || (g.starts_at && Date.parse(g.starts_at) > Date.now())));
  const usable = !local.busy && !a.disabled && !r?.error && !stale && !expiryChanged && inv?.eligible && r?.quote && !uncertain;
  let title = 'Banked resets', content, controls;
  if (d) {
    const confirmedInventory = d.inventory || inv;
    const grant = confirmedInventory?.grants?.find((g) => g.id === d.grant);
    if (d.action === 'redeem') {
      title = 'Use 1 reset?';
      const choices = (confirmedInventory?.grants || []).filter((g) => g.usable);
      const selection = a.provider === 'claude' && choices.length > 1
        ? `<label class="reset-selection">Grant<select data-reset-grant="${esc(a.id)}" ${local.busy ? 'disabled' : ''}>${choices.map((g) => `<option value="${esc(g.id)}" ${g.id === d.grant ? 'selected' : ''}>${esc(g.label)} · ${g.remaining} left</option>`).join('')}</select></label>`
        : grant ? `<p class="reset-grant-name">${esc(grant.label)}</p>` : '';
      content = `<p>This uses one saved reset.</p>${a.provider === 'codex' ? '<p class="sub">Codex chooses the reset and restores its subscription limits.</p>' : `${selection}${grant ? `<p class="sub">${esc(grantDetail(grant))}</p>` : ''}`}`;
      controls = `${resetButton(a, 'banked-cancel', 'Back', local.busy)}${resetButton(a, 'banked-confirm', local.busy ? 'Applying…' : 'Apply reset', local.busy || a.disabled)}`;
    } else if (d.action === 'retry') {
      title = 'Retry reset request?';
      content = '<p>A reset may already have been used. This retries the saved request.</p>';
      controls = `${resetButton(a, 'banked-cancel', 'Back', local.busy)}${resetButton(a, 'banked-confirm', local.busy ? 'Retrying…' : 'Confirm retry', local.busy || a.disabled)}`;
    } else {
      title = 'Resolve reset outcome';
      content = '<p>Check your provider account first, then record the result.</p>';
      controls = `${resetButton(a, 'banked-cancel', 'Back', local.busy)}${resetButton(a, 'banked-confirm', 'Reset was used', local.busy, 'data-resolution="resolve-used"')}${resetButton(a, 'banked-confirm', 'No reset was used', local.busy, 'data-resolution="resolve-unused"')}`;
    }
  } else {
    const grants = (inv?.grants || []).map((g) => `<li><div class="reset-grant-head"><b>${esc(g.label || 'Subscription reset')}</b><span>${g.remaining} left</span></div><p class="sub">${esc(grantDetail(g))}</p>${g.reason ? `<p class="sub">${esc(g.reason)}</p>` : ''}</li>`).join('');
    const reason = a.disabled ? 'Enable this account to apply a reset.' : stale || expiryChanged ? 'Refresh to check availability.' : inv?.reason;
    const message = uncertain ? '<p class="warn">A reset may have been used. New resets are blocked until resolved.</p>' : op ? `<p class="${['applied', 'reconciled_used'].includes(op.status) ? 'ok' : 'sub'}" role="status">${esc(op.message)}</p>` : '';
    content = `<p class="reset-count">${esc(bankedLabel(a))}${a.provider === 'claude' && inv?.applicable != null && inv.applicable !== inv.available && !r?.error ? `<span class="sub"> · ${inv.applicable} usable now</span>` : ''}</p>${grants ? `<ul class="reset-grants">${grants}</ul>` : ''}${reason && !r?.error ? `<p class="sub">${esc(reason)}</p>` : ''}${message}`;
    controls = `${resetButton(a, 'banked-refresh', local.busy ? 'Checking…' : 'Refresh', local.busy)}${resetButton(a, 'banked-open', 'Use 1 reset', !usable, r?.checked_at ? `data-reset-deadline="${esc(new Date(Date.parse(r.checked_at) + 5 * 60 * 1000).toISOString())}"` : '')}`;
    if (uncertain) {
      const retryable = a.provider === 'claude' && r.retryable && Date.parse(op.retry_until) > Date.now();
      controls = `${resetButton(a, 'banked-refresh', local.busy ? 'Checking…' : 'Refresh', local.busy)}${retryable ? resetButton(a, 'banked-retry', 'Retry request', local.busy || a.disabled, `data-reset-deadline="${esc(op.retry_until)}"`) : ''}${resetButton(a, 'banked-resolve', 'Check outcome', local.busy)}`;
    }
  }
  return `<header class="reset-modal-head"><div><h2 id="reset-modal-title">${title}</h2><p id="reset-modal-account">${esc(acctLabel(a))} · ${esc(PROVIDER[a.provider])}</p></div><button type="button" class="btn ghost small reset-close" data-act="banked-close" data-id="${esc(a.id)}" aria-label="Close reset details">×</button></header>
    <div class="reset-modal-body" aria-busy="${!!local.busy}">${content}${local.error || r?.error ? `<p class="err" role="alert">${esc(local.error || r.error)}</p>` : ''}</div>
    <footer class="reset-modal-footer">${!d && r ? `<span class="sub">Checked <span data-ago="${esc(r.checked_at)}">${ago(r.checked_at)}</span></span>` : ''}<div class="actions">${controls}</div></footer>`;
}
function syncResetModal() {
  if (!S.resetModal) return;
  const a = (S.accounts || []).find((a) => a.id === S.resetModal);
  if (!a || S.locked) { closeResetModal(); return; }
  patch('banked-reset-modal', () => bankedResetsHTML(a));
  const modal = $('#banked-reset-modal');
  if (!modal.open) modal.showModal();
}
function closeResetModal() {
  const id = S.resetModal;
  if (!id) return;
  S.resetModal = null;
  if (S.resets[id]) S.resets[id].dialog = null;
  $('#banked-reset-modal').close();
  document.querySelector(`[data-act="banked-details"][data-id="${CSS.escape(id)}"]`)?.focus({ preventScroll: true });
}
function patchResets() {
  patch('acct-list', accountListHTML);
  patch('ov-accounts', ovAccountsHTML);
  syncResetModal();
}
async function bankedAction(act, id, resolution) {
  let a = (S.accounts || []).find((x) => x.id === id);
  if (!a) return;
  const local = S.resets[id] ||= {};
  if (act === 'banked-details') {
    S.resetModal = id;
    local.dialog = null;
    syncResetModal();
    if (local.busy) return;
  }
  if (local.busy) return;
  if (act === 'banked-cancel') { local.dialog = null; syncResetModal(); $('#banked-reset-modal [data-act="banked-close"]')?.focus(); return; }
  if (act === 'banked-resolve' || act === 'banked-retry') {
    local.dialog = { action: act === 'banked-retry' ? 'retry' : 'resolve', request: a.banked_resets?.operation?.request_id };
    syncResetModal(); $('#banked-reset-modal [data-act="banked-cancel"]')?.focus(); return;
  }
  local.busy = true; local.error = null;
  patchResets();
  try {
    const path = `/accounts/${encodeURIComponent(id)}/banked-resets`;
    if (['banked-details', 'banked-refresh', 'banked-open'].includes(act)) {
      const r = await api(act === 'banked-refresh' ? `/accounts/${encodeURIComponent(id)}/quota/refresh` : path, act === 'banked-refresh' ? { method: 'POST' } : {});
      a = (S.accounts || []).find((x) => x.id === id) || a;
      a.banked_resets = r;
      if (act === 'banked-open' && S.resetModal === id && r.quote && r.inventory?.eligible && !r.error) {
        local.dialog = { action: 'redeem', request: r.quote, grant: r.inventory.selected_grant || '', inventory: r.inventory };
      }
    } else if (act === 'banked-confirm' && local.dialog) {
      const d = local.dialog;
      const result = await api(path, { method: 'POST', body: JSON.stringify({ action: d.action === 'resolve' ? resolution : d.action, request_id: d.request, grant_id: d.grant || '', confirmed: true }) });
      a = (S.accounts || []).find((x) => x.id === id) || a;
      a.banked_resets = result;
      local.dialog = null;
    }
  } catch (e) {
    local.error = e.message;
    if (act === 'banked-confirm') {
      local.dialog = null;
      a = (S.accounts || []).find((x) => x.id === id) || a;
      if (a.banked_resets) a.banked_resets.quote = null;
      local.error += ' Refresh status before continuing.';
    }
  } finally {
    local.busy = false;
    patchResets();
    refreshAccounts();
    if (S.resetModal === id) $('#banked-reset-modal [data-act="banked-cancel"], #banked-reset-modal [data-act="banked-close"]')?.focus();
  }
}
document.addEventListener('change', (e) => {
  if (e.target.matches('[data-reset-grant]')) {
    const local = S.resets[e.target.dataset.resetGrant];
    if (local?.dialog) { local.dialog.grant = e.target.value; syncResetModal(); }
  }
});
const resetModal = $('#banked-reset-modal');
resetModal.addEventListener('cancel', (e) => { e.preventDefault(); closeResetModal(); });
resetModal.addEventListener('keydown', (e) => {
  if (e.key !== 'Tab') return;
  const targets = [...resetModal.querySelectorAll('button:not([disabled]), select:not([disabled])')];
  const first = targets[0], last = targets.at(-1);
  if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last?.focus(); }
  else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first?.focus(); }
});
resetModal.addEventListener('click', (e) => {
  const box = resetModal.getBoundingClientRect();
  if (e.target === resetModal && (e.clientX < box.left || e.clientX > box.right || e.clientY < box.top || e.clientY > box.bottom)) closeResetModal();
});

// requests --------------------------------------------------------------

function matches(r) {
  const q = S.filter.trim().toLowerCase();
  if (!q) return true;
  const attempts = (r.routing_attempts || []).flatMap((a) => [a.account, a.previous_account, a.reason, routingReason(a.reason)]);
  return [r.model, r.account, r.provider, r.client, String(r.status), r.status === 499 ? 'closed' : '', r.error,
    r.session_id, r.session_source, r.routing_strategy, r.routing_reason, routingReason(r.routing_reason),
    r.routing_warning, ...(ROUTING_WARNING[r.routing_warning] || []), ...attempts,
  ].some((s) => s != null && String(s).toLowerCase().includes(q));
}

function reqCountHTML() {
  const n = S.requests.filter(matches).length;
  return `${fmt(n)} shown`;
}

function requestsHTML() {
  const rows = S.requests.filter(matches);
  return `
    <div class="page-head">
      <div><h1>Requests</h1><p>The last 300 requests since the server started.</p></div>
      <div class="toolbar">
        <label class="sr-only" for="req-filter">Filter requests</label>
        <input id="req-filter" type="text" placeholder="Filter by session, account, model…" value="${esc(S.filter)}" autocomplete="off" spellcheck="false">
        <button class="btn" data-act="pause" aria-pressed="${S.paused}">${S.paused ? 'Resume' : 'Pause'}</button>
      </div>
    </div>
    <p class="note" style="margin:-8px 0 12px" id="req-count">${reqCountHTML()}</p>
    <div class="table-wrap"><table>
      <thead><tr><th>Time</th><th>Route</th><th>Model</th><th>Account</th><th>Status</th><th class="r hide-sm">First token</th><th class="r">Total</th><th class="r">In</th><th class="r">Out</th><th class="r">Cached</th></tr></thead>
      <tbody id="req-body">${rows.map((r) => requestRowHTML(r)).join('')}</tbody>
    </table></div>
    ${rows.length ? '' : requestEmptyHTML()}`;
}

function requestEmptyHTML() {
  return `<div class="empty" id="req-empty"><h3>${S.filter ? 'Nothing matches that filter' : 'No requests yet'}</h3><p>${S.filter ? 'Try a session ID, an account, a model or a routing reason.' : 'Requests appear here the moment a client sends one.'}</p></div>`;
}

function bindRequests() {
  const input = $('#req-filter');
  input?.addEventListener('input', () => {
    S.filter = input.value;
    const rows = S.requests.filter(matches);
    $('#req-body').innerHTML = rows.map((r) => requestRowHTML(r)).join('');
    $('#req-empty')?.remove();
    if (!rows.length) $('#req-body').closest('.table-wrap').insertAdjacentHTML('afterend', requestEmptyHTML());
    patch('req-count', reqCountHTML);
  });
}

// lock ------------------------------------------------------------------

function lockHTML() {
  if (S.locked === 'remote') {
    return `<div class="lock"><h1>Dashboard is local-only</h1>
      <p>Without a management key the dashboard only answers on localhost. Set <span class="mono">management-key</span> in config.yaml on the server, then reload this page.</p></div>`;
  }
  return `<div class="lock"><h1>Dashboard locked</h1>
    <p>Enter the <span class="mono">management-key</span> from config.yaml.</p>
    <form data-form="unlock"><label class="sr-only" for="mk">Management key</label>
      <input id="mk" class="mono" type="password" name="key" autocomplete="current-password" required autofocus>
      <button class="btn primary" type="submit">Unlock</button>
      <p class="msg err" id="lock-msg" aria-live="polite"></p></form></div>`;
}

function bindLock() {
  $('#mk')?.focus();
}

// ---------------------------------------------------------------- actions

async function startLogin(provider) {
  S.panel = provider;
  if (provider === 'vertex') {
    S.login = null;
    if (S.route !== 'accounts') location.hash = '#/accounts';
    else { patch('acct-panel', panelHTML); patch('acct-head', accountHeadHTML); }
    $('textarea[name="json"]')?.focus();
    return;
  }
  S.login = { provider, status: 'starting' };
  if (S.route !== 'accounts') location.hash = '#/accounts';
  else { patch('acct-panel', panelHTML); patch('acct-head', accountHeadHTML); }
  // Open the tab synchronously so popup blockers allow it.
  const tab = window.open('about:blank', '_blank');
  try {
    const r = await api(`/login/${provider}`, { method: 'POST' });
    S.login = { provider, state: r.state, url: r.url, callback: r.callback, kind: r.kind, user_code: r.user_code, status: 'pending' };
    if (tab) { tab.opener = null; tab.location.href = r.url; }
  } catch (e) {
    tab?.close();
    S.login = { provider, status: 'error', message: e.message };
  }
  patch('acct-panel', panelHTML);
  schedulePoll();
}

let pollTimer = 0;
function schedulePoll() {
  clearTimeout(pollTimer);
  if (S.login && S.login.status === 'pending') pollTimer = setTimeout(pollLogin, 1500);
}

async function pollLogin() {
  const L = S.login;
  if (!L || !L.state || L.status !== 'pending') return;
  try {
    const r = await api(`/login/${encodeURIComponent(L.state)}`);
    if (r.status !== L.status) {
      Object.assign(L, { status: r.status, message: r.message });
      const input = $('#paste-url');
      const keep = input ? input.value : '';
      patch('acct-panel', panelHTML);
      if ($('#paste-url') && keep) $('#paste-url').value = keep;
      if (r.status === 'done') refreshAccounts();
    }
  } catch {}
  schedulePoll();
}

async function submitPaste(form) {
  const L = S.login;
  const input = form.elements.input.value.trim();
  if (!input || !L) return;
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  btn.textContent = 'Connecting…';
  try {
    const r = await api(`/login/${encodeURIComponent(L.state)}/code`, { method: 'POST', body: JSON.stringify({ input }) });
    Object.assign(L, { status: 'done', message: r.label });
    refreshAccounts();
  } catch (e) {
    L.error = e.message;
  }
  patch('acct-panel', panelHTML);
  schedulePoll();
}

async function submitVertex(form) {
  const data = Object.fromEntries(new FormData(form).entries());
  const msg = $('#vertex-msg');
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  btn.textContent = 'Checking…';
  try {
    const r = await api('/vertex', { method: 'POST', body: JSON.stringify(data) });
    S.panel = 'vertex-done';
    S.login = { provider: 'vertex', status: 'done', message: r.label };
    patch('acct-panel', panelHTML);
    patch('acct-head', accountHeadHTML);
    refreshAccounts();
  } catch (e) {
    msg.className = 'msg err';
    msg.textContent = e.message;
    btn.disabled = false;
    btn.textContent = 'Add service account';
  }
}

async function submitKey(form) {
  const data = Object.fromEntries(new FormData(form).entries());
  data.provider = S.keyProvider;
  const msg = $('#key-msg');
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await api('/keys', { method: 'POST', body: JSON.stringify(data) });
    S.panel = null;
    patch('acct-panel', panelHTML);
    patch('acct-head', accountHeadHTML);
    refreshAccounts();
  } catch (e) {
    msg.className = 'msg err';
    msg.textContent = e.message;
    btn.disabled = false;
  }
}

async function accountAction(act, id) {
  const a = (S.accounts || []).find((x) => x.id === id);
  try {
    if (act === 'toggle') {
      a.disabled = !a.disabled;
      patch('acct-list', accountListHTML);
      await api(`/accounts/${encodeURIComponent(id)}/toggle`, { method: 'POST', body: JSON.stringify({ disabled: a.disabled }) });
    } else if (act === 'refresh') {
      const btn = document.querySelector(`[data-act="refresh"][data-id="${CSS.escape(id)}"]`);
      if (btn) { btn.disabled = true; btn.style.opacity = 1; btn.firstElementChild.style.animation = 'pulse 1s infinite'; }
      await api(`/accounts/${encodeURIComponent(id)}/refresh`, { method: 'POST' });
    } else if (act === 'reset') {
      await api(`/accounts/${encodeURIComponent(id)}/reset`, { method: 'POST' });
    } else if (act === 'delete') {
      S.confirm = null;
      await api(`/accounts/${encodeURIComponent(id)}`, { method: 'DELETE' });
    }
  } catch (e) {
    if (a) a.last_error = e.message;
  }
  refreshAccounts();
}

async function copy(btn) {
  const text = btn.dataset.text;
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement('textarea');
    ta.value = text;
    document.body.append(ta);
    ta.select();
    document.execCommand('copy');
    ta.remove();
  }
  const prev = btn.innerHTML;
  if (btn.classList.contains('linkbtn')) btn.textContent = 'Copied';
  else btn.innerHTML = btn.querySelector('span') ? `${ICON.check}<span>Copied</span>` : ICON.check;
  btn.classList.add('copied');
  setTimeout(() => { btn.innerHTML = prev; btn.classList.remove('copied'); }, 1400);
}

document.addEventListener('click', (e) => {
  const el = e.target.closest('[data-act]');
  if (!el) return;
  const { act, id } = el.dataset;
  switch (act) {
    case 'copy': return copy(el);
    case 'privacy': {
      S.private = !S.private;
      localStorage.setItem('cliproxyapi-rust.private', S.private ? '1' : '0');
      const paste = $('#paste-url')?.value;
      render();
      if (paste && $('#paste-url')) $('#paste-url').value = paste;
      return $('#privacy')?.focus();
    }
    case 'reveal-config':
      S.config.reveal = true;
      render();
      return $('#cfg-yaml')?.focus();
    case 'quota-display':
      setQuotaDisplay(id);
      return $(`[data-act="quota-display"][data-id="${S.quotaDisplay}"]`)?.focus();
    case 'filter-session':
      S.filter = id;
      if (S.route !== 'requests') { location.hash = '#/requests'; return; }
      render();
      return $('#req-filter')?.focus();
    case 'snippet':
      S.snippet = id;
      localStorage.setItem('cliproxyapi-rust.snippet', id);
      return patch('endpoint', endpointHTML);
    case 'toggle-setup':
      S.setup = setupOpen() ? 'closed' : 'open';
      localStorage.setItem('cliproxyapi-rust.setup', S.setup);
      patch('endpoint', endpointHTML);
      return $('[data-act="toggle-setup"]')?.focus();
    case 'start-login':
      if (S.panel === el.dataset.provider && S.login && S.login.status === 'pending') return;
      return startLogin(el.dataset.provider);
    case 'open-panel':
      S.panel = el.dataset.panel;
      if (S.route !== 'accounts') { location.hash = '#/accounts'; return; }
      patch('acct-panel', panelHTML);
      patch('acct-head', accountHeadHTML);
      return $('#acct-panel input')?.focus();
    case 'close-panel':
      S.panel = null;
      S.login = null;
      clearTimeout(pollTimer);
      patch('acct-panel', panelHTML);
      return patch('acct-head', accountHeadHTML);
    case 'key-provider':
      S.keyProvider = id;
      patch('acct-panel', panelHTML);
      return $(`[data-act="key-provider"][data-id="${id}"]`)?.focus();
    case 'confirm-delete':
      S.confirm = id;
      patch('acct-list', accountListHTML);
      return $('[data-act="cancel-delete"]')?.focus();
    case 'cancel-delete':
      S.confirm = null;
      return patch('acct-list', accountListHTML);
    case 'banked-close': return closeResetModal();
    case 'banked-details': case 'banked-refresh': case 'banked-open': case 'banked-confirm': case 'banked-cancel': case 'banked-retry': case 'banked-resolve':
      return bankedAction(act, id, el.dataset.resolution);
    case 'toggle': case 'refresh': case 'reset': case 'delete':
      return accountAction(act, id);
    case 'pause':
      S.paused = !S.paused;
      return render();
    case 'save-config': return saveConfig();
    case 'revert-config': return discardConfig();
  }
});

document.addEventListener('submit', async (e) => {
  const form = e.target.closest('form[data-form]');
  if (!form) return;
  e.preventDefault();
  const kind = form.dataset.form;
  if (kind === 'paste') return submitPaste(form);
  if (kind === 'key') return submitKey(form);
  if (kind === 'vertex') return submitVertex(form);
  if (kind === 'unlock') {
    S.key = form.elements.key.value.trim();
    try {
      await api('/overview');
      localStorage.setItem('cliproxyapi-rust.key', S.key);
      S.locked = null;
      await boot();
    } catch {
      const m = $('#lock-msg');
      if (m) m.textContent = 'That key was not accepted.';
    }
  }
});

// ---------------------------------------------------------------- routing + timers

function onRoute() {
  closeResetModal();
  const r = (location.hash.replace(/^#\/?/, '') || 'overview').split('/')[0];
  S.route = ['overview', 'accounts', 'requests', 'config'].includes(r) ? r : 'overview';
  if (S.route !== 'config') Object.assign(S.config, { msg: null, reveal: false });
  render();
  if (S.route === 'accounts' && S.panel === 'key') $('#acct-panel input')?.focus();
  view.focus({ preventScroll: true });
  window.scrollTo(0, 0);
}

setInterval(() => {
  let expired = false;
  for (const el of document.querySelectorAll('[data-until]')) {
    if (Date.parse(el.dataset.until) <= Date.now()) expired = true;
    el.textContent = until(el.dataset.until);
  }
  for (const el of document.querySelectorAll('[data-reset-deadline]')) {
    if (Date.parse(el.dataset.resetDeadline) <= Date.now()) el.disabled = true;
  }
  for (const el of document.querySelectorAll('[data-ago]')) el.textContent = ago(el.dataset.ago);
  if ([...document.querySelectorAll('[data-quota-reset]')].some((el) => Date.parse(el.dataset.quotaReset) <= Date.now())) {
    patch('ov-accounts', ovAccountsHTML);
    patch('acct-list', accountListHTML);
    syncResetModal();
    expired = true;
  }
  if (expired) refreshAccounts();
}, 1000);

// Refresh notification activity only while its settings section is visible.
setInterval(() => {
  if (!document.hidden && !S.locked && S.route === 'config' && S.config.section === 'notifications') loadNotifications();
}, 5000);

// Resync the hour of traffic once a minute (rolls the window forward).
setInterval(async () => {
  if (S.locked || !S.overview) return;
  try {
    S.overview = await api('/overview');
    if (S.route === 'overview') { patch('figures', figuresHTML); patch('bars', barsHTML); }
  } catch {}
}, 60000);

window.addEventListener('beforeunload', (e) => {
  if (configDirty() || rawDirty()) e.preventDefault();
});

window.addEventListener('storage', (e) => {
  if (e.key === 'cliproxyapi-rust.quota-display' || e.key === null) setQuotaDisplay(e.newValue, false);
});

async function boot() {
  render();
  try {
    await loadAll();
  } catch (e) {
    if (!(e instanceof ApiError && (e.status === 401 || e.status === 403))) {
      view.innerHTML = `<div class="lock"><h1>Can't reach CLIProxyAPI-Rust</h1><p>${esc(e.message)}. Check that the server is running, then reload.</p></div>`;
    }
    return;
  }
  render();
  if (!ws || ws.readyState > 1) connectLive();
}

window.addEventListener('hashchange', onRoute);
const initial = (location.hash.replace(/^#\/?/, '') || 'overview').split('/')[0];
S.route = ['overview', 'accounts', 'requests', 'config'].includes(initial) ? initial : 'overview';
boot();
