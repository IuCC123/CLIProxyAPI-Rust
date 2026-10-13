use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::accounts::Pool;
use crate::config::Config;
use crate::ir::Usage;

pub struct App {
    cfg: ArcSwap<Config>,
    pub cfg_path: PathBuf,
    pub config_write: Mutex<()>,
    /// Startup settings remain separate from hot-reloaded settings.
    pub startup_config: Config,
    pub pool: Pool,
    pub sessions: Arc<crate::affinity::Sessions>,
    pub http: Http,
    pub notifications: crate::notifications::Service,
    pub stats: Stats,
    pub logins: Mutex<HashMap<String, crate::mgmt::Login>>,
    pub reset_quotes: Mutex<HashMap<String, crate::banked_resets::Quote>>,
    #[cfg(test)]
    pub reset_test_origin: Mutex<Option<String>>,
    pub started: DateTime<Utc>,
    pub live: broadcast::Sender<String>,
    /// Ignore our own writes to the auth dir in the file watcher.
    quiet_until: AtomicI64,
}

impl App {
    /// Construct shared application state with startup-pinned notification permissions and lazy journal access.
    pub fn new(cfg: Config, cfg_path: PathBuf) -> Arc<Self> {
        let pool = Pool::default();
        pool.reload(&cfg);
        let (live, _) = broadcast::channel(512);
        let sessions = Arc::new(crate::affinity::Sessions::load(&cfg.auth_dir(), cfg.session_affinity_idle_seconds));
        Arc::new(Self {
            notifications: crate::notifications::Service::new(&cfg),
            http: Http::new(&cfg.proxy_url),
            startup_config: cfg.clone(),
            cfg: ArcSwap::from_pointee(cfg),
            cfg_path,
            config_write: Mutex::new(()),
            pool,
            sessions,
            stats: Stats::default(),
            logins: Mutex::new(HashMap::new()),
            reset_quotes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            reset_test_origin: Mutex::new(None),
            started: Utc::now(),
            live,
            quiet_until: AtomicI64::new(0),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.load_full()
    }

    pub fn set_config(&self, cfg: Config) {
        self.http.set_default_proxy(&cfg.proxy_url);
        self.pool.reload(&cfg);
        self.cfg.store(Arc::new(cfg));
        self.broadcast("accounts", serde_json::Value::Null);
    }

    pub fn reload_accounts(&self) {
        self.pool.reload(&self.cfg());
        self.broadcast("accounts", serde_json::Value::Null);
    }

    pub fn suppress_reload(&self) {
        self.quiet_until.store(Utc::now().timestamp() + 3, Ordering::Relaxed);
    }

    pub fn reload_suppressed(&self) -> bool {
        Utc::now().timestamp() < self.quiet_until.load(Ordering::Relaxed)
    }

    pub fn broadcast(&self, kind: &str, data: impl Serialize) {
        if self.live.receiver_count() > 0 {
            let msg = serde_json::json!({ "type": kind, "data": data }).to_string();
            let _ = self.live.send(msg);
        }
    }
}

// ------------------------------------------------------------------------ http

pub struct Http {
    default_proxy: Mutex<String>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl Http {
    fn new(proxy: &str) -> Self {
        Self { default_proxy: Mutex::new(proxy.to_string()), clients: Mutex::new(HashMap::new()) }
    }

    fn set_default_proxy(&self, proxy: &str) {
        *self.default_proxy.lock() = proxy.to_string();
    }

    /// Client for the given proxy (falls back to the configured default).
    pub fn client(&self, proxy: Option<&str>) -> reqwest::Client {
        self.build(proxy, None, false)
    }

    /// Spending requests must never follow redirects or retry in the HTTP layer.
    pub fn for_reset(&self, proxy: Option<&str>) -> reqwest::Client {
        self.build(proxy, None, true)
    }

    /// Antigravity accounts each get their own HTTP/1.1 pool, as the IDE does;
    /// Google's backend treats shared HTTP/2 connections less kindly.
    pub fn for_account(&self, acct: &crate::accounts::Account) -> reqwest::Client {
        match acct.provider {
            crate::accounts::Provider::Antigravity => self.build(acct.proxy_url.as_deref(), Some(&acct.id), false),
            _ => self.build(acct.proxy_url.as_deref(), None, false),
        }
    }

    fn build(&self, proxy: Option<&str>, h1_pool: Option<&str>, reset: bool) -> reqwest::Client {
        let proxy =
            proxy.filter(|p| !p.is_empty()).map(String::from).unwrap_or_else(|| self.default_proxy.lock().clone());
        let mut key = match h1_pool {
            Some(id) => format!("{proxy}\0h1:{id}"),
            None => proxy.clone(),
        };
        if reset {
            key.push_str("\0reset");
        }
        let mut clients = self.clients.lock();
        if let Some(c) = clients.get(&key) {
            return c.clone();
        }
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(600))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30));
        if reset {
            b = b.redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never());
        }
        if proxy == "direct" || proxy == "none" {
            // CLIProxyAPI's spelling for "no proxy, not even the default one".
            b = b.no_proxy();
        } else if !proxy.is_empty() {
            match reqwest::Proxy::all(&proxy) {
                Ok(p) => b = b.proxy(p),
                Err(e) => tracing::error!("invalid proxy-url {proxy}: {e}"),
            }
        }
        if h1_pool.is_some() {
            b = b.http1_only().pool_idle_timeout(Duration::from_secs(200));
        }
        let c = b.build().expect("http client");
        clients.insert(key, c.clone());
        c
    }
}

