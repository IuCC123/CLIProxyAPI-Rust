//! OpenAI-style media endpoints on top of whatever accounts can make images
//! or video: `/v1/images/generations`, `/v1/images/edits`, `/v1/videos/*`,
//! plus `/v1/responses/compact`.
//!
//! Images: ChatGPT (Codex) accounts through the Responses `image_generation`
//! tool, OpenAI / xAI / compatible keys natively, Vertex Imagen via `predict`,
//! and Gemini image models (AI Studio, Vertex, Antigravity) via generateContent.

use std::sync::Arc;

use axum::http::HeaderMap;
use chrono::{Duration, Utc};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::accounts::{Account, Credential, Provider};
use crate::formats;
use crate::ir::{Format, Usage};
use crate::proxy::{Tracker, error_message};
use crate::sse::SseDecoder;
use crate::state::App;
use crate::upstream::{self, Target};

const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";
const DEFAULT_VIDEO_MODEL: &str = "grok-imagine-video-1.5";
/// Codex runs the image tool from a small chat model.
const CODEX_IMAGE_HOST_MODEL: &str = "gpt-5.4-mini";

pub type Outcome = Result<Value, (u16, Value)>;

fn fail(status: u16, msg: impl Into<String>) -> (u16, Value) {
    (status, formats::error_body(Format::Chat, status, &msg.into()))
}

