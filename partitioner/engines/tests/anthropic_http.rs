//! End-to-end Anthropic client tests against the fake server: exact headers, no
//! key leak, model resolution (line present/absent, pagination, override) and
//! the 401/429/529 paths.
mod common;

use std::sync::{Arc, Mutex};

use common::{Fake, Resp};
use gentle_engines::anthropic::{AnthropicError, Client, Message, ANTHROPIC_VERSION};

#[test]
fn request_headers_are_exact_and_no_key_leaks() {
    let fake = Fake::start(|req, _| {
        if req.path.starts_with("/v1/messages") {
            Resp::sse(common::sse_hello())
        } else {
            Resp::json(200, r#"{"data":[],"has_more":false}"#)
        }
    });
    let client = Client::new("sk-ant-secret-12345", &fake.base);
    let mut out = String::new();
    client
        .stream_message("SYS", &[Message::user("hi")], "claude-haiku", &mut |t| out.push_str(t), &|| false)
        .unwrap();
    assert_eq!(out, "Hola mundo");

    let reqs = fake.requests();
    let msg = reqs.iter().find(|r| r.path.starts_with("/v1/messages")).expect("a messages request");
    assert_eq!(msg.headers.get("x-api-key").map(String::as_str), Some("sk-ant-secret-12345"));
    assert_eq!(msg.headers.get("anthropic-version").map(String::as_str), Some(ANTHROPIC_VERSION));
    assert!(msg.headers.get("content-type").unwrap().starts_with("application/json"));
    // The key must appear ONLY in the x-api-key header, never in the body.
    assert!(!msg.body.contains("sk-ant-"));
    // The ephemeral cache control rode along on the system prompt.
    assert!(msg.body.contains("ephemeral"));
}

#[test]
fn auth_error_maps_to_the_recovery_hint() {
    let fake = Fake::start(|_, _| {
        Resp::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"}}"#)
    });
    let client = Client::new("sk-ant-bad-00000000", &fake.base);
    let err = client.validate_key().unwrap_err();
    assert_eq!(err, AnthropicError::Auth);
    assert_eq!(err.to_string(), "invalid API key — run `gentle-explain key set`");
}

#[test]
fn rate_limit_is_retried_then_succeeds_honoring_retry_after() {
    let started = Arc::new(Mutex::new(std::time::Instant::now()));
    let started2 = started.clone();
    let fake = Fake::start(move |req, count| {
        if req.path.starts_with("/v1/messages") {
            if count == 1 {
                *started2.lock().unwrap() = std::time::Instant::now();
                return Resp::json(429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#)
                    .header("retry-after", "1");
            }
            return Resp::sse(common::sse_hello());
        }
        Resp::json(200, r#"{"data":[],"has_more":false}"#)
    });
    let client = Client::new("sk-ant-ok-12345678", &fake.base);
    let mut out = String::new();
    client
        .stream_message("SYS", &[Message::user("hi")], "claude-haiku", &mut |t| out.push_str(t), &|| false)
        .unwrap();
    assert_eq!(out, "Hola mundo");
    // Two attempts were made and the retry honored the ~1s retry-after.
    assert_eq!(fake.requests().iter().filter(|r| r.path.starts_with("/v1/messages")).count(), 2);
    assert!(started.lock().unwrap().elapsed() >= std::time::Duration::from_millis(900));
}

#[test]
fn persistent_overload_gives_up_after_two_retries() {
    let fake = Fake::start(|_, _| {
        Resp::json(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#)
    });
    let client = Client::new("sk-ant-ok-12345678", &fake.base);
    let err = client
        .stream_message("SYS", &[Message::user("hi")], "claude-haiku", &mut |_| {}, &|| false)
        .unwrap_err();
    assert_eq!(err, AnthropicError::Overloaded);
    // Initial try + 2 retries = 3 requests.
    assert_eq!(fake.requests().len(), 3);
}

#[test]
fn model_resolution_prefers_line_field() {
    let fake = Fake::start(|_, _| {
        Resp::json(
            200,
            r#"{"data":[
                {"id":"claude-haiku-3","created_at":"2024-01-01T00:00:00Z","line":"haiku"},
                {"id":"claude-haiku-4-5","created_at":"2025-10-01T00:00:00Z","line":"haiku"},
                {"id":"claude-sonnet-4-5","created_at":"2025-09-01T00:00:00Z","line":"sonnet"}
            ],"has_more":false}"#,
        )
    });
    let client = Client::new("sk-ant-ok-12345678", &fake.base);
    assert_eq!(client.resolve_model("haiku", None).unwrap(), "claude-haiku-4-5");
}

#[test]
fn model_resolution_without_line_field_falls_back_to_id_and_paginates() {
    let fake = Fake::start(|req, _| {
        // Page 1 unless after_id is present.
        if req.path.contains("after_id=claude-haiku-3") {
            Resp::json(
                200,
                r#"{"data":[{"id":"claude-haiku-4-5","created_at":"2025-10-01T00:00:00Z"}],"has_more":false,"last_id":"claude-haiku-4-5"}"#,
            )
        } else {
            Resp::json(
                200,
                r#"{"data":[{"id":"claude-haiku-3","created_at":"2024-01-01T00:00:00Z"}],"has_more":true,"last_id":"claude-haiku-3"}"#,
            )
        }
    });
    let client = Client::new("sk-ant-ok-12345678", &fake.base);
    // Newest haiku id across both pages wins.
    assert_eq!(client.resolve_model("haiku", None).unwrap(), "claude-haiku-4-5");
    // Two pages were fetched.
    assert_eq!(fake.requests().iter().filter(|r| r.path.starts_with("/v1/models")).count(), 2);
}

#[test]
fn model_override_skips_the_api_and_caches() {
    let fake = Fake::start(|_, _| Resp::json(200, r#"{"data":[],"has_more":false}"#));
    let client = Client::new("sk-ant-ok-12345678", &fake.base);
    assert_eq!(client.resolve_model("haiku", Some("claude-haiku-pinned")).unwrap(), "claude-haiku-pinned");
    // No model listing happened because the override short-circuits.
    assert!(fake.requests().is_empty());
    // And the override is cached for later calls.
    assert_eq!(client.resolve_model("haiku", None).unwrap(), "claude-haiku-pinned");
    assert!(fake.requests().is_empty());
}
