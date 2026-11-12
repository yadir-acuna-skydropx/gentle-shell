//! Anthropic Messages API client (S17): streaming SSE, model resolution and
//! bounded retries. The API key is passed only in the `x-api-key` header and
//! never appears in logs, errors, `Debug` or panic messages.
use std::io::BufRead;
use std::io::BufReader;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;

/// The Anthropic API version header value.
///
/// Verified against <https://platform.claude.com/docs/en/api/versioning>
/// (`"versions":{"newest":"2026-10-21",...}`). Change this one constant to move
/// to a newer version.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Default API base; overridable via `GENTLE_ENGINES_ANTHROPIC_API` (tests).
pub const DEFAULT_BASE: &str = "https://api.anthropic.com";

/// Resolves the API base from the environment, else the default.
pub fn base_url() -> String {
    std::env::var("GENTLE_ENGINES_ANTHROPIC_API")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// Errors surfaced by the client. None of these carry the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicError {
    /// HTTP 401 — a bad or missing key.
    Auth,
    /// HTTP 429 after exhausting retries.
    RateLimited,
    /// HTTP 529 after exhausting retries.
    Overloaded,
    /// Any other API failure, or an SSE `error` event (then `status == 0`).
    Api { status: u16, kind: String, message: String },
    /// A transport / connection / stream failure.
    Transport(String),
}

impl std::fmt::Display for AnthropicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnthropicError::Auth => write!(f, "invalid API key — run `gentle-explain key set`"),
            AnthropicError::RateLimited => write!(f, "Anthropic is rate limiting right now; try again shortly"),
            AnthropicError::Overloaded => write!(f, "Anthropic is overloaded right now; try again shortly"),
            AnthropicError::Api { status: 0, message, .. } => write!(f, "Anthropic error: {message}"),
            AnthropicError::Api { status, kind, message } => {
                write!(f, "Anthropic error {status} ({kind}): {message}")
            }
            AnthropicError::Transport(e) => write!(f, "network error: {e}"),
        }
    }
}

impl std::error::Error for AnthropicError {}

/// One conversation turn sent to the Messages API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Message { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Message { role: "assistant".into(), content: content.into() }
    }
}

/// A model as returned by `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Present only when the API groups models by line (e.g. `"haiku"`).
    #[serde(default)]
    pub line: Option<String>,
}

/// Picks the newest model of `line`. Prefers an explicit `line` field; falling
/// back to the newest id containing the word. `None` when nothing matches.
pub fn pick_model(models: &[Model], line: &str) -> Option<String> {
    let line_lc = line.to_lowercase();
    let by_line: Vec<&Model> =
        models.iter().filter(|m| m.line.as_deref().map(|l| l.eq_ignore_ascii_case(&line_lc)).unwrap_or(false)).collect();
    if !by_line.is_empty() {
        return newest(&by_line).map(|m| m.id.clone());
    }
    let by_id: Vec<&Model> = models.iter().filter(|m| m.id.to_lowercase().contains(&line_lc)).collect();
    newest(&by_id).map(|m| m.id.clone())
}

/// Newest by `created_at` (ISO-8601 sorts lexically); ties and missing dates
/// fall back to the highest id so the choice is deterministic.
fn newest<'a>(models: &[&'a Model]) -> Option<&'a Model> {
    models
        .iter()
        .copied()
        .max_by(|a, b| {
            a.created_at
                .as_deref()
                .unwrap_or("")
                .cmp(b.created_at.as_deref().unwrap_or(""))
                .then_with(|| a.id.cmp(&b.id))
        })
}

/// Bounded backoff for a 429/529 retry: honor `retry_after` when present, else
/// grow `1, 2, 4…`, capped at 8 seconds. `attempt` starts at 0.
pub fn retry_delay(retry_after: Option<u64>, attempt: u32) -> Duration {
    let secs = retry_after.unwrap_or(1u64 << attempt.min(3)).min(8);
    Duration::from_secs(secs)
}

