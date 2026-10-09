//! Isolated webhook transport. No OAuth headers, ambient proxies, redirects or raw error logging.
use super::{Destination, Format, PrivateEndpoint, state::Event, store::safe_file};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};
use std::{
    io::Read,
    net::{IpAddr, SocketAddr},
    path::Path,
    time::Duration,
};
use url::Url;

pub struct Credentials {
    url: Url,
    bearer: Option<HeaderValue>,
}
// Intentionally no Debug/Serialize; the URL itself is a credential.
/// Resolve external URL/token bindings from the environment or bounded private secret files.
pub fn credentials(dir: &Path, d: &Destination) -> Result<Credentials, &'static str> {
    if !super::valid_id(&d.id) {
        return Err("invalid_destination");
    }
    let prefix = format!("CLIPROXYAPI_NOTIFY_{}", d.id.to_ascii_uppercase().replace('-', "_"));
    let read = |suffix: &str, extension: &str, required: bool| -> Result<Option<String>, &'static str> {
        match std::env::var(format!("{prefix}_{suffix}")) {
            Ok(value) => {
                if value.len() > 8192 {
                    return Err("credential_too_large");
                }
                Ok(Some(value.trim().into()))
            }
            Err(std::env::VarError::NotUnicode(_)) => Err("invalid_credential"),
            Err(std::env::VarError::NotPresent) => {
                let path = dir.join(format!("{}.{extension}", d.id));
                match std::fs::symlink_metadata(&path) {
                    Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(_) => return Err("credential_unavailable"),
                    Ok(_) => {}
                }
                // Reject symlinked directory components before opening the fixed filename.
                let mut component = std::path::PathBuf::new();
                for part in dir.components() {
                    component.push(part);
                    // A Windows drive/UNC prefix is not a filesystem entry until its root is appended.
                    if matches!(part, std::path::Component::Prefix(_)) {
                        continue;
                    }
                    if std::fs::symlink_metadata(&component)
                        .map_err(|_| "credential_unavailable")?
                        .file_type()
                        .is_symlink()
                    {
                        return Err("insecure_credential_directory");
                    }
                }
                let mut value = String::new();
                safe_file(&path)?.take(8193).read_to_string(&mut value).map_err(|_| "invalid_credential")?;
                if value.len() > 8192 {
                    return Err("credential_too_large");
                }
                Ok(Some(value.trim().into()))
            }
        }
    };
    let url = read("URL", "url", true)?.ok_or("credential_unavailable")?;
    let bearer = read("BEARER_TOKEN", "bearer", false)?;
    if bearer.as_ref().is_some_and(|value| value.is_empty()) {
        return Err("invalid_bearer");
    }
    parse_credentials(&url, bearer.as_deref())
}
/// Accept only bounded HTTPS URLs without userinfo or fragments and validate the bearer header.
pub fn parse_credentials(url: &str, bearer: Option<&str>) -> Result<Credentials, &'static str> {
    if url.len() > 8192 || bearer.is_some_and(|token| token.len() > 8192) {
        return Err("credential_too_large");
    }
    let url = Url::parse(url.trim()).map_err(|_| "invalid_url")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err("unsafe_url");
    }
    let bearer = bearer
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let mut header =
                HeaderValue::from_str(&format!("Bearer {}", value.trim())).map_err(|_| "invalid_bearer")?;
            header.set_sensitive(true);
            Ok::<HeaderValue, &'static str>(header)
        })
        .transpose()?;
    Ok(Credentials { url, bearer })
}
/// Detect external bindings even when malformed so managed credentials cannot override them.
pub fn external_present(dir: &Path, d: &Destination) -> bool {
    if !super::valid_id(&d.id) {
        return false;
    }
    let prefix = format!("CLIPROXYAPI_NOTIFY_{}", d.id.to_ascii_uppercase().replace('-', "_"));
    ["URL", "BEARER_TOKEN"].iter().any(|suffix| std::env::var_os(format!("{prefix}_{suffix}")).is_some())
        || ["url", "bearer"].iter().any(|extension| {
            match std::fs::symlink_metadata(dir.join(format!("{}.{extension}", d.id))) {
                Ok(_) => true,
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            }
        })
}
/// Resolve the authoritative external source first, otherwise load the managed bundle.
pub fn resolved_credentials(dir: &Path, root: &Path, d: &Destination) -> Result<Credentials, &'static str> {
    if !super::valid_id(&d.id) {
        return Err("invalid_destination");
    }
    if external_present(dir, d) {
        return credentials(dir, d);
    }
    if !super::credentials::present(root, &d.id) {
        return Err("credential_unavailable");
    }
    let bundle = super::credentials::read(root, &d.id)?;
    parse_credentials(&bundle.url, bundle.bearer_token.as_deref())
}
pub struct Outcome {
    pub success: bool,
    pub retry: bool,
    pub status: Option<u16>,
    pub reason: &'static str,
    pub retry_after: Option<i64>,
}
impl Outcome {
    /// Return a fixed diagnostic category without retaining raw transport errors or credential values.
    fn error(reason: &'static str, retry: bool) -> Self {
        Self { success: false, retry, status: None, reason, retry_after: None }
    }
}
/// Human presentation is resolved at send time; never serialized into the durable outbox.
pub struct Presentation {
    display_name: Option<String>,
    time_zone: chrono_tz::Tz,
    provider_logos: bool,
}
impl Presentation {
    /// Resolve a sanitized display name and time zone for this delivery without persisting either.
    pub fn new(name: Option<&str>, time_zone: chrono_tz::Tz) -> Self {
        Self { display_name: name.map(safe_name), time_zone, provider_logos: true }
    }
    /// Apply the current Discord thumbnail preference to this delivery.
    pub fn with_provider_logos(mut self, enabled: bool) -> Self {
        self.provider_logos = enabled;
        self
    }
    /// Format human timestamps in the selected zone without visible zone names or offsets.
    fn local(&self, at: chrono::DateTime<chrono::Utc>) -> String {
        at.with_timezone(&self.time_zone).format("%Y-%m-%d %H:%M:%S").to_string()
    }
}
/// Bound display names and strip control and directional characters before presenting them.
pub fn safe_name(name: &str) -> String {
    let name:String=name.chars().filter(|c|!c.is_control()&&!matches!(*c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\u{feff}')).take(160).collect();
    let name = name.trim();
    if name.is_empty() { "Subscription".into() } else { name.into() }
}
/// Escape display names for the target chat format without enabling mentions or markup.
fn chat_name(name: &str, format: Format) -> String {
    let mut escaped = String::new();
    for c in name.chars() {
        // Break automatic mentions/email/URL links while retaining the visible name.
        if matches!(c, '@' | '.' | ':') {
            escaped.push(c);
            escaped.push('\u{200b}');
            continue;
        }
        if format == Format::Slack {
            match c {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                _ => escaped.push(c),
            }
        } else {
            if matches!(format, Format::Discord | Format::Mattermost | Format::Teams)
                && matches!(c, '\\' | '*' | '_' | '`' | '~' | '[' | ']' | '(' | ')' | '<' | '>' | '|' | '#')
            {
                escaped.push('\\');
            }
            escaped.push(c);
        }
    }
    escaped
}
/// Translate supported quota scopes into readable limit labels.
fn limit_label(window: &str, model: Option<&str>) -> &'static str {
    match (window, model) {
        ("5h", _) => "5-hour limit",
        ("week opus", _) | ("week", Some("opus")) => "Weekly Opus limit",
        ("week sonnet", _) | ("week", Some("sonnet")) => "Weekly Sonnet limit",
        ("week overage", _) => "Weekly overage limit",
        ("week", _) => "Weekly limit",
        ("day", _) => "Daily limit",
        ("unknown opus", _) | ("unknown", Some("opus")) => "Opus quota",
        ("unknown sonnet", _) | ("unknown", Some("sonnet")) => "Sonnet quota",
        _ => "Subscription quota",
    }
}
/// Describe exhaustion, partial recovery, or full availability using local times and current identity.
fn text(e: &Event, presentation: &Presentation, format: Format) -> String {
    if e.event == "notification.test" {
        return format!("Notification delivery test\nSent: {}", presentation.local(e.observed_at));
    }
    let name = chat_name(presentation.display_name.as_deref().unwrap_or("Subscription"), format);
    let provider = match e.provider.as_str() {
        "claude" => "Claude",
        "codex" => "ChatGPT / Codex",
        _ => "AI provider",
    };
    let limit = limit_label(&e.window, e.model.as_deref());
    let title = match e.event.as_str() {
        "quota.exhausted" => format!("{limit} exhausted"),
        "quota.window_recovered" => format!("{limit} available again"),
        "quota.available" => "Subscription quota available again".into(),
        _ => "Subscription quota update".into(),
    };
    let timestamp_label = if e.event == "quota.exhausted" { "Detected" } else { "Confirmed available" };
    let mut text = format!("{title}\n{provider} · {name}\n{timestamp_label}: {}", presentation.local(e.observed_at));
    if e.event == "quota.exhausted" {
        let reset = e.resets_at.map(|reset| presentation.local(reset)).unwrap_or_else(|| "not provided".into());
        text.push_str(&format!("\nEstimated reset: {reset}"));
    }
    if e.event == "quota.window_recovered" && !e.remaining_blockers.is_empty() {
        let blockers: Vec<_> = e.remaining_blockers.iter().map(|window| limit_label(window, None)).collect();
        text.push_str(&format!("\nOther limits still exhausted: {}", blockers.join(", ")));
    }
    text
}
/// Build the platform-specific request while preserving generic event IDs and UTC timestamps.
pub fn payload(d: &Destination, e: &Event, presentation: &Presentation) -> Value {
    let text = text(e, presentation, d.format);
    match d.format {
        Format::Generic => {
            let mut value = json!(e);
            if e.event == "notification.test" {
                for field in ["subscription", "provider", "window", "model", "used", "remaining_blockers", "resets_at"]
                {
                    value.as_object_mut().unwrap().remove(field);
                }
                value["message"] = json!("Notification delivery test");
            } else {
                value["subscription_display_name"] = json!(presentation.display_name);
            }
            value["time_zone"] = json!(presentation.time_zone.name());
            value["observed_at_local"] = json!(e.observed_at.with_timezone(&presentation.time_zone).to_rfc3339());
            value["resets_at_local"] =
                json!(e.resets_at.map(|at| at.with_timezone(&presentation.time_zone).to_rfc3339()));
            value
        }
        Format::Discord => {
            let mut value = json!({"content":text,"allowed_mentions":{"parse":[]}});
            let logos = logos(d, e, presentation);
            if !logos.is_empty() {
                value["embeds"]=json!(logos.iter().map(|logo|json!({"title":if e.event=="notification.test"{format!("{} logo preview",logo.title)}else{logo.title.into()},"color":logo.color,"thumbnail":{"url":format!("attachment://{}",logo.filename)}})).collect::<Vec<_>>());
                value["attachments"]=json!(logos.iter().enumerate().map(|(id,logo)|json!({"id":id,"filename":logo.filename,"description":format!("{} logo",logo.title)})).collect::<Vec<_>>());
            }
            value
        }
        Format::Slack => {
            json!({"text":text,"mrkdwn":false,"link_names":false,"unfurl_links":false,"unfurl_media":false})
        }
        Format::Mattermost => json!({"text":text}),
        Format::Teams => {
            json!({"type":"message","attachments":[{"contentType":"application/vnd.microsoft.card.adaptive","contentUrl":null,"content":{"$schema":"http://adaptivecards.io/schemas/adaptive-card.json","type":"AdaptiveCard","version":"1.2","body":[{"type":"TextBlock","text":text,"wrap":true}]}}]})
        }
        Format::Telegram => json!({"chat_id":d.chat_id,"text":text,"disable_web_page_preview":true}),
    }
}
struct Logo {
    filename: &'static str,
    title: &'static str,
    color: u32,
    bytes: &'static [u8],
}
static CLAUDE_LOGO: Logo =
    Logo { filename: "claude.png", title: "Claude", color: 0xD97757, bytes: include_bytes!("assets/claude.png") };
static CODEX_LOGO: Logo = Logo {
    filename: "codex.png",
    title: "ChatGPT / Codex",
    color: 0x10A37F,
    bytes: include_bytes!("assets/codex.png"),
};
/// Select bundled Discord thumbnails only when enabled and the event identifies a supported provider.
fn logos(d: &Destination, e: &Event, presentation: &Presentation) -> Vec<&'static Logo> {
    if d.format != Format::Discord || !presentation.provider_logos {
        return Vec::new();
    }
    if e.event == "notification.test" {
        return vec![&CLAUDE_LOGO, &CODEX_LOGO];
    }
    if !matches!(e.event.as_str(), "quota.exhausted" | "quota.window_recovered" | "quota.available") {
        return Vec::new();
    }
    match e.provider.as_str() {
        "claude" => vec![&CLAUDE_LOGO],
        "codex" => vec![&CODEX_LOGO],
        _ => Vec::new(),
    }
}
/// Encode Discord JSON and bundled PNG attachments using a fresh multipart boundary.
fn multipart_body(payload: &Value, logos: &[&Logo]) -> (String, Vec<u8>) {
    let json = payload.to_string();
    // Everything outside JSON is fixed metadata. Random boundaries are also checked
    // against every part to prevent a payload from ever splitting the framing.
    let boundary = loop {
        let boundary = format!("cliproxy-{}", uuid::Uuid::new_v4().simple());
        if !json.contains(&boundary)
            && logos.iter().all(|logo| !logo.bytes.windows(boundary.len()).any(|part| part == boundary.as_bytes()))
        {
            break boundary;
        }
    };
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{json}\r\n").as_bytes());
    for (id, logo) in logos.iter().enumerate() {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files[{id}]\"; filename=\"{}\"\r\nContent-Type: image/png\r\n\r\n",logo.filename).as_bytes());
        body.extend_from_slice(logo.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}
