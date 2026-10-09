//! Exercise actual HTTP and WebSocket routes against token-free local providers.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use axum::Json;
use axum::extract::{
    State,
    ws::{Message, WebSocketUpgrade},
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::config::{Config, KeyEntry, Routing};
use crate::state::{App, RequestLog};

#[derive(Default)]
struct Mock {
    mode: AtomicU8,
    sequence: AtomicU64,
    calls: Mutex<Vec<(String, Value, &'static str)>>,
    ws_connections: AtomicU64,
    ws_mode: Mutex<WsMode>,
    ws_rich_output: AtomicBool,
    ws_control: Mutex<Option<WsControl>>,
    quota_gate: Mutex<Option<Arc<QuotaGate>>>,
}

#[derive(Default)]
struct QuotaGate {
    started: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
impl Mock {
    /// Wait for the mock provider to pause a response while newer quota evidence is applied.
    async fn await_quota_gate(&self, account: &str) {
        let gate = if account == "a" { self.quota_gate.lock().take() } else { None };
        if let Some(gate) = gate {
            gate.started.notify_one();
            gate.resume.notified().await;
        }
    }
}

#[derive(Clone, Copy, Default)]
enum WsMode {
    #[default]
    Normal,
    Partial,
    DropOnHandshake,
}

enum WsCommand {
    Ping(Vec<u8>),
    Data(Value),
    Flood,
    Close,
    Drop,
}

#[derive(Debug, PartialEq)]
enum WsObservation {
    Pong(Vec<u8>),
    Closed,
    Dropped,
    Flooding,
}

struct WsControl {
    commands: tokio::sync::mpsc::UnboundedReceiver<WsCommand>,
    observations: tokio::sync::mpsc::UnboundedSender<WsObservation>,
}

struct WsPeer {
    commands: tokio::sync::mpsc::UnboundedSender<WsCommand>,
    observations: tokio::sync::mpsc::UnboundedReceiver<WsObservation>,
}

impl WsPeer {
    async fn observe(&mut self) -> WsObservation {
        tokio::time::timeout(std::time::Duration::from_secs(3), self.observations.recv())
            .await
            .expect("proxy did not service the idle upstream websocket")
            .expect("mock upstream disconnected before the expected observation")
    }
}

fn account(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .into()
}

fn completed(mock: &Mock, account: &str, body: &Value) -> Value {
    json!({"id":format!("resp_{}", mock.sequence.fetch_add(1, Ordering::Relaxed)), "object":"response", "status":"completed", "model":body["model"], "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":account}]}], "usage":{"input_tokens":100, "input_tokens_details":{"cached_tokens":80}, "output_tokens":1}})
}

fn events(response: &Value, omit_output: bool) -> Vec<Value> {
    let mut terminal = response.clone();
    if omit_output {
        terminal["output"] = json!([]);
    }
    vec![
        json!({"type":"response.created", "response":{"id":response["id"], "model":response["model"]}}),
        json!({"type":"response.output_text.delta", "delta":response["output"][0]["content"][0]["text"], "output_index":0, "content_index":0}),
        json!({"type":"response.completed", "response":terminal}),
    ]
}

fn rich_events(response: &mut Value) -> Vec<Value> {
    let message = response["output"][0].clone();
    response["output"] = json!([
        {"id":"reasoning_local", "type":"reasoning", "summary":[{"type":"summary_text", "text":"tool planning"}], "encrypted_content":"local-encrypted-reasoning"},
        {"id":"tool_local", "type":"function_call", "call_id":"call_local", "name":"lookup", "arguments":"{\"query\":\"local\"}"},
        message
    ]);
    let mut out = vec![json!({"type":"response.created", "response":{"id":response["id"], "model":response["model"]}})];
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        out.push(json!({"type":"response.output_item.added", "output_index":index, "item":item}));
        if item["type"] == "reasoning" {
            out.push(
                json!({"type":"response.reasoning_summary_text.delta", "output_index":index, "delta":"tool planning"}),
            );
        }
        out.push(json!({"type":"response.output_item.done", "output_index":index, "item":item}));
    }
    let mut terminal = response.clone();
    // The proxy must rebuild the history from the streamed tool and reasoning items.
    terminal["output"] = json!([]);
    out.push(json!({"type":"response.completed", "response":terminal}));
    out
}

/// Handle synthetic HTTP provider requests and coordinate delayed quota-response tests.
async fn mock_http(State(mock): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let account = account(&headers);
    mock.calls.lock().push((account.clone(), body.clone(), "http"));
    let mode = mock.mode.load(Ordering::Relaxed);
    mock.await_quota_gate(&account).await;
    if account == "a" && ((1..=3).contains(&mode) || mode == 6) {
        let (status, code) = match mode {
            1 => (429, "usage_limit_reached"),
            2 => (429, "rate_limit_exceeded"),
            6 => (401, "invalid_api_key"),
            _ => (503, "server_is_overloaded"),
        };
        return (
            StatusCode::from_u16(status).unwrap(),
            [("retry-after", "120")],
            Json(json!({"error":{"code":code, "message":"mock error", "resets_in_seconds":3600}})),
        )
            .into_response();
    }
    if account == "a" && mode == 7 {
        return Json(json!({"type":"response.failed", "response":{"error":{"code":"usage_limit_reached", "message":"mock quota error"}}})).into_response();
    }
    if account == "a" && mode == 4 {
        let event = json!({"type":"response.failed", "response":{"error":{"code":"usage_limit_reached", "message":"mock error"}}});
        return ([("content-type", "text/event-stream")], format!("data: {event}\n\n")).into_response();
    }
    if body["messages"].is_array() {
        return Json(json!({"id":"message", "type":"message", "role":"assistant", "model":body["model"], "content":[{"type":"text", "text":account}], "stop_reason":"end_turn", "usage":{"input_tokens":100,"output_tokens":1}})).into_response();
    }
    let response = completed(&mock, &account, &body);
    let mut out = if body["stream"] == true {
        let sse: String = events(&response, true).into_iter().map(|v| format!("data: {v}\n\n")).collect();
        ([("content-type", "text/event-stream")], sse).into_response()
    } else {
        Json(response).into_response()
    };
    if account == "a" && mode == 5 {
        for (k, v) in [
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-after-seconds", "3600"),
        ] {
            out.headers_mut().insert(k, v.parse().unwrap());
        }
    }
    out
}

