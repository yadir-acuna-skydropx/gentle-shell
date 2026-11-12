//! NDJSON protocol between the partitioner and the `gentle-arch` engine (S18).
//!
//! Mirrors [`crate::protocol`] (one JSON object per line, flushed per message,
//! capped at [`crate::protocol::MAX_LINE`]) but for architecture review/design.
//! No secret ever appears in these types.
use serde::{Deserialize, Serialize};

pub use crate::protocol::{KeyState, MAX_LINE};

/// Partitioner → arch engine messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ArchClientMsg {
    /// Review the repo at `path` (default cwd when empty). `deep` selects opus.
    Review {
        id: u64,
        #[serde(default)]
        path: String,
        #[serde(default)]
        deep: bool,
    },
    /// Design from a free-text `brief`. `deep` selects opus.
    Design {
        id: u64,
        brief: String,
        #[serde(default)]
        deep: bool,
    },
    /// Cancel the in-flight request with this id.
    Cancel { id: u64 },
    /// Stop the engine.
    Shutdown,
}

/// Arch engine → partitioner messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ArchEngineMsg {
    /// A streamed chunk of report text.
    Delta { id: u64, text: String },
    /// The request finished; `path` is the written report file.
    Done { id: u64, path: String },
    /// The request failed; `message` is safe to show (never a secret).
    Error { id: u64, message: String },
    /// Sent once on start: key presence, resolved model, KB chunk count.
    Status { key: KeyState, model: String, kb_chunks: usize },
}

impl ArchEngineMsg {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("arch engine messages always serialize")
    }
}

impl ArchClientMsg {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("arch client messages always serialize")
    }
}

/// Outcome of parsing one inbound client line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Msg(ArchClientMsg),
    Ignore,
    Oversize,
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
    match serde_json::from_str::<ArchClientMsg>(trimmed) {
        Ok(msg) => Parsed::Msg(msg),
        Err(_) => Parsed::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_messages_round_trip() {
        for msg in [
            ArchEngineMsg::Delta { id: 1, text: "## Summary".into() },
            ArchEngineMsg::Done { id: 2, path: "./.gentle-arch/review-1.md".into() },
            ArchEngineMsg::Error { id: 3, message: "boom".into() },
            ArchEngineMsg::Status { key: KeyState::Set, model: "claude-sonnet".into(), kb_chunks: 42 },
        ] {
            let line = msg.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<ArchEngineMsg>(&line).unwrap(), msg);
        }
    }

    #[test]
    fn client_messages_round_trip_and_default_fields() {
        let review = parse_client_line(r#"{"type":"review","id":1}"#);
        assert_eq!(review, Parsed::Msg(ArchClientMsg::Review { id: 1, path: String::new(), deep: false }));
        let design = parse_client_line(r#"{"type":"design","id":2,"brief":"banco","deep":true}"#);
        assert_eq!(design, Parsed::Msg(ArchClientMsg::Design { id: 2, brief: "banco".into(), deep: true }));
        assert_eq!(parse_client_line(r#"{"type":"cancel","id":3}"#), Parsed::Msg(ArchClientMsg::Cancel { id: 3 }));
        assert_eq!(parse_client_line(r#"{"type":"shutdown"}"#), Parsed::Msg(ArchClientMsg::Shutdown));
        assert_eq!(parse_client_line("  "), Parsed::Ignore);
        assert_eq!(parse_client_line("{bad"), Parsed::Invalid);
    }

    #[test]
    fn status_wire_shape() {
        let s = ArchEngineMsg::Status { key: KeyState::Missing, model: String::new(), kb_chunks: 0 }.to_line();
        assert_eq!(s, r#"{"type":"status","key":"missing","model":"","kb_chunks":0}"#);
    }
}
