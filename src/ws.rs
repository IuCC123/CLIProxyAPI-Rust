//! Responses API over websocket (`GET /v1/responses`), the transport Codex uses.
//!
//! Each `response.create` message is one turn. Codex OAuth accounts get a
//! native upstream websocket (server-side `previous_response_id` works as-is);
//! every other provider is served through the normal pipeline, with
//! `previous_response_id` expanded from a small local history.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures::{FutureExt, SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;

use crate::accounts::{Account, Provider};
use crate::formats::{StreamParser, responses};
use crate::ir::{self, Event, Format, Usage};
use crate::proxy::{self, Call, Reply, Tracker};
use crate::sse::SseEvent;
use crate::state::App;

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type ClientTx = futures::stream::SplitSink<WebSocket, Message>;

const UPSTREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// A turn upload also gets time for a slow (about 256 kbit/s) uplink, so a long
/// conversation isn't cut off while it is still moving.
const SLOW_UPLINK_BYTES_PER_SEC: usize = 32 * 1024;
const MAX_IDLE_DRAIN: usize = 32;

struct UpstreamFailure {
    operation: &'static str,
    status: u16,
    message: String,
}

impl UpstreamFailure {
    fn timeout(operation: &'static str) -> Self {
        Self { operation, status: 504, message: format!("codex websocket {operation} timed out") }
    }

    fn socket(operation: &'static str, error: &tungstenite::Error) -> Self {
        Self { operation, status: 502, message: format!("codex websocket {operation} failed: {}", socket_error(error)) }
    }

    fn log(&self, connection_id: &str, phase: &'static str) {
        tracing::warn!(connection_id, phase, operation = self.operation, status = self.status, error = %self.message,
            "codex upstream websocket failure");
    }
}

// Some errors contain raw frames, HTTP bodies, or URLs. Keep transport diagnostics safe.
fn socket_error(error: &tungstenite::Error) -> String {
    match error {
        tungstenite::Error::Io(error) => format!("I/O {:?}, OS error {:?}", error.kind(), error.raw_os_error()),
        tungstenite::Error::Protocol(error) => format!("protocol error: {error}"),
        tungstenite::Error::Http(response) => format!("handshake rejected: {}", response.status()),
        tungstenite::Error::ConnectionClosed => "connection closed".into(),
        tungstenite::Error::AlreadyClosed => "connection already closed".into(),
        tungstenite::Error::Tls(_) => "TLS error".into(),
        tungstenite::Error::Capacity(_) => "capacity exceeded".into(),
        tungstenite::Error::WriteBufferFull(_) => "write buffer full".into(),
        tungstenite::Error::Utf8(_) => "invalid UTF-8".into(),
        tungstenite::Error::AttackAttempt => "attack attempt detected".into(),
        tungstenite::Error::Url(_) => "invalid websocket URL".into(),
        tungstenite::Error::HttpFormat(_) => "invalid HTTP format".into(),
    }
}

async fn upstream_write(
    operation: &'static str,
    write: impl Future<Output = Result<(), tungstenite::Error>>,
) -> Result<(), UpstreamFailure> {
    upstream_write_within(UPSTREAM_WRITE_TIMEOUT, operation, write).await
}

fn send_deadline(len: usize) -> Duration {
    UPSTREAM_WRITE_TIMEOUT + Duration::from_secs((len / SLOW_UPLINK_BYTES_PER_SEC) as u64)
}

async fn upstream_write_within(
    deadline: Duration,
    operation: &'static str,
    write: impl Future<Output = Result<(), tungstenite::Error>>,
) -> Result<(), UpstreamFailure> {
    tokio::time::timeout(deadline, write)
        .await
        .map_err(|_| UpstreamFailure::timeout(operation))?
        .map_err(|error| UpstreamFailure::socket(operation, &error))
}

async fn close_upstream(up: &mut Upstream, connection_id: &str, phase: &'static str) {
    if let Err(failure) = upstream_write("close", up.close(None)).await {
        failure.log(connection_id, phase);
    }
}

