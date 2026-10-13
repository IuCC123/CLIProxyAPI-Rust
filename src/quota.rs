//! Subscription quota: how much of each usage window (Claude's 5-hour and
//! weekly limits, ChatGPT's Codex windows) an account has used. Read from
//! response headers on every request and from the providers' usage endpoints
//! in the background, so routing can prefer the account with most headroom.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::accounts::{Account, Credential, Provider};
use crate::state::App;

const CLAUDE_USAGE: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Accounts without quota data count as half used.
pub const UNKNOWN: f64 = 50.0;
const POLL_EVERY: i64 = 5 * 60;
/// Response headers keep a busy account's shared windows current, so the usage endpoint
/// is only needed now and then, for the model-specific windows headers don't carry.
const BUSY_POLL_EVERY: i64 = 30 * 60;
/// Banked resets change rarely; opening the dashboard panel checks on demand.
const RESET_POLL_EVERY: i64 = 30 * 60;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Window {
    /// "5h", "week", ...
    pub name: String,
    /// 0-100.
    pub used: f64,
    pub resets_at: Option<DateTime<Utc>>,
    /// Only counts for models whose name contains this ("opus", "sonnet").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Quota {
    pub windows: Vec<Window>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Last accepted usage-endpoint refresh; partial observations do not advance this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Usage-endpoint checks in a row that brought no data, and when the last one ran.
    #[serde(skip)]
    pub(crate) check_failures: u32,
    #[serde(skip)]
    pub(crate) checked_at: Option<DateTime<Utc>>,
}

impl Quota {
    fn live(&self, model: &str) -> impl Iterator<Item = &Window> {
        let now = Utc::now();
        let model = model.to_ascii_lowercase();
        self.windows
            .iter()
            .filter(move |w| w.model.as_ref().is_none_or(|m| model.contains(m.as_str())))
            .filter(move |w| w.resets_at.is_none_or(|r| r > now))
    }

    /// Use of the tightest window that applies to `model` (None = no data).
    pub fn pressure(&self, model: &str) -> Option<f64> {
        self.updated_at?;
        Some(self.live(model).map(|w| w.used).fold(0.0, f64::max))
    }

    pub fn exhausted(&self, model: &str) -> bool {
        self.live(model).any(|w| w.used >= 100.0)
    }

    /// The general weekly renewal, ignoring expired, short and model-specific windows.
    pub fn weekly_reset(&self, model: &str) -> Option<DateTime<Utc>> {
        self.live(model).filter(|w| w.name == "week" && w.model.is_none()).filter_map(|w| w.resets_at).min()
    }

    /// Missing data is unknown; an elapsed window has its allowance available again.
    pub fn five_hour_remaining(&self, model: &str) -> Option<f64> {
        let lower = model.to_ascii_lowercase();
        if !self.windows.iter().any(|w| w.name == "5h" && w.model.as_ref().is_none_or(|m| lower.contains(m))) {
            return None;
        }
        let used = self.live(model).filter(|w| w.name == "5h").map(|w| w.used).fold(0.0, f64::max);
        Some(100.0 - used.clamp(0.0, 100.0))
    }

    /// Allowance left in the tightest weekly window that applies to `model` (None = no data).
    pub fn weekly_remaining(&self, model: &str) -> Option<f64> {
        self.live(model)
            .filter(|w| w.name.starts_with("week"))
            .map(|w| 100.0 - w.used.clamp(0.0, 100.0))
            .reduce(f64::min)
    }

    /// Headroom in the tightest known applicable window, for accounts without 5-hour data.
    pub fn remaining(&self, model: &str) -> Option<f64> {
        let lower = model.to_ascii_lowercase();
        if !self.windows.iter().any(|w| w.model.as_ref().is_none_or(|m| lower.contains(m))) {
            return None;
        }
        let used = self.live(model).map(|w| w.used).fold(0.0, f64::max);
        Some(100.0 - used.clamp(0.0, 100.0))
    }