/// Extracts `{error:{type,message}}` from an error body; falls back to the raw
/// text (truncated) when the shape is unexpected.
pub fn parse_api_error(body: &str) -> (String, String) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = v.get("error") {
            let kind = err.get("type").and_then(|t| t.as_str()).unwrap_or("error").to_string();
            let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
            return (kind, message);
        }
    }
    let snippet: String = body.chars().take(200).collect();
    ("error".to_string(), snippet)
}

/// Parses a Messages SSE stream, calling `on_delta` for each `text_delta`.
/// Stops early (returning `Ok`) when `cancel()` is true. An `error` event maps
/// to [`AnthropicError::Api`] with `status == 0`.
pub fn parse_sse<R: BufRead>(
    reader: R,
    on_delta: &mut dyn FnMut(&str),
    cancel: &dyn Fn() -> bool,
) -> Result<(), AnthropicError> {
    let mut data = String::new();
    for line in reader.lines() {
        if cancel() {
            return Ok(());
        }
        let line = line.map_err(|e| AnthropicError::Transport(e.to_string()))?;
        if line.is_empty() {
            if !data.is_empty() {
                if dispatch_event(&data, on_delta)? {
                    return Ok(());
                }
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue; // SSE comment / heartbeat
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
        // `event:`/`id:` lines are ignored; the JSON `type` drives behaviour.
    }
    if !data.is_empty() {
        dispatch_event(&data, on_delta)?;
    }
    Ok(())
}

/// Handles one SSE event's data payload. Returns `Ok(true)` on `message_stop`.
fn dispatch_event(data: &str, on_delta: &mut dyn FnMut(&str)) -> Result<bool, AnthropicError> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return Ok(false); // ignore unparseable data lines
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("content_block_delta") => {
            if let Some(delta) = v.get("delta") {
                if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                        on_delta(text);
                    }
                }
            }
            Ok(false)
        }
        Some("message_stop") => Ok(true),
        Some("error") => {
            let (kind, message) = v
                .get("error")
                .map(|e| {
                    (
                        e.get("type").and_then(|t| t.as_str()).unwrap_or("error").to_string(),
                        e.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                    )
                })
                .unwrap_or_else(|| ("error".into(), String::new()));
            Err(AnthropicError::Api { status: 0, kind, message })
        }
        _ => Ok(false), // ping, message_start, content_block_start/stop, message_delta…
    }
}

/// A Messages API client. Holds the key privately; `Debug` never prints it.
pub struct Client {
    agent: ureq::Agent,
    key: String,
    base: String,
    model_cache: Mutex<Option<String>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("base", &self.base).field("key", &"<redacted>").finish()
    }
}

