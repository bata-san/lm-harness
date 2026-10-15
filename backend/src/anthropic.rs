//! Anthropic Messages API (`POST /v1/messages`, `POST /v1/messages/count_tokens`, and the
//! Anthropic shape of `GET /v1/models`) on top of the local OpenAI-compatible servers.
//!
//! Requests are translated to `/v1/chat/completions` of the running instance chosen by the same
//! rules as the transparent proxy (`proxy::pick_target`); responses (and SSE streams) are
//! translated back. Authentication is done by `auth::require_auth` (API key as `x-api-key` or
//! `Authorization: Bearer`).
//!
//! Known limits: `thinking`, `metadata`, `cache_control`, document blocks and server tools are
//! ignored; `stop_reason: "stop_sequence"` is only reported when the upstream says which stop
//! string matched (llama.cpp's OpenAI endpoint does not, so it reports `end_turn`);
//! `count_tokens` is a character-count estimate (about 4 chars per token, 256 tokens per image),
//! not a real tokenizer.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::privacy;
use crate::proxy::{self, Target};
use crate::state::AppState;

/// Images arrive base64 inside the JSON body.
const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

// ------------------------------------------------------------------ errors
/// Anthropic-style error response.
pub(crate) fn anth_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({ "type": "error", "error": { "type": kind, "message": message.into() } }))).into_response()
}

fn no_instance() -> Response {
    anth_error(StatusCode::SERVICE_UNAVAILABLE, "overloaded_error", "No model server is running. Start a model in LLM Harness first.")
}

/// Error type for an upstream HTTP status.
fn error_type_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// Convert a non-2xx upstream (OpenAI-style) response into an Anthropic error.
async fn upstream_failure(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    // The body may hold prompt text: the client gets a truncated message, the log gets the size only.
    tracing::warn!("{}", privacy::upstream_log_summary(status.as_u16(), text.len()));
    let msg = privacy::upstream_error_message(status.as_u16(), &text);
    let out_status = if status.is_client_error() || status.is_server_error() { status } else { StatusCode::BAD_GATEWAY };
    anth_error(out_status, error_type_for(out_status), msg)
}

// ------------------------------------------------------------------ request translation
fn text_of_blocks(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b {
                Value::String(s) => Some(s.as_str()),
                Value::Object(_) if b["type"] == "text" => b["text"].as_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn image_url(block: &Value) -> Option<String> {
    let src = block.get("source")?;
    match src["type"].as_str()? {
        "base64" => Some(format!("data:{};base64,{}", src["media_type"].as_str().unwrap_or("image/png"), src["data"].as_str()?)),
        "url" => Some(src["url"].as_str()?.to_owned()),
        _ => None,
    }
}

/// Text of a `tool_result` content (string, or blocks; images are replaced by a placeholder).
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn convert_user(content: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let blocks = match content {
        Value::String(s) => {
            out.push(json!({"role": "user", "content": s}));
            return Ok(());
        }
        Value::Array(a) => a,
        _ => return Err("messages.content: must be a string or an array of content blocks".into()),
    };
    let mut tool_msgs = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => parts.push(json!({"type": "text", "text": b["text"].as_str().unwrap_or("")})),
            Some("image") => match image_url(b) {
                Some(url) => parts.push(json!({"type": "image_url", "image_url": {"url": url}})),
                None => return Err("image block: unsupported or missing source".into()),
            },
            Some("tool_result") => {
                let id = b["tool_use_id"].as_str().ok_or("tool_result: tool_use_id is required")?;
                tool_msgs.push(json!({"role": "tool", "tool_call_id": id, "content": tool_result_text(&b["content"])}));
            }
            _ => {} // documents, unknown blocks: ignored
        }
    }
    // Tool results must directly follow the assistant's tool calls, so they go first.
    out.extend(tool_msgs);
    if parts.is_empty() {
        return Ok(());
    }
    if parts.iter().all(|p| p["type"] == "text") {
        let text = parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n");
        out.push(json!({"role": "user", "content": text}));
    } else {
        out.push(json!({"role": "user", "content": parts}));
    }
    Ok(())
}

