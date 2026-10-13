use super::*;
#[test]
fn credential_public_url_is_a_strict_normalized_https_origin() {
    assert_eq!(credential_public_origin(""), Ok(None));
    assert_eq!(
        credential_public_origin("https://DASHBOARD.example.test:443/"),
        Ok(Some("https://dashboard.example.test".into()))
    );
    assert_eq!(credential_public_origin("https://[2001:db8::1]:8443"), Ok(Some("https://[2001:db8::1]:8443".into())));
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
    assert_eq!(credential_public_origin(&format!("https://{}", "a".repeat(301))), Err("credential_public_url_invalid"));
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
    let persisted = std::fs::read_to_string(app.notifications.root.join(".quota-notifications/state.json")).unwrap();
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
        let polled = Utc::now();
        acct.state.lock().quota.refreshed_at = Some(polled);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(acct.state.lock().quota.refreshed_at, Some(polled), "{kind}: a fresh poll is not repeated");
        acct.state.lock().quota.refreshed_at = Some(polled - chrono::Duration::minutes(5));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(acct.state.lock().quota.refreshed_at.is_none(), "{kind}: confirmation polls continue");
        assert_waiting(&app);
        task.abort();
        let _ = task.await;
    }
}
#[tokio::test]
async fn headerless_quota_rejection_does_not_poll_every_minute() {
    let (_temp, app, acct) = fixture(true);
    let task = tokio::spawn(worker(app.clone()));
    let reset = Utc::now() + chrono::Duration::hours(4);
    // A Codex WebSocket rejection carries no usage headers, so its scope is unknown.
    acct.cool_quota("gpt-6.1-sol", reset, "quota exhausted", acct.quota_epoch());
    until_logs(&app, 1).await;
    let windows = vec![
        crate::quota::Window { resets_at: Some(reset), ..sample("5h", 100.0) },
        crate::quota::Window { resets_at: Some(reset + chrono::Duration::days(3)), ..sample("week", 40.0) },
    ];
    crate::quota::authoritative(&mut acct.state.lock(), windows, None);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(acct.state.lock().quota.refreshed_at.is_some(), "a poll that confirms exhaustion is not repeated");
    task.abort();
    let _ = task.await;
}
#[tokio::test]
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