impl Client {
    /// Builds a client for `base` (e.g. [`base_url`]) with the given key.
    pub fn new(key: impl Into<String>, base: impl Into<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(20)))
            .build()
            .into();
        Client { agent, key: key.into(), base: base.into(), model_cache: Mutex::new(None) }
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base.trim_end_matches('/'))
    }
    fn models_url(&self, after: Option<&str>) -> String {
        let base = self.base.trim_end_matches('/');
        match after {
            Some(a) => format!("{base}/v1/models?limit=1000&after_id={a}"),
            None => format!("{base}/v1/models?limit=1000"),
        }
    }

    /// Cheap authentication check used by `key set` and status: a single
    /// `GET /v1/models`. Maps 401 to [`AnthropicError::Auth`].
    pub fn validate_key(&self) -> Result<(), AnthropicError> {
        self.list_models().map(|_| ())
    }

    /// Fetches every active model, following pagination.
    pub fn list_models(&self) -> Result<Vec<Model>, AnthropicError> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut resp = self
                .agent
                .get(self.models_url(after.as_deref()))
                .header("x-api-key", &self.key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .call()
                .map_err(|e| AnthropicError::Transport(e.to_string()))?;
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().map_err(|e| AnthropicError::Transport(e.to_string()))?;
            if status != 200 {
                return Err(self.map_status(status, None, &body));
            }
            let page: ModelsPage = serde_json::from_str(&body)
                .map_err(|e| AnthropicError::Api { status, kind: "parse".into(), message: e.to_string() })?;
            out.extend(page.data);
            if page.has_more {
                match page.last_id {
                    Some(id) => after = Some(id),
                    None => break,
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Resolves the model id for `line` (e.g. `"haiku"`), honoring an explicit
    /// `override_model`, then the process cache, then the API. The result is
    /// cached for the process lifetime.
    pub fn resolve_model(&self, line: &str, override_model: Option<&str>) -> Result<String, AnthropicError> {
        if let Some(m) = override_model.filter(|m| !m.trim().is_empty()) {
            let mut cache = self.model_cache.lock().unwrap();
            *cache = Some(m.to_string());
            return Ok(m.to_string());
        }
        if let Some(m) = self.model_cache.lock().unwrap().clone() {
            return Ok(m);
        }
        let models = self.list_models()?;
        let picked = pick_model(&models, line).ok_or_else(|| AnthropicError::Api {
            status: 0,
            kind: "no_model".into(),
            message: format!("no active model found for line '{line}'"),
        })?;
        *self.model_cache.lock().unwrap() = Some(picked.clone());
        Ok(picked)
    }

    /// Streams a completion, delivering each text delta to `on_delta`. Honors
    /// 429/529 with bounded retries (max 2) and a `retry-after` header.
    pub fn stream_message(
        &self,
        system: &str,
        messages: &[Message],
        model: &str,
        on_delta: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), AnthropicError> {
        let body = build_body(system, messages, model);
        let mut attempt: u32 = 0;
        loop {
            if cancel() {
                return Ok(());
            }
            let mut resp = self
                .agent
                .post(self.messages_url())
                .header("x-api-key", &self.key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .send(body.as_str())
                .map_err(|e| AnthropicError::Transport(e.to_string()))?;
            let status = resp.status().as_u16();
            if status == 200 {
                let reader = BufReader::new(resp.body_mut().as_reader());
                return parse_sse(reader, on_delta, cancel);
            }
            let retry_after =
                resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            match status {
                401 => return Err(AnthropicError::Auth),
                429 | 529 if attempt < 2 => {
                    std::thread::sleep(retry_delay(retry_after, attempt));
                    attempt += 1;
                    continue;
                }
                429 => return Err(AnthropicError::RateLimited),
                529 => return Err(AnthropicError::Overloaded),
                _ => return Err(self.map_status(status, None, &text)),
            }
        }
    }

    fn map_status(&self, status: u16, _retry_after: Option<u64>, body: &str) -> AnthropicError {
        match status {
            401 => AnthropicError::Auth,
            429 => AnthropicError::RateLimited,
            529 => AnthropicError::Overloaded,
            _ => {
                let (kind, message) = parse_api_error(body);
                AnthropicError::Api { status, kind, message }
            }
        }
    }
}

#[derive(Deserialize)]
struct ModelsPage {
    #[serde(default)]
    data: Vec<Model>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

/// Builds the Messages request body with the stable system prompt marked for
/// ephemeral prompt caching.
pub fn build_body(system: &str, messages: &[Message], model: &str) -> String {
    let msgs: Vec<serde_json::Value> =
        messages.iter().map(|m| serde_json::json!({"role": m.role, "content": m.content})).collect();
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "stream": true,
        "system": [{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": msgs,
    });
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn collect_sse(input: &str) -> Result<String, AnthropicError> {
        let mut out = String::new();
        parse_sse(Cursor::new(input.to_string()), &mut |t| out.push_str(t), &|| false)?;
        Ok(out)
    }

    #[test]
    fn sse_collects_text_deltas_and_stops() {
        let input = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hola\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" mundo\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"after stop\"}}\n\n",
        );
        assert_eq!(collect_sse(input).unwrap(), "Hola mundo");
    }

    #[test]
    fn sse_handles_ping_and_comments() {
        let input = concat!(
            ": heartbeat\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
        );
        assert_eq!(collect_sse(input).unwrap(), "x");
    }

    #[test]
    fn sse_multiline_data_is_joined() {
        // A single event split across multiple data: lines (joined with \n).
        let input = "data: {\"type\":\"content_block_delta\",\ndata: \"delta\":{\"type\":\"text_delta\",\"text\":\"ab\"}}\n\n";
        assert_eq!(collect_sse(input).unwrap(), "ab");
    }

    #[test]
    fn sse_error_event_becomes_api_error() {
        let input = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"boom\"}}\n\n";
        let err = collect_sse(input).unwrap_err();
        assert_eq!(err, AnthropicError::Api { status: 0, kind: "overloaded_error".into(), message: "boom".into() });
    }

    #[test]
    fn sse_cancel_stops_mid_stream() {
        let input = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"one\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"two\"}}\n\n",
        );
        let mut out = String::new();
        let seen = std::cell::Cell::new(0u32);
        parse_sse(
            Cursor::new(input.to_string()),
            &mut |t| {
                out.push_str(t);
                seen.set(seen.get() + 1);
            },
            &|| seen.get() >= 1,
        )
        .unwrap();
        assert_eq!(out, "one");
    }

    #[test]
    fn pick_model_prefers_line_then_id_by_newest() {
        let models = vec![
            Model { id: "claude-haiku-3".into(), display_name: None, created_at: Some("2024-01-01".into()), line: Some("haiku".into()) },
            Model { id: "claude-haiku-4-5".into(), display_name: None, created_at: Some("2025-10-01".into()), line: Some("haiku".into()) },
            Model { id: "claude-sonnet-4".into(), display_name: None, created_at: Some("2025-05-01".into()), line: Some("sonnet".into()) },
        ];
        assert_eq!(pick_model(&models, "haiku"), Some("claude-haiku-4-5".into()));

        // No line field → fall back to newest id containing the word.
        let no_line = vec![
            Model { id: "claude-haiku-3".into(), display_name: None, created_at: Some("2024-01-01".into()), line: None },
            Model { id: "claude-haiku-4-5".into(), display_name: None, created_at: Some("2025-10-01".into()), line: None },
        ];
        assert_eq!(pick_model(&no_line, "haiku"), Some("claude-haiku-4-5".into()));
        assert_eq!(pick_model(&no_line, "opus"), None);
    }

    #[test]
    fn retry_delay_is_bounded_and_honors_retry_after() {
        assert_eq!(retry_delay(None, 0), Duration::from_secs(1));
        assert_eq!(retry_delay(None, 1), Duration::from_secs(2));
        assert_eq!(retry_delay(None, 2), Duration::from_secs(4));
        assert_eq!(retry_delay(Some(3), 0), Duration::from_secs(3));
        assert_eq!(retry_delay(Some(1000), 0), Duration::from_secs(8)); // capped
    }

    #[test]
    fn parse_api_error_reads_the_error_shape() {
        let (kind, message) =
            parse_api_error(r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"}}"#);
        assert_eq!(kind, "authentication_error");
        assert_eq!(message, "x-api-key header is required");
    }

    #[test]
    fn error_display_and_debug_never_leak_the_key() {
        let c = Client::new("sk-ant-supersecret-value", "http://127.0.0.1:1");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("supersecret"));
        assert!(dbg.contains("redacted"));
        // The auth error carries the recovery hint, never the key.
        let e = AnthropicError::Auth;
        assert_eq!(e.to_string(), "invalid API key — run `gentle-explain key set`");
    }

    #[test]
    fn build_body_marks_system_for_caching() {
        let body = build_body("SYS", &[Message::user("hi")], "claude-haiku");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(v["system"][0]["text"], "SYS");
        assert_eq!(v["stream"], true);
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["model"], "claude-haiku");
    }
}