fn convert_assistant(content: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let blocks = match content {
        Value::String(s) => {
            out.push(json!({"role": "assistant", "content": s}));
            return Ok(());
        }
        Value::Array(a) => a,
        _ => return Err("messages.content: must be a string or an array of content blocks".into()),
    };
    let mut texts: Vec<&str> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => texts.push(b["text"].as_str().unwrap_or("")),
            Some("tool_use") => {
                let id = b["id"].as_str().ok_or("tool_use: id is required")?;
                let name = b["name"].as_str().ok_or("tool_use: name is required")?;
                let args = serde_json::to_string(b.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_| "{}".into());
                calls.push(json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}));
            }
            _ => {} // thinking / redacted_thinking / unknown: dropped
        }
    }
    let text = texts.join("\n");
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert("content".into(), if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) });
    if !calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(calls));
    }
    out.push(Value::Object(msg));
    Ok(())
}

fn convert_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t.get("input_schema").is_some() || t["type"] == "custom")
                .filter_map(|t| {
                    let name = t["name"].as_str()?;
                    let schema = t.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(json!({"type": "function", "function": {"name": name, "description": t["description"].as_str().unwrap_or(""), "parameters": schema}}))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Anthropic Messages request -> OpenAI chat completions request. `Err` is a 400 message.
pub(crate) fn to_openai(req: &Value, upstream_model: &str) -> Result<Value, String> {
    let obj = req.as_object().ok_or("request body must be a JSON object")?;
    let messages = obj.get("messages").and_then(Value::as_array).ok_or("messages: Field required")?;
    if messages.is_empty() {
        return Err("messages: at least one message is required".into());
    }
    let max_tokens = obj.get("max_tokens").and_then(Value::as_u64).filter(|n| *n > 0).ok_or("max_tokens: Field required (a positive integer)")?;

    let mut out: Vec<Value> = Vec::new();
    if let Some(sys) = obj.get("system") {
        let t = text_of_blocks(sys);
        if !t.is_empty() {
            out.push(json!({"role": "system", "content": t}));
        }
    }
    for m in messages {
        let content = m.get("content").ok_or("messages.content: Field required")?;
        match m["role"].as_str() {
            Some("user") => convert_user(content, &mut out)?,
            Some("assistant") => convert_assistant(content, &mut out)?,
            _ => return Err("messages.role: must be \"user\" or \"assistant\"".into()),
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(upstream_model));
    body.insert("messages".into(), Value::Array(out));
    body.insert("max_tokens".into(), json!(max_tokens));
    for k in ["temperature", "top_p", "top_k"] {
        if let Some(v) = obj.get(k).filter(|v| v.is_number()) {
            body.insert(k.into(), v.clone());
        }
    }
    if let Some(s) = obj.get("stop_sequences").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        body.insert("stop".into(), Value::Array(s.clone()));
    }
    let tools = obj.get("tools").map(convert_tools).unwrap_or_default();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = obj.get("tool_choice") {
            match tc["type"].as_str() {
                Some("auto") => {
                    body.insert("tool_choice".into(), json!("auto"));
                }
                Some("any") => {
                    body.insert("tool_choice".into(), json!("required"));
                }
                Some("none") => {
                    body.insert("tool_choice".into(), json!("none"));
                }
                Some("tool") => {
                    let name = tc["name"].as_str().ok_or("tool_choice: name is required for type \"tool\"")?;
                    body.insert("tool_choice".into(), json!({"type": "function", "function": {"name": name}}));
                }
                _ => {}
            }
            if tc["disable_parallel_tool_use"] == true {
                body.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    if obj.get("stream").and_then(Value::as_bool) == Some(true) {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(body))
}

// ------------------------------------------------------------------ response translation
fn new_msg_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4().simple())
}

fn new_tool_id() -> String {
    format!("toolu_{}", uuid::Uuid::new_v4().simple())
}

/// `stop_word` is the stop string the upstream reports it matched (if it does).
fn map_stop_reason(finish: Option<&str>, saw_tool: bool, stop_word: Option<&str>) -> (&'static str, Option<String>) {
    if let Some(w) = stop_word.filter(|w| !w.is_empty()) {
        return ("stop_sequence", Some(w.to_owned()));
    }
    match finish {
        Some("length") => ("max_tokens", None),
        Some("tool_calls" | "function_call") => ("tool_use", None),
        _ if saw_tool => ("tool_use", None),
        _ => ("end_turn", None),
    }
}

fn stop_word_of(choice: &Value) -> Option<String> {
    choice.get("stopping_word").or_else(|| choice.get("stop_sequence")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// OpenAI chat completion -> Anthropic message.
pub(crate) fn from_openai(resp: &Value, model: &str) -> Value {
    let choice = resp.pointer("/choices/0").cloned().unwrap_or(Value::Null);
    let msg = &choice["message"];
    let mut content: Vec<Value> = Vec::new();
    if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
        content.push(json!({"type": "text", "text": t}));
    }
    let mut saw_tool = false;
    if let Some(calls) = msg["tool_calls"].as_array() {
        for c in calls {
            saw_tool = true;
            let args = c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
            let input = if args.trim().is_empty() { json!({}) } else { serde_json::from_str(args).unwrap_or_else(|_| json!({})) };
            let id = c["id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(new_tool_id);
            content.push(json!({"type": "tool_use", "id": id, "name": c.pointer("/function/name").and_then(Value::as_str).unwrap_or(""), "input": input}));
        }
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }
    let (stop_reason, stop_sequence) = map_stop_reason(choice["finish_reason"].as_str(), saw_tool, stop_word_of(&choice).as_deref());
    json!({
        "id": new_msg_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": stop_sequence,
        "usage": {
            "input_tokens": resp.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": resp.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        },
    })
}

// ------------------------------------------------------------------ streaming translation
type Event = (&'static str, Value);

enum Cur {
    None,
    Text(usize),
    /// Anthropic block index + the OpenAI `tool_calls[].index` it belongs to.
    Tool(usize, u64),
}

/// OpenAI chunk stream -> Anthropic event stream (pure state machine; no I/O).
pub(crate) struct StreamTranslator {
    model: String,
    id: String,
    input_estimate: u64,
    next_index: usize,
    cur: Cur,
    ping_sent: bool,
    finish: Option<String>,
    stop_word: Option<String>,
    usage_in: Option<u64>,
    usage_out: Option<u64>,
    out_chars: usize,
    saw_tool: bool,
    done: bool,
}

impl StreamTranslator {
    pub(crate) fn new(model: &str, input_estimate: u64) -> Self {
        Self {
            model: model.to_owned(),
            id: new_msg_id(),
            input_estimate,
            next_index: 0,
            cur: Cur::None,
            ping_sent: false,
            finish: None,
            stop_word: None,
            usage_in: None,
            usage_out: None,
            out_chars: 0,
            saw_tool: false,
            done: false,
        }
    }

    /// `message_start`.
    pub(crate) fn start(&self) -> Vec<Event> {
        vec![(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": self.input_estimate, "output_tokens": 1},
            }}),
        )]
    }

    fn close_current(&mut self, ev: &mut Vec<Event>) {
        let idx = match self.cur {
            Cur::Text(i) | Cur::Tool(i, _) => i,
            Cur::None => return,
        };
        ev.push(("content_block_stop", json!({"type": "content_block_stop", "index": idx})));
        self.cur = Cur::None;
    }

    fn open_block(&mut self, block: Value, tool_oai: Option<u64>, ev: &mut Vec<Event>) -> usize {
        self.close_current(ev);
        let idx = self.next_index;
        self.next_index += 1;
        ev.push(("content_block_start", json!({"type": "content_block_start", "index": idx, "content_block": block})));
        if !self.ping_sent {
            self.ping_sent = true;
            ev.push(("ping", json!({"type": "ping"})));
        }
        self.cur = match tool_oai {
            Some(o) => Cur::Tool(idx, o),
            None => Cur::Text(idx),
        };
        idx
    }

    /// Feed one parsed OpenAI `chat.completion.chunk`.
    pub(crate) fn feed(&mut self, chunk: &Value) -> Vec<Event> {
        let mut ev = Vec::new();
        if let Some(n) = chunk.pointer("/usage/prompt_tokens").and_then(Value::as_u64) {
            self.usage_in = Some(n);
        }
        if let Some(n) = chunk.pointer("/usage/completion_tokens").and_then(Value::as_u64) {
            self.usage_out = Some(n);
        }
        let Some(choice) = chunk.pointer("/choices/0") else { return ev };
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(f.to_owned());
        }
        if let Some(w) = stop_word_of(choice) {
            self.stop_word = Some(w);
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            let idx = match self.cur {
                Cur::Text(i) => i,
                _ => self.open_block(json!({"type": "text", "text": ""}), None, &mut ev),
            };
            self.out_chars += t.len();
            ev.push(("content_block_delta", json!({"type": "content_block_delta", "index": idx, "delta": {"type": "text_delta", "text": t}})));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for c in calls {
                self.saw_tool = true;
                let oai = c["index"].as_u64().unwrap_or(0);
                let idx = match self.cur {
                    Cur::Tool(i, o) if o == oai => i,
                    _ => {
                        let id = c["id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(new_tool_id);
                        let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
                        self.open_block(json!({"type": "tool_use", "id": id, "name": name, "input": {}}), Some(oai), &mut ev)
                    }
                };
                if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                    self.out_chars += a.len();
                    ev.push(("content_block_delta", json!({"type": "content_block_delta", "index": idx, "delta": {"type": "input_json_delta", "partial_json": a}})));
                }
            }
        }
        ev
    }

    /// End of upstream stream (`[DONE]` or EOF): closes the open block and emits
    /// `message_delta` + `message_stop`. Idempotent.
    pub(crate) fn finish(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        if self.done {
            return ev;
        }
        self.done = true;
        if self.next_index == 0 {
            // Anthropic messages always carry at least one content block.
            self.open_block(json!({"type": "text", "text": ""}), None, &mut ev);
        }
        self.close_current(&mut ev);
        let (stop_reason, stop_sequence) = map_stop_reason(self.finish.as_deref(), self.saw_tool, self.stop_word.as_deref());
        let out = self.usage_out.unwrap_or_else(|| (self.out_chars as u64).div_ceil(4));
        let mut usage = json!({"output_tokens": out});
        if let Some(i) = self.usage_in {
            usage["input_tokens"] = json!(i);
        }
        ev.push(("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": stop_sequence}, "usage": usage})));
        ev.push(("message_stop", json!({"type": "message_stop"})));
        ev
    }
}