/// An image to edit, as a data URL or http(s) URL.
fn image_inputs(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["images", "image"] {
        match &body[key] {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => {
                for it in a {
                    if let Some(s) = it.as_str().or_else(|| it["image_url"].as_str()).or_else(|| it["url"].as_str()) {
                        out.push(s.to_string());
                    }
                }
            }
            Value::Object(_) => {
                if let Some(s) = body[key]["image_url"].as_str().or_else(|| body[key]["url"].as_str()) {
                    out.push(s.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

fn aspect_ratio(size: &str) -> Option<&'static str> {
    let (w, h) = size.split_once('x')?;
    let (w, h): (f64, f64) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    let r = w / h;
    [
        (1.0, "1:1"),
        (1.5, "3:2"),
        (2.0 / 3.0, "2:3"),
        (16.0 / 9.0, "16:9"),
        (9.0 / 16.0, "9:16"),
        (4.0 / 3.0, "4:3"),
        (0.75, "3:4"),
    ]
    .iter()
    .min_by(|a, b| (a.0 - r).abs().total_cmp(&(b.0 - r).abs()))
    .map(|(_, s)| *s)
}

fn images_response(results: Vec<(String, String)>, body: &Value, extra: Value) -> Value {
    let as_url = body["response_format"] == "url";
    let data: Vec<Value> =
        results
            .into_iter()
            .map(|(mime, b64)| {
                if as_url { json!({ "url": format!("data:{mime};base64,{b64}") }) } else { json!({ "b64_json": b64 }) }
            })
            .collect();
    let mut out = json!({ "created": Utc::now().timestamp(), "data": data });
    if let (Some(o), Value::Object(e)) = (out.as_object_mut(), extra) {
        o.extend(e);
    }
    out
}

fn creds(acct: &Account) -> (String, bool) {
    match &*acct.cred.read() {
        Credential::OAuth(o) => (o.access_token.clone(), true),
        Credential::ApiKey { key, .. } => (key.clone(), false),
    }
}

/// Base URL + auth headers for an OpenAI-shaped upstream, borrowed from the chat request builder.
fn openai_base(app: &App, acct: &Account, headers: &HeaderMap, model: &str) -> (String, Vec<(String, String)>) {
    let cfg = app.cfg();
    let t = Target {
        acct,
        cfg: &cfg,
        client_headers: headers,
        model,
        wire: if acct.provider == Provider::Compat { Format::Chat } else { Format::Responses },
        passthrough: true,
        stream: false,
        count_tokens: false,
    };
    let p = upstream::prepare(&t, json!({}));
    let base = p.url.trim_end_matches("/chat/completions").trim_end_matches("/responses").to_string();
    let headers = p.headers.into_iter().filter(|(k, _)| k != "accept" && k != "x-grok-conv-id").collect();
    (base, headers)
}

/// Track the request quota epoch before sending an authenticated upstream media operation.
async fn send(
    app: &App,
    acct: &Account,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<reqwest::Response, (u16, Value)> {
    let mut rb = app.http.for_account(acct).post(url);
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let epoch = acct.quota_epoch();
    let resp = rb.json(body).send().await.map_err(|e| fail(502, format!("upstream connection failed: {e}")))?;
    crate::quota::observe(acct, resp.headers(), epoch);
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let response_headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        if crate::proxy::quota_exhausted(acct, body["model"].as_str().unwrap_or_default(), status, &text) {
            crate::proxy::mark_quota_exhausted(
                acct,
                body["model"].as_str().unwrap_or_default(),
                &response_headers,
                &text,
                epoch,
            );
        }
        let error = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|v| v["error"].is_object())
            .unwrap_or_else(|| formats::error_body(Format::Chat, status, &error_message(&text)));
        return Err((status, error));
    }
    Ok(resp)
}

async fn read_json(resp: reqwest::Response) -> Outcome {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err((status, formats::error_body(Format::Chat, status, &error_message(&text))));
    }
    serde_json::from_str(&text).map_err(|_| fail(502, "upstream returned invalid JSON"))
}

// ---------------------------------------------------------------------- images

async fn codex_image(app: &App, acct: &Account, headers: &HeaderMap, body: &Value, edit: bool) -> Outcome {
    let mut tool = json!({
        "type": "image_generation",
        "action": if edit { "edit" } else { "generate" },
        "model": body["model"].as_str().unwrap_or(DEFAULT_IMAGE_MODEL),
    });
    for k in ["size", "quality", "background", "output_format", "moderation", "input_fidelity"] {
        if let Some(v) = body[k].as_str().filter(|v| !v.is_empty()) {
            tool[k] = v.into();
        }
    }
    for k in ["output_compression", "partial_images"] {
        if body[k].is_number() {
            tool[k] = body[k].clone();
        }
    }
    if let Some(mask) = body["mask"]["image_url"].as_str().or(body["mask"].as_str()) {
        tool["input_image_mask"] = json!({ "image_url": mask });
    }
    let mut content = vec![json!({ "type": "input_text", "text": body["prompt"].as_str().unwrap_or_default() })];
    for img in image_inputs(body) {
        content.push(json!({ "type": "input_image", "image_url": img }));
    }
    let req = json!({
        "model": CODEX_IMAGE_HOST_MODEL, "instructions": "", "stream": true, "store": false,
        "reasoning": { "effort": "medium", "summary": "auto" }, "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "tool_choice": { "type": "image_generation" }, "tools": [tool],
        "input": [{ "type": "message", "role": "user", "content": content }],
    });
    let (token, _) = creds(acct);
    let account_id = match &*acct.cred.read() {
        Credential::OAuth(o) => o.account_id.clone(),
        _ => None,
    };
    let base = match &*acct.cred.read() {
        Credential::OAuth(o) => o.base_url.clone(),
        _ => None,
    }
    .unwrap_or_else(|| upstream::CODEX_BACKEND.into());
    let mut h = upstream::codex_headers(headers, &token, account_id.as_deref(), true);
    h.push(("content-type".into(), "application/json".into()));
    h.push(("accept".into(), "text/event-stream".into()));
    let resp = send(app, acct, &format!("{}/responses", base.trim_end_matches('/')), &h, &req).await?;
    if !resp.status().is_success() {
        return read_json(resp).await;
    }
    let mut stream = resp.bytes_stream();
    let mut dec = SseDecoder::default();
    let mut images: Vec<(String, String)> = Vec::new();
    let mut meta = json!({});
    let mut usage = Value::Null;
    let mut handle = |data: &str, images: &mut Vec<(String, String)>| -> Option<(u16, Value)> {
        let v: Value = serde_json::from_str(data).ok()?;
        match v["type"].as_str()? {
            "response.output_item.done" if v["item"]["type"] == "image_generation_call" => {
                let it = &v["item"];
                if let Some(r) = it["result"].as_str().filter(|r| !r.is_empty()) {
                    let fmt = it["output_format"].as_str().unwrap_or("png");
                    images.push((format!("image/{fmt}"), r.to_string()));
                    for k in ["background", "output_format", "quality", "size"] {
                        if it[k].is_string() {
                            meta[k] = it[k].clone();
                        }
                    }
                    if let Some(p) = it["revised_prompt"].as_str() {
                        meta["revised_prompt"] = p.into();
                    }
                }
            }
            "response.completed" => usage = v["response"]["tool_usage"]["image_gen"].clone(),
            "response.failed" | "error" => {
                let e = if v["response"]["error"].is_object() { &v["response"]["error"] } else { &v["error"] };
                let msg = e["message"].as_str().unwrap_or("image generation failed").to_string();
                return Some(fail(502, msg));
            }
            _ => {}
        }
        None
    };
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| fail(502, format!("upstream stream error: {e}")))?;
        for ev in dec.push(&chunk) {
            if let Some(err) = handle(&ev.data, &mut images) {
                return Err(err);
            }
        }
    }
    for ev in dec.finish() {
        if let Some(err) = handle(&ev.data, &mut images) {
            return Err(err);
        }
    }
    if images.is_empty() {
        return Err(fail(502, "the upstream did not return an image"));
    }
    let revised = meta.as_object_mut().and_then(|m| m.remove("revised_prompt"));
    let mut out = images_response(images, body, meta);
    if let Some(p) = revised
        && let Some(d) = out["data"].as_array_mut()
    {
        d.iter_mut().for_each(|x| x["revised_prompt"] = p.clone());
    }
    if usage.is_object() {
        out["usage"] = usage;
    }
    Ok(out)
}

async fn imagen(app: &App, acct: &Account, headers: &HeaderMap, model: &str, body: &Value) -> Outcome {
    let cfg = app.cfg();
    let t = Target {
        acct,
        cfg: &cfg,
        client_headers: headers,
        model,
        wire: Format::Gemini,
        passthrough: true,
        stream: false,
        count_tokens: false,
    };
    let p = upstream::prepare(&t, json!({}));
    let url = p.url.replace(":generateContent", ":predict");
    let mut params = json!({ "sampleCount": body["n"].as_u64().unwrap_or(1).clamp(1, 4) });
    if let Some(r) = body["size"].as_str().and_then(aspect_ratio) {
        params["aspectRatio"] = r.into();
    }
    if let Some(n) = body["negative_prompt"].as_str() {
        params["negativePrompt"] = n.into();
    }
    let req = json!({ "instances": [{ "prompt": body["prompt"].as_str().unwrap_or_default() }], "parameters": params });
    let v = read_json(send(app, acct, &url, &p.headers, &req).await?).await?;
    let images: Vec<(String, String)> = v["predictions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let data = p["bytesBase64Encoded"].as_str()?;
            Some((p["mimeType"].as_str().unwrap_or("image/png").to_string(), data.to_string()))
        })
        .collect();
    if images.is_empty() {
        return Err(fail(502, "Imagen returned no images (the prompt may have been filtered)"));
    }
    Ok(images_response(images, body, json!({})))
}

