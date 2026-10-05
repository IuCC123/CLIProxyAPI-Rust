'use strict';

const CONFIG_SECTIONS = [
  ['server', 'Server'], ['access', 'Access'], ['routing', 'Routing'], ['connections', 'Connections'],
  ['providers', 'Providers'], ['models', 'Models'], ['diagnostics', 'Diagnostics'], ['yaml', 'YAML file'],
];
const CONFIG_PROVIDERS = [
  ['claude', 'Claude', 'claude-api-key'], ['codex', 'OpenAI / Codex', 'codex-api-key'],
  ['gemini', 'Gemini', 'gemini-api-key'], ['vertex', 'Vertex AI', 'vertex-api-key'],
  ['kimi', 'Kimi', 'kimi-api-key'], ['xai', 'Grok / xAI', 'xai-api-key'], ['meta', 'Meta', 'meta-api-key'],
  ['compat', 'OpenAI-compatible', 'openai-compatibility'],
];
const configClone = (value) => JSON.parse(JSON.stringify(value));
const configEqual = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const configPath = (path) => esc(JSON.stringify(path));
const configId = (path) => `setting-${path.map((part) => encodeURIComponent(part)).join('-')}`;
const configGet = (path, root = S.config.values) => path.reduce((v, part) => v?.[part], root);

function configSet(path, value) {
  let parent = S.config.values;
  for (let i = 0; i < path.length - 1; i++) {
    const key = path[i];
    if (!Object.hasOwn(parent, key) || parent[key] == null) {
      Object.defineProperty(parent, key, { value: typeof path[i + 1] === 'number' ? [] : {}, writable: true, enumerable: true, configurable: true });
    }
    parent = parent[key];
  }
  Object.defineProperty(parent, path[path.length - 1], { value, writable: true, enumerable: true, configurable: true });
}

function configChanges() {
  const c = S.config;
  if (!c.values || !c.saved) return {};
  return Object.fromEntries(Object.entries(c.values).filter(([key, value]) => !configEqual(value, c.saved[key])));
}

function configDirty() { return Object.keys(configChanges()).length > 0; }

function configHelp(path, text) {
  return text ? `<small id="${configId(path)}-help">${esc(text)}</small>` : '';
}

function configField(path, label, opts = {}) {
  const value = configGet(path);
  const id = configId(path);
  const error = S.config.errors[id];
  const secret = opts.type === 'password';
  const editing = S.config.secrets[JSON.stringify(path)];
  const shown = secret && value && !editing ? '' : value ?? '';
  const placeholder = secret && value && !editing ? 'Saved key · leave unchanged' : opts.placeholder || '';
  const input = `<input id="${id}" data-cfg="${configPath(path)}" ${opts.nullable ? 'data-nullable="true"' : ''}
    type="${opts.type || 'text'}" value="${esc(shown)}" placeholder="${esc(placeholder)}" ${secret ? 'data-secret="true"' : ''}
    ${opts.mono !== false ? 'class="mono" spellcheck="false"' : ''}
    ${opts.required && !(secret && value) ? 'required' : ''} ${opts.min != null ? `min="${opts.min}"` : ''}
    ${opts.max != null ? `max="${opts.max}"` : ''} ${opts.type === 'number' ? 'step="1"' : ''}
    ${secret ? 'autocomplete="new-password"' : 'autocomplete="off"'}
    aria-describedby="${opts.help ? `${id}-help ` : ''}${id}-error" ${error ? 'aria-invalid="true"' : ''}>`;
  return `<div class="field ${opts.wide ? 'wide' : ''}"><label for="${id}">${esc(label)}${opts.restart ? '<span class="cfg-badge">Needs restart</span>' : ''}</label>
    ${secret ? `<div class="cfg-secret">${input}<button type="button" class="btn ghost small" data-config-act="reveal" data-path="${configPath(path)}" aria-label="Show ${esc(label)}">Show</button>
      ${value ? `<button type="button" class="btn ghost small" data-config-act="clear-secret" data-path="${configPath(path)}" aria-label="Clear ${esc(label)}">Clear</button>` : ''}</div>` : input}
    ${configHelp(path, opts.help)}<small id="${id}-error" class="cfg-error" data-error-for="${id}">${esc(error || '')}</small></div>`;
}

function configSelect(path, label, options, opts = {}) {
  const id = configId(path);
  const value = configGet(path);
  return `<div class="field"><label for="${id}">${esc(label)}</label>
    <select id="${id}" data-cfg="${configPath(path)}" ${opts.help ? `aria-describedby="${id}-help"` : ''}>
      ${options.map(([v, text]) => `<option value="${esc(v)}" ${String(value) === String(v) ? 'selected' : ''}>${esc(text)}</option>`).join('')}
    </select>${configHelp(path, opts.help)}<small class="cfg-error" data-error-for="${id}"></small></div>`;
}

function configSwitch(path, label, help = '', restart = false) {
  const id = configId(path);
  return `<div class="cfg-toggle"><div><label id="${id}-label" for="${id}">${esc(label)}${restart ? '<span class="cfg-badge">Needs restart</span>' : ''}</label>${configHelp(path, help)}</div>
    <button id="${id}" type="button" class="switch" role="switch" aria-checked="${!!configGet(path)}" aria-labelledby="${id}-label"
      ${help ? `aria-describedby="${id}-help"` : ''} data-config-act="switch" data-path="${configPath(path)}"></button></div>`;
}