fn sse_bytes(ev: &Event) -> Bytes {
    Bytes::from(format!("event: {}\ndata: {}\n\n", ev.0, ev.1))
}

/// Incremental parser for the upstream SSE stream: yields the payload of each `data:` line.
#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
            }
        }
        out
    }
}

/// Stream the upstream response as Anthropic SSE. A task owns the upstream connection and stops
/// (dropping it, which aborts the generation) as soon as the client goes away.
fn stream_response(upstream: reqwest::Response, model: String, input_estimate: u64) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(32);
    tokio::spawn(async move {
        let mut tr = StreamTranslator::new(&model, input_estimate);
        let mut parser = SseParser::default();
        let mut up = upstream.bytes_stream();
        for e in tr.start() {
            if tx.send(sse_bytes(&e)).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                item = up.next() => match item {
                    Some(Ok(bytes)) => {
                        for data in parser.feed(&bytes) {
                            let events = if data.trim() == "[DONE]" {
                                tr.finish()
                            } else if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
                                    let msg = privacy::truncate_chars(msg, privacy::ECHO_MAX_CHARS);
                                    let e = ("error", json!({"type": "error", "error": {"type": "api_error", "message": msg}}));
                                    let _ = tx.send(sse_bytes(&e)).await;
                                    return;
                                }
                                tr.feed(&v)
                            } else {
                                Vec::new()
                            };
                            for e in events {
                                if tx.send(sse_bytes(&e)).await.is_err() {
                                    return;
                                }
                            }
                            if tr.done {
                                return;
                            }
                        }
                    }
                    Some(Err(err)) => {
                        let e = ("error", json!({"type": "error", "error": {"type": "api_error", "message": format!("upstream stream failed: {err}")}}));
                        let _ = tx.send(sse_bytes(&e)).await;
                        return;
                    }
                    None => {
                        for e in tr.finish() {
                            if tx.send(sse_bytes(&e)).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                },
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx)) });
    let mut resp = Response::new(Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    h.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    h.insert("x-accel-buffering", "no".parse().unwrap());
    resp
}