async fn gemini_image(app: &App, acct: &Account, headers: &HeaderMap, model: &str, body: &Value) -> Outcome {
    let mut parts = vec![json!({ "text": body["prompt"].as_str().unwrap_or_default() })];
    for img in image_inputs(body) {
        match crate::ir::Image::from_url(&img) {
            crate::ir::Image::Base64 { mime, data } => {
                parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }))
            }
            crate::ir::Image::Url(u) => parts.push(json!({ "fileData": { "mimeType": "image/*", "fileUri": u } })),
        }
    }
    let mut gc = json!({ "responseModalities": ["TEXT", "IMAGE"], "candidateCount": 1 });
    if let Some(r) = body["size"].as_str().and_then(aspect_ratio) {
        gc["imageConfig"] = json!({ "aspectRatio": r });
    }
    let req = json!({ "contents": [{ "role": "user", "parts": parts }], "generationConfig": gc });
    let cfg = app.cfg();
    let t = Target {
        acct,
        cfg: &cfg,
        client_headers: headers,
        model,
        wire: Format::Gemini,
        passthrough: true,
        stream: false,
        count_tokens: false,
    };
    let n = body["n"].as_u64().unwrap_or(1).clamp(1, 4);
    let mut images = Vec::new();
    for _ in 0..n {
        let p = upstream::prepare(&t, req.clone());
        let v = crate::antigravity::unwrap(read_json(send(app, acct, &p.url, &p.headers, &p.body).await?).await?);
        for part in v["candidates"][0]["content"]["parts"].as_array().into_iter().flatten() {
            let inline = part.get("inlineData").or_else(|| part.get("inline_data"));
            if let Some(d) = inline.and_then(|i| i["data"].as_str()) {
                let mime = inline.and_then(|i| i["mimeType"].as_str()).unwrap_or("image/png");
                images.push((mime.to_string(), d.to_string()));
            }
        }
    }
    if images.is_empty() {
        return Err(fail(502, "the model answered without an image"));
    }
    Ok(images_response(images, body, json!({})))
}