/// Check IPv4 or IPv6 prefix membership without DNS resolution.
pub fn cidr_contains(cidr: &str, ip: IpAddr) -> bool {
    let Some((network, bits)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(network) = network.parse::<IpAddr>() else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };
    match (network, ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}
/// Normalize IPv4-mapped IPv6 addresses before applying network permissions.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}
/// Recognize only RFC1918 IPv4 and IPv6 unique-local addresses eligible for explicit exceptions.
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private(),
        IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}
/// Exclude private, metadata, documentation, transition, and other special-use destination addresses.
pub fn public(ip: IpAddr) -> bool {
    let ip = normalize(ip);
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            v.octets() != [168, 63, 129, 16]
                && !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_multicast()
                && !v.is_broadcast()
                && !v.is_unspecified()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0)
                && !(a == 192 && b == 0 && c == 2)
                && !(a == 192 && b == 88 && c == 99)
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 198 && b == 51 && c == 100)
                && !(a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            // Only native global unicast; exclude transition/tunneling and documentation.
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}
/// Allow public addresses or exact private host/port/CIDR exceptions; never allow metadata or loopback.
pub fn allowed(ip: IpAddr, host: &str, port: u16, endpoints: &[PrivateEndpoint]) -> bool {
    let ip = normalize(ip);
    if ip == "fd00:ec2::254".parse::<IpAddr>().unwrap() {
        return false;
    }
    if public(ip) {
        return true;
    }
    // Private RFC1918/ULA only: no metadata, loopback, link-local or special-use bypass.
    private(ip)
        && endpoints.iter().any(|entry| {
            entry.host.eq_ignore_ascii_case(host)
                && entry.port == port
                && entry.cidrs.iter().any(|c| cidr_contains(c, ip))
        })
}
/// Vet every resolved address and pin connections to that set while retaining TLS verification.
async fn client(
    url: &Url,
    endpoints: &[PrivateEndpoint],
    ca_file: Option<&Path>,
) -> Result<reqwest::Client, &'static str> {
    let host = url.host_str().ok_or("unsafe_url")?;
    let port = url.port_or_known_default().ok_or("unsafe_url")?;
    let addresses: Vec<_> = match url.host().ok_or("unsafe_url")? {
        url::Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Domain(_) => tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| "dns_timeout")?
            .map_err(|_| "dns_failed")?
            .take(33)
            .collect(),
    };
    if addresses.is_empty() || addresses.len() > 32 || addresses.iter().any(|a| !allowed(a.ip(), host, port, endpoints))
    {
        return Err("blocked_address");
    }
    let mut builder = isolated_builder().resolve_to_addrs(host, &addresses);
    if let Some(path) = ca_file {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "ca_unavailable")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1 << 20 {
            return Err("invalid_ca_file");
        }
        let file = std::fs::File::open(path).map_err(|_| "ca_unavailable")?;
        let mut bytes = Vec::new();
        file.take((1 << 20) + 1).read_to_end(&mut bytes).map_err(|_| "ca_unavailable")?;
        if bytes.len() > 1 << 20 {
            return Err("invalid_ca_file");
        }
        let certificates = reqwest::Certificate::from_pem_bundle(&bytes).map_err(|_| "invalid_ca_file")?;
        if certificates.is_empty() {
            return Err("invalid_ca_file");
        }
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder.build().map_err(|_| "transport_unavailable")
}
/// Disable ambient proxies, redirects, automatic retries, and decompression; bound transport time.
fn isolated_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .gzip(false)
        .brotli(false)
        .deflate(false)
        .zstd(false)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
}
/// Resolve current credentials and network policy, then attempt one sanitized webhook delivery.
pub async fn send(
    dir: &Path,
    endpoints: &[PrivateEndpoint],
    ca_file: Option<&Path>,
    root: &Path,
    d: &Destination,
    e: &Event,
    presentation: &Presentation,
) -> Outcome {
    let secret = match resolved_credentials(dir, root, d) {
        Ok(secret) => secret,
        Err(reason) => return Outcome::error(reason, false),
    };
    let client = match client(&secret.url, endpoints, ca_file).await {
        Ok(client) => client,
        Err(reason) => return Outcome::error(reason, matches!(reason, "dns_timeout" | "dns_failed")),
    };
    dispatch(client, secret, d, e, presentation).await
}
/// Send the platform payload and validate a bounded acknowledgement without exposing response bodies.
async fn dispatch(
    client: reqwest::Client,
    mut secret: Credentials,
    d: &Destination,
    e: &Event,
    presentation: &Presentation,
) -> Outcome {
    if d.format == Format::Discord {
        secret.url.query_pairs_mut().append_pair("wait", "true");
    }
    let payload = payload(d, e, presentation);
    let logos = logos(d, e, presentation);
    let mut request = client.post(secret.url);
    if logos.is_empty() {
        request = request.json(&payload);
    } else {
        let (content_type, body) = multipart_body(&payload, &logos);
        request = request.header(CONTENT_TYPE, content_type).body(body);
    }
    if let Some(bearer) = secret.bearer {
        request = request.header(AUTHORIZATION, bearer);
    }
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(_) => return Outcome::error("network_error", true),
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok().or_else(|| DateTimeParser::seconds(v)))
        .map(|s| s.clamp(1, 3600));
    if !(200..300).contains(&status) {
        return Outcome {
            success: false,
            retry: matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
            status: Some(status),
            reason: "http_rejected",
            retry_after,
        };
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > 65536 {
                    return Outcome { status: Some(status), ..Outcome::error("response_too_large", false) };
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Outcome { status: Some(status), ..Outcome::error("response_read_failed", true) },
        }
    }
    let acknowledged = match d.format {
        Format::Slack | Format::Mattermost => std::str::from_utf8(&bytes).is_ok_and(|s| s.trim() == "ok"),
        Format::Telegram => serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["ok"] == true),
        _ => true,
    };
    Outcome {
        success: acknowledged,
        retry: false,
        status: Some(status),
        reason: if acknowledged { "accepted" } else { "acknowledgement_failed" },
        retry_after: None,
    }
}
struct DateTimeParser;
impl DateTimeParser {
    /// Convert an HTTP-date Retry-After header into a delay relative to the current time.
    fn seconds(value: &str) -> Option<i64> {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|at| (at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds())
    }
}
#[cfg(test)]
#[path = "logo_tests.rs"]
mod logo_tests;
#[cfg(test)]
mod tests {
    use super::*;
    /// Create a deterministic credential-free event for payload tests.
    fn event() -> Event {
        Event {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            event: "quota.exhausted".into(),
            subscription: "0123456789abcdef01234567".into(),
            provider: "claude".into(),
            window: "5h".into(),
            model: None,
            observed_at: "2026-01-01T12:00:00Z".parse().unwrap(),
            resets_at: Some("2026-07-01T12:00:00Z".parse().unwrap()),
            used: Some(100.0),
            remaining_blockers: vec!["5h".into()],
        }
    }
    /// Construct a synthetic target format for acknowledgement tests.
    fn destination(format: Format) -> Destination {
        Destination { id: "test".into(), format, enabled: true, chat_id: Some("1234".into()) }
    }
    #[test]
    /// Verify that human times follow dst and generic keeps original machine identity and utc times.
    fn human_times_follow_dst_and_generic_keeps_original_machine_identity_and_utc_times() {
        let event = event();
        let presentation = Presentation::new(Some("My subscription"), chrono_tz::America::Denver);
        let human = text(&event, &presentation, Format::Telegram);
        assert!(human.contains("My subscription"));
        assert!(!human.contains(&event.subscription));
        assert!(human.contains("Detected: 2026-01-01 05:00:00"));
        assert!(human.contains("Estimated reset: 2026-07-01 06:00:00"));
        assert!(!human.contains("MST") && !human.contains("MDT") && !human.contains("America/Denver"));
        assert!(!human.contains("-07:00") && !human.contains("-06:00"));
        let generic = payload(&destination(Format::Generic), &event, &presentation);
        assert_eq!(generic["subscription"], event.subscription);
        assert_eq!(generic["subscription_display_name"], "My subscription");
        assert_eq!(generic["time_zone"], "America/Denver");
        assert_eq!(generic["observed_at_local"], "2026-01-01T05:00:00-07:00");
        assert_eq!(generic["resets_at_local"], "2026-07-01T06:00:00-06:00");
        assert_eq!(generic["observed_at"], json!(event.observed_at));
        assert_eq!(generic["resets_at"], json!(event.resets_at));
    }
    #[test]
    /// Verify that chat messages distinguish exhaustion partial recovery and full availability.
    fn chat_messages_distinguish_exhaustion_partial_recovery_and_full_availability() {
        let mut event = event();
        event.provider = "codex".into();
        let presentation = Presentation::new(Some("Work account"), chrono_tz::UTC);
        for (window, title) in [
            ("5h", "5-hour limit"),
            ("week", "Weekly limit"),
            ("week opus", "Weekly Opus limit"),
            ("week sonnet", "Weekly Sonnet limit"),
            ("day", "Daily limit"),
            ("unknown", "Subscription quota"),
        ] {
            event.window = window.into();
            let message = text(&event, &presentation, Format::Discord);
            assert!(message.starts_with(&format!("{title} exhausted\nChatGPT / Codex · Work account\n")));
            assert!(message.contains("Detected: 2026-01-01 12:00:00"));
            assert!(message.contains("Estimated reset: 2026-07-01 12:00:00"));
            assert!(!message.contains("quota.exhausted") && !message.contains("still exhausted"));
            assert!(!message.contains("confirmation required") && !message.contains("UTC"));
        }
        event.window = "5h".into();
        event.event = "quota.window_recovered".into();
        event.remaining_blockers = vec!["week".into(), "week opus".into()];
        let message = text(&event, &presentation, Format::Discord);
        assert!(message.starts_with("5-hour limit available again\nChatGPT / Codex · Work account\n"));
        assert!(message.contains("Confirmed available: 2026-01-01 12:00:00"));
        assert!(message.contains("Other limits still exhausted: Weekly limit, Weekly Opus limit"));
        assert!(!message.contains("Estimated reset"));
        event.event = "quota.available".into();
        event.window = "all".into();
        event.remaining_blockers.clear();
        let message = text(&event, &presentation, Format::Discord);
        assert!(message.starts_with("Subscription quota available again\n"));
        assert!(!message.contains("exhausted") && !message.contains("reset"));
        event.event = "quota.exhausted".into();
        event.window = "unknown".into();
        event.resets_at = None;
        assert!(text(&event, &presentation, Format::Discord).ends_with("Estimated reset: not provided"));
    }
    #[test]
    /// Verify that chat platforms preserve readable messages and safe payload contracts.
    fn chat_platforms_preserve_readable_messages_and_safe_payload_contracts() {
        let event = event();
        let presentation = Presentation::new(Some("Work account"), chrono_tz::UTC);
        for format in [Format::Discord, Format::Slack, Format::Mattermost, Format::Teams, Format::Telegram] {
            let value = payload(&destination(format), &event, &presentation);
            let message = match format {
                Format::Discord => {
                    assert_eq!(value["allowed_mentions"]["parse"], json!([]));
                    value["content"].as_str().unwrap()
                }
                Format::Teams => {
                    assert_eq!(value["type"], "message");
                    let attachment = &value["attachments"][0];
                    assert_eq!(attachment["contentType"], "application/vnd.microsoft.card.adaptive");
                    assert_eq!(attachment["content"]["type"], "AdaptiveCard");
                    assert_eq!(attachment["content"]["body"][0]["type"], "TextBlock");
                    assert_eq!(attachment["content"]["body"][0]["wrap"], true);
                    attachment["content"]["body"][0]["text"].as_str().unwrap()
                }
                Format::Slack => {
                    assert_eq!(value["mrkdwn"], false);
                    assert_eq!(value["link_names"], false);
                    assert_eq!(value["unfurl_links"], false);
                    value["text"].as_str().unwrap()
                }
                Format::Telegram => {
                    assert_eq!(value["chat_id"], "1234");
                    assert_eq!(value["disable_web_page_preview"], true);
                    assert!(value.get("parse_mode").is_none());
                    value["text"].as_str().unwrap()
                }
                _ => value["text"].as_str().unwrap(),
            };
            assert!(message.starts_with("5-hour limit exhausted\nClaude · Work account\nDetected: "));
            assert!(message.contains("\nEstimated reset: "));
        }
    }
    #[test]
    /// Verify that display names are bounded single line and cannot add mentions links or markdown.
    fn display_names_are_bounded_single_line_and_cannot_add_mentions_links_or_markdown() {
        let malicious = "\u{202e}@everyone [click](https://evil.example) <@U123>\r\n";
        let safe = safe_name(malicious);
        assert!(!safe.chars().any(char::is_control));
        assert!(!safe.contains('\u{202e}'));
        assert_eq!(safe_name(&"🦊".repeat(200)).chars().count(), 160);
        assert_eq!(safe_name("\r\n\u{202e}"), "Subscription");
        let presentation = Presentation::new(Some(malicious), chrono_tz::UTC);
        let event = event();
        for format in [Format::Discord, Format::Slack, Format::Mattermost, Format::Teams, Format::Telegram] {
            let value = payload(&destination(format), &event, &presentation);
            let text = match format {
                Format::Discord => value["content"].as_str().unwrap(),
                Format::Teams => value["attachments"][0]["content"]["body"][0]["text"].as_str().unwrap(),
                _ => value["text"].as_str().unwrap(),
            };
            assert!(!text.contains("@everyone"));
            assert!(!text.contains("https://evil.example"));
            assert!(!text.contains("<@U123>"));
            if matches!(format, Format::Discord | Format::Mattermost | Format::Teams) {
                assert!(!text.contains("[click]("));
            }
            if format == Format::Discord {
                assert_eq!(value["allowed_mentions"]["parse"], json!([]));
            }
            if format == Format::Slack {
                assert_eq!(value["mrkdwn"], false);
                assert_eq!(value["link_names"], false);
            }
            if format == Format::Telegram {
                assert!(value.get("parse_mode").is_none());
            }
        }
    }
    #[test]
    /// Verify that test messages have no fake subscription or provider even for legacy events.
    fn test_messages_have_no_fake_subscription_or_provider_even_for_legacy_events() {
        let mut event = event();
        event.event = "notification.test".into();
        event.subscription = "000000000000000000000000".into();
        let presentation = Presentation::new(None, chrono_tz::America::Denver);
        let text = text(&event, &presentation, Format::Discord);
        assert!(text.starts_with("Notification delivery test"));
        assert!(!text.contains("claude"));
        assert!(!text.contains(&event.subscription));
        let generic = payload(&destination(Format::Generic), &event, &presentation);
        assert_eq!(generic["message"], "Notification delivery test");
        assert!(generic.get("subscription").is_none());
        assert!(generic.get("provider").is_none());
        assert!(generic.get("subscription_display_name").is_none());
    }
    #[tokio::test]
    /// Verify that isolated transport refuses redirects bounds responses and checks acknowledgements.
    async fn isolated_transport_refuses_redirects_bounds_responses_and_checks_acknowledgements() {
        use axum::{
            Router,
            body::Body,
            http::{Response, StatusCode},
            routing::post,
        };
        use std::{
            future::IntoFuture,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let target_hits = hits.clone();
        let router = Router::new()
            .route(
                "/redirect",
                post(|| async {
                    Response::builder().status(307).header("location", "/target").body(Body::empty()).unwrap()
                }),
            )
            .route(
                "/target",
                post(move || {
                    let hits = target_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .route(
                "/limited",
                post(|| async {
                    Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("retry-after", "999999")
                        .body(Body::from("secret-body-sentinel"))
                        .unwrap()
                }),
            )
            .route("/large", post(|| async { vec![b'x'; 65537] }))
            .route("/slack", post(|| async { "ok" }))
            .route("/telegram", post(|| async { r#"{"ok":false,"description":"secret-body-sentinel"}"# }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(axum::serve(listener, router).into_future());
        // Internal mock transport only: production `send` always requires vetted HTTPS.
        let client = isolated_builder().https_only(false).build().unwrap();
        let event = Event {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            event: "quota.exhausted".into(),
            subscription: "0123456789abcdef01234567".into(),
            provider: "claude".into(),
            window: "5h".into(),
            model: None,
            observed_at: chrono::Utc::now(),
            resets_at: None,
            used: Some(100.0),
            remaining_blockers: vec!["5h".into()],
        };
        let mut destination = Destination { id: "test".into(), format: Format::Generic, enabled: true, chat_id: None };
        let credentials = |path: &str| Credentials {
            url: Url::parse(&format!("http://{address}/{path}?secret-url-sentinel")).unwrap(),
            bearer: None,
        };
        let redirect = dispatch(
            client.clone(),
            credentials("redirect"),
            &destination,
            &event,
            &Presentation::new(None, chrono_tz::UTC),
        )
        .await;
        assert_eq!(redirect.status, Some(307));
        assert!(!redirect.retry);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let limited = dispatch(
            client.clone(),
            credentials("limited"),
            &destination,
            &event,
            &Presentation::new(None, chrono_tz::UTC),
        )
        .await;
        assert_eq!(limited.status, Some(429));
        assert!(limited.retry);
        assert_eq!(limited.retry_after, Some(3600));
        assert_eq!(limited.reason, "http_rejected");
        let oversized = dispatch(
            client.clone(),
            credentials("large"),
            &destination,
            &event,
            &Presentation::new(None, chrono_tz::UTC),
        )
        .await;
        assert_eq!(oversized.reason, "response_too_large");
        assert!(!oversized.retry);
        destination.format = Format::Slack;
        assert!(
            dispatch(
                client.clone(),
                credentials("slack"),
                &destination,
                &event,
                &Presentation::new(None, chrono_tz::UTC)
            )
            .await
            .success
        );
        destination.format = Format::Telegram;
        let refused =
            dispatch(client, credentials("telegram"), &destination, &event, &Presentation::new(None, chrono_tz::UTC))
                .await;
        assert_eq!(refused.reason, "acknowledgement_failed");
        assert!(!refused.success);
        server.abort();
        let _ = server.await;
    }
    #[test]
    /// Verify that special addresses and mapped ipv6 cannot bypass filter.
    fn special_addresses_and_mapped_ipv6_cannot_bypass_filter() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.1.2.3",
            "100.64.1.2",
            "192.0.2.1",
            "198.18.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }
    #[test]
    /// Verify that private allowlist is exact host port and subnet.
    fn private_allowlist_is_exact_host_port_and_subnet() {
        let allow = vec![PrivateEndpoint { host: "chat.example".into(), port: 443, cidrs: vec!["10.1.0.0/16".into()] }];
        assert!(allowed("10.1.2.3".parse().unwrap(), "chat.example", 443, &allow));
        for (ip, host, port) in [
            ("10.2.2.3", "chat.example", 443),
            ("10.1.2.3", "other.example", 443),
            ("10.1.2.3", "chat.example", 8443),
            ("127.0.0.1", "chat.example", 443),
            ("169.254.169.254", "chat.example", 443),
        ] {
            assert!(!allowed(ip.parse().unwrap(), host, port, &allow));
        }
    }
}
