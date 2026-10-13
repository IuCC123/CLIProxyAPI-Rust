//! Management API used by the dashboard, plus OAuth login orchestration.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{Credential, Provider, set_file_disabled, write_oauth_file};
use crate::config::{Config, ModelAlias};
use crate::oauth;
use crate::state::App;

#[derive(Clone, Serialize)]
pub struct Login {
    pub provider: Provider,
    pub status: &'static str,
    pub message: Option<String>,
    pub url: String,
    pub callback: bool,
    /// "redirect" (browser OAuth) or "device" (enter `user_code` at `url`).
    pub kind: &'static str,
    pub user_code: Option<String>,
    #[serde(skip)]
    pub verifier: String,
    #[serde(skip)]
    pub created: Instant,
}

static CLAUDE_CB: AtomicBool = AtomicBool::new(false);
static CODEX_CB: AtomicBool = AtomicBool::new(false);
static ANTIGRAVITY_CB: AtomicBool = AtomicBool::new(false);

fn remember(app: &Arc<App>, state: &str, login: &Login) {
    let mut logins = app.logins.lock();
    logins.retain(|_, l| l.created.elapsed() < Duration::from_secs(1800));
    logins.insert(state.to_string(), login.clone());
}

fn settle(app: &Arc<App>, state: &str, result: &Result<String>) {
    if let Some(l) = app.logins.lock().get_mut(state) {
        match result {
            Ok(label) => {
                l.status = "done";
                l.message = Some(label.clone());
            }
            Err(e) => {
                l.status = "error";
                l.message = Some(format!("{e:#}"));
            }
        }
    }
    app.broadcast("login", json!({ "state": state }));
}

pub async fn start_login(app: &Arc<App>, provider: Provider) -> Result<(String, Login)> {
    let state = oauth::random_state();
    match provider {
        Provider::Claude | Provider::Codex | Provider::Antigravity => {
            let pkce = oauth::pkce();
            let url = oauth::auth_url(provider, &state, &pkce);
            let callback = ensure_callback_server(app, provider).await;
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url,
                callback,
                kind: "redirect",
                user_code: None,
                verifier: pkce.verifier,
                created: Instant::now(),
            };
            remember(app, &state, &login);
            Ok((state, login))
        }
        Provider::Kimi | Provider::Xai | Provider::Meta => {
            let dev = crate::device::start(app, provider).await?;
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url: dev.verification_uri.clone(),
                callback: true,
                kind: "device",
                user_code: Some(dev.user_code.clone()),
                verifier: String::new(),
                created: Instant::now(),
            };
            remember(app, &state, &login);
            let (app2, state2) = (app.clone(), state.clone());
            tokio::spawn(async move {
                let result = async {
                    let signed = crate::device::wait(&app2, provider, &dev).await?;
                    save_signed(&app2, provider, signed)
                }
                .await;
                settle(&app2, &state2, &result);
            });
            Ok((state, login))
        }
        Provider::Devin => {
            // Devin accepts any localhost redirect, so use a fresh port per login.
            let pkce = oauth::pkce();
            let (callback, redirect) = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    serve_callback(app, listener, "/callback", provider, None);
                    (true, format!("http://127.0.0.1:{port}/callback"))
                }
                Err(_) => (false, String::new()),
            };
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url: crate::devin::auth_url(&redirect, &state, &pkce.challenge),
                callback,
                kind: "redirect",
                user_code: None,
                verifier: pkce.verifier,
                created: Instant::now(),
            };
            remember(app, &state, &login);
            Ok((state, login))
        }
        Provider::Vertex => Err(anyhow!("Vertex uses a service account key: import the JSON instead")),
        Provider::Gemini | Provider::Compat => Err(anyhow!("{} uses API keys", provider.as_str())),
    }
}

fn save_signed(app: &Arc<App>, provider: Provider, s: crate::device::Signed) -> Result<String> {
    let path = app.cfg().auth_dir().join(&s.file);
    write_oauth_file(&path, provider, &s.oauth, &s.extra)?;
    app.reload_accounts();
    Ok(s.oauth.email.unwrap_or(s.file))
}

/// Listens on the fixed OAuth redirect port while logins are pending.
async fn ensure_callback_server(app: &Arc<App>, provider: Provider) -> bool {
    let (flag, port, path) = match provider {
        Provider::Claude => (&CLAUDE_CB, oauth::claude::PORT, "/callback"),
        Provider::Antigravity => (&ANTIGRAVITY_CB, crate::antigravity::PORT, "/oauth-callback"),
        _ => (&CODEX_CB, oauth::codex::PORT, "/auth/callback"),
    };
    if flag.load(Ordering::SeqCst) {
        return true;
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "cannot listen on localhost:{port} for the OAuth callback ({e}); paste the redirect URL instead"
            );
            return false;
        }
    };
    flag.store(true, Ordering::SeqCst);
    serve_callback(app, listener, path, provider, Some(flag));
    true
}