async fn image_once(app: &App, acct: &Account, headers: &HeaderMap, model: &str, body: &Value, edit: bool) -> Outcome {
    let (_, oauth) = creds(acct);
    match acct.provider {
        Provider::Codex if oauth => codex_image(app, acct, headers, body, edit).await,
        Provider::Codex | Provider::Xai | Provider::Compat => {
            let (base, h) = openai_base(app, acct, headers, model);
            let mut req = body.clone();
            req["model"] = model.into();
            if let Some(o) = req.as_object_mut() {
                o.remove("stream");
            }
            let path = if edit { "images/edits" } else { "images/generations" };
            read_json(send(app, acct, &format!("{base}/{path}"), &h, &req).await?).await
        }
        Provider::Vertex if model.starts_with("imagen-") => imagen(app, acct, headers, model, body).await,
        Provider::Gemini | Provider::Vertex | Provider::Antigravity => {
            gemini_image(app, acct, headers, model, body).await
        }
        p => Err(fail(400, format!("{} accounts can't generate images", p.as_str()))),
    }
}

/// Shares coding-session assignments with generation and compaction calls.
async fn with_accounts<F, Fut>(
    app: &Arc<App>,
    model: &str,
    kind: &'static str,
    session: Option<crate::affinity::SessionIdentity>,
    mut op: F,
) -> Outcome
where
    F: FnMut(Arc<Account>, String) -> Fut,
    Fut: std::future::Future<Output = Outcome>,
{
    let cfg = app.cfg();
    let session_key = session.as_ref().map(|s| s.key.as_str());
    let _lease = session_key.map(|s| app.sessions.hold(s, cfg.session_affinity_idle_seconds));
    let (only, model) = app.pool.route(model);
    let model = app.pool.canonical(&model, only.as_ref());
    let mut tracker = Tracker::new(app, Format::Chat, false, kind, &model);
    tracker.session(session_key, session.as_ref().map(|s| s.source), &cfg);
    let mut tried: Vec<String> = Vec::new();
    let mut last: Option<(u16, Value)> = None;
    while tried.len() < cfg.request_retry.max(1) as usize {
        let selected = match app.sessions.pick_with_reason(&app.pool, &cfg, &model, session_key, &tried, only.as_ref())
        {
            Ok(pair) => pair,
            Err((status, msg)) => {
                let (s, b) = last.unwrap_or_else(|| fail(status, msg));
                tracker.finish(s, &Usage::default(), Some(error_message(&b.to_string())));
                return Err((s, b));
            }
        };
        tracker.selected(&selected);
        let (acct, upstream_model) = (selected.account, selected.model);
        tried.push(acct.id.clone());
        if let Err(e) = crate::oauth::ensure_ready(app, &acct).await {
            last = Some(fail(401, format!("token refresh failed: {e}")));
            continue;
        }
        let epoch = acct.quota_epoch();
        tracker.request_epoch(epoch);
        match op(acct.clone(), upstream_model).await {
            Ok(v) => {
                acct.record_ok();
                let usage = Usage {
                    input: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
                    output: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
                    ..Default::default()
                };
                tracker.finish(200, &usage, None);
                return Ok(v);
            }
            Err((status, body)) => {
                let msg = error_message(&body.to_string());
                if crate::proxy::quota_exhausted(&acct, &model, status, &body.to_string()) {
                    crate::proxy::mark_quota_exhausted(
                        &acct,
                        &model,
                        &reqwest::header::HeaderMap::new(),
                        &body.to_string(),
                        epoch,
                    );
                    last = Some((status, body));
                    continue;
                }
                match status {
                    429 => acct.cool(Some(&model), Utc::now() + Duration::seconds(60), &format!("429: {msg}")),
                    401 | 403 => acct.cool(None, Utc::now() + Duration::minutes(10), &format!("{status}: {msg}")),
                    400 | 404 | 413 | 422 => {
                        tracker.finish(status, &Usage::default(), Some(msg));
                        return Err((status, body));
                    }
                    _ => acct.state.lock().last_error = Some(format!("{status}: {msg}")),
                }
                last = Some((status, body));
            }
        }
    }
    let (s, b) = last.unwrap_or_else(|| fail(404, format!("no available account serves model `{model}`")));
    tracker.finish(s, &Usage::default(), Some(error_message(&b.to_string())));
    Err((s, b))
}