struct Session {
    upstream: Option<(Arc<Account>, Upstream)>,
    /// Response ids known to the current upstream socket.
    upstream_responses: HashSet<String>,
    /// Epoch of the last turn, so late quota events cannot undo a newer quota refresh.
    upstream_quota_epoch: u64,
    key: Option<String>,
    source: Option<&'static str>,
    pending_selection: Option<crate::affinity::Selected>,
    connection_id: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            upstream: None,
            upstream_responses: HashSet::new(),
            upstream_quota_epoch: 0,
            key: None,
            source: None,
            pending_selection: None,
            connection_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

impl Session {
    fn discard_upstream(&mut self) -> Option<(Arc<Account>, Upstream)> {
        self.upstream_responses.clear();
        self.upstream_quota_epoch = 0;
        self.upstream.take()
    }

    async fn close_upstream(&mut self, phase: &'static str) {
        if let Some((_, mut up)) = self.discard_upstream() {
            close_upstream(&mut up, &self.connection_id, phase).await;
        }
    }
}

async fn idle_upstream(sess: &mut Session, event: Option<Result<tungstenite::Message, tungstenite::Error>>) {
    match event {
        Some(Ok(tungstenite::Message::Ping(_))) => {
            // Tungstenite queues its automatic Pong until the next write/flush.
            let (_, up) = sess.upstream.as_mut().unwrap();
            if let Err(failure) = upstream_write("flush", up.flush()).await {
                failure.log(&sess.connection_id, "idle");
                sess.discard_upstream();
            }
        }
        Some(Ok(tungstenite::Message::Close(frame))) => {
            tracing::debug!(
                connection_id = sess.connection_id,
                phase = "idle",
                close_code = frame.as_ref().map(|frame| u16::from(frame.code)),
                "codex upstream websocket closed"
            );
            if let Some((_, mut up)) = sess.discard_upstream()
                && let Err(failure) = upstream_write("flush", up.flush()).await
            {
                failure.log(&sess.connection_id, "idle_close");
            }
        }
        Some(Err(error)) => {
            UpstreamFailure::socket("read", &error).log(&sess.connection_id, "idle");
            sess.discard_upstream();
        }
        None => {
            tracing::debug!(connection_id = sess.connection_id, phase = "idle", "codex upstream websocket ended");
            sess.discard_upstream();
        }
        Some(Ok(tungstenite::Message::Text(text))) => idle_data(sess, &text),
        Some(Ok(tungstenite::Message::Binary(data))) => idle_data(sess, &String::from_utf8_lossy(&data)),
        Some(Ok(_)) => {}
    }
}

fn idle_data(sess: &mut Session, data: &str) {
    if let Ok(value) = serde_json::from_str::<Value>(data)
        && value["type"] == "codex.rate_limits"
    {
        if let Some((acct, _)) = &sess.upstream {
            crate::quota::observe_codex_event(acct, &value, sess.upstream_quota_epoch);
        }
    } else {
        tracing::warn!(
            connection_id = sess.connection_id,
            phase = "idle",
            "unexpected codex upstream application frame between turns"
        );
        sess.discard_upstream();
    }
}

async fn drain_idle_upstream(sess: &mut Session) {
    for _ in 0..MAX_IDLE_DRAIN {
        let Some((_, up)) = &mut sess.upstream else { return };
        // The finite drain supplies its own budget; a cooperative yield is not an empty socket.
        let Some(event) = tokio::task::unconstrained(up.next()).now_or_never() else { return };
        idle_upstream(sess, event).await;
    }
    // A busy upstream must not prevent a ready client from submitting or closing.
    tracing::warn!(
        connection_id = sess.connection_id,
        phase = "idle",
        "codex upstream idle drain limit reached; reconnecting on the next turn"
    );
    sess.discard_upstream();
}

struct ClientGone;

async fn send(tx: &mut ClientTx, text: String) -> Result<(), ClientGone> {
    tx.send(Message::Text(text.into())).await.map_err(|_| ClientGone)
}

fn error_event(status: u16, body: &Value) -> String {
    let err = if body["error"].is_object() {
        body["error"].clone()
    } else {
        json!({ "message": proxy::error_message(&body.to_string()) })
    };
    json!({ "type": "error", "status": status, "error": err }).to_string()
}

fn input_items(body: &Value) -> Vec<Value> {
    crate::affinity::input_items(body)
}

pub async fn handle(app: Arc<App>, headers: HeaderMap, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut sess = Session::default();
    loop {
        let msg = tokio::select! {
            // The client gets priority; queued upstream frames are drained before submission.
            biased;
            msg = rx.next() => {
                let Some(msg) = msg else { break };
                msg
            }
            upstream = async {
                match &mut sess.upstream {
                    Some((_, up)) => up.next().await,
                    None => std::future::pending().await,
                }
            } => {
                idle_upstream(&mut sess, upstream).await;
                continue;
            }
        };
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(mut body) = serde_json::from_str::<Value>(&text) else {
            if send(&mut tx, error_event(400, &json!({ "error": { "message": "invalid JSON" } }))).await.is_err() {
                break;
            }
            continue;
        };
        if body["type"] != "response.create" {
            let msg = format!("unsupported message type `{}`", body["type"].as_str().unwrap_or_default());
            if send(&mut tx, error_event(400, &json!({ "error": { "message": msg, "type": "invalid_request_error" } })))
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        if let Some(o) = body.as_object_mut() {
            o.remove("type");
        }
        drain_idle_upstream(&mut sess).await;
        if turn(&app, &headers, &mut sess, body, &mut tx).await.is_err() {
            break;
        }
    }
    sess.close_upstream("client_closed").await;
}

async fn turn(
    app: &Arc<App>,
    headers: &HeaderMap,
    sess: &mut Session,
    mut body: Value,
    tx: &mut ClientTx,
) -> Result<(), ClientGone> {
    sess.pending_selection = None;
    // Full conversation for local history (and for providers without server state).
    let prev = body["previous_response_id"].as_str().map(String::from);
    let requested = crate::affinity::session_identity(headers, &body);
    let previous =
        prev.as_deref().and_then(|id| app.sessions.previous(headers, id, requested.as_ref().map(|s| s.key.as_str())));
    let (key, source) = if let Some(identity) = requested {
        (identity.key, identity.source)
    } else if let Some((key, _)) = &previous {
        (key.clone(), "previous_response_id")
    } else if let Some(key) = &sess.key {
        (key.clone(), "websocket_connection")
    } else {
        (crate::affinity::connection_key(headers, &sess.connection_id), "websocket_connection")
    };
    if sess.key.as_ref().is_some_and(|old| old != &key) {
        sess.close_upstream("session_changed").await;
    }
    sess.key = Some(key);
    sess.source = Some(source);
    let _lease = app.sessions.hold(sess.key.as_deref().unwrap(), app.cfg().session_affinity_idle_seconds);
    let mut full = if prev.is_some() { previous.map(|(_, items)| items) } else { Some(vec![]) };
    if let Some(items) = &mut full {
        items.extend(input_items(&body));
    }

    let cfg = app.cfg();
    if cfg.codex_websockets {
        match native_turn(app, headers, sess, &body, full.as_deref(), tx).await {
            Native::Done => {
                sess.pending_selection = None;
                if headers.get("x-cliproxy-session-end").is_some_and(|v| v == "true") {
                    app.sessions.end(sess.key.as_deref().unwrap());
                }
                return Ok(());
            }
            Native::Gone => {
                sess.pending_selection = None;
                return Err(ClientGone);
            }
            Native::Fallback => {}
        }
    }

    if prev.is_some() {
        if full.is_none() {
            let err = json!({ "error": {
                "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
                "type": "invalid_request_error", "code": "previous_response_not_found", "param": "previous_response_id"
            }});
            return send(tx, error_event(400, &err)).await;
        }
        body["input"] = Value::Array(full.clone().unwrap());
        body.as_object_mut().unwrap().remove("previous_response_id");
    }

    let call = Call {
        format: Format::Responses,
        body,
        headers: headers.clone(),
        stream: true,
        transport: "ws",
        path_model: None,
        session: sess.key.clone(),
        session_source: sess.source,
        routing_selection: sess.pending_selection.take(),
    };
    match proxy::execute(app.clone(), call).await {
        Reply::Stream { mut frames, .. } => {
            while let Some(f) = frames.next().await {
                send(tx, f.data).await?;
            }
            Ok(())
        }
        Reply::Json(v) => send(tx, json!({ "type": "response.completed", "response": v }).to_string()).await,
        Reply::Error(status, body) => send(tx, error_event(status, &body)).await,
    }
}

fn capture(
    app: &App,
    headers: &HeaderMap,
    sess: &mut Session,
    data: &str,
    full: Option<&[Value]>,
    aggregate: &ir::Aggregate,
) {
    let Ok(v) = serde_json::from_str::<Value>(data) else { return };
    let r = &v["response"];
    let Some(id) = r["id"].as_str() else { return };
    // Only the most recent responses need connection-local continuation state.
    if sess.upstream_responses.len() >= 100 {
        sess.upstream_responses.clear();
    }
    sess.upstream_responses.insert(id.to_string());
    if let (Some(key), Some(full)) = (&sess.key, full) {
        let mut response = r.clone();
        crate::affinity::complete_output(&mut response, aggregate);
        app.sessions.remember(headers, key, &response, full);
    }
}

enum Native {
    Done,
    Gone,
    Fallback,
}

async fn connect(acct: &Arc<Account>, client_headers: &HeaderMap) -> Result<Upstream, String> {
    let (url, headers) = crate::upstream::codex_ws_url(acct, client_headers);
    let mut req =
        tungstenite::client::IntoClientRequest::into_client_request(url.as_str()).map_err(|e| socket_error(&e))?;
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) =
            (tungstenite::http::HeaderName::from_bytes(k.as_bytes()), tungstenite::http::HeaderValue::from_str(&v))
        {
            req.headers_mut().insert(name, val);
        }
    }
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "websocket handshake timed out".to_string())?
        .map_err(|e| socket_error(&e))?;
    Ok(ws)
}