function configRemove(path, label = 'Remove row') {
  return `<button type="button" class="btn ghost small danger" data-config-act="remove" data-path="${configPath(path)}" aria-label="${esc(label)}">Remove</button>`;
}

function configStrings(path, label, help = '', secret = false) {
  const list = configGet(path) || [];
  return `<div class="cfg-list"><div class="cfg-list-head"><h3>${esc(label)}</h3>
    <button type="button" class="btn small" data-config-act="add-string" data-path="${configPath(path)}">Add ${secret ? 'key' : 'pattern'}</button></div>
    ${help ? `<p class="cfg-description">${esc(help)}</p>` : ''}
    ${list.length ? list.map((_, i) => `<div class="cfg-string-row">${configField([...path, i], `${secret ? 'API key' : 'Model pattern'} ${i + 1}`, { type: secret ? 'password' : 'text', required: true, placeholder: secret ? 'Enter an API key' : 'claude-*' })}${configRemove([...path, i], `Remove ${secret ? 'API key' : 'pattern'} ${i + 1}`)}</div>`).join('')
      : `<p class="cfg-empty">${secret ? 'No keys configured.' : 'No models excluded.'}</p>`}</div>`;
}

function configAliases(path, oauth = false) {
  const list = configGet(path) || [];
  return `<div class="cfg-list"><div class="cfg-list-head"><h3>${oauth ? 'Model aliases' : 'Allowed models and aliases'}</h3>
    <button type="button" class="btn small" data-config-act="add-alias" data-path="${configPath(path)}" ${oauth ? 'data-oauth="true"' : ''}>Add model</button></div>
    <p class="cfg-description">${oauth ? 'Expose an upstream model under a name your clients use.' : 'Leave empty to allow all available models. An alias changes the name clients use.'}</p>
    ${list.length ? list.map((_, i) => `<div class="cfg-alias-row">${configField([...path, i, 'name'], 'Upstream model', { required: true, placeholder: 'claude-sonnet-4-6' })}
      ${configField([...path, i, 'alias'], 'Alias', { required: oauth, nullable: !oauth, placeholder: oauth ? 'sonnet' : 'Same as upstream' })}
      ${oauth ? `<div class="cfg-fork">${configSwitch([...path, i, 'fork'], 'Keep original name', 'Serve both the original name and the alias.')}</div>` : ''}
      ${configRemove([...path, i], `Remove model ${i + 1}`)}</div>`).join('') : '<p class="cfg-empty">No model aliases configured.</p>'}</div>`;
}

function configHeaders(path) {
  const headers = configGet(path) || {};
  return `<div class="cfg-list"><div class="cfg-list-head"><h3>Extra HTTP headers</h3>
    <button type="button" class="btn small" data-config-act="add-header" data-path="${configPath(path)}">Add header</button></div>
    <p class="cfg-description">Sent with requests to this provider.</p>
    ${Object.entries(headers).map(([key], i) => `<div class="cfg-header-row"><div class="field"><label for="${configId([...path, i, 'key'])}">Header name</label>
      <input id="${configId([...path, i, 'key'])}" class="mono" type="text" value="${esc(key)}" data-header-path="${configPath(path)}" data-header-key="${esc(key)}" data-header-index="${i}" required autocomplete="off" spellcheck="false">
      <small class="cfg-error" data-header-error="true">${esc(S.config.errors[configId([...path, i, 'key'])] || '')}</small></div>
      ${configField([...path, key], 'Value')}
      <button type="button" class="btn ghost small danger" data-config-act="remove-header" data-path="${configPath(path)}" data-key="${esc(key)}" aria-label="Remove ${esc(key)} header">Remove</button></div>`).join('') || '<p class="cfg-empty">No extra headers configured.</p>'}</div>`;
}

function configServerHTML() {
  return `<h2>Server</h2><div class="cfg-grid">
    ${configField(['host'], 'Bind address', { restart: true, placeholder: '127.0.0.1' })}
    ${configField(['port'], 'Port', { type: 'number', min: 1, max: 65535, required: true, restart: true })}
    ${configField(['auth-dir'], 'Credentials directory', { required: true, wide: true, help: 'OAuth credential files are read from this directory.' })}
    </div><div class="cfg-divider"></div>
    ${configSwitch(['tls', 'enable'], 'HTTPS', '', true)}
    <div class="cfg-grid cfg-dependent ${configGet(['tls', 'enable']) ? '' : 'is-disabled'}" data-tls-fields>
      ${configField(['tls', 'cert'], 'Certificate path', { required: !!configGet(['tls', 'enable']) })}
      ${configField(['tls', 'key'], 'Private key path', { required: !!configGet(['tls', 'enable']) })}
    </div>`;
}

function configAccessHTML() {
  return `<h2>Access</h2>${configStrings(['api-keys'], 'Client API keys', 'Clients send one of these keys to use the proxy. Leave empty to allow access without a key.', true)}
    <div class="cfg-divider"></div><div class="cfg-grid">${configField(['management-key'], 'Dashboard key', { type: 'password', wide: true,
      help: 'Protects the dashboard and management API. With no key, access is limited to localhost.' })}
    ${configSelect(['management-allow-remote'], 'Remote dashboard access', [['null', 'Default (allow with a dashboard key)'], ['true', 'Allow'], ['false', 'Localhost only']], {
      help: 'Remote access requires a dashboard key, even when allowed.' })}</div>`;
}