pub async fn images(app: Arc<App>, headers: HeaderMap, body: Value, edit: bool) -> Outcome {
    if body["prompt"].as_str().is_none_or(|p| p.trim().is_empty()) {
        return Err(fail(400, "`prompt` is required"));
    }
    if edit && image_inputs(&body).is_empty() {
        return Err(fail(400, "an `image` to edit is required"));
    }
    let model = body["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_IMAGE_MODEL).to_string();
    let app2 = app.clone();
    let session = crate::affinity::session_identity(&headers, &body);
    with_accounts(&app, &model, "images", session, move |acct, upstream_model| {
        let (app, headers, body) = (app2.clone(), headers.clone(), body.clone());
        async move { image_once(&app, &acct, &headers, &upstream_model, &body, edit).await }
    })
    .await
}

/// Turns a multipart image edit into the JSON shape (`images` as data URLs).
pub async fn multipart_to_json(mut form: axum::extract::Multipart) -> Result<Value, String> {
    use base64::Engine;
    let mut body = json!({});
    let mut images = Vec::new();
    while let Some(field) = form.next_field().await.map_err(|e| e.to_string())? {
        let name = field.name().unwrap_or_default().trim_end_matches("[]").to_string();
        let mime = field.content_type().map(String::from);
        let bytes = field.bytes().await.map_err(|e| e.to_string())?;
        match name.as_str() {
            "image" | "mask" => {
                let mime = mime.filter(|m| m.starts_with("image/")).unwrap_or_else(|| "image/png".into());
                let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
                if name == "mask" {
                    body["mask"] = json!({ "image_url": url });
                } else {
                    images.push(json!({ "image_url": url }));
                }
            }
            _ => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                body[name.as_str()] = match text.parse::<i64>() {
                    Ok(n) if name == "n" || name.ends_with("compression") || name == "partial_images" => n.into(),
                    _ => text.into(),
                };
            }
        }
    }
    body["images"] = images.into();
    Ok(body)
}

// ---------------------------------------------------------------------- videos

