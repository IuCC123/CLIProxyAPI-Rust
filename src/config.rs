use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_yaml::Value as Yaml;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// Interface to bind. Use 0.0.0.0 to expose on the network (set api-keys first).
    pub host: String,
    pub port: u16,
    /// Directory holding OAuth credential files. Compatible with CLIProxyAPI.
    pub auth_dir: String,
    /// Keys clients must send (Authorization: Bearer, x-api-key or x-goog-api-key).
    /// Empty means no client authentication.
    pub api_keys: Vec<String>,
    /// Protects the dashboard and management API. Empty means localhost-only access.
    /// A bcrypt hash (as CLIProxyAPI stores it) works too.
    pub management_key: String,
    /// With a management key set, allow the dashboard from other machines (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_allow_remote: Option<bool>,
    /// Optional upstream proxy (http://, https://, socks5://).
    pub proxy_url: String,
    /// How many different accounts to try before giving up on a request.
    #[serde(deserialize_with = "lenient_u32")]
    pub request_retry: u32,
    /// How new sessions choose an account.
    pub routing: Routing,
    /// Smart routing prefers other accounts below this 5-hour remaining percentage.
    #[serde(deserialize_with = "percentage")]
    pub five_hour_reserve_percent: u8,
    /// Keep each coding session on its account until subscription quota is exhausted.
    pub session_affinity: bool,
    /// Forget inactive sessions after this many seconds (default one day).
    pub session_affinity_idle_seconds: u64,
    /// Keep an upstream websocket open to Codex when clients connect over websocket.
    pub codex_websockets: bool,
    /// Rewrite non-Claude-Code requests on Claude OAuth accounts so they look like Claude Code.
    pub claude_cloak: bool,
    /// Check Claude and ChatGPT subscriptions for banked rate-limit resets and allow
    /// spending them from the dashboard. Off by default: it uses unofficial endpoints.
    pub banked_resets: bool,
    /// Subscription exhaustion and confirmed recovery alerts; credentials live outside YAML.
    pub notifications: crate::notifications::Config,
    pub debug: bool,
    /// Serve HTTPS with this certificate.
    #[serde(skip_serializing_if = "Tls::is_off")]
    pub tls: Tls,
    /// Only route unprefixed model names to accounts without a `prefix`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub force_model_prefix: bool,
    /// Per-provider renames for OAuth accounts (`claude: [{name, alias, fork}]`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_model_alias: BTreeMap<String, Vec<OAuthAlias>>,
    /// Per-provider model patterns OAuth accounts must not serve (`*` wildcards).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
    /// CLIProxyAPI settings found in the file that have no effect here.
    #[serde(skip)]
    pub ignored: Vec<String>,
    pub claude_api_key: Vec<KeyEntry>,
    pub codex_api_key: Vec<KeyEntry>,
    pub gemini_api_key: Vec<KeyEntry>,
    /// Vertex AI express-mode API keys (service accounts go in the auth dir).
    pub vertex_api_key: Vec<KeyEntry>,
    /// Kimi Code (api.kimi.com/coding) or Moonshot platform keys.
    pub kimi_api_key: Vec<KeyEntry>,
    pub xai_api_key: Vec<KeyEntry>,
    pub meta_api_key: Vec<KeyEntry>,
    pub openai_compatibility: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Routing {
    /// The account with the most subscription quota left (falls back to round-robin).
    #[default]
    LeastUsed,
    /// Balance weekly renewal priority, 5-hour headroom and current account load.
    SmartQuota,
    RoundRobin,
    FillFirst,
}

/// Accepts `routing: fill-first` and CLIProxyAPI's `routing: { strategy: fill-first }`.
/// Strategies this crate doesn't have (weighted-round-robin) fall back to round-robin.
impl<'de> Deserialize<'de> for Routing {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Yaml::deserialize(d)?;
        let s = match &v {
            Yaml::String(s) => s.as_str(),
            Yaml::Mapping(m) => m.get("strategy").and_then(Yaml::as_str).unwrap_or_default(),
            _ => "",
        };
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "smart-quota" | "soonest-reset" => Routing::SmartQuota,
            "fill-first" => Routing::FillFirst,
            "round-robin" | "weighted-round-robin" => Routing::RoundRobin,
            _ => Routing::LeastUsed,
        })
    }
}

fn lenient_u32<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = Yaml::deserialize(d)?;
    Ok(v.as_i64().map(|n| n.clamp(0, u32::MAX as i64) as u32).unwrap_or(3))
}

fn percentage<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    let n = u8::deserialize(d)?;
    if n > 100 {
        return Err(serde::de::Error::custom("5-hour reserve must be a whole percentage between 0 and 100"));
    }
    Ok(n)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct Tls {
    pub enable: bool,
    pub cert: String,
    pub key: String,
}