async fn mock_chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let account = account(&headers);
    mock.calls.lock().push((account.clone(), body.clone(), "chat"));
    Json(json!({
        "id":"chat-response", "object":"chat.completion", "model":body["model"],
        "choices":[{"index":0,"message":{"role":"assistant","content":account},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":80}}
    }))
    .into_response()
}

/// Handle synthetic native WebSocket turns with deterministic quota and disconnect scenarios.
async fn mock_ws(State(mock): State<Arc<Mock>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let account = account(&headers);
    upgrade.on_upgrade(move |socket| async move {
        mock.ws_connections.fetch_add(1, Ordering::Relaxed);
        if matches!(*mock.ws_mode.lock(), WsMode::DropOnHandshake) {
            return;
        }
        let (mut tx, mut rx) = socket.split();
        let mut control = mock.ws_control.lock().take();
        let mut known = std::collections::HashSet::<String>::new();
        let mut flooding = false;
        let mut flood_announced = false;
        loop {
            let message = tokio::select! {
                // The provider must still read and answer requests while sending idle frames.
                biased;
                message = rx.next() => message,
                command = async {
                    match &mut control {
                        Some(control) => control.commands.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match command {
                        Some(WsCommand::Ping(data)) => {
                            if tx.send(Message::Ping(data.into())).await.is_err() { return; }
                        }
                        Some(WsCommand::Data(value)) => {
                            if tx.send(Message::Text(value.to_string().into())).await.is_err() { return; }
                        }
                        Some(WsCommand::Flood) => flooding = true,
                        Some(WsCommand::Close) => {
                            if tx.send(Message::Close(None)).await.is_err() { return; }
                        }
                        Some(WsCommand::Drop) => {
                            drop(tx);
                            drop(rx);
                            let _ = control.as_ref().unwrap().observations.send(WsObservation::Dropped);
                            return;
                        }
                        None => control = None,
                    }
                    continue;
                }
                result = async {
                    for _ in 0..64 {
                        tx.feed(Message::Pong(vec![0; 16].into())).await?;
                    }
                    tx.flush().await
                }, if flooding => {
                    if result.is_err() { return; }
                    if !flood_announced {
                        let _ = control.as_ref().unwrap().observations.send(WsObservation::Flooding);
                        flood_announced = true;
                    }
                    continue;
                }
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Pong(data))) => {
                    if let Some(control) = &control {
                        let _ = control.observations.send(WsObservation::Pong(data.to_vec()));
                    }
                    continue;
                }
                Some(Ok(Message::Close(_))) => {
                    let _ = tx.flush().await;
                    if let Some(control) = &control {
                        let _ = control.observations.send(WsObservation::Closed);
                    }
                    return;
                }
                Some(Ok(_)) => continue,
                _ => {
                    if let Some(control) = &control {
                        let _ = control.observations.send(WsObservation::Dropped);
                    }
                    return;
                }
            };
            let body: Value = serde_json::from_str(&text).unwrap();
            mock.calls.lock().push((account.clone(), body.clone(), "ws"));
            let mode = mock.mode.load(Ordering::Relaxed);
            mock.await_quota_gate(&account).await;
            if account == "a" && mode == 1 {
                let error = json!({"type":"error", "status":429, "error":{"code":"usage_limit_reached", "message":"mock error", "resets_in_seconds":3600}});
                if tx.send(Message::Text(error.to_string().into())).await.is_err() { break; }
                continue;
            }
            if let Some(prev) = body["previous_response_id"].as_str() && !known.contains(prev) {
                let error = json!({"type":"error", "status":400, "error":{"code":"previous_response_not_found", "message":"unknown on this socket"}});
                if tx.send(Message::Text(error.to_string().into())).await.is_err() { break; }
                continue;
            }
            let mut response = completed(&mock, &account, &body);
            known.insert(response["id"].as_str().unwrap().to_string());
            let mut output = if mock.ws_rich_output.load(Ordering::Relaxed) {
                rich_events(&mut response)
            } else {
                events(&response, true)
            };
            let partial = matches!(*mock.ws_mode.lock(), WsMode::Partial);
            if partial {
                output.truncate(2);
            }
            for event in output {
                if tx.send(Message::Text(event.to_string().into())).await.is_err() { return; }
            }
            if partial {
                return;
            }
        }
    }).into_response()
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(router: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    Server { url, task }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    directory: PathBuf,
    cfg: Config,
    app: Arc<App>,
    proxy: Server,
    provider: Server,
    mock: Arc<Mock>,
}

impl Fixture {
    fn control_ws(&self) -> WsPeer {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (observation_tx, observations) = tokio::sync::mpsc::unbounded_channel();
        *self.mock.ws_control.lock() = Some(WsControl { commands: command_rx, observations: observation_tx });
        WsPeer { commands, observations }
    }

    async fn new(routing: Routing, native: bool) -> Self {
        let mock = Arc::new(Mock::default());
        let provider = serve(
            axum::Router::new()
                .route("/v1/responses", post(mock_http).get(mock_ws))
                .route("/v1/responses/compact", post(mock_http))
                .route("/v1/messages", post(mock_http))
                .route("/v1/chat/completions", post(mock_chat))
                .layer(axum::extract::DefaultBodyLimit::disable())
                .with_state(mock.clone()),
        )
        .await;
        let directory = std::env::temp_dir().join(format!("cliproxy-routing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = Config {
            auth_dir: directory.to_string_lossy().into(),
            api_keys: vec!["client-one".into(), "client-two".into()],
            routing,
            codex_websockets: native,
            ..Default::default()
        };
        if native {
            for account in ["a", "b"] {
                std::fs::write(directory.join(format!("{account}.json")), json!({"type":"codex", "access_token":account, "email":account, "base_url":format!("{}/v1", provider.url)}).to_string()).unwrap();
            }
        } else {
            cfg.codex_api_key = ["a", "b"]
                .map(|account| KeyEntry {
                    api_key: account.into(),
                    base_url: Some(format!("{}/v1", provider.url)),
                    label: Some(account.into()),
                    ..Default::default()
                })
                .to_vec();
        }
        let app = App::new(cfg.clone(), directory.join("config.yaml"));
        let proxy = serve(crate::server::router(app.clone())).await;
        Self { directory, cfg, app, proxy, provider, mock }
    }

    async fn request(&self, task: Option<&str>, body: Value) -> (u16, Value) {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/responses", self.proxy.url))
            .bearer_auth("client-one")
            .json(&body);
        if let Some(task) = task {
            request = request.header("thread-id", task);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }

    async fn socket(
        &self,
        task: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        self.socket_with_task(Some(task)).await
    }

    async fn socket_with_task(
        &self,
        task: Option<&str>,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let mut request =
            format!("{}/v1/responses", self.proxy.url.replacen("http://", "ws://", 1)).into_client_request().unwrap();
        request.headers_mut().insert("authorization", "Bearer client-one".parse().unwrap());
        if let Some(task) = task {
            request.headers_mut().insert("thread-id", task.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    async fn logs(&self, count: usize) -> Vec<RequestLog> {
        // A WebSocket's terminal event can reach the client just before its tracker finishes.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let mut logs: Vec<_> = self.app.stats.recent.lock().iter().cloned().collect();
                if logs.len() >= count {
                    logs.sort_by_key(|log| log.id);
                    return logs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("request diagnostics were not recorded")
    }

    fn recover(&self) {
        self.mock.mode.store(0, Ordering::Relaxed);
        for account in self.app.pool.all() {
            let mut state = account.state.lock();
            state.cooldowns.clear();
            state.quota_cooldowns.clear();
            state.quota = Default::default();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.proxy.task.abort();
        self.provider.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn prompt() -> Value {
    json!({"model":"gpt-6.1-sol","input":"question"})
}
fn answer(response: &Value) -> &str {
    response["output"][0]["content"][0]["text"].as_str().unwrap()
}

async fn turn(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    mut body: Value,
) -> Value {
    body["type"] = "response.create".into();
    socket.send(tungstenite::Message::Text(body.to_string().into())).await.unwrap();
    loop {
        let event = ws_event(socket).await;
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.completed" {
            return event["response"].clone();
        }
    }
}

async fn ws_event(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Value {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("proxy did not finish the websocket turn")
            .unwrap()
            .unwrap();
        match frame {
            tungstenite::Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            tungstenite::Message::Close(_) => panic!("proxy closed the client websocket before completing the turn"),
            _ => {}
        }
    }
}

async fn client_ping(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let data = b"idle-client-barrier".to_vec();
    socket.send(tungstenite::Message::Ping(data.clone().into())).await.unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(reply, tungstenite::Message::Pong(data.into()));
}

#[tokio::test]
async fn smart_balancing_uses_reserve_and_session_load_over_http_and_websockets() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::SmartQuota, native).await;
        let accounts = fixture.app.pool.all();
        let now = chrono::Utc::now();
        for (account, days) in accounts.iter().zip([3, 5]) {
            account.state.lock().quota = crate::quota::Quota {
                windows: vec![
                    crate::quota::Window {
                        name: "5h".into(),
                        used: 0.0,
                        resets_at: Some(now + chrono::Duration::hours(4)),
                        model: None,
                    },
                    crate::quota::Window {
                        name: "week".into(),
                        used: 20.0,
                        resets_at: Some(now + chrono::Duration::days(days)),
                        model: None,
                    },
                ],
                updated_at: Some(now),
                ..Default::default()
            };
        }
        let first = fixture.request(Some("http-first"), prompt()).await;
        assert_eq!(first.0, 200);
        assert_eq!(answer(&first.1), "a");
        let mut socket = fixture.socket("ws-second").await;
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b"); // new load counts before quota moves
        assert_eq!(answer(&fixture.request(Some("http-first"), prompt()).await.1), "a");

        // Change the reserve at runtime without disturbing either existing assignment.
        let mut cfg = fixture.cfg.clone();
        cfg.five_hour_reserve_percent = 60;
        fixture.app.set_config(cfg);
        accounts[0].state.lock().quota.windows[0].used = 41.0;
        assert_eq!(answer(&fixture.request(Some("new-after-reserve"), prompt()).await.1), "b");
        assert_eq!(answer(&fixture.request(Some("http-first"), prompt()).await.1), "a");
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");

        let logs = fixture.logs(6).await;
        assert!(logs.iter().all(|log| log.routing_strategy == Routing::SmartQuota));
        assert!(accounts.iter().all(|a| a.state.lock().active_requests.load(Ordering::Relaxed) == 0));
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn smart_balancing_uses_weekly_headroom_without_inventing_a_five_hour_reserve() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::SmartQuota, native).await;
        let accounts = fixture.app.pool.all();
        let reset = chrono::Utc::now() + chrono::Duration::days(3);
        for (account, used) in accounts.iter().zip([80.0, 20.0]) {
            crate::quota::authoritative(
                &mut account.state.lock(),
                vec![crate::quota::Window { name: "week".into(), used, resets_at: Some(reset), model: None }],
                None,
            );
        }
        // Identical renewals: known weekly headroom beats the old equal 50% fallback.
        assert_eq!(answer(&fixture.request(Some("weekly-http"), prompt()).await.1), "b");
        let mut socket = fixture.socket("weekly-ws").await;
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");

        // A real 5-hour window at the reserve takes precedence over weekly-only headroom.
        accounts[0].state.lock().quota.windows.push(crate::quota::Window {
            name: "5h".into(),
            used: 70.0,
            resets_at: Some(reset),
            model: None,
        });
        assert_eq!(answer(&fixture.request(Some("real-reserve"), prompt()).await.1), "a");
        assert_eq!(answer(&fixture.request(Some("weekly-http"), prompt()).await.1), "b");
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b"); // existing assignment stays pinned
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn request_load_releases_on_retry_finish_cancel_and_drop() {
    let fixture = Fixture::new(Routing::SmartQuota, false).await;
    let accounts = fixture.app.pool.all();
    let count = |i: usize| accounts[i].state.lock().active_requests.load(Ordering::Relaxed);
    let new_tracker =
        || crate::proxy::Tracker::new(&fixture.app, crate::ir::Format::Responses, true, "http", "gpt-6.1-sol");
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    assert_eq!(count(0), 1);
    tracker.attempt(&accounts[0]); // retry on same account must not accumulate
    assert_eq!(count(0), 1);
    assert_eq!(answer(&fixture.request(None, prompt()).await.1), "b"); // unbound requests see in-flight work
    tracker.attempt(&accounts[1]);
    assert_eq!((count(0), count(1)), (0, 1));
    tracker.finish(200, &crate::ir::Usage::default(), None);
    assert_eq!((count(0), count(1)), (0, 0));
    drop(tracker);
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    tracker.cancel();
    tracker.cancel();
    assert_eq!(count(0), 0);
    drop(tracker);
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    drop(tracker); // client disconnects mid-stream
    assert_eq!(count(0), 0);
}

#[tokio::test]
async fn http_tasks_stay_pinned_and_migrate_only_when_quota_runs_out() {
    for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
        let fixture = Fixture::new(routing, false).await;
        let task = "private-coding-thread-identifier";
        for _ in 0..3 {
            let (status, response) = fixture.request(Some(task), prompt()).await;
            assert_eq!(status, 200);
            assert_eq!(answer(&response), "a");
        }
        let initial = fixture.logs(3).await;
        let session = initial[0].session_id.as_deref().unwrap();
        assert_eq!(session.len(), 64);
        assert!(session.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(initial[0].routing_reason, Some("new_session"));
        for log in &initial[1..] {
            assert_eq!(log.routing_reason, Some("session_reused"));
        }
        fixture.mock.mode.store(1, Ordering::Relaxed);
        let (status, response) = fixture.request(Some(task), prompt()).await;
        assert_eq!(status, 200);
        assert_eq!(answer(&response), "b");
        let migrated = fixture.logs(4).await.pop().unwrap();
        assert_eq!(migrated.routing_reason, Some("quota_exhausted"));
        assert_eq!(migrated.routing_attempts.len(), 2);
        assert_eq!(migrated.routing_attempts[0].reason, "session_reused");
        assert_eq!(migrated.routing_attempts[1].reason, "quota_exhausted");
        assert_eq!(
            migrated.routing_attempts[1].previous_account.as_deref(),
            Some(initial[0].routing_attempts[0].account_id.as_str())
        );
        assert_ne!(migrated.routing_attempts[0].account_id, migrated.routing_attempts[1].account_id);
        fixture.recover();
        for _ in 0..3 {
            assert_eq!(answer(&fixture.request(Some(task), prompt()).await.1), "b");
        }
        let logs = fixture.logs(7).await;
        for log in &logs {
            assert_eq!(log.session_id.as_deref(), Some(session));
            assert_eq!(log.session_source, Some("thread-id"));
            assert_eq!(log.routing_strategy, routing);
            assert_eq!(log.routing_warning, None);
        }
        for log in &logs[4..] {
            assert_eq!(log.routing_reason, Some("session_reused"));
        }
        let serialized = serde_json::to_string(&logs).unwrap();
        assert!(!serialized.contains(task));
        assert!(!serialized.contains("client-one"));
        let restarted = App::new(fixture.cfg.clone(), fixture.directory.join("config.yaml"));
        let proxy = serve(crate::server::router(restarted)).await;
        let response: Value = reqwest::Client::new()
            .post(format!("{}/v1/responses", proxy.url))
            .bearer_auth("client-one")
            .header("thread-id", task)
            .json(&prompt())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(answer(&response), "b");
    }
}

#[tokio::test]
async fn temporary_failures_detour_and_the_session_returns_to_its_subscription() {
    for mode in [2, 3, 6] {
        let fixture = Fixture::new(Routing::RoundRobin, false).await;
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
        fixture.mock.mode.store(mode, Ordering::Relaxed);
        // The client still gets an answer, from the other subscription...
        let (status, body) = fixture.request(Some("task"), prompt()).await;
        assert_eq!(status, 200, "mode {mode}");
        assert_eq!(answer(&body), "b");
        assert_eq!(fixture.logs(2).await.pop().unwrap().routing_reason, Some("temporary_detour"));
        // ...and the session goes back to its own once that recovers.
        fixture.recover();
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    }
}

#[tokio::test]
async fn quota_headers_and_stream_errors_trigger_migration_on_the_next_call() {
    for mode in [4, 5] {
        let fixture = Fixture::new(Routing::RoundRobin, false).await;
        fixture.mock.mode.store(mode, Ordering::Relaxed);
        assert_eq!(fixture.request(Some("task"), prompt()).await.0, if mode == 4 { 429 } else { 200 });
        fixture.mock.mode.store(0, Ordering::Relaxed);
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "b");
    }
}

#[tokio::test]
/// Verify that delayed http json and sse quota errors preserve newer recovery.
async fn delayed_http_json_and_sse_quota_errors_preserve_newer_recovery() {
    for (mode, converted) in [(1, false), (4, false), (7, false), (4, true), (7, true)] {
        for newer_epoch in [false, true] {
            let fixture = Fixture::new(Routing::RoundRobin, false).await;
            assert_eq!(answer(&fixture.request(Some("epoch-task"), prompt()).await.1), "a");
            let account = fixture.app.pool.all().into_iter().find(|a| a.label == "a").unwrap();
            account.state.lock().notifications_enabled = true;
            let gate = Arc::new(QuotaGate::default());
            *fixture.mock.quota_gate.lock() = Some(gate.clone());
            fixture.mock.mode.store(mode, Ordering::Relaxed);
            let request = async {
                if converted {
                    reqwest::Client::new()
                        .post(format!("{}/v1/chat/completions", fixture.proxy.url))
                        .bearer_auth("client-one")
                        .header("thread-id", "epoch-task")
                        .json(&json!({"model":"gpt-6.1-sol", "messages":[{"role":"user","content":"question"}]}))
                        .send()
                        .await
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap();
                } else {
                    fixture.request(Some("epoch-task"), prompt()).await;
                }
            };
            let release = async {
                tokio::time::timeout(std::time::Duration::from_secs(3), gate.started.notified()).await.unwrap();
                if newer_epoch {
                    let mut state = account.state.lock();
                    state.quota_epoch += 1;
                    crate::quota::usage(
                        &mut state,
                        crate::accounts::Provider::Codex,
                        &json!({"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":0,"reset_at":4102444800i64},"secondary_window":{"limit_window_seconds":604800,"used_percent":0,"reset_at":4102444800i64}}}),
                    );
                }
                if newer_epoch {
                    fixture.mock.mode.store(0, Ordering::Relaxed);
                }
                gate.resume.notify_one();
            };
            let _ = tokio::join!(request, release);
            let state = account.state.lock();
            assert_eq!(
                state.quota_cooldowns.is_empty(),
                newer_epoch,
                "mode={mode}, converted={converted}, newer={newer_epoch}"
            );
            assert_eq!(state.notification_evidence.observations.iter().any(|o| o.unknown.is_some()), !newer_epoch);
            if newer_epoch {
                assert!(state.quota.windows.iter().all(|w| w.used == 0.0));
            }
        }
    }
}

#[tokio::test]
/// Verify that reused native websocket turn captures its own quota epoch.
async fn reused_native_websocket_turn_captures_its_own_quota_epoch() {
    for newer_epoch in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, true).await;
        let mut socket = fixture.socket("epoch-ws-task").await;
        let first = turn(&mut socket, prompt()).await;
        fixture.logs(1).await;
        let account = fixture.app.pool.all().into_iter().find(|a| a.label == "a").unwrap();
        {
            let mut state = account.state.lock();
            state.notifications_enabled = true;
            state.quota_epoch += 1; // A reused connection's next turn needs this newer epoch.
        }
        let gate = Arc::new(QuotaGate::default());
        *fixture.mock.quota_gate.lock() = Some(gate.clone());
        fixture.mock.mode.store(1, Ordering::Relaxed);
        let body = json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"next"});
        let request = turn(&mut socket, body);
        let release = async {
            tokio::time::timeout(std::time::Duration::from_secs(3), gate.started.notified()).await.unwrap();
            if newer_epoch {
                let mut state = account.state.lock();
                state.quota_epoch += 1;
                crate::quota::usage(
                    &mut state,
                    crate::accounts::Provider::Codex,
                    &json!({"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":0,"reset_at":4102444800i64},"secondary_window":{"limit_window_seconds":604800,"used_percent":0,"reset_at":4102444800i64}}}),
                );
            }
            if newer_epoch {
                fixture.mock.mode.store(0, Ordering::Relaxed);
            }
            gate.resume.notify_one();
        };
        let _ = tokio::join!(request, release);
        fixture.logs(2).await;
        let state = account.state.lock();
        assert_eq!(state.quota_cooldowns.is_empty(), newer_epoch);
        assert_eq!(state.notification_evidence.observations.iter().any(|o| o.unknown.is_some()), !newer_epoch);
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls[1].2, "ws");
        assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 1, "the second turn must reuse its socket");
    }
}