    #[cfg(test)]
    /// Use the normal routing-only refresh schedule for baseline tests.
    fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.needs_refresh_for(now, false)
    }
    /// Retain failure backoff while polling authoritatively for notifications even on busy subscriptions.
    fn needs_refresh_for(&self, now: DateTime<Utc>, notifications: bool) -> bool {
        // A check that brought no data waits 2, 4, 8 ... up to 30 minutes before the next.
        if self.check_failures > 0
            && let Some(at) = self.checked_at
            && (now - at).num_seconds() < (60i64 << self.check_failures.min(5)).min(BUSY_POLL_EVERY)
        {
            return false;
        }
        let Some(refreshed) = self.refreshed_at else { return true };
        let busy =
            !notifications && self.updated_at.is_some_and(|u| u > refreshed && (now - u).num_seconds() < POLL_EVERY);
        (now - refreshed).num_seconds() >= if busy { BUSY_POLL_EVERY } else { POLL_EVERY }
    }

    fn checked(&mut self, now: DateTime<Utc>, refreshed: bool) {
        self.checked_at = Some(now);
        self.check_failures = if refreshed { 0 } else { self.check_failures.saturating_add(1) };
    }

    /// A window that is used up, and when it resets.
    pub fn exhausted_until(&self, model: &str) -> Option<DateTime<Utc>> {
        self.live(model).filter(|w| w.used >= 100.0).filter_map(|w| w.resets_at).max()
    }

    fn set(&mut self, windows: Vec<Window>, plan: Option<String>) {
        if windows.is_empty() {
            return;
        }
        // Observations can omit any window. Only replace the same window and model scope;
        // missing data must not erase exhaustion or renewal dates learned earlier.
        let retained: Vec<Window> = self
            .windows
            .iter()
            .filter(|w| !windows.iter().any(|n| n.name == w.name && n.model == w.model))
            .cloned()
            .collect();
        self.windows = windows;
        self.windows.extend(retained);
        self.updated_at = Some(Utc::now());
        if plan.is_some() {
            self.plan = plan;
        }
    }
}

fn label(secs: i64) -> String {
    match secs {
        s if s >= 6 * 86_400 => "week".into(),
        s if s >= 20 * 3600 => "day".into(),
        s => format!("{}h", (s + 1800) / 3600),
    }
}

fn ts(secs: i64) -> Option<DateTime<Utc>> {
    (secs > 0).then(|| DateTime::from_timestamp(secs, 0)).flatten()
}

/// Reads the quota headers a Claude or ChatGPT response carries.
pub fn observe(acct: &Account, headers: &reqwest::header::HeaderMap, epoch: u64) {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    let num = |n: &str| h(n).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite());
    let mut windows = Vec::new();
    let mut plan = None;
    match acct.provider {
        Provider::Claude => {
            for (key, name) in [("5h", "5h"), ("7d", "week")] {
                let rejected = h(&format!("anthropic-ratelimit-unified-{key}-status")) == Some("rejected");
                let Some(u) = num(&format!("anthropic-ratelimit-unified-{key}-utilization"))
                    .filter(|v| (0.0..=1.0).contains(v))
                    .or_else(|| rejected.then_some(1.0))
                else {
                    continue;
                };
                let reset = num(&format!("anthropic-ratelimit-unified-{key}-reset")).and_then(|t| ts(t as i64));
                let used = if rejected { 100.0 } else { (u * 100.0).clamp(0.0, 100.0) };
                windows.push(Window { name: name.into(), used, resets_at: reset, model: None });
            }
        }
        Provider::Codex => {
            for which in ["primary", "secondary"] {
                let minutes = num(&format!("x-codex-{which}-window-minutes"))
                    .filter(|v| (1.0..=525600.0).contains(v))
                    .unwrap_or(0.0) as i64;
                let Some(used) =
                    num(&format!("x-codex-{which}-used-percent")).filter(|v| minutes > 0 && (0.0..=100.0).contains(v))
                else {
                    continue;
                };
                let reset = num(&format!("x-codex-{which}-reset-at")).and_then(|t| ts(t as i64)).or_else(|| {
                    num(&format!("x-codex-{which}-reset-after-seconds"))
                        .filter(|s| (0.0..=31536000.0).contains(s))
                        .and_then(|s| Utc::now().checked_add_signed(chrono::Duration::seconds(s as i64)))
                });
                windows.push(Window {
                    name: label(minutes * 60),
                    used: used.clamp(0.0, 100.0),
                    resets_at: reset,
                    model: None,
                });
            }
            plan = h("x-codex-plan-type").map(String::from);
        }
        _ => return,
    }
    let mut st = acct.state.lock();
    if st.quota_epoch == epoch && !st.quota_refreshing {
        if st.notifications_enabled {
            st.notification_evidence.observe(&windows, false);
        }
        st.quota.set(windows, plan);
    }
}

