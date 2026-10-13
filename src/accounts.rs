//! Accounts: OAuth credential files + API keys from config, with selection,
//! cooldowns and usage counters.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::{Config, ModelAlias, OAuthAlias, Routing};
use crate::ir::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
    Vertex,
    Antigravity,
    Kimi,
    Xai,
    Meta,
    Devin,
    #[serde(rename = "openai-compat")]
    Compat,
}

pub const PROVIDERS: [Provider; 10] = [
    Provider::Claude,
    Provider::Codex,
    Provider::Gemini,
    Provider::Vertex,
    Provider::Antigravity,
    Provider::Kimi,
    Provider::Xai,
    Provider::Meta,
    Provider::Devin,
    Provider::Compat,
];

const ANTIGRAVITY_MODELS: &[&str] = &[
    "claude-opus-4-6-thinking",
    "claude-sonnet-4-6",
    "gemini-3.8-flash-high",
    "gemini-3.7-flash-high",
    "gemini-3.6-flash-high",
    "gemini-pro-agent",
    "gemini-3.1-pro-low",
    "gemini-3-flash",
    "gemini-3.1-flash-lite",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-image",
    "gpt-oss-120b-medium",
];

const DEVIN_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-fable-5-1",
    "claude-sonnet-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5-6-sol",
    "gpt-5-6-terra",
    "gpt-5-6-luna",
    "gemini-3-8-flash",
    "gemini-3-7-flash",
    "gemini-3-1-pro",
    "grok-4-7",
    "grok-4-6",
    "kimi-k3",
    "glm-5-3",
    "glm-5-3-flash",
    "deepseek-v4-pro",
    "deepseek-v4-1-flash",
    "swe-2",
    "swe-1-7",
    "swe-1-7-lightning",
];

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
            Provider::Vertex => "vertex",
            Provider::Antigravity => "antigravity",
            Provider::Kimi => "kimi",
            Provider::Xai => "xai",
            Provider::Meta => "meta",
            Provider::Devin => "devin",
            Provider::Compat => "openai-compat",
        }
    }

    /// Accepts the canonical name and the aliases people actually type.
    pub fn parse(s: &str) -> Option<Provider> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Provider::Claude,
            "codex" | "openai" | "chatgpt" => Provider::Codex,
            "gemini" | "aistudio" | "google" => Provider::Gemini,
            "vertex" | "vertex-ai" => Provider::Vertex,
            "antigravity" => Provider::Antigravity,
            "kimi" | "moonshot" => Provider::Kimi,
            "xai" | "grok" => Provider::Xai,
            "meta" | "muse" => Provider::Meta,
            "devin" | "windsurf" | "codeium" => Provider::Devin,
            "compat" | "openai-compat" | "openai-compatibility" => Provider::Compat,
            _ => return None,
        })
    }

    /// Wire formats the upstream accepts, preferred one first. A client whose
    /// format is in this list is passed through; everything else is translated
    /// to the first entry.
    pub fn wires(self) -> &'static [Format] {
        match self {
            Provider::Claude => &[Format::Claude],
            Provider::Codex | Provider::Xai | Provider::Meta => &[Format::Responses],
            Provider::Gemini | Provider::Vertex | Provider::Antigravity => &[Format::Gemini],
            Provider::Kimi => &[Format::Chat, Format::Claude, Format::Responses],
            // Devin speaks its own protobuf dialect, built from the IR.
            Provider::Devin => &[],
            Provider::Compat => &[Format::Chat],
        }
    }

    /// Models this provider serves when routing automatically. Names only an
    /// aggregator knows (`gemini-3.8-flash-high`, `gpt-5-6-sol`) stay with it.
    pub fn serves(self, model: &str) -> bool {
        let aggregator_only = || {
            let known = |list: &[&str]| list.iter().any(|m| m.eq_ignore_ascii_case(model));
            (known(ANTIGRAVITY_MODELS) || known(DEVIN_MODELS)) && !known(self.builtin_models())
        };
        self.family(model) && (matches!(self, Provider::Antigravity | Provider::Devin) || !aggregator_only())
    }

    /// Model-name families this provider can serve (used for explicit `provider/model`).
    pub fn family(self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        match self {
            Provider::Claude => m.starts_with("claude-"),
            Provider::Codex => {
                m.starts_with("gpt-")
                    || m.starts_with("codex-")
                    || (m.len() >= 2 && m.starts_with('o') && m.as_bytes()[1].is_ascii_digit())
            }
            Provider::Gemini => m.starts_with("gemini-") || m.starts_with("gemma-"),
            Provider::Vertex => m.starts_with("gemini-") || m.starts_with("imagen-"),
            Provider::Kimi => m.starts_with("kimi-") || m.starts_with("moonshot-"),
            Provider::Xai => m.starts_with("grok-"),
            Provider::Meta => m.starts_with("muse-"),
            Provider::Antigravity | Provider::Devin => {
                self.builtin_models().iter().any(|b| b.eq_ignore_ascii_case(model))
            }
            Provider::Compat => false,
        }
    }

    /// Aggregators (Antigravity, Devin) also serve other vendors' models;
    /// requests prefer the vendor's own provider and overflow to them.
    pub fn first_party(self, model: &str) -> bool {
        !matches!(self, Provider::Antigravity | Provider::Devin)
            || !PROVIDERS
                .iter()
                .any(|p| !matches!(p, Provider::Antigravity | Provider::Devin | Provider::Compat) && p.serves(model))
    }

    pub fn builtin_models(self) -> &'static [&'static str] {
        match self {
            Provider::Claude => &[
                "claude-fable-5-1",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-haiku-4-5-20251001",
            ],
            Provider::Codex => &[
                "gpt-6-astra",
                "gpt-6.1-sol",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-image-2",
            ],
            Provider::Gemini => &[
                "gemini-3.8-flash",
                "gemini-3.7-flash",
                "gemini-3.6-flash",
                "gemini-3.1-pro-preview",
                "gemini-3.5-flash-lite",
                "gemini-3.1-flash-image-preview",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
            ],
            Provider::Vertex => &[
                "gemini-3.8-flash",
                "gemini-3.7-flash",
                "gemini-3.1-pro",
                "gemini-3.5-flash-lite",
                "gemini-3.1-flash-image",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
                "imagen-4.0-generate-001",
                "imagen-4.0-ultra-generate-001",
                "imagen-4.0-fast-generate-001",
            ],
            Provider::Antigravity => ANTIGRAVITY_MODELS,
            Provider::Kimi => &[
                "kimi-k3",
                "kimi-k3-256k",
                "kimi-k2.8",
                "kimi-k2.8-code",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6",
                "kimi-k2.5",
                "kimi-k2-thinking",
            ],
            Provider::Xai => &[
                "grok-4.7",
                "grok-4.7-build-fast",
                "grok-4.6",
                "grok-build-0.1",
                "grok-4.5",
                "grok-4.3",
                "grok-4.20-0309-reasoning",
                "grok-4.20-0309-non-reasoning",
                "grok-3-mini",
                "grok-imagine-image-2.0",
                "grok-imagine-video-1.5",
            ],
            Provider::Meta => &["muse-spark-1.3", "muse-spark-1.2", "muse-spark-1.1"],
            Provider::Devin => DEVIN_MODELS,
            Provider::Compat => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub enum Credential {
    OAuth(OAuth),
    ApiKey { key: String, base_url: Option<String> },
}

#[derive(Debug, Clone, Default)]
pub struct OAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    /// ChatGPT account id (Codex) or Anthropic account uuid (Claude).
    pub account_id: Option<String>,
    /// Optional API base override (e.g. a gateway), from `base_url` in the file.
    pub base_url: Option<String>,
    /// Google Cloud project (Antigravity, Vertex).
    pub project_id: Option<String>,
    /// Every other field of the credential file (provider specific extras).
    pub raw: Map<String, Value>,
}

impl OAuth {
    pub fn field(&self, key: &str) -> Option<&str> {
        self.raw.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counters {
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

#[derive(Debug, Default)]
pub struct AccountState {
    /// Shared across account reloads so in-flight requests remain counted.
    pub active_requests: Arc<AtomicUsize>,
    pub disabled: bool,
    /// Cooldowns keyed by model ("*" = whole account).
    pub cooldowns: HashMap<String, DateTime<Utc>>,
    /// Confirmed subscription exhaustion, distinct from temporary rate limits. A
    /// banked reset or a fresh usage poll clears it; sessions move off it.
    pub quota_cooldowns: HashMap<String, DateTime<Utc>>,
    pub quota_epoch: u64,
    pub quota_refreshing: bool,
    pub banked_resets: Option<crate::banked_resets::View>,
    pub strikes: u32,
    pub last_error: Option<String>,
    pub last_used: Option<DateTime<Utc>>,
    pub counters: Counters,
    /// Subscription usage windows (Claude, ChatGPT).
    pub quota: crate::quota::Quota,
    pub notification_evidence: crate::notifications::state::Evidence,
    pub notifications_enabled: bool,
    /// In-memory cutoff also records pauses shorter than a notification worker tick.
    pub notifications_changed_at: Option<DateTime<Utc>>,
}

/// Counts an actual request attempt, including streaming, until completion or cancellation.
pub struct RequestLoad(Arc<AtomicUsize>);

impl RequestLoad {
    pub fn new(account: &Account) -> Self {
        let active = account.state.lock().active_requests.clone();
        active.fetch_add(1, Ordering::Relaxed);
        Self(active)
    }
}

impl Drop for RequestLoad {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Account {
    pub id: String,
    pub provider: Provider,
    pub label: String,
    pub path: Option<PathBuf>,
    /// Display name of the openai-compatibility group.
    pub group: Option<String>,
    /// Public model name -> upstream model name. Empty = provider default families.
    pub models: Vec<ModelAlias>,
    pub headers: BTreeMap<String, String>,
    pub proxy_url: Option<String>,
    pub cred: RwLock<Credential>,
    pub state: Mutex<AccountState>,
    pub refresh_lock: tokio::sync::Mutex<()>,
    /// Stable per-account device id for Claude cloaking.
    pub device_id: String,
    pub session_id: String,
    /// Models discovered from the upstream at runtime (Antigravity).
    pub discovered: RwLock<Vec<String>>,
    /// Clients reach this account as `prefix/model`.
    pub prefix: Option<String>,
    /// Model patterns this account must not serve.
    pub excluded: Vec<String>,
    /// OAuth model renames (config `oauth-model-alias` + the file's `model_aliases`).
    pub aliases: Vec<OAuthAlias>,
}

/// `gemini-3-8-flash` -> `gemini-3.8-flash`, `gpt-6-1-sol` -> `gpt-6.1-sol`
/// (single digits joined by a hyphen are a version number).
fn dot_versions(m: &str) -> String {
    let b = m.as_bytes();
    let mut out = String::with_capacity(m.len());
    for (i, c) in m.char_indices() {
        let digit = |j: usize| b.get(j).is_some_and(|c| c.is_ascii_digit());
        let lone = |j: usize| digit(j) && !digit(j.wrapping_sub(1)) && !digit(j + 1);
        // Only between two single digits, so dates (20251001) are left alone.
        if c == '-' && i > 0 && lone(i - 1) && lone(i + 1) && (i < 2 || b[i - 2] == b'-') {
            out.push('.');
        } else {
            out.push(c);
        }
    }
    out
}

/// `*` wildcard match, case-insensitive (`gemini-2.5-*`, `*-preview`, `*flash*`).
pub fn wildcard(pattern: &str, s: &str) -> bool {
    let (p, s) = (pattern.trim().to_ascii_lowercase(), s.to_ascii_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == s;
    }
    let mut rest = s.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(at) = rest.find(part) else { return false };
            rest = &rest[at + part.len()..];
        }
    }
    true
}

impl Account {
    pub fn first_party(&self, model: &str) -> bool {
        !self.models.is_empty() || self.provider.first_party(model)
    }

    pub fn is_oauth(&self) -> bool {
        matches!(*self.cred.read(), Credential::OAuth(_))
    }

    /// Upstream model name if this account can serve `model`.
    pub fn resolve(&self, model: &str) -> Option<String> {
        self.resolve_with(model, false)
    }

    fn excludes(&self, upstream: &str) -> bool {
        self.excluded.iter().any(|p| wildcard(p, upstream))
    }

    /// `forced`: the client named this provider explicitly, so any model of its family goes.
    pub fn resolve_with(&self, model: &str, forced: bool) -> Option<String> {
        self.resolve_inner(model, forced).filter(|m| !self.excludes(m))
    }

    fn resolve_inner(&self, model: &str, forced: bool) -> Option<String> {
        if !self.models.is_empty() {
            return self.models.iter().find(|m| m.public().eq_ignore_ascii_case(model)).map(|m| m.name.clone());
        }
        if let Some(a) = self.aliases.iter().find(|a| a.alias.eq_ignore_ascii_case(model)) {
            return Some(a.name.clone());
        }
        // Renamed without `fork`: the original name is gone.
        if self.aliases.iter().any(|a| a.name.eq_ignore_ascii_case(model) && !a.fork) {
            return None;
        }
        let aggregator = matches!(self.provider, Provider::Antigravity | Provider::Devin);
        let known = if forced { aggregator || self.provider.family(model) } else { self.provider.serves(model) }
            || self.discovered.read().iter().any(|m| m.eq_ignore_ascii_case(model));
        known.then(|| self.upstream_name(model))
    }

    /// Kimi Code exposes `k3`, `kimi-for-coding`, ... while clients use `kimi-*` names.
    fn upstream_name(&self, model: &str) -> String {
        if self.provider != Provider::Kimi || self.custom_base() {
            return model.to_string();
        }
        let base = model.trim().to_ascii_lowercase();
        let base = base.trim_end_matches("[1m]");
        match base {
            "kimi-k2.8" | "kimi-k2.8-code" | "kimi-k2.7-code" | "kimi-for-coding" => "kimi-for-coding".into(),
            "kimi-k2.7-code-highspeed" | "kimi-for-coding-highspeed" => "kimi-for-coding-highspeed".into(),
            other => other.strip_prefix("kimi-").unwrap_or(other).to_string(),
        }
    }

    /// True when an API key points at a non-default endpoint.
    fn custom_base(&self) -> bool {
        match &*self.cred.read() {
            Credential::ApiKey { base_url, .. } => base_url.as_ref().is_some_and(|b| !b.trim().is_empty()),
            Credential::OAuth(_) => false,
        }
    }

    pub fn public_models(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.iter().filter(|m| !self.excludes(&m.name)).map(|m| m.public().to_string()).collect();
        }
        let mut out: Vec<String> = self.provider.builtin_models().iter().map(|s| s.to_string()).collect();
        for m in self.discovered.read().iter() {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(m)) {
                out.push(m.clone());
            }
        }
        out.retain(|m| !self.excludes(m) && !self.aliases.iter().any(|a| a.name.eq_ignore_ascii_case(m) && !a.fork));
        for a in &self.aliases {
            if !self.excludes(&a.name) && !out.iter().any(|o| o.eq_ignore_ascii_case(&a.alias)) {
                out.push(a.alias.clone());
            }
        }
        out
    }

    pub fn cooling_until(&self, model: &str) -> Option<DateTime<Utc>> {
        let quota_model = self.resolve(model).unwrap_or_else(|| model.to_string());
        let st = self.state.lock();
        let now = Utc::now();
        if st.quota_refreshing {
            return Some(now + chrono::Duration::seconds(30));
        }
        // A used-up quota window counts as a cooldown, so we don't wait for the 429.
        let spent = st
            .quota
            .exhausted_until(&quota_model)
            .or_else(|| st.quota.exhausted(&quota_model).then(|| Utc::now() + chrono::Duration::minutes(5)));
        [
            st.cooldowns.get("*"),
            st.cooldowns.get(model),
            st.quota_cooldowns.get("*"),
            st.quota_cooldowns.get(model),
            spent.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|t| **t > now)
        .max()
        .copied()
    }

    pub fn exhausted_until(&self, model: &str) -> Option<DateTime<Utc>> {
        let quota_model = self.resolve(model).unwrap_or_else(|| model.to_string());
        let st = self.state.lock();
        let spent = st
            .quota
            .exhausted_until(&quota_model)
            .or_else(|| st.quota.exhausted(&quota_model).then(|| Utc::now() + chrono::Duration::minutes(5)));
        [st.quota_cooldowns.get("*"), st.quota_cooldowns.get(model), spent.as_ref()]
            .into_iter()
            .flatten()
            .filter(|t| **t > Utc::now())
            .max()
            .copied()
    }

    /// Records confirmed quota exhaustion, unless a banked reset is being applied.
    pub fn exhaust(&self, model: &str, until: DateTime<Utc>, reason: &str, epoch: u64) {
        self.cool_quota(model, until, reason, epoch);
    }

    pub fn cool(&self, model: Option<&str>, until: DateTime<Utc>, reason: &str) {
        let mut st = self.state.lock();
        st.cooldowns.insert(model.unwrap_or("*").to_string(), until);
        st.last_error = Some(reason.to_string());
    }

    pub fn quota_epoch(&self) -> u64 {
        self.state.lock().quota_epoch
    }

    /// Apply a model quota rejection only if its request predates no newer usage or reset evidence.
    pub fn cool_quota(&self, model: &str, until: DateTime<Utc>, reason: &str, epoch: u64) {
        let mut st = self.state.lock();
        if st.quota_epoch == epoch && !st.quota_refreshing && !st.disabled {
            st.quota_cooldowns.insert(model.to_string(), until);
            if st.notifications_enabled {
                st.notification_evidence.exhaust(model);
            }
            st.last_error = Some(reason.to_string());
        }
    }

    pub fn record_ok(&self) {
        let mut st = self.state.lock();
        st.strikes = 0;
        st.last_error = None;
    }

    pub fn snapshot(&self) -> Value {
        let st = self.state.lock();
        let now = Utc::now();
        let mut cooldowns: BTreeMap<&str, String> =
            st.cooldowns.iter().filter(|(_, t)| **t > now).map(|(k, t)| (k.as_str(), t.to_rfc3339())).collect();
        for (model, deadline) in &st.quota_cooldowns {
            if *deadline > now {
                let value = deadline.to_rfc3339();
                cooldowns
                    .entry(model.as_str())
                    .and_modify(|v| {
                        if value > *v {
                            *v = value.clone();
                        }
                    })
                    .or_insert(value);
            }
        }
        if st.quota_refreshing {
            cooldowns.insert("*", (now + chrono::Duration::seconds(30)).to_rfc3339());
        }
        // A used-up usage window blocks every model until it resets.
        if let Some(t) = st.quota.exhausted_until("") {
            cooldowns.entry("*").or_insert(t.to_rfc3339());
        }
        let (kind, expires, email) = match &*self.cred.read() {
            Credential::OAuth(o) if o.raw.contains_key("service_account") => ("service-account", None, o.email.clone()),
            Credential::OAuth(o) => ("oauth", o.expires_at.map(|t| t.to_rfc3339()), o.email.clone()),
            Credential::ApiKey { .. } => ("api-key", None, None),
        };
        serde_json::json!({
            "id": self.id,
            "provider": self.provider,
            "label": self.label,
            "email": email,
            "kind": kind,
            "group": self.group,
            "file": self.path.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().to_string()),
            "disabled": st.disabled,
            "cooldowns": cooldowns,
            "last_error": st.last_error,
            "last_used": st.last_used.map(|t| t.to_rfc3339()),
            "expires_at": expires,
            "counters": st.counters,
            "quota": st.quota,
            "banked_resets": st.banked_resets,
            "models": self.public_models(),
        })
    }
}

