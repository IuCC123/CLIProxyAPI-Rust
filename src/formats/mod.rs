//! Format translators. Each module knows how to:
//! * parse a client request into the IR (`parse_request`)
//! * build an upstream request from the IR (`build_request`)
//! * decode the upstream stream into IR events (`Parser`)
//! * render IR events back to its own wire format (`Renderer`, `render_full`)

pub mod chat;
pub mod claude;
pub mod gemini;
pub mod responses;

use std::borrow::Cow;

use serde_json::Value;

use crate::ir::{Aggregate, CacheTextBlock, Event, Format, Message, Part, Request, Role};
use crate::sse::SseEvent;

/// One outgoing stream frame. `event` is the SSE event name (also used as the
/// websocket message type for the Responses API).
#[derive(Debug, Clone)]
pub struct Frame {
    pub event: Option<Cow<'static, str>>,
    pub data: String,
}

impl Frame {
    pub fn data(data: impl Into<String>) -> Self {
        Self { event: None, data: data.into() }
    }
    pub fn named(event: &'static str, data: impl Into<String>) -> Self {
        Self { event: Some(Cow::Borrowed(event)), data: data.into() }
    }
    pub fn to_sse(&self) -> String {
        crate::sse::frame(self.event.as_deref(), &self.data)
    }
}

pub trait StreamParser: Send {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>);
}

pub trait StreamRenderer: Send {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>);
    fn finish(&mut self, out: &mut Vec<Frame>);
}

pub fn parse_request(format: Format, body: &Value) -> Result<Request, String> {
    match format {
        Format::Chat => chat::parse_request(body),
        Format::Claude => claude::parse_request(body),
        Format::Responses => responses::parse_request(body),
        Format::Gemini => gemini::parse_request(body),
    }
}

/// Keep compatible OpenAI cache controls, including explicit caller extensions,
/// when translating into either OpenAI wire format.
/// Provider-specific cache controls (such as Anthropic's TTLs) are not interchangeable.
///
/// Only prewarming fails a request: it asks for a different kind of request. Hints
/// that can't be carried over exactly are dropped, so the upstream falls back to its
/// default caching rather than the client losing its answer.
pub fn preserve_cache_hints(
    source: Format,
    target: Format,
    original: &Value,
    translated: &mut Value,
) -> Result<(), String> {
    if target != Format::Responses && original["prompt_cache_options"]["prewarm"] == true {
        return Err("Prompt cache prewarming is only supported by the Responses API".into());
    }
    if let Err(reason) = carry_cache_hints(source, target, original, translated) {
        tracing::warn!("dropping explicit prompt cache hints: {reason}");
        strip_breakpoints(translated);
        if let Some(body) = translated.as_object_mut() {
            body.remove("prompt_cache_options");
        }
        if matches!(target, Format::Chat | Format::Responses) {
            for field in ["prompt_cache_key", "prompt_cache_retention"] {
                if !original[field].is_null() {
                    translated[field] = original[field].clone();
                }
            }
        }
    }
    Ok(())
}

fn strip_breakpoints(body: &mut Value) {
    // get_mut, not indexing: indexing a missing key would insert a null field.
    for field in ["messages", "input"] {
        for item in body.get_mut(field).and_then(Value::as_array_mut).into_iter().flatten() {
            for part in ["content", "output"] {
                for block in item.get_mut(part).and_then(Value::as_array_mut).into_iter().flatten() {
                    if let Some(block) = block.as_object_mut() {
                        block.remove("prompt_cache_breakpoint");
                    }
                }
            }
        }
    }
}