/// Codex websocket sessions report quota as a `codex.rate_limits` event.
pub fn observe_codex_event(acct: &Account, v: &Value, epoch: u64) {
    if v["type"] != "codex.rate_limits" {
        return;
    }
    let rl = if v["rate_limits"].is_object() { &v["rate_limits"] } else { v };
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|which| {
            let w = &rl[*which];
            let minutes = w["window_minutes"].as_i64().filter(|m| (1..=525600).contains(m))?;
            let reset = w["reset_at"].as_i64().and_then(ts).or_else(|| {
                w["reset_after_seconds"]
                    .as_i64()
                    .filter(|s| (0..=31536000).contains(s))
                    .and_then(|s| Utc::now().checked_add_signed(chrono::Duration::seconds(s)))
            });
            Some(Window {
                name: label(minutes * 60),
                used: w["used_percent"].as_f64().filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?,
                resets_at: reset,
                model: None,
            })
        })
        .collect();
    let mut st = acct.state.lock();
    if st.quota_epoch == epoch && !st.quota_refreshing {
        if st.notifications_enabled {
            st.notification_evidence.observe(&windows, false);
        }
        st.quota.set(windows, v["plan_type"].as_str().map(String::from));
    }
}

/// Parse finite Claude shared and model-scoped usage without inventing missing windows.
fn claude_windows(v: &Value) -> Vec<Window> {
    let rfc = |s: &Value| s.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc));
    [
        ("five_hour", "5h", None),
        ("seven_day", "week", None),
        ("seven_day_opus", "week opus", Some("opus")),
        ("seven_day_sonnet", "week sonnet", Some("sonnet")),
        ("seven_day_overage_included", "week overage", None),
    ]
    .iter()
    .filter_map(|(key, name, model)| {
        let w = &v[*key];
        Some(Window {
            name: (*name).into(),
            used: w["utilization"].as_f64().filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?,
            resets_at: rfc(&w["resets_at"]),
            model: model.map(String::from),
        })
    })
    .collect()
}

/// Parse bounded Codex usage windows and authoritative reached-limit indicators.
fn codex_windows(v: &Value) -> Vec<Window> {
    let rl = &v["rate_limit"];
    let reached = rl["limit_reached"] == true;
    ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let w = &rl[*key];
            let secs = w["limit_window_seconds"].as_i64().filter(|s| (1..=31536000).contains(s))?;
            let used = w["used_percent"].as_f64().filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?;
            let reset = w["reset_at"].as_i64().and_then(ts);
            let used = if reached && used >= 99.0 { 100.0 } else { used.clamp(0.0, 100.0) };
            Some(Window { name: label(secs), used, resets_at: reset, model: None })
        })
        .collect()
}

/// Asks the provider's usage endpoint (free, no tokens) for current quota.
pub async fn poll(app: &App, acct: &Arc<Account>) -> anyhow::Result<()> {
    let epoch = acct.quota_epoch();
    let started = Utc::now();
    let (token, account_id) = match &*acct.cred.read() {
        Credential::OAuth(o) if o.base_url.is_none() => (o.access_token.clone(), o.account_id.clone()),
        _ => return Ok(()),
    };
    let http = app.http.client(acct.proxy_url.as_deref());
    let (windows, plan) = match acct.provider {
        Provider::Claude => {
            let v: Value = http
                .get(CLAUDE_USAGE)
                .bearer_auth(&token)
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("user-agent", crate::upstream::CC_USER_AGENT)
                .timeout(Duration::from_secs(15))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            (claude_windows(&v), None)
        }
        Provider::Codex => {
            let mut rb = http
                .get(CODEX_USAGE)
                .bearer_auth(&token)
                .header("user-agent", crate::upstream::CODEX_USER_AGENT)
                .header("originator", crate::upstream::CODEX_ORIGINATOR)
                .timeout(Duration::from_secs(15));
            if let Some(id) = &account_id {
                rb = rb.header("chatgpt-account-id", id);
            }
            let v: Value = rb.send().await?.error_for_status()?.json().await?;
            (codex_windows(&v), v["plan_type"].as_str().map(String::from))
        }
        _ => return Ok(()),
    };
    let mut st = acct.state.lock();
    if st.quota_epoch == epoch && !st.quota_refreshing {
        st.quota_epoch += 1;
        authoritative_at(&mut st, windows, plan, started);
    }
    Ok(())
}

