use super::*;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::Config;

struct Fixture {
    dir: PathBuf,
    app: Arc<App>,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cliproxy-body-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config { auth_dir: dir.to_string_lossy().into(), ..Default::default() };
        let app = App::new(cfg, dir.join("config.yaml"));
        Self { dir, app }
    }

    fn assert_rejection(&self, status: u16) {
        let recent = self.app.stats.recent.lock();
        assert_eq!(recent.len(), 1);
        let log = &recent[0];
        assert_eq!(log.status, status);
        assert_eq!(log.attempts, 0);
        assert!(log.account.is_empty());
        assert!(log.error.is_some());
        assert_eq!(log.output_tokens, 0);
        assert_eq!(self.app.stats.active.load(Ordering::Relaxed), 0);
        let totals = self.app.stats.totals.lock();
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.failed, 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn encode(encoding: &str, body: &[u8]) -> Vec<u8> {
    match encoding {
        "zstd" => zstd::stream::encode_all(body, 3).unwrap(),
        "gzip" => {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(body).unwrap();
            encoder.finish().unwrap()
        }
        _ => body.to_vec(),
    }
}

async fn request(app: Arc<App>, path: &str, encoding: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len());
    if let Some(encoding) = encoding {
        req = req.header(header::CONTENT_ENCODING, encoding);
    }
    let response = crate::server::router(app).oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn compressed_json_reaches_all_json_routes() {
    for path in [
        "/v1/responses",
        "/backend-api/codex/responses",
        "/v1/responses/compact",
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/completions",
    ] {
        for encoding in [None, Some("identity"), Some("zstd"), Some("gzip"), Some(" ZsTd ")] {
            let fixture = Fixture::new();
            let body = encode(encoding.unwrap_or("identity").trim().to_ascii_lowercase().as_str(), b"{}");
            let (status, response) = request(fixture.app.clone(), path, encoding, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path} {encoding:?}: {response}");
            assert_eq!(response["error"]["message"], "`model` is required", "{path} {encoding:?}");
        }
    }
}

#[tokio::test]
async fn corrupt_or_truncated_compression_is_logged_without_forwarding() {
    for encoding in ["gzip", "zstd"] {
        for body in [b"not compressed".to_vec(), {
            let mut body = encode(encoding, b"{}");
            body.pop();
            body
        }] {
            let fixture = Fixture::new();
            let (status, response) = request(fixture.app.clone(), "/v1/responses", Some(encoding), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["error"]["message"], "invalid or incomplete request body encoding");
            fixture.assert_rejection(400);
        }
    }
}

#[tokio::test]
async fn invalid_json_objects_remain_rejected_and_are_logged() {
    for encoding in ["identity", "zstd", "gzip"] {
        for body in [b"[]".as_slice(), b"null", b"\"text\"", b"{"] {
            let fixture = Fixture::new();
            let (status, response) =
                request(fixture.app.clone(), "/v1/responses", Some(encoding), encode(encoding, body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["error"]["message"], "request body must be a JSON object");
            fixture.assert_rejection(400);
        }
    }
}

#[tokio::test]
async fn unsupported_or_stacked_encodings_are_rejected_and_logged() {
    for encoding in ["br", "deflate", "gzip, zstd", "", "identity, gzip"] {
        let fixture = Fixture::new();
        let (status, response) = request(fixture.app.clone(), "/v1/responses", Some(encoding), b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(response["error"]["message"], "unsupported Content-Encoding");
        fixture.assert_rejection(415);
    }
    let fixture = Fixture::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::CONTENT_ENCODING, "gzip")
        .header(header::CONTENT_ENCODING, "zstd")
        .body(Body::from(b"{}".as_slice()))
        .unwrap();
    let response = crate::server::router(fixture.app.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    fixture.assert_rejection(415);
}

#[tokio::test]
async fn authentication_runs_before_body_decoding() {
    let fixture = Fixture::new();
    let mut cfg = (*fixture.app.cfg()).clone();
    cfg.api_keys = vec!["local-test-key".into()];
    fixture.app.set_config(cfg);
    let (status, response) = request(fixture.app.clone(), "/v1/responses", Some("zstd"), b"corrupt".to_vec()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response["error"]["message"], "invalid or missing API key");
    assert!(fixture.app.stats.recent.lock().is_empty());
}

async fn bounded_probe(req: Request) -> (StatusCode, String) {
    match read_body(req.into_body(), 64).await {
        Ok(body) => (StatusCode::OK, String::from_utf8(body.to_vec()).unwrap()),
        Err((status, message)) => (StatusCode::from_u16(status).unwrap(), message.into()),
    }
}

#[tokio::test]
async fn both_encoded_and_decoded_streams_have_size_limits() {
    let router = Router::new().route("/", post(bounded_probe));
    for encoding in ["gzip", "zstd"] {
        // The compressed representation fits in 64 bytes, but its output does not.
        let large = encode(encoding, &[b'a'; 4096]);
        assert!(large.len() < 64);
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header(header::CONTENT_ENCODING, encoding)
            .body(Body::from(large))
            .unwrap();
        let response = router.clone().oneshot(decode_body(limit_encoded_body(req, 64))).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // The raw stream hits its bound before decompression, even without Content-Length.
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header(header::CONTENT_ENCODING, encoding)
            .body(Body::from(encode(encoding, b"{}")))
            .unwrap();
        let response = router.clone().oneshot(decode_body(limit_encoded_body(req, 1))).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
    let req = Request::builder().method("POST").uri("/").body(Body::from(vec![b'a'; 64])).unwrap();
    assert_eq!(router.oneshot(decode_body(limit_encoded_body(req, 64))).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn decoded_json_is_forwarded_without_stale_encoding_headers() {
    let upstream = Router::new().route("/responses", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
        assert!(!headers.contains_key(header::CONTENT_ENCODING));
        assert_eq!(body["model"], "gpt-local-test");
        assert_eq!(body["input"][0]["content"][0]["text"], "local synthetic input");
        assert_eq!(headers[header::CONTENT_LENGTH].to_str().unwrap().parse::<usize>().unwrap(), body.to_string().len());
        let response = json!({"id":"resp_local", "object":"response", "status":"completed", "model":"gpt-local-test", "output":[], "usage":{"input_tokens":1,"output_tokens":0}});
        ([(header::CONTENT_TYPE, "text/event-stream")], format!("data: {}\n\ndata: {}\n\n", json!({"type":"response.created", "response":response}), json!({"type":"response.completed", "response":response})))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let fixture = Fixture::new();
    // Fake subscription credentials point only at the local provider, never OpenAI.
    std::fs::write(fixture.dir.join("codex.json"), json!({"type":"codex","access_token":"local-test-token","email":"local-test","base_url":format!("http://{addr}")}).to_string()).unwrap();
    fixture.app.reload_accounts();
    for encoding in ["identity", "gzip", "zstd"] {
        let body = serde_json::to_vec(&json!({"model":"gpt-local-test","input":"local synthetic input"})).unwrap();
        let (status, response) =
            request(fixture.app.clone(), "/v1/responses", Some(encoding), encode(encoding, &body)).await;
        assert_eq!(status, StatusCode::OK, "{encoding}: {response}");
        assert_eq!(response["id"], "resp_local");
    }
    assert_eq!(fixture.app.stats.totals.lock().requests, 3);
    assert_eq!(fixture.app.stats.totals.lock().failed, 0);
    task.abort();
}

#[tokio::test]
async fn concatenated_members_are_decoded_completely() {
    for encoding in ["gzip", "zstd"] {
        let fixture = Fixture::new();
        let mut body = encode(encoding, b"{");
        body.extend(encode(encoding, b"}"));
        let (status, response) = request(fixture.app.clone(), "/v1/responses", Some(encoding), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"]["message"], "`model` is required");
    }
}

#[tokio::test]
async fn trailing_corruption_or_truncated_next_member_is_rejected() {
    for encoding in ["gzip", "zstd"] {
        for tail in [b"trailing garbage".to_vec(), {
            let mut member = encode(encoding, b"{}");
            member.pop();
            member
        }] {
            let fixture = Fixture::new();
            let mut body = encode(encoding, b"{}");
            body.extend(tail);
            let (status, response) = request(fixture.app.clone(), "/v1/responses", Some(encoding), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["error"]["message"], "invalid or incomplete request body encoding");
            fixture.assert_rejection(400);
        }
    }
}

#[tokio::test]
async fn get_routes_do_not_read_or_decode_request_bodies() {
    let fixture = Fixture::new();
    let body = Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header(header::CONTENT_ENCODING, "unsupported")
        .body(body)
        .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crate::server::router(fixture.app.clone()).oneshot(req),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(fixture.app.stats.recent.lock().is_empty());
}