function configRoutingHTML() {
  const strategy = configGet(['routing']);
  const what = configGet(['session-affinity']) ? 'new sessions' : 'requests';
  const help = { 'least-used': `Send ${what} to the account with the most subscription quota remaining. Falls back to round robin when quota is unavailable.`,
    'smart-quota': `Send ${what} to the earliest weekly reset. Below the reserves, prefer other accounts while sessions are active; idle accounts can use their remaining quota. Resets within an hour compete on quota and load.`,
    'round-robin': `Rotate ${what} across available accounts.`, 'fill-first': `Send ${what} to the first available account until it cannot serve them, then the next.` }[strategy];
  return `<h2>Routing</h2><div class="cfg-grid">
    ${configSelect(['routing'], 'Account selection', [['least-used', 'Most quota remaining'], ['smart-quota', 'Smart quota balancing'], ['round-robin', 'Round robin'], ['fill-first', 'Fill first']], { help })}
    ${strategy === 'smart-quota' ? configField(['five-hour-reserve-percent'], '5-hour reserve for existing sessions (%)', { type: 'number', min: 0, max: 100, required: true,
      help: `Below this level (or 5% weekly quota left), prefer other accounts for ${what} while this account has an in-flight request or a session used in the last five minutes. Idle accounts can admit a session to drain remaining quota. If every account needs its reserve, usable quota remains available. Default: 30%. Set 0 to turn off the 5-hour reserve.` }) : ''}
    ${configField(['request-retry'], 'Account attempts', { type: 'number', min: 0, max: 4294967295, required: true,
      help: 'Maximum accounts to try before a request fails. Zero still tries one account.' })}</div>
    <div class="cfg-divider"></div>${configSwitch(['session-affinity'], 'Keep sessions on one account', 'A coding session stays on the account it started on, so its prompt cache keeps working. It moves when that subscription runs out or the account is disabled; while an account is busy, its requests briefly use another.')}
    <div class="cfg-grid cfg-dependent ${configGet(['session-affinity']) ? '' : 'is-disabled'}">
      ${configField(['session-affinity-idle-seconds'], 'Forget idle sessions after (seconds)', { type: 'number', min: 60, required: true, help: '86400 is one day.' })}
    </div>
    <div class="cfg-divider"></div>${configSwitch(['force-model-prefix'], 'Require model prefixes', 'Unprefixed requests only use accounts without a prefix. Use prefix/model to select a prefixed account.')}`;
}

function configConnectionsHTML() {
  return `<h2>Connections</h2><div class="cfg-grid">${configField(['proxy-url'], 'Upstream proxy', { wide: true,
    placeholder: 'socks5://127.0.0.1:1080', help: 'Default proxy for upstream requests. Supports HTTP, HTTPS and SOCKS5; individual providers can override it.' })}</div>
    <div class="cfg-divider"></div>${configSwitch(['codex-websockets'], 'Native Codex websockets', 'Keep a native upstream websocket connection for clients using Codex over websockets.')}
    ${configSwitch(['claude-cloak'], 'Claude Code compatibility', 'Make requests through Claude OAuth accounts resemble Claude Code requests for other clients.')}
    ${configSwitch(['banked-resets'], 'Banked resets', 'Show saved Claude and ChatGPT limit resets beside each subscription and let you spend them. Checks every 30 minutes through unofficial provider endpoints.')}`;
}

// Provider ids here are config groups; the logo sprite knows them as account providers.
const configLogo = (provider, name) => logo(provider === 'compat' ? 'openai-compat' : provider, name, 'api-key');