#[tokio::test]
async fn response_ids_continue_the_task_without_session_headers_and_replay_full_input() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(None, prompt()).await.1;
    let first_log = fixture.logs(1).await.pop().unwrap();
    assert_eq!(first_log.session_source, Some("generated_response"));
    assert_eq!(first_log.routing_warning, Some("missing_session_id"));
    fixture.mock.mode.store(1, Ordering::Relaxed);
    let body = json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"next"});
    let (status, next) = fixture.request(None, body).await;
    assert_eq!(status, 200);
    assert_eq!(answer(&next), "b");
    let next_log = fixture.logs(2).await.pop().unwrap();
    assert_eq!(next_log.session_id, first_log.session_id);
    assert_eq!(next_log.session_source, Some("previous_response_id"));
    assert_eq!(next_log.routing_warning, Some("response_id_only"));
    // The first turn had no session id, so the continuation makes the first assignment.
    assert_eq!(next_log.routing_reason, Some("new_session"));
    {
        let calls = fixture.mock.calls.lock();
        let (_, sent, _) = calls.last().unwrap();
        assert!(sent["previous_response_id"].is_null());
        assert_eq!(sent["input"].as_array().unwrap().len(), 3);
    }
    assert_eq!(
        fixture.request(None, json!({"model":"gpt-6.1-sol","previous_response_id":"external", "input":"next"})).await.0,
        200
    );
}

