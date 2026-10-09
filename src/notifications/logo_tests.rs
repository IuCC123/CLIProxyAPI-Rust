use super::*;
use axum::{
    Router,
    extract::{Multipart, OriginalUri, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use parking_lot::Mutex;
use std::{net::SocketAddr, sync::Arc};

/// Create a synthetic Discord destination without webhook credentials.
fn discord() -> Destination {
    Destination { id: "ops".into(), format: Format::Discord, enabled: true, chat_id: None }
}

/// Create a credential-free event fixture with deterministic timestamps and scopes.
fn sample_event(provider: &str, event: &str) -> Event {
    Event {
        version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        event: event.into(),
        subscription: "0123456789abcdef01234567".into(),
        provider: provider.into(),
        window: if event == "notification.test" { "test" } else { "5h" }.into(),
        model: None,
        observed_at: chrono::DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z").unwrap().to_utc(),
        resets_at: None,
        used: (event != "notification.test").then_some(100.0),
        remaining_blockers: Vec::new(),
    }
}

/// Build a human presentation fixture without modifying the durable event.
fn presentation() -> Presentation {
    Presentation::new(Some("Synthetic subscription"), chrono_tz::UTC)
}

/// Read the attachment filenames declared by a synthetic Discord payload.
fn attachment_names(payload: &Value) -> Vec<&str> {
    payload["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|attachment| attachment["filename"].as_str().unwrap())
        .collect()
}

#[test]
/// Verify that discord provider quota events attach one logo without changing message content.
fn discord_provider_quota_events_attach_one_logo_without_changing_message_content() {
    let destination = discord();
    let logos = presentation();
    let plain = Presentation::new(Some("Synthetic subscription"), chrono_tz::UTC).with_provider_logos(false);

    for (provider, filename) in [("claude", "claude.png"), ("codex", "codex.png")] {
        let event = sample_event(provider, "quota.exhausted");
        let decorated = payload(&destination, &event, &logos);
        let undecorated = payload(&destination, &event, &plain);
        assert_eq!(decorated["content"], undecorated["content"]);
        assert_eq!(decorated["allowed_mentions"], json!({"parse": []}));
        assert_eq!(decorated["embeds"][0]["thumbnail"]["url"], format!("attachment://{filename}"));
        assert_eq!(attachment_names(&decorated), vec![filename]);
        assert_eq!(
            undecorated,
            json!({
                "content": decorated["content"],
                "allowed_mentions": {"parse": []}
            })
        );
    }
}

#[test]
/// Verify that test notifications preview both logos unknown providers do not get a logo and other formats are unchanged.
fn test_notifications_preview_both_logos_unknown_providers_do_not_get_a_logo_and_other_formats_are_unchanged() {
    let logos = presentation();
    let plain = Presentation::new(Some("Synthetic subscription"), chrono_tz::UTC).with_provider_logos(false);
    let destination = discord();

    let preview = payload(&destination, &sample_event("", "notification.test"), &logos);
    assert_eq!(attachment_names(&preview), vec!["claude.png", "codex.png"]);
    assert_eq!(preview["embeds"].as_array().unwrap().len(), 2);
    assert!(preview["content"].as_str().unwrap().contains("Notification delivery test"));

    let unknown = sample_event("custom-provider", "quota.exhausted");
    let unknown_payload = payload(&destination, &unknown, &logos);
    assert_eq!(unknown_payload, payload(&destination, &unknown, &plain));
    assert!(unknown_payload.get("embeds").is_none());
    assert!(unknown_payload.get("attachments").is_none());

    for format in [Format::Generic, Format::Slack, Format::Mattermost, Format::Teams, Format::Telegram] {
        let destination = Destination {
            id: "ops".into(),
            format,
            enabled: true,
            chat_id: (format == Format::Telegram).then(|| "chat123".into()),
        };
        for event in [sample_event("claude", "quota.exhausted"), sample_event("", "notification.test")] {
            assert_eq!(payload(&destination, &event, &logos), payload(&destination, &event, &plain));
        }
    }
}

#[test]
/// Verify that provider logos are preview attachments only when enabled.
fn provider_logos_are_preview_attachments_only_when_enabled() {
    let destination = Destination { format: Format::Generic, ..discord() };
    let event = sample_event("claude", "quota.exhausted");
    let with_logos = payload(&destination, &event, &presentation());
    let without_logos = payload(
        &destination,
        &event,
        &Presentation::new(Some("Synthetic subscription"), chrono_tz::UTC).with_provider_logos(false),
    );
    assert_eq!(with_logos, without_logos);

    let destination = discord();
    let with_logos = payload(&destination, &event, &presentation());
    let without_logos = payload(
        &destination,
        &event,
        &Presentation::new(Some("Synthetic subscription"), chrono_tz::UTC).with_provider_logos(false),
    );
    assert!(with_logos["attachments"].is_array());
    assert_eq!(without_logos["content"], with_logos["content"]);
    assert!(without_logos.get("embeds").is_none());
    assert!(without_logos.get("attachments").is_none());
}

#[test]
/// Verify that provider logos config defaults on and accepts explicit opt out.
fn provider_logos_config_defaults_on_and_accepts_explicit_opt_out() {
    assert!(crate::notifications::Config::default().provider_logos);
    assert!(crate::config::Config::parse("").unwrap().notifications.provider_logos);
    assert!(
        !crate::config::Config::parse("notifications:\n  provider-logos: false\n")
            .unwrap()
            .notifications
            .provider_logos
    );
}

#[derive(Clone, Debug)]
struct Part {
    name: Option<String>,
    filename: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Received {
    uri: String,
    authorization: Option<String>,
    parts: Vec<Part>,
}

/// Capture one loopback multipart request to verify the actual attachment bytes and metadata.
async fn receive_discord(
    State(received): State<Arc<Mutex<Option<Received>>>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    mut form: Multipart,
) -> StatusCode {
    let mut parts = Vec::new();
    while let Some(field) = form.next_field().await.unwrap() {
        parts.push(Part {
            name: field.name().map(str::to_owned),
            filename: field.file_name().map(str::to_owned),
            content_type: field.content_type().map(str::to_owned),
            bytes: field.bytes().await.unwrap().to_vec(),
        });
    }
    *received.lock() = Some(Received {
        uri: uri.to_string(),
        authorization: headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned),
        parts,
    });
    StatusCode::NO_CONTENT
}

struct TestServer(tokio::task::JoinHandle<()>);

impl Drop for TestServer {
    /// Release the test task or remove its temporary files when the fixture leaves scope.
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
/// Verify that discord multipart matches payload metadata and bundled png bytes.
async fn discord_multipart_matches_payload_metadata_and_bundled_png_bytes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let received = Arc::new(Mutex::new(None));
    let router = Router::new().route("/webhook", post(receive_discord)).with_state(received.clone());
    let server = TestServer(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));

    let destination = discord();
    let event = sample_event("claude", "quota.exhausted");
    let credential = Credentials { url: Url::parse(&format!("http://{address}/webhook")).unwrap(), bearer: None };
    let outcome = dispatch(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        credential,
        &destination,
        &event,
        &presentation(),
    )
    .await;
    assert!(outcome.success, "{}", outcome.reason);
    assert_eq!(outcome.status, Some(StatusCode::NO_CONTENT.as_u16()));

    let got = received.lock().clone().expect("mock endpoint should receive request");
    assert!(got.uri.contains("wait=true"), "{}", got.uri);
    assert!(got.authorization.is_none());
    let payload_part = got.parts.iter().find(|part| part.name.as_deref() == Some("payload_json")).unwrap();
    let payload: Value = serde_json::from_slice(&payload_part.bytes).unwrap();
    assert_eq!(payload["allowed_mentions"], json!({"parse": []}));
    assert_eq!(payload["embeds"][0]["title"], "Claude");
    assert_eq!(payload["embeds"][0]["color"], CLAUDE_LOGO.color);
    let file = got.parts.iter().find(|part| part.filename.as_deref() == Some("claude.png")).unwrap();
    assert_eq!(file.name.as_deref(), Some("files[0]"));
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
    assert_eq!(file.bytes, CLAUDE_LOGO.bytes);
    assert_eq!(file.bytes, include_bytes!("assets/claude.png"));
    assert!(file.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(payload["embeds"][0]["thumbnail"]["url"], "attachment://claude.png");
    assert_eq!(attachment_names(&payload), vec!["claude.png"]);
    assert_eq!(payload["attachments"][0]["id"], 0);
    drop(server);
}