// ------------------------------------------------------------------ token estimate
/// Rough prompt size: characters of all text in the request / 4 (+256 tokens per image).
pub(crate) fn estimate_tokens(req: &Value) -> u64 {
    fn chars(v: &Value) -> u64 {
        match v {
            Value::String(s) => s.chars().count() as u64,
            Value::Array(a) => a.iter().map(chars).sum(),
            Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("image") => 1024,
            Value::Object(o) => o.iter().filter(|(k, _)| !matches!(k.as_str(), "type" | "cache_control")).map(|(k, v)| k.len() as u64 + chars(v)).sum(),
            _ => 0,
        }
    }
    let mut total = 0;
    for k in ["system", "messages", "tools"] {
        if let Some(v) = req.get(k) {
            total += chars(v);
        }
    }
    total.div_ceil(4)
}

// ------------------------------------------------------------------ handlers
fn parse_body(body: &[u8]) -> Result<Value, Box<Response>> {
    serde_json::from_slice::<Value>(body).map_err(|e| Box::new(anth_error(StatusCode::BAD_REQUEST, "invalid_request_error", format!("Request body is not valid JSON: {e}"))))
}

async fn messages(State(state): State<AppState>, body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let requested = req.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
    if requested.trim().is_empty() {
        return anth_error(StatusCode::BAD_REQUEST, "invalid_request_error", "model: Field required");
    }
    let Some(target) = proxy::pick_target(&state, Some(&requested)) else {
        return no_instance();
    };
    run_messages(&target, &req, &requested).await
}