#[tokio::test]
async fn websocket_fallback_and_reconnect_share_http_assignments_and_history() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(Some("task"), prompt()).await.1;
    let mut socket = fixture.socket("task").await;
    let next =
        turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"next"})).await;
    socket.close(None).await.unwrap();
    let mut socket = fixture.socket("task").await;
    turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":next["id"],"input":"last"})).await;
    assert!(fixture.mock.calls.lock().iter().all(|(account, _, _)| account == "a"));
    assert_eq!(fixture.mock.calls.lock().last().unwrap().1["input"].as_array().unwrap().len(), 5);
    let logs = fixture.logs(3).await;
    for log in &logs {
        assert_eq!(log.session_id, logs[0].session_id);
        assert!(log.session_id.is_some());
        assert_eq!(log.session_source, Some("thread-id"));
        assert_eq!(log.routing_warning, None);
    }
    for log in &logs[1..] {
        assert_eq!(log.transport, "ws");
        assert_eq!(log.routing_reason, Some("session_reused"));
    }
}

#[tokio::test]
async fn native_websocket_idle_ping_is_answered_before_next_turn_and_reuses_connection() {
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    let mut peer = fixture.control_ws();
    let mut socket = fixture.socket("idle-ping").await;
    let first = turn(&mut socket, prompt()).await;
    client_ping(&mut socket).await;
    let ping = b"idle-upstream-ping".to_vec();
    peer.commands.send(WsCommand::Ping(ping.clone())).unwrap();
    assert_eq!(peer.observe().await, WsObservation::Pong(ping));
    // No new request has been sent while the provider waits for its Pong.
    assert_eq!(fixture.mock.calls.lock().len(), 1);
    turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"next"})).await;
    assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 1);
    let calls = fixture.mock.calls.lock();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(account, _, transport)| account == "a" && *transport == "ws"));
    assert_eq!(calls[1].1["previous_response_id"], first["id"]);
    assert_eq!(calls[1].1["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn native_websocket_idle_disconnects_reconnect_and_replay_tool_and_reasoning_history() {
    for command in [WsCommand::Close, WsCommand::Drop] {
        let fixture = Fixture::new(Routing::RoundRobin, true).await;
        fixture.mock.ws_rich_output.store(true, Ordering::Relaxed);
        let mut peer = fixture.control_ws();
        let mut socket = fixture.socket("idle-disconnect").await;
        let first = turn(&mut socket, prompt()).await;
        client_ping(&mut socket).await;
        let expected = if matches!(command, WsCommand::Close) { WsObservation::Closed } else { WsObservation::Dropped };
        peer.commands.send(command).unwrap();
        assert_eq!(peer.observe().await, expected);
        // Exercise the client side while the proxy services the dropped upstream.
        client_ping(&mut socket).await;
        let input = json!([
            {"type":"function_call_output", "call_id":"call_local", "output":"tool result"},
            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"continue"}]}
        ]);
        turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":input})).await;
        assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 2);
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls.len(), 2, "the turn must not be retried through HTTP");
        assert!(calls.iter().all(|(account, _, transport)| account == "a" && *transport == "ws"));
        let replay = &calls[1].1;
        assert!(replay["previous_response_id"].is_null());
        let items = replay["input"].as_array().unwrap();
        assert_eq!(items.len(), 6, "{items:?}");
        assert_eq!(items[0]["content"][0]["text"], "question");
        let reasoning = items.iter().find(|item| item["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["encrypted_content"], "local-encrypted-reasoning");
        assert_eq!(reasoning["summary"][0]["text"], "tool planning");
        let tool = items.iter().find(|item| item["type"] == "function_call").unwrap();
        assert_eq!(tool["call_id"], "call_local");
        assert_eq!(tool["name"], "lookup");
        assert_eq!(tool["arguments"], "{\"query\":\"local\"}");
        assert!(items.iter().any(|item| item["role"] == "assistant" && item["content"][0]["text"] == "a"));
        assert_eq!(&items[4..], input.as_array().unwrap());
        assert!(items.iter().all(|item| item["id"].is_null()));
    }
}