// --------------------------------------------------------------------- loading

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc)),
        Value::Number(n) => {
            let n = n.as_i64()?;
            let secs = if n > 10_000_000_000 { n / 1000 } else { n };
            DateTime::from_timestamp(secs, 0)
        }
        _ => None,
    }
}

pub fn file_provider(kind: &str) -> Option<Provider> {
    let k = kind.trim().to_ascii_lowercase();
    if k.starts_with("kimi") {
        return Some(Provider::Kimi);
    }
    match k.as_str() {
        "gemini" | "gemini-cli" | "aistudio" | "compat" | "openai-compat" => None,
        other => Provider::parse(other),
    }
}

pub fn read_oauth_file(path: &Path) -> Option<(Provider, OAuth, bool, Map<String, Value>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let map: Map<String, Value> = serde_json::from_str(&text).ok()?;
    let provider = file_provider(map.get("type").and_then(Value::as_str)?)?;
    let s = |k: &str| map.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let mut access = s("access_token");
    if provider == Provider::Meta {
        // Meta requests use the API key minted from the device token.
        access = s("api_key").or(access).filter(|t| !t.starts_with("dca:"));
    }
    if provider == Provider::Devin {
        access = s("session_token").or_else(|| s("api_key")).or(access);
    }
    let oauth = OAuth {
        access_token: access.unwrap_or_default(),
        refresh_token: s("refresh_token").unwrap_or_default(),
        expires_at: map.get("expired").or_else(|| map.get("expires_at")).and_then(parse_time),
        email: s("email").or_else(|| s("client_email")),
        account_id: s("account_id").or_else(|| s("account_uuid")),
        base_url: s("base_url"),
        project_id: s("project_id")
            .or_else(|| map.get("service_account").and_then(|v| v["project_id"].as_str()).map(String::from)),
        raw: map.clone(),
    };
    let disabled = map.get("disabled").and_then(Value::as_bool).unwrap_or(false);
    Some((provider, oauth, disabled, map))
}