// ----------------------------------------------------------------------- stats

#[derive(Debug, Clone, Serialize)]
pub struct RoutingAttempt {
    pub account_id: String,
    pub account: String,
    pub reason: &'static str,
    pub previous_account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestLog {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub client: &'static str,
    pub provider: String,
    pub model: String,
    pub account: String,
    /// Client-scoped fingerprint; never the client's raw session ID or API key.
    pub session_id: Option<String>,
    pub session_source: Option<&'static str>,
    pub routing_strategy: crate::config::Routing,
    pub routing_reason: Option<&'static str>,
    pub routing_warning: Option<&'static str>,
    pub routing_attempts: Vec<RoutingAttempt>,
    pub status: u16,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub stream: bool,
    pub transport: &'static str,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub ok: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Bucket {
    pub minute: i64,
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
}

#[derive(Default)]
pub struct Stats {
    pub totals: Mutex<Totals>,
    pub recent: Mutex<VecDeque<RequestLog>>,
    pub series: Mutex<VecDeque<Bucket>>,
    pub active: std::sync::atomic::AtomicU64,
    next_id: std::sync::atomic::AtomicU64,
}

const RECENT: usize = 300;
const MINUTES: usize = 60;

impl Stats {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn record(&self, log: &RequestLog) {
        let ok = log.status < 400;
        {
            let mut t = self.totals.lock();
            t.requests += 1;
            if ok {
                t.ok += 1
            } else {
                t.failed += 1
            }
            t.input_tokens += log.input_tokens;
            t.output_tokens += log.output_tokens;
            t.cache_tokens += log.cache_tokens;
        }
        {
            let minute = log.ts.timestamp() / 60;
            let mut s = self.series.lock();
            if s.back().map(|b| b.minute) != Some(minute) {
                s.push_back(Bucket { minute, ..Default::default() });
                while s.len() > MINUTES {
                    s.pop_front();
                }
            }
            let b = s.back_mut().unwrap();
            b.requests += 1;
            if !ok {
                b.failed += 1;
            }
            b.tokens += log.input_tokens + log.output_tokens + log.cache_tokens;
        }
        let mut r = self.recent.lock();
        r.push_back(log.clone());
        while r.len() > RECENT {
            r.pop_front();
        }
    }

    pub fn series(&self) -> Vec<Bucket> {
        let now = Utc::now().timestamp() / 60;
        let s = self.series.lock();
        (0..MINUTES as i64)
            .rev()
            .map(|ago| {
                let m = now - ago;
                s.iter().find(|b| b.minute == m).cloned().unwrap_or(Bucket { minute: m, ..Default::default() })
            })
            .collect()
    }
}

pub fn usage_tokens(u: &Usage) -> (u64, u64, u64) {
    (u.input + u.cache_write, u.output, u.cache_read)
}