#[tokio::test]
async fn native_websocket_partial_stream_failure_is_reported_without_replay() {
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    let mut socket = fixture.socket("partial-stream").await;
    let first = turn(&mut socket, prompt()).await;
    *fixture.mock.ws_mode.lock() = WsMode::Partial;
    let body = json!({"type":"response.create", "model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"failed turn"});
    socket.send(tungstenite::Message::Text(body.to_string().into())).await.unwrap();
    assert_eq!(ws_event(&mut socket).await["type"], "response.created");
    let delta = ws_event(&mut socket).await;
    assert_eq!(delta["type"], "response.output_text.delta");
    assert_eq!(delta["delta"], "a");
    let failure = ws_event(&mut socket).await;
    assert_eq!(failure["type"], "error");
    assert_eq!(failure["status"], 502);
    assert_eq!(failure["error"]["type"], "upstream_error");
    assert_eq!(fixture.mock.calls.lock().len(), 2);
    assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 1);
    let logs = fixture.logs(2).await;
    assert_eq!(logs[1].status, 502);
    assert!(logs[1].error.as_ref().unwrap().contains("websocket"));
    *fixture.mock.ws_mode.lock() = WsMode::Normal;
    turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"retry explicitly"}))
        .await;
    assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 2);
    let calls = fixture.mock.calls.lock();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|(_, _, transport)| *transport == "ws"));
    assert!(calls[2].1["previous_response_id"].is_null());
    assert_eq!(calls[2].1["input"].as_array().unwrap().len(), 3);
    assert!(!calls[2].1["input"].to_string().contains("failed turn"));
}