impl Tls {
    fn is_off(&self) -> bool {
        !self.enable && self.cert.is_empty() && self.key.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct OAuthAlias {
    /// Upstream model name.
    pub name: String,
    /// Name clients use.
    pub alias: String,
    /// Keep serving `name` as well (otherwise the alias replaces it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct KeyEntry {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Restrict (and optionally rename) models served by this key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Clients must call `prefix/model` to reach this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Model patterns this key must not serve (`*` wildcards).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct CompatEntry {
    pub name: String,
    pub base_url: String,
    pub api_keys: Vec<String>,
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ModelAlias {
    /// Model name sent upstream.
    pub name: String,
    /// Name clients use. Defaults to `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl ModelAlias {
    pub fn public(&self) -> &str {
        self.alias.as_deref().filter(|a| !a.is_empty()).unwrap_or(&self.name)
    }
}

impl Default for Config {
    /// Initialize opt-in features as disabled and preserve the existing server defaults.
    fn default() -> Self {
        Self {
            host: default_host(),
            port: 8317,
            auth_dir: "~/.cli-proxy-api".into(),
            api_keys: vec![],
            management_key: String::new(),
            management_allow_remote: None,
            proxy_url: String::new(),
            request_retry: 3,
            routing: Routing::LeastUsed,
            five_hour_reserve_percent: 30,
            session_affinity: true,
            session_affinity_idle_seconds: 86_400,
            codex_websockets: true,
            claude_cloak: true,
            banked_resets: false,
            notifications: crate::notifications::Config::default(),
            debug: false,
            tls: Tls::default(),
            force_model_prefix: false,
            oauth_model_alias: BTreeMap::new(),
            oauth_excluded_models: BTreeMap::new(),
            ignored: vec![],
            claude_api_key: vec![],
            codex_api_key: vec![],
            gemini_api_key: vec![],
            vertex_api_key: vec![],
            kimi_api_key: vec![],
            xai_api_key: vec![],
            meta_api_key: vec![],
            openai_compatibility: vec![],
        }
    }
}

const TEMPLATE: &str = r#"# CLIProxyAPI-Rust configuration. Changes are picked up automatically.

host: "127.0.0.1"          # use 0.0.0.0 to expose on your network (set api-keys first!)
port: 8317
auth-dir: "~/.cli-proxy-api" # OAuth credentials (compatible with CLIProxyAPI)

# Keys your clients must send. Leave empty to allow anyone who can reach the port.
api-keys: []

# Protects the dashboard + management API. Empty = only reachable from localhost.
management-key: ""

proxy-url: ""               # optional upstream proxy, e.g. socks5://127.0.0.1:1080
request-retry: 3            # accounts to try before failing a request
routing: least-used         # new sessions: least-used | smart-quota | round-robin | fill-first
five-hour-reserve-percent: 30 # smart-quota: prefer other accounts below this remaining percentage
session-affinity: true      # keep a session on its subscription until quota is exhausted
session-affinity-idle-seconds: 86400 # expire assignments after a day without requests
codex-websockets: true      # native upstream websocket for Codex websocket clients
claude-cloak: true          # make non-Claude-Code clients look like Claude Code on OAuth accounts
banked-resets: false        # show and spend banked Claude/ChatGPT limit resets (unofficial endpoints)
notifications:
  enabled: false            # configure destinations under Config → Notifications
  time-zone: UTC            # IANA time zone for message timestamps; e.g. America/Denver
  provider-logos: true      # bundled logo thumbnails for Discord alerts
  credential-ui-enabled: false # opt in under Config → Notifications before entering secrets
  credential-public-url: ""  # public HTTPS dashboard origin for remote credential entry; e.g. https://proxy.example.net
  credential-proxy-cidrs: [] # optional advanced proxy compatibility; startup-only, never trust all clients
  destinations: []          # add credentials in the dashboard; private files/environment also supported
debug: false

# API keys (optional). Accounts (Claude, Codex, Antigravity, Kimi, xAI, Meta, Devin, Vertex)
# are added with `cliproxyapi-rust login <provider>` or from the dashboard.
claude-api-key: []
#  - api-key: "sk-ant-..."
#    base-url: "https://api.anthropic.com"   # optional

codex-api-key: []
#  - api-key: "sk-..."
#    base-url: "https://api.openai.com/v1"   # optional

gemini-api-key: []
#  - api-key: "AIza..."

vertex-api-key: []          # Vertex AI express mode; service accounts: `cliproxyapi-rust login vertex --file sa.json`
#  - api-key: "AQ..."

kimi-api-key: []
#  - api-key: "sk-kimi-..."                 # Kimi Code key
#  - api-key: "sk-..."                      # Moonshot platform key
#    base-url: "https://api.moonshot.ai/v1"

xai-api-key: []
#  - api-key: "xai-..."

meta-api-key: []
#  - api-key: "..."

openai-compatibility: []
#  - name: openrouter
#    base-url: "https://openrouter.ai/api/v1"
#    api-keys: ["sk-or-..."]
#    models:
#      - name: "moonshotai/kimi-k3"
#        alias: "kimi-k3"
"#;

/// 127.0.0.1, unless the environment says otherwise (the Docker image binds all interfaces).
fn default_host() -> String {
    std::env::var("CLIPROXYAPI_RUST_DEFAULT_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

/// The commented starter config, bound to the default host.
pub fn template() -> String {
    TEMPLATE.replacen("host: \"127.0.0.1\"", &format!("host: \"{}\"", default_host()), 1)
}

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| (p == "~").then_some(""))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, template()).with_context(|| format!("writing {}", path.display()))?;
            tracing::info!("created default config at {}", path.display());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse compatible YAML and validate routing plus notification configuration before applying it.
    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut doc: Yaml = serde_yaml::from_str(text).context("invalid config")?;
        let ignored = crate::compat::normalize(&mut doc);
        let mut cfg: Config = serde_yaml::from_value(doc).context("invalid config")?;
        cfg.notifications.validate().map_err(anyhow::Error::msg)?;
        cfg.ignored = ignored;
        Ok(cfg)
    }

    pub fn auth_dir(&self) -> PathBuf {
        expand_home(&self.auth_dir)
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }
}
