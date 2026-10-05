//! Shared coding-session assignments. Only identifiers and account ids are persisted;
//! conversation input stays in a bounded, process-local continuation cache.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::HeaderMap;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::{Account, Only, PROVIDERS, Pick, Pool, Provider};
use crate::config::{Config, Routing};

const MAX_SESSIONS: usize = 10_000;
const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSES: usize = 1_000;
/// Recent assignments count immediately, including the gaps between coding turns.
const LOAD_IDLE_SECONDS: i64 = 5 * 60;

fn digest(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim).filter(|s| !s.is_empty())
}

fn identifier(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 1024)
}

/// Authentication middleware overwrites this scope, including for query-string keys.
pub fn client_scope(headers: &HeaderMap) -> String {
    if let Some(scope) = header(headers, "x-cliproxy-client-scope") {
        return scope.to_string();
    }
    let key = header(headers, "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| header(headers, "x-api-key"))
        .or_else(|| header(headers, "x-goog-api-key"))
        .unwrap_or("anonymous");
    digest(&["client", key])
}

pub fn scope_for_key(key: Option<&str>) -> String {
    digest(&["client", key.unwrap_or("anonymous")])
}

pub struct SessionIdentity {
    pub key: String,
    pub source: &'static str,
}

/// Read the original client request before provider translation or Claude cloaking.
/// The source identifies a field; the raw client identifier is never returned.
pub fn session_identity(headers: &HeaderMap, body: &Value) -> Option<SessionIdentity> {
    let claude_user = body["metadata"]["user_id"].as_str().unwrap_or_default();
    let claude_json: Value = serde_json::from_str(claude_user).unwrap_or(Value::Null);
    let (id, source) = [
        "x-cliproxy-session-id",
        "thread-id",
        "x-codex-thread-id",
        "session_id",
        "session-id",
        "x-claude-code-session-id",
    ]
    .into_iter()
    .find_map(|name| header(headers, name).filter(|s| s.len() <= 1024).map(|id| (id, name)))
    .or_else(|| identifier(&claude_json["session_id"]).map(|id| (id, "metadata.user_id.session_id")))
    .or_else(|| {
        claude_user
            .strip_prefix("user_")
            .filter(|s| s.contains("_account_"))?
            .rsplit_once("_session_")
            .map(|(_, s)| s)
            .filter(|s| !s.is_empty() && s.len() <= 1024)
            .map(|id| (id, "metadata.user_id"))
    })
    .or_else(|| identifier(&body["metadata"]["session_id"]).map(|id| (id, "metadata.session_id")))
    .or_else(|| identifier(&body["metadata"]["thread_id"]).map(|id| (id, "metadata.thread_id")))
    .or_else(|| identifier(&body["conversation"]["id"]).map(|id| (id, "conversation.id")))
    .or_else(|| identifier(&body["conversation"]).map(|id| (id, "conversation")))
    .or_else(|| identifier(&body["prompt_cache_key"]).map(|id| (id, "prompt_cache_key")))?;
    Some(SessionIdentity { key: digest(&[&client_scope(headers), "session", id]), source })
}

pub fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    session_identity(headers, body).map(|identity| identity.key)
}

pub fn connection_key(headers: &HeaderMap, id: &str) -> String {
    digest(&[&client_scope(headers), "connection", id])
}