/// Serves the OAuth redirect on `listener` until no login for `provider` is pending.
fn serve_callback(
    app: &Arc<App>,
    listener: tokio::net::TcpListener,
    path: &'static str,
    provider: Provider,
    flag: Option<&'static AtomicBool>,
) {
    let router = Router::new().route(path, get(callback)).with_state(app.clone());
    let app2 = app.clone();
    tokio::spawn(async move {
        let shutdown = async move {
            // Stay up while a login for this provider is pending (max 15 minutes).
            let started = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let pending = app2.logins.lock().values().any(|l| l.provider == provider && l.status == "pending");
                if !pending || started.elapsed() > Duration::from_secs(900) {
                    break;
                }
            }
        };
        let _ = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;
        if let Some(f) = flag {
            f.store(false, Ordering::SeqCst);
        }
    });
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(app): State<Arc<App>>, Query(q): Query<CallbackQuery>) -> Html<String> {
    let result = match (q.code, q.state, q.error) {
        (_, _, Some(e)) => Err(q.error_description.unwrap_or(e)),
        (Some(code), Some(state), _) => complete_login(&app, &state, &code).await.map_err(|e| format!("{e:#}")),
        _ => Err("missing code or state".to_string()),
    };
    let (title, body) = match result {
        Ok(label) => ("Signed in", format!("Connected <b>{}</b>. You can close this tab.", html_escape(&label))),
        Err(e) => ("Sign-in failed", html_escape(&e)),
    };
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="color-scheme" content="dark"><title>{title}</title>
<body style="margin:0;height:100vh;display:grid;place-items:center;background:#000;color:#f5f5f5;font:15px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif">
<div style="text-align:center;max-width:420px;padding:24px"><div style="font-size:13px;letter-spacing:.08em;text-transform:uppercase;color:#737373">CLIProxyAPI-Rust</div>
<h1 style="font-size:22px;font-weight:600;margin:10px 0">{title}</h1><p style="color:#a3a3a3;margin:0">{body}</p></div></body>"#
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub async fn complete_login(app: &Arc<App>, state: &str, code: &str) -> Result<String> {
    let (provider, verifier) = {
        let logins = app.logins.lock();
        let l = logins.get(state).ok_or_else(|| anyhow!("unknown or expired login, start again"))?;
        if l.status != "pending" {
            return Err(anyhow!("this login was already completed"));
        }
        if l.kind == "device" {
            return Err(anyhow!("approve the code in your browser; there is nothing to paste"));
        }
        (l.provider, l.verifier.clone())
    };
    let result = async {
        if provider == Provider::Devin {
            let signed = crate::devin::complete_login(app, code.trim(), &verifier).await?;
            return save_signed(app, provider, signed);
        }
        let (cred, name, extra) = oauth::exchange(app, provider, code.trim(), state, &verifier).await?;
        let path = app.cfg().auth_dir().join(&name);
        write_oauth_file(&path, provider, &cred, &extra)?;
        app.reload_accounts();
        Ok::<_, anyhow::Error>(cred.email.unwrap_or(name))
    }
    .await;
    settle(app, state, &result);
    result
}

/// Accepts a pasted redirect URL, a `code#state` string or a bare code.
pub fn parse_pasted(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Some(q) = input.split_once('?').map(|(_, q)| q).filter(|q| q.contains("code=")) {
        let mut code = String::new();
        let mut state = None;
        for (k, v) in url::form_urlencoded::parse(q.split('#').next().unwrap_or(q).as_bytes()) {
            match k.as_ref() {
                "code" => code = v.into_owned(),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        return (code, state);
    }
    (input.to_string(), None)
}

// ---------------------------------------------------------------------- router

/// Apply the existing management access policy to dashboard and notification routes.
pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .route("/overview", get(overview))
        .route("/accounts", get(accounts))
        .route("/accounts/{id}", delete(delete_account))
        .route("/accounts/{id}/toggle", post(toggle_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/accounts/{id}/banked-resets", get(banked_resets).post(apply_banked_reset))
        .route("/accounts/{id}/quota/refresh", post(refresh_quota))
        .route("/keys", post(add_key))
        .route("/vertex", post(import_vertex))
        .route("/requests", get(requests))
        .route("/models", get(models))
        .route("/notifications", get(notification_status))
        .route("/notifications/{id}/test", post(test_notification))
        .route(
            "/notifications/{id}/credentials",
            put(put_notification_credentials).delete(delete_notification_credentials),
        )
        .route("/config", get(get_config).put(put_config))
        .route("/config/settings", get(get_settings).patch(patch_settings))
        .route("/login/{target}", post(login_start).get(login_status))
        .route("/login/{target}/code", post(login_code))
        .route("/live", get(live))
        .layer(middleware::from_fn_with_state(app, auth))
        .layer(middleware::from_fn(notification_no_store))
}
/// Prevent intermediaries and browsers from caching notification management responses.
async fn notification_no_store(req: Request, next: Next) -> Response {
    let sensitive = req.uri().path().ends_with("/credentials") || req.uri().path() == "/notifications";
    let response = next.run(req).await;
    if sensitive { no_store(response) } else { response }
}

async fn auth(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let cfg = app.cfg();
    let key = cfg.management_key.clone();
    if key.is_empty() {
        if addr.ip().is_loopback() {
            return next.run(req).await;
        }
        return err(
            StatusCode::FORBIDDEN,
            "the dashboard is only reachable from localhost until you set management-key",
        );
    }
    if cfg.management_allow_remote == Some(false) && !addr.ip().is_loopback() {
        return err(StatusCode::FORBIDDEN, "remote management is off (allow-remote: false)");
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    let query = req
        .uri()
        .query()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned()));
    if bearer.or(query).is_some_and(|k| management_key_matches(&k, &key)) {
        return next.run(req).await;
    }
    err(StatusCode::UNAUTHORIZED, "management key required")
}

/// Plain keys compare in constant time; bcrypt hashes (CLIProxyAPI hashes
/// `secret-key` on first start) are verified once per key and remembered.
fn management_key_matches(provided: &str, configured: &str) -> bool {
    use sha2::{Digest, Sha256};
    static VERIFIED: parking_lot::Mutex<Vec<[u8; 32]>> = parking_lot::Mutex::new(Vec::new());
    if !["$2a$", "$2b$", "$2y$"].iter().any(|p| configured.starts_with(p)) {
        return constant_eq(provided, configured);
    }
    let id: [u8; 32] = Sha256::digest(format!("{configured}\0{provided}").as_bytes()).into();
    if VERIFIED.lock().contains(&id) {
        return true;
    }
    let ok = bcrypt::verify(provided, configured).unwrap_or(false);
    if ok {
        VERIFIED.lock().push(id);
    }
    ok
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn overview(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg();
    let accounts = app.pool.all();
    let (mut active, mut cooling, mut disabled) = (0, 0, 0);
    let mut providers = std::collections::BTreeMap::<&str, usize>::new();
    for a in &accounts {
        *providers.entry(a.provider.as_str()).or_default() += 1;
        let st = a.state.lock();
        if st.disabled {
            disabled += 1;
        } else if st.quota_refreshing
            || st.cooldowns.get("*").is_some_and(|t| *t > chrono::Utc::now())
            || st.quota_cooldowns.get("*").is_some_and(|t| *t > chrono::Utc::now())
            || st.quota.exhausted_until("").is_some()
        {
            cooling += 1;
        } else {
            active += 1;
        }
    }
    let host = if cfg.host == "0.0.0.0" || cfg.host.is_empty() { "127.0.0.1".to_string() } else { cfg.host.clone() };
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": app.started.to_rfc3339(),
        "uptime_secs": (chrono::Utc::now() - app.started).num_seconds(),
        "base_url": format!("http://{host}:{}", cfg.port),
        "client_keys": cfg.api_keys,
        "routing": cfg.routing,
        "banked_resets": cfg.banked_resets,
        "session_affinity": cfg.session_affinity,
        "management_key": !cfg.management_key.is_empty(),
        "totals": *app.stats.totals.lock(),
        "active": app.stats.active.load(Ordering::Relaxed),
        "series": app.stats.series(),
        "accounts": { "total": accounts.len(), "active": active, "cooling": cooling, "disabled": disabled, "providers": providers },
        "models": app.pool.models().len(),
        "config_path": app.cfg_path.display().to_string(),
        "auth_dir": cfg.auth_dir().display().to_string(),
    }))
}

async fn accounts(State(app): State<Arc<App>>) -> Json<Value> {
    Json(Value::Array(app.pool.all().iter().map(|a| a.snapshot()).collect()))
}

async fn requests(State(app): State<Arc<App>>) -> Json<Value> {
    let recent = app.stats.recent.lock();
    Json(serde_json::to_value(recent.iter().rev().collect::<Vec<_>>()).unwrap_or_default())
}

async fn models(State(app): State<Arc<App>>) -> Json<Value> {
    Json(Value::Array(app.pool.models().into_iter().map(|(m, p)| json!({ "id": m, "provider": p })).collect()))
}

#[derive(Deserialize)]
struct ToggleBody {
    disabled: bool,
}

/// Persist account activation and invalidate quota responses and notification evidence captured before a toggle.
async fn toggle_account(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<ToggleBody>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path
        && let Err(e) = set_file_disabled(path, b.disabled)
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    {
        let mut st = acct.state.lock();
        if st.disabled != b.disabled {
            st.quota_epoch += 1;
            st.notifications_changed_at = Some(chrono::Utc::now());
            st.notification_evidence = Default::default();
        }
        st.disabled = b.disabled;
    }
    app.broadcast("accounts", Value::Null);
    ok()
}

async fn refresh_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if !acct.is_oauth() {
        return err(StatusCode::BAD_REQUEST, "API keys don't need refreshing");
    }
    match oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), true).await {
        Ok(()) => {
            let _ = crate::quota::poll(&app, &acct).await;
            acct.state.lock().cooldowns.remove("*");
            app.broadcast("accounts", Value::Null);
            ok()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn reset_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    let mut st = acct.state.lock();
    st.cooldowns.clear();
    st.quota_cooldowns.clear();
    st.strikes = 0;
    st.last_error = None;
    drop(st);
    app.broadcast("accounts", Value::Null);
    ok()
}

const RESETS_OFF: &str = "Banked resets are turned off. Turn them on under Config, Connections.";

async fn banked_resets(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if !app.cfg().banked_resets {
        return err(StatusCode::NOT_FOUND, RESETS_OFF);
    }
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    match crate::banked_resets::refresh(&app, &acct).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}
async fn apply_banked_reset(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<crate::banked_resets::Action>,
) -> Response {
    if !app.cfg().banked_resets {
        return err(StatusCode::NOT_FOUND, RESETS_OFF);
    }
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    match tokio::spawn(crate::banked_resets::apply(app, acct, body)).await {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(e)) => err(StatusCode::CONFLICT, e.to_string()),
        Err(_) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, "Reset operation interrupted; refresh its status before continuing")
        }
    }
}
async fn refresh_quota(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if !matches!(acct.provider, Provider::Codex | Provider::Claude) || !acct.is_oauth() {
        return err(
            StatusCode::BAD_REQUEST,
            "Subscription quota is only available for Codex and Claude OAuth accounts",
        );
    }
    if oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
        return err(StatusCode::BAD_GATEWAY, "Could not refresh subscription credentials");
    }
    // Quota can still refresh if this subscription does not offer banked resets.
    acct.state.lock().quota_epoch += 1;
    if crate::quota::poll(&app, &acct).await.is_err() {
        return err(StatusCode::BAD_GATEWAY, "Could not refresh provider usage");
    }
    app.broadcast("accounts", Value::Null);
    if !app.cfg().banked_resets {
        return ok();
    }
    match crate::banked_resets::refresh(&app, &acct).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

