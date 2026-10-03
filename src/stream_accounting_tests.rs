//! Account for terminal events even when the client stops before transport EOF.

use super::*;
use crate::config::{Config, KeyEntry};
use crate::ir::Finish;
use axum::body::{Body, Bytes};
use axum::http::header;
use axum::routing::get;
use std::convert::Infallible;
use std::sync::atomic::Ordering;

struct Upstream(tokio::task::JoinHandle<()>);

impl Drop for Upstream {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn response(messages: Vec<Value>, done: bool, linger: bool) -> (reqwest::Response, Upstream) {
    let mut data: String = messages.into_iter().map(|v| format!("data: {v}\n\n")).collect();
    if done {
        data.push_str("data: [DONE]\n\n");
    }
    let router = axum::Router::new().route(
        "/",
        get(move || {
            let data = data.clone();
            async move {
                let body = Body::from_stream(async_stream::stream! {
                    yield Ok::<_, Infallible>(Bytes::from(data));
                    if linger { std::future::pending::<()>().await; }
                });
                ([(header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = Upstream(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    (reqwest::get(url).await.unwrap(), server)
}

fn tracker(format: Format) -> (Arc<App>, Tracker) {
    let cfg = Config {
        auth_dir: "/nonexistent".into(),
        codex_api_key: vec![KeyEntry { api_key: "mock-only".into(), ..Default::default() }],
        ..Default::default()
    };
    let app = App::new(cfg, "/nonexistent/config.yaml".into());
    let mut tracker = Tracker::new(&app, format, true, "http", "gpt-6.1-sol");
    tracker.attempt(&app.pool.all()[0]);
    (app, tracker)
}

fn recorded(app: &App, status: u16, input: u64, output: u64, cache: u64) -> RequestLog {
    let recent = app.stats.recent.lock();
    assert_eq!(recent.len(), 1);
    let log = recent[0].clone();
    assert_eq!(log.status, status);
    assert_eq!((log.input_tokens, log.output_tokens, log.cache_tokens), (input, output, cache));
    assert_eq!(app.stats.active.load(Ordering::Relaxed), 0);
    let totals = app.stats.totals.lock();
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.failed, u64::from(status >= 400));
    assert_eq!((totals.input_tokens, totals.output_tokens, totals.cache_tokens), (input, output, cache));
    let account = app.pool.all()[0].clone();
    let state = account.state.lock();
    assert_eq!(state.counters.requests, 1);
    assert_eq!(state.counters.failures, u64::from(status >= 400));
    assert_eq!(
        (state.counters.input_tokens, state.counters.output_tokens, state.counters.cache_tokens),
        (input, output, cache)
    );
    log
}

fn completed_messages(format: Format) -> Vec<Value> {
    match format {
        Format::Responses => vec![json!({"type":"response.completed", "response":{
            "id":"resp_test", "status":"completed", "output":[],
            "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":4}
        }})],
        Format::Claude => vec![
            json!({"type":"message_start", "message":{"id":"msg_test","usage":{"input_tokens":20,"cache_read_input_tokens":80}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ],
        Format::Chat => vec![
            json!({"id":"chat_test","choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]}),
            // Usage arrives after the finish reason, as in stream_options.include_usage.
            json!({"id":"chat_test","choices":[],"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":80},"completion_tokens":4}}),
        ],
        Format::Gemini => vec![json!({"candidates":[{"content":{"parts":[{"text":"OK"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":80,"candidatesTokenCount":4}})],
    }
}

fn terminal(format: Format, frame: &Frame) -> bool {
    if format == Format::Chat {
        return frame.data == "[DONE]";
    }
    let v: Value = serde_json::from_str(&frame.data).unwrap();
    match format {
        Format::Responses => v["type"] == "response.completed",
        Format::Claude => v["type"] == "message_stop",
        Format::Gemini => v["candidates"][0]["finishReason"] == "STOP",
        Format::Chat => unreachable!(),
    }
}

#[tokio::test]
async fn passthrough_completion_keeps_status_and_usage_before_eof() {
    for format in [Format::Responses, Format::Claude, Format::Chat, Format::Gemini] {
        let (app, tracker) = tracker(format);
        let (response, _server) = response(completed_messages(format), format == Format::Chat, true).await;
        let mut stream = passthrough_stream(response, format, tracker, false);
        loop {
            let frame = stream.next().await.unwrap();
            if terminal(format, &frame) {
                break;
            }
            // A Chat finish reason must not discard the following usage-only chunk.
            assert!(app.stats.recent.lock().is_empty());
        }
        // The server deliberately keeps the body open after its terminal event.
        drop(stream);
        assert!(recorded(&app, 200, 20, 4, 80).error.is_none());
    }
}

#[tokio::test]
async fn rendered_completion_keeps_status_and_usage_when_final_frame_is_last_poll() {
    for format in [Format::Responses, Format::Claude, Format::Chat, Format::Gemini] {
        let (app, tracker) = tracker(format);
        let events = futures::stream::iter([
            Event::Text("OK".into()),
            Event::Usage(Usage { input: 20, output: 4, cache_read: 80, ..Default::default() }),
            Event::Finish(Finish::Stop),
        ]);
        let req = Arc::new(Request { include_usage: true, ..Default::default() });
        let mut stream = render_stream(Box::pin(events), format, "gpt-6.1-sol".into(), req, tracker);
        while !terminal(format, &stream.next().await.unwrap()) {}
        drop(stream);
        assert!(recorded(&app, 200, 20, 4, 80).error.is_none());
    }
}

#[tokio::test]
async fn interrupted_passthrough_keeps_received_usage_and_stays_cancelled() {
    let (app, tracker) = tracker(Format::Claude);
    let messages = vec![json!({"type":"message_start","message":{"id":"msg_test","usage":{
        "input_tokens":20,"output_tokens":1,"cache_read_input_tokens":80,"cache_creation_input_tokens":5
    }}})];
    let (response, _server) = response(messages, false, true).await;
    let mut stream = passthrough_stream(response, Format::Claude, tracker, false);
    stream.next().await.unwrap();
    drop(stream);
    assert_eq!(recorded(&app, 499, 25, 1, 80).error.as_deref(), Some("client disconnected"));
}

#[tokio::test]
async fn interrupted_rendered_stream_keeps_received_usage_and_stays_cancelled() {
    let (app, tracker) = tracker(Format::Responses);
    let events = futures::stream::iter([
        Event::Usage(Usage { input: 20, output: 1, cache_read: 80, cache_write: 5, ..Default::default() }),
        Event::Text("partial".into()),
    ])
    .chain(futures::stream::pending());
    let mut stream = render_stream(Box::pin(events), Format::Responses, "gpt-6.1-sol".into(), Arc::default(), tracker);
    stream.next().await.unwrap();
    drop(stream);
    assert_eq!(recorded(&app, 499, 25, 1, 80).error.as_deref(), Some("client disconnected"));
}

#[tokio::test]
async fn upstream_errors_keep_their_status_and_usage_when_dropped() {
    for passthrough in [false, true] {
        let (app, tracker) = tracker(Format::Claude);
        let (response, _server) = response(
            vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":80}}}),
                json!({"type":"error","error":{"type":"overloaded_error","message":"mock overloaded"}}),
            ],
            false,
            true,
        )
        .await;
        let mut stream = if passthrough {
            passthrough_stream(response, Format::Claude, tracker, false)
        } else {
            let events = futures::stream::iter([
                Event::Usage(Usage { input: 20, cache_read: 80, ..Default::default() }),
                Event::Error { status: 529, message: "mock overloaded".into() },
            ])
            .chain(futures::stream::pending());
            render_stream(Box::pin(events), Format::Claude, "claude-sonnet-4-6".into(), Arc::default(), tracker)
        };
        loop {
            let frame = stream.next().await.unwrap();
            if frame.event.as_deref() == Some("error")
                || serde_json::from_str::<Value>(&frame.data).unwrap()["type"] == "error"
            {
                break;
            }
        }
        drop(stream);
        assert_eq!(recorded(&app, 529, 20, 0, 80).error.as_deref(), Some("mock overloaded"));
    }
}

#[tokio::test]
async fn draining_a_completed_stream_records_usage_once() {
    let (app, tracker) = tracker(Format::Responses);
    let (response, _server) = response(completed_messages(Format::Responses), false, false).await;
    let mut stream = passthrough_stream(response, Format::Responses, tracker, false);
    while stream.next().await.is_some() {}
    drop(stream);
    assert!(recorded(&app, 200, 20, 4, 80).error.is_none());
}