/// Writes refreshed tokens back into the credential file, keeping unknown fields.
pub fn write_oauth_file(path: &Path, provider: Provider, o: &OAuth, extra: &[(&str, Value)]) -> std::io::Result<()> {
    let mut map: Map<String, Value> =
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    map.insert("type".into(), provider.as_str().into());
    map.insert("access_token".into(), o.access_token.clone().into());
    map.insert("refresh_token".into(), o.refresh_token.clone().into());
    if let Some(e) = &o.email {
        map.insert("email".into(), e.clone().into());
    }
    if let Some(t) = o.expires_at {
        map.insert("expired".into(), t.to_rfc3339().into());
    }
    if let Some(a) = &o.account_id {
        let key = if provider == Provider::Claude { "account_uuid" } else { "account_id" };
        map.insert(key.into(), a.clone().into());
    }
    if let Some(p) = &o.project_id {
        map.insert("project_id".into(), p.clone().into());
    }
    if let Some(b) = &o.base_url {
        map.insert("base_url".into(), b.clone().into());
    }
    if provider == Provider::Meta {
        map.insert("api_key".into(), o.access_token.clone().into());
    }
    if provider == Provider::Devin {
        map.insert("api_key".into(), o.access_token.clone().into());
        map.insert("session_token".into(), o.access_token.clone().into());
        map.remove("access_token");
    }
    if o.refresh_token.is_empty() {
        map.remove("refresh_token");
    }
    map.insert("last_refresh".into(), Utc::now().to_rfc3339().into());
    for (k, v) in extra {
        map.insert((*k).into(), v.clone());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(map))?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(tmp, path)
}

