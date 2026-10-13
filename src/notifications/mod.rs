//! Quota transitions and bounded secure notification delivery, inside the proxy process.
mod credentials;
mod delivery;
pub mod state;
mod store;
#[cfg(test)]
mod tests;

use crate::{
    accounts::{Account, Credential, Provider},
    config::Config as AppConfig,
    state::App,
};
use chrono::Utc;
use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use state::{Event, Subscription};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use store::{Log, MAX_PENDING, Pending, Store};

/// Default destination delivery and bundled provider thumbnails to enabled.
fn yes() -> bool {
    true
}
/// Default explicit private endpoint permissions to the HTTPS port.
fn https_port() -> u16 {
    443
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub time_zone: String,
    pub provider_logos: bool,
    pub credential_ui_enabled: bool,
    pub credential_public_url: String,
    pub credential_proxy_cidrs: Vec<String>,
    pub secrets_dir: Option<String>,
    pub ca_file: Option<String>,
    pub private_endpoints: Vec<PrivateEndpoint>,
    pub destinations: Vec<Destination>,
}
impl Default for Config {
    /// Keep monitoring and secret entry opt-in, with UTC timestamps and bundled thumbnails.
    fn default() -> Self {
        Self {
            enabled: false,
            time_zone: "UTC".into(),
            provider_logos: true,
            credential_ui_enabled: false,
            credential_public_url: String::new(),
            credential_proxy_cidrs: Vec::new(),
            secrets_dir: None,
            ca_file: None,
            private_endpoints: Vec::new(),
            destinations: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PrivateEndpoint {
    pub host: String,
    #[serde(default = "https_port")]
    pub port: u16,
    pub cidrs: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Destination {
    pub id: String,
    pub format: Format,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub chat_id: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Generic,
    Discord,
    Slack,
    Mattermost,
    Teams,
    Telegram,
}
/// Accept only bounded lowercase destination IDs safe for filenames and environment bindings.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.as_bytes()[0] != b'-'
        && id.as_bytes()[id.len() - 1] != b'-'
}
impl Config {
    /// Validate configuration bounds and reject invalid time zones, origins, destinations, or network rules.
    pub fn validate(&self) -> Result<(), String> {
        credential_public_origin(&self.credential_public_url).map_err(|_|"notifications: credential-public-url must be an HTTPS origin without credentials, query, fragment or path".to_string())?;
        if self.credential_proxy_cidrs.len() > 16 {
            return Err("notifications: too many credential-proxy-cidrs".into());
        }
        for cidr in &self.credential_proxy_cidrs {
            let valid = if let Some((ip, bits)) = cidr.split_once('/') {
                ip.parse::<std::net::IpAddr>()
                    .ok()
                    .zip(bits.parse::<u32>().ok())
                    .is_some_and(|(ip, bits)| bits > 0 && bits <= if ip.is_ipv4() { 32 } else { 128 })
            } else {
                cidr.parse::<std::net::IpAddr>().is_ok()
            };
            if cidr.len() > 80 || !valid {
                return Err("notifications: invalid credential-proxy-cidrs".into());
            }
        }
        if self.time_zone.is_empty() || self.time_zone.len() > 64 || self.time_zone.parse::<chrono_tz::Tz>().is_err() {
            return Err("notifications: time-zone must be a valid IANA time zone".into());
        }
        if self.destinations.len() > 8 || self.private_endpoints.len() > 16 {
            return Err("notifications: too many destinations or private endpoints".into());
        }
        let mut ids = BTreeSet::new();
        for d in &self.destinations {
            if !valid_id(&d.id) || !ids.insert(&d.id) {
                return Err("notifications: invalid or duplicate destination ID".into());
            }
            if d.format == Format::Telegram
                && d.chat_id.as_deref().is_none_or(|s| {
                    s.is_empty()
                        || s.len() > 64
                        || !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'@'))
                })
            {
                return Err("notifications: Telegram requires a valid chat-id".into());
            }
            if d.chat_id.as_ref().is_some_and(|s| s.len() > 64) {
                return Err("notifications: chat-id too long".into());
            }
        }
        if self.secrets_dir.as_ref().is_some_and(|s| s.is_empty() || s.len() > 4096) {
            return Err("notifications: invalid secrets-dir".into());
        }
        if self.ca_file.as_ref().is_some_and(|s| s.is_empty() || s.len() > 4096) {
            return Err("notifications: invalid ca-file".into());
        }
        for entry in &self.private_endpoints {
            if entry.port == 0
                || entry.host.is_empty()
                || entry.host.len() > 253
                || !entry
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
                || entry.cidrs.is_empty()
                || entry.cidrs.len() > 16
            {
                return Err("notifications: invalid private endpoint".into());
            }
            for cidr in &entry.cidrs {
                let Some((ip, bits)) = cidr.split_once('/') else {
                    return Err("notifications: invalid private endpoint CIDR".into());
                };
                let Ok(ip) = ip.parse::<std::net::IpAddr>() else {
                    return Err("notifications: invalid private endpoint CIDR".into());
                };
                let Ok(bits) = bits.parse::<u32>() else {
                    return Err("notifications: invalid private endpoint CIDR".into());
                };
                if bits == 0 || bits > if ip.is_ipv4() { 32 } else { 128 } {
                    return Err("notifications: invalid private endpoint CIDR".into());
                }
            }
        }
        Ok(())
    }
}
/// The deployment's public HTTPS origin, never a webhook or credential-bearing URL.
pub fn credential_public_origin(value: &str) -> Result<Option<String>, &'static str> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 300
        || value.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '@' | '?' | '#'))
    {
        return Err("credential_public_url_invalid");
    }
    let (_, authority_and_path) = value.split_once("://").ok_or("credential_public_url_invalid")?;
    let authority = authority_and_path.split('/').next().ok_or("credential_public_url_invalid")?;
    if authority.contains('@') {
        return Err("credential_public_url_invalid");
    }
    if authority_and_path.split_once('/').is_some_and(|(_, path)| !path.is_empty()) {
        return Err("credential_public_url_invalid");
    }
    let url = url::Url::parse(value).map_err(|_| "credential_public_url_invalid")?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("credential_public_url_invalid");
    }
    Ok(Some(url.origin().ascii_serialization()))
}
struct Inner {
    store: Option<Store>,
    error: Option<&'static str>,
    warning: Option<&'static str>,
    tests: BTreeMap<String, chrono::DateTime<Utc>>,
}
pub struct Service {
    inner: Mutex<Inner>,
    root: PathBuf,
    secrets: PathBuf,
    private_endpoints: Vec<PrivateEndpoint>,
    ca_file: Option<PathBuf>,
    credential_proxy_cidrs: Vec<String>,
    credentials_write: Mutex<()>,
}
impl Service {
    /// Capture startup-only secret paths and network permissions without creating monitoring state.
    pub fn new(cfg: &AppConfig) -> Self {
        let root = resolve_links(cfg.auth_dir());
        let secrets = resolve_links(
            cfg.notifications
                .secrets_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".notification-secrets")),
        );
        Self {
            inner: Mutex::new(Inner { store: None, error: None, warning: None, tests: BTreeMap::new() }),
            root,
            secrets,
            private_endpoints: cfg.notifications.private_endpoints.clone(),
            ca_file: cfg.notifications.ca_file.as_ref().map(PathBuf::from),
            credential_proxy_cidrs: cfg.notifications.credential_proxy_cidrs.clone(),
            credentials_write: Mutex::new(()),
        }
    }
    /// Initialize the exclusively locked journal on demand and retain a safe startup error on failure.
    fn open(&self) -> Result<(), &'static str> {
        let mut inner = self.inner.lock();
        if inner.store.is_none() {
            match Store::open(&self.root) {
                Ok(store) => {
                    inner.store = Some(store);
                    inner.error = None;
                }
                Err(error) => {
                    inner.error = Some(error);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    /// Expose credential readiness and sanitized delivery activity without reading secrets into responses.
    pub fn status(&self, app: &App) -> Value {
        let cfg = app.cfg();
        let (active, error, warning, pending, installation, stored_logs) = {
            let inner = self.inner.lock();
            (
                cfg.notifications.enabled && inner.store.is_some() && inner.error.is_none(),
                inner.error,
                inner.warning,
                inner.store.as_ref().map_or(0, |store| store.journal.pending.len()),
                inner.store.as_ref().map(|store| store.journal.installation.clone()),
                inner.store.as_ref().map(|store| store.journal.logs.clone()),
            )
        };
        let destinations:Vec<_>=cfg.notifications.destinations.iter().map(|d|{
            let external=delivery::external_present(&self.secrets,d);
            let managed=!external&&credentials::present(&self.root,&d.id);
            json!({"id":d.id,"format":d.format,"enabled":d.enabled,"credential_ready":delivery::resolved_credentials(&self.secrets,&self.root,d).is_ok(),"credential_source":if external{"external"}else if managed{"managed"}else{"none"},"credential_configured":external||managed,"credential_editable":!external&&cfg!(unix)})
        }).collect();
        let mut supported = 0;
        let mut unsupported = 0;
        let mut names = BTreeMap::new();
        for account in app.pool.all() {
            if native(&account) {
                supported += 1;
                if let Some(salt) = installation.as_deref() {
                    names.entry(identity(&account, salt)).or_insert_with(|| delivery::safe_name(&account.label));
                }
            } else {
                unsupported += 1
            }
        }
        let logs: Option<Vec<Value>> = stored_logs.map(|logs| {
            logs.iter()
                .map(|log| {
                    let mut value = json!(log);
                    let name = if log.event == "notification.test" {
                        Some("Notification delivery test")
                    } else {
                        log.subscription.as_deref().and_then(|id| names.get(id).map(String::as_str))
                    };
                    value["display_name"] = json!(name);
                    value
                })
                .collect()
        });
        json!({"enabled":cfg.notifications.enabled,"time_zone":cfg.notifications.time_zone,"credential_ui_enabled":cfg.notifications.credential_ui_enabled,"credential_public_url":credential_public_origin(&cfg.notifications.credential_public_url).ok().flatten().unwrap_or_default(),"active":active,"error":error,"warning":warning,"pending":pending,"destinations":destinations,"logs":logs,"capabilities":{"supported":supported,"unsupported":unsupported},"private_endpoints_restart_required":true})
    }
    /// Rate-limit a single test delivery and journal only its credential-free result.
    pub async fn test(&self, app: &App, id: &str) -> Result<Value, &'static str> {
        let cfg = app.cfg();
        if !cfg.notifications.enabled {
            return Err("notifications_disabled");
        }
        let d = cfg
            .notifications
            .destinations
            .iter()
            .find(|d| d.id == id && d.enabled)
            .ok_or("destination_unavailable")?
            .clone();
        self.open()?;
        {
            let mut inner = self.inner.lock();
            if inner.tests.get(id).is_some_and(|at| (Utc::now() - *at).num_seconds() < 30) {
                return Err("test_rate_limited");
            }
            inner.tests.retain(|_, at| (Utc::now() - *at).num_seconds() < 30);
            inner.tests.insert(id.into(), Utc::now());
        }
        let event = Event {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            event: "notification.test".into(),
            subscription: String::new(),
            provider: String::new(),
            window: "test".into(),
            model: None,
            observed_at: Utc::now(),
            resets_at: None,
            used: None,
            remaining_blockers: Vec::new(),
        };
        let notification_config = app.cfg();
        let presentation = delivery::Presentation::new(None, time_zone(&notification_config.notifications))
            .with_provider_logos(notification_config.notifications.provider_logos);
        let outcome = delivery::send(
            &self.secrets,
            &self.private_endpoints,
            self.ca_file.as_deref(),
            &self.root,
            &d,
            &event,
            &presentation,
        )
        .await;
        let mut inner = self.inner.lock();
        if let Some(store) = inner.store.as_mut() {
            store.log(log(&d.id, &event, 1, &outcome, false));
            store.save()?;
        }
        if outcome.success { Ok(json!({"delivered":true,"http_status":outcome.status})) } else { Err(outcome.reason) }
    }
    /// Match the request peer only against the startup-configured proxy CIDRs.
    pub fn trusted_credential_proxy(&self, peer: std::net::IpAddr) -> bool {
        let original_peer = peer;
        let peer = match peer {
            std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(peer),
            _ => peer,
        };
        self.credential_proxy_cidrs.iter().any(|cidr| {
            cidr.parse::<std::net::IpAddr>().is_ok_and(|ip| ip == peer || ip == original_peer)
                || delivery::cidr_contains(cidr, peer)
                || delivery::cidr_contains(cidr, original_peer)
        })
    }
    /// Serialize credential changes with config edits and refuse externally managed destinations.
    pub fn save_credentials(
        &self,
        app: &App,
        id: &str,
        url: String,
        bearer_token: Option<String>,
    ) -> Result<(), &'static str> {
        let _config_guard = app.config_write.lock();
        let _guard = self.credentials_write.lock();
        let cfg = app.cfg();
        if !cfg.notifications.credential_ui_enabled {
            return Err("credential_ui_disabled");
        }
        let destination = cfg
            .notifications
            .destinations
            .iter()
            .find(|destination| destination.id == id)
            .ok_or("destination_unavailable")?;
        if delivery::external_present(&self.secrets, destination) {
            return Err("credentials_externally_managed");
        }
        delivery::parse_credentials(&url, bearer_token.as_deref())?;
        let bundle = credentials::Bundle {
            url: url.trim().into(),
            bearer_token: bearer_token.map(|token| token.trim().into()).filter(|token: &String| !token.is_empty()),
        };
        credentials::save(&self.root, id, &bundle)?;
        app.broadcast("notifications", Value::Null);
        Ok(())
    }
    /// Remove managed bundles, including orphaned IDs, without deleting external secret bindings.
    pub fn remove_credentials(&self, app: &App, id: &str) -> Result<(), &'static str> {
        let _config_guard = app.config_write.lock();
        let _guard = self.credentials_write.lock();
        let cfg = app.cfg();
        if !cfg.notifications.credential_ui_enabled {
            return Err("credential_ui_disabled");
        }
        if !valid_id(id) {
            return Err("invalid_destination");
        }
        // Administrators can clean up an orphan left by an offline YAML edit.
        let destination = Destination { id: id.into(), format: Format::Generic, enabled: false, chat_id: None };
        if delivery::external_present(&self.secrets, &destination) {
            return Err("credentials_externally_managed");
        }
        credentials::remove(&self.root, id)?;
        app.broadcast("notifications", Value::Null);
        Ok(())
    }
    /// Call under config_write so credential saves cannot race destination edits.
    pub fn validate_destination_change(&self, app: &App, next: &AppConfig) -> Result<(), &'static str> {
        let current = app.cfg();
        for destination in &current.notifications.destinations {
            if !next.notifications.destinations.iter().any(|entry| entry.id == destination.id)
                && credentials::present(&self.root, &destination.id)
            {
                return Err("Remove saved notification credentials before deleting or renaming a destination.");
            }
        }
        for destination in &next.notifications.destinations {
            if !current.notifications.destinations.iter().any(|entry| entry.id == destination.id)
                && credentials::present(&self.root, &destination.id)
            {
                return Err("This destination ID already has saved credentials. Remove them before reusing the ID.");
            }
        }
        Ok(())
    }
    /// Request authoritative usage after an exhausted window reaches its estimated reset time.
    pub fn reset_due(&self, acct: &Account, now: chrono::DateTime<Utc>) -> bool {
        let inner = self.inner.lock();
        let Some(store) = &inner.store else {
            return false;
        };
        let id = identity(acct, &store.journal.installation);
        store.journal.subscriptions.get(&id).is_some_and(|s| {
            s.windows
                .values()
                .any(|w| w.exhausted && w.reset.is_some_and(|reset| reset + chrono::Duration::seconds(5) <= now))
        })
    }
}
/// Links in a configured path (such as /home -> /var/home) are the operator's choice, so they are
/// resolved once at startup; the credential checks still reject any link inside the result. Parts
/// that do not exist yet are kept as written under the deepest existing directory.
fn resolve_links(path: PathBuf) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return missing.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(part)) => {
                missing.push(part);
                existing = parent;
            }
            _ => return path,
        }
    }
}
/// Restrict monitoring to Claude/Codex OAuth subscriptions using native provider endpoints.
fn native(acct: &Account) -> bool {
    matches!(acct.provider, Provider::Claude | Provider::Codex)
        && matches!(&*acct.cred.read(),Credential::OAuth(o) if o.base_url.is_none())
}
/// Hash provider identity with an installation-local salt rather than persisting account identifiers.
fn identity(acct: &Account, salt: &str) -> String {
    let cred = acct.cred.read();
    let provider = acct.provider.as_str();
    let key = match &*cred {
        Credential::OAuth(o) => o.account_id.as_deref().filter(|s| !s.is_empty()).unwrap_or(&acct.id),
        _ => &acct.id,
    };
    let mut hash = Sha256::new();
    for part in [salt, provider, key] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())[..24].into()
}
/// Resolve the validated display zone, falling back to UTC for legacy configuration.
fn time_zone(config: &Config) -> chrono_tz::Tz {
    config.time_zone.parse().unwrap_or(chrono_tz::UTC)
}
/// Resolve the current account label at send time without storing identifying presentation data.
fn display_name(app: &App, id: &str, salt: &str) -> Option<String> {
    app.pool
        .all()
        .iter()
        .find(|account| native(account) && identity(account, salt) == id)
        .map(|account| delivery::safe_name(&account.label))
}
/// Build one bounded delivery record using fixed outcome categories and credential-free event fields.
fn log(destination: &str, event: &Event, attempt: u8, outcome: &delivery::Outcome, retrying: bool) -> Log {
    Log {
        timestamp: Utc::now(),
        destination: destination.into(),
        event: event.event.clone(),
        subscription: (event.event != "notification.test").then(|| event.subscription.clone()),
        window: (event.event != "notification.test").then(|| event.window.clone()),
        attempt,
        outcome: if outcome.success {
            "delivered"
        } else if retrying {
            "retrying"
        } else {
            "terminal_failure"
        }
        .into(),
        http_status: outcome.status,
        detail: outcome.reason.into(),
    }
}
// Fresh evidence is deliberately memory-only: durable quota state is not proof that a
// subscription is still available after a restart or an administrator pauses it.
#[derive(Clone, Default)]
struct FreshEvidence {
    authoritative: BTreeSet<String>,
    exhausted: BTreeSet<String>,
}
/// Keep shared and model-scoped quota windows distinct in confirmation evidence.
fn window_key(name: &str, model: Option<&str>) -> String {
    format!("{}:{}", name, model.unwrap_or("*"))
}
/// Collect the scopes still exhausted when a recovery event is considered.
fn blockers(subscription: &Subscription) -> Vec<String> {
    subscription
        .windows
        .values()
        .filter(|window| window.exhausted)
        .map(|window| {
            if window.name == "unknown" {
                window.model.as_ref().map(|model| format!("unknown {model}")).unwrap_or_else(|| window.name.clone())
            } else {
                window.name.clone()
            }
        })
        .collect()
}
impl FreshEvidence {
    /// Track fresh coverage and exhaustion, requiring full applicable coverage to confirm unknown scopes.
    fn record(&mut self, subscription: &Subscription, observation: &state::Observation) {
        for window in observation.windows.iter().filter(|w| state::valid_window(w)) {
            let key = window_key(&window.name, window.model.as_deref());
            // Match the reducer: elapsed or superseded samples are not confirmation.
            if window.resets_at.is_some_and(|reset| reset <= observation.at)
                || subscription.windows.get(&key).is_some_and(|old| old.observed_at > observation.at)
            {
                continue;
            }
            if observation.authoritative {
                self.authoritative.insert(key.clone());
            }
            if window.used >= 100.0 {
                self.exhausted.insert(key);
            }
        }
        if let Some(scope) = &observation.unknown {
            self.exhausted.insert(window_key("unknown", (scope != "*").then_some(scope.as_str())));
        }
        if observation.authoritative
            && observation.windows.iter().any(|w| w.name == "5h" && w.model.is_none())
            && observation.windows.iter().any(|w| w.name == "week" && w.model.is_none())
        {
            for unknown in subscription.windows.values().filter(|w| w.name == "unknown") {
                let applicable =
                    |model: &Option<String>| model.is_none() || unknown.model.is_none() || *model == unknown.model;
                let covered = subscription
                    .windows
                    .values()
                    .filter(|w| w.name != "unknown" && applicable(&w.model))
                    .all(|old| observation.windows.iter().any(|w| w.name == old.name && w.model == old.model));
                let available = observation.windows.iter().filter(|w| applicable(&w.model)).all(|w| {
                    state::valid_window(w) && w.used < 100.0 && w.resets_at.is_none_or(|reset| reset > observation.at)
                });
                if covered && available && unknown.observed_at <= observation.at {
                    self.authoritative.insert(window_key("unknown", unknown.model.as_deref()));
                }
            }
        }
    }
    /// Require authoritative confirmation for every learned window before releasing resumed recovery.
    fn complete(&self, subscription: &Subscription) -> bool {
        !subscription.windows.is_empty() && subscription.windows.keys().all(|key| self.authoritative.contains(key))
    }
    /// Gate dispatch on fresh matching exhaustion or authoritative recovery coverage.
    fn permits(&self, event: &Event, subscription: &Subscription, resumed: bool) -> bool {
        let key = window_key(&event.window, event.model.as_deref());
        match event.event.as_str() {
            "quota.exhausted" => self.exhausted.contains(&key),
            "quota.available" => self.complete(subscription),
            "quota.window_recovered" => self.authoritative.contains(&key) && (!resumed || self.complete(subscription)),
            _ => false,
        }
    }
}
/// Persist bounded transitions before dispatch, cancel superseded events, and retry four sends concurrently.
pub async fn worker(app: Arc<App>) {
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    let mut fresh: BTreeMap<String, FreshEvidence> = BTreeMap::new();
    let mut resumed = BTreeSet::new();
    let mut capture_resumed = true;
    let mut fresh_after: BTreeMap<String, chrono::DateTime<Utc>> = BTreeMap::new();
    let mut enabled_since = app.started;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cfg = app.cfg();
        if !cfg.notifications.enabled {
            seen.clear();
            fresh.clear();
            capture_resumed = true;
            enabled_since = Utc::now();
            continue;
        }
        if app.notifications.open().is_err() {
            continue;
        }
        let salt = {
            let inner = app.notifications.inner.lock();
            let store = inner.store.as_ref().unwrap();
            if capture_resumed {
                resumed = store.journal.pending.iter().map(|pending| pending.event.id.clone()).collect();
                capture_resumed = false;
            }
            store.journal.installation.clone()
        };
        let accounts: Vec<_> = app.pool.all().into_iter().filter(|a| native(a)).collect();
        let mut present = BTreeSet::new();
        let mut enabled = BTreeSet::new();
        let mut snapshots = Vec::new();
        let mut sources = BTreeMap::new();
        for acct in accounts {
            let id = identity(&acct, &salt);
            present.insert(id.clone());
            let key = format!("{}:{}", acct.id, id);
            let mut st = acct.state.lock();
            if let Some(changed_at) = st.notifications_changed_at
                && fresh_after.get(&id).is_none_or(|previous| *previous < changed_at)
            {
                fresh.remove(&id);
                seen.remove(&key);
                fresh_after.insert(id.clone(), changed_at);
                let inner = app.notifications.inner.lock();
                resumed.extend(
                    inner
                        .store
                        .as_ref()
                        .unwrap()
                        .journal
                        .pending
                        .iter()
                        .filter(|pending| pending.event.subscription == id)
                        .map(|pending| pending.event.id.clone()),
                );
            }
            if st.disabled {
                fresh.remove(&id);
                let inner = app.notifications.inner.lock();
                resumed.extend(
                    inner
                        .store
                        .as_ref()
                        .unwrap()
                        .journal
                        .pending
                        .iter()
                        .filter(|pending| pending.event.subscription == id)
                        .map(|pending| pending.event.id.clone()),
                );
                fresh_after.insert(id, Utc::now());
                continue;
            }
            enabled.insert(id.clone());
            let latest = st.notification_evidence.sequence;
            if seen.get(&key).is_some_and(|previous| *previous > latest) {
                seen.remove(&key);
                fresh.remove(&id);
            }
            let observations: Vec<_> = st
                .notification_evidence
                .observations
                .iter()
                .filter(|o| o.sequence > *seen.get(&key).unwrap_or(&0))
                .cloned()
                .collect();
            let after = fresh_after.get(&id).copied().unwrap_or(enabled_since).max(enabled_since);
            let confirmation_needed = {
                let inner = app.notifications.inner.lock();
                inner.store.as_ref().and_then(|s| s.journal.subscriptions.get(&id)).is_some_and(|s| {
                    observations.iter().filter(|o| !o.authoritative).flat_map(|o| &o.windows).any(|w| {
                        w.used < 100.0
                            && s.windows.values().any(|old| old.exhausted && old.name == w.name && old.model == w.model)
                    })
                })
            };
            let awaiting_confirmation = {
                let inner = app.notifications.inner.lock();
                inner.store.as_ref().is_some_and(|store| {
                    let recovery_pending = store.journal.pending.iter().any(|pending| {
                        pending.event.subscription == id
                            && matches!(pending.event.event.as_str(), "quota.available" | "quota.window_recovered")
                    });
                    store.journal.subscriptions.get(&id).is_some_and(|subscription| {
                        (recovery_pending || subscription.windows.values().any(|window| window.exhausted))
                            && !fresh.get(&id).is_some_and(|evidence| evidence.complete(subscription))
                    })
                })
            };
            // A subscription can stay exhausted for days, so its confirmation polls keep the
            // normal 5-minute pace; one-off recovery hints are confirmed straight away.
            let confirmation_due =
                st.quota.refreshed_at.is_none_or(|at| Utc::now() - at >= chrono::Duration::minutes(5));
            if (awaiting_confirmation && confirmation_due) || st.notification_evidence.overflow || confirmation_needed {
                st.quota.refreshed_at = None;
            }
            snapshots.push((
                key.clone(),
                id,
                acct.provider.as_str().to_string(),
                observations,
                st.notification_evidence.overflow,
                after,
            ));
            sources.insert(key, acct.clone());
        }
        seen.retain(|key, _| sources.contains_key(key));
        fresh.retain(|id, _| present.contains(id));
        fresh_after.retain(|id, _| present.contains(id));
        let mut acknowledged = Vec::new();
        let jobs = {
            let mut inner = app.notifications.inner.lock();
            if snapshots.iter().any(|(_, _, _, _, overflow, _)| *overflow) {
                inner.warning = Some("observation_history_overflow_reconciling");
            }
            let store = inner.store.as_mut().unwrap();
            let before = store.journal.clone();
            let mut next_fresh = fresh.clone();
            let mut capacity = false;
            let old_subs = store.journal.subscriptions.len();
            let old_pending = store.journal.pending.len();
            store.journal.subscriptions.retain(|id, _| present.contains(id));
            // A disabled destination pauses its outbox; removal cancels it.
            store.journal.pending.retain(|p| {
                present.contains(&p.event.subscription)
                    && cfg.notifications.destinations.iter().any(|d| d.id == p.destination)
            });
            let mut dirty = old_subs != store.journal.subscriptions.len() || old_pending != store.journal.pending.len();
            for (_, id, provider, observations, _, after) in &snapshots {
                if observations.is_empty() {
                    continue;
                }
                if !store.journal.subscriptions.contains_key(id) && store.journal.subscriptions.len() >= 4096 {
                    capacity = true;
                    break;
                }
                let subscription = store
                    .journal
                    .subscriptions
                    .entry(id.clone())
                    .or_insert_with(|| Subscription { provider: provider.clone(), ..Default::default() });
                let mut events = Vec::new();
                for observation in observations.iter().filter(|o| o.at >= *after) {
                    events.extend(subscription.apply(id, observation));
                    next_fresh.entry(id.clone()).or_default().record(subscription, observation);
                    dirty = true;
                }
                for event in events {
                    for d in cfg.notifications.destinations.iter().filter(|d| d.enabled) {
                        if store.journal.pending.len() >= MAX_PENDING {
                            capacity = true;
                            break;
                        }
                        store.journal.pending.push_back(Pending {
                            destination: d.id.clone(),
                            event: event.clone(),
                            attempt: 0,
                            next: Utc::now(),
                        });
                    }
                }
            }
            let mut cancelled = Vec::new();
            store.journal.pending.retain(|p| {
                let stale = store.journal.subscriptions.get(&p.event.subscription).is_some_and(|s| {
                    match p.event.event.as_str() {
                        "quota.exhausted" => !s
                            .windows
                            .values()
                            .any(|w| w.exhausted && w.name == p.event.window && w.model == p.event.model),
                        "quota.available" => s.windows.values().any(|w| w.exhausted),
                        "quota.window_recovered" => {
                            s.windows
                                .values()
                                .any(|w| w.exhausted && w.name == p.event.window && w.model == p.event.model)
                                || blockers(s) != p.event.remaining_blockers
                        }
                        _ => false,
                    }
                });
                let expired = (Utc::now() - p.event.observed_at).num_hours() >= 48;
                if stale || expired {
                    cancelled.push(Log {
                        timestamp: Utc::now(),
                        destination: p.destination.clone(),
                        event: p.event.event.clone(),
                        subscription: Some(p.event.subscription.clone()),
                        window: Some(p.event.window.clone()),
                        attempt: p.attempt,
                        outcome: if expired { "expired" } else { "cancelled" }.into(),
                        http_status: None,
                        detail: if expired { "retention_expired" } else { "superseded" }.into(),
                    });
                    false
                } else {
                    true
                }
            });
            dirty |= !cancelled.is_empty();
            for entry in cancelled {
                store.log(entry);
            }
            let persistence = if capacity {
                Err("outbox_capacity_exceeded")
            } else if dirty {
                store.save()
            } else {
                Ok(())
            };
            let reconciled = persistence.is_ok();
            if let Err(error) = persistence {
                store.journal = before;
                inner.error = Some(error);
            } else {
                for (account, _, _, observations, _, _) in &snapshots {
                    if let Some(last) = observations.last() {
                        seen.insert(account.clone(), last.sequence);
                        acknowledged.push((account.clone(), last.sequence));
                    }
                }
                fresh = next_fresh;
                inner.error = None;
            }
            // Existing durable jobs may drain even while new observations exceed capacity.
            let store = inner.store.as_ref().unwrap();
            let pending_ids: BTreeSet<_> =
                store.journal.pending.iter().map(|pending| pending.event.id.clone()).collect();
            resumed.retain(|id| pending_ids.contains(id));
            let mut blocked = BTreeSet::new();
            let mut jobs = Vec::new();
            for p in &store.journal.pending {
                if !blocked.insert((p.destination.clone(), p.event.subscription.clone())) {
                    continue;
                }
                if jobs.len() == 4 {
                    break;
                }
                if p.next <= Utc::now()
                    && enabled.contains(&p.event.subscription)
                    // Never claim recovery against state rolled back after failed reconciliation.
                    && (reconciled || p.event.event == "quota.exhausted")
                    && store.journal.subscriptions.get(&p.event.subscription).is_some_and(|subscription| {
                        fresh.get(&p.event.subscription).is_some_and(|evidence| {
                            evidence.permits(&p.event, subscription, resumed.contains(&p.event.id))
                        })
                    })
                    && let Some(destination) =
                        cfg.notifications.destinations.iter().find(|d| d.id == p.destination && d.enabled)
                {
                    jobs.push((p.clone(), destination.clone()));
                }
            }
            jobs
        };
        for (key, through) in acknowledged {
            if let Some(acct) = sources.get(&key) {
                let mut st = acct.state.lock();
                st.notification_evidence.observations.retain(|o| o.sequence > through);
                st.notification_evidence.overflow = false;
            }
        }
        // Requests run independently, bounded to four, and retain per-destination/subscription order.
        let results: Vec<_> = stream::iter(jobs)
            .map(|(p, d)| {
                let app = app.clone();
                async move {
                    let service = &app.notifications;
                    // A config change can pause a queued job before its network request starts.
                    let live = app.cfg();
                    let outcome = if live.notifications.enabled
                        && live.notifications.destinations.iter().any(|entry| entry.id == d.id && entry.enabled)
                    {
                        let salt = service.inner.lock().store.as_ref().unwrap().journal.installation.clone();
                        let name = display_name(&app, &p.event.subscription, &salt);
                        let presentation = delivery::Presentation::new(name.as_deref(), time_zone(&live.notifications))
                            .with_provider_logos(live.notifications.provider_logos);
                        delivery::send(
                            &service.secrets,
                            &service.private_endpoints,
                            service.ca_file.as_deref(),
                            &service.root,
                            &d,
                            &p.event,
                            &presentation,
                        )
                        .await
                    } else {
                        delivery::Outcome {
                            success: false,
                            retry: true,
                            status: None,
                            reason: "paused",
                            retry_after: Some(30),
                        }
                    };
                    (p, outcome)
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;
        if !results.is_empty() {
            let mut inner = app.notifications.inner.lock();
            let store = inner.store.as_mut().unwrap();
            let before = store.journal.clone();
            for (job, outcome) in results {
                if outcome.reason == "paused" {
                    continue;
                }
                let Some(index) = store
                    .journal
                    .pending
                    .iter()
                    .position(|p| p.destination == job.destination && p.event.id == job.event.id)
                else {
                    continue;
                };
                let attempt = job.attempt.saturating_add(1);
                let retry = outcome.retry && attempt < 8;
                store.log(log(&job.destination, &job.event, attempt, &outcome, retry));
                if retry {
                    let pending = &mut store.journal.pending[index];
                    pending.attempt = attempt;
                    let delay = outcome.retry_after.unwrap_or((15i64 << attempt.min(7)).min(3600))
                        + i64::from(rand::random::<u8>() % 11);
                    pending.next = Utc::now() + chrono::Duration::seconds(delay);
                } else {
                    store.journal.pending.remove(index);
                }
            }
            if let Err(error) = store.save() {
                store.journal = before;
                inner.error = Some(error);
            }
            app.broadcast("notifications", Value::Null);
        }
    }
}
