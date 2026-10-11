//! OpenAI-compatible reverse proxy `/v1/*` to running inference servers.
//!
//! The target instance is chosen from the request body's `"model"` field (falling back to the
//! first running instance). Response bodies are streamed through untouched (SSE works), and the
//! harness credentials (`Authorization`, `Cookie`) never reach the upstream server.
//! Authentication itself is done by `auth::require_auth` (session cookie, or an `lh_...` API key
//! as `Authorization: Bearer`), which wraps this router in `main.rs`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::instances::Instance;
use crate::state::AppState;

/// Largest JSON request body that is buffered to read the `model` field.
const MAX_JSON_BODY: usize = 64 * 1024 * 1024;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        // Loopback traffic must never go through an environment HTTP proxy.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(60))
        // No overall timeout: generations (and SSE streams) can run for minutes.
        .build()
        .expect("build proxy client")
});

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/models", get(list_models)).route("/v1/{*rest}", any(forward))
}

// ------------------------------------------------------------------ errors
/// OpenAI-style error body.
fn oai_error(status: StatusCode, message: impl Into<String>, kind: &str, code: &str) -> Response {
    let body = json!({ "error": { "message": message.into(), "type": kind, "param": null, "code": code } });
    (status, Json(body)).into_response()
}

fn no_instance() -> Response {
    oai_error(StatusCode::SERVICE_UNAVAILABLE, "No model server is running. Start a model in LLM Harness first.", "server_error", "no_model_running")
}

// ------------------------------------------------------------------ targets
/// A running server a request can be routed to.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// Model id under models_dir (empty for ollama).
    pub model: String,
    pub name: String,
    pub runtime: String,
    pub ready: bool,
    /// `http://host:port`
    pub base: String,
    /// Model name the upstream server itself knows (vLLM serves the absolute path).
    pub served: String,
    pub started_at: f64,
}

/// Address to connect to: wildcard bind addresses mean loopback.
fn upstream_base(host: &str, port: u16) -> String {
    let h = host.trim();
    let h = if h.is_empty() || h == "0.0.0.0" || h == "::" || h == "[::]" { "127.0.0.1" } else { h };
    if h.contains(':') && !h.starts_with('[') {
        format!("http://[{h}]:{port}")
    } else {
        format!("http://{h}:{port}")
    }
}

fn target_of(inst: &Instance, models_dir: &std::path::Path) -> Target {
    let served = if inst.runtime == "vllm" && !inst.model.is_empty() { models_dir.join(&inst.model).to_string_lossy().into_owned() } else { inst.model.clone() };
    Target {
        model: inst.model.clone(),
        name: inst.proc.name.clone(),
        runtime: inst.runtime.clone(),
        ready: inst.ready(),
        base: upstream_base(&inst.host, inst.port),
        served,
        started_at: inst.proc.started_at,
    }
}

fn targets(state: &AppState) -> Vec<Target> {
    let dir = state.settings.get().models_dir();
    let all: Vec<Arc<Instance>> = state.instances.alive_instances();
    all.iter().map(|i| target_of(i, &dir)).collect()
}

fn stem(s: &str) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s).to_ascii_lowercase();
    base.strip_suffix(".gguf").map(str::to_owned).unwrap_or(base)
}

/// Pick the server for a request. Ready servers are preferred; an explicit `model` is matched by
/// model id, server name, served name, file stem and path suffix; otherwise the first server wins.
pub fn select_target(targets: &[Target], requested: Option<&str>) -> Option<usize> {
    if targets.is_empty() {
        return None;
    }
    let mut pool: Vec<usize> = (0..targets.len()).filter(|i| targets[*i].ready).collect();
    if pool.is_empty() {
        pool = (0..targets.len()).collect();
    }
    if let Some(req) = requested.map(str::trim).filter(|r| !r.is_empty()) {
        let req_stem = stem(req);
        let matchers: [&dyn Fn(&Target) -> bool; 5] = [
            &|t| !t.model.is_empty() && t.model == req,
            &|t| t.name == req || (!t.served.is_empty() && t.served == req),
            &|t| !t.model.is_empty() && stem(&t.model) == req_stem,
            &|t| !t.model.is_empty() && (req.ends_with(&format!("/{}", t.model)) || t.model.ends_with(&format!("/{req}"))),
            &|t| t.name.eq_ignore_ascii_case(req),
        ];
        for m in matchers {
            if let Some(i) = pool.iter().copied().find(|i| m(&targets[*i])) {
                return Some(i);
            }
        }
    }
    pool.first().copied()
}