#[tokio::test]
async fn native_websocket_idle_quota_events_update_routing_without_overwriting_newer_quota() {
    for newer_epoch in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, true).await;
        let mut peer = fixture.control_ws();
        let mut socket = fixture.socket("idle-quota").await;
        let first = turn(&mut socket, prompt()).await;
        let log = fixture.logs(1).await.pop().unwrap();
        let account = fixture.app.pool.all().into_iter().find(|a| a.id == log.routing_attempts[0].account_id).unwrap();
        if newer_epoch {
            account.state.lock().quota_epoch += 1;
        }
        peer.commands.send(WsCommand::Data(json!({
            "type":"codex.rate_limits", "rate_limits":{"primary":{"window_minutes":300, "used_percent":100.0, "reset_after_seconds":3600}}
        }))).unwrap();
        // A Pong proves the preceding rate-limit frame was serviced during idle.
        peer.commands.send(WsCommand::Ping(b"quota-barrier".to_vec())).unwrap();
        assert_eq!(peer.observe().await, WsObservation::Pong(b"quota-barrier".to_vec()));
        {
            let state = account.state.lock();
            if newer_epoch {
                assert!(state.quota.windows.is_empty());
            } else {
                assert_eq!(state.quota.windows[0].used, 100.0);
            }
        }
        turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"next"})).await;
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].2, "ws");
        if newer_epoch {
            assert_eq!(calls[1].0, "a");
            assert_eq!(calls[1].1["previous_response_id"], first["id"]);
        } else {
            assert_eq!(calls[1].0, "b");
            assert!(calls[1].1["previous_response_id"].is_null());
            assert_eq!(calls[1].1["input"].as_array().unwrap().len(), 3);
        }
    }
}

#[tokio::test]
async fn native_websocket_unexpected_idle_frames_retire_connection_without_rewriting_history() {
    for kind in ["error", "response.completed"] {
        let fixture = Fixture::new(Routing::RoundRobin, true).await;
        let mut peer = fixture.control_ws();
        let mut socket = fixture.socket("idle-unexpected").await;
        let first = turn(&mut socket, prompt()).await;
        peer.commands.send(WsCommand::Data(json!({"type":kind, "response":{"id":"unexpected"}}))).unwrap();
        assert_eq!(peer.observe().await, WsObservation::Dropped);
        turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"next"})).await;
        assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 2);
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(_, _, transport)| *transport == "ws"));
        assert!(calls[1].1["previous_response_id"].is_null());
        assert_eq!(calls[1].1["input"].as_array().unwrap().len(), 3);
        assert_eq!(calls[1].1["input"][0]["content"][0]["text"], "question");
    }
}

#[tokio::test]
async fn native_websocket_busy_idle_upstream_does_not_starve_next_turn() {
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    let mut peer = fixture.control_ws();
    let mut socket = fixture.socket("idle-flood").await;
    let first = turn(&mut socket, prompt()).await;
    peer.commands.send(WsCommand::Flood).unwrap();
    assert_eq!(peer.observe().await, WsObservation::Flooding);
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        turn(&mut socket, json!({"model":"gpt-6.1-sol", "previous_response_id":first["id"], "input":"next"})),
    )
    .await
    .expect("continuous upstream frames starved a ready client turn");
    let connections = fixture.mock.ws_connections.load(Ordering::Relaxed);
    assert!(matches!(connections, 1 | 2));
    let calls = fixture.mock.calls.lock();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(account, _, transport)| account == "a" && *transport == "ws"));
    // TCP may leave fewer than the drain limit ready. Both healthy reuse and retirement are valid.
    if connections == 1 {
        assert_eq!(calls[1].1["previous_response_id"], first["id"]);
        assert_eq!(calls[1].1["input"].as_array().unwrap().len(), 1);
    } else {
        assert!(calls[1].1["previous_response_id"].is_null());
        assert_eq!(calls[1].1["input"].as_array().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn native_websocket_dropped_connection_never_runs_a_turn_twice() {
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    *fixture.mock.ws_mode.lock() = WsMode::DropOnHandshake;
    let mut socket = fixture.socket("dropped-upstream").await;
    // TCP buffering decides whether the drop fails the send (macOS and Linux, usually) or is
    // only seen on the next read (Windows can buffer the whole message). A failed send never
    // reached upstream, so HTTP serves the turn once; after a complete send the turn may have
    // run, so it is reported and never replayed.
    let body = json!({"type":"response.create", "model":"gpt-6.1-sol", "input":"x".repeat(16 * 1024 * 1024)});
    socket.send(tungstenite::Message::Text(body.to_string().into())).await.unwrap();
    let served = loop {
        let event = ws_event(&mut socket).await;
        if event["type"] == "error" {
            assert_eq!(event["status"], 502, "{event}");
            assert_eq!(event["error"]["type"], "upstream_error");
            break false;
        }
        if event["type"] == "response.completed" {
            break true;
        }
    };
    assert_eq!(fixture.mock.ws_connections.load(Ordering::Relaxed), 1);
    {
        let calls = fixture.mock.calls.lock();
        if served {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].2, "http");
        } else {
            assert!(calls.is_empty(), "a completed send must not be replayed over HTTP");
        }
    }
    let logs = fixture.logs(1).await;
    assert_eq!(logs[0].status, if served { 200 } else { 502 });
    assert_eq!(logs[0].transport, "ws");
}