function configProvidersHTML() {
  const c = S.config;
  const [provider, label, field] = CONFIG_PROVIDERS.find(([p]) => p === c.provider);
  const entries = configGet([field]) || [];
  const compatible = provider === 'compat';
  return `<div class="cfg-section-head"><h2>Providers</h2><button type="button" class="btn" data-config-act="add-provider">Add ${compatible ? 'provider' : 'key'}</button></div>
    <div class="cfg-provider-tabs" role="group" aria-label="Provider">${CONFIG_PROVIDERS.map(([p, name, f]) => `<button type="button" class="btn ghost small" data-config-act="provider" data-provider="${p}" aria-pressed="${p === provider}">${configLogo(p)}${esc(name)}<span class="cfg-count">${configGet([f])?.length || 0}</span></button>`).join('')}</div>
    ${entries.length ? entries.map((entry, i) => {
      const path = [field, i];
      const openKey = `${field}:${i}`;
      return `<details class="cfg-provider" data-cfg-open="${openKey}" ${c.opens[openKey] ? 'open' : ''}>
        <summary>${configLogo(provider, entry.name)}<span>${esc(entry.label || entry.name || `${label} key ${i + 1}`)}</span>
          ${entry.prefix ? `<span class="cfg-count mono">${esc(entry.prefix)}/</span>` : ''}
          ${compatible && entry.disabled ? '<span class="cfg-count">Disabled</span>' : ''}<span class="cfg-chevron" aria-hidden="true">›</span></summary>
        <div class="cfg-provider-body"><div class="cfg-grid">
          ${compatible ? configField([...path, 'name'], 'Provider name', { required: true, mono: false }) : configField([...path, 'label'], 'Label', { nullable: true, mono: false, placeholder: 'Optional' })}
          ${configField([...path, 'base-url'], 'Base URL', { type: 'url', required: compatible, nullable: !compatible, placeholder: compatible ? 'http://localhost:11434/v1' : 'Provider default' })}
          ${!compatible ? configField([...path, 'api-key'], 'API key', { type: 'password', required: true, wide: true }) : ''}</div>
          ${compatible ? configStrings([...path, 'api-keys'], 'API keys', 'Leave empty for local servers that do not require authentication.', true) : ''}
          ${compatible ? configSwitch([...path, 'disabled'], 'Disable provider', 'Keep its configuration while excluding it from requests.') : ''}
          <details class="cfg-advanced" data-cfg-open="${openKey}:advanced" ${c.opens[`${openKey}:advanced`] ? 'open' : ''}><summary>Advanced options<span class="cfg-chevron" aria-hidden="true">›</span></summary>
            <div class="cfg-grid">${configField([...path, 'prefix'], 'Model prefix', { nullable: true, placeholder: 'team', help: 'Clients use prefix/model to route requests to this key or provider.' })}
              ${configField([...path, 'proxy-url'], 'Proxy override', { nullable: true, placeholder: 'Use default proxy' })}</div>
            ${configAliases([...path, 'models'])}
            ${configStrings([...path, 'excluded-models'], 'Excluded models', 'Model names or patterns this provider must not serve. Use * as a wildcard.')}
            ${configHeaders([...path, 'headers'])}
          </details><div class="cfg-provider-foot">${configRemove(path, `Remove ${entry.label || entry.name || label}`)}</div>
        </div></details>`;
    }).join('') : `<div class="cfg-empty-state"><h3>No ${compatible ? 'compatible providers' : `${esc(label)} keys`} configured</h3><p>${compatible ? 'Connect OpenRouter, Ollama, or another OpenAI-compatible endpoint.' : 'Add an API key to use alongside your signed-in accounts.'}</p></div>`}`;
}

function configModelsHTML() {
  const names = [...new Set([...['claude', 'codex', 'antigravity', 'kimi', 'xai', 'meta', 'devin', 'vertex'],
    ...Object.keys(configGet(['oauth-model-alias']) || {}), ...Object.keys(configGet(['oauth-excluded-models']) || {})])];
  const provider = S.config.oauthProvider;
  return `<h2>OAuth model rules</h2><p class="cfg-description">Rules for signed-in accounts. API key model rules are set under Providers.</p>
    <div class="field cfg-provider-select"><label for="oauth-provider">Provider</label><select id="oauth-provider" data-config-provider="oauth">${names.map((p) => `<option value="${esc(p)}" ${p === provider ? 'selected' : ''}>${esc(PROVIDER[p] || p)}</option>`).join('')}</select></div>
    ${configAliases(['oauth-model-alias', provider], true)}
    ${configStrings(['oauth-excluded-models', provider], 'Excluded models', 'Model names or patterns these OAuth accounts must not serve. Use * as a wildcard.')}`;
}

function configDiagnosticsHTML() {
  return `<h2>Diagnostics</h2>${configSwitch(['debug'], 'Debug logging', 'Write detailed server logs for troubleshooting.', true)}
    <div class="cfg-divider"></div><h3>Compatibility notices</h3>
    ${S.config.ignored.length ? `<p class="cfg-description">These settings are retained in the file but have no effect in CLIProxyAPI-Rust.</p><ul class="cfg-notices">${S.config.ignored.map((s) => `<li>${esc(s)}</li>`).join('')}</ul>` : '<p class="cfg-description">No ignored CLIProxyAPI features were detected.</p>'}`;
}

// The whole file, for settings the sections don't cover. One kind of edit at a time:
// form drafts and YAML drafts never pile up on top of each other.
const rawDirty = () => S.config.raw.text != null && S.config.raw.text !== S.config.raw.saved;

function configYamlHTML() {
  const c = S.config;
  const r = c.raw;
  const head = '<h2>YAML file</h2><p class="cfg-description">The whole config.yaml, including settings the other sections don\'t cover. Changes are checked before they are applied.</p>';
  if (configDirty()) return `${head}<p class="cfg-empty">Save or discard the changes in the other sections first.</p>`;
  if (S.private && !c.reveal) {
    return `${head}<div class="cfg-empty-state"><h3>Hidden while emails and keys are hidden</h3><p>config.yaml holds your API keys in plain text.</p>
      <button type="button" class="btn" data-act="reveal-config">${ICON.eye}Show file</button></div>`;
  }
  if (r.text == null) {
    loadRawConfig();
    return `${head}<div class="skel-rows" aria-busy="true" aria-label="Loading">${'<div class="skel"></div>'.repeat(4)}</div>`;
  }
  return `${head}<label class="sr-only" for="cfg-yaml">config.yaml</label>
    <textarea id="cfg-yaml" class="editor" spellcheck="false" autocapitalize="off" autocomplete="off">${esc(r.text)}</textarea>`;
}