pub fn set_file_disabled(path: &Path, disabled: bool) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut map: Map<String, Value> = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    if disabled {
        map.insert("disabled".into(), true.into());
    } else {
        map.remove("disabled");
    }
    std::fs::write(path, serde_json::to_vec_pretty(&Value::Object(map))?)
}

fn mask(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 10 {
        return format!("{}…", k.get(..3).unwrap_or(""));
    }
    format!("{}…{}", &k[..6], &k[k.len() - 4..])
}

fn key_id(prefix: &str, key: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(key.as_bytes());
    format!("{prefix}:{}", &hex::encode(h)[..10])
}

fn random_hex(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    hex::encode(bytes)
}

struct Spec {
    id: String,
    provider: Provider,
    label: String,
    path: Option<PathBuf>,
    group: Option<String>,
    models: Vec<ModelAlias>,
    headers: BTreeMap<String, String>,
    proxy_url: Option<String>,
    cred: Credential,
    disabled: bool,
    device_id: Option<String>,
    prefix: Option<String>,
    excluded: Vec<String>,
    aliases: Vec<OAuthAlias>,
}

/// Config sections keyed by provider name (`claude`, `codex`, `aistudio`, ...).
fn for_provider<T: Clone>(map: &BTreeMap<String, Vec<T>>, p: Provider) -> Vec<T> {
    map.iter()
        .filter(|(k, _)| Provider::parse(k) == Some(p) || (k.as_str() == "aistudio" && p == Provider::Gemini))
        .flat_map(|(_, v)| v.clone())
        .collect()
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|s| s.trim().trim_matches('/').to_string()).filter(|s| !s.is_empty())
}

fn collect(cfg: &Config) -> Vec<Spec> {
    let mut specs = Vec::new();
    let dir = cfg.auth_dir();
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(&dir).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect()).unwrap_or_default();
    files.sort();
    for path in files {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some((provider, oauth, disabled, map)) = read_oauth_file(&path) else { continue };
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let device_id = map
            .get("claude_device_ids")
            .and_then(|v| v.get(0))
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64)
            .map(String::from);
        let mut aliases = for_provider(&cfg.oauth_model_alias, provider);
        let file_aliases = map.get("model_aliases").or_else(|| map.get("model-aliases"));
        if let Some(list) = file_aliases.and_then(|v| serde_json::from_value::<Vec<OAuthAlias>>(v.clone()).ok()) {
            // Per-account aliases take precedence over global ones.
            aliases.retain(|a| !list.iter().any(|l| l.alias.eq_ignore_ascii_case(&a.alias)));
            aliases.splice(0..0, list);
        }
        aliases.retain(|a| !a.name.is_empty() && !a.alias.is_empty());
        let mut excluded = for_provider(&cfg.oauth_excluded_models, provider);
        if let Some(list) =
            map.get("excluded_models").and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        {
            excluded.extend(list);
        }
        let prefix = nonempty(&map.get("prefix").and_then(Value::as_str).map(String::from));
        specs.push(Spec {
            prefix,
            excluded,
            aliases,
            id: format!("file:{name}"),
            provider,
            label: oauth
                .email
                .clone()
                .or_else(|| oauth.field("user_name").map(String::from))
                .unwrap_or_else(|| name.trim_end_matches(".json").to_string()),
            path: Some(path),
            group: None,
            models: vec![],
            headers: BTreeMap::new(),
            proxy_url: map.get("proxy_url").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
            cred: Credential::OAuth(oauth),
            disabled,
            device_id,
        });
    }
    let keys = [
        (Provider::Claude, &cfg.claude_api_key),
        (Provider::Codex, &cfg.codex_api_key),
        (Provider::Gemini, &cfg.gemini_api_key),
        (Provider::Vertex, &cfg.vertex_api_key),
        (Provider::Kimi, &cfg.kimi_api_key),
        (Provider::Xai, &cfg.xai_api_key),
        (Provider::Meta, &cfg.meta_api_key),
    ];
    for (provider, entries) in keys {
        for e in entries.iter().filter(|e| !e.api_key.trim().is_empty()) {
            specs.push(Spec {
                id: key_id(provider.as_str(), &e.api_key),
                provider,
                label: e.label.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| mask(&e.api_key)),
                path: None,
                group: None,
                models: e.models.clone(),
                headers: e.headers.clone(),
                proxy_url: e.proxy_url.clone(),
                cred: Credential::ApiKey { key: e.api_key.trim().to_string(), base_url: e.base_url.clone() },
                disabled: false,
                device_id: None,
                prefix: nonempty(&e.prefix),
                excluded: e.excluded_models.clone(),
                aliases: vec![],
            });
        }
    }
    for c in cfg.openai_compatibility.iter().filter(|c| !c.disabled) {
        for k in c.api_keys.iter().filter(|k| !k.trim().is_empty()) {
            specs.push(Spec {
                id: key_id(&format!("compat-{}", c.name), k),
                provider: Provider::Compat,
                label: format!("{} {}", c.name, mask(k)),
                path: None,
                group: Some(c.name.clone()),
                models: c.models.clone(),
                headers: c.headers.clone(),
                proxy_url: c.proxy_url.clone(),
                cred: Credential::ApiKey { key: k.trim().to_string(), base_url: Some(c.base_url.clone()) },
                disabled: false,
                device_id: None,
                prefix: nonempty(&c.prefix),
                excluded: c.excluded_models.clone(),
                aliases: vec![],
            });
        }
        // Keyless local endpoints (Ollama, LM Studio, ...)
        if c.api_keys.iter().all(|k| k.trim().is_empty()) && !c.base_url.is_empty() {
            specs.push(Spec {
                id: format!("compat-{}:nokey", c.name),
                provider: Provider::Compat,
                label: c.name.clone(),
                path: None,
                group: Some(c.name.clone()),
                models: c.models.clone(),
                headers: c.headers.clone(),
                proxy_url: c.proxy_url.clone(),
                cred: Credential::ApiKey { key: String::new(), base_url: Some(c.base_url.clone()) },
                disabled: false,
                device_id: None,
                prefix: nonempty(&c.prefix),
                excluded: c.excluded_models.clone(),
                aliases: vec![],
            });
        }
    }
    specs
}