async fn delete_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path {
        if let Err(e) = std::fs::remove_file(path) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        app.reload_accounts();
        return ok();
    }
    let key = match &*acct.cred.read() {
        Credential::ApiKey { key, .. } => key.clone(),
        _ => String::new(),
    };
    let group = if acct.provider == Provider::Compat { acct.group.clone() } else { None };
    edit_config(&app, |doc| crate::compat::remove_key(doc, &key, group.as_deref()))
}

#[derive(Deserialize)]
struct KeyBody {
    provider: String,
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: String,
}

async fn add_key(State(app): State<Arc<App>>, Json(b): Json<KeyBody>) -> Response {
    let key = b.api_key.trim().to_string();
    let base = Some(b.base_url.trim().to_string()).filter(|s| !s.is_empty());
    let models: Vec<ModelAlias> = b
        .models
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| match m.split_once('=') {
            Some((alias, name)) => ModelAlias { name: name.trim().into(), alias: Some(alias.trim().into()) },
            None => ModelAlias { name: m.into(), alias: None },
        })
        .collect();
    let models: Vec<(String, Option<String>)> = models.into_iter().map(|m| (m.name, m.alias)).collect();
    let (group, name) = match Provider::parse(&b.provider) {
        Some(Provider::Compat) => {
            let Some(base) = &base else { return err(StatusCode::BAD_REQUEST, "base URL is required") };
            if models.is_empty() {
                return err(StatusCode::BAD_REQUEST, "list at least one model");
            }
            let name = if b.name.trim().is_empty() {
                url::Url::parse(base)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from))
                    .unwrap_or_else(|| "provider".into())
            } else {
                b.name.trim().to_string()
            };
            ("openai-compatibility", Some(name))
        }
        Some(
            p @ (Provider::Claude
            | Provider::Codex
            | Provider::Gemini
            | Provider::Vertex
            | Provider::Kimi
            | Provider::Xai
            | Provider::Meta),
        ) => {
            if key.is_empty() {
                return err(StatusCode::BAD_REQUEST, "API key is required");
            }
            (p.as_str(), None)
        }
        Some(p) => return err(StatusCode::BAD_REQUEST, format!("{} does not take API keys", p.as_str())),
        None => return err(StatusCode::BAD_REQUEST, "unknown provider"),
    };
    let new = crate::compat::NewKey { group, api_key: &key, base_url: base.as_deref(), models, name: name.as_deref() };
    edit_config(&app, |doc| crate::compat::add_key(doc, &new))
}

