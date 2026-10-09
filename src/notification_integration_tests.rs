use axum::http::StatusCode;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

struct Temp(PathBuf);

impl Temp {
    /// Create an isolated temporary test directory without using production credentials.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("notification-integration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    /// Release the test task or remove its temporary files when the fixture leaves scope.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Construct a synthetic management fixture with an isolated auth directory and config file.
fn app(temp: &Temp, enabled: bool, secrets_dir: Option<&Path>) -> Arc<crate::state::App> {
    let mut cfg = crate::config::Config {
        auth_dir: temp.0.join("auth").to_string_lossy().into_owned(),
        management_key: "notification-test-management-key".into(),
        ..Default::default()
    };
    cfg.notifications.enabled = enabled;
    cfg.notifications.secrets_dir = secrets_dir.map(|p| p.to_string_lossy().into_owned());
    cfg.notifications.destinations.push(crate::notifications::Destination {
        id: "ops".into(),
        format: crate::notifications::Format::Discord,
        enabled: true,
        chat_id: None,
    });
    std::fs::create_dir_all(cfg.auth_dir()).unwrap();
    crate::state::App::new(cfg, temp.0.join("config.yaml"))
}

/// Provision a synthetic external secret with private permissions on Unix.
fn private_file(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

struct TestServer(tokio::task::JoinHandle<()>);

impl Drop for TestServer {
    /// Release the test task or remove its temporary files when the fixture leaves scope.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run the synthetic management router on an ephemeral loopback port.
async fn serve(app: Arc<crate::state::App>) -> (String, TestServer) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::mgmt::router(app.clone()).with_state(app);
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    (origin, TestServer(task))
}

#[tokio::test]
/// Verify dashboard toggles invalidate in-flight quota responses and pre-pause notification evidence.
async fn dashboard_account_toggle_discards_stale_quota_and_notification_evidence() {
    let temp = Temp::new();
    let app = app(&temp, true, None);
    private_file(
        &app.cfg().auth_dir().join("account.json"),
        &json!({"type":"claude","account_id":"toggle-test-account","access_token":"toggle-test-token","expired":"2099-01-01T00:00:00Z"}).to_string(),
    );
    app.reload_accounts();
    let account = app.pool.all()[0].clone();
    let epoch = account.quota_epoch();
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let route = format!("{origin}/accounts/{}/toggle", account.id);
    let window = crate::quota::Window { name: "5h".into(), used: 10.0, resets_at: None, model: None };

    for disabled in [true, false] {
        account.state.lock().notification_evidence.observe(std::slice::from_ref(&window), true);
        let before = account.quota_epoch();
        let response = client
            .post(&route)
            .bearer_auth("notification-test-management-key")
            .json(&json!({"disabled":disabled}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        {
            let state = account.state.lock();
            assert_eq!(state.disabled, disabled);
            assert_eq!(state.quota_epoch, before + 1);
            assert!(state.notifications_changed_at.is_some());
            assert!(state.notification_evidence.observations.is_empty());
        }
        // The later file-watcher reload must preserve the invalidation already performed by the route.
        app.reload_accounts();
        assert_eq!(app.pool.all()[0].quota_epoch(), before + 1);
    }
    account.cool_quota("claude-sonnet-5-5", chrono::Utc::now() + chrono::Duration::hours(1), "quota", epoch);
    let headers = reqwest::header::HeaderMap::from_iter([(
        "anthropic-ratelimit-unified-5h-status".parse().unwrap(),
        "rejected".parse().unwrap(),
    )]);
    crate::quota::observe(&account, &headers, epoch);
    let state = account.state.lock();
    assert!(state.quota_cooldowns.is_empty());
    assert!(state.notification_evidence.observations.is_empty());
}

#[tokio::test]
/// Verify that notification test blocks loopback and persists only sanitized details.
async fn notification_test_blocks_loopback_and_persists_only_sanitized_details() {
    let temp = Temp::new();
    let secrets = temp.0.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    let sentinel_url = "https://127.0.0.1:9/sentinel-notification-url";
    let sentinel_token = "sentinel-notification-bearer";
    private_file(&secrets.join("ops.url"), sentinel_url);
    private_file(&secrets.join("ops.bearer"), sentinel_token);
    let app = app(&temp, true, Some(&secrets));

    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{origin}/notifications/ops/test"))
        .bearer_auth("notification-test-management-key")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "blocked_address");

    let status = client
        .get(format!("{origin}/notifications"))
        .bearer_auth("notification-test-management-key")
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = status.json().await.unwrap();
    assert_eq!(status["logs"][0]["detail"], "blocked_address");
    assert_eq!(status["logs"][0]["outcome"], "terminal_failure");
    let state = std::fs::read(temp.0.join("auth/.quota-notifications/state.json")).unwrap();
    let persisted = String::from_utf8(state).unwrap();
    for secret in [sentinel_url, sentinel_token] {
        assert!(!persisted.contains(secret));
        assert!(!status.to_string().contains(secret));
    }
}

#[tokio::test]
/// Verify that notification management requires key and test rejects form content type.
async fn notification_management_requires_key_and_test_rejects_form_content_type() {
    let temp = Temp::new();
    let app = app(&temp, false, None);
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();

    let unauthenticated = client.get(format!("{origin}/notifications")).send().await.unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let form = client
        .post(format!("{origin}/notifications/ops/test"))
        .bearer_auth("notification-test-management-key")
        .header(reqwest::header::ORIGIN, "https://attacker.invalid")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(!temp.0.join("auth/.quota-notifications").exists());
}

#[cfg(not(unix))]
#[tokio::test]
/// Verify that platforms without Unix permission protection refuse managed writes and keep external delivery available.
async fn managed_credentials_fail_closed_on_non_unix_without_disabling_external_credentials() {
    let temp = Temp::new();
    let secrets = temp.0.join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    let app = app(&temp, true, Some(&secrets));
    let mut config = (*app.cfg()).clone();
    config.notifications.credential_ui_enabled = true;
    app.set_config(config);
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let route = format!("{origin}/notifications/ops/credentials");
    let response = client
        .put(&route)
        .bearer_auth("notification-test-management-key")
        .header(reqwest::header::ORIGIN, &origin)
        .json(&json!({"url": "https://hooks.example.invalid/windows-sentinel"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[reqwest::header::CACHE_CONTROL], "no-store");
    assert_eq!(response.json::<Value>().await.unwrap()["error"], "managed_credentials_unsupported_platform");
    assert!(!temp.0.join("auth/.notification-credentials").exists());
    assert_eq!(app.notifications.status(&app)["destinations"][0]["credential_editable"], false);
    let removed = client
        .delete(&route)
        .bearer_auth("notification-test-management-key")
        .header(reqwest::header::ORIGIN, &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(removed.json::<Value>().await.unwrap()["error"], "managed_credentials_unsupported_platform");

    private_file(&secrets.join("ops.url"), "https://127.0.0.1:9/windows-external-sentinel");
    let status = app.notifications.status(&app);
    assert_eq!(status["destinations"][0]["credential_source"], "external");
    assert_eq!(status["destinations"][0]["credential_ready"], true);
    assert_eq!(app.notifications.test(&app, "ops").await.unwrap_err(), "blocked_address");
}

#[test]
/// Verify that notification config rejects inline url credentials.
fn notification_config_rejects_inline_url_credentials() {
    let error = crate::config::Config::parse(
        "notifications:\n  enabled: true\n  destinations:\n    - id: ops\n      format: discord\n      url: https://example.invalid/secret-sentinel\n",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"));
}

#[tokio::test]
/// Verify that hot config cannot expand startup notification secret access.
async fn hot_config_cannot_expand_startup_notification_secret_access() {
    let temp = Temp::new();
    let startup_secrets = temp.0.join("startup-secrets");
    let runtime_secrets = temp.0.join("runtime-secrets");
    std::fs::create_dir_all(&startup_secrets).unwrap();
    std::fs::create_dir_all(&runtime_secrets).unwrap();
    // A runtime-only URL must never be used: keep it loopback so even a regression
    // cannot make this test contact an external service.
    private_file(&runtime_secrets.join("ops.url"), "https://127.0.0.1:9/hook");

    let app = app(&temp, true, Some(&startup_secrets));
    let mut hot = (*app.cfg()).clone();
    hot.notifications.secrets_dir = Some(runtime_secrets.to_string_lossy().into_owned());
    hot.notifications.private_endpoints = vec![crate::notifications::PrivateEndpoint {
        host: "127.0.0.1".into(),
        port: 443,
        cidrs: vec!["127.0.0.1/32".into()],
    }];
    app.set_config(hot);

    let status = app.notifications.status(&app);
    assert_eq!(status["destinations"][0]["credential_ready"], false);
    let result = app.notifications.test(&app, "ops").await;
    assert_eq!(result.unwrap_err(), "credential_unavailable");
    let state = std::fs::read_to_string(temp.0.join("auth/.quota-notifications/state.json")).unwrap();
    assert!(state.contains("credential_unavailable"));
}

#[tokio::test]
/// Verify that status does not create notification state when feature is disabled.
async fn status_does_not_create_notification_state_when_feature_is_disabled() {
    let temp = Temp::new();
    let app = app(&temp, false, None);
    let (origin, _server) = serve(app).await;
    let response = reqwest::Client::new()
        .get(format!("{origin}/notifications"))
        .bearer_auth("notification-test-management-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let status: Value = response.json().await.unwrap();
    assert_eq!(status["enabled"], false);
    assert!(!temp.0.join("auth/.quota-notifications").exists());
}

#[tokio::test]
/// Verify that management settings round trip timezone and reject invalid zone.
async fn management_settings_round_trip_timezone_and_reject_invalid_zone() {
    let temp = Temp::new();
    let app = app(&temp, false, None);
    let initial = serde_yaml::to_string(&*app.cfg()).unwrap();
    std::fs::write(&app.cfg_path, initial).unwrap();
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let settings_url = format!("{origin}/config/settings");

    let defaults = client.get(&settings_url).bearer_auth("notification-test-management-key").send().await.unwrap();
    assert_eq!(defaults.status(), StatusCode::OK);
    let defaults: Value = defaults.json().await.unwrap();
    assert_eq!(defaults["values"]["notifications"]["time-zone"], "UTC");
    assert_eq!(defaults["defaults"]["notifications"]["time-zone"], "UTC");

    let mut notifications = defaults["values"]["notifications"].clone();
    notifications["time-zone"] = json!("America/Denver");

    let saved = client
        .patch(&settings_url)
        .bearer_auth("notification-test-management-key")
        .json(&json!({
            "revision": defaults["revision"],
            "changes": {"notifications": notifications}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let saved: Value = saved.json().await.unwrap();
    assert_eq!(saved["values"]["notifications"]["time-zone"], "America/Denver");
    assert_eq!(app.cfg().notifications.time_zone, "America/Denver");
    let persisted = std::fs::read_to_string(&app.cfg_path).unwrap();
    assert_eq!(crate::config::Config::parse(&persisted).unwrap().notifications.time_zone, "America/Denver");

    let mut invalid_notifications = saved["values"]["notifications"].clone();
    invalid_notifications["time-zone"] = json!("Mars/Olympus_Mons");

    let invalid = client
        .patch(&settings_url)
        .bearer_auth("notification-test-management-key")
        .json(&json!({
            "revision": saved["revision"],
            "changes": {"notifications": invalid_notifications}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(crate::config::Config::parse("notifications:\n  time-zone: Mars/Olympus_Mons\n").is_err());
    assert_eq!(std::fs::read_to_string(&app.cfg_path).unwrap(), persisted);
    assert_eq!(app.cfg().notifications.time_zone, "America/Denver");
}