async function loadRawConfig() {
  const r = S.config.raw;
  if (r.loading) return;
  r.loading = true;
  try {
    const res = await api('/config');
    Object.assign(r, { text: res.text, saved: res.text });
  } catch (e) { S.config.msg = { kind: 'err', text: e.message }; }
  r.loading = false;
  if (S.route === 'config' && S.config.section === 'yaml') render();
}

async function saveRawConfig() {
  const c = S.config;
  if (c.busy || !rawDirty()) return;
  const ta = $('#cfg-yaml');
  const pos = ta ? [ta.selectionStart, ta.scrollTop] : null;
  c.busy = true; c.msg = null; render();
  try {
    const res = await api('/config', { method: 'PUT', body: JSON.stringify({ text: c.raw.text }) });
    c.raw.saved = c.raw.text;
    acceptConfig(await api('/config/settings'));
    c.msg = { kind: 'ok', text: res.restart_required ? 'Saved. Some changes need a server restart.' : 'Saved and applied.' };
    refreshAccounts();
  } catch (e) { c.msg = { kind: 'err', text: e.message }; }
  c.busy = false; render();
  const ta2 = $('#cfg-yaml');
  if (ta2 && pos) { ta2.focus(); ta2.selectionStart = ta2.selectionEnd = pos[0]; ta2.scrollTop = pos[1]; }
}

function configFootHTML() {
  const c = S.config;
  if (c.section === 'yaml') {
    const dirty = rawDirty();
    const msg = c.msg || (dirty ? { kind: '', text: 'The file has unsaved changes.' } : { kind: 'dim', text: 'Saved changes apply immediately.' });
    return `<div class="cfg-foot-actions"><button type="button" class="btn primary" data-config-act="save" ${dirty && !c.busy ? '' : 'disabled'}>${c.busy ? 'Saving…' : 'Save file'}</button>
      <button type="button" class="btn ghost" data-config-act="discard" ${dirty && !c.busy ? '' : 'disabled'}>Revert</button></div>
      <p class="msg ${msg.kind}" role="status">${esc(msg.text)}</p>`;
  }
  const count = Object.keys(configChanges()).length;
  const error = Object.values(c.errors).find(Boolean);
  const msg = c.msg || (count ? { kind: '', text: `${count} ${count === 1 ? 'setting has' : 'settings have'} unsaved changes.` } : { kind: 'dim', text: 'Changes are saved to your config file.' });
  return `<div class="cfg-foot-actions"><button type="button" class="btn primary" data-config-act="save" ${count && !c.busy ? '' : 'disabled'}>${c.busy ? 'Saving…' : 'Save changes'}</button>
    <button type="button" class="btn ghost" data-config-act="discard" ${count && !c.busy ? '' : 'disabled'}>Discard changes</button></div>
    <p class="msg ${error ? 'err' : msg.kind}" role="status">${esc(error || msg.text)}</p>`;
}

function configHTML() {
  const c = S.config;
  if (!c.values) {
    if (!c.loading && !c.msg) loadConfig();
    if (c.msg) return `<div class="empty"><h1>Configuration</h1><p class="err" role="alert">${esc(c.msg.text)}</p><button class="btn" data-config-act="reload">Try again</button></div>`;
    return skeletonHTML();
  }
  const sections = { server: configServerHTML, access: configAccessHTML, routing: configRoutingHTML,
    connections: configConnectionsHTML, providers: configProvidersHTML, models: configModelsHTML, diagnostics: configDiagnosticsHTML, yaml: configYamlHTML };
  return `<div class="page-head"><div><h1>Configuration</h1><p class="mono cfg-path">${esc(home(c.path))}</p></div>
      <button type="button" class="btn ghost" data-config-act="reload" ${c.busy ? 'disabled' : ''}>Reload settings</button></div>
    ${c.reloadConfirm ? '<div class="cfg-banner"><span>Reloading will discard your unsaved changes.</span><button class="btn small" data-config-act="confirm-reload">Reload and discard</button><button class="btn ghost small" data-config-act="cancel-reload">Keep editing</button></div>' : ''}
    ${c.restart_fields.length ? `<div class="cfg-banner warn" role="status">Restart CLIProxyAPI-Rust to apply changes to ${esc(c.restart_fields.join(', '))}.</div>` : ''}
    <div class="cfg-layout"><nav class="cfg-nav" aria-label="Configuration sections">${CONFIG_SECTIONS.map(([id, name]) => `<button type="button" data-config-act="section" data-section="${id}" ${c.section === id ? 'aria-current="page"' : ''}>${name}</button>`).join('')}</nav>
    <form id="config-form" class="cfg-content" aria-label="${esc(CONFIG_SECTIONS.find(([id]) => id === c.section)[1])} settings" novalidate>
      <fieldset ${c.busy ? 'disabled' : ''}>${sections[c.section]()}</fieldset></form></div>
    <div class="cfg-foot" id="config-foot" aria-live="polite">${configFootHTML()}</div>`;
}

function acceptConfig(result) {
  const c = S.config;
  Object.assign(c, { values: result.values, saved: configClone(result.values), defaults: result.defaults, revision: result.revision,
    path: result.path, ignored: result.ignored || [], restart_fields: result.restart_fields || [], errors: {}, secrets: {}, reloadConfirm: false,
    raw: { text: null, saved: null, loading: false } });
}