#[derive(Deserialize)]
struct VertexBody {
    json: String,
    #[serde(default)]
    location: String,
}

async fn import_vertex(State(app): State<Arc<App>>, Json(b): Json<VertexBody>) -> Response {
    match crate::vertex::import(&app, &b.json, &b.location).await {
        Ok(label) => Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Applies an edit to the config file's YAML tree, keeping every setting this
/// binary doesn't know about (so the file still works with CLIProxyAPI).
/// A rewrite drops comments, so the commented original is kept once as config.yaml.bak.
fn keep_original(app: &App, text: &str) {
    let backup = app.cfg_path.with_extension("yaml.bak");
    if text.contains('#') && !backup.exists() {
        let _ = std::fs::write(&backup, text);
    }
}

/// Validate and persist a config edit under the shared config lock before reloading accounts.
fn edit_config(app: &Arc<App>, edit: impl FnOnce(&mut serde_yaml::Value)) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    let mut doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(serde_yaml::Value::Null) | Err(_) if text.trim().is_empty() => {
            serde_yaml::Value::Mapping(Default::default())
        }
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("config.yaml doesn't parse: {e}")),
    };
    let original = doc.clone();
    edit(&mut doc);
    let out = match crate::config_editor::render(&text, &original, &doc) {
        Ok((t, rewritten)) => {
            if rewritten {
                keep_original(app, &text);
            }
            t
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let cfg = match Config::parse(&out) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if let Err(message) = app.notifications.validate_destination_change(app, &cfg) {
        return err(StatusCode::CONFLICT, message);
    }
    if let Err(e) = std::fs::write(&app.cfg_path, out) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    app.set_config(cfg);
    ok()
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Value> {
    let text = std::fs::read_to_string(&app.cfg_path).unwrap_or_default();
    Json(json!({ "text": text, "path": app.cfg_path.display().to_string() }))
}

/// Report sanitized delivery status and whether this request may enter managed credentials.
async fn notification_status(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let mut status = app.notifications.status(&app);
    let guard = credential_guard(&app, peer.ip(), req.headers(), req.uri(), false);
    status["credential_ui_ready"] = json!(guard.is_ok());
    status["credential_ui_reason"] = json!(guard.err().unwrap_or("ready"));
    no_store(Json(status).into_response())
}
/// Mark a management response as non-cacheable, including rejected credential requests.
fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    response
}
/// Return a fixed credential failure without reflecting submitted values.
fn credential_error(status: StatusCode, message: &'static str) -> Response {
    no_store(err(status, message))
}
/// Reject duplicate or non-text headers before interpreting security-sensitive request metadata.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, &'static str> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|value| value.to_str().map(str::trim).map_err(|_| "invalid_credential_request_header"))
        .transpose()?;
    if values.next().is_some() {
        return Err("invalid_credential_request_header");
    }
    Ok(value)
}
/// Parse a bounded authority without accepting userinfo, whitespace, or URL path components.
fn credential_host(value: &str, scheme: &str) -> Result<url::Url, &'static str> {
    if value.is_empty()
        || value.len() > 300
        || value.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#' | '@' | ','))
    {
        return Err("invalid_credential_request_host");
    }
    let url = url::Url::parse(&format!("{scheme}://{value}/")).map_err(|_| "invalid_credential_request_host")?;
    if url.host().is_none() {
        return Err("invalid_credential_request_host");
    }
    Ok(url)
}
/// Require opt-in secret entry, management bearer authentication, and the configured origin policy.
fn credential_guard(
    app: &App,
    peer: std::net::IpAddr,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    mutation: bool,
) -> Result<(), &'static str> {
    let cfg = app.cfg();
    if !cfg.notifications.credential_ui_enabled {
        return Err("credential_ui_disabled");
    }
    if !cfg.management_key.is_empty() {
        let bearer = single_header(headers, "authorization")?
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or("credential_authorization_header_required")?;
        if !management_key_matches(bearer, &cfg.management_key) {
            return Err("credential_authorization_header_required");
        }
    }
    if single_header(headers, "sec-fetch-site")?.is_some_and(|site| !matches!(site, "same-origin" | "none")) {
        return Err("credential_cross_site_request_rejected");
    }
    if let Some(public_origin) =
        crate::notifications::credential_public_origin(&cfg.notifications.credential_public_url)?
    {
        // Public-origin mode is an administrator's deployment trust decision. Host
        // and forwarded headers cannot independently attest browser TLS behind a proxy.
        return credential_origin_check(headers, &public_origin, mutation);
    }
    let header_host = single_header(headers, "host")?;
    let authority = uri.authority().map(|authority| authority.as_str());
    let host = header_host.or(authority).ok_or("invalid_credential_request_host")?;
    let trusted = app.notifications.trusted_credential_proxy(peer);
    let tls = app.startup_config.tls.enable;
    let forwarded = single_header(headers, "x-forwarded-proto")?;
    let effective_host = if trusted { single_header(headers, "x-forwarded-host")?.unwrap_or(host) } else { host };
    let normalized_peer = match peer {
        std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(peer),
        _ => peer,
    };
    let scheme = if tls || trusted && forwarded == Some("https") {
        "https"
    } else {
        let host = credential_host(host, "http")?;
        let local_host = match host.host() {
            Some(url::Host::Domain(domain)) => domain == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        let forwarded_headers = ["forwarded", "x-forwarded-proto", "x-forwarded-host", "x-forwarded-for", "x-real-ip"]
            .iter()
            .any(|name| headers.contains_key(*name));
        if !normalized_peer.is_loopback() || !local_host || forwarded_headers {
            return Err("credential_https_required");
        }
        "http"
    };
    let expected = credential_host(effective_host, scheme)?;
    if let (Some(host), Some(authority)) = (header_host, authority)
        && credential_host(host, scheme)?.origin() != credential_host(authority, scheme)?.origin()
    {
        return Err("invalid_credential_request_host");
    }
    credential_origin_check(headers, &expected.origin().ascii_serialization(), mutation)
}
/// Require an exact mutation Origin and reject malformed, duplicate, or cross-origin metadata.
fn credential_origin_check(headers: &HeaderMap, expected: &str, required: bool) -> Result<(), &'static str> {
    let Some(origin) = single_header(headers, "origin")? else {
        return if required { Err("credential_origin_required") } else { Ok(()) };
    };
    if origin.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '@' | '?' | '#')) {
        return Err("credential_origin_mismatch");
    }
    let (_, authority_and_path) = origin.split_once("://").ok_or("credential_origin_mismatch")?;
    if authority_and_path.split_once('/').is_some_and(|(_, path)| !path.is_empty()) {
        return Err("credential_origin_mismatch");
    }
    let origin = url::Url::parse(origin).map_err(|_| "credential_origin_mismatch")?;
    if origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.origin().ascii_serialization() != expected
    {
        return Err("credential_origin_mismatch");
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationCredentialBody {
    url: String,
    #[serde(default)]
    bearer_token: Option<String>,
}
/// Accept bounded same-origin JSON and replace a write-only credential bundle.
async fn put_notification_credentials(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    if let Err(message) = credential_guard(&app, peer.ip(), req.headers(), req.uri(), true) {
        return credential_error(StatusCode::FORBIDDEN, message);
    }
    if !single_header(req.headers(), "content-type").ok().flatten().is_some_and(|value| {
        value.split(';').next().is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return credential_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "credential_json_required");
    }
    let bytes = match axum::body::to_bytes(req.into_body(), 20 << 10).await {
        Ok(bytes) => bytes,
        Err(_) => return credential_error(StatusCode::PAYLOAD_TOO_LARGE, "credential_body_too_large"),
    };
    let body: NotificationCredentialBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => return credential_error(StatusCode::BAD_REQUEST, "invalid_credential_body"),
    };
    match app.notifications.save_credentials(&app, &id, body.url, body.bearer_token) {
        Ok(()) => no_store(Json(json!({"saved":true})).into_response()),
        Err(message) => credential_error(
            if message == "credential_ui_disabled" {
                StatusCode::FORBIDDEN
            } else if message == "credentials_externally_managed" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            message,
        ),
    }
}
/// Authenticate an empty-body deletion without returning the removed credential.
async fn delete_notification_credentials(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    if let Err(message) = credential_guard(&app, peer.ip(), req.headers(), req.uri(), true) {
        return credential_error(StatusCode::FORBIDDEN, message);
    }
    if axum::body::to_bytes(req.into_body(), 0).await.is_err() {
        return credential_error(StatusCode::BAD_REQUEST, "credential_delete_body_must_be_empty");
    }
    match app.notifications.remove_credentials(&app, &id) {
        Ok(()) => no_store(Json(json!({"removed":true})).into_response()),
        Err(message) => credential_error(
            if message == "credential_ui_disabled" {
                StatusCode::FORBIDDEN
            } else if message == "credentials_externally_managed" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            message,
        ),
    }
}