/// Relay one native turn while binding streamed quota and rejection evidence to its request epoch.
async fn native_turn(
    app: &Arc<App>,
    headers: &HeaderMap,
    sess: &mut Session,
    body: &Value,
    full: Option<&[Value]>,
    tx: &mut ClientTx,
) -> Native {
    let cfg = app.cfg();
    // Native websockets don't go through HTTP proxies.
    if !cfg.proxy_url.is_empty() {
        return Native::Fallback;
    }
    let (model, suffix) = ir::split_model_suffix(body["model"].as_str().unwrap_or_default());
    if model.is_empty() {
        return Native::Fallback;
    }

    let (only, model) = app.pool.route(&model);
    let model = app.pool.canonical(&model, only.as_ref());
    if only.as_ref().is_some_and(|o| *o != crate::accounts::Only::Provider(Provider::Codex)) {
        return Native::Fallback;
    }

    // Refresh the shared assignment every turn, including on an existing socket.
    let selected = match app.sessions.pick_with_reason(&app.pool, &cfg, &model, sess.key.as_deref(), &[], only.as_ref())
    {
        Ok(pair) => pair,
        Err((status, message)) => {
            let result = send(tx, error_event(status, &json!({"error":{"message":message}}))).await;
            return if result.is_ok() { Native::Done } else { Native::Gone };
        }
    };
    sess.pending_selection = Some(selected.clone());
    let acct = selected.account.clone();
    let upstream_model = selected.model.clone();
    if acct.provider != Provider::Codex || !acct.is_oauth() || acct.proxy_url.is_some() {
        return Native::Fallback;
    }
    let reuse = sess.upstream.as_ref().is_some_and(|(a, _)| a.id == acct.id);
    if !reuse {
        sess.close_upstream("account_changed").await;
        if crate::oauth::ensure_fresh(app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
            return Native::Fallback;
        }
        let mut upstream_headers = headers.clone();
        if !upstream_headers.contains_key("session_id")
            && !upstream_headers.contains_key("session-id")
            && let Some(key) = body["prompt_cache_key"].as_str()
            && let Ok(value) = key.parse()
        {
            upstream_headers.insert("session_id", value);
        }
        match connect(&acct, &upstream_headers).await {
            Ok(ws) => {
                sess.upstream_quota_epoch = acct.quota_epoch();
                sess.upstream = Some((acct, ws));
            }
            Err(e) => {
                tracing::warn!(connection_id = sess.connection_id, phase = "handshake", account = %acct.label,
                    error = %e, "codex websocket unavailable, using HTTP");
                return Native::Fallback;
            }
        }
    }
    let (acct, mut up) = sess.upstream.take().unwrap();
    let mut payload = body.clone();
    if let Some(prev) = body["previous_response_id"].as_str()
        && !sess.upstream_responses.contains(prev)
    {
        let Some(full) = full else {
            sess.upstream = Some((acct, up));
            let err = json!({"error":{"message":"Previous response is unavailable on this connection; resend full conversation input without previous_response_id", "code":"previous_response_not_found"}});
            return if send(tx, error_event(400, &err)).await.is_ok() { Native::Done } else { Native::Gone };
        };
        payload["input"] = Value::Array(full.to_vec());
        payload.as_object_mut().unwrap().remove("previous_response_id");
    }
    crate::upstream::sanitize_codex_body(&mut payload, &upstream_model, true);
    if let Some(r) = &suffix
        && let Some(e) = r.effort_level()
    {
        payload["reasoning"]["effort"] = e.into();
    }
    payload["type"] = "response.create".into();

    let quota_epoch = acct.quota_epoch();
    sess.upstream_quota_epoch = quota_epoch;
    let mut tracker = Tracker::new(app, Format::Responses, true, "ws", &model);
    tracker.session(sess.key.as_deref(), sess.source, &cfg);
    tracker.selected(&selected);
    tracker.request_epoch(quota_epoch);
    let payload = payload.to_string();
    let deadline = send_deadline(payload.len());
    if let Err(failure) =
        upstream_write_within(deadline, "send", up.send(tungstenite::Message::Text(payload.into()))).await
    {
        tracing::warn!(connection_id = sess.connection_id, phase = "turn", account = %acct.label,
            error = %failure.message, "codex websocket send failed, using HTTP");
        // A send only fails or times out with part of the message still unwritten, and the
        // socket is dropped here, so upstream never received the request: HTTP runs it once.
        sess.discard_upstream();
        tracker.cancel();
        return Native::Fallback;
    }

    let mut parser = responses::Parser::default();
    let mut aggregate = ir::Aggregate::default();
    let mut usage = Usage::default();
    let mut evs = Vec::new();
    let mut error: Option<(u16, String)> = None;
    let mut forwarded = false;
    let mut terminal = false;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(600), up.next()).await;
        let text = match next {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => t.to_string(),
            Ok(Some(Ok(tungstenite::Message::Binary(b)))) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Some(Ok(tungstenite::Message::Close(frame)))) => {
                tracing::warn!(
                    connection_id = sess.connection_id,
                    phase = "turn",
                    close_code = frame.as_ref().map(|frame| u16::from(frame.code)),
                    "codex upstream websocket closed during turn"
                );
                if let Err(failure) = upstream_write("flush", up.flush()).await {
                    failure.log(&sess.connection_id, "turn_close");
                }
                error = Some((502, "codex websocket closed".into()));
                break;
            }
            Ok(None) => {
                tracing::warn!(
                    connection_id = sess.connection_id,
                    phase = "turn",
                    "codex upstream websocket ended during turn"
                );
                error = Some((502, "codex websocket closed".into()));
                break;
            }
            Ok(Some(Ok(tungstenite::Message::Ping(_)))) => {
                if let Err(failure) = upstream_write("flush", up.flush()).await {
                    failure.log(&sess.connection_id, "turn");
                    error = Some((failure.status, failure.message));
                    break;
                }
                continue;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                let failure = UpstreamFailure::socket("read", &e);
                failure.log(&sess.connection_id, "turn");
                error = Some((failure.status, failure.message));
                break;
            }
            Err(_) => {
                let failure = UpstreamFailure::timeout("read");
                failure.log(&sess.connection_id, "turn");
                error = Some((failure.status, failure.message));
                break;
            }
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let kind = v["type"].as_str().unwrap_or_default().to_string();
        crate::quota::observe_codex_event(&acct, &v, quota_epoch);
        parser.feed(&SseEvent { event: None, data: text.clone() }, &mut evs);
        for ev in evs.drain(..) {
            aggregate.push(&ev);
            match ev {
                Event::Usage(u) => usage.merge(&u),
                Event::Error { status, message } => error = Some((status, message)),
                Event::Text(_) | Event::Reasoning(_) | Event::ToolStart { .. } => tracker.first_token(),
                _ => {}
            }
        }
        if matches!(kind.as_str(), "error" | "response.failed")
            && let Some((status, msg)) = error.clone()
        {
            if proxy::quota_exhausted(&acct, &model, status, &text) {
                proxy::mark_quota_exhausted(&acct, &model, &reqwest::header::HeaderMap::new(), &text, quota_epoch);
                app.broadcast("accounts", Value::Null);
                if !forwarded {
                    sess.discard_upstream();
                    close_upstream(&mut up, &sess.connection_id, "quota_rejected").await;
                    tracker.finish(status, &usage, Some(msg));
                    return Native::Fallback;
                }
            } else if !forwarded && matches!(status, 401 | 403) {
                // HTTP fallback refreshes credentials on the same assigned account.
                sess.discard_upstream();
                close_upstream(&mut up, &sess.connection_id, "auth_rejected").await;
                tracker.cancel();
                return Native::Fallback;
            } else if status == 429 {
                // A plain rate limit: step aside briefly. Sessions detour meanwhile.
                acct.cool(Some(&model), chrono::Utc::now() + chrono::Duration::seconds(60), &format!("429: {msg}"));
                app.broadcast("accounts", Value::Null);
            } else if matches!(status, 401 | 403) {
                // An error after response.created also invalidates this socket.
                // Refresh the same subscription before the client's next turn.
                if let Err(e) = crate::oauth::ensure_fresh(app, &acct, chrono::Duration::minutes(5), true).await {
                    acct.cool(
                        None,
                        chrono::Utc::now() + chrono::Duration::seconds(60),
                        &format!("token refresh failed: {e}"),
                    );
                }
            }
        }
        if kind == "response.completed" || kind == "response.incomplete" {
            capture(app, headers, sess, &text, full, &aggregate);
        }
        terminal = matches!(kind.as_str(), "response.completed" | "response.incomplete" | "response.failed" | "error");
        forwarded = true;
        if send(tx, text).await.is_err() {
            sess.discard_upstream();
            tracker.finish(499, &usage, Some("client disconnected".into()));
            return Native::Gone;
        }
        if terminal {
            break;
        }
    }
    if terminal && !error.as_ref().is_some_and(|(status, _)| matches!(status, 401 | 403)) {
        sess.upstream = Some((acct.clone(), up));
    } else {
        sess.discard_upstream();
    }
    if !terminal {
        // The upstream socket died mid-turn: tell the client and reconnect next turn.
        let (status, msg) = error.clone().unwrap_or((502, "codex websocket closed".into()));
        let body = json!({ "error": { "message": msg, "type": "upstream_error" } });
        if send(tx, error_event(status, &body)).await.is_err() {
            tracker.finish(status, &usage, Some(msg));
            return Native::Gone;
        }
    }
    match error {
        Some((s, m)) => tracker.finish(s, &usage, Some(m)),
        None => {
            acct.record_ok();
            tracker.finish(200, &usage, None)
        }
    }
    Native::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};
    use tokio_tungstenite::WebSocketStream;
    use tungstenite::protocol::Role;

    async fn blocked_upstream() -> (WebSocketStream<DuplexStream>, DuplexStream) {
        let (socket, peer) = duplex(1);
        (WebSocketStream::from_raw_socket(socket, Role::Client, None).await, peer)
    }

    async fn buffered_upstream(buffer: Vec<u8>) -> (Session, WebSocketStream<tokio::net::TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let up = WebSocketStream::from_partially_read(
            tokio_tungstenite::MaybeTlsStream::Plain(client),
            buffer,
            Role::Client,
            None,
        )
        .await;
        let pool = crate::accounts::Pool::default();
        pool.reload(&crate::config::Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![crate::config::KeyEntry { api_key: "local-mock".into(), ..Default::default() }],
            ..Default::default()
        });
        let acct = pool.all().pop().unwrap();
        let sess = Session {
            upstream: Some((acct, up)),
            upstream_responses: HashSet::from(["prior-response".into()]),
            upstream_quota_epoch: 7,
            key: Some("retained-session".into()),
            ..Default::default()
        };
        (sess, peer)
    }

    #[tokio::test]
    async fn ready_upstream_drain_stops_at_frame_budget() {
        for frames in [MAX_IDLE_DRAIN - 1, MAX_IDLE_DRAIN, MAX_IDLE_DRAIN + 1] {
            // Every Pong is already buffered, so TCP batching cannot change the ready-frame count.
            let (mut sess, _peer) = buffered_upstream([0x8a, 0].repeat(frames)).await;
            tokio::time::timeout(Duration::from_secs(1), drain_idle_upstream(&mut sess))
                .await
                .expect("a ready upstream drain must return within its frame budget");
            if frames < MAX_IDLE_DRAIN {
                assert!(sess.upstream.is_some());
                assert!(sess.upstream_responses.contains("prior-response"));
                assert_eq!(sess.upstream_quota_epoch, 7);
            } else {
                assert!(sess.upstream.is_none());
                assert!(sess.upstream_responses.is_empty());
                assert_eq!(sess.upstream_quota_epoch, 0);
            }
            assert_eq!(sess.key.as_deref(), Some("retained-session"));
        }
    }

    #[tokio::test]
    async fn ready_upstream_close_is_drained_before_submitting_next_turn() {
        // Buffered server Ping and normal Close make readiness independent of network scheduling.
        let (mut sess, mut peer) = buffered_upstream(vec![0x89, 1, b'p', 0x88, 2, 0x03, 0xe8]).await;
        tokio::time::timeout(Duration::from_secs(1), drain_idle_upstream(&mut sess)).await.unwrap();
        assert!(sess.upstream.is_none());
        assert!(sess.upstream_responses.is_empty());
        assert_eq!(sess.upstream_quota_epoch, 0);
        assert_eq!(sess.key.as_deref(), Some("retained-session"));
        let pong = tokio::time::timeout(Duration::from_secs(1), peer.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(pong, tungstenite::Message::Pong(vec![b'p'].into()));
        let close = tokio::time::timeout(Duration::from_secs(1), peer.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(close, tungstenite::Message::Close(_)));
    }

    #[test]
    fn turn_uploads_get_time_for_slow_uplinks() {
        assert_eq!(send_deadline(1024), UPSTREAM_WRITE_TIMEOUT);
        // A 6 MB turn at 300 KB/s takes about 20 s; the deadline covers a 256 kbit/s uplink.
        assert_eq!(send_deadline(6 << 20), UPSTREAM_WRITE_TIMEOUT + Duration::from_secs(192));
    }

    #[tokio::test]
    async fn upstream_send_flush_and_close_stop_at_write_deadline() {
        let (mut sender, _sender_peer) = blocked_upstream().await;
        let (mut flusher, _flusher_peer) = blocked_upstream().await;
        let (mut closer, _closer_peer) = blocked_upstream().await;
        // Feed queues a frame; the unread one-byte transport blocks its flush.
        flusher.feed(tungstenite::Message::Text("queued request".into())).await.unwrap();
        let started = tokio::time::Instant::now();
        let failures = tokio::time::timeout(UPSTREAM_WRITE_TIMEOUT + Duration::from_secs(3), async {
            futures::join!(
                upstream_write("send", sender.send(tungstenite::Message::Text("submitted request".into()))),
                upstream_write("flush", flusher.flush()),
                upstream_write("close", closer.close(None)),
            )
        })
        .await
        .expect("blocked websocket writes outlived their deadline");
        assert!(started.elapsed() >= UPSTREAM_WRITE_TIMEOUT);
        for (operation, result) in [("send", failures.0), ("flush", failures.1), ("close", failures.2)] {
            let failure = result.expect_err("an unread duplex transport must block");
            assert_eq!(failure.status, 504);
            assert_eq!(failure.operation, operation);
            assert_eq!(failure.message, format!("codex websocket {operation} timed out"));
        }
    }

    #[test]
    fn upstream_failure_diagnostics_exclude_frames_credentials_bodies_and_urls() {
        let secret = "private-prompt-token-account";
        let errors = [
            tungstenite::Error::WriteBufferFull(tungstenite::Message::Text(secret.into())),
            tungstenite::Error::Utf8(secret.into()),
            tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(secret.into())),
            tungstenite::Error::Io(std::io::Error::other(secret)),
            tungstenite::Error::Http(
                tungstenite::http::Response::builder()
                    .status(403)
                    .header("authorization", secret)
                    .body(Some(secret.as_bytes().to_vec()))
                    .unwrap(),
            ),
        ];
        for error in errors {
            let failure = UpstreamFailure::socket("send", &error);
            assert_eq!(failure.status, 502);
            assert!(!failure.message.contains(secret));
            assert!(failure.message.starts_with("codex websocket send failed:"));
        }
        let error = tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake);
        assert!(UpstreamFailure::socket("read", &error).message.contains("Connection reset without closing handshake"));
    }
}
