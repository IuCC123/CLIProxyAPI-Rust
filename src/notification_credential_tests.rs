use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const KEY: &str = "notification-credential-management-key";

struct Temp(PathBuf);

impl Temp {
    /// Create an isolated temporary test directory without using production credentials.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("notification-credential-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}

impl Drop for Temp {
    /// Release the test task or remove its temporary files when the fixture leaves scope.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Construct a synthetic management fixture with an isolated auth directory and config file.
fn app(temp: &Temp, secrets_dir: Option<&Path>) -> Arc<crate::state::App> {
    let mut cfg = crate::config::Config {
        auth_dir: temp.0.join("auth").to_string_lossy().into_owned(),
        management_key: KEY.into(),
        ..Default::default()
    };
    cfg.notifications.enabled = true;
    cfg.notifications.credential_ui_enabled = true;
    cfg.notifications.secrets_dir = secrets_dir.map(|path| path.to_string_lossy().into_owned());
    cfg.notifications.destinations.push(crate::notifications::Destination {
        id: "ops".into(),
        format: crate::notifications::Format::Discord,
        enabled: true,
        chat_id: None,
    });
    std::fs::create_dir_all(cfg.auth_dir()).unwrap();
    let app = crate::state::App::new(cfg, temp.0.join("config.yaml"));
    std::fs::write(&app.cfg_path, serde_yaml::to_string(&*app.cfg()).unwrap()).unwrap();
    app
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
    let router = Router::new().nest("/api", crate::mgmt::router(app.clone())).with_state(app);
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    (origin, TestServer(task))
}

/// Build the management route for a synthetic destination.
fn credential_route(origin: &str, id: &str) -> String {
    format!("{origin}/api/notifications/{id}/credentials")
}

/// Attach synthetic management authentication and same-origin headers to a test request.
fn authenticated(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    origin: &str,
) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, origin)
        .header("sec-fetch-site", "same-origin")
}

/// Provision a synthetic external secret with private permissions on Unix.
fn private_file(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[cfg(unix)]
/// Verify that a managed test bundle is readable only by its Unix owner.
fn assert_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "managed credential files must be mode 0600");
}

#[cfg(unix)]
/// Verify that a managed test directory excludes group and other Unix access.
fn assert_private_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "managed credential directories must be mode 0700");
}