// JSON prevents ordinary cross-origin forms from triggering a localhost send.
/// Require JSON admission before attempting a rate-limited destination test.
async fn test_notification(State(app): State<Arc<App>>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    if !body.as_object().is_some_and(|value| value.is_empty()) {
        return err(StatusCode::BAD_REQUEST, "Send an empty JSON object to test a configured destination");
    }
    match app.notifications.test(&app, &id).await {
        Ok(value) => Json(value).into_response(),
        Err(message) => err(StatusCode::BAD_REQUEST, message),
    }
}

#[derive(Deserialize)]
struct ConfigBody {
    text: String,
}

/// Validate raw YAML and credential lifecycle constraints before atomically applying it.
async fn put_config(State(app): State<Arc<App>>, Json(b): Json<ConfigBody>) -> Response {
    let _guard = app.config_write.lock();
    let cfg = match Config::parse(&b.text) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if let Err(message) = app.notifications.validate_destination_change(&app, &cfg) {
        return err(StatusCode::CONFLICT, message);
    }
    if let Err(e) = std::fs::write(&app.cfg_path, &b.text) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let restart = !crate::config_editor::restart_fields(&app.startup_config, &cfg).is_empty();
    app.set_config(cfg);
    Json(json!({ "ok": true, "restart_required": restart })).into_response()
}

