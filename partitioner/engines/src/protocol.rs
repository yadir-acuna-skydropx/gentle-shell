//! NDJSON protocol between the partitioner and the `gentle-explain` engine (S17).
//!
//! One JSON object per line, flushed after every message. Logs go only to
//! stderr. Lines are capped at [`MAX_LINE`]; an oversized or invalid line is
//! skipped, never a panic. The API key never appears in any of these types.
use serde::{Deserialize, Serialize};

/// Hard cap on a single protocol line (1 MiB). Anything longer is dropped.
pub const MAX_LINE: usize = 1024 * 1024;

/// Whether an Anthropic API key is stored, reported in [`EngineMsg::Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyState {
    Set,
    Missing,
}

/// Partitioner → engine messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    /// Explain a code selection; starts a fresh conversation.
    Explain {
        id: u64,
        path: String,
        language: String,
        selection: String,
        #[serde(default)]
        context_before: String,
        #[serde(default)]
        context_after: String,
    },
    /// A follow-up question in the same conversation as the last explain.
    Ask { id: u64, question: String },
    /// A one-shot command-helper request (S24): answer or recommend shell
    /// commands from the terminal's recent output and the user's question. It
    /// never touches the explain conversation.
    Suggest {
        id: u64,
        #[serde(default)]
        command_line: String,
        #[serde(default)]
        recent_output: String,
        question: String,
    },
    /// Cancel the in-flight request with this id.
    Cancel { id: u64 },
    /// Stop the engine.
    Shutdown,
}

/// Engine → partitioner messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EngineMsg {
    /// A streamed chunk of explanation text.
    Delta { id: u64, text: String },
    /// The request finished successfully.
    Done { id: u64 },
    /// The request failed; `message` is safe to show (never the key).
    Error { id: u64, message: String },
    /// Sent once on start: whether a key is stored and the resolved model.
    Status { key: KeyState, model: String },
}

impl EngineMsg {
    /// Serializes to a single NDJSON line (no trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("engine messages always serialize")
    }
}

impl ClientMsg {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("client messages always serialize")
    }
}

/// Outcome of parsing one inbound client line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Msg(ClientMsg),
    /// Empty or unknown-and-unaddressable: silently ignored.
    Ignore,
    /// Longer than [`MAX_LINE`]: skipped and logged.
    Oversize,
    /// Not valid JSON / unknown shape: skipped and logged.
    Invalid,
}

/// Parses one inbound line from the partitioner. Never panics.
pub fn parse_client_line(line: &str) -> Parsed {
    if line.len() > MAX_LINE {
        return Parsed::Oversize;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Ignore;
    }
    match serde_json::from_str::<ClientMsg>(trimmed) {
        Ok(msg) => Parsed::Msg(msg),
        Err(_) => Parsed::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_engine(msg: EngineMsg) {
        let line = msg.to_line();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<EngineMsg>(&line).unwrap(), msg);
    }
    fn rt_client(msg: ClientMsg) {
        let line = msg.to_line();
        assert_eq!(serde_json::from_str::<ClientMsg>(&line).unwrap(), msg);
    }

    #[test]
    fn engine_messages_round_trip() {
        rt_engine(EngineMsg::Delta { id: 1, text: "hola".into() });
        rt_engine(EngineMsg::Done { id: 2 });
        rt_engine(EngineMsg::Error { id: 3, message: "boom".into() });
        rt_engine(EngineMsg::Status { key: KeyState::Set, model: "claude-haiku".into() });
    }

    #[test]
    fn client_messages_round_trip() {
        rt_client(ClientMsg::Explain {
            id: 1,
            path: "src/a.rs".into(),
            language: "rust".into(),
            selection: "fn x() {}".into(),
            context_before: "// before".into(),
            context_after: "// after".into(),
        });
        rt_client(ClientMsg::Ask { id: 2, question: "¿por qué?".into() });
        rt_client(ClientMsg::Suggest {
            id: 4,
            command_line: "git stat".into(),
            recent_output: "fatal: not a git repository".into(),
            question: "¿cómo inicio un repo?".into(),
        });
        rt_client(ClientMsg::Cancel { id: 3 });
        rt_client(ClientMsg::Shutdown);
    }

    #[test]
    fn status_wire_shape_matches_the_spec() {
        let set = EngineMsg::Status { key: KeyState::Set, model: "m".into() }.to_line();
        assert_eq!(set, r#"{"type":"status","key":"set","model":"m"}"#);
        let missing = EngineMsg::Status { key: KeyState::Missing, model: String::new() }.to_line();
        assert_eq!(missing, r#"{"type":"status","key":"missing","model":""}"#);
    }

    #[test]
    fn explain_defaults_missing_context_fields() {
        let line = r#"{"type":"explain","id":7,"path":"p","language":"rust","selection":"s"}"#;
        assert_eq!(
            parse_client_line(line),
            Parsed::Msg(ClientMsg::Explain {
                id: 7,
                path: "p".into(),
                language: "rust".into(),
                selection: "s".into(),
                context_before: String::new(),
                context_after: String::new(),
            })
        );
    }

    #[test]
    fn parses_known_and_rejects_bad_lines() {
        assert_eq!(parse_client_line(r#"{"type":"cancel","id":5}"#), Parsed::Msg(ClientMsg::Cancel { id: 5 }));
        assert_eq!(parse_client_line(r#"{"type":"shutdown"}"#), Parsed::Msg(ClientMsg::Shutdown));
        assert_eq!(parse_client_line("   "), Parsed::Ignore);
        assert_eq!(parse_client_line("{not json"), Parsed::Invalid);
        assert_eq!(parse_client_line(r#"{"type":"nope"}"#), Parsed::Invalid);
        let big = format!(r#"{{"type":"ask","id":1,"question":"{}"}}"#, "x".repeat(MAX_LINE));
        assert_eq!(parse_client_line(&big), Parsed::Oversize);
    }
}