/// Provider usage is authoritative, but absent windows cannot prove a cooldown has ended.
pub fn authoritative(st: &mut crate::accounts::AccountState, windows: Vec<Window>, plan: Option<String>) {
    authoritative_at(st, windows, plan, Utc::now());
}
/// Merge provider usage using the poll start time and preserve windows the provider did not report.
fn authoritative_at(
    st: &mut crate::accounts::AccountState,
    windows: Vec<Window>,
    plan: Option<String>,
    observed_at: DateTime<Utc>,
) {
    if windows.is_empty() {
        return;
    }
    if st.notifications_enabled {
        st.notification_evidence.observe_at(&windows, true, observed_at);
    }
    let covered: Vec<String> = st
        .quota_cooldowns
        .keys()
        .filter(|model| {
            st.quota
                .windows
                .iter()
                .filter(|old| old.model.as_ref().is_none_or(|m| model.contains(m)))
                .all(|old| windows.iter().any(|new| new.name == old.name && new.model == old.model))
        })
        .cloned()
        .collect();
    st.quota.set(windows, plan);
    st.quota.refreshed_at = st.quota.updated_at;
    let quota = &st.quota;
    st.quota_cooldowns.retain(|model, _| {
        !covered.contains(model) || quota.pressure(if model == "*" { "" } else { model }).is_none_or(|u| u >= 100.0)
    });
}

pub fn usage(st: &mut crate::accounts::AccountState, provider: Provider, value: &Value) {
    let (windows, plan) = match provider {
        Provider::Claude => (claude_windows(value), None),
        Provider::Codex => (codex_windows(value), value["plan_type"].as_str().map(String::from)),
        _ => return,
    };
    authoritative(st, windows, plan);
}