#[tokio::test]
/// Verify that managed credentials save replace clear reload and delete without leaking secrets.
async fn managed_credentials_save_replace_clear_reload_and_delete_without_leaking_secrets() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let config_before = std::fs::read(&app.cfg_path).unwrap();
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let first_url = "https://hooks.example.invalid/first-url-sentinel";
    let first_token = "first-bearer-token-sentinel";
    let second_url = "https://hooks.example.invalid/replacement-url-sentinel";

    let saved = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url": first_url, "bearer_token": first_token}))
        .send()
        .await
        .unwrap();
    assert!(saved.status().is_success());
    assert_eq!(saved.headers().get(reqwest::header::CACHE_CONTROL).unwrap(), "no-store");
    let saved: Value = saved.json().await.unwrap();
    assert_eq!(saved["saved"], true);
    for sentinel in [first_url, first_token] {
        assert!(!saved.to_string().contains(sentinel));
    }

    let credential_path = temp.0.join("auth/.notification-credentials/ops.json");
    let first_bytes = std::fs::read(&credential_path).expect("managed credential file should be created");
    assert_private_file(&credential_path);
    assert_private_directory(credential_path.parent().unwrap());
    let first_record: Value = serde_json::from_slice(&first_bytes).unwrap();
    assert_eq!(first_record["url"], first_url);
    assert_eq!(first_record["bearer_token"], first_token);

    let response = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url": second_url}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.headers().get(reqwest::header::CACHE_CONTROL).unwrap(), "no-store");
    let replacement_response = response.json::<Value>().await.unwrap();
    assert_eq!(replacement_response["saved"], true);
    assert!(!replacement_response.to_string().contains(first_token));
    assert!(!replacement_response.to_string().contains(second_url));
    let replaced: Value = serde_json::from_slice(&std::fs::read(&credential_path).unwrap()).unwrap();
    assert_eq!(replaced["url"], second_url);
    assert!(replaced.get("bearer_token").is_none_or(Value::is_null));
    assert!(!String::from_utf8_lossy(&std::fs::read(&credential_path).unwrap()).contains(first_token));

    let config = (*app.cfg()).clone();
    let reloaded = crate::state::App::new(config, app.cfg_path.clone());
    let (reload_origin, _reload_server) = serve(reloaded.clone()).await;
    let status = client.get(format!("{reload_origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = status.json().await.unwrap();
    assert_eq!(status["destinations"][0]["credential_source"], "managed");
    assert_eq!(status["destinations"][0]["credential_configured"], true);
    assert_eq!(status["destinations"][0]["credential_ready"], true);
    assert_eq!(status["destinations"][0]["credential_editable"], true);
    for sentinel in [first_url, first_token, second_url] {
        assert!(!status.to_string().contains(sentinel));
    }

    for endpoint in ["/api/config", "/api/config/settings"] {
        let response = client.get(format!("{reload_origin}{endpoint}")).bearer_auth(KEY).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        for sentinel in [first_url, first_token, second_url] {
            assert!(!body.contains(sentinel));
        }
    }
    let read_endpoint = client.get(credential_route(&reload_origin, "ops")).bearer_auth(KEY).send().await.unwrap();
    assert!(!read_endpoint.status().is_success(), "credential values must have no read endpoint");
    assert_eq!(std::fs::read(&app.cfg_path).unwrap(), config_before, "credential updates must not rewrite YAML");
    assert!(!temp.0.join("auth/.quota-notifications").exists(), "credential writes must not touch the event journal");

    let deleted =
        authenticated(&client, reqwest::Method::DELETE, &credential_route(&reload_origin, "ops"), &reload_origin)
            .send()
            .await
            .unwrap();
    assert!(deleted.status().is_success());
    let deleted: Value = deleted.json().await.unwrap();
    assert_eq!(deleted["removed"], true);
    assert!(!credential_path.exists());
}

/// Submit a synthetic raw YAML edit through the authenticated management API.
async fn put_raw_config(client: &reqwest::Client, origin: &str, cfg: &crate::config::Config) -> reqwest::Response {
    let text = serde_yaml::to_string(cfg).unwrap();
    authenticated(client, reqwest::Method::PUT, &format!("{origin}/api/config"), origin)
        .json(&json!({"text": text}))
        .send()
        .await
        .unwrap()
}

/// Submit a synthetic destination edit through the structured settings API.
async fn patch_destinations(client: &reqwest::Client, origin: &str, destinations: Value) -> reqwest::Response {
    let settings: Value = client
        .get(format!("{origin}/api/config/settings"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    authenticated(client, reqwest::Method::PATCH, &format!("{origin}/api/config/settings"), origin)
        .json(&json!({
            "revision": settings["revision"],
            "changes": {"notifications": {"destinations": destinations}}
        }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
/// Verify that saved credentials block raw config delete and rename until removed.
async fn saved_credentials_block_raw_config_delete_and_rename_until_removed() {
    let temp = Temp::new();
    let current_app = app(&temp, None);
    let (origin, _server) = serve(current_app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let sentinel = "lifecycle-credential-sentinel";
    let saved = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url":"https://hooks.example.invalid/lifecycle", "bearer_token":sentinel}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let before = std::fs::read(&current_app.cfg_path).unwrap();
    let expected = "Remove saved notification credentials before deleting or renaming a destination.";

    for replacement_id in [None, Some("renamed")] {
        let mut changed = (*current_app.cfg()).clone();
        if let Some(id) = replacement_id {
            changed.notifications.destinations[0].id = id.to_owned();
        } else {
            changed.notifications.destinations.clear();
        }
        let response = put_raw_config(&client, &origin, &changed).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.text().await.unwrap();
        assert!(body.contains(expected));
        assert!(!body.contains(sentinel));
        assert_eq!(
            std::fs::read(&current_app.cfg_path).unwrap(),
            before,
            "rejected raw config must leave disk unchanged"
        );
    }

    let deleted = authenticated(&client, reqwest::Method::DELETE, &route, &origin).send().await.unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let mut removed = (*current_app.cfg()).clone();
    removed.notifications.destinations.clear();
    assert_eq!(put_raw_config(&client, &origin, &removed).await.status(), StatusCode::OK);
    let mut reused = (*current_app.cfg()).clone();
    reused.notifications.destinations.push(crate::notifications::Destination {
        id: "ops".into(),
        format: crate::notifications::Format::Discord,
        enabled: true,
        chat_id: None,
    });
    assert_eq!(put_raw_config(&client, &origin, &reused).await.status(), StatusCode::OK);
    let status: Value =
        client.get(format!("{origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap().json().await.unwrap();
    assert_eq!(status["destinations"][0]["credential_source"], "none");
}

#[tokio::test]
/// Verify that managed credentials block structured destination changes but removal unblocks patch.
async fn managed_credentials_block_structured_destination_changes_but_removal_unblocks_patch() {
    let temp = Temp::new();
    let current_app = app(&temp, None);
    let (origin, _server) = serve(current_app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let sentinel = "patch-lifecycle-secret-sentinel";
    assert_eq!(
        authenticated(&client, reqwest::Method::PUT, &route, &origin)
            .json(&json!({"url":"https://hooks.example.invalid/patch", "bearer_token":sentinel}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let before = std::fs::read(&current_app.cfg_path).unwrap();
    for destinations in [json!([]), json!([{"id":"renamed","format":"discord","enabled":true}])] {
        let response = patch_destinations(&client, &origin, destinations).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.text().await.unwrap();
        assert!(body.contains("Remove saved notification credentials before deleting or renaming a destination."));
        assert!(!body.contains(sentinel));
        assert_eq!(std::fs::read(&current_app.cfg_path).unwrap(), before);
    }
    assert_eq!(
        authenticated(&client, reqwest::Method::DELETE, &route, &origin).send().await.unwrap().status(),
        StatusCode::OK
    );
    assert!(patch_destinations(&client, &origin, json!([])).await.status().is_success());
}

#[tokio::test]
/// Verify that orphaned managed credentials block id reuse until deleted but external only does not.
async fn orphaned_managed_credentials_block_id_reuse_until_deleted_but_external_only_does_not() {
    let temp = Temp::new();
    let current_app = app(&temp, None);
    let (origin, _server) = serve(current_app.clone()).await;
    let client = reqwest::Client::new();
    let creds_dir = temp.0.join("auth/.notification-credentials");
    std::fs::create_dir_all(&creds_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    private_file(
        &creds_dir.join("reserved.json"),
        r#"{"url":"https://hooks.example.invalid/orphan","bearer_token":"orphan-secret-sentinel"}"#,
    );
    let mut cfg = (*current_app.cfg()).clone();
    cfg.notifications.destinations.push(crate::notifications::Destination {
        id: "reserved".into(),
        format: crate::notifications::Format::Discord,
        enabled: true,
        chat_id: None,
    });
    let config_before = std::fs::read(&current_app.cfg_path).unwrap();
    let response = put_raw_config(&client, &origin, &cfg).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response.text().await.unwrap();
    assert!(body.contains("This destination ID already has saved credentials. Remove them before reusing the ID."));
    assert!(!body.contains("orphan-secret-sentinel"));
    assert_eq!(std::fs::read(&current_app.cfg_path).unwrap(), config_before);

    let removed = authenticated(&client, reqwest::Method::DELETE, &credential_route(&origin, "reserved"), &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    assert_eq!(put_raw_config(&client, &origin, &cfg).await.status(), StatusCode::OK);

    let external_temp = Temp::new();
    let external_dir = external_temp.0.join("external");
    std::fs::create_dir_all(&external_dir).unwrap();
    let external_path = external_dir.join("ops.json");
    private_file(
        &external_path,
        r#"{"url":"https://hooks.example.invalid/external","bearer_token":"external-secret"}"#,
    );
    let external_app = app(&external_temp, Some(&external_dir));
    let (external_origin, _external_server) = serve(external_app.clone()).await;
    let mut external_cfg = (*external_app.cfg()).clone();
    external_cfg.notifications.destinations.clear();
    assert_eq!(put_raw_config(&client, &external_origin, &external_cfg).await.status(), StatusCode::OK);
    assert!(external_path.exists(), "removing a destination must not alter externally managed credentials");
}

#[tokio::test]
/// Verify that managed credential api requires bearer auth and rejects origin and forwarded header spoofing.
async fn managed_credential_api_requires_bearer_auth_and_rejects_origin_and_forwarded_header_spoofing() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let body = json!({"url":"https://hooks.example.invalid/sentinel"});

    let no_auth = client.put(&route).json(&body).send().await.unwrap();
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
    let query_auth = client
        .put(format!("{route}?key={KEY}"))
        .header(reqwest::header::ORIGIN, &origin)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(!query_auth.status().is_success(), "query parameter authentication must not authorize credential writes");

    let cross_origin = authenticated(&client, reqwest::Method::PUT, &route, "https://attacker.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
    let cross_site = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .header("sec-fetch-site", "cross-site")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    let forged_proxy = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-host", "attacker.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(forged_proxy.status(), StatusCode::FORBIDDEN);
    let same_site = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .header("sec-fetch-site", "same-site")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(same_site.status(), StatusCode::FORBIDDEN);
    assert!(!temp.0.join("auth/.notification-credentials").exists());
}

#[tokio::test]
/// Verify that loopback http accepts a localhost host without forwarded headers.
async fn loopback_http_accepts_a_localhost_host_without_forwarded_headers() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app).await;
    let port = origin.rsplit(':').next().unwrap();
    let local_origin = format!("http://localhost:{port}");
    let route = credential_route(&origin, "ops");
    let response = reqwest::Client::new()
        .put(route)
        .bearer_auth(KEY)
        .header(reqwest::header::HOST, format!("localhost:{port}"))
        .header(reqwest::header::ORIGIN, &local_origin)
        .header("sec-fetch-site", "same-origin")
        .json(&json!({"url":"https://hooks.example.invalid/local-http"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
/// Verify that credential ui is opt in hot reloadable and disabling it preserves saved secret.
async fn credential_ui_is_opt_in_hot_reloadable_and_disabling_it_preserves_saved_secret() {
    assert!(!crate::notifications::Config::default().credential_ui_enabled);
    let temp = Temp::new();
    let initially_enabled = app(&temp, None);
    let mut disabled_config = (*initially_enabled.cfg()).clone();
    disabled_config.notifications.credential_ui_enabled = false;
    let disabled_app = crate::state::App::new(disabled_config.clone(), initially_enabled.cfg_path.clone());
    std::fs::write(&disabled_app.cfg_path, serde_yaml::to_string(&disabled_config).unwrap()).unwrap();
    let (origin, _server) = serve(disabled_app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let secret = "ui-gated-secret-sentinel";
    let request = || {
        authenticated(&client, reqwest::Method::PUT, &route, &origin)
            .json(&json!({"url":format!("https://hooks.example.invalid/{secret}")}))
    };

    let disabled_status = client.get(format!("{origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(disabled_status.status(), StatusCode::OK);
    let disabled_status: Value = disabled_status.json().await.unwrap();
    assert_eq!(disabled_status["credential_ui_enabled"], false);
    assert_eq!(disabled_status["credential_ui_ready"], false);
    assert_eq!(disabled_status["credential_ui_reason"], "credential_ui_disabled");
    assert_eq!(disabled_status["credential_public_url"], "");
    assert!(disabled_status.get("credential_proxy_peer").is_none());
    assert!(disabled_status.get("credential_proxy_cidr").is_none());

    let disabled_put = request().send().await.unwrap();
    assert_eq!(disabled_put.status(), StatusCode::FORBIDDEN);
    assert!(!disabled_put.text().await.unwrap().contains(secret));
    let disabled_delete = authenticated(&client, reqwest::Method::DELETE, &route, &origin).send().await.unwrap();
    assert_eq!(disabled_delete.status(), StatusCode::FORBIDDEN);
    assert!(!temp.0.join("auth/.notification-credentials").exists());

    let mut hot_enabled = (*disabled_app.cfg()).clone();
    hot_enabled.notifications.credential_ui_enabled = true;
    disabled_app.set_config(hot_enabled);
    let saved = request().send().await.unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let credential_path = temp.0.join("auth/.notification-credentials/ops.json");
    let saved_bytes = std::fs::read(&credential_path).unwrap();
    assert!(String::from_utf8_lossy(&saved_bytes).contains(secret));

    let mut hot_disabled = (*disabled_app.cfg()).clone();
    hot_disabled.notifications.credential_ui_enabled = false;
    disabled_app.set_config(hot_disabled);
    let disabled_put = request().send().await.unwrap();
    assert_eq!(disabled_put.status(), StatusCode::FORBIDDEN);
    assert!(!disabled_put.text().await.unwrap().contains(secret));
    let disabled_delete = authenticated(&client, reqwest::Method::DELETE, &route, &origin).send().await.unwrap();
    assert_eq!(disabled_delete.status(), StatusCode::FORBIDDEN);
    assert_eq!(std::fs::read(&credential_path).unwrap(), saved_bytes);
}

#[tokio::test]
/// Verify that only startup trusted proxy cidrs can assert a single https forwarded proto.
async fn only_startup_trusted_proxy_cidrs_can_assert_a_single_https_forwarded_proto() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let cfg_path = app.cfg_path.clone();
    let mut hot_config = (*app.cfg()).clone();
    hot_config.notifications.credential_proxy_cidrs = vec!["127.0.0.1/32".into()];
    app.set_config(hot_config.clone());
    let (origin, _hot_server) = serve(app).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let (_, host_port) = origin.split_once("://").unwrap();
    let forwarded_origin = format!("https://{host_port}");
    let body = json!({"url":"https://hooks.example.invalid/trusted-proxy"});

    let hot_change = client
        .put(&route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, &forwarded_origin)
        .header("sec-fetch-site", "same-origin")
        .header("x-forwarded-proto", "https")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(hot_change.status(), StatusCode::FORBIDDEN, "proxy trust must be startup-only");
    assert!(!temp.0.join("auth/.notification-credentials").exists());

    std::fs::write(&cfg_path, serde_yaml::to_string(&hot_config).unwrap()).unwrap();
    let trusted_app = crate::state::App::new(hot_config, cfg_path);
    let (trusted_origin, _trusted_server) = serve(trusted_app).await;
    let (_, trusted_host_port) = trusted_origin.split_once("://").unwrap();
    let trusted_browser_origin = format!("https://{trusted_host_port}");
    let trusted_route = credential_route(&trusted_origin, "ops");
    let accepted = client
        .put(&trusted_route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, &trusted_browser_origin)
        .header("sec-fetch-site", "same-origin")
        .header("x-forwarded-proto", "https")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);

    let multi_proto = client
        .put(&trusted_route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, &trusted_browser_origin)
        .header("sec-fetch-site", "same-origin")
        .header("x-forwarded-proto", "https,http")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(multi_proto.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
/// Verify that configured public origin is exact hot reloadable and revocable without losing secret.
async fn configured_public_origin_is_exact_hot_reloadable_and_revocable_without_losing_secret() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let mut initial = (*app.cfg()).clone();
    // This deployment setting is an admin browser Origin allowlist. The API below
    // remains a loopback HTTP test server, so the header is not TLS attestation.
    initial.notifications.credential_public_url = "HTTPS://ADMIN.EXAMPLE.INVALID:443".into();
    app.set_config(initial);
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let trusted_origin = "https://admin.example.invalid";
    let first_secret = "public-origin-credential-sentinel";
    let request_for_origin = |origin: &str, method: reqwest::Method| {
        let request = client
            .request(method, &route)
            .bearer_auth(KEY)
            .header("sec-fetch-site", "same-origin")
            .json(&json!({"url":format!("https://hooks.example.invalid/{first_secret}")}));
        if origin.is_empty() { request } else { request.header(reqwest::header::ORIGIN, origin) }
    };

    let status = client.get(format!("{origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = status.json().await.unwrap();
    assert_eq!(status["credential_public_url"], trusted_origin);
    assert!(status.get("credential_proxy_peer").is_none());
    assert!(status.get("credential_proxy_cidr").is_none());

    for wrong_origin in [
        "",
        "http://admin.example.invalid",
        "https://admin.example.invalid:8443",
        "https://admin.example.invalid.attacker.invalid",
    ] {
        let response = request_for_origin(wrong_origin, reqwest::Method::PUT).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "origin: {wrong_origin}");
        assert!(!response.text().await.unwrap().contains(first_secret));
    }
    assert!(!temp.0.join("auth/.notification-credentials").exists());

    let accepted = client
        .put(&route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, trusted_origin)
        .header("sec-fetch-site", "same-origin")
        .header(reqwest::header::HOST, "attacker.invalid:9000")
        .header("x-forwarded-proto", "http")
        .header("x-forwarded-host", "spoofed-forwarded-host.invalid")
        .header("x-forwarded-for", "203.0.113.9")
        .json(&json!({"url":format!("https://hooks.example.invalid/{first_secret}")}))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let credential_path = temp.0.join("auth/.notification-credentials/ops.json");
    let saved = std::fs::read(&credential_path).unwrap();

    let mut changed = (*app.cfg()).clone();
    changed.notifications.credential_public_url = "https://portal.example.invalid".into();
    app.set_config(changed);
    let changed_status = client.get(format!("{origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(changed_status.status(), StatusCode::OK);
    assert_eq!(
        changed_status.json::<Value>().await.unwrap()["credential_public_url"],
        "https://portal.example.invalid"
    );

    for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
        let old_origin = request_for_origin(trusted_origin, method).send().await.unwrap();
        assert_eq!(old_origin.status(), StatusCode::FORBIDDEN);
        assert_eq!(std::fs::read(&credential_path).unwrap(), saved);
    }

    let new_secret = "replacement-public-origin-sentinel";
    let updated = client
        .put(&route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, "https://portal.example.invalid")
        .header("sec-fetch-site", "same-origin")
        .json(&json!({"url":format!("https://hooks.example.invalid/{new_secret}")}))
        .send()
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_secret = std::fs::read(&credential_path).unwrap();

    let mut revoked = (*app.cfg()).clone();
    revoked.notifications.credential_ui_enabled = false;
    app.set_config(revoked);
    let revoked_put = client
        .put(&route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, "https://portal.example.invalid")
        .header("sec-fetch-site", "same-origin")
        .json(&json!({"url":"https://hooks.example.invalid/revoked"}))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked_put.status(), StatusCode::FORBIDDEN);
    let revoked_delete = client
        .delete(&route)
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, "https://portal.example.invalid")
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(revoked_delete.status(), StatusCode::FORBIDDEN);
    assert_eq!(std::fs::read(&credential_path).unwrap(), updated_secret);
}

#[tokio::test]
/// Verify that invalid managed credential inputs fail without echo or partial write.
async fn invalid_managed_credential_inputs_fail_without_echo_or_partial_write() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let sentinel = "invalid-input-secret-sentinel";
    let inputs = [
        json!({"url":format!("https://user:{sentinel}@hooks.example.invalid/path")}),
        json!({"url":"http://hooks.example.invalid/path","bearer_token":sentinel}),
        json!({"url":"https://hooks.example.invalid/path","bearer_token":format!("{sentinel}\r\nX-Evil: yes")}),
        json!({"url":"https://hooks.example.invalid/path","unexpected":sentinel}),
    ];
    for input in inputs {
        let response = authenticated(&client, reqwest::Method::PUT, &route, &origin).json(&input).send().await.unwrap();
        assert!(!response.status().is_success());
        assert!(!response.text().await.unwrap().contains(sentinel));
    }

    let oversize = json!({"url":"https://hooks.example.invalid/path","bearer_token":"x".repeat(20 * 1024)});
    let response = authenticated(&client, reqwest::Method::PUT, &route, &origin).json(&oversize).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!response.text().await.unwrap().contains(sentinel));

    for input in [
        json!({"url":format!("https://hooks.example.invalid/{}", "p".repeat(8200))}),
        json!({"url":"https://hooks.example.invalid/path","bearer_token":"x".repeat(8193)}),
    ] {
        let response = authenticated(&client, reqwest::Method::PUT, &route, &origin).json(&input).send().await.unwrap();
        assert!(!response.status().is_success());
        assert!(!response.text().await.unwrap().contains(sentinel));
    }

    let malformed = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(format!("{{\"url\":\"{sentinel}"))
        .send()
        .await
        .unwrap();
    assert!(!malformed.status().is_success());
    assert!(!malformed.text().await.unwrap().contains(sentinel));
    let traversal = client
        .put(format!("{origin}/api/notifications/bad%2Fid/credentials"))
        .bearer_auth(KEY)
        .header(reqwest::header::ORIGIN, &origin)
        .json(&json!({"url":format!("https://hooks.example.invalid/{sentinel}")}))
        .send()
        .await
        .unwrap();
    assert!(!traversal.status().is_success());
    assert!(!traversal.text().await.unwrap().contains(sentinel));
    assert!(!temp.0.join("auth/.notification-credentials/ops.json").exists());
}

#[cfg(unix)]
#[tokio::test]
/// Verify that managed credential replacement rejects symlink and hardlink targets.
async fn managed_credential_replacement_rejects_symlink_and_hardlink_targets() {
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app).await;
    let client = reqwest::Client::new();
    let managed_dir = temp.0.join("auth/.notification-credentials");
    std::fs::create_dir(&managed_dir).unwrap();
    std::fs::set_permissions(&managed_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = temp.0.join("operator-owned-file");
    let original = b"operator-owned-content-sentinel";
    std::fs::write(&target, original).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let managed = managed_dir.join("ops.json");
    symlink(&target, &managed).unwrap();

    let route = credential_route(&origin, "ops");
    let symlink_write = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url":"https://hooks.example.invalid/replace-symlink"}))
        .send()
        .await
        .unwrap();
    assert!(!symlink_write.status().is_success());
    assert_eq!(std::fs::read(&target).unwrap(), original);
    assert!(std::fs::symlink_metadata(&managed).unwrap().file_type().is_symlink());

    std::fs::remove_file(&managed).unwrap();
    std::fs::hard_link(&target, &managed).unwrap();
    let hardlink_write = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url":"https://hooks.example.invalid/replace-hardlink"}))
        .send()
        .await
        .unwrap();
    assert!(!hardlink_write.status().is_success());
    assert_eq!(std::fs::read(&target).unwrap(), original);
    assert!(std::fs::metadata(&target).unwrap().nlink() > 1);
}

#[tokio::test]
/// Verify that invalid external credentials remain authoritative and cannot be replaced by managed values.
async fn invalid_external_credentials_remain_authoritative_and_cannot_be_replaced_by_managed_values() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let route = credential_route(&origin, "ops");
    let managed_url = "https://hooks.example.invalid/managed-sentinel";
    let response = authenticated(&client, reqwest::Method::PUT, &route, &origin)
        .json(&json!({"url": managed_url}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let managed_path = temp.0.join("auth/.notification-credentials/ops.json");
    let managed_before = std::fs::read(&managed_path).unwrap();

    let external_dir = temp.0.join("external-secrets");
    std::fs::create_dir_all(&external_dir).unwrap();
    let external_sentinel = "external-invalid-url-sentinel";
    private_file(&external_dir.join("ops.url"), external_sentinel);
    let mut config = (*app.cfg()).clone();
    config.notifications.secrets_dir = Some(external_dir.to_string_lossy().into_owned());
    let reloaded = crate::state::App::new(config, app.cfg_path.clone());
    let (reload_origin, _reload_server) = serve(reloaded).await;

    let status = client.get(format!("{reload_origin}/api/notifications")).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = status.json().await.unwrap();
    assert_eq!(status["destinations"][0]["credential_source"], "external");
    assert_eq!(status["destinations"][0]["credential_configured"], true);
    assert_eq!(status["destinations"][0]["credential_ready"], false);
    assert_eq!(status["destinations"][0]["credential_editable"], false);
    assert!(!status.to_string().contains(external_sentinel));

    let replacement =
        authenticated(&client, reqwest::Method::PUT, &credential_route(&reload_origin, "ops"), &reload_origin)
            .json(&json!({"url":"https://hooks.example.invalid/replacement"}))
            .send()
            .await
            .unwrap();
    assert_eq!(replacement.status(), StatusCode::CONFLICT);
    assert!(!replacement.text().await.unwrap().contains(external_sentinel));
    assert_eq!(std::fs::read(&managed_path).unwrap(), managed_before);

    let test_send = authenticated(
        &client,
        reqwest::Method::POST,
        &format!("{reload_origin}/api/notifications/ops/test"),
        &reload_origin,
    )
    .json(&json!({}))
    .send()
    .await
    .unwrap();
    assert_eq!(test_send.status(), StatusCode::BAD_REQUEST);
    assert_eq!(test_send.json::<Value>().await.unwrap()["error"], "invalid_url");
    assert_eq!(std::fs::read(&managed_path).unwrap(), managed_before);
}

#[tokio::test]
/// Verify that managed credentials use unified resolver and block loopback before outbound send.
async fn managed_credentials_use_unified_resolver_and_block_loopback_before_outbound_send() {
    let temp = Temp::new();
    let app = app(&temp, None);
    let (origin, _server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    let sentinel_url = "https://127.0.0.1:9/managed-secret-sentinel";
    let sentinel_token = "managed-bearer-secret-sentinel";
    let saved = authenticated(&client, reqwest::Method::PUT, &credential_route(&origin, "ops"), &origin)
        .json(&json!({"url":sentinel_url,"bearer_token":sentinel_token}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);

    // The unified resolver must load this managed bundle. The loopback target is
    // rejected by address policy before an HTTP connection or webhook send.
    assert_eq!(app.notifications.test(&app, "ops").await.unwrap_err(), "blocked_address");
    let status = app.notifications.status(&app).to_string();
    let journal = std::fs::read_to_string(temp.0.join("auth/.quota-notifications/state.json")).unwrap();
    for secret in [sentinel_url, sentinel_token] {
        assert!(!status.contains(secret));
        assert!(!journal.contains(secret));
    }
    assert!(journal.contains("blocked_address"));
}