fn carry_cache_hints(source: Format, target: Format, original: &Value, translated: &mut Value) -> Result<(), String> {
    let breakpoints = openai_breakpoint_count(source, original);
    if !matches!(target, Format::Chat | Format::Responses) {
        if breakpoints > 0 || original["prompt_cache_options"]["mode"] == "explicit" {
            return Err(
                "Explicit OpenAI prompt caching cannot be translated to this provider; use its native cache controls"
                    .into(),
            );
        }
        return Ok(());
    }
    if breakpoints != openai_breakpoint_count(target, translated) {
        return Err("This request's prompt cache breakpoints cannot be preserved during format translation; use the upstream's native API format".into());
    }
    if breakpoints > 0 {
        let field = if source == Format::Chat { "messages" } else { "input" };
        let mut conversation_started = false;
        let mut assistant_tools_seen = false;
        for item in original[field].as_array().into_iter().flatten() {
            if source == Format::Responses && target == Format::Chat {
                if matches!(item["type"].as_str(), Some("function_call" | "custom_tool_call")) {
                    assistant_tools_seen = true;
                } else if item["role"] == "user"
                    || matches!(item["type"].as_str(), Some("function_call_output" | "custom_tool_call_output"))
                {
                    assistant_tools_seen = false;
                } else if assistant_tools_seen
                    && item["role"] == "assistant"
                    && item["content"]
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().any(|b| !b["prompt_cache_breakpoint"].is_null()))
                {
                    return Err("A cache breakpoint after an assistant tool call requires the Responses API".into());
                }
            }
            if item["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    !matches!(
                        block["type"].as_str(),
                        Some("text" | "input_text" | "output_text" | "image_url" | "input_image")
                    )
                })
            }) {
                return Err(
                    "Cache breakpoints with unsupported content require the upstream's native API format".into()
                );
            }
            if matches!(item["role"].as_str(), Some("system" | "developer")) {
                if conversation_started {
                    return Err(
                        "Cache breakpoints with interleaved system messages require the upstream's native API format"
                            .into(),
                    );
                }
            } else {
                conversation_started = true;
            }
            // Chat tool messages support text only; moving an image into a
            // separate user message would move the cache boundary as well.
            if target == Format::Chat
                && item["output"].as_array().is_some_and(|blocks| blocks.iter().any(|b| b["type"] != "input_text"))
            {
                return Err("Cache breakpoints with non-text tool outputs require the Responses API".into());
            }
            if item["role"] == "assistant"
                && item["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|b| !matches!(b["type"].as_str(), Some("text" | "output_text")))
                })
            {
                return Err(
                    "Cache breakpoints with non-text assistant content require the upstream's native API format".into(),
                );
            }
        }
    }
    for field in ["prompt_cache_key", "prompt_cache_retention"] {
        if !original[field].is_null() {
            translated[field] = original[field].clone();
        }
    }
    if let Some(options) = original["prompt_cache_options"].as_object() {
        let mut compatible = serde_json::Map::new();
        for field in ["mode", "ttl", "prewarm"] {
            if field == "prewarm" && target == Format::Chat {
                continue;
            }
            if let Some(value) = options.get(field) {
                compatible.insert(field.into(), value.clone());
            }
        }
        translated["prompt_cache_options"] = Value::Object(compatible);
    }
    Ok(())
}

fn openai_breakpoint_count(format: Format, body: &Value) -> usize {
    let field = if matches!(format, Format::Chat | Format::Claude) { "messages" } else { "input" };
    body[field]
        .as_array()
        .into_iter()
        .flatten()
        .map(|message| {
            ["content", "output"]
                .iter()
                .map(|field| {
                    message[field]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|block| !block["prompt_cache_breakpoint"].is_null())
                        .count()
                })
                .sum::<usize>()
        })
        .sum()
}

pub(crate) fn parse_openai_system(content: &Value, req: &mut Request) {
    let text = text_of(content);
    if text.is_empty() {
        return;
    }
    if let Some(blocks) = content.as_array()
        && blocks.iter().any(|block| !block["prompt_cache_breakpoint"].is_null())
    {
        let blocks = blocks
            .iter()
            .filter_map(|block| {
                Some(CacheTextBlock {
                    text: block["text"].as_str()?.to_string(),
                    breakpoint: block["prompt_cache_breakpoint"]["mode"] == "explicit",
                })
            })
            .collect();
        req.system_cache_blocks.insert(req.system.len(), blocks);
    }
    req.system.push(text);
}

