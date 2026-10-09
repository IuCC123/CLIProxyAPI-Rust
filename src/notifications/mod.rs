//! Quota transitions and bounded secure notification delivery, inside the proxy process.
mod credentials;
mod delivery;
pub mod state;
mod store;

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
        let root = cfg.auth_dir();
        let secrets = cfg
            .notifications
            .secrets_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".notification-secrets"));
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
            if awaiting_confirmation || st.notification_evidence.overflow || confirmation_needed {
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    /// Verify that credential public url is a strict normalized https origin.
    fn credential_public_url_is_a_strict_normalized_https_origin() {
        assert_eq!(credential_public_origin(""), Ok(None));
        assert_eq!(
            credential_public_origin("https://DASHBOARD.example.test:443/"),
            Ok(Some("https://dashboard.example.test".into()))
        );
        assert_eq!(
            credential_public_origin("https://[2001:db8::1]:8443"),
            Ok(Some("https://[2001:db8::1]:8443".into()))
        );
        for value in [
            "http://dashboard.example.test",
            "https://",
            "https://user@dashboard.example.test",
            "https://@dashboard.example.test",
            "https://dashboard.example.test?",
            "https://dashboard.example.test#",
            "https://dashboard.example.test/.",
            "https://dashboard.example.test/a/..",
            "https://dashboard.example.test//",
            " https://dashboard.example.test",
            "https://dashboard.example.test ",
            "https://dashboard.example.test\\",
        ] {
            assert_eq!(credential_public_origin(value), Err("credential_public_url_invalid"), "{value}");
            let cfg = Config { credential_public_url: value.into(), ..Default::default() };
            assert!(cfg.validate().is_err());
        }
        assert_eq!(
            credential_public_origin(&format!("https://{}", "a".repeat(301))),
            Err("credential_public_url_invalid")
        );
    }
    struct Temp(PathBuf);
    impl Temp {
        /// Create an isolated temporary test directory without using production credentials.
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("notifications-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        /// Release the test task or remove its temporary files when the fixture leaves scope.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    /// Create synthetic subscription state and an isolated journal for notification worker tests.
    fn fixture(enabled: bool) -> (Temp, Arc<App>, Arc<Account>) {
        let temp = Temp::new();
        std::fs::write(temp.0.join("account.json"),json!({"type":"claude","account_id":"private-provider-account","email":"private-email-sentinel@example.test","access_token":"private-oauth-sentinel","expired":"2099-01-01T00:00:00Z"}).to_string()).unwrap();
        let cfg = AppConfig {
            auth_dir: temp.0.to_string_lossy().into(),
            notifications: Config {
                enabled,
                destinations: vec![Destination {
                    id: format!("test-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
                    format: Format::Generic,
                    enabled: true,
                    chat_id: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let app = App::new(cfg, temp.0.join("config.yaml"));
        let acct = app.pool.all()[0].clone();
        (temp, app, acct)
    }
    /// Bound the wait for asynchronous worker delivery records in a test.
    async fn until_logs(app: &App, count: usize) {
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if app.notifications.status(app)["logs"].as_array().is_some_and(|logs| logs.len() >= count) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    /// Verify that destination validation prevents credential namespace escape.
    fn destination_validation_prevents_credential_namespace_escape() {
        for id in ["", "../secret", "A", "foo_bar", "-foo", "foo-", "é"] {
            assert!(!valid_id(id));
        }
        for id in ["discord", "ops-chat", "123"] {
            assert!(valid_id(id));
        }
        let bad = Config {
            destinations: vec![Destination {
                id: "telegram".into(),
                format: Format::Telegram,
                enabled: true,
                chat_id: None,
            }],
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }
    #[test]
    /// Verify that old config and legacy journal remain loadable without persisting presentation.
    fn old_config_and_legacy_journal_remain_loadable_without_persisting_presentation() {
        let old: Config = serde_json::from_value(json!({"enabled":true,"destinations":[]})).unwrap();
        assert_eq!(old.time_zone, "UTC");
        assert!(old.validate().is_ok());
        let mut invalid = old;
        invalid.time_zone = "NoSuch/Zone".into();
        assert!(invalid.validate().is_err());
        invalid.time_zone = "America/Denver".into();
        assert!(invalid.validate().is_ok());
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        let path = temp.0.join(".quota-notifications/state.json");
        drop(store);
        let legacy = json!({"installation":uuid::Uuid::new_v4().to_string(),"subscriptions":{},"pending":[],"logs":[{"timestamp":"2026-01-01T12:00:00Z","destination":"test","event":"notification.test","subscription":"000000000000000000000000","window":"test","attempt":1,"outcome":"delivered","http_status":204,"detail":"accepted"}]});
        std::fs::write(&path, legacy.to_string()).unwrap();
        let reopened = Store::open(&temp.0).unwrap();
        assert_eq!(reopened.journal.logs.len(), 1);
        reopened.save().unwrap();
        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("time_zone"));
        assert!(!persisted.contains("display_name"));
    }
    #[test]
    /// Verify that network and secret policy remain startup only after management config reload.
    fn network_and_secret_policy_remain_startup_only_after_management_config_reload() {
        let (_temp, app, _) = fixture(false);
        let mut cfg = (*app.cfg()).clone();
        cfg.notifications.private_endpoints =
            vec![PrivateEndpoint { host: "internal.example".into(), port: 443, cidrs: vec!["10.0.0.0/8".into()] }];
        cfg.notifications.secrets_dir = Some("/arbitrary/management/path".into());
        cfg.notifications.ca_file = Some("/arbitrary/management/ca".into());
        app.set_config(cfg);
        assert!(app.notifications.private_endpoints.is_empty());
        assert!(app.notifications.ca_file.is_none());
        assert!(app.notifications.secrets.ends_with(".notification-secrets"));
    }
    #[tokio::test]
    /// Verify that accepted first tick headers are sent and evidence overflow recovers.
    async fn accepted_first_tick_headers_are_sent_and_evidence_overflow_recovers() {
        let (_temp, app, acct) = fixture(true);
        let headers = reqwest::header::HeaderMap::from_iter([(
            "anthropic-ratelimit-unified-5h-status".parse().unwrap(),
            "rejected".parse().unwrap(),
        )]);
        crate::quota::observe(&acct, &headers, acct.quota_epoch());
        let task = tokio::spawn(worker(app.clone()));
        until_logs(&app, 1).await;
        assert!(acct.state.lock().notification_evidence.observations.is_empty());
        // Missing test credentials fail locally; this test never reaches any webhook.
        let status = app.notifications.status(&app);
        assert_eq!(status["logs"][0]["detail"], "credential_unavailable");
        let serialized = status.to_string();
        assert!(serialized.contains("private-email-sentinel"));
        let persisted =
            std::fs::read_to_string(app.notifications.root.join(".quota-notifications/state.json")).unwrap();
        assert!(!persisted.contains("private-email-sentinel"));
        assert!(!serialized.contains("private-oauth-sentinel"));
        assert!(!serialized.contains("private-provider-account"));
        {
            let mut st = acct.state.lock();
            let window = crate::quota::Window { name: "5h".into(), used: 100.0, resets_at: None, model: None };
            for _ in 0..140 {
                st.notification_evidence.observe(std::slice::from_ref(&window), false);
            }
            crate::quota::authoritative(&mut st, vec![crate::quota::Window { used: 0.0, ..window }], None);
        }
        until_logs(&app, 2).await;
        let status = app.notifications.status(&app);
        assert_eq!(status["logs"][1]["event"], "quota.available");
        assert!(status["error"].is_null());
        assert!(acct.state.lock().notification_evidence.observations.is_empty());
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    /// Verify that feature off does not capture observations or create notification state.
    async fn feature_off_does_not_capture_observations_or_create_notification_state() {
        let (temp, app, acct) = fixture(false);
        let headers = reqwest::header::HeaderMap::from_iter([(
            "anthropic-ratelimit-unified-5h-status".parse().unwrap(),
            "rejected".parse().unwrap(),
        )]);
        crate::quota::observe(&acct, &headers, acct.quota_epoch());
        assert!(acct.state.lock().notification_evidence.observations.is_empty());
        let task = tokio::spawn(worker(app.clone()));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!temp.0.join(".quota-notifications").exists());
        task.abort();
        let _ = task.await;
    }
    /// Create a shared-window quota sample for recovery confirmation tests.
    fn sample(name: &str, used: f64) -> crate::quota::Window {
        crate::quota::Window { name: name.into(), used, resets_at: None, model: None }
    }
    /// Persist a synthetic event before starting a worker to exercise restart recovery rules.
    fn seed_pending(app: &App, acct: &Account, kind: &str) -> String {
        app.notifications.open().unwrap();
        let mut inner = app.notifications.inner.lock();
        let store = inner.store.as_mut().unwrap();
        let id = identity(acct, &store.journal.installation);
        let mut subscription = Subscription { provider: "claude".into(), ..Default::default() };
        let old = Utc::now() - chrono::Duration::minutes(5);
        let observation = state::Observation {
            sequence: 1,
            at: old,
            windows: vec![sample("5h", 100.0), sample("week", 100.0)],
            authoritative: true,
            unknown: None,
        };
        let exhausted = subscription.apply(&id, &observation);
        let observation = state::Observation {
            sequence: 2,
            at: old + chrono::Duration::seconds(1),
            windows: if kind == "quota.window_recovered" {
                vec![sample("5h", 0.0)]
            } else {
                vec![sample("5h", 0.0), sample("week", 0.0)]
            },
            authoritative: true,
            unknown: None,
        };
        let event = if kind == "quota.exhausted" {
            exhausted.into_iter().find(|event| event.window == "5h").unwrap()
        } else {
            subscription.apply(&id, &observation).into_iter().find(|event| event.event == kind).unwrap()
        };
        store.journal.subscriptions.insert(id.clone(), subscription);
        store.journal.pending.push_back(Pending {
            destination: app.cfg().notifications.destinations[0].id.clone(),
            event,
            attempt: 0,
            next: Utc::now(),
        });
        store.save().unwrap();
        id
    }
    /// Wait until the worker consumes synthetic evidence without contacting a real provider.
    async fn consumed(app: &App, acct: &Account) {
        tokio::time::timeout(Duration::from_secs(6), async {
            while !acct.state.lock().notification_evidence.observations.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Missing credentials finish immediately without DNS or an outbound connection.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(app.notifications.status(app)["error"].is_null());
    }
    /// Assert that unconfirmed persisted recovery remains queued without a delivery attempt.
    fn assert_waiting(app: &App) {
        let status = app.notifications.status(app);
        assert_eq!(status["pending"], 1);
        assert!(status["logs"].as_array().unwrap().is_empty());
    }
    #[tokio::test]
    /// Verify missing learned windows do not force extra polling for healthy subscriptions, including after restart.
    async fn healthy_subscription_with_omitted_model_window_keeps_normal_poll_schedule() {
        for restart in [false, true] {
            let (temp, app, acct) = fixture(true);
            app.notifications.open().unwrap();
            {
                let mut inner = app.notifications.inner.lock();
                let store = inner.store.as_mut().unwrap();
                let id = identity(&acct, &store.journal.installation);
                let mut subscription = Subscription { provider: "claude".into(), ..Default::default() };
                assert!(
                    subscription
                        .apply(
                            &id,
                            &state::Observation {
                                sequence: 1,
                                at: Utc::now() - chrono::Duration::minutes(5),
                                windows: vec![
                                    sample("5h", 10.0),
                                    sample("week", 20.0),
                                    crate::quota::Window { model: Some("opus".into()), ..sample("week opus", 30.0) }
                                ],
                                authoritative: true,
                                unknown: None,
                            },
                        )
                        .is_empty()
                );
                store.journal.subscriptions.insert(id, subscription);
                store.save().unwrap();
            }
            let app = if restart {
                let cfg = (*app.cfg()).clone();
                drop(acct);
                drop(app);
                App::new(cfg, temp.0.join("config.yaml"))
            } else {
                app
            };
            let acct = app.pool.all()[0].clone();
            crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0), sample("week", 20.0)], None);
            let refreshed = acct.state.lock().quota.refreshed_at;
            assert!(refreshed.is_some());
            let task = tokio::spawn(worker(app.clone()));
            consumed(&app, &acct).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(acct.state.lock().quota.refreshed_at, refreshed);
            assert_eq!(app.notifications.status(&app)["pending"], 0);
            assert!(app.notifications.status(&app)["logs"].as_array().unwrap().is_empty());
            task.abort();
            let _ = task.await;
        }
    }
    #[tokio::test]
    /// Verify exhausted windows and pending recovery still request authoritative confirmation of missing windows.
    async fn incomplete_confirmation_keeps_refreshing_exhausted_or_pending_recovery() {
        for kind in ["quota.exhausted", "quota.available"] {
            let (_temp, app, acct) = fixture(true);
            seed_pending(&app, &acct, kind);
            let task = tokio::spawn(worker(app.clone()));
            let used = if kind == "quota.exhausted" { 100.0 } else { 20.0 };
            crate::quota::authoritative(&mut acct.state.lock(), vec![sample("week", used)], None);
            consumed(&app, &acct).await;
            assert_waiting(&app);
            acct.state.lock().quota.refreshed_at = Some(Utc::now());
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(acct.state.lock().quota.refreshed_at.is_none());
            assert_waiting(&app);
            task.abort();
            let _ = task.await;
        }
    }
    #[tokio::test]
    /// Verify that persisted available after restart waits for all fresh authoritative windows.
    async fn persisted_available_after_restart_waits_for_all_fresh_authoritative_windows() {
        let (temp, app, acct) = fixture(true);
        seed_pending(&app, &acct, "quota.available");
        let cfg = (*app.cfg()).clone();
        drop(acct);
        drop(app);
        // A new App opens the actual persisted journal and starts a new worker.
        let app = App::new(cfg, temp.0.join("config.yaml"));
        let acct = app.pool.all()[0].clone();
        let task = tokio::spawn(worker(app.clone()));
        acct.state.lock().notification_evidence.observe(&[sample("5h", 10.0)], false);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        assert!(acct.state.lock().quota.refreshed_at.is_none());
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0), sample("week", 20.0)], None);
        until_logs(&app, 1).await;
        let status = app.notifications.status(&app);
        assert_eq!(status["logs"][0]["event"], "quota.available");
        assert_eq!(status["logs"][0]["detail"], "credential_unavailable");
        assert_eq!(status["pending"], 0);
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    /// Verify that reenabled pending recovery discards pre pause confirmation.
    async fn reenabled_pending_recovery_discards_pre_pause_confirmation() {
        let (_temp, app, acct) = fixture(true);
        seed_pending(&app, &acct, "quota.available");
        let task = tokio::spawn(worker(app.clone()));
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        let mut cfg = (*app.cfg()).clone();
        cfg.notifications.enabled = false;
        app.set_config(cfg);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let mut cfg = (*app.cfg()).clone();
        cfg.notifications.enabled = true;
        app.set_config(cfg);
        // Week confirmation alone must not combine with the old pre-pause 5h read.
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("week", 20.0)], None);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0), sample("week", 20.0)], None);
        until_logs(&app, 1).await;
        assert_eq!(app.notifications.status(&app)["logs"][0]["event"], "quota.available");
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    /// Verify that brief account or global pause requires new confirmation even between worker ticks.
    async fn brief_account_or_global_pause_requires_new_confirmation_even_between_worker_ticks() {
        for account_pause in [false, true] {
            let (_temp, app, acct) = fixture(true);
            seed_pending(&app, &acct, "quota.available");
            {
                let mut inner = app.notifications.inner.lock();
                let store = inner.store.as_mut().unwrap();
                store.journal.pending[0].next = Utc::now() + chrono::Duration::hours(1);
                store.save().unwrap();
            }
            let task = tokio::spawn(worker(app.clone()));
            crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0), sample("week", 20.0)], None);
            consumed(&app, &acct).await;
            // Both changes happen synchronously, before the worker can see a disabled tick.
            if account_pause {
                crate::accounts::set_file_disabled(acct.path.as_deref().unwrap(), true).unwrap();
                app.reload_accounts();
                crate::accounts::set_file_disabled(acct.path.as_deref().unwrap(), false).unwrap();
                app.reload_accounts();
            } else {
                let mut cfg = (*app.cfg()).clone();
                cfg.notifications.enabled = false;
                app.set_config(cfg.clone());
                cfg.notifications.enabled = true;
                app.set_config(cfg);
            }
            let acct = app.pool.all()[0].clone();
            crate::quota::authoritative(&mut acct.state.lock(), vec![sample("week", 20.0)], None);
            {
                let mut inner = app.notifications.inner.lock();
                let store = inner.store.as_mut().unwrap();
                store.journal.pending[0].next = Utc::now();
                store.save().unwrap();
            }
            consumed(&app, &acct).await;
            assert_waiting(&app);
            crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
            until_logs(&app, 1).await;
            assert_eq!(app.notifications.status(&app)["logs"][0]["event"], "quota.available");
            task.abort();
            let _ = task.await;
        }
    }
    #[tokio::test]
    /// Verify that persisted partial recovery waits but current partial recovery dispatches.
    async fn persisted_partial_recovery_waits_but_current_partial_recovery_dispatches() {
        let (_temp, app, acct) = fixture(true);
        seed_pending(&app, &acct, "quota.window_recovered");
        let task = tokio::spawn(worker(app.clone()));
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("week", 100.0)], None);
        until_logs(&app, 1).await;
        assert_eq!(app.notifications.status(&app)["logs"][0]["event"], "quota.window_recovered");
        task.abort();
        let _ = task.await;

        let (_temp, app, acct) = fixture(true);
        let task = tokio::spawn(worker(app.clone()));
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 100.0), sample("week", 100.0)], None);
        until_logs(&app, 2).await;
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
        until_logs(&app, 3).await;
        let status = app.notifications.status(&app);
        assert_eq!(status["logs"][2]["event"], "quota.window_recovered");
        {
            let inner = app.notifications.inner.lock();
            assert!(
                inner
                    .store
                    .as_ref()
                    .unwrap()
                    .journal
                    .subscriptions
                    .values()
                    .next()
                    .unwrap()
                    .windows
                    .values()
                    .any(|window| window.name == "week" && window.exhausted)
            );
        }
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    /// Verify that persisted exhaustion requires fresh matching blocking window.
    async fn persisted_exhaustion_requires_fresh_matching_blocking_window() {
        let (_temp, app, acct) = fixture(true);
        seed_pending(&app, &acct, "quota.exhausted");
        let task = tokio::spawn(worker(app.clone()));
        acct.state.lock().notification_evidence.observe(&[sample("week", 20.0)], false);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        acct.state.lock().notification_evidence.observe(&[sample("5h", 100.0)], false);
        until_logs(&app, 1).await;
        assert_eq!(app.notifications.status(&app)["logs"][0]["event"], "quota.exhausted");
        task.abort();
        let _ = task.await;
    }
    #[test]
    /// Verify that unknown recovery confirmation covers shared and learned model windows.
    fn unknown_recovery_confirmation_covers_shared_and_learned_model_windows() {
        let mut subscription = Subscription { provider: "claude".into(), ..Default::default() };
        let mut observation = state::Observation {
            sequence: 1,
            at: Utc::now(),
            windows: Vec::new(),
            authoritative: false,
            unknown: Some("*".into()),
        };
        subscription.apply("id", &observation);
        let opus = crate::quota::Window { model: Some("opus".into()), ..sample("week opus", 0.0) };
        observation.unknown = None;
        observation.authoritative = true;
        observation.windows = vec![sample("5h", 0.0), sample("week", 0.0), opus.clone()];
        observation.at = Utc::now();
        let event = subscription.apply("id", &observation).pop().unwrap();
        assert_eq!(event.event, "quota.available");
        let mut fresh = FreshEvidence::default();
        observation.windows.pop();
        observation.at = Utc::now();
        fresh.record(&subscription, &observation);
        assert!(!fresh.permits(&event, &subscription, true));
        observation.windows.push(opus);
        observation.at = Utc::now();
        fresh.record(&subscription, &observation);
        assert!(fresh.permits(&event, &subscription, true));
    }
    #[tokio::test]
    /// Verify that resumed partial recovery with changed blockers is cancelled.
    async fn resumed_partial_recovery_with_changed_blockers_is_cancelled() {
        let (_temp, app, acct) = fixture(true);
        seed_pending(&app, &acct, "quota.window_recovered");
        let task = tokio::spawn(worker(app.clone()));
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0)], None);
        consumed(&app, &acct).await;
        assert_waiting(&app);
        crate::quota::authoritative(&mut acct.state.lock(), vec![sample("5h", 10.0), sample("week", 20.0)], None);
        until_logs(&app, 2).await;
        let status = app.notifications.status(&app);
        assert_eq!(status["logs"][0]["event"], "quota.window_recovered");
        assert_eq!(status["logs"][0]["outcome"], "cancelled");
        assert_eq!(status["logs"][1]["event"], "quota.available");
        assert_eq!(status["logs"][1]["detail"], "credential_unavailable");
        task.abort();
        let _ = task.await;
    }
}
