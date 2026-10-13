mod accounts;
mod affinity;
mod antigravity;
mod banked_resets;
mod compat;
mod config;
mod config_editor;
mod device;
mod devin;
mod formats;
mod ir;
mod media;
mod mgmt;
#[cfg(all(test, unix))]
mod notification_credential_tests;
#[cfg(test)]
mod notification_integration_tests;
mod notifications;
mod oauth;
mod proxy;
mod quota;
#[cfg(test)]
mod routing_tests;
mod schema;
mod server;
mod sse;
mod state;
mod upstream;
mod vertex;
mod ws;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::accounts::Provider;
use crate::config::Config;
use crate::state::App;

#[derive(Parser)]
#[command(
    name = "cliproxyapi-rust",
    version,
    about = "OpenAI / Claude / Gemini compatible proxy for your Claude, ChatGPT, Gemini, Antigravity, Grok, Kimi, Meta, Devin and Vertex accounts"
)]
struct Cli {
    /// Path to the config file (created with defaults if missing).
    #[arg(short, long, global = true, env = "CLIPROXYAPI_RUST_CONFIG", default_value = "config.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the proxy server (default).
    Serve,
    /// Sign in to an account: claude, codex, antigravity, kimi, xai, meta, devin or vertex.
    Login {
        provider: String,
        /// Print the URL instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
        /// Vertex: path to a service account key (JSON).
        #[arg(long)]
        file: Option<PathBuf>,
        /// Vertex: region, e.g. us-central1 or global.
        #[arg(long, default_value = "us-central1")]
        location: String,
    },
    /// Show what the config and auth directory contain, without starting the server.
    /// Handy before switching from CLIProxyAPI.
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    // CLIProxyAPI's Go-style flags (-config, -claude-login, ...) work too.
    let cli = Cli::parse_from(compat::translate_args(std::env::args().collect()));
    if matches!(cli.cmd, Some(Cmd::Check)) {
        return check(&cli.config);
    }
    let cfg = Config::load(&cli.config)?;
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| if cfg.debug { "cliproxyapi_rust=debug".into() } else { "cliproxyapi_rust=info".into() });
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).compact().init();
    std::fs::create_dir_all(cfg.auth_dir()).ok();

    let app = App::new(cfg, cli.config.clone());
    match cli.cmd {
        Some(Cmd::Login { provider, no_browser, file, location }) => {
            login(app, &provider, no_browser, file, &location).await
        }
        _ => serve(app).await,
    }
}