/// Translate, forward to `target` and translate the answer back.
pub(crate) async fn run_messages(target: &Target, req: &Value, requested: &str) -> Response {
    let oai = match to_openai(req, &proxy::upstream_model(target, requested)) {
        Ok(b) => b,
        Err(msg) => return anth_error(StatusCode::BAD_REQUEST, "invalid_request_error", msg),
    };
    let stream = oai["stream"] == true;
    let upstream = match proxy::post_json(target, "/v1/chat/completions", &oai).await {
        Ok(r) => r,
        Err((status, _, msg)) => {
            let kind = if status == StatusCode::SERVICE_UNAVAILABLE { "overloaded_error" } else { "api_error" };
            return anth_error(status, kind, msg);
        }
    };
    if !upstream.status().is_success() {
        return upstream_failure(upstream).await;
    }
    if stream {
        return stream_response(upstream, requested.to_owned(), estimate_tokens(req));
    }
    match upstream.json::<Value>().await {
        Ok(v) => Json(from_openai(&v, requested)).into_response(),
        Err(e) => anth_error(StatusCode::BAD_GATEWAY, "api_error", format!("Model server returned an unreadable response: {e}")),
    }
}

async fn count_tokens(body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !req.get("messages").is_some_and(Value::is_array) {
        return anth_error(StatusCode::BAD_REQUEST, "invalid_request_error", "messages: Field required");
    }
    Json(json!({ "input_tokens": estimate_tokens(&req) })).into_response()
}

/// `GET /v1/models` in Anthropic shape (used by `proxy::list_models` when `anthropic-version` is set).
pub(crate) fn models_response(found: &[Target]) -> Response {
    let mut seen = std::collections::HashSet::new();
    let data: Vec<Value> = found
        .iter()
        .filter(|t| seen.insert(t.model.clone()))
        .map(|t| {
            let created = chrono::DateTime::from_timestamp(t.started_at as i64, 0).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            json!({ "type": "model", "id": t.model, "display_name": t.name_or_model(), "created_at": created })
        })
        .collect();
    let first = data.first().map(|d| d["id"].clone()).unwrap_or(Value::Null);
    let last = data.last().map(|d| d["id"].clone()).unwrap_or(Value::Null);
    Json(json!({ "data": data, "has_more": false, "first_id": first, "last_id": last })).into_response()
}