/// Keeps quota fresh for signed-in Claude and ChatGPT accounts.
pub async fn poller(app: Arc<App>) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    loop {
        let mut changed = false;
        for acct in app.pool.all() {
            // Learn an Antigravity account's real model list without waiting for a request.
            if acct.provider == Provider::Antigravity
                && acct.is_oauth()
                && acct.discovered.read().is_empty()
                && !acct.state.lock().disabled
            {
                match crate::oauth::ensure_ready(&app, &acct).await {
                    Ok(()) => changed |= !acct.discovered.read().is_empty(),
                    Err(e) => tracing::debug!(account = %acct.label, "antigravity setup failed: {e:#}"),
                }
            }
            if !matches!(acct.provider, Provider::Claude | Provider::Codex) || !acct.is_oauth() {
                continue;
            }
            let reset_stale = app.cfg().banked_resets && {
                let st = acct.state.lock();
                !st.disabled
                    && st
                        .banked_resets
                        .as_ref()
                        .is_none_or(|v| (Utc::now() - v.checked_at).num_seconds() >= RESET_POLL_EVERY)
            };
            if reset_stale {
                let _ = crate::banked_resets::refresh(&app, &acct).await;
                changed = true;
            }
            let now = Utc::now();
            let reset_due = app.cfg().notifications.enabled && app.notifications.reset_due(&acct, now);
            let stale = {
                let st = acct.state.lock();
                let deadline_check = reset_due
                    && st.quota.check_failures == 0
                    && st.quota.checked_at.is_none_or(|checked| (now - checked).num_seconds() >= 60);
                !st.disabled && (st.quota.needs_refresh_for(now, app.cfg().notifications.enabled) || deadline_check)
            };
            if !stale || crate::oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
                continue;
            }
            let before = acct.state.lock().quota.refreshed_at;
            let result = poll(&app, &acct).await;
            {
                let mut st = acct.state.lock();
                let refreshed = st.quota.refreshed_at.is_some() && st.quota.refreshed_at != before;
                st.quota.checked(Utc::now(), refreshed);
            }
            match result {
                Ok(()) => changed = true,
                Err(e) => tracing::debug!(account = %acct.label, "quota check failed: {e:#}"),
            }
        }
        if changed {
            app.broadcast("accounts", Value::Null);
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_headers_preserve_known_five_hour_exhaustion() {
        let reset = Utc::now() + chrono::Duration::hours(4);
        let mut q = Quota::default();
        q.set(vec![Window { name: "5h".into(), used: 100.0, resets_at: Some(reset), model: None }], None);
        q.set(
            vec![Window {
                name: "week".into(),
                used: 20.0,
                resets_at: Some(reset + chrono::Duration::days(3)),
                model: None,
            }],
            None,
        );
        assert!(q.exhausted("gpt-6.1-sol"));
        assert_eq!(q.five_hour_remaining("gpt-6.1-sol"), Some(0.0));
    }

    #[test]
    fn partial_headers_preserve_weekly_priority() {
        let reset = Utc::now() + chrono::Duration::days(3);
        let mut q = Quota::default();
        q.set(vec![Window { name: "week".into(), used: 33.0, resets_at: Some(reset), model: None }], None);
        q.set(
            vec![Window {
                name: "5h".into(),
                used: 0.0,
                resets_at: Some(reset - chrono::Duration::days(2)),
                model: None,
            }],
            None,
        );
        assert_eq!(q.weekly_reset("claude-sonnet-5-5"), Some(reset));
    }

    #[test]
    fn partial_observations_only_slow_the_usage_refresh() {
        let cfg = crate::config::Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![crate::config::KeyEntry { api_key: "a".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = crate::accounts::Pool::default();
        pool.reload(&cfg);
        let acct = &pool.all()[0];
        let headers = reqwest::header::HeaderMap::from_iter([
            (reqwest::header::HeaderName::from_static("x-codex-primary-window-minutes"), "10080".parse().unwrap()),
            (reqwest::header::HeaderName::from_static("x-codex-primary-used-percent"), "20".parse().unwrap()),
        ]);
        observe(acct, &headers, acct.quota_epoch());
        assert!(acct.state.lock().quota.needs_refresh(Utc::now())); // headers cannot replace the first poll
        let usage_value = json!({"rate_limit":{"primary_window":{"limit_window_seconds":604800,"used_percent":20}}});
        let refreshed;
        {
            let mut st = acct.state.lock();
            usage(&mut st, Provider::Codex, &usage_value);
            refreshed = st.quota.refreshed_at.unwrap();
            assert!(!st.quota.needs_refresh(refreshed + chrono::Duration::seconds(POLL_EVERY - 1)));
            assert!(st.quota.needs_refresh(refreshed + chrono::Duration::seconds(POLL_EVERY)));
        }
        observe(acct, &headers, acct.quota_epoch());
        let event = json!({"type":"codex.rate_limits","primary":{"window_minutes":300,"used_percent":95,"reset_after_seconds":3600}});
        observe_codex_event(acct, &event, acct.quota_epoch());
        {
            let st = acct.state.lock();
            assert_eq!(st.quota.refreshed_at, Some(refreshed));
            // A busy account still refreshes its model-specific windows, just less often.
            assert!(!st.quota.needs_refresh(refreshed + chrono::Duration::seconds(POLL_EVERY)));
            assert!(st.quota.needs_refresh(refreshed + chrono::Duration::seconds(BUSY_POLL_EVERY)));
            assert_eq!(st.quota.windows.len(), 2); // WebSocket update also retains the weekly window
            assert_eq!(st.quota.five_hour_remaining("gpt-6.1-sol"), Some(5.0));
        }
        let mut st = acct.state.lock();
        usage(&mut st, Provider::Codex, &json!({}));
        assert_eq!(st.quota.refreshed_at, Some(refreshed)); // empty data does not claim a refresh
        st.quota.refreshed_at = Some(refreshed - chrono::Duration::seconds(POLL_EVERY));
        usage(&mut st, Provider::Codex, &usage_value);
        assert!(!st.quota.needs_refresh(Utc::now()));
        assert_eq!(st.quota.five_hour_remaining("gpt-6.1-sol"), Some(5.0));
    }

    #[test]
    fn usage_checks_that_bring_no_data_back_off() {
        let now = Utc::now();
        let mut q = Quota::default();
        assert!(q.needs_refresh(now));
        q.checked(now, false);
        assert!(!q.needs_refresh(now + chrono::Duration::seconds(119)));
        assert!(q.needs_refresh(now + chrono::Duration::seconds(120)));
        for _ in 0..10 {
            q.checked(now, false);
        }
        assert!(!q.needs_refresh(now + chrono::Duration::seconds(BUSY_POLL_EVERY - 1)));
        assert!(q.needs_refresh(now + chrono::Duration::seconds(BUSY_POLL_EVERY)));
        // A successful check ends the backoff; a banked reset can still force the next one.
        q.refreshed_at = Some(now);
        q.checked(now, true);
        assert!(q.needs_refresh(now + chrono::Duration::seconds(POLL_EVERY)));
        q.refreshed_at = None;
        assert!(q.needs_refresh(now));
    }

    #[test]
    fn partial_updates_keep_model_scopes_and_expired_limits_stop_applying() {
        let now = Utc::now();
        let week =
            Window { name: "week".into(), used: 40.0, resets_at: Some(now + chrono::Duration::days(3)), model: None };
        let mut st = crate::accounts::AccountState::default();
        authoritative(
            &mut st,
            vec![
                week.clone(),
                Window { model: Some("opus".into()), used: 100.0, ..week.clone() },
                Window {
                    name: "5h".into(),
                    used: 100.0,
                    resets_at: Some(now - chrono::Duration::seconds(1)),
                    model: None,
                },
            ],
            None,
        );
        st.quota_cooldowns.insert("claude-opus-5-5".into(), now + chrono::Duration::hours(1));
        authoritative(&mut st, vec![Window { used: 20.0, ..week }], None);
        assert_eq!(st.quota.windows.len(), 3);
        assert!(st.quota_cooldowns.contains_key("claude-opus-5-5"));
        assert_eq!(st.quota.remaining("claude-opus-5-5"), Some(0.0));
        assert_eq!(st.quota.remaining("claude-sonnet-5-5"), Some(80.0));
        assert_eq!(st.quota.five_hour_remaining("claude-sonnet-5-5"), Some(100.0));
        assert!(!st.quota.exhausted("claude-sonnet-5-5"));
    }

    #[test]
    fn remaining_requires_applicable_data_and_recognizes_elapsed_windows() {
        let mut q = Quota::default();
        assert_eq!(q.remaining("claude-sonnet-5-5"), None);
        q.set(vec![Window { name: "week opus".into(), used: 80.0, resets_at: None, model: Some("opus".into()) }], None);
        assert_eq!(q.remaining("claude-sonnet-5-5"), None);
        assert_eq!(q.remaining("CLAUDE-OPUS-5-5"), Some(20.0));
        q.windows[0].resets_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(q.remaining("claude-opus-5-5"), Some(100.0));
        assert_eq!(q.five_hour_remaining("claude-opus-5-5"), None);
    }

    #[test]
    fn weekly_reset_ignores_other_windows_and_expired_dates() {
        let now = Utc::now();
        let weekly = now + chrono::Duration::days(3);
        let mut q = Quota::default();
        assert_eq!(q.weekly_reset("claude-opus-5-5"), None);
        q.set(
            vec![
                Window { name: "5h".into(), used: 5.0, resets_at: Some(now + chrono::Duration::hours(1)), model: None },
                Window {
                    name: "week opus".into(),
                    used: 5.0,
                    resets_at: Some(now + chrono::Duration::days(1)),
                    model: Some("opus".into()),
                },
                Window { name: "week".into(), used: 33.0, resets_at: Some(weekly), model: None },
            ],
            None,
        );
        assert_eq!(q.weekly_reset("claude-opus-5-5"), Some(weekly));
        q.windows[2].resets_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(q.weekly_reset("claude-opus-5-5"), None);
        q.windows[2].resets_at = None;
        assert_eq!(q.weekly_reset("claude-opus-5-5"), None);
    }

    #[test]
    fn usage_endpoints_parse() {
        let claude = json!({
            "five_hour": { "utilization": 81.0, "resets_at": "2099-10-02T12:09:59.520850+00:00" },
            "seven_day": { "utilization": 56.0, "resets_at": "2099-10-05T18:59:59+00:00" },
            "seven_day_opus": null,
        });
        let w = claude_windows(&claude);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].name.as_str(), w[0].used), ("5h", 81.0));
        assert_eq!(w[1].name, "week");

        let codex = json!({ "rate_limit": { "limit_reached": false,
            "primary_window": { "used_percent": 31, "limit_window_seconds": 604800, "reset_at": 4102444800i64 },
            "secondary_window": null } });
        let w = codex_windows(&codex);
        assert_eq!((w[0].name.as_str(), w[0].used), ("week", 31.0));
    }

    #[test]
    fn pressure_uses_the_tightest_live_window() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let past = Utc::now() - chrono::Duration::hours(1);
        let mut q = Quota::default();
        assert_eq!(q.pressure("claude-opus-5-5"), None);
        q.set(
            vec![
                Window { name: "5h".into(), used: 81.0, resets_at: Some(future), model: None },
                Window { name: "week".into(), used: 56.0, resets_at: Some(future), model: None },
                Window { name: "old".into(), used: 99.0, resets_at: Some(past), model: None },
                Window { name: "week opus".into(), used: 100.0, resets_at: Some(future), model: Some("opus".into()) },
            ],
            None,
        );
        assert_eq!(q.pressure("claude-sonnet-5-5"), Some(81.0));
        assert_eq!(q.pressure("claude-opus-5-5"), Some(100.0));
        assert_eq!(q.exhausted_until("claude-opus-5-5"), Some(future));
        assert_eq!(q.exhausted_until("claude-sonnet-5-5"), None);
    }
}