/// Initialize the shared proxy and optional notification worker alongside existing background tasks.
async fn serve(app: Arc<App>) -> Result<()> {
    let cfg = app.cfg();
    if !cfg.is_loopback() && cfg.api_keys.is_empty() {
        tracing::warn!(
            "listening on {} without api-keys: anyone who can reach this port can use your accounts",
            cfg.host
        );
    }
    let addr: SocketAddr = format!("{}:{}", if cfg.host.is_empty() { "0.0.0.0" } else { &cfg.host }, cfg.port)
        .parse()
        .or_else(|_| format!("[{}]:{}", cfg.host, cfg.port).parse())
        .context("invalid host/port")?;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    if !cfg.ignored.is_empty() {
        tracing::warn!("these CLIProxyAPI settings have no effect here: {}", cfg.ignored.join(", "));
    }
    let tls = if cfg.tls.enable {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cfg.tls.cert, &cfg.tls.key)
            .await
            .context("loading tls.cert / tls.key")?;
        Some(tls)
    } else {
        None
    };

    tokio::spawn(oauth::refresher(app.clone()));
    tokio::spawn(antigravity::version_updater(app.clone()));
    tokio::spawn(quota::poller(app.clone()));
    tokio::spawn(notifications::worker(app.clone()));
    tokio::spawn(watch(app.clone()));

    let scheme = if cfg.tls.enable { "https" } else { "http" };
    let shown = if addr.ip().is_unspecified() {
        format!("{scheme}://127.0.0.1:{}", addr.port())
    } else {
        format!("{scheme}://{addr}")
    };
    let accounts = app.pool.all();
    println!();
    println!("  \x1b[1mCLIProxyAPI-Rust\x1b[0m {}", env!("CARGO_PKG_VERSION"));
    println!("  dashboard  {shown}");
    println!("  openai     {shown}/v1");
    println!("  anthropic  {shown}");
    println!("  gemini     {shown}/v1beta");
    println!("  accounts   {} loaded from {}", accounts.len(), cfg.auth_dir().display());
    if accounts.is_empty() {
        println!(
            "\n  No accounts yet. Run `cliproxyapi-rust login <provider>` (claude, codex, antigravity, kimi, xai,\n  meta, devin, vertex) or open the dashboard."
        );
    }
    println!();

    let service = server::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    if let Some(tls) = tls {
        let handle = axum_server::Handle::new();
        let stop = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            stop.graceful_shutdown(Some(Duration::from_secs(10)));
        });
        axum_server::from_tcp_rustls(listener.into_std()?, tls)?.handle(handle).serve(service).await?;
    } else {
        axum::serve(listener, service).with_graceful_shutdown(shutdown_signal()).await?;
    }
    app.sessions.save();
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop` / systemd.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async { if let Some(t) = term.as_mut() { t.recv().await; } else { std::future::pending::<()>().await } } => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn auth_signature(dir: &Path) -> Vec<(String, Option<SystemTime>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .map(|e| (e.file_name().to_string_lossy().to_string(), mtime(&e.path())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Hot-reloads the config file and the auth directory.
async fn watch(app: Arc<App>) {
    let mut cfg_time = mtime(&app.cfg_path);
    let mut auth = auth_signature(&app.cfg().auth_dir());
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        app.sessions.flush(app.cfg().session_affinity_idle_seconds);
        let t = mtime(&app.cfg_path);
        if t != cfg_time {
            cfg_time = t;
            let _guard = app.config_write.lock();
            match std::fs::read_to_string(&app.cfg_path).map_err(anyhow::Error::from).and_then(|s| Config::parse(&s)) {
                Ok(cfg) => match app.notifications.validate_destination_change(&app, &cfg) {
                    Ok(()) => {
                        tracing::info!("config reloaded");
                        app.set_config(cfg);
                    }
                    Err(message) => tracing::error!("config not reloaded: {message}"),
                },
                Err(e) => tracing::error!("config not reloaded: {e:#}"),
            }
        }
        let sig = auth_signature(&app.cfg().auth_dir());
        if sig != auth {
            auth = sig;
            if !app.reload_suppressed() {
                tracing::info!("auth directory changed, reloading accounts");
                app.reload_accounts();
            }
        }
    }
}

async fn login(app: Arc<App>, provider: &str, no_browser: bool, file: Option<PathBuf>, location: &str) -> Result<()> {
    let Some(provider) = Provider::parse(provider) else {
        anyhow::bail!("unknown provider `{provider}` (claude, codex, antigravity, kimi, xai, meta, devin, vertex)")
    };
    if provider == Provider::Vertex {
        let Some(path) = file else { anyhow::bail!("pass the service account key with --file key.json") };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let label = vertex::import(&app, &text, location).await?;
        println!("\n✓ Added Vertex service account {label}");
        return Ok(());
    }
    let (state, login) = mgmt::start_login(&app, provider).await?;
    if login.kind == "device" {
        println!(
            "\nOpen this URL and enter the code:\n\n  {}\n\n  code: \x1b[1m{}\x1b[0m\n",
            login.url,
            login.user_code.unwrap_or_default()
        );
    } else {
        println!("\nOpen this URL to sign in:\n\n  {}\n", login.url);
    }
    if !no_browser && open::that(&login.url).is_err() {
        println!("(could not open a browser automatically)");
    }
    if login.kind == "device" {
        println!("Waiting for approval…");
    } else if login.callback {
        println!("Waiting for the browser to finish… (or paste the redirect URL here)");
    } else {
        println!("Paste the URL your browser was redirected to:");
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if !line.trim().is_empty() && tx.blocking_send(line.trim().to_string()).is_err() {
                break;
            }
            line.clear();
        }
    });
    loop {
        tokio::select! {
            Some(input) = rx.recv(), if login.kind != "device" => {
                let (code, _) = mgmt::parse_pasted(&input);
                match mgmt::complete_login(&app, &state, &code).await {
                    Ok(label) => { println!("\n✓ Signed in as {label}"); return Ok(()); }
                    Err(e) => { println!("\n✗ {e:#}"); return Err(e); }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                let status = app.logins.lock().get(&state).map(|l| (l.status, l.message.clone()));
                match status {
                    Some(("done", m)) => { println!("\n✓ Signed in as {}", m.unwrap_or_default()); return Ok(()); }
                    Some(("error", m)) => anyhow::bail!(m.unwrap_or_default()),
                    _ => {}
                }
            }
        }
    }
}

fn check(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = Config::parse(&text)?;
    let bind = if cfg.host.is_empty() { "0.0.0.0" } else { &cfg.host };
    let key = &cfg.management_key;
    let hashed = ["$2a$", "$2b$", "$2y$"].iter().any(|p| key.starts_with(p));
    println!("\n  \x1b[1mCLIProxyAPI-Rust\x1b[0m {} · {}\n", env!("CARGO_PKG_VERSION"), path.display());
    println!("  listen       {bind}:{}{}", cfg.port, if cfg.tls.enable { " (https)" } else { "" });
    println!(
        "  client keys  {}",
        if cfg.api_keys.is_empty() { "none (open)".to_string() } else { cfg.api_keys.len().to_string() }
    );
    println!(
        "  dashboard    {}",
        match (key.is_empty(), hashed, cfg.management_allow_remote) {
            (true, _, _) => "localhost only, no key".to_string(),
            (false, h, remote) => format!(
                "management key{}{}",
                if h { " (bcrypt hash)" } else { "" },
                if remote == Some(false) { ", localhost only" } else { "" }
            ),
        }
    );
    println!("  routing      {:?}, {} accounts per request", cfg.routing, cfg.request_retry.max(1));
    println!(
        "  affinity     {}, idle expiry {}s",
        if cfg.session_affinity { "on" } else { "off" },
        cfg.session_affinity_idle_seconds
    );
    if !cfg.proxy_url.is_empty() {
        println!("  proxy        {}", cfg.proxy_url);
    }
    let dir = cfg.auth_dir();
    println!("  auth dir     {}", dir.display());

    let pool = accounts::Pool::default();
    pool.reload(&cfg);
    let all = pool.all();
    println!("\n  accounts     {}", all.len());
    let mut by: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for a in &all {
        let e = by.entry(a.provider.as_str()).or_default();
        if a.is_oauth() { e.0 += 1 } else { e.1 += 1 }
    }
    for (p, (oauth, keys)) in by {
        let mut parts = vec![];
        if oauth > 0 {
            parts.push(format!("{oauth} signed in"));
        }
        if keys > 0 {
            parts.push(format!("{keys} API key{}", if keys == 1 { "" } else { "s" }));
        }
        println!("    {p:<14}{}", parts.join(", "));
    }
    println!("  models       {}", pool.models().len());

    let mut skipped = vec![];
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") && accounts::read_oauth_file(&p).is_none() {
            let kind = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v["type"].as_str().map(String::from))
                .unwrap_or_else(|| "unknown".into());
            skipped.push(format!("{} (type {kind})", p.file_name().unwrap_or_default().to_string_lossy()));
        }
    }
    if !cfg.ignored.is_empty() {
        println!("\n  not used here: {}", cfg.ignored.join(", "));
    }
    if !skipped.is_empty() {
        println!("  skipped credential files: {}", skipped.join(", "));
    }
    println!();
    Ok(())
}