fn settings_response(app: &Arc<App>, text: &str) -> anyhow::Result<Value> {
    let cfg = Config::parse(text)?;
    let restart = crate::config_editor::restart_fields(&app.startup_config, &cfg);
    Ok(json!({
        "values": crate::config_editor::values(text)?,
        "defaults": crate::config_editor::values("")?,
        "revision": crate::config_editor::revision(text),
        "path": app.cfg_path.display().to_string(),
        "ignored": cfg.ignored,
        "restart_fields": restart,
        "restart_required": !restart.is_empty(),
    }))
}

async fn get_settings(State(app): State<Arc<App>>) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    match settings_response(&app, &text) {
        Ok(body) => Json(body).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct SettingsBody {
    revision: String,
    changes: serde_json::Map<String, Value>,
}

/// Preserve YAML layout while applying validated structured edits and credential lifecycle guards.
async fn patch_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    if body.revision != crate::config_editor::revision(&text) {
        return err(
            StatusCode::CONFLICT,
            "The config changed since you opened it. Reload the latest settings before saving.",
        );
    }
    let (out, cfg, rewritten) = match crate::config_editor::apply(&text, &body.changes) {
        Ok(result) => result,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if let Err(message) = app.notifications.validate_destination_change(&app, &cfg) {
        return err(StatusCode::CONFLICT, message);
    }
    let mut response = match settings_response(&app, &out) {
        Ok(body) => body,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    response["rewritten"] = rewritten.into();
    // Also catch external file changes made while validation was running.
    if std::fs::read_to_string(&app.cfg_path).ok().as_deref() != Some(&text) {
        return err(StatusCode::CONFLICT, "The config changed while saving. Reload the latest settings before saving.");
    }
    if out != text {
        if rewritten {
            keep_original(&app, &text);
        }
        if let Err(e) = std::fs::write(&app.cfg_path, &out) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not save config: {e}"));
        }
        app.set_config(cfg);
    }
    Json(response).into_response()
}

async fn login_start(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    let Some(provider) = Provider::parse(&target) else { return err(StatusCode::BAD_REQUEST, "unknown provider") };
    match start_login(&app, provider).await {
        Ok((state, l)) => Json(json!({
            "state": state, "url": l.url, "callback": l.callback, "kind": l.kind, "user_code": l.user_code,
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn login_status(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    match app.logins.lock().get(&target) {
        Some(l) => Json(serde_json::to_value(l).unwrap_or_default()).into_response(),
        None => err(StatusCode::NOT_FOUND, "unknown login"),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    input: String,
}

async fn login_code(State(app): State<Arc<App>>, Path(target): Path<String>, Json(b): Json<CodeBody>) -> Response {
    let (code, state) = parse_pasted(&b.input);
    if code.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no authorization code found");
    }
    if state.as_deref().is_some_and(|s| s != target) {
        return err(StatusCode::BAD_REQUEST, "that URL belongs to a different login attempt");
    }
    match complete_login(&app, &target, &code).await {
        Ok(label) => Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn live(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = app.live.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => if socket.send(Message::Text(m.into())).await.is_err() { break },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = tick.tick() => {
                    let msg = json!({ "type": "tick", "data": {
                        "active": app.stats.active.load(Ordering::Relaxed),
                        "totals": *app.stats.totals.lock(),
                    }}).to_string();
                    if socket.send(Message::Text(msg.into())).await.is_err() { break }
                }
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Verify that public credential origin ignores proxy headers and requires mutation origin.
    fn public_credential_origin_ignores_proxy_headers_and_requires_mutation_origin() {
        let cfg = Config {
            auth_dir: "/nonexistent/public-credential-origin-test".into(),
            management_key: "synthetic-origin-key".into(),
            notifications: crate::notifications::Config {
                credential_ui_enabled: true,
                credential_public_url: "https://dashboard.example.test:443/".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let app = App::new(cfg, std::path::PathBuf::from("/nonexistent/public-credential-config.yaml"));
        let uri = "/api/notifications/ops/credentials".parse().unwrap();
        let peer = "192.0.2.1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer synthetic-origin-key".parse().unwrap());
        headers.insert("host", "internal-proxy.invalid:8319".parse().unwrap());
        headers.insert("x-forwarded-proto", "http, https".parse().unwrap());
        headers.append("x-forwarded-proto", "invalid".parse().unwrap());
        headers.insert("x-forwarded-host", "untrusted.invalid".parse().unwrap());
        assert!(credential_guard(&app, peer, &headers, &uri, false).is_ok());
        assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("credential_origin_required"));
        headers.insert("origin", "https://dashboard.example.test".parse().unwrap());
        assert!(credential_guard(&app, peer, &headers, &uri, true).is_ok());
        for origin in [
            "http://dashboard.example.test",
            "https://other.example.test",
            "https://dashboard.example.test/a/..",
            "https://dashboard.example.test?",
        ] {
            headers.insert("origin", origin.parse().unwrap());
            assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("credential_origin_mismatch"));
            assert_eq!(credential_guard(&app, peer, &headers, &uri, false), Err("credential_origin_mismatch"));
        }
        headers.insert("origin", "https://dashboard.example.test".parse().unwrap());
        headers.insert("sec-fetch-site", "same-site".parse().unwrap());
        assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("credential_cross_site_request_rejected"));
        headers.remove("sec-fetch-site");
        let mut cfg = (*app.cfg()).clone();
        cfg.notifications.credential_public_url = "https://new.example.test".into();
        app.set_config(cfg);
        assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("credential_origin_mismatch"));
        headers.insert("origin", "https://new.example.test".parse().unwrap());
        assert!(credential_guard(&app, peer, &headers, &uri, true).is_ok());
        headers.remove("authorization");
        assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("credential_authorization_header_required"));
    }

    #[tokio::test]
    async fn key_edits_fall_back_to_a_rewrite_when_formatting_cannot_be_kept() {
        let dir = std::env::temp_dir().join(format!("cliproxyapi-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let original = format!(
            "# indentless list\nauth-dir: {}\nclaude-api-key:\n- api-key: first\n  headers: {{X-Team: core}}\n",
            dir.display()
        );
        std::fs::write(&path, &original).unwrap();
        let app = App::new(Config::parse(&original).unwrap(), path.clone());
        let response = edit_config(&app, |doc| {
            crate::compat::add_key(
                doc,
                &crate::compat::NewKey {
                    group: "claude",
                    api_key: "second",
                    base_url: None,
                    models: vec![],
                    name: None,
                },
            )
        });
        assert_eq!(response.status(), StatusCode::OK);
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.claude_api_key.len(), 2);
        assert_eq!(cfg.claude_api_key[0].headers["X-Team"], "core");
        assert_eq!(std::fs::read_to_string(dir.join("config.yaml.bak")).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn structured_saves_validate_and_reject_stale_edits() {
        let dir = std::env::temp_dir().join(format!("cliproxyapi-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let original = format!("# Keep this comment\nport: 8317 # port note\nauth-dir: {}\n", dir.display());
        std::fs::write(&path, &original).unwrap();
        let app = App::new(Config::parse(&original).unwrap(), path.clone());
        let version = crate::config_editor::revision(&original);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: version.clone(),
                changes: json!({"port": 70000}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: version.clone(),
                changes: json!({"port": 9000}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# Keep this comment"));
        assert!(saved.contains("# port note"));
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody { revision: version, changes: json!({"request-retry": 4}).as_object().unwrap().clone() }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: crate::config_editor::revision(&saved),
                changes: json!({"request-retry": 4}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response: Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(response["restart_fields"], json!(["port"]));
        assert_eq!(response["values"]["request-retry"], json!(4));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    /// Verify that credential guard accepts http2 authority and rejects conflicting host.
    fn credential_guard_accepts_http2_authority_and_rejects_conflicting_host() {
        let cfg = Config {
            auth_dir: "/nonexistent/credential-h2-test".into(),
            management_key: "synthetic-h2-key".into(),
            tls: crate::config::Tls { enable: true, ..Default::default() },
            notifications: crate::notifications::Config { credential_ui_enabled: true, ..Default::default() },
            ..Default::default()
        };
        let app = App::new(cfg, std::path::PathBuf::from("/nonexistent/credential-h2-config.yaml"));
        let uri: axum::http::Uri = "https://dashboard.example.test/api/notifications/ops/credentials".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer synthetic-h2-key".parse().unwrap());
        headers.insert("origin", "https://dashboard.example.test".parse().unwrap());
        let peer = "192.0.2.1".parse().unwrap();
        assert!(credential_guard(&app, peer, &headers, &uri, true).is_ok());
        headers.insert("host", "dashboard.example.test:443".parse().unwrap());
        assert!(credential_guard(&app, peer, &headers, &uri, true).is_ok());
        headers.insert("host", "conflicting.example.test".parse().unwrap());
        assert_eq!(credential_guard(&app, peer, &headers, &uri, true), Err("invalid_credential_request_host"));
        headers.remove("host");
        let relative = "/api/notifications/ops/credentials".parse().unwrap();
        assert_eq!(credential_guard(&app, peer, &headers, &relative, true), Err("invalid_credential_request_host"));
    }
    #[test]
    fn bcrypt_management_keys() {
        let hash = bcrypt::hash("open sesame", 4).unwrap();
        assert!(management_key_matches("open sesame", &hash));
        assert!(management_key_matches("open sesame", &hash));
        assert!(!management_key_matches("wrong", &hash));
        assert!(management_key_matches("plain", "plain"));
        assert!(!management_key_matches("plain", "other"));
    }
}