// ------------------------------------------------------------------------ pool

#[derive(Default)]
pub struct Pool {
    accounts: RwLock<Vec<Arc<Account>>>,
    cursor: Mutex<HashMap<String, usize>>,
    /// `force-model-prefix`: unprefixed names skip accounts that have a prefix.
    force_prefix: std::sync::atomic::AtomicBool,
}

/// Restricts a request to some accounts (`provider/model` or `prefix/model`).
#[derive(Debug, Clone, PartialEq)]
pub enum Only {
    Provider(Provider),
    Prefix(String),
}

pub enum Pick {
    Ok(Arc<Account>, String),
    /// Every candidate is cooling down; earliest availability.
    Cooling(DateTime<Utc>),
    None,
}

impl Pool {
    /// Resolve a pinned account without replacing it with a different provider.
    pub fn resolve_account(&self, a: &Account, model: &str, only: Option<&Only>) -> Option<String> {
        let allowed = match only {
            Some(Only::Provider(p)) => *p == a.provider,
            Some(Only::Prefix(x)) => a.prefix.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(x)),
            None => !(self.force_prefix.load(std::sync::atomic::Ordering::Relaxed) && a.prefix.is_some()),
        };
        allowed.then(|| a.resolve_with(model, matches!(only, Some(Only::Provider(_))))).flatten()
    }
    /// Rebuild configured accounts while preserving compatible state and invalidating paused evidence.
    pub fn reload(&self, cfg: &Config) {
        self.force_prefix.store(cfg.force_model_prefix, std::sync::atomic::Ordering::Relaxed);
        let specs = collect(cfg);
        let old: HashMap<String, Arc<Account>> =
            self.accounts.read().iter().map(|a| (a.id.clone(), a.clone())).collect();
        let mut next = Vec::with_capacity(specs.len());
        for s in specs {
            if let Some(prev) = old.get(&s.id) {
                // Keep counters and cooldowns; refresh credentials from disk/config.
                let same_shape = prev.models.len() == s.models.len()
                    && prev.headers == s.headers
                    && prev.proxy_url == s.proxy_url
                    && prev.prefix == s.prefix
                    && prev.excluded == s.excluded
                    && prev.aliases == s.aliases;
                if same_shape {
                    let identity_changed = match (&*prev.cred.read(), &s.cred) {
                        (Credential::OAuth(old), Credential::OAuth(new)) => {
                            old.account_id != new.account_id || old.base_url != new.base_url
                        }
                        _ => false,
                    };
                    *prev.cred.write() = s.cred;
                    let mut st = prev.state.lock();
                    if st.disabled != s.disabled || st.notifications_enabled != cfg.notifications.enabled {
                        st.notifications_changed_at = Some(Utc::now());
                        st.notification_evidence = Default::default();
                    }
                    if st.disabled != s.disabled {
                        st.quota_epoch += 1;
                    }
                    st.disabled = s.disabled;
                    st.notifications_enabled = cfg.notifications.enabled;
                    if !st.notifications_enabled {
                        st.notification_evidence = Default::default();
                    }
                    if identity_changed {
                        st.quota_epoch += 1;
                        st.quota_refreshing = false;
                        st.quota = Default::default();
                        st.notification_evidence = Default::default();
                        st.quota_cooldowns.clear();
                        st.banked_resets = None;
                    }
                    drop(st);
                    next.push(prev.clone());
                    continue;
                }
            }
            let discovered = old.get(&s.id).map(|p| p.discovered.read().clone()).unwrap_or_default();
            let mut state = old
                .get(&s.id)
                .filter(|previous| {
                    previous.provider == s.provider
                        && match (&*previous.cred.read(), &s.cred) {
                            (Credential::OAuth(old), Credential::OAuth(new)) => {
                                old.account_id == new.account_id && old.base_url == new.base_url
                            }
                            (Credential::ApiKey { .. }, Credential::ApiKey { .. }) => true,
                            _ => false,
                        }
                })
                .map(|p| {
                    let st = p.state.lock();
                    AccountState {
                        active_requests: st.active_requests.clone(),
                        cooldowns: st.cooldowns.clone(),
                        quota_cooldowns: st.quota_cooldowns.clone(),
                        strikes: st.strikes,
                        last_error: st.last_error.clone(),
                        last_used: st.last_used,
                        counters: st.counters.clone(),
                        quota: st.quota.clone(),
                        quota_epoch: st.quota_epoch + u64::from(st.disabled != s.disabled),
                        notification_evidence: if st.disabled != s.disabled
                            || st.notifications_enabled != cfg.notifications.enabled
                        {
                            Default::default()
                        } else {
                            st.notification_evidence.clone()
                        },
                        notifications_changed_at: if st.disabled != s.disabled
                            || st.notifications_enabled != cfg.notifications.enabled
                        {
                            Some(Utc::now())
                        } else {
                            st.notifications_changed_at
                        },
                        ..Default::default()
                    }
                })
                .unwrap_or_default();
            state.disabled = s.disabled;
            state.notifications_enabled = cfg.notifications.enabled;
            next.push(Arc::new(Account {
                id: s.id,
                provider: s.provider,
                label: s.label,
                path: s.path,
                group: s.group,
                models: s.models,
                headers: s.headers,
                proxy_url: s.proxy_url,
                cred: RwLock::new(s.cred),
                state: Mutex::new(state),
                refresh_lock: tokio::sync::Mutex::new(()),
                device_id: s.device_id.unwrap_or_else(|| random_hex(32)),
                session_id: uuid::Uuid::new_v4().to_string(),
                discovered: RwLock::new(discovered),
                prefix: s.prefix,
                excluded: s.excluded,
                aliases: s.aliases,
            }));
        }
        // Another program (CLIProxyAPI itself) may be rewriting a credential file
        // right now; keep the account rather than dropping it for a moment.
        for (id, prev) in &old {
            if let Some(p) = &prev.path
                && p.exists()
                && !next.iter().any(|a| a.id == *id)
                && read_oauth_file(p).is_none()
            {
                next.push(prev.clone());
            }
        }
        *self.accounts.write() = next;
    }

    /// The model name some account actually serves, for names that are close:
    /// `gemini-3-8-flash` -> `gemini-3.8-flash`, `google-gemini-...`, `-preview`,
    /// and tiered providers (Antigravity) -> `...-high`.
    pub fn canonical(&self, model: &str, only: Option<&Only>) -> String {
        let force = self.force_prefix.load(std::sync::atomic::Ordering::Relaxed);
        let accounts = self.accounts.read();
        let usable: Vec<&Arc<Account>> = accounts
            .iter()
            .filter(|a| match only {
                Some(Only::Provider(p)) => *p == a.provider,
                Some(Only::Prefix(x)) => a.prefix.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(x)),
                None => !(force && a.prefix.is_some()),
            })
            .filter(|a| !a.state.lock().disabled)
            .collect();
        // A listed model beats one that only matches a family (`gpt-*` takes anything).
        let listed: Vec<String> = usable.iter().flat_map(|a| a.public_models()).collect();
        let known = |m: &str| listed.iter().any(|l| l.eq_ignore_ascii_case(m));
        let forced = matches!(only, Some(Only::Provider(_)));
        let served = |m: &str| usable.iter().any(|a| a.resolve_with(m, forced).is_some());
        if known(model) {
            return model.to_string();
        }
        let mut bases = vec![model.to_ascii_lowercase()];
        for strip in ["google-", "google/", "models/", "anthropic/", "openai/"] {
            if let Some(rest) = bases[0].strip_prefix(strip) {
                bases.push(rest.to_string());
            }
        }
        for b in bases.clone() {
            bases.push(dot_versions(&b));
        }
        for b in bases.clone() {
            if let Some(rest) = b.strip_suffix("-preview") {
                bases.push(rest.to_string());
            }
        }
        let candidates: Vec<String> =
            bases.iter().flat_map(|b| ["", "-high", "-medium", "-low"].map(|t| format!("{b}{t}"))).collect();
        if let Some(c) = candidates.iter().find(|c| known(c)) {
            return c.clone();
        }
        if served(model) {
            return model.to_string();
        }
        candidates.into_iter().find(|c| served(c)).unwrap_or_else(|| model.to_string())
    }

    pub fn all(&self) -> Vec<Arc<Account>> {
        self.accounts.read().clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Account>> {
        self.accounts.read().iter().find(|a| a.id == id).cloned()
    }

    /// Public models served by at least one enabled account (prefixed ones as `prefix/model`).
    pub fn models(&self) -> Vec<(String, Provider)> {
        let force = self.force_prefix.load(std::sync::atomic::Ordering::Relaxed);
        let mut seen = std::collections::BTreeMap::new();
        for a in self.accounts.read().iter() {
            if a.state.lock().disabled {
                continue;
            }
            for m in a.public_models() {
                if let Some(p) = &a.prefix {
                    seen.entry(format!("{p}/{m}")).or_insert(a.provider);
                    if force {
                        continue;
                    }
                }
                seen.entry(m).or_insert(a.provider);
            }
        }
        seen.into_iter().collect()
    }

    /// Splits an optional `provider/` prefix off a model name (`antigravity/claude-sonnet-4-6`).
    /// Names that some account serves verbatim (`openai/gpt-oss` on OpenRouter) are left alone.
    pub fn route(&self, model: &str) -> (Option<Only>, String) {
        if let Some((prefix, rest)) = model.split_once('/')
            && !rest.is_empty()
        {
            let accounts = self.accounts.read();
            if accounts.iter().any(|a| a.prefix.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(prefix))) {
                return (Some(Only::Prefix(prefix.to_string())), rest.to_string());
            }
            if let Some(p) = Provider::parse(prefix)
                && !accounts.iter().any(|a| a.resolve(model).is_some())
            {
                return (Some(Only::Provider(p)), rest.to_string());
            }
        }
        (None, model.to_string())
    }

    pub fn pick(
        &self,
        model: &str,
        exclude: &[String],
        cfg: &Config,
        pinned: Option<&str>,
        only: Option<&Only>,
        session_load: &HashMap<String, usize>,
    ) -> Pick {
        let force = self.force_prefix.load(std::sync::atomic::Ordering::Relaxed);
        let accounts = self.accounts.read();
        let mut candidates: Vec<(&Arc<Account>, String)> = Vec::new();
        let mut earliest: Option<DateTime<Utc>> = None;
        for a in accounts.iter() {
            let allowed = match only {
                Some(Only::Provider(p)) => *p == a.provider,
                Some(Only::Prefix(x)) => a.prefix.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(x)),
                None => !(force && a.prefix.is_some()),
            };
            if !allowed || exclude.contains(&a.id) || a.state.lock().disabled {
                continue;
            }
            let forced = matches!(only, Some(Only::Provider(_)));
            let Some(upstream) = a.resolve_with(model, forced) else { continue };
            if let Some(until) = a.cooling_until(model) {
                earliest = Some(earliest.map_or(until, |e| e.min(until)));
                continue;
            }
            candidates.push((a, upstream));
        }
        if candidates.is_empty() {
            return earliest.map(Pick::Cooling).unwrap_or(Pick::None);
        }
        if let Some(pin) = pinned
            && let Some((a, m)) = candidates.iter().find(|(a, _)| a.id == pin)
        {
            return Pick::Ok((*a).clone(), m.clone());
        }
        // Prefer the model vendor for new assignments, while preserving existing overflow pins.
        if candidates.iter().any(|(a, _)| a.first_party(model)) {
            candidates.retain(|(a, _)| a.first_party(model));
        }
        let idx = match cfg.routing {
            Routing::FillFirst => 0,
            Routing::SmartQuota => {
                let now = Utc::now();
                let scores: Vec<_> = candidates
                    .iter()
                    .map(|(a, upstream)| {
                        let st = a.state.lock();
                        let remaining = st.quota.five_hour_remaining(upstream);
                        // Earlier weekly renewals get up to twice the weight, never unlimited
                        // priority. Missing/expired weekly dates receive the neutral weight.
                        let renewal = st.quota.weekly_reset(upstream).map_or(1.0, |reset| {
                            2.0 - ((reset - now).num_seconds() as f64 / (7.0 * 86_400.0)).clamp(0.0, 1.0)
                        });
                        // A busy session is already represented by its in-flight request;
                        // take the larger count instead of charging that work twice.
                        let load = session_load
                            .get(&a.id)
                            .copied()
                            .unwrap_or(0)
                            .max(st.active_requests.load(Ordering::Relaxed));
                        // Use known weekly/model-specific headroom when no 5-hour window is
                        // reported. The reserve below still applies only to real 5-hour data.
                        let headroom =
                            remaining.or_else(|| st.quota.remaining(upstream)).unwrap_or(crate::quota::UNKNOWN);
                        // Sessions on an account nearly out of its weekly allowance would soon
                        // have to move and lose their cache: fade over the last quarter.
                        let weekly = st.quota.weekly_remaining(upstream).map_or(1.0, |left| (left / 25.0).min(1.0));
                        let weight = headroom * renewal * weekly / (1.0 + load as f64);
                        (remaining, weight)
                    })
                    .collect();
                let reserve = f64::from(cfg.five_hour_reserve_percent);
                let protect = reserve > 0.0 && scores.iter().any(|(left, _)| left.is_some_and(|v| v >= reserve));
                // This is an admission preference, not a cooldown: never refuse usable
                // accounts merely because all are below the reserve (or quota is unknown).
                let eligible = |i: usize| !protect || scores[i].0.is_some_and(|left| left >= reserve);
                let best = (0..scores.len()).filter(|&i| eligible(i)).map(|i| scores[i].1).fold(0.0, f64::max);
                let tied: Vec<_> = (0..scores.len()).filter(|&i| eligible(i) && scores[i].1 >= best - 1e-9).collect();
                let mut cur = self.cursor.lock();
                let c = cur.entry(model.to_ascii_lowercase()).or_insert(0);
                let i = tied[*c % tied.len()];
                *c = c.wrapping_add(1);
                i
            }
            Routing::LeastUsed => {
                // Most headroom in its tightest usage window; near-ties take turns.
                let scores: Vec<f64> = candidates
                    .iter()
                    .map(|(a, _)| a.state.lock().quota.pressure(model).unwrap_or(crate::quota::UNKNOWN))
                    .collect();
                let best = scores.iter().copied().fold(f64::INFINITY, f64::min);
                let tied: Vec<usize> = (0..candidates.len()).filter(|i| scores[*i] <= best + 2.0).collect();
                let mut cur = self.cursor.lock();
                let c = cur.entry(model.to_ascii_lowercase()).or_insert(0);
                let i = tied[*c % tied.len()];
                *c = c.wrapping_add(1);
                i
            }
            Routing::RoundRobin => {
                let mut cur = self.cursor.lock();
                let c = cur.entry(model.to_ascii_lowercase()).or_insert(0);
                let i = *c % candidates.len();
                *c = c.wrapping_add(1);
                i
            }
        };
        let (a, m) = &candidates[idx];
        Pick::Ok((*a).clone(), m.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_quota_prefers_earlier_renewals_and_respects_availability() {
        use crate::quota::{Quota, Window};
        use chrono::Duration;
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            claude_api_key: ["cybex", "itpals", "digitalbrain"]
                .map(|key| crate::config::KeyEntry { api_key: key.into(), ..Default::default() })
                .to_vec(),
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        let accounts = pool.accounts.read().clone();
        let now = Utc::now();
        for (account, (hours, used)) in accounts.iter().zip([(82, 33.0), (104, 71.0), (129, 16.0)]) {
            account.state.lock().quota = Quota {
                windows: vec![
                    Window { name: "week".into(), used, resets_at: Some(now + Duration::hours(hours)), model: None },
                    Window { name: "5h".into(), used: 0.0, resets_at: Some(now + Duration::hours(1)), model: None },
                ],
                updated_at: Some(now),
                ..Default::default()
            };
        }
        let pick = |routing, exclude: &[String], pinned| match pool.pick(
            "claude-sonnet-4-6",
            exclude,
            &Config { routing, ..cfg.clone() },
            pinned,
            None,
            &HashMap::new(),
        ) {
            Pick::Ok(a, _) => a.id.clone(),
            _ => panic!("expected available account"),
        };
        let strategy = Routing::SmartQuota;
        assert_eq!(pick(Routing::LeastUsed, &[], None), accounts[2].id);
        for _ in 0..4 {
            assert_eq!(pick(strategy, &[], None), accounts[0].id);
        }
        assert_eq!(pick(strategy, &[], Some(accounts[2].id.as_str())), accounts[2].id);
        assert_eq!(pick(strategy, &[accounts[0].id.clone()], None), accounts[1].id);
        accounts[0].state.lock().disabled = true;
        assert_eq!(pick(strategy, &[], None), accounts[1].id);
        accounts[0].state.lock().disabled = false;
        // A 5-hour limit blocks selection even when the weekly reset is earliest.
        accounts[0].state.lock().quota.windows[1].used = 100.0;
        assert_eq!(pick(strategy, &[], None), accounts[1].id);
        accounts[0].state.lock().quota.windows[1].used = 0.0;
        accounts[0].state.lock().quota.windows[0].used = 100.0;
        assert_eq!(pick(strategy, &[], None), accounts[1].id);
        accounts[0].state.lock().quota.windows[0].used = 33.0;
        // Past and unknown weekly resets must not beat a known future reset.
        accounts[0].state.lock().quota.windows[0].resets_at = Some(now - Duration::hours(1));
        accounts[2].state.lock().quota.windows[0].resets_at = None;
        assert_eq!(pick(strategy, &[], None), accounts[1].id);
        // Equal dates break ties by 5-hour headroom.
        accounts[1].state.lock().quota.windows[1].used = 10.0;
        accounts[0].state.lock().quota.windows[0].resets_at = Some(now + Duration::hours(104));
        assert_eq!(pick(strategy, &[], None), accounts[0].id);
        // Without weekly dates, use 5-hour headroom instead.
        accounts[0].state.lock().quota.windows[1].used = 20.0;
        for account in &accounts {
            account.state.lock().quota.windows[0].resets_at = None;
        }
        assert_eq!(pick(strategy, &[], None), accounts[2].id);
    }

    #[test]
    fn smart_quota_avoids_accounts_nearly_out_of_their_week() {
        use crate::quota::{Quota, Window};
        use chrono::Duration;
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            claude_api_key: ["a", "b"]
                .map(|key| crate::config::KeyEntry { api_key: key.into(), ..Default::default() })
                .to_vec(),
            routing: Routing::SmartQuota,
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        let accounts = pool.accounts.read().clone();
        let now = Utc::now();
        // An earlier renewal must not outweigh a week that is almost used up.
        for (account, (days, used)) in accounts.iter().zip([(2, 98.0), (6, 0.0)]) {
            account.state.lock().quota = Quota {
                windows: vec![
                    Window { name: "week".into(), used, resets_at: Some(now + Duration::days(days)), model: None },
                    Window { name: "5h".into(), used: 0.0, resets_at: Some(now + Duration::hours(1)), model: None },
                ],
                updated_at: Some(now),
                ..Default::default()
            };
        }
        for _ in 0..4 {
            match pool.pick("claude-sonnet-4-6", &[], &cfg, None, None, &HashMap::new()) {
                Pick::Ok(a, _) => assert_eq!(a.id, accounts[1].id),
                _ => panic!("expected available account"),
            }
        }
    }

    #[test]
    fn request_load_survives_account_reload_and_is_released() {
        let mut cfg = Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![crate::config::KeyEntry { api_key: "a".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        let old = pool.all()[0].clone();
        let request = RequestLoad::new(&old);
        cfg.codex_api_key[0].headers.insert("test-header".into(), "value".into());
        pool.reload(&cfg); // changed shape replaces the Arc<Account>
        let new = pool.all()[0].clone();
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(new.state.lock().active_requests.load(Ordering::Relaxed), 1);
        drop(request);
        assert_eq!(new.state.lock().active_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn aggregator_names_stay_with_aggregators() {
        assert!(Provider::Gemini.serves("gemini-3.8-flash"));
        assert!(!Provider::Gemini.serves("gemini-3.8-flash-high"));
        assert!(!Provider::Vertex.serves("gemini-pro-agent"));
        assert!(Provider::Antigravity.serves("gemini-3.8-flash-high"));
        assert!(!Provider::Codex.serves("gpt-5-6-sol"));
        assert!(Provider::Devin.serves("gpt-5-6-sol"));
        // Shared names belong to the vendor first.
        assert!(Provider::Claude.serves("claude-sonnet-4-6"));
        assert!(!Provider::Antigravity.first_party("claude-sonnet-4-6"));
        assert!(Provider::Antigravity.first_party("gemini-3.8-flash-high"));
        assert!(Provider::Kimi.serves("kimi-k3") && !Provider::Devin.first_party("kimi-k3"));
    }

    #[test]
    fn provider_prefix_routing() {
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            claude_api_key: vec![crate::config::KeyEntry { api_key: "k".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        let claude = Only::Provider(Provider::Claude);
        assert_eq!(pool.route("claude/claude-opus-5-5"), (Some(claude.clone()), "claude-opus-5-5".into()));
        assert_eq!(pool.route("moonshotai/kimi-k3"), (None, "moonshotai/kimi-k3".into()));
        assert!(matches!(pool.pick("claude-opus-5-5", &[], &cfg, None, Some(&claude), &HashMap::new()), Pick::Ok(..)));
        let codex = Only::Provider(Provider::Codex);
        assert!(matches!(pool.pick("claude-opus-5-5", &[], &cfg, None, Some(&codex), &HashMap::new()), Pick::None));
    }

    #[test]
    fn close_model_names_resolve() {
        assert_eq!(dot_versions("gemini-3-8-flash"), "gemini-3.8-flash");
        assert_eq!(dot_versions("gpt-6-1-sol"), "gpt-6.1-sol");
        assert_eq!(dot_versions("claude-haiku-4-5-20251001"), "claude-haiku-4.5-20251001");
        assert_eq!(dot_versions("gpt-5.5"), "gpt-5.5");
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![crate::config::KeyEntry { api_key: "k".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        assert_eq!(pool.canonical("gpt-6-1-sol", None), "gpt-6.1-sol");
        assert_eq!(pool.canonical("openai/gpt-6-astra", None), "gpt-6-astra");
        assert_eq!(pool.canonical("nothing-like-it", None), "nothing-like-it");
    }

    #[test]
    fn prefixes_exclusions_and_aliases() {
        assert!(wildcard("gemini-2.5-*", "gemini-2.5-pro"));
        assert!(wildcard("*-preview", "gemini-3-pro-preview"));
        assert!(wildcard("*flash*", "gemini-2.5-flash-lite"));
        assert!(!wildcard("*flash*", "gemini-2.5-pro"));
        assert!(wildcard("claude-opus-5-5", "CLAUDE-OPUS-5-5"));

        let text = r#"
auth-dir: /nonexistent
force-model-prefix: true
claude-api-key:
  - api-key: team-key
    prefix: team
  - api-key: plain-key
    excluded-models: ["claude-opus-*"]
"#;
        let cfg = Config::parse(text).unwrap();
        let pool = Pool::default();
        pool.reload(&cfg);
        let team = Only::Prefix("team".into());
        assert_eq!(pool.route("team/claude-opus-5-5"), (Some(team.clone()), "claude-opus-5-5".into()));
        let Pick::Ok(a, _) = pool.pick("claude-opus-5-5", &[], &cfg, None, Some(&team), &HashMap::new()) else {
            panic!()
        };
        assert_eq!(a.prefix.as_deref(), Some("team"));
        // Unprefixed: the team key is reserved and the plain key excludes opus.
        assert!(matches!(pool.pick("claude-opus-5-5", &[], &cfg, None, None, &HashMap::new()), Pick::None));
        assert!(matches!(pool.pick("claude-sonnet-5-5", &[], &cfg, None, None, &HashMap::new()), Pick::Ok(..)));
        let models: Vec<String> = pool.models().into_iter().map(|(m, _)| m).collect();
        assert!(models.contains(&"team/claude-opus-5-5".to_string()));
        assert!(!models.contains(&"claude-opus-5-5".to_string()));
    }

    #[test]
    fn oauth_aliases_rename_models() {
        let a = |fork| crate::config::OAuthAlias { name: "claude-opus-5-5".into(), alias: "opus".into(), fork };
        let acct = |aliases| Account {
            id: "x".into(),
            provider: Provider::Claude,
            label: "x".into(),
            path: None,
            group: None,
            models: vec![],
            headers: BTreeMap::new(),
            proxy_url: None,
            cred: RwLock::new(Credential::ApiKey { key: "k".into(), base_url: None }),
            state: Mutex::new(AccountState::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
            device_id: String::new(),
            session_id: String::new(),
            discovered: RwLock::new(vec![]),
            prefix: None,
            excluded: vec![],
            aliases,
        };
        let renamed = acct(vec![a(false)]);
        assert_eq!(renamed.resolve("opus").as_deref(), Some("claude-opus-5-5"));
        assert!(renamed.resolve("claude-opus-5-5").is_none());
        assert!(renamed.public_models().contains(&"opus".to_string()));
        let forked = acct(vec![a(true)]);
        assert_eq!(forked.resolve("claude-opus-5-5").as_deref(), Some("claude-opus-5-5"));
    }
}