impl Target {
    /// Human label for model lists: the model's file stem.
    fn name_or_model(&self) -> String {
        let base = self.model.rsplit(['/', '\\']).next().unwrap_or(&self.model);
        base.strip_suffix(".gguf").unwrap_or(base).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post as axum_post;

    fn tr(req: Value) -> Value {
        to_openai(&req, "up-model").unwrap()
    }

    #[test]
    fn text_system_and_sampling() {
        let o = tr(json!({
            "model": "claude-x", "max_tokens": 64, "temperature": 0.5, "top_p": 0.9, "top_k": 40,
            "stop_sequences": ["END"], "thinking": {"type": "enabled", "budget_tokens": 10}, "metadata": {"user_id": "u"},
            "system": [{"type": "text", "text": "A"}, {"type": "text", "text": "B"}],
            "messages": [{"role": "user", "content": "hi"}, {"role": "assistant", "content": [{"type": "text", "text": "yo"}]}, {"role": "user", "content": [{"type": "text", "text": "x"}, {"type": "text", "text": "y"}]}],
        }));
        assert_eq!(o["model"], "up-model");
        assert_eq!(o["max_tokens"], 64);
        assert_eq!(o["top_k"], 40);
        assert_eq!(o["stop"], json!(["END"]));
        assert!(o.get("thinking").is_none() && o.get("stream").is_none());
        let m = o["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "A\n\nB"}));
        assert_eq!(m[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(m[2], json!({"role": "assistant", "content": "yo"}));
        assert_eq!(m[3], json!({"role": "user", "content": "x\ny"}));
        let o = tr(json!({"max_tokens": 1, "system": "S", "messages": [{"role": "user", "content": "q"}]}));
        assert_eq!(o["messages"][0]["content"], "S");
    }

    #[test]
    fn validation_errors() {
        assert!(to_openai(&json!({"messages": [{"role": "user", "content": "x"}]}), "m").unwrap_err().contains("max_tokens"));
        assert!(to_openai(&json!({"max_tokens": 5}), "m").unwrap_err().contains("messages"));
        assert!(to_openai(&json!({"max_tokens": 5, "messages": [{"role": "system", "content": "x"}]}), "m").is_err());
    }

    #[test]
    fn images_become_data_uris() {
        let o = tr(json!({"max_tokens": 5, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}},
            {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}},
        ]}]}));
        let c = &o["messages"][0]["content"];
        assert_eq!(c[0], json!({"type": "text", "text": "what?"}));
        assert_eq!(c[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
        assert_eq!(c[2]["image_url"]["url"], "https://x/y.png");
    }

    #[test]
    fn tools_and_tool_results() {
        let o = tr(json!({
            "max_tokens": 5,
            "tools": [{"name": "get_w", "description": "d", "input_schema": {"type": "object", "properties": {"c": {"type": "string"}}}}],
            "tool_choice": {"type": "tool", "name": "get_w", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}, {"type": "tool_use", "id": "toolu_1", "name": "get_w", "input": {"c": "Tokyo"}}]},
                {"role": "user", "content": [{"type": "text", "text": "thanks"}, {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "sunny"}]}]},
            ],
        }));
        assert_eq!(o["tools"][0]["function"]["name"], "get_w");
        assert_eq!(o["tools"][0]["function"]["parameters"]["properties"]["c"]["type"], "string");
        assert_eq!(o["tool_choice"], json!({"type": "function", "function": {"name": "get_w"}}));
        assert_eq!(o["parallel_tool_calls"], false);
        let m = o["messages"].as_array().unwrap();
        assert_eq!(m[1]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(m[1]["tool_calls"][0]["function"]["arguments"], "{\"c\":\"Tokyo\"}");
        assert_eq!(m[1]["content"], "ok");
        // tool result comes before the user's text
        assert_eq!(m[2], json!({"role": "tool", "tool_call_id": "toolu_1", "content": "sunny"}));
        assert_eq!(m[3], json!({"role": "user", "content": "thanks"}));

        for (tc, want) in [(json!({"type": "any"}), json!("required")), (json!({"type": "auto"}), json!("auto")), (json!({"type": "none"}), json!("none"))] {
            let o = tr(json!({"max_tokens": 5, "tools": [{"name": "t", "input_schema": {}}], "tool_choice": tc, "messages": [{"role": "user", "content": "x"}]}));
            assert_eq!(o["tool_choice"], want);
        }
        // assistant with only a tool call -> null content
        let o = tr(json!({"max_tokens": 5, "messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "n", "input": {}}]}]}));
        assert!(o["messages"][0]["content"].is_null());
    }

    #[test]
    fn response_text_tool_and_stop_reasons() {
        let r = from_openai(&json!({"choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 7, "completion_tokens": 3}}), "claude-x");
        assert_eq!(r["type"], "message");
        assert_eq!(r["role"], "assistant");
        assert_eq!(r["model"], "claude-x");
        assert!(r["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(r["content"], json!([{"type": "text", "text": "hello"}]));
        assert_eq!(r["stop_reason"], "end_turn");
        assert!(r["stop_sequence"].is_null());
        assert_eq!(r["usage"], json!({"input_tokens": 7, "output_tokens": 3}));

        let r = from_openai(&json!({"choices": [{"message": {"content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]}, "finish_reason": "tool_calls"}]}), "m");
        assert_eq!(r["content"], json!([{"type": "tool_use", "id": "call_1", "name": "f", "input": {"a": 1}}]));
        assert_eq!(r["stop_reason"], "tool_use");

        let r = from_openai(&json!({"choices": [{"message": {"content": "cut"}, "finish_reason": "length"}]}), "m");
        assert_eq!(r["stop_reason"], "max_tokens");
        let r = from_openai(&json!({"choices": [{"message": {"content": "a"}, "finish_reason": "stop", "stopping_word": "END"}]}), "m");
        assert_eq!(r["stop_reason"], "stop_sequence");
        assert_eq!(r["stop_sequence"], "END");
        // empty completion still has one block
        let r = from_openai(&json!({"choices": [{"message": {"content": ""}, "finish_reason": "stop"}]}), "m");
        assert_eq!(r["content"], json!([{"type": "text", "text": ""}]));
    }

    #[test]
    fn token_estimate() {
        let n = estimate_tokens(&json!({"messages": [{"role": "user", "content": "x".repeat(400)}]}));
        assert!((100..=110).contains(&n), "{n}");
        assert!(estimate_tokens(&json!({"messages": [{"role": "user", "content": [{"type": "image", "source": {"type": "base64", "data": "A".repeat(100000)}}]}]})) < 400);
    }

    #[test]
    fn sse_parser_handles_split_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: {\"a\"").is_empty());
        let out = p.feed(b":1}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(out, vec!["{\"a\":1}", "[DONE]"]);
    }

    fn names(ev: &[Event]) -> Vec<&'static str> {
        ev.iter().map(|e| e.0).collect()
    }

    #[test]
    fn translator_text_then_tools_event_order() {
        let mut t = StreamTranslator::new("m", 5);
        let mut all = t.start();
        for c in [
            json!({"choices": [{"delta": {"role": "assistant", "content": ""}}]}),
            json!({"choices": [{"delta": {"content": "Hel"}}]}),
            json!({"choices": [{"delta": {"content": "lo"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "f", "arguments": ""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 4}}),
        ] {
            all.extend(t.feed(&c));
        }
        all.extend(t.finish());
        assert_eq!(
            names(&all),
            ["message_start", "content_block_start", "ping", "content_block_delta", "content_block_delta", "content_block_stop", "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(all[3].1["delta"], json!({"type": "text_delta", "text": "Hel"}));
        assert_eq!(all[6].1["content_block"], json!({"type": "tool_use", "id": "c1", "name": "f", "input": {}}));
        assert_eq!(all[6].1["index"], 1);
        assert_eq!(all[7].1["delta"], json!({"type": "input_json_delta", "partial_json": "{\"a\":"}));
        assert_eq!(all[10].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(all[10].1["usage"], json!({"output_tokens": 4, "input_tokens": 9}));
        assert!(t.finish().is_empty());
    }

    #[test]
    fn translator_empty_stream_still_well_formed() {
        let mut t = StreamTranslator::new("m", 1);
        let mut all = t.start();
        all.extend(t.finish());
        assert_eq!(names(&all), ["message_start", "content_block_start", "ping", "content_block_stop", "message_delta", "message_stop"]);
        assert_eq!(all[4].1["delta"]["stop_reason"], "end_turn");
    }

    async fn fake_upstream(status: u16, content_type: &'static str, body: String) -> Target {
        let app = Router::new().route(
            "/v1/chat/completions",
            axum_post(move |Json(req): Json<Value>| {
                let body = body.clone();
                async move {
                    // echo what we were sent in a header so tests can inspect the translation
                    let seen = req.to_string().replace('\n', " ");
                    Response::builder().status(status).header("content-type", content_type).header("x-seen", seen).body(Body::from(body)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Target { model: "org/m/m.gguf".into(), name: "m".into(), runtime: "llama.cpp".into(), ready: true, base: format!("http://{addr}"), served: "org/m/m.gguf".into(), started_at: 0.0 }
    }

    fn parse_sse(text: &str) -> Vec<(String, Value)> {
        text.split("\n\n")
            .filter(|b| !b.trim().is_empty())
            .map(|b| {
                let mut lines = b.lines();
                let ev = lines.next().unwrap().strip_prefix("event: ").unwrap().to_owned();
                let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
                (ev, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    #[tokio::test]
    async fn streaming_through_fake_upstream() {
        let chunks = [
            json!({"choices": [{"delta": {"role": "assistant"}}]}),
            json!({"choices": [{"delta": {"content": "Hi"}}]}),
            json!({"choices": [{"delta": {"content": "!"}, "finish_reason": "length"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
        ];
        let mut body = String::new();
        for c in chunks {
            body.push_str(&format!("data: {c}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        let target = fake_upstream(200, "text/event-stream", body).await;
        let req = json!({"model": "claude-x", "max_tokens": 10, "stream": true, "messages": [{"role": "user", "content": "hello"}]});
        let resp = run_messages(&target, &req, "claude-x").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let ev = parse_sse(std::str::from_utf8(&bytes).unwrap());
        let order: Vec<&str> = ev.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(order, ["message_start", "content_block_start", "ping", "content_block_delta", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]);
        assert_eq!(ev[0].1["message"]["model"], "claude-x");
        assert_eq!(ev[3].1["delta"]["text"], "Hi");
        assert_eq!(ev[6].1["delta"]["stop_reason"], "max_tokens");
        assert_eq!(ev[6].1["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn non_streaming_and_upstream_errors() {
        let ok = json!({"choices": [{"message": {"content": "pong"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
        let target = fake_upstream(200, "application/json", ok.to_string()).await;
        let req = json!({"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "ping"}]});
        let resp = run_messages(&target, &req, "claude-x").await;
        assert_eq!(resp.status(), 200);
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["content"][0]["text"], "pong");

        let err = json!({"error": {"message": "context too long", "type": "exceed_context_size_error"}});
        let target = fake_upstream(400, "application/json", err.to_string()).await;
        let resp = run_messages(&target, &req, "claude-x").await;
        assert_eq!(resp.status(), 400);
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "context too long"}}));

        // unreachable upstream -> 503 overloaded_error; bad request -> 400
        let mut dead = target.clone();
        dead.base = "http://127.0.0.1:1".into();
        assert_eq!(run_messages(&dead, &req, "claude-x").await.status(), 503);
        assert_eq!(run_messages(&dead, &json!({"model": "x"}), "x").await.status(), 400);
        assert_eq!(no_instance().status(), 503);
    }

    #[test]
    fn models_shape() {
        let t = Target { model: "org/m/Qwen.gguf".into(), name: "q".into(), runtime: "llama.cpp".into(), ready: true, base: String::new(), served: String::new(), started_at: 86400.0 };
        let resp = models_response(&[t.clone(), t]);
        assert_eq!(resp.status(), 200);
    }
}