// ------------------------------------------------------------------ request building
/// Segments like `..` could escape `/v1/` on the upstream (`/v1/../metrics`).
fn path_is_safe(path: &str) -> bool {
    path.split('/').all(|seg| {
        let lower = seg.to_ascii_lowercase();
        let decoded = lower.replace("%2e", ".");
        decoded != ".." && decoded != "." && !lower.contains("%2f") && !lower.contains("%5c") && !lower.contains('\\')
    })
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(name, "connection" | "keep-alive" | "proxy-authenticate" | "proxy-authorization" | "proxy-connection" | "te" | "trailer" | "trailers" | "transfer-encoding" | "upgrade")
}

/// Headers forwarded upstream: no harness credentials, no hop-by-hop headers, no Host.
pub fn forward_headers(src: &HeaderMap, keep_content_length: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        let n = name.as_str();
        let drop = is_hop_by_hop(n)
            || matches!(n, "authorization" | "cookie" | "host" | "x-api-key" | "x-harness-csrf")
            || (n == "content-length" && !keep_content_length);
        if !drop {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn response_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        let n = name.as_str();
        if !is_hop_by_hop(n) && n != "set-cookie" {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn is_json(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| {
        let v = v.to_ascii_lowercase();
        v.starts_with("application/json") || v.contains("+json")
    })
}

fn model_field(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body).ok()?.get("model")?.as_str().map(str::to_owned)
}

/// Replace `"model"` by the name the upstream knows; `None` when nothing needs changing.
fn rewrite_model(body: &[u8], served: &str) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let cur = v.get("model")?.as_str()?;
    if cur == served || served.is_empty() {
        return None;
    }
    v["model"] = Value::String(served.to_owned());
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

// ------------------------------------------------------------------ handlers
async fn list_models(State(state): State<AppState>, req: Request) -> Response {
    let found: Vec<Target> = targets(&state).into_iter().filter(|t| t.runtime != "ollama" && !t.model.is_empty()).collect();
    if found.is_empty() {
        // Nothing of ours is running: let an ollama server (if any) answer, else 503.
        return forward(State(state), req).await;
    }
    let mut seen = std::collections::HashSet::new();
    let data: Vec<Value> = found
        .iter()
        .filter(|t| seen.insert(t.model.clone()))
        .map(|t| json!({ "id": t.model, "object": "model", "created": t.started_at as i64, "owned_by": t.runtime }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn forward(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if !path_is_safe(parts.uri.path()) {
        return oai_error(StatusCode::BAD_REQUEST, "Invalid request path", "invalid_request_error", "invalid_path");
    }
    let has_body = !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS | Method::DELETE);
    let buffer = has_body && is_json(&parts.headers);

    // JSON bodies are buffered (to read `model`); anything else is streamed untouched.
    let (buffered, stream_body) = if buffer {
        match axum::body::to_bytes(body, MAX_JSON_BODY).await {
            Ok(b) => (Some(b), None),
            Err(_) => return oai_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large or unreadable", "invalid_request_error", "body_too_large"),
        }
    } else {
        (None, has_body.then_some(body))
    };
    let requested = buffered.as_deref().and_then(model_field);

    let all = targets(&state);
    let Some(idx) = select_target(&all, requested.as_deref()) else {
        return no_instance();
    };
    let target = &all[idx];

    let path_q = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or(parts.uri.path());
    let url = format!("{}{}", target.base, path_q);
    let mut rb = CLIENT.request(parts.method.clone(), &url).headers(forward_headers(&parts.headers, !buffer));
    rb = match (buffered, stream_body) {
        (Some(b), _) => {
            let b = if target.runtime == "vllm" { rewrite_model(&b, &target.served).unwrap_or(b) } else { b };
            rb.body(b)
        }
        (None, Some(body)) => rb.body(reqwest::Body::wrap_stream(body.into_data_stream())),
        (None, None) => rb,
    };

    let upstream = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            let (status, code) = if e.is_connect() || e.is_timeout() { (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable") } else { (StatusCode::BAD_GATEWAY, "upstream_error") };
            tracing::warn!("proxy to {} failed: {e}", target.base);
            return oai_error(status, format!("Model server is not reachable: {}", target.name), "server_error", code);
        }
    };

    let status = upstream.status();
    let headers = response_headers(upstream.headers());
    // No buffering: chunks are handed to the client as they arrive. Dropping the response (client
    // disconnect) closes the upstream connection, which aborts the generation.
    let mut resp = Response::new(Body::from_stream(upstream.bytes_stream()));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn t(model: &str, name: &str, runtime: &str, ready: bool) -> Target {
        Target { model: model.into(), name: name.into(), runtime: runtime.into(), ready, base: "http://127.0.0.1:8080".into(), served: model.into(), started_at: 0.0 }
    }

    #[test]
    fn selects_by_model_field_with_fallbacks() {
        let ts = vec![
            t("org/Qwen3-30B-A3B/Qwen3-30B-A3B-Q4_K_M.gguf", "qwen", "llama.cpp", true),
            t("org/Llama-3.1-8B/Llama-3.1-8B-Q8_0.gguf", "llama", "llama.cpp", true),
        ];
        assert_eq!(select_target(&[], Some("x")), None);
        assert_eq!(select_target(&ts, Some("org/Llama-3.1-8B/Llama-3.1-8B-Q8_0.gguf")), Some(1));
        assert_eq!(select_target(&ts, Some("llama")), Some(1));
        assert_eq!(select_target(&ts, Some("Llama-3.1-8B-Q8_0")), Some(1));
        assert_eq!(select_target(&ts, Some("Llama-3.1-8B/Llama-3.1-8B-Q8_0.gguf")), Some(1));
        // unknown / missing / empty model -> first
        assert_eq!(select_target(&ts, Some("gpt-4o")), Some(0));
        assert_eq!(select_target(&ts, None), Some(0));
        assert_eq!(select_target(&ts, Some("  ")), Some(0));
    }

    #[test]
    fn prefers_ready_servers() {
        let ts = vec![t("a.gguf", "a", "llama.cpp", false), t("b.gguf", "b", "llama.cpp", true)];
        assert_eq!(select_target(&ts, None), Some(1));
        let starting = vec![t("a.gguf", "a", "llama.cpp", false)];
        assert_eq!(select_target(&starting, Some("a.gguf")), Some(0));
    }

    #[test]
    fn credentials_and_hop_headers_are_stripped() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer lh_secret"),
            ("cookie", "harness_session=abc"),
            ("host", "harness.local"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("x-api-key", "k"),
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
            ("content-length", "12"),
        ] {
            h.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let f = forward_headers(&h, false);
        let names: Vec<&str> = f.keys().map(|k| k.as_str()).collect();
        assert!(names.contains(&"content-type") && names.contains(&"accept"));
        for bad in ["authorization", "cookie", "host", "connection", "transfer-encoding", "x-api-key", "content-length"] {
            assert!(!names.contains(&bad), "{bad} leaked");
        }
        assert!(forward_headers(&h, true).contains_key("content-length"));
    }

    #[test]
    fn path_safety() {
        assert!(path_is_safe("/v1/chat/completions"));
        assert!(path_is_safe("/v1/models/some-model"));
        for bad in ["/v1/../metrics", "/v1/%2e%2e/props", "/v1/%2E%2e/props", "/v1/a/./b", "/v1/a%2Fb", "/v1/a\\b"] {
            assert!(!path_is_safe(bad), "{bad}");
        }
    }

    #[test]
    fn upstream_addresses() {
        assert_eq!(upstream_base("0.0.0.0", 8080), "http://127.0.0.1:8080");
        assert_eq!(upstream_base("", 1), "http://127.0.0.1:1");
        assert_eq!(upstream_base("::", 1), "http://127.0.0.1:1");
        assert_eq!(upstream_base("192.168.1.5", 9), "http://192.168.1.5:9");
        assert_eq!(upstream_base("::1", 9), "http://[::1]:9");
    }

    #[test]
    fn model_extraction_and_rewrite() {
        assert_eq!(model_field(br#"{"model":"m","stream":true}"#).as_deref(), Some("m"));
        assert_eq!(model_field(b"not json"), None);
        assert_eq!(model_field(br#"{"model":3}"#), None);
        let out = rewrite_model(br#"{"model":"org/m","x":1}"#, "/models/org/m").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "/models/org/m");
        assert_eq!(v["x"], 1);
        assert!(rewrite_model(br#"{"model":"same"}"#, "same").is_none());
        assert!(rewrite_model(br#"{"x":1}"#, "same").is_none());
    }

    #[test]
    fn no_instance_is_503() {
        assert_eq!(no_instance().status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