// Keep a subscription across model changes within a provider, while respecting
// explicit account prefixes and intentional switches to another provider.
fn route_scope(pool: &Pool, model: &str, only: Option<&Only>) -> String {
    match only {
        Some(Only::Prefix(p)) => format!("prefix:{}", p.to_ascii_lowercase()),
        Some(Only::Provider(p)) => format!("provider:{}", p.as_str()),
        None => {
            let vendor = |name: &str| {
                PROVIDERS
                    .iter()
                    .find(|p| {
                        !matches!(p, Provider::Antigravity | Provider::Devin | Provider::Compat) && p.family(name)
                    })
                    .copied()
            };
            vendor(model)
                .or_else(|| {
                    pool.all().iter().find_map(|a| pool.resolve_account(a, model, None).and_then(|m| vendor(&m)))
                })
                .map(|p| format!("provider:{}", p.as_str()))
                .unwrap_or_else(|| format!("model:{}", model.to_ascii_lowercase()))
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    #[serde(default)]
    session: String,
    account: String,
    last_seen: i64,
}

struct Conversation {
    session: String,
    input: Vec<Value>,
    bytes: usize,
    last_seen: i64,
}

#[derive(Default)]
struct Registry {
    bindings: HashMap<String, Binding>,
    responses: HashMap<String, Conversation>,
    history_bytes: usize,
    dirty: bool,
    last_flush: i64,
    active: HashMap<String, usize>,
}

impl Registry {
    fn account_load(&self, cfg: &Config) -> HashMap<String, usize> {
        let mut load = HashMap::new();
        if !weighs_session_load(cfg) {
            return load;
        }
        let cutoff = Utc::now().timestamp()
            - LOAD_IDLE_SECONDS.min(cfg.session_affinity_idle_seconds.min(i64::MAX as u64) as i64);
        let mut seen = HashSet::new();
        for (key, b) in &self.bindings {
            let session = if b.session.is_empty() { key.as_str() } else { b.session.as_str() };
            if (b.last_seen > cutoff || self.active.contains_key(session)) && seen.insert((&b.account, session)) {
                *load.entry(b.account.clone()).or_insert(0) += 1;
            }
        }
        load
    }
}

fn weighs_session_load(cfg: &Config) -> bool {
    cfg.routing == Routing::SmartQuota && cfg.session_affinity
}

pub struct Sessions {
    registry: Mutex<Registry>,
    path: Option<PathBuf>,
}

/// Keep an assignment alive until a call (including its response stream) ends.
pub struct Lease {
    sessions: Arc<Sessions>,
    session: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut registry = self.sessions.registry.lock();
        if let Some(count) = registry.active.get_mut(&self.session) {
            *count -= 1;
            if *count == 0 {
                registry.active.remove(&self.session);
            }
        }
        for binding in registry.bindings.values_mut().filter(|b| b.session == self.session) {
            binding.last_seen = Utc::now().timestamp();
        }
        registry.dirty = true;
    }
}

pub type Selection = Result<(Arc<Account>, String), (u16, String)>;

#[derive(Clone)]
pub struct Selected {
    pub account: Arc<Account>,
    pub model: String,
    pub strategy: Routing,
    pub reason: &'static str,
    pub previous_account: Option<String>,
}

fn selection(pick: Pick, model: &str) -> Selection {
    match pick {
        Pick::Ok(a, m) => Ok((a, m)),
        Pick::Cooling(t) => Err((
            429,
            format!(
                "all accounts for {model} are rate limited; next available in {}s",
                (t - Utc::now()).num_seconds().max(1)
            ),
        )),
        Pick::None => Err((404, format!("no available account serves model `{model}`"))),
    }
}

impl Sessions {
    pub fn hold(self: &Arc<Self>, session: &str, idle: u64) -> Lease {
        let session = digest(&["owner", session]);
        let mut registry = self.registry.lock();
        Self::prune_locked(&mut registry, idle);
        *registry.active.entry(session.clone()).or_default() += 1;
        Lease { sessions: self.clone(), session }
    }

    pub fn end(&self, session: &str) {
        let owner = digest(&["owner", session]);
        let mut registry = self.registry.lock();
        registry.bindings.retain(|_, b| b.session != owner);
        registry.responses.retain(|_, c| c.session != session);
        registry.history_bytes = registry.responses.values().map(|c| c.bytes).sum();
        registry.dirty = true;
        self.flush_locked(&mut registry);
    }
    pub fn load(auth_dir: &Path, idle: u64) -> Self {
        let path = auth_dir.join(".routing-sessions.state");
        let mut registry = Registry::default();
        match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice(&data) {
                Ok(bindings) => registry.bindings = bindings,
                Err(e) => tracing::warn!(path = %path.display(), "session assignments could not be read: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), "session assignments could not be read: {e}"),
        }
        let sessions = Self { registry: Mutex::new(registry), path: Some(path) };
        sessions.prune(idle);
        sessions
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Self { registry: Mutex::new(Registry::default()), path: None }
    }

    pub fn pick(
        &self,
        pool: &Pool,
        cfg: &Config,
        model: &str,
        session: Option<&str>,
        exclude: &[String],
        only: Option<&Only>,
    ) -> Selection {
        self.pick_with_reason(pool, cfg, model, session, exclude, only)
            .map(|selected| (selected.account, selected.model))
    }

    /// Capture the routing reason while selecting under one lock. Concurrent calls
    /// cannot create competing assignments or report a stale migration decision.
    pub fn pick_with_reason(
        &self,
        pool: &Pool,
        cfg: &Config,
        model: &str,
        session: Option<&str>,
        exclude: &[String],
        only: Option<&Only>,
    ) -> Result<Selected, (u16, String)> {
        let Some(session) = session.filter(|_| cfg.session_affinity) else {
            let (account, model) = selection(self.pick_unbound(pool, cfg, model, exclude, None, only), model)?;
            return Ok(Selected {
                account,
                model,
                strategy: cfg.routing,
                reason: if cfg.session_affinity { "missing_session" } else { "affinity_disabled" },
                previous_account: None,
            });
        };
        let key = digest(&[session, &route_scope(pool, model, only)]);
        let mut registry = self.registry.lock();
        Self::prune_locked(&mut registry, cfg.session_affinity_idle_seconds);
        let mut excluded = exclude.to_vec();
        // Why an existing assignment has to move, if it does.
        let mut moving: Option<&'static str> = None;
        if let Some(binding) = registry.bindings.get_mut(&key) {
            binding.last_seen = Utc::now().timestamp();
            let assigned = pool.get(&binding.account);
            let upstream = assigned.as_ref().and_then(|acct| pool.resolve_account(acct, model, only));
            match (&assigned, upstream) {
                (None, _) => moving = Some("account_removed"),
                (Some(acct), _) if acct.state.lock().disabled => moving = Some("account_disabled"),
                (Some(_), None) => moving = Some("model_unavailable"),
                (Some(acct), Some(_)) if acct.exhausted_until(model).is_some() => moving = Some("quota_exhausted"),
                (Some(acct), Some(upstream)) => {
                    if acct.cooling_until(model).is_none() && !excluded.contains(&acct.id) {
                        registry.dirty = true;
                        return Ok(Selected {
                            account: acct.clone(),
                            model: upstream,
                            strategy: cfg.routing,
                            reason: "session_reused",
                            previous_account: None,
                        });
                    }
                    // A rate limit or failed attempt is temporary: serve this request from
                    // another account but keep the assignment, so the session comes back
                    // (and finds its prompt cache) once its subscription recovers.
                    let assigned_id = acct.id.clone();
                    if !excluded.contains(&assigned_id) {
                        excluded.push(assigned_id.clone());
                    }
                    let (account, model) =
                        selection(pool.pick(model, &excluded, cfg, None, only, &registry.account_load(cfg)), model)?;
                    return Ok(Selected {
                        account,
                        model,
                        strategy: cfg.routing,
                        reason: "temporary_detour",
                        previous_account: Some(assigned_id),
                    });
                }
            }
            if let Some(acct) = &assigned
                && !excluded.contains(&acct.id)
            {
                excluded.push(acct.id.clone());
            }
        } else if registry.bindings.len() >= MAX_SESSIONS {
            // Make room by forgetting the longest-idle assignment with nothing in flight.
            let oldest = registry
                .bindings
                .iter()
                .filter(|(_, b)| !registry.active.contains_key(&b.session))
                .min_by_key(|(_, b)| b.last_seen)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(oldest) => {
                    registry.bindings.remove(&oldest);
                }
                // Every assignment is in use: route this request on its own rather than fail it.
                None => {
                    let (account, model) =
                        selection(pool.pick(model, exclude, cfg, None, only, &registry.account_load(cfg)), model)?;
                    return Ok(Selected {
                        account,
                        model,
                        strategy: cfg.routing,
                        reason: "missing_session",
                        previous_account: None,
                    });
                }
            }
        }
        let (acct, upstream) =
            selection(pool.pick(model, &excluded, cfg, None, only, &registry.account_load(cfg)), model)?;
        let previous = registry.bindings.insert(
            key,
            Binding {
                session: digest(&["owner", session]),
                account: acct.id.clone(),
                last_seen: Utc::now().timestamp(),
            },
        );
        if let Some(old) = &previous {
            tracing::info!(from = %old.account, to = %acct.id, reason = moving.unwrap_or("quota_exhausted"), "session moved to another account");
        }
        registry.dirty = true;
        self.flush_locked(&mut registry);
        Ok(Selected {
            account: acct,
            model: upstream,
            strategy: cfg.routing,
            reason: if previous.is_some() { moving.unwrap_or("quota_exhausted") } else { "new_session" },
            previous_account: previous.map(|binding| binding.account),
        })
    }

    pub fn prune(&self, idle: u64) {
        let mut registry = self.registry.lock();
        Self::prune_locked(&mut registry, idle);
    }

    /// Retries and requests without affinity still respect the reserve and current load.
    pub fn pick_unbound(
        &self,
        pool: &Pool,
        cfg: &Config,
        model: &str,
        exclude: &[String],
        pinned: Option<&str>,
        only: Option<&Only>,
    ) -> Pick {
        // Only smart quota balancing weighs session load; other strategies skip the registry.
        let load = if weighs_session_load(cfg) { self.registry.lock().account_load(cfg) } else { HashMap::new() };
        pool.pick(model, exclude, cfg, pinned, only, &load)
    }

    fn prune_locked(registry: &mut Registry, idle: u64) {
        let cutoff = Utc::now().timestamp().saturating_sub(idle.clamp(1, i64::MAX as u64) as i64);
        let count = registry.bindings.len();
        registry.bindings.retain(|_, b| b.last_seen > cutoff || registry.active.contains_key(&b.session));
        registry.dirty |= count != registry.bindings.len();
        registry.responses.retain(|_, c| c.last_seen > cutoff);
        registry.history_bytes = registry.responses.values().map(|c| c.bytes).sum();
    }

    pub fn flush(&self, idle: u64) {
        let mut registry = self.registry.lock();
        Self::prune_locked(&mut registry, idle);
        if Utc::now().timestamp() - registry.last_flush >= 30 {
            self.flush_locked(&mut registry);
        }
    }

    pub fn save(&self) {
        self.flush_locked(&mut self.registry.lock());
    }

    fn flush_locked(&self, registry: &mut Registry) {
        if !registry.dirty {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        let write = || -> anyhow::Result<()> {
            use std::io::Write;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let result = (|| -> anyhow::Result<()> {
                let mut file = opts.open(&temporary)?;
                file.write_all(&serde_json::to_vec(&registry.bindings)?)?;
                file.sync_all()?;
                std::fs::rename(&temporary, path)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        };
        match write() {
            Ok(()) => registry.dirty = false,
            Err(e) => tracing::warn!(path = %path.display(), "session assignments could not be persisted: {e}"),
        }
        registry.last_flush = Utc::now().timestamp();
    }

    pub fn previous(&self, headers: &HeaderMap, id: &str, session: Option<&str>) -> Option<(String, Vec<Value>)> {
        let key = digest(&[&client_scope(headers), "response", id]);
        let mut registry = self.registry.lock();
        let previous = registry.responses.get_mut(&key)?;
        if session.is_some_and(|s| s != previous.session) {
            return None;
        }
        previous.last_seen = Utc::now().timestamp();
        Some((previous.session.clone(), previous.input.clone()))
    }

    pub fn remember(&self, headers: &HeaderMap, session: &str, response: &Value, full_input: &[Value]) {
        let Some(id) = response["id"].as_str() else {
            return;
        };
        let Some(output) = response["output"].as_array() else {
            return;
        };
        let mut input = full_input.to_vec();
        input.extend(output.iter().cloned());
        let bytes = serde_json::to_vec(&input).map(|v| v.len()).unwrap_or(MAX_RESPONSE_BYTES + 1);
        if bytes > MAX_RESPONSE_BYTES {
            return;
        }
        let key = digest(&[&client_scope(headers), "response", id]);
        let mut registry = self.registry.lock();
        if let Some(old) = registry.responses.remove(&key) {
            registry.history_bytes -= old.bytes;
        }
        while registry.responses.len() >= MAX_RESPONSES || registry.history_bytes + bytes > MAX_HISTORY_BYTES {
            let Some(oldest) = registry.responses.iter().min_by_key(|(_, c)| c.last_seen).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = registry.responses.remove(&oldest) {
                registry.history_bytes -= old.bytes;
            }
        }
        registry.history_bytes += bytes;
        registry
            .responses
            .insert(key, Conversation { session: session.into(), input, bytes, last_seen: Utc::now().timestamp() });
    }
}

/// Some Codex backends omit output from the final event after streaming it.
pub fn complete_output(response: &mut Value, aggregate: &crate::ir::Aggregate) {
    if response["output"].as_array().is_none_or(|items| items.is_empty()) {
        let rebuilt = crate::formats::responses::render_full(
            aggregate,
            response["model"].as_str().unwrap_or_default(),
            &crate::ir::Request::default(),
        );
        response["output"] = rebuilt["output"].clone();
    }
}

pub fn input_items(body: &Value) -> Vec<Value> {
    match &body["input"] {
        Value::String(s) => vec![
            serde_json::json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": s }] }),
        ],
        Value::Array(items) => items.clone(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{KeyEntry, Routing};
    use chrono::Duration;
    use serde_json::json;

    fn config(routing: Routing) -> Config {
        Config {
            auth_dir: "/nonexistent".into(),
            routing,
            codex_api_key: ["a", "b"].map(|key| KeyEntry { api_key: key.into(), ..Default::default() }).to_vec(),
            ..Default::default()
        }
    }

    fn smart_pool() -> (Config, Pool) {
        let cfg = config(Routing::SmartQuota);
        let pool = Pool::default();
        pool.reload(&cfg);
        let now = Utc::now();
        for (a, days) in pool.all().iter().zip([3, 5]) {
            a.state.lock().quota = crate::quota::Quota {
                windows: vec![
                    crate::quota::Window {
                        name: "5h".into(),
                        used: 0.0,
                        resets_at: Some(now + Duration::hours(4)),
                        model: None,
                    },
                    crate::quota::Window {
                        name: "week".into(),
                        used: 30.0,
                        resets_at: Some(now + Duration::days(days)),
                        model: None,
                    },
                ],
                updated_at: Some(now),
                ..Default::default()
            };
        }
        (cfg, pool)
    }

    #[test]
    fn smart_reserve_protects_existing_sessions_and_never_blocks_available_quota() {
        let (cfg, pool) = smart_pool();
        let sessions = Sessions::memory();
        let pick = |task| sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some(task), &[], None).unwrap().0;
        let accounts = pool.all();
        let a = pick("first");
        assert_eq!(a.id, accounts[0].id);
        a.state.lock().quota.windows[0].used = 71.0; // 29% left
        let b = pick("second");
        assert_eq!(b.id, accounts[1].id);
        assert_eq!(pick("first").id, a.id); // reserve never evicts a pinned session
        b.state.lock().quota.windows[0].used = 90.0;
        assert_eq!(pick("third").id, a.id); // all below reserve: use the best available account
        a.state.lock().quota.windows[0].used = 100.0;
        assert_eq!(pick("first").id, b.id); // genuine exhaustion migrates
        a.state.lock().quota.windows[0].used = 0.0;
        assert_eq!(pick("first").id, b.id); // recovery does not disrupt the replacement
    }

    #[test]
    fn smart_reserve_admits_idle_accounts_and_releases_after_completion_or_inactivity() {
        for window in [0, 1] {
            let (cfg, pool) = smart_pool();
            let accounts = pool.all();
            accounts[0].state.lock().quota.windows[window].used = if window == 0 { 80.0 } else { 96.0 };
            let sessions = Arc::new(Sessions::memory());
            let pick = |task| sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some(task), &[], None).unwrap().0;
            assert_eq!(pick("first").id, accounts[0].id); // idle account can consume its reserve
            assert_eq!(pick("second").id, accounts[1].id); // protect the first session from new work
            assert_eq!(pick("first").id, accounts[0].id); // the existing session keeps its cache

            let lease = sessions.hold("first", cfg.session_affinity_idle_seconds);
            sessions
                .registry
                .lock()
                .bindings
                .values_mut()
                .filter(|b| b.account == accounts[0].id)
                .for_each(|b| b.last_seen -= LOAD_IDLE_SECONDS + 1);
            assert_eq!(pick("while-streaming").id, accounts[1].id); // long streams remain active
            drop(lease);
            sessions.end("first");
            assert_eq!(pick("after-completion").id, accounts[0].id); // explicit completion releases the reserve
            assert_eq!(pick("after-admission").id, accounts[1].id);

            sessions
                .registry
                .lock()
                .bindings
                .values_mut()
                .filter(|b| b.account == accounts[0].id)
                .for_each(|b| b.last_seen -= LOAD_IDLE_SECONDS + 1);
            assert_eq!(pick("after-inactivity").id, accounts[0].id); // old pins do not reserve quota
            assert_eq!(pick("after-completion").id, accounts[0].id); // the older affinity survives inactivity

            let unbound = Sessions::memory();
            let request = crate::accounts::RequestLoad::new(&accounts[0]);
            assert_eq!(unbound.pick(&pool, &cfg, "gpt-6.1-sol", None, &[], None).unwrap().0.id, accounts[1].id);
            drop(request);
            assert_eq!(unbound.pick(&pool, &cfg, "gpt-6.1-sol", None, &[], None).unwrap().0.id, accounts[0].id);
        }
    }

    #[test]
    fn smart_reserve_boundaries_unknown_quota_and_expired_windows() {
        let (mut cfg, pool) = smart_pool();
        let accounts = pool.all();
        let _request = crate::accounts::RequestLoad::new(&accounts[0]);
        let pick = |cfg: &Config| {
            Sessions::memory().pick(&pool, cfg, "gpt-6.1-sol", Some("new"), &[], None).unwrap().0.id.clone()
        };
        accounts[0].state.lock().quota.windows[0].used = 61.0;
        accounts[1].state.lock().quota.windows[0].used = 60.0;
        cfg.five_hour_reserve_percent = 40;
        assert_eq!(pick(&cfg), accounts[1].id);
        cfg.five_hour_reserve_percent = 0;
        assert_eq!(pick(&cfg), accounts[0].id);
        cfg.five_hour_reserve_percent = 30;
        accounts[0].state.lock().quota.windows[0].used = 70.0;
        accounts[1].state.lock().quota.windows[0].used = 70.0;
        assert_eq!(pick(&cfg), accounts[0].id); // exactly the reserve remains eligible
        accounts[0].state.lock().quota.windows.remove(0);
        assert_eq!(pick(&cfg), accounts[0].id); // no five-hour reserve is invented for missing data
        accounts[1].state.lock().quota.windows.remove(0);
        assert_eq!(pick(&cfg), accounts[0].id); // both unknown: renewal priority still works
        accounts[0].state.lock().quota.windows.push(crate::quota::Window {
            name: "5h".into(),
            used: 100.0,
            resets_at: Some(Utc::now() - Duration::seconds(1)),
            model: None,
        });
        assert_eq!(pick(&cfg), accounts[0].id); // elapsed window is available again
        cfg.five_hour_reserve_percent = 100;
        assert_eq!(pick(&cfg), accounts[0].id);
    }

    #[test]
    fn simultaneous_smart_assignments_pack_the_earliest_healthy_week() {
        let (cfg, pool) = smart_pool();
        let sessions = Sessions::memory();
        let assign = |sessions: &Sessions| {
            let barrier = std::sync::Barrier::new(24);
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..24)
                    .map(|i| {
                        let (cfg, pool, barrier) = (&cfg, &pool, &barrier);
                        scope.spawn(move || {
                            let task = format!("parallel-{i}");
                            barrier.wait();
                            let account = sessions.pick(pool, cfg, "gpt-6.1-sol", Some(&task), &[], None).unwrap().0;
                            (task, account.id.clone())
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
            })
        };
        let assignments = assign(&sessions);
        let accounts = pool.all();
        let early = assignments.iter().filter(|(_, id)| *id == accounts[0].id).count();
        assert_eq!(early, assignments.len(), "session count must not outweigh the earlier reset");
        accounts[0].state.lock().quota.windows[1].used = 96.0;
        assert_eq!(
            sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("after-draining"), &[], None).unwrap().0.id,
            accounts[1].id
        );
        for (task, id) in assignments {
            assert_eq!(sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some(&task), &[], None).unwrap().0.id, id);
        }
        let idle = Sessions::memory();
        let assignments = assign(&idle);
        assert_eq!(
            assignments.iter().filter(|(_, id)| *id == accounts[0].id).count(),
            1,
            "one new session may drain an idle reserve; later assignments must protect it"
        );
    }

    #[test]
    fn smart_routing_respects_model_specific_quota_through_an_alias() {
        let cfg = Config {
            routing: Routing::SmartQuota,
            auth_dir: "/nonexistent".into(),
            claude_api_key: ["a", "b"]
                .map(|key| KeyEntry {
                    api_key: key.into(),
                    models: vec![crate::config::ModelAlias {
                        name: "claude-opus-5-5".into(),
                        alias: Some("coding".into()),
                    }],
                    ..Default::default()
                })
                .to_vec(),
            ..Default::default()
        };
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        let pick = |task| sessions.pick(&pool, &cfg, "coding", Some(task), &[], None).unwrap().0;
        let a = pick("existing");
        a.state.lock().quota.windows.push(crate::quota::Window {
            name: "week opus".into(),
            used: 100.0,
            resets_at: Some(Utc::now() + Duration::days(1)),
            model: Some("opus".into()),
        });
        assert_ne!(pick("new").id, a.id);
        assert_ne!(pick("existing").id, a.id);
    }

    #[test]
    fn smart_session_load_deduplicates_scopes_and_releases_idle_or_ended_work() {
        let (mut cfg, pool) = smart_pool();
        let sessions = Arc::new(Sessions::memory());
        let a = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0;
        {
            let mut registry = sessions.registry.lock();
            let binding = registry.bindings.values().next().unwrap().clone();
            registry.bindings.insert("another-route-scope".into(), binding);
            assert_eq!(registry.account_load(&cfg)[&a.id], 1);
            registry.bindings.values_mut().for_each(|b| b.last_seen -= LOAD_IDLE_SECONDS + 1);
            assert!(registry.account_load(&cfg).is_empty());
        }
        let lease = sessions.hold("task", cfg.session_affinity_idle_seconds);
        assert_eq!(sessions.registry.lock().account_load(&cfg)[&a.id], 1); // long-running stream
        cfg.session_affinity = false;
        assert!(sessions.registry.lock().account_load(&cfg).is_empty());
        cfg.session_affinity = true;
        drop(lease);
        assert_eq!(sessions.registry.lock().account_load(&cfg)[&a.id], 1); // recent after completion
        sessions.end("task");
        assert!(sessions.registry.lock().account_load(&cfg).is_empty());
    }

    #[test]
    fn all_strategies_pin_and_keep_the_replacement_after_recovery() {
        for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
            let cfg = config(routing);
            let pool = Pool::default();
            pool.reload(&cfg);
            let sessions = Sessions::memory();
            let pick = || sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0;
            let a = pick();
            for _ in 0..5 {
                assert_eq!(pick().id, a.id);
            }
            // A model change inside the same provider also preserves the subscription.
            assert_eq!(sessions.pick(&pool, &cfg, "gpt-6-astra", Some("task"), &[], None).unwrap().0.id, a.id);
            a.exhaust("gpt-6.1-sol", Utc::now() + Duration::minutes(5), "quota exhausted");
            let b = pick();
            assert_ne!(a.id, b.id);
            a.state.lock().quota_cooldowns.clear();
            a.state.lock().cooldowns.clear();
            assert_eq!(pick().id, b.id);
        }
    }

    #[test]
    fn routing_metadata_distinguishes_assignment_reuse_and_quota_migration() {
        for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
            let cfg = config(routing);
            let pool = Pool::default();
            pool.reload(&cfg);
            let sessions = Sessions::memory();
            let pick = || sessions.pick_with_reason(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap();
            let first = pick();
            assert_eq!(first.reason, "new_session");
            assert_eq!(first.strategy, routing);
            assert_eq!(first.model, "gpt-6.1-sol");
            assert!(first.previous_account.is_none());

            let reused = pick();
            assert_eq!(reused.reason, "session_reused");
            assert_eq!(reused.account.id, first.account.id);
            assert!(reused.previous_account.is_none());

            first.account.exhaust("gpt-6.1-sol", Utc::now() + Duration::minutes(5), "quota exhausted");
            let migrated = pick();
            assert_eq!(migrated.reason, "quota_exhausted");
            assert_eq!(migrated.strategy, routing);
            assert_eq!(migrated.previous_account.as_deref(), Some(first.account.id.as_str()));
            assert_ne!(migrated.account.id, first.account.id);

            let next = pick();
            assert_eq!(next.reason, "session_reused");
            assert_eq!(next.account.id, migrated.account.id);
            assert!(next.previous_account.is_none());
        }
    }

    #[test]
    fn routing_metadata_explains_why_per_request_selection_is_used() {
        for (enabled, session, reason) in [
            (true, None, "missing_session"),
            (false, None, "affinity_disabled"),
            (false, Some("task"), "affinity_disabled"),
        ] {
            let cfg = Config { session_affinity: enabled, ..config(Routing::RoundRobin) };
            let pool = Pool::default();
            pool.reload(&cfg);
            let sessions = Sessions::memory();
            let pick = || sessions.pick_with_reason(&pool, &cfg, "gpt-6.1-sol", session, &[], None).unwrap();
            let first = pick();
            let second = pick();
            assert_eq!(first.reason, reason);
            assert_eq!(second.reason, reason);
            assert_eq!(first.strategy, Routing::RoundRobin);
            assert_ne!(first.account.id, second.account.id);
            assert!(first.previous_account.is_none());
            assert!(second.previous_account.is_none());
            assert!(sessions.registry.lock().bindings.is_empty());
        }
    }

    #[test]
    fn temporary_cooldowns_detour_and_disabled_accounts_move_the_session() {
        let cfg = config(Routing::RoundRobin);
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        let pick = || sessions.pick_with_reason(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap();
        let a = pick().account;
        // A rate limit serves the request elsewhere but keeps the assignment.
        a.cool(None, Utc::now() + Duration::seconds(60), "temporary");
        let detour = pick();
        assert_eq!(detour.reason, "temporary_detour");
        assert_ne!(detour.account.id, a.id);
        assert_eq!(detour.previous_account.as_deref(), Some(a.id.as_str()));
        a.state.lock().cooldowns.clear();
        assert_eq!(pick().account.id, a.id);
        // So does a failed attempt within one request.
        let retry = sessions
            .pick_with_reason(&pool, &cfg, "gpt-6.1-sol", Some("task"), std::slice::from_ref(&a.id), None)
            .unwrap();
        assert_eq!(retry.reason, "temporary_detour");
        assert_eq!(pick().account.id, a.id);
        // Disabling the account moves the session for good.
        a.state.lock().disabled = true;
        let moved = pick();
        assert_eq!(moved.reason, "account_disabled");
        assert_ne!(moved.account.id, a.id);
        a.state.lock().disabled = false;
        assert_eq!(pick().account.id, moved.account.id);
    }

    #[test]
    fn a_full_table_forgets_the_longest_idle_session_instead_of_failing() {
        let cfg = config(Routing::RoundRobin);
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        {
            let mut registry = sessions.registry.lock();
            let now = Utc::now().timestamp();
            for i in 0..MAX_SESSIONS {
                registry.bindings.insert(
                    format!("old-{i}"),
                    Binding {
                        session: format!("owner-{i}"),
                        account: "gone".into(),
                        last_seen: now - 20_000 + i as i64,
                    },
                );
            }
        }
        let selected = sessions.pick_with_reason(&pool, &cfg, "gpt-6.1-sol", Some("new-task"), &[], None).unwrap();
        assert_eq!(selected.reason, "new_session");
        let registry = sessions.registry.lock();
        assert_eq!(registry.bindings.len(), MAX_SESSIONS);
        assert!(!registry.bindings.contains_key("old-0"));
    }

    #[test]
    fn identifiers_are_client_scoped_and_claude_metadata_is_read() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer first".parse().unwrap());
        let body = json!({"metadata":{"user_id":json!({"session_id":"coding-task"}).to_string()}});
        let a = session_key(&headers, &body).unwrap();
        headers.insert("thread-id", "coding-task".parse().unwrap());
        assert_eq!(session_key(&headers, &json!({})).unwrap(), a);
        headers.insert("authorization", "Bearer second".parse().unwrap());
        assert_ne!(session_key(&headers, &body).unwrap(), a);
        assert!(session_key(&HeaderMap::new(), &json!({"metadata":{"user_id":"ordinary-user"}})).is_none());
        assert_eq!(
            session_key(&HeaderMap::new(), &json!({"metadata":{"user_id":"user_x_account_y_session_coding-task"}})),
            session_key(&HeaderMap::new(), &json!({"prompt_cache_key":"coding-task"}))
        );
    }

    #[test]
    fn identity_sources_preserve_existing_hashes_and_precedence() {
        let headers = HeaderMap::new();
        let expected = digest(&[&client_scope(&headers), "session", "task"]);
        for (source, body) in [
            ("metadata.user_id.session_id", json!({"metadata":{"user_id":r#"{"session_id":"task"}"#}})),
            ("metadata.user_id", json!({"metadata":{"user_id":"user_u_account_a_session_task"}})),
            ("metadata.session_id", json!({"metadata":{"session_id":"task"}})),
            ("metadata.thread_id", json!({"metadata":{"thread_id":"task"}})),
            ("conversation.id", json!({"conversation":{"id":"task"}})),
            ("conversation", json!({"conversation":"task"})),
            ("prompt_cache_key", json!({"prompt_cache_key":"task"})),
        ] {
            let identity = session_identity(&headers, &body).unwrap();
            assert_eq!(identity.source, source);
            assert_eq!(identity.key, expected);
            assert_eq!(session_key(&headers, &body).as_deref(), Some(expected.as_str()));
        }
        let mut headers = HeaderMap::new();
        for name in [
            "x-claude-code-session-id",
            "session-id",
            "session_id",
            "x-codex-thread-id",
            "thread-id",
            "x-cliproxy-session-id",
        ] {
            headers.insert(name, "task".parse().unwrap());
            let identity = session_identity(&headers, &json!({"prompt_cache_key":"lower-priority"})).unwrap();
            assert_eq!(identity.source, name);
            assert_eq!(identity.key, expected);
        }
        let invalid = "x".repeat(1025);
        let mut headers = HeaderMap::new();
        headers.insert("thread-id", invalid.parse().unwrap());
        let body = json!({"metadata":{"session_id":invalid},"prompt_cache_key":"task"});
        let identity = session_identity(&headers, &body).unwrap();
        assert_eq!(identity.source, "prompt_cache_key");
        assert_eq!(identity.key, expected);
        assert!(session_identity(&HeaderMap::new(), &json!({})).is_none());
    }

    #[test]
    fn round_robin_distributes_sessions_and_concurrent_calls_share_assignment() {
        let cfg = config(Routing::RoundRobin);
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        let a = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("one"), &[], None).unwrap().0;
        let b = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("two"), &[], None).unwrap().0;
        assert_ne!(a.id, b.id);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..12)
                .map(|_| {
                    scope.spawn(|| {
                        sessions.pick_with_reason(&pool, &cfg, "gpt-6.1-sol", Some("parallel"), &[], None).unwrap()
                    })
                })
                .collect();
            let selected: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert!(selected.iter().all(|s| s.account.id == selected[0].account.id));
            assert_eq!(selected.iter().filter(|s| s.reason == "new_session").count(), 1);
            assert_eq!(selected.iter().filter(|s| s.reason == "session_reused").count(), selected.len() - 1);
        });
    }

    #[test]
    fn history_cannot_cross_client_or_session_boundaries() {
        let sessions = Sessions::memory();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer first".parse().unwrap());
        sessions.remember(
            &headers,
            "task",
            &json!({"id":"r1", "output":[{"text":"answer"}]}),
            &[json!({"text":"question"})],
        );
        assert_eq!(sessions.previous(&headers, "r1", Some("task")).unwrap().1.len(), 2);
        assert!(sessions.previous(&headers, "r1", Some("another-task")).is_none());
        headers.insert("authorization", "Bearer second".parse().unwrap());
        assert!(sessions.previous(&headers, "r1", None).is_none());
    }

    #[test]
    fn assignments_survive_restart_and_idle_sessions_expire() {
        let dir = std::env::temp_dir().join(format!("cliproxy-affinity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = config(Routing::RoundRobin);
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::load(&dir, 86_400);
        let a = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("sensitive-task-id"), &[], None).unwrap().0;
        let loaded = Sessions::load(&dir, 86_400);
        assert_eq!(loaded.pick(&pool, &cfg, "gpt-6.1-sol", Some("sensitive-task-id"), &[], None).unwrap().0.id, a.id);
        let data = std::fs::read_to_string(dir.join(".routing-sessions.state")).unwrap();
        assert!(!data.contains("sensitive-task-id"));
        loaded.registry.lock().bindings.values_mut().for_each(|b| b.last_seen -= 90_000);
        loaded.prune(86_400);
        assert!(loaded.registry.lock().bindings.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn active_calls_prevent_idle_expiry_and_explicit_completion_releases_assignments() {
        let cfg = config(Routing::RoundRobin);
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Arc::new(Sessions::memory());
        let lease = sessions.hold("task", 86_400);
        let first = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0;
        sessions.registry.lock().bindings.values_mut().for_each(|b| b.last_seen -= 90_000);
        sessions.prune(86_400);
        assert_eq!(sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0.id, first.id);
        drop(lease);
        sessions.end("task");
        assert!(sessions.registry.lock().bindings.is_empty());
        assert_ne!(sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0.id, first.id);
    }

    #[test]
    fn disabling_affinity_restores_per_request_routing_and_nested_config_works() {
        let cfg = Config::parse("routing:\n  strategy: round-robin\n  session-affinity: false\n").unwrap();
        assert!(!cfg.session_affinity);
        assert!(!cfg.ignored.iter().any(|s| s.contains("session affinity")));
        let cfg = Config { session_affinity: false, ..config(Routing::RoundRobin) };
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        let first = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0;
        let second = sessions.pick(&pool, &cfg, "gpt-6.1-sol", Some("task"), &[], None).unwrap().0;
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn exhaustion_from_usage_windows_and_provider_aliases_preserves_the_new_account() {
        let mut cfg = config(Routing::LeastUsed);
        for entry in &mut cfg.codex_api_key {
            entry.models = vec![
                crate::config::ModelAlias { name: "gpt-6.1-sol".into(), alias: Some("coding".into()) },
                crate::config::ModelAlias { name: "gpt-6-astra".into(), alias: None },
            ];
        }
        let pool = Pool::default();
        pool.reload(&cfg);
        let sessions = Sessions::memory();
        let a = sessions.pick(&pool, &cfg, "coding", Some("task"), &[], None).unwrap().0;
        assert_eq!(sessions.pick(&pool, &cfg, "gpt-6-astra", Some("task"), &[], None).unwrap().0.id, a.id);
        a.state.lock().quota = crate::quota::Quota {
            updated_at: Some(Utc::now()),
            windows: vec![crate::quota::Window { name: "5h".into(), used: 100.0, resets_at: None, model: None }],
            ..Default::default()
        };
        let b = sessions.pick(&pool, &cfg, "coding", Some("task"), &[], None).unwrap().0;
        assert_ne!(a.id, b.id);
        a.state.lock().quota = Default::default();
        assert_eq!(sessions.pick(&pool, &cfg, "coding", Some("task"), &[], None).unwrap().0.id, b.id);
    }
}