#[tokio::test]
async fn native_websocket_reconnect_replays_history_and_quota_failover_stays_on_replacement() {
    for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
        let fixture = Fixture::new(routing, true).await;
        let mut socket = fixture.socket("task").await;
        let first = turn(&mut socket, prompt()).await;
        let second =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"second"})).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().1["previous_response_id"], first["id"]);
        socket.close(None).await.unwrap();
        let mut socket = fixture.socket("task").await;
        let third =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":second["id"],"input":"third"})).await;
        assert!(fixture.mock.calls.lock().last().unwrap().1["previous_response_id"].is_null());
        assert_eq!(fixture.mock.calls.lock().last().unwrap().1["input"].as_array().unwrap().len(), 5);
        fixture.mock.mode.store(1, Ordering::Relaxed);
        let fourth =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":third["id"],"input":"fourth"})).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");
        fixture.recover();
        turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":fourth["id"],"input":"last"})).await;
        {
            let calls = fixture.mock.calls.lock();
            assert_eq!(calls.last().unwrap().0, "b");
            assert_eq!(calls.last().unwrap().2, "ws");
            assert_eq!(calls.last().unwrap().1["input"].as_array().unwrap().len(), 9);
        }
        // Quota exhaustion records the failed native turn and its successful HTTP fallback.
        let logs = fixture.logs(6).await;
        for log in &logs {
            assert_eq!(log.session_id, logs[0].session_id);
            assert!(log.session_id.is_some());
            assert_eq!(log.session_source, Some("thread-id"));
            assert_eq!(log.routing_strategy, routing);
            assert_eq!(log.routing_warning, None);
            assert_eq!(log.transport, "ws");
        }
        assert_eq!(logs[0].routing_reason, Some("new_session"));
        let migration = logs.iter().find(|log| log.routing_reason == Some("quota_exhausted")).unwrap();
        assert_eq!(migration.status, 200);
        assert_eq!(
            migration.routing_attempts[0].previous_account,
            Some(logs[0].routing_attempts[0].account_id.clone())
        );
        assert_eq!(logs.last().unwrap().routing_reason, Some("session_reused"));
    }
}

#[tokio::test]
async fn authentication_scopes_prevent_cross_client_pins_and_accept_query_keys() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses?key=client-one", fixture.proxy.url))
        .header("thread-id", "task")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.proxy.url))
        .bearer_auth("client-two")
        .header("thread-id", "task")
        .header("x-cliproxy-client-scope", crate::affinity::scope_for_key(Some("client-one")))
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "b");
}

#[tokio::test]
async fn compaction_uses_the_tasks_account_and_configuration_changes_do_not_move_it() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(Some("task"), prompt()).await.1;
    let mut cfg = fixture.cfg.clone();
    cfg.routing = Routing::LeastUsed;
    fixture.app.set_config(cfg);
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses/compact", fixture.proxy.url))
        .bearer_auth("client-one")
        .header("thread-id", "task")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), answer(&first));
    let logs = fixture.logs(2).await;
    assert_eq!(logs[1].session_id, logs[0].session_id);
    assert!(logs[1].session_id.is_some());
    assert_eq!(logs[1].session_source, Some("thread-id"));
    assert_eq!(logs[1].routing_strategy, Routing::LeastUsed);
    assert_eq!(logs[1].routing_reason, Some("session_reused"));
    assert_eq!(logs[1].routing_warning, None);
}

#[tokio::test]
async fn claude_metadata_pins_the_session_before_request_translation() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.claude_api_key = cfg.codex_api_key.clone();
    cfg.codex_api_key.clear();
    for key in &mut cfg.claude_api_key {
        key.base_url = Some(fixture.provider.url.clone());
    }
    fixture.app.set_config(cfg);
    for _ in 0..4 {
        let body = json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"question"}],"max_tokens":32,"metadata":{"user_id":json!({"device_id":"device","session_id":"claude-task"}).to_string()}});
        let response: Value = reqwest::Client::new()
            .post(format!("{}/v1/messages", fixture.proxy.url))
            .header("x-api-key", "client-one")
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["content"][0]["text"], "a");
    }
    assert!(fixture.mock.calls.lock().iter().all(|(account, _, _)| account == "a"));
}

#[tokio::test]
async fn ending_a_task_releases_its_assignment_and_missing_codex_history_is_explicit() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.proxy.url))
        .bearer_auth("client-one")
        .header("thread-id", "task")
        .header("x-cliproxy-session-end", "true")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "a");
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "b");
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    let (status, response) = fixture
        .request(
            Some("task"),
            json!({"model":"gpt-6.1-sol","input":"next","previous_response_id":"from-before-restart"}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"]["code"], "previous_response_not_found");
    assert!(fixture.mock.calls.lock().is_empty());
}

#[tokio::test]
async fn diagnostics_distinguish_agent_threads_and_missing_or_disabled_affinity() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let threads = ["private-parent-thread", "private-agent-one-thread", "private-agent-two-thread"];
    for thread in threads.into_iter().chain([threads[0]]) {
        assert_eq!(fixture.request(Some(thread), prompt()).await.0, 200);
    }
    let logs = fixture.logs(4).await;
    let sessions: std::collections::HashSet<_> = logs[..3].iter().map(|log| log.session_id.clone()).collect();
    assert_eq!(sessions.len(), 3);
    assert!(!sessions.contains(&None));
    assert_eq!(logs[0].session_id, logs[3].session_id);
    assert_eq!(logs[0].account, logs[3].account);
    assert_eq!(logs[3].routing_reason, Some("session_reused"));
    let serialized = serde_json::to_string(&logs).unwrap();
    assert!(threads.iter().all(|thread| !serialized.contains(*thread)));
    assert!(!serialized.contains("client-one"));

    for _ in 0..2 {
        assert_eq!(fixture.request(None, prompt()).await.0, 200);
    }
    let logs = fixture.logs(6).await;
    assert_ne!(logs[4].session_id, logs[5].session_id);
    assert_ne!(logs[4].account, logs[5].account);
    for log in &logs[4..] {
        assert_eq!(log.session_source, Some("generated_response"));
        assert_eq!(log.routing_reason, Some("missing_session"));
        assert_eq!(log.routing_warning, Some("missing_session_id"));
    }

    let mut cfg = fixture.cfg.clone();
    cfg.session_affinity = false;
    fixture.app.set_config(cfg);
    for _ in 0..2 {
        assert_eq!(fixture.request(Some(threads[0]), prompt()).await.0, 200);
    }
    let logs = fixture.logs(8).await;
    assert_ne!(logs[6].account, logs[7].account);
    for log in &logs[6..] {
        assert_eq!(log.session_id, logs[0].session_id);
        assert_eq!(log.session_source, Some("thread-id"));
        assert_eq!(log.routing_reason, Some("affinity_disabled"));
        assert_eq!(log.routing_warning, Some("affinity_disabled"));
        assert_eq!(log.routing_strategy, Routing::RoundRobin);
    }
}