/// Remembers which account created a video so status polls reach it.
static VIDEOS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub async fn video_create(app: Arc<App>, headers: HeaderMap, body: Value, kind: &str) -> Outcome {
    let model = body["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_VIDEO_MODEL).to_string();
    let app2 = app.clone();
    let kind = kind.to_string();
    let session = crate::affinity::session_identity(&headers, &body);
    with_accounts(&app, &model, "video", session, move |acct, upstream_model| {
        let (app, headers, body, kind) = (app2.clone(), headers.clone(), body.clone(), kind.clone());
        async move {
            if acct.provider != Provider::Xai {
                return Err(fail(400, "video generation needs an xAI account"));
            }
            let (base, mut h) = openai_base(&app, &acct, &headers, &upstream_model);
            if let Some(k) = headers.get("x-idempotency-key").and_then(|v| v.to_str().ok()) {
                h.push(("x-idempotency-key".into(), k.to_string()));
            }
            let mut req = body;
            req["model"] = upstream_model.into();
            let v = read_json(send(&app, &acct, &format!("{base}/videos/{kind}"), &h, &req).await?).await?;
            if let Some(id) = v["request_id"].as_str().or(v["id"].as_str()) {
                let mut map = VIDEOS.lock();
                map.push((id.to_string(), acct.id.clone()));
                let excess = map.len().saturating_sub(500);
                map.drain(..excess);
            }
            Ok(v)
        }
    })
    .await
}

pub async fn video_status(app: Arc<App>, headers: HeaderMap, id: String) -> Outcome {
    let owner = VIDEOS.lock().iter().find(|(v, _)| *v == id).map(|(_, a)| a.clone());
    let candidates: Vec<Arc<Account>> = app
        .pool
        .all()
        .into_iter()
        .filter(|a| a.provider == Provider::Xai && owner.as_ref().is_none_or(|o| *o == a.id))
        .collect();
    let mut last = fail(404, "unknown video id");
    for acct in candidates {
        if crate::oauth::ensure_ready(&app, &acct).await.is_err() {
            continue;
        }
        let (base, h) = openai_base(&app, &acct, &headers, DEFAULT_VIDEO_MODEL);
        let mut rb = app.http.client(acct.proxy_url.as_deref()).get(format!("{base}/videos/{id}"));
        for (k, v) in &h {
            rb = rb.header(k.as_str(), v.as_str());
        }
        match rb.send().await {
            Ok(r) => match read_json(r).await {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            },
            Err(e) => last = fail(502, format!("upstream connection failed: {e}")),
        }
    }
    Err(last)
}

// --------------------------------------------------------------------- compact

/// `/v1/responses/compact`: server-side conversation compaction (Codex, xAI API).
pub async fn compact(app: Arc<App>, headers: HeaderMap, body: Value) -> Outcome {
    let Some(model) = body["model"].as_str().map(String::from) else { return Err(fail(400, "`model` is required")) };
    let (model, _) = crate::ir::split_model_suffix(&model);
    let app2 = app.clone();
    let session = crate::affinity::session_identity(&headers, &body);
    with_accounts(&app, &model, "http", session, move |acct, upstream_model| {
        let (app, headers, body) = (app2.clone(), headers.clone(), body.clone());
        async move {
            if !matches!(acct.provider, Provider::Codex | Provider::Xai) {
                return Err(fail(501, format!("{} has no compaction endpoint", acct.provider.as_str())));
            }
            let (_, oauth) = creds(&acct);
            let mut req = body;
            if acct.provider == Provider::Codex && oauth {
                upstream::sanitize_codex_body(&mut req, &upstream_model, false);
            } else {
                req["model"] = upstream_model.clone().into();
            }
            if let Some(o) = req.as_object_mut() {
                o.remove("stream");
            }
            let (mut base, h) = openai_base(&app, &acct, &headers, &upstream_model);
            if acct.provider == Provider::Xai && base == crate::device::xai::CLI_BASE {
                base = crate::device::xai::API_BASE.into();
            }
            read_json(send(&app, &acct, &format!("{base}/responses/compact"), &h, &req).await?).await
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_map_to_aspect_ratios() {
        assert_eq!(aspect_ratio("1024x1024"), Some("1:1"));
        assert_eq!(aspect_ratio("1536x1024"), Some("3:2"));
        assert_eq!(aspect_ratio("1024x1792"), Some("9:16"));
        assert_eq!(aspect_ratio("auto"), None);
    }

    #[test]
    fn edit_inputs_accept_every_shape() {
        let b = json!({ "image": "data:image/png;base64,AA", "images": [{ "image_url": "https://x/y.png" }, "data:image/jpeg;base64,BB"] });
        assert_eq!(image_inputs(&b).len(), 3);
    }
}