async function loadConfig() {
  const c = S.config;
  if (c.loading || c.busy) return;
  c.loading = true;
  c.msg = null;
  try { acceptConfig(await api('/config/settings')); }
  catch (e) { c.msg = { kind: 'err', text: e.message }; }
  c.loading = false;
  if (S.route === 'config') render();
}

function configUpdateFoot() {
  const foot = $('#config-foot');
  if (foot) foot.innerHTML = configFootHTML();
}

function configError(path, message) {
  const id = configId(path);
  if (message) S.config.errors[id] = message; else delete S.config.errors[id];
  const field = document.getElementById(id);
  if (field) { if (message) field.setAttribute('aria-invalid', 'true'); else field.removeAttribute('aria-invalid'); }
  const text = document.querySelector(`[data-error-for="${CSS.escape(id)}"]`);
  if (text) text.textContent = message || '';
}

function configValidate() {
  const c = S.config;
  c.errors = {};
  const invalidPaths = [];
  const invalid = (path, message) => { invalidPaths.push(path); configError(path, message); };
  const v = c.values;
  const changed = configChanges();
  for (const input of document.querySelectorAll('[data-header-path]')) {
    if (!input.checkValidity()) invalid([...JSON.parse(input.dataset.headerPath), Number(input.dataset.headerIndex), 'key'], input.validationMessage);
  }
  if ('port' in changed && (!Number.isInteger(v.port) || v.port < 1 || v.port > 65535)) invalid(['port'], 'Enter a port between 1 and 65535.');
  if ('host' in changed && v.host) {
    let valid = false;
    if (v.host.includes(':')) { try { valid = !!new URL(`http://[${v.host}]/`).hostname; } catch {} }
    else { const parts = v.host.split('.'); valid = parts.length === 4 && parts.every((p) => /^(0|[1-9]\d{0,2})$/.test(p) && Number(p) <= 255); }
    if (!valid) invalid(['host'], 'Enter an IPv4 or IPv6 address.');
  }
  if ('auth-dir' in changed && !v['auth-dir']?.trim()) invalid(['auth-dir'], 'Enter a credentials directory.');
  if ('request-retry' in changed && (!Number.isInteger(v['request-retry']) || v['request-retry'] < 0 || v['request-retry'] > 4294967295)) invalid(['request-retry'], 'Enter a nonnegative whole number.');
  if ('five-hour-reserve-percent' in changed && (!Number.isInteger(v['five-hour-reserve-percent']) || v['five-hour-reserve-percent'] < 0 || v['five-hour-reserve-percent'] > 100)) invalid(['five-hour-reserve-percent'], 'Enter a whole percentage between 0 and 100.');
  if ('session-affinity-idle-seconds' in changed && (!Number.isInteger(v['session-affinity-idle-seconds']) || v['session-affinity-idle-seconds'] < 60)) invalid(['session-affinity-idle-seconds'], 'Enter at least 60 seconds.');
  const checkURL = (path, proxy = false, required = false) => {
    const value = configGet(path);
    if (!value) { if (required) invalid(path, 'Enter a base URL.'); return; }
    try { const u = new URL(value); if (!u.hostname || !(proxy ? ['http:', 'https:', 'socks5:', 'socks5h:'] : ['http:', 'https:']).includes(u.protocol)) throw new Error(); }
    catch { invalid(path, proxy ? 'Use a complete HTTP, HTTPS or SOCKS5 URL.' : 'Use a complete HTTP or HTTPS URL.'); }
  };
  if ('tls' in changed && v.tls.enable) {
    if (!v.tls.cert?.trim()) invalid(['tls', 'cert'], 'Enter a certificate path.');
    if (!v.tls.key?.trim()) invalid(['tls', 'key'], 'Enter a private key path.');
  }
  if ('proxy-url' in changed) checkURL(['proxy-url'], true);
  for (const [provider, , field] of CONFIG_PROVIDERS) {
    if (!(field in changed)) continue;
    (v[field] || []).forEach((entry, i) => {
      const path = [field, i];
      if (provider === 'compat') {
        if (!entry.name?.trim()) invalid([...path, 'name'], 'Enter a provider name.');
        (entry['api-keys'] || []).forEach((key, j) => { if (!key.trim()) invalid([...path, 'api-keys', j], 'Enter an API key or remove this row.'); });
      } else if (!entry['api-key']?.trim()) invalid([...path, 'api-key'], 'Enter an API key.');
      checkURL([...path, 'base-url'], false, provider === 'compat');
      checkURL([...path, 'proxy-url'], true);
      (entry.models || []).forEach((model, j) => { if (!model.name?.trim()) invalid([...path, 'models', j, 'name'], 'Enter an upstream model name.'); });
      (entry['excluded-models'] || []).forEach((pattern, j) => { if (!pattern.trim()) invalid([...path, 'excluded-models', j], 'Enter a model pattern or remove this row.'); });
      for (const [name, value] of Object.entries(entry.headers || {})) {
        if (!/^[!#$%&'*+.^_`|~\w-]+$/.test(name) || /[\r\n]/.test(value)) invalid([...path, 'headers', name], 'Use a valid HTTP header name and value.');
      }
    });
  }
  if ('api-keys' in changed) (v['api-keys'] || []).forEach((key, i) => { if (!key.trim()) invalid(['api-keys', i], 'Enter an API key or remove this row.'); });
  if ('oauth-model-alias' in changed) for (const [provider, aliases] of Object.entries(v['oauth-model-alias'] || {})) aliases.forEach((alias, i) => {
    for (const key of ['name', 'alias']) if (!alias[key]?.trim()) invalid(['oauth-model-alias', provider, i, key], `Enter ${key === 'name' ? 'an upstream model name' : 'an alias'}.`);
  });
  if ('oauth-excluded-models' in changed) for (const [provider, patterns] of Object.entries(v['oauth-excluded-models'] || {})) patterns.forEach((pattern, i) => {
    if (!pattern.trim()) invalid(['oauth-excluded-models', provider, i], 'Enter a model pattern or remove this row.');
  });
  if (Object.keys(c.errors).length) {
    const id = Object.keys(c.errors)[0];
    const path = invalidPaths[0];
    if (path[0].startsWith('oauth-')) { c.section = 'models'; c.oauthProvider = path[1]; }
    else {
      const provider = CONFIG_PROVIDERS.find(([, , field]) => path[0] === field);
      if (provider) { c.section = 'providers'; c.provider = provider[0];
        for (let i = 0; i < v[provider[2]].length; i++) { c.opens[`${provider[2]}:${i}`] = true; c.opens[`${provider[2]}:${i}:advanced`] = true; }
      } else if (path[0] === 'api-keys') c.section = 'access';
      else if (['request-retry', 'session-affinity-idle-seconds', 'five-hour-reserve-percent'].includes(path[0])) c.section = 'routing';
      else if (path[0] === 'proxy-url') c.section = 'connections';
      else c.section = 'server';
    }
    render(); document.getElementById(id)?.focus(); return false;
  }
  return true;
}

async function saveConfig() {
  const c = S.config;
  if (c.section === 'yaml') return saveRawConfig();
  if (c.busy || !configDirty() || !configValidate()) return;
  const changes = configChanges();
  const oldKey = c.saved['management-key'];
  const newKey = c.values['management-key'];
  c.busy = true; c.msg = null; render();
  try {
    const result = await api('/config/settings', { method: 'PATCH', body: JSON.stringify({ revision: c.revision, changes }) });
    acceptConfig(result);
    c.msg = { kind: 'ok', text: [result.restart_required ? 'Saved. Some changes need a server restart.' : 'Saved and applied.',
      result.rewritten ? 'This file\'s layout couldn\'t be kept, so its comments were removed; the original is in config.yaml.bak.' : ''].filter(Boolean).join(' ') };
    // A newly configured dashboard key must also authenticate this browser's next request.
    if (oldKey !== newKey) {
      S.key = newKey;
      if (newKey) localStorage.setItem('cliproxyapi-rust.key', newKey); else localStorage.removeItem('cliproxyapi-rust.key');
      ws?.close();
    }
    refreshAccounts();
  } catch (e) { c.msg = { kind: 'err', text: e.message }; }
  c.busy = false; render();
}

function discardConfig() {
  if (S.config.busy) return;
  if (S.config.section === 'yaml') {
    Object.assign(S.config.raw, { text: S.config.raw.saved });
    S.config.msg = null;
    return render();
  }
  Object.assign(S.config, { values: configClone(S.config.saved), errors: {}, msg: null, secrets: {}, reloadConfirm: false });
  render();
}

function bindConfig() {
  const form = $('#config-form');
  if (!form) return;
  const ta = $('#cfg-yaml');
  ta?.addEventListener('input', () => { S.config.raw.text = ta.value; S.config.msg = null; configUpdateFoot(); });
  ta?.addEventListener('keydown', (e) => {
    if (e.key === 'Tab' && !e.shiftKey) {
      e.preventDefault();
      ta.setRangeText('  ', ta.selectionStart, ta.selectionEnd, 'end');
      S.config.raw.text = ta.value; configUpdateFoot();
    }
  });
  form.addEventListener('submit', (e) => { e.preventDefault(); saveConfig(); });
  form.addEventListener('input', (e) => {
    const input = e.target.closest('[data-cfg]');
    if (!input) return;
    const path = JSON.parse(input.dataset.cfg);
    let value = input.type === 'number' ? (input.value === '' ? null : Number(input.value)) : input.value;
    if (input.dataset.nullable && value === '') value = null;
    if (path[0] === 'management-allow-remote') value = JSON.parse(input.value);
    if (input.dataset.secret) S.config.secrets[JSON.stringify(path)] = true;
    configSet(path, value);
    configError(path, ''); S.config.msg = null;
    if (path[0] === 'routing') { render(); document.getElementById(input.id)?.focus(); }
    else configUpdateFoot();
  });
  const updateHeaderName = (e) => {
    const input = e.target.closest('[data-header-path]');
    if (input) {
      const path = JSON.parse(input.dataset.headerPath);
      const headers = configGet(path);
      const old = input.dataset.headerKey;
      const name = input.value.trim();
      if (!name || (name !== old && Object.hasOwn(headers, name))) {
        input.setCustomValidity('Use a unique, nonempty header name.');
        if (e.type === 'change') input.reportValidity();
        return;
      }
      input.setCustomValidity('');
      if (old !== name) {
        Object.defineProperty(headers, name, { value: headers[old], writable: true, enumerable: true, configurable: true });
        delete headers[old];
        const row = input.closest('.cfg-header-row');
        const valueInput = row.querySelector('[data-cfg]');
        const oldId = valueInput.id;
        valueInput.dataset.cfg = JSON.stringify([...path, name]);
        valueInput.id = configId([...path, name]);
        row.querySelector(`label[for="${CSS.escape(oldId)}"]`).htmlFor = valueInput.id;
        row.querySelector('[data-error-for]').dataset.errorFor = valueInput.id;
        row.querySelector('[data-error-for]').id = `${valueInput.id}-error`;
        valueInput.setAttribute('aria-describedby', `${valueInput.id}-error`);
        row.querySelector('[data-config-act="remove-header"]').dataset.key = name;
        input.dataset.headerKey = name;
      }
      S.config.msg = null; configUpdateFoot();
    }
    if (e.target.matches('[data-config-provider="oauth"]')) { S.config.oauthProvider = e.target.value; render(); }
  };
  form.addEventListener('input', updateHeaderName);
  form.addEventListener('change', updateHeaderName);
  form.addEventListener('toggle', (e) => {
    if (e.target.dataset.cfgOpen) S.config.opens[e.target.dataset.cfgOpen] = e.target.open;
  }, true);
}

document.addEventListener('keydown', (e) => {
  if (S.route === 'config' && (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 's') { e.preventDefault(); saveConfig(); }
});

document.addEventListener('click', (e) => {
  const button = e.target.closest('[data-config-act]');
  if (!button || S.config.busy) return;
  const c = S.config;
  const act = button.dataset.configAct;
  const path = button.dataset.path ? JSON.parse(button.dataset.path) : null;
  if (act === 'save') return saveConfig();
  if (act === 'discard') return discardConfig();
  if (act === 'reload') {
    if (configDirty() || rawDirty()) { c.reloadConfirm = true; render(); return; }
    return loadConfig();
  }
  if (act === 'confirm-reload') return loadConfig();
  if (act === 'cancel-reload') { c.reloadConfirm = false; render(); return; }
  if (act === 'section') {
    if (c.section === 'yaml' && button.dataset.section !== 'yaml' && rawDirty()) {
      c.msg = { kind: 'err', text: 'Save or revert the file first.' }; configUpdateFoot(); return;
    }
    c.section = button.dataset.section; c.msg = null; render(); $('.cfg-content h2')?.scrollIntoView({ block: 'nearest' }); return;
  }
  if (act === 'provider') { c.provider = button.dataset.provider; render(); return; }
  if (act === 'switch') {
    configSet(path, !configGet(path)); c.msg = null; render(); document.getElementById(configId(path))?.focus(); return;
  }
  if (act === 'reveal') {
    const input = document.getElementById(configId(path));
    const visible = input.type === 'password';
    input.type = visible ? 'text' : 'password'; input.value = configGet(path) || '';
    button.textContent = visible ? 'Hide' : 'Show'; button.setAttribute('aria-label', `${visible ? 'Hide' : 'Show'} key`);
    return;
  }
  if (act === 'clear-secret') { configSet(path, ''); c.secrets[JSON.stringify(path)] = true; }
  if (act === 'add-provider') {
    const [provider, , field] = CONFIG_PROVIDERS.find(([p]) => p === c.provider);
    const list = c.values[field];
    c.opens[`${field}:${list.length}`] = true;
    list.push(provider === 'compat' ? { name: '', 'base-url': '', 'api-keys': [], models: [], headers: {}, 'excluded-models': [], disabled: false }
      : { 'api-key': '', label: null, 'base-url': null, 'proxy-url': null, prefix: null, models: [], headers: {}, 'excluded-models': [] });
  }
  if (act === 'add-string' || act === 'add-alias') {
    const list = configGet(path) || [];
    list.push(act === 'add-string' ? '' : button.dataset.oauth ? { name: '', alias: '', fork: false } : { name: '', alias: null });
    configSet(path, list);
  }
  if (act === 'remove') {
    const parent = path.slice(0, -1); const index = path[path.length - 1];
    configGet(parent).splice(index, 1);
    c.secrets = {};
  }
  if (act === 'add-header') {
    const headers = configGet(path) || {};
    let key = 'X-Header'; let n = 2; while (Object.hasOwn(headers, key)) key = `X-Header-${n++}`;
    headers[key] = ''; configSet(path, headers);
  }
  if (act === 'remove-header') delete configGet(path)[button.dataset.key];
  c.msg = null; c.errors = {}; render();
  if (act.startsWith('add-')) {
    const inputs = formInputsForAddedRow(act, path);
    inputs?.focus();
  }
});

function formInputsForAddedRow(act, path) {
  if (act === 'add-provider') return $('.cfg-provider[open]:last-of-type input');
  if (path) {
    if (act === 'add-header') return document.querySelectorAll('[data-header-path]')[document.querySelectorAll('[data-header-path]').length - 1];
    const index = (configGet(path)?.length || 1) - 1;
    return document.getElementById(configId([...path, index, ...(act === 'add-alias' ? ['name'] : [])]));
  }
}