pub(crate) fn openai_system_blocks(req: &Request, index: usize, kind: &str) -> Value {
    use serde_json::json;
    match req.system_cache_blocks.get(&index) {
        Some(blocks) => Value::Array(
            blocks
                .iter()
                .map(|block| {
                    let mut value = json!({"type": kind, "text": block.text});
                    if block.breakpoint {
                        value["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
                    }
                    value
                })
                .collect(),
        ),
        None => json!([{"type": kind, "text": req.system[index]}]),
    }
}

pub(crate) fn mark_openai_breakpoint(content: &mut [Value]) {
    if let Some(last) = content.last_mut() {
        last["prompt_cache_breakpoint"] = serde_json::json!({"mode": "explicit"});
    }
}

pub(crate) fn has_openai_breakpoints(parts: &[Part]) -> bool {
    parts.iter().any(|part| match part {
        Part::CacheBreakpoint => true,
        Part::ToolResult { content, .. } => has_openai_breakpoints(content),
        _ => false,
    })
}

/// Preserve marked content boundaries while keeping parallel function calls in
/// one assistant turn, as required by Chat Completions tool-response ordering.
pub(crate) fn merge_openai_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(last) = out.last_mut()
            && last.role == Role::Assistant
            && message.role == Role::Assistant
            && last.parts.iter().chain(&message.parts).any(|p| matches!(p, Part::ToolCall { .. }))
        {
            last.parts.extend(message.parts);
        } else {
            out.push(message);
        }
    }
    out
}

pub fn parser(format: Format) -> Box<dyn StreamParser> {
    match format {
        Format::Chat => Box::new(chat::Parser::default()),
        Format::Claude => Box::new(claude::Parser::default()),
        Format::Responses => Box::new(responses::Parser::default()),
        Format::Gemini => Box::new(gemini::Parser::default()),
    }
}

pub fn renderer(format: Format, model: &str, req: &Request) -> Box<dyn StreamRenderer> {
    match format {
        Format::Chat => Box::new(chat::Renderer::new(model, req.include_usage)),
        Format::Claude => Box::new(claude::Renderer::new(model)),
        Format::Responses => Box::new(responses::Renderer::new(model, req)),
        Format::Gemini => Box::new(gemini::Renderer::new(model)),
    }
}

pub fn render_full(format: Format, agg: &Aggregate, model: &str, req: &Request) -> Value {
    match format {
        Format::Chat => chat::render_full(agg, model),
        Format::Claude => claude::render_full(agg, model),
        Format::Responses => responses::render_full(agg, model, req),
        Format::Gemini => gemini::render_full(agg, model),
    }
}

/// Converts a complete (non-streaming) upstream JSON body into events.
pub fn full_to_events(format: Format, body: &Value) -> Vec<Event> {
    match format {
        Format::Chat => chat::full_to_events(body),
        Format::Claude => claude::full_to_events(body),
        Format::Responses => responses::full_to_events(body),
        Format::Gemini => gemini::full_to_events(body),
    }
}

/// Error body in the client's dialect.
pub fn error_body(format: Format, status: u16, message: &str) -> Value {
    use serde_json::json;
    match format {
        Format::Claude => json!({
            "type": "error",
            "error": { "type": claude_error_type(status), "message": message }
        }),
        Format::Gemini => json!({
            "error": { "code": status, "message": message, "status": gemini_status(status) }
        }),
        Format::Chat | Format::Responses => json!({
            "error": { "message": message, "type": openai_error_type(status), "code": status }
        }),
    }
}

fn claude_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        415 => "invalid_request_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn openai_error_type(status: u16) -> &'static str {
    match status {
        400 | 404 | 413 | 415 => "invalid_request_error",
        401 | 403 => "authentication_error",
        429 => "rate_limit_exceeded",
        _ => "server_error",
    }
}

fn gemini_status(status: u16) -> &'static str {
    match status {
        400 | 413 | 415 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        503 => "UNAVAILABLE",
        _ => "INTERNAL",
    }
}

// ------------------------------------------------------------ small JSON helpers