#[tokio::test]
async fn websocket_without_client_identity_warns_that_affinity_is_connection_only() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, native).await;
        let mut socket = fixture.socket_with_task(None).await;
        turn(&mut socket, prompt()).await;
        turn(&mut socket, prompt()).await;
        socket.close(None).await.unwrap();
        let mut socket = fixture.socket_with_task(None).await;
        turn(&mut socket, prompt()).await;
        let logs = fixture.logs(3).await;
        assert_eq!(logs[0].session_id, logs[1].session_id);
        assert_eq!(logs[0].account, logs[1].account);
        assert_ne!(logs[0].session_id, logs[2].session_id);
        assert_ne!(logs[0].account, logs[2].account);
        for log in &logs {
            assert!(log.session_id.is_some());
            assert_eq!(log.session_source, Some("websocket_connection"));
            assert_eq!(log.routing_warning, Some("connection_only"));
        }
        assert_eq!(logs[0].routing_reason, Some("new_session"));
        assert_eq!(logs[1].routing_reason, Some("session_reused"));
        assert_eq!(logs[2].routing_reason, Some("new_session"));
    }
}

#[tokio::test]
/// Verify that websocket native selection preserves decision when api keys require http fallback.
async fn websocket_native_selection_preserves_decision_when_api_keys_require_http_fallback() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.codex_websockets = true;
    fixture.app.set_config(cfg);
    let mut socket = fixture.socket("fallback-task").await;
    turn(&mut socket, prompt()).await;
    turn(&mut socket, prompt()).await;
    let logs = fixture.logs(2).await;
    assert_eq!(logs[0].routing_reason, Some("new_session"));
    assert_eq!(logs[1].routing_reason, Some("session_reused"));
    let previous = logs[0].routing_attempts[0].account_id.clone();
    fixture.app.pool.get(&previous).unwrap().exhaust(
        "gpt-6.1-sol",
        chrono::Utc::now() + chrono::Duration::minutes(5),
        "mock observed quota exhaustion",
        fixture.app.pool.get(&previous).unwrap().quota_epoch(),
    );
    turn(&mut socket, prompt()).await;
    turn(&mut socket, prompt()).await;
    let logs = fixture.logs(4).await;
    assert_eq!(logs[2].routing_reason, Some("quota_exhausted"));
    assert_eq!(logs[2].routing_attempts[0].previous_account.as_deref(), Some(previous.as_str()));
    assert_ne!(logs[2].routing_attempts[0].account_id, previous);
    assert_eq!(logs[3].routing_reason, Some("session_reused"));
    assert!(logs[3].routing_attempts[0].previous_account.is_none());
    for log in &logs {
        assert_eq!(log.session_id, logs[0].session_id);
        assert_eq!(log.attempts, 1);
        assert_eq!(log.routing_attempts.len(), 1);
    }
    let calls = fixture.mock.calls.lock();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|(_, _, transport)| *transport == "http"));
}

#[tokio::test]
async fn websocket_turn_without_its_original_body_identifier_reports_connection_only() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, native).await;
        let mut socket = fixture.socket_with_task(None).await;
        let mut first = prompt();
        first["prompt_cache_key"] = "private-first-frame-session-key".into();
        turn(&mut socket, first).await;
        turn(&mut socket, prompt()).await;
        let logs = fixture.logs(2).await;
        assert_eq!(logs[0].session_source, Some("prompt_cache_key"));
        assert_eq!(logs[0].routing_warning, None);
        assert_eq!(logs[1].session_source, Some("websocket_connection"));
        assert_eq!(logs[1].routing_warning, Some("connection_only"));
        assert_eq!(logs[1].routing_reason, Some("session_reused"));
        assert_eq!(logs[0].session_id, logs[1].session_id);
        assert_eq!(logs[0].account, logs[1].account);
        assert!(!serde_json::to_string(&logs).unwrap().contains("private-first-frame-session-key"));
    }
}

#[tokio::test]
async fn translated_responses_preserve_cache_controls_and_reject_unsupported_prewarming() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.codex_api_key.clear();
    cfg.openai_compatibility = vec![crate::config::CompatEntry {
        name: "local-chat".into(),
        base_url: format!("{}/v1", fixture.provider.url),
        api_keys: vec!["a".into()],
        models: vec![crate::config::ModelAlias { name: "cache-compat-model".into(), alias: None }],
        ..Default::default()
    }];
    fixture.app.set_config(cfg);
    let mut body = json!({
        "model":"cache-compat-model",
        "prompt_cache_key":"private-cache-key",
        "prompt_cache_retention":"24h",
        "prompt_cache_options":{"mode":"explicit","ttl":"30m"},
        "input":[
            {"role":"system","content":[
                {"type":"input_text","text":"stable instructions","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"input_text","text":"variable instructions"}
            ]},
            {"role":"user","content":[
                {"type":"input_text","text":"stable context","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"input_text","text":"changing question"}
            ]}
        ]
    });
    let (status, response) = fixture.request(Some("cache-task"), body.clone()).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(answer(&response), "a");
    {
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls.len(), 1);
        let (_, sent, transport) = &calls[0];
        assert_eq!(*transport, "chat");
        for field in ["prompt_cache_key", "prompt_cache_options", "prompt_cache_retention"] {
            assert_eq!(sent[field], body[field]);
        }
        let messages = sent["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        for (message, original) in messages.iter().zip(body["input"].as_array().unwrap()) {
            assert_eq!(message["role"], original["role"]);
            assert_eq!(message["content"][0]["text"], original["content"][0]["text"]);
            assert_eq!(message["content"][0]["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
            assert_eq!(message["content"][1]["text"], original["content"][1]["text"]);
            assert!(message["content"][1]["prompt_cache_breakpoint"].is_null());
        }
    }
    body["prompt_cache_options"]["prewarm"] = true.into();
    let (status, response) = fixture.request(Some("cache-task"), body).await;
    assert_eq!(status, 400, "{response}");
    assert!(response["error"]["message"].as_str().unwrap().contains("prewarming"));
    assert_eq!(fixture.mock.calls.lock().len(), 1, "unsupported cache controls reached the upstream");
}
