//! Helpers that keep request / response content out of logs and error strings.
//!
//! Rule of thumb for this crate: prompts, model output, tool arguments and auth headers are never
//! passed to `tracing`, never stored in the admin ring buffer and never written into an instance
//! log. Where an upstream error body has to reach the client, it goes through
//! [`upstream_error_message`] (truncated) and the log line uses [`upstream_log_summary`] (status +
//! length only).

use serde_json::Value;

/// Longest upstream error text echoed back to a client (characters, not bytes).
pub const ECHO_MAX_CHARS: usize = 200;

/// The first `max` characters of `s`, never splitting a UTF-8 sequence.
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Message for a non-2xx upstream body that is returned to the client. Prefers the structured
/// `error.message` / `message` field, otherwise the raw body; always at most [`ECHO_MAX_CHARS`]
/// characters. The result is for the client only and must not be logged.
pub fn upstream_error_message(status: u16, body: &str) -> String {
    let structured = serde_json::from_str::<Value>(body).ok().and_then(|v| v.pointer("/error/message").or_else(|| v.get("message")).and_then(Value::as_str).map(str::to_owned));
    match structured {
        Some(m) => truncate_chars(&m, ECHO_MAX_CHARS),
        None if body.trim().is_empty() => format!("upstream returned HTTP {status}"),
        None => truncate_chars(body.trim(), ECHO_MAX_CHARS),
    }
}

/// What may be logged about a failed upstream response: the status and the body length, nothing
/// from inside the body.
pub fn upstream_log_summary(status: u16, body_len: usize) -> String {
    format!("upstream HTTP {status} ({body_len} bytes body)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_char_based() {
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("abc", 10), "abc");
        // 3 chars of 3 bytes each: must not split a sequence
        assert_eq!(truncate_chars("あいう", 2), "あい");
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn structured_upstream_message_is_used_and_capped() {
        let body = r#"{"error":{"message":"context too long","type":"exceed_context_size_error"}}"#;
        assert_eq!(upstream_error_message(400, body), "context too long");
        let long = "x".repeat(900);
        let body = format!(r#"{{"message":"{long}"}}"#);
        let msg = upstream_error_message(500, &body);
        assert_eq!(msg.chars().count(), ECHO_MAX_CHARS);
    }

    #[test]
    fn raw_body_is_capped_and_empty_body_gets_status() {
        let raw = "あ".repeat(1000);
        assert_eq!(upstream_error_message(500, &raw).chars().count(), ECHO_MAX_CHARS);
        assert_eq!(upstream_error_message(502, "  \n "), "upstream returned HTTP 502");
        assert_eq!(upstream_error_message(500, "Internal boom"), "Internal boom");
    }

    #[test]
    fn log_summary_never_contains_body_text() {
        let s = upstream_log_summary(500, 1234);
        assert_eq!(s, "upstream HTTP 500 (1234 bytes body)");
        assert!(!s.contains("prompt"));
    }
}
