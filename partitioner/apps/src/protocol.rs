//! NDJSON protocol shared by the `gentle-apps` engine and the partitioner.
//!
//! One JSON object per line, flushed after every message. Logs go only on
//! stderr. Lines are capped at [`MAX_LINE`]; an oversized or invalid line is
//! skipped and logged, never a panic.
use serde::{Deserialize, Serialize};

/// Hard cap on a single protocol line (1 MiB). Anything longer is dropped.
pub const MAX_LINE: usize = 1024 * 1024;

/// Connection state of one app, shown as the status dot in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// One app's identity and current connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStatus {
    pub id: String,
    pub name: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One action a connector offers on an [`Item`] (shown in the panel's action
/// menu). `confirm` asks the panel to prompt "Run <label>? y/n" first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub confirm: bool,
}

/// `skip_serializing_if` helper: keeps the old wire shape for flat items.
fn is_false(b: &bool) -> bool {
    !b
}

/// One actionable message surfaced by a connector (e.g. a Slack mention) or a
/// hierarchical node (a GitHub section, repo, branch, tree entry…).
///
/// The last three fields are optional and default-absent, so an old flat item
/// (Slack) still parses and still serializes to its original shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub text: String,
    pub ts: String,
    pub link: String,
    /// Optional category hint for the panel icon (e.g. `"pr"`, `"repo"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// True when the item has children reachable via an `expand` request.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expandable: bool,
    /// Actions offered on this item (open, worktree_add, checkout…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ItemAction>,
}

/// Engine → partitioner messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EngineMsg {
    /// Full current app list; sent on start and on any status change.
    Apps { apps: Vec<AppStatus> },
    /// The full current item list for one app.
    Items { app: String, items: Vec<Item> },
    /// Correlated outcome of a `reply` or `refresh` request.
    Result {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Correlated children of an `expand` request.
    Children { id: u64, app: String, item: String, items: Vec<Item> },
    /// Correlated outcome of an `action` request. `message` carries a success
    /// detail (e.g. the created worktree path); `error` carries a failure.
    ActionResult {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Correlated full detail of a `view` action on an item (S27): the
    /// in-terminal reader content. `title` is the window title and `body` is
    /// the full content as plain markdown text. Emitted instead of an
    /// `ActionResult` when the action id is `"view"`.
    Detail {
        id: u64,
        app: String,
        item: String,
        title: String,
        body: String,
    },
}

/// Partitioner → engine messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    Reply { id: u64, app: String, item: String, text: String },
    Refresh { id: u64, app: String },
    /// Request the children of a hierarchical item.
    Expand { id: u64, app: String, item: String },
    /// Run an action on an item.
    Action { id: u64, app: String, item: String, action: String },
    Shutdown,
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
    /// A well-formed, known message.
    Msg(ClientMsg),
    /// A known shape but unknown `type`, carrying an `id` to answer with an error.
    UnknownWithId(u64),
    /// Unknown and unaddressable: silently ignored.
    Ignore,
    /// Longer than [`MAX_LINE`]: skipped and logged.
    Oversize,
    /// Not valid JSON: skipped and logged.
    Invalid,
}