pub(crate) fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                let t = it.get("text").and_then(Value::as_str).or_else(|| it.as_str());
                if let Some(t) = t {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn args_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "{}".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ir::{Event, Part, Sig};
    use crate::sse::SseEvent;

    fn sse(data: serde_json::Value) -> SseEvent {
        SseEvent { event: None, data: data.to_string() }
    }

    fn translated_cache_request(source: Format, target: Format, body: &Value) -> Result<Value, String> {
        let req = parse_request(source, body)?;
        let mut out = match target {
            Format::Chat => chat::build_request(&req, "gpt-6-astra"),
            Format::Responses => responses::build_request(
                &req,
                "gpt-6-astra",
                &responses::BuildOpts { chatgpt_backend: false, custom_tools: true, default_reasoning: false },
            ),
            _ => unreachable!(),
        };
        preserve_cache_hints(source, target, body, &mut out)?;
        Ok(out)
    }

    #[test]
    fn openai_cache_hints_and_exact_text_boundaries_survive_both_translations() {
        let body = json!({
            "model":"gpt-6-astra",
            "prompt_cache_key":"task", "prompt_cache_retention":"24h",
            "prompt_cache_options":{"mode":"explicit","ttl":"30m"},
            "messages":[
                {"role":"system","content":[
                    {"type":"text","text":"stable instructions","prompt_cache_breakpoint":{"mode":"explicit"}},
                    {"type":"text","text":"variable instructions"}
                ]},
                {"role":"user","content":[
                    {"type":"text","text":"stable context","prompt_cache_breakpoint":{"mode":"explicit"}},
                    {"type":"text","text":"changing question"}
                ]},
                {"role":"user","content":"another user turn"},
                {"role":"assistant","content":[
                    {"type":"text","text":"stable answer","prompt_cache_breakpoint":{"mode":"explicit"}},
                    {"type":"text","text":"answer suffix"}
                ]}
            ]
        });
        let responses = translated_cache_request(Format::Chat, Format::Responses, &body).unwrap();
        let chat = translated_cache_request(Format::Responses, Format::Chat, &responses).unwrap();
        for out in [&responses, &chat] {
            for field in ["prompt_cache_key", "prompt_cache_retention", "prompt_cache_options"] {
                assert_eq!(out[field], body[field]);
            }
        }
        assert_eq!(chat["messages"], body["messages"]);
        for index in [0, 1, 3] {
            assert_eq!(responses["input"][index]["content"][0]["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
            assert!(responses["input"][index]["content"][1]["prompt_cache_breakpoint"].is_null());
        }
        assert_eq!(responses["input"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn openai_cache_breakpoints_preserve_images_and_tool_text_boundaries() {
        let body = json!({"messages":[
            {"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.com/image.png"},"prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"text","text":"describe this"}
            ]},
            {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":[
                {"type":"text","text":"stable result","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"text","text":"volatile result"}
            ]}
        ]});
        let responses = translated_cache_request(Format::Chat, Format::Responses, &body).unwrap();
        assert_eq!(responses["input"][0]["content"][0]["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
        assert_eq!(responses["input"][2]["output"][0]["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
        assert!(responses["input"][2]["output"][1]["prompt_cache_breakpoint"].is_null());
        let chat = translated_cache_request(Format::Responses, Format::Chat, &responses).unwrap();
        assert_eq!(chat["messages"][0], body["messages"][0]);
        assert_eq!(chat["messages"][2], body["messages"][2]);
    }

    #[test]
    fn cache_translation_rejects_unsupported_boundaries_and_responses_only_prewarm() {
        let body = json!({"input":[{"role":"user","content":[
            {"type":"input_file","file_id":"file_1","prompt_cache_breakpoint":{"mode":"explicit"}}
        ]}],"prompt_cache_key":"task","prompt_cache_options":{"mode":"explicit"}});
        // Boundaries that can't be kept fall back to default caching; the request still goes.
        let out = translated_cache_request(Format::Responses, Format::Chat, &body).unwrap();
        assert!(out["prompt_cache_options"].is_null());
        assert_eq!(out["prompt_cache_key"], "task");
        assert!(!out.to_string().contains("prompt_cache_breakpoint"));
        let body = json!({"input":"context","prompt_cache_options":{"prewarm":true}});
        assert!(translated_cache_request(Format::Responses, Format::Chat, &body).unwrap_err().contains("prewarming"));
        let body = json!({"input":"context","prompt_cache_options":{"mode":"implicit","ttl":"30m","prewarm":false}});
        let out = translated_cache_request(Format::Responses, Format::Chat, &body).unwrap();
        assert_eq!(out["prompt_cache_options"], json!({"mode":"implicit","ttl":"30m"}));
        let body =
            json!({"messages":[{"role":"user","content":"question"}],"prompt_cache_options":{"mode":"explicit"}});
        let mut claude = json!({});
        preserve_cache_hints(Format::Chat, Format::Claude, &body, &mut claude).unwrap();
        assert_eq!(claude, json!({}));
        let mut out = json!({});
        preserve_cache_hints(
            Format::Claude,
            Format::Responses,
            &json!({"cache_control":{"type":"ephemeral","ttl":"1h"}}),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, json!({}));
    }

    #[test]
    fn cache_translation_keeps_intentional_explicit_mode_without_breakpoints() {
        let body = json!({"input":"uncached question","prompt_cache_options":{"mode":"explicit","ttl":"30m"}});
        let out = translated_cache_request(Format::Responses, Format::Chat, &body).unwrap();
        assert_eq!(out["prompt_cache_options"], body["prompt_cache_options"]);
    }

    #[test]
    fn cache_translation_preserves_explicit_openai_extensions_from_claude() {
        let original = json!({
            "messages":[{"role":"user","content":"question"}],
            "cache_control":{"type":"ephemeral","ttl":"1h"},
            "prompt_cache_key":"task", "prompt_cache_retention":"24h",
            "prompt_cache_options":{"mode":"implicit","ttl":"30m"}
        });
        for target in [Format::Chat, Format::Responses] {
            let mut out = json!({});
            preserve_cache_hints(Format::Claude, target, &original, &mut out).unwrap();
            for field in ["prompt_cache_key", "prompt_cache_retention", "prompt_cache_options"] {
                assert_eq!(out[field], original[field]);
            }
            assert!(out["cache_control"].is_null());
        }
    }

    #[test]
    fn cache_translation_drops_breakpoints_that_reordering_would_move() {
        // Why each request can't keep its breakpoints, and that it still goes out without them.
        let cases = [
            (
                json!({"input":[
                    {"role":"user","content":"question"},
                    {"role":"developer","content":[{"type":"input_text","text":"late instructions","prompt_cache_breakpoint":{"mode":"explicit"}}]}
                ]}),
                "interleaved",
            ),
            (
                json!({"input":[{"type":"function_call_output","call_id":"call_1","output":[
                    {"type":"input_image","image_url":"https://example.com/image.png"},
                    {"type":"input_text","text":"result","prompt_cache_breakpoint":{"mode":"explicit"}}
                ]}]}),
                "non-text tool",
            ),
            (
                json!({"input":[{"role":"user","content":[
                    {"type":"input_file","file_id":"file_1"},
                    {"type":"input_text","text":"cached suffix","prompt_cache_breakpoint":{"mode":"explicit"}}
                ]}]}),
                "unsupported content",
            ),
        ];
        for (original, reason) in cases {
            let req = parse_request(Format::Responses, &original).unwrap();
            let mut built = chat::build_request(&req, "gpt-6-astra");
            assert!(
                carry_cache_hints(Format::Responses, Format::Chat, &original, &mut built).unwrap_err().contains(reason)
            );
            let out = translated_cache_request(Format::Responses, Format::Chat, &original).unwrap();
            assert!(!out.to_string().contains("prompt_cache_breakpoint"), "{reason}");
        }
    }

    #[test]
    fn cache_translation_keeps_parallel_tool_calls_in_one_assistant_turn() {
        let original = json!({"input":[
            {"role":"user","content":[{"type":"input_text","text":"context","prompt_cache_breakpoint":{"mode":"explicit"}}]},
            {"type":"function_call","call_id":"a","name":"first","arguments":"{}"},
            {"type":"function_call","call_id":"b","name":"second","arguments":"{}"},
            {"type":"function_call_output","call_id":"a","output":"first result"},
            {"type":"function_call_output","call_id":"b","output":"second result"}
        ]});
        let out = translated_cache_request(Format::Responses, Format::Chat, &original).unwrap();
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["tool_call_id"], "a");
        assert_eq!(messages[3]["tool_call_id"], "b");
    }

    #[test]
    fn chat_to_claude_orders_tool_results_and_drops_unsigned_thinking() {
        let body = json!({
            "model": "claude-opus-5-5",
            "reasoning_effort": "high",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call.1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } }
                ]},
                { "role": "tool", "tool_call_id": "call.1", "content": "sunny" },
                { "role": "user", "content": "thanks" }
            ]
        });
        let req = parse_request(Format::Chat, &body).unwrap();
        let out = claude::build_request(&req, "claude-opus-5-5");
        assert_eq!(out["system"][0]["text"], "be brief");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // Tool ids are sanitized and the result leads the following user turn.
        assert_eq!(msgs[1]["content"][0]["id"], "call_1");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(msgs[2]["content"][1]["text"], "thanks");
        // Adaptive thinking accepts unsigned tool history without changing effort.
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert_eq!(out["output_config"]["effort"], "high");
    }

    #[test]
    fn effort_maps_to_adaptive_or_budget_thinking() {
        let body =
            json!({ "model": "x", "reasoning_effort": "low", "messages": [{ "role": "user", "content": "hi" }] });
        let req = parse_request(Format::Chat, &body).unwrap();
        let adaptive = claude::build_request(&req, "claude-sonnet-5-5");
        assert_eq!(adaptive["thinking"]["type"], "adaptive");
        assert_eq!(adaptive["output_config"]["effort"], "low");
        let budget = claude::build_request(&req, "claude-sonnet-4-5-20250929");
        assert_eq!(budget["thinking"]["type"], "enabled");
        assert_eq!(budget["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn claude_drops_sampling_and_forced_tools_the_model_rejects() {
        let body = json!({
            "model": "x", "temperature": 0.2, "parallel_tool_calls": false,
            "tool_choice": { "type": "function", "function": { "name": "read_file" } },
            "tools": [{ "type": "function", "function": { "name": "read_file", "parameters": { "type": "object" } } }],
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = parse_request(Format::Chat, &body).unwrap();
        for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"] {
            let out = claude::build_request(&req, model);
            assert!(out["temperature"].is_null(), "{model}");
            assert_eq!(out["tool_choice"], json!({ "type": "auto", "disable_parallel_tool_use": true }), "{model}");
        }
        // Opus 4.8 keeps forced tools but rejects sampling; Sonnet 4.6 accepts both.
        let opus = claude::build_request(&req, "claude-opus-4-8");
        assert!(opus["temperature"].is_null());
        assert_eq!(opus["tool_choice"]["name"], "read_file");
        let sonnet = claude::build_request(&req, "claude-sonnet-4-6");
        assert_eq!(sonnet["temperature"], 0.2);
        assert_eq!(sonnet["tool_choice"]["type"], "tool");
        assert_eq!(sonnet["tool_choice"]["disable_parallel_tool_use"], true);

        // With thinking on, a forced choice becomes auto but keeps the parallel-call limit.
        let mut thinking = body.clone();
        thinking["reasoning_effort"] = "high".into();
        let req = parse_request(Format::Chat, &thinking).unwrap();
        let out = claude::build_request(&req, "claude-sonnet-4-6");
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert!(out["temperature"].is_null());
        assert_eq!(out["tool_choice"], json!({ "type": "auto", "disable_parallel_tool_use": true }));
    }

    #[test]
    fn codex_stream_aggregates_reasoning_text_and_tools() {
        let mut p = parser(Format::Responses);
        let mut evs = Vec::new();
        for d in [
            json!({ "type": "response.created", "response": { "id": "r1", "model": "gpt-6-astra" } }),
            json!({ "type": "response.reasoning_summary_part.added", "output_index": 0 }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "think" }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "reasoning", "id": "rs", "encrypted_content": "ENC" } }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "hi" }),
            json!({ "type": "response.output_item.added", "output_index": 2, "item": { "type": "function_call", "call_id": "c1", "name": "f" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"a\":1}" }),
            json!({ "type": "response.output_item.done", "output_index": 2, "item": { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}" } }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 10, "input_tokens_details": { "cached_tokens": 4 }, "output_tokens": 5 } } }),
        ] {
            p.feed(&sse(d), &mut evs);
        }
        let mut agg = Aggregate::default();
        evs.iter().for_each(|e| agg.push(e));
        assert_eq!(agg.text(), "hi");
        assert_eq!(agg.reasoning_text(), "think");
        assert!(
            matches!(&agg.parts[0], Part::Reasoning { sig: Some(Sig::Codex { encrypted, .. }), .. } if encrypted == "ENC")
        );
        assert!(matches!(&agg.parts[2], Part::ToolCall { args, .. } if args == "{\"a\":1}"));
        assert_eq!((agg.usage.input, agg.usage.cache_read, agg.usage.output), (6, 4, 5));
        assert_eq!(agg.finish_reason(), crate::ir::Finish::ToolCalls);
    }

    #[test]
    fn codex_reasoning_round_trips_through_claude_clients() {
        // Render a Codex signature to a Claude client...
        let req = Request::default();
        let mut r = renderer(Format::Claude, "gpt-6-astra", &req);
        let mut frames = Vec::new();
        r.push(&Event::Reasoning("t".into()), &mut frames);
        r.push(&Event::ReasoningSig(Sig::Codex { id: None, encrypted: "ENC".into() }), &mut frames);
        r.finish(&mut frames);
        let sig = frames
            .iter()
            .filter_map(|f| serde_json::from_str::<serde_json::Value>(&f.data).ok())
            .find_map(|v| v["delta"]["signature"].as_str().map(String::from))
            .unwrap();
        // ...and parse it back when the client replays the conversation.
        let body = json!({ "model": "gpt-6-astra", "messages": [
            { "role": "user", "content": "q" },
            { "role": "assistant", "content": [{ "type": "thinking", "thinking": "t", "signature": sig }, { "type": "text", "text": "a" }] },
            { "role": "user", "content": "q2" }
        ]});
        let parsed = parse_request(Format::Claude, &body).unwrap();
        let out = responses::build_request(
            &parsed,
            "gpt-6-astra",
            &responses::BuildOpts { chatgpt_backend: true, custom_tools: true, default_reasoning: true },
        );
        let reasoning = out["input"].as_array().unwrap().iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["encrypted_content"], "ENC");
    }

    #[test]
    fn responses_renderer_emits_a_complete_sequence() {
        let req = Request::default();
        let mut r = renderer(Format::Responses, "m", &req);
        let mut frames = Vec::new();
        for ev in [
            Event::Text("hel".into()),
            Event::Text("lo".into()),
            Event::ToolStart { key: 0, id: "c1".into(), name: "f".into() },
            Event::ToolArgs { key: 0, delta: "{}".into() },
        ] {
            r.push(&ev, &mut frames);
        }
        r.finish(&mut frames);
        let kinds: Vec<_> = frames.iter().map(|f| f.event.clone().unwrap().into_owned()).collect();
        assert_eq!(kinds.first().unwrap(), "response.created");
        assert_eq!(kinds.last().unwrap(), "response.completed");
        let done: serde_json::Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
        let output = done["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["content"][0]["text"], "hello");
        assert_eq!(output[1]["call_id"], "c1");
        // Sequence numbers are strictly increasing.
        let seqs: Vec<u64> = frames
            .iter()
            .map(|f| serde_json::from_str::<serde_json::Value>(&f.data).unwrap()["sequence_number"].as_u64().unwrap())
            .collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn gemini_function_responses_get_names_and_signatures() {
        let body = json!({ "model": "x", "messages": [
            { "role": "user", "content": "q" },
            { "role": "assistant", "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "lookup", "arguments": "{}" } }] },
            { "role": "tool", "tool_call_id": "c1", "content": "42" }
        ]});
        let req = parse_request(Format::Chat, &body).unwrap();
        let out = gemini::build_request(&req, "gemini-3.8-flash");
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "skip_thought_signature_validator");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "lookup");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["result"], "42");
    }

    #[test]
    fn model_suffix_parsing() {
        let (m, r) = crate::ir::split_model_suffix("gpt-6-astra(high)");
        assert_eq!(m, "gpt-6-astra");
        assert_eq!(r.unwrap().effort.as_deref(), Some("high"));
        let (m, r) = crate::ir::split_model_suffix("claude-opus-4-1(8000)");
        assert_eq!(m, "claude-opus-4-1");
        assert_eq!(r.unwrap().budget, Some(8000));
        assert!(crate::ir::split_model_suffix("plain").1.is_none());
    }
}