/// Parses one inbound line from the partitioner. Never panics: oversized or
/// invalid input maps to [`Parsed::Oversize`] / [`Parsed::Invalid`].
pub fn parse_client_line(line: &str) -> Parsed {
    if line.len() > MAX_LINE {
        return Parsed::Oversize;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Ignore;
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Parsed::Invalid,
    };
    match serde_json::from_value::<ClientMsg>(value.clone()) {
        Ok(msg) => Parsed::Msg(msg),
        Err(_) => match value.get("id").and_then(|v| v.as_u64()) {
            Some(id) => Parsed::UnknownWithId(id),
            None => Parsed::Ignore,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_engine(msg: EngineMsg) {
        let line = msg.to_line();
        assert!(!line.contains('\n'), "a message is a single line");
        let back: EngineMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    fn round_trip_client(msg: ClientMsg) {
        let line = msg.to_line();
        let back: ClientMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn every_engine_message_round_trips() {
        round_trip_engine(EngineMsg::Apps {
            apps: vec![AppStatus {
                id: "slack".into(),
                name: "Slack".into(),
                status: Status::Disconnected,
                detail: Some("not connected".into()),
            }],
        });
        round_trip_engine(EngineMsg::Items {
            app: "slack".into(),
            items: vec![Item {
                id: "1".into(),
                title: "#general · ana".into(),
                text: "hi".into(),
                ts: "123".into(),
                link: "https://x".into(),
                ..Default::default()
            }],
        });
        round_trip_engine(EngineMsg::Result { id: 7, ok: true, error: None });
        round_trip_engine(EngineMsg::Result { id: 8, ok: false, error: Some("boom".into()) });
    }

    #[test]
    fn every_client_message_round_trips() {
        round_trip_client(ClientMsg::Reply { id: 1, app: "slack".into(), item: "i1".into(), text: "hola".into() });
        round_trip_client(ClientMsg::Refresh { id: 2, app: "slack".into() });
        round_trip_client(ClientMsg::Expand { id: 3, app: "github".into(), item: "sec:repos".into() });
        round_trip_client(ClientMsg::Action {
            id: 4,
            app: "github".into(),
            item: "branch|o|r|main|abc".into(),
            action: "open".into(),
        });
        round_trip_client(ClientMsg::Shutdown);
    }

    #[test]
    fn children_and_action_result_round_trip() {
        round_trip_engine(EngineMsg::Children {
            id: 9,
            app: "github".into(),
            item: "sec:repos".into(),
            items: vec![Item {
                id: "repo|o|r".into(),
                title: "o/r".into(),
                text: String::new(),
                ts: String::new(),
                link: "https://github.com/o/r".into(),
                kind: Some("repo".into()),
                expandable: true,
                actions: vec![ItemAction { id: "open".into(), label: "Open".into(), confirm: false }],
            }],
        });
        round_trip_engine(EngineMsg::ActionResult {
            id: 10,
            ok: true,
            message: Some("/tmp/r-main".into()),
            error: None,
        });
        round_trip_engine(EngineMsg::ActionResult { id: 11, ok: false, message: None, error: Some("boom".into()) });
    }

    #[test]
    fn detail_round_trips_and_carries_the_reader_text() {
        // S27: the `view` action returns a Detail with the full reader body.
        let msg = EngineMsg::Detail {
            id: 12,
            app: "github".into(),
            item: "issue:octo/repo#7".into(),
            title: "octo/repo#7 · A bug".into(),
            body: "octo/repo#7 · issue · open · @ann\n\nA bug\n\nhttps://x\n\nbody".into(),
        };
        let line = msg.to_line();
        assert!(!line.contains('\n'), "a message is a single line");
        let back: EngineMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
        // The wire tag is `detail`.
        assert!(line.contains("\"type\":\"detail\""), "{line}");
    }

    #[test]
    fn expand_and_action_parse_from_the_wire() {
        assert_eq!(
            parse_client_line(r#"{"type":"expand","id":5,"app":"github","item":"sec:repos"}"#),
            Parsed::Msg(ClientMsg::Expand { id: 5, app: "github".into(), item: "sec:repos".into() })
        );
        assert_eq!(
            parse_client_line(r#"{"type":"action","id":6,"app":"github","item":"i","action":"open"}"#),
            Parsed::Msg(ClientMsg::Action {
                id: 6,
                app: "github".into(),
                item: "i".into(),
                action: "open".into()
            })
        );
    }

    #[test]
    fn an_old_flat_item_without_the_new_fields_still_parses() {
        // Backward compatibility: a Slack-shaped item (no kind/expandable/actions)
        // deserializes with the new fields defaulted (absent, not expandable).
        let line = r##"{"id":"C1:1.2","title":"#general","text":"hi","ts":"1.2","link":"l"}"##;
        let item: Item = serde_json::from_str(line).unwrap();
        assert_eq!(item.kind, None);
        assert!(!item.expandable);
        assert!(item.actions.is_empty());
        // And it serializes back to exactly the old shape (no new keys).
        assert_eq!(serde_json::to_string(&item).unwrap(), line);
    }

    #[test]
    fn status_serializes_lowercase() {
        let s = serde_json::to_string(&Status::Connected).unwrap();
        assert_eq!(s, "\"connected\"");
    }

    #[test]
    fn apps_wire_shape_matches_the_spec() {
        let line = EngineMsg::Apps {
            apps: vec![AppStatus {
                id: "slack".into(),
                name: "Slack".into(),
                status: Status::Disconnected,
                detail: None,
            }],
        }
        .to_line();
        assert_eq!(line, r#"{"type":"apps","apps":[{"id":"slack","name":"Slack","status":"disconnected"}]}"#);
    }

    #[test]
    fn parses_known_client_messages() {
        assert_eq!(
            parse_client_line(r#"{"type":"reply","id":3,"app":"slack","item":"i","text":"hi"}"#),
            Parsed::Msg(ClientMsg::Reply { id: 3, app: "slack".into(), item: "i".into(), text: "hi".into() })
        );
        assert_eq!(parse_client_line(r#"{"type":"shutdown"}"#), Parsed::Msg(ClientMsg::Shutdown));
    }

    #[test]
    fn oversized_line_is_skipped() {
        let line = format!(r#"{{"type":"reply","id":1,"app":"slack","item":"i","text":"{}"}}"#, "x".repeat(MAX_LINE));
        assert_eq!(parse_client_line(&line), Parsed::Oversize);
    }

    #[test]
    fn invalid_json_is_skipped() {
        assert_eq!(parse_client_line("{not json"), Parsed::Invalid);
        assert_eq!(parse_client_line("garbage"), Parsed::Invalid);
    }

    #[test]
    fn unknown_type_with_id_is_answerable_without_id_is_ignored() {
        assert_eq!(parse_client_line(r#"{"type":"nope","id":9}"#), Parsed::UnknownWithId(9));
        assert_eq!(parse_client_line(r#"{"type":"nope"}"#), Parsed::Ignore);
        assert_eq!(parse_client_line("   "), Parsed::Ignore);
    }
}
