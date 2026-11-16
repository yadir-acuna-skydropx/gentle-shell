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

/// One Project (v2) single-select / iteration option offered in the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOption {
    pub id: String,
    pub name: String,
}

/// One milestone offered in the editor (number + title).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneOption {
    pub number: u64,
    pub title: String,
}

/// One Project (v2) field on an issue's project item (S30): Status/Type
/// (single_select), Estimate (number), Iteration (iteration), plus text/date.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFieldSchema {
    pub field_id: String,
    pub name: String,
    /// `single_select` | `number` | `iteration` | `text` | `date`.
    pub kind: String,
    /// Current display value (option/iteration name, number, text), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Options for `single_select` / `iteration`; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<FieldOption>,
}

/// One Project (v2) item the issue belongs to, with its editable fields (S30).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSchema {
    pub project_id: String,
    pub item_id: String,
    pub title: String,
    pub fields: Vec<ProjectFieldSchema>,
}

/// The editable schema of an issue (or a blank new issue) handed to the UI
/// editor (S30): current values plus the allowed options fetched off the UI
/// thread. `number` is absent for a new issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSchema {
    /// `owner/name`.
    pub repo: String,
    /// The issue number, or `None` for a new issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    /// `open` | `closed`.
    pub state: String,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    /// Current milestone number, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<u64>,
    /// Repo users that can be assigned.
    pub assignable_users: Vec<String>,
    /// Labels defined in the repo.
    pub repo_labels: Vec<String>,
    /// Open milestones in the repo.
    pub milestones: Vec<MilestoneOption>,
    /// The Project (v2) items this issue belongs to, with their fields.
    pub projects: Vec<ProjectSchema>,
    /// A non-fatal note shown in the editor (e.g. the missing-scope hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One Project (v2) field change applied after the issue PATCH (S30). `value`
/// is the option id (single_select), iteration id, number-as-string or text; an
/// empty `value` clears the field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFieldEdit {
    pub project_id: String,
    pub item_id: String,
    pub field_id: String,
    /// `single_select` | `number` | `iteration` | `text` | `date`.
    pub kind: String,
    pub value: String,
}

/// A form diff for an existing issue (S30): only `Some`/non-empty parts are
/// applied, so the UI sends just what the user changed. Backward compatible —
/// every field defaults to absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueFieldEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `open` | `closed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// `None` unchanged; `Some(0)` clears; `Some(n>0)` sets milestone n.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<u64>,
    /// A new comment to post (never part of the PATCH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Project (v2) field updates to apply after the PATCH.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_fields: Vec<ProjectFieldEdit>,
}

/// The fields of a brand-new issue (S30).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewIssue {
    pub title: String,
    pub body: String,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<u64>,
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
    /// Correlated editable schema of an issue (or a blank new issue) for the
    /// editor overlay (S30). Emitted in reply to `issueform` / `newissueform`.
    IssueForm {
        id: u64,
        app: String,
        item: String,
        schema: IssueSchema,
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
    /// Request the editable schema of an existing issue (S30).
    IssueForm { id: u64, app: String, item: String },
    /// Request the blank editable schema (options) for a new issue in `repo`
    /// (`owner/name`), S30.
    NewIssueForm { id: u64, app: String, repo: String },
    /// Apply a form diff to an existing issue (fields + project fields +
    /// comment), S30.
    IssueEdit { id: u64, app: String, item: String, edit: IssueFieldEdit },
    /// Create a new issue in `repo` (`owner/name`), S30.
    IssueCreate { id: u64, app: String, repo: String, issue: NewIssue },
    /// Create or switch to the issue's local branch (S30).
    IssueBranch { id: u64, app: String, item: String },
    /// Commit the already-staged changes with `message` (S30).
    IssueCommit { id: u64, app: String, item: String, message: String },
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
    fn issue_editor_messages_round_trip_and_stay_backward_compatible() {
        // S30: the editor schema (edit flavour, with projects + options).
        let schema = IssueSchema {
            repo: "octo/repo".into(),
            number: Some(7),
            title: "A bug".into(),
            body: "desc".into(),
            state: "open".into(),
            assignees: vec!["ann".into()],
            labels: vec!["bug".into()],
            milestone: Some(3),
            assignable_users: vec!["ann".into(), "bob".into()],
            repo_labels: vec!["bug".into(), "p1".into()],
            milestones: vec![MilestoneOption { number: 3, title: "v1".into() }],
            projects: vec![ProjectSchema {
                project_id: "PVT_1".into(),
                item_id: "PVTI_1".into(),
                title: "Roadmap".into(),
                fields: vec![ProjectFieldSchema {
                    field_id: "F_status".into(),
                    name: "Status".into(),
                    kind: "single_select".into(),
                    current: Some("Todo".into()),
                    options: vec![FieldOption { id: "opt1".into(), name: "Todo".into() }],
                }],
            }],
            note: Some("Project fields need: gh auth refresh -s project".into()),
        };
        round_trip_engine(EngineMsg::IssueForm {
            id: 20,
            app: "github".into(),
            item: "issue:octo/repo#7".into(),
            schema,
        });
        // A blank new-issue schema (number absent) still round-trips.
        round_trip_engine(EngineMsg::IssueForm {
            id: 21,
            app: "github".into(),
            item: "issue:new|octo/repo".into(),
            schema: IssueSchema { repo: "octo/repo".into(), state: "open".into(), ..Default::default() },
        });

        round_trip_client(ClientMsg::IssueForm { id: 1, app: "github".into(), item: "issue:octo/repo#7".into() });
        round_trip_client(ClientMsg::NewIssueForm { id: 2, app: "github".into(), repo: "octo/repo".into() });
        round_trip_client(ClientMsg::IssueEdit {
            id: 3,
            app: "github".into(),
            item: "issue:octo/repo#7".into(),
            edit: IssueFieldEdit {
                title: Some("New title".into()),
                milestone: Some(0),
                comment: Some("ping".into()),
                project_fields: vec![ProjectFieldEdit {
                    project_id: "PVT_1".into(),
                    item_id: "PVTI_1".into(),
                    field_id: "F_status".into(),
                    kind: "single_select".into(),
                    value: "opt1".into(),
                }],
                ..Default::default()
            },
        });
        round_trip_client(ClientMsg::IssueCreate {
            id: 4,
            app: "github".into(),
            repo: "octo/repo".into(),
            issue: NewIssue { title: "t".into(), body: "b".into(), ..Default::default() },
        });
        round_trip_client(ClientMsg::IssueBranch { id: 5, app: "github".into(), item: "issue:octo/repo#7".into() });
        round_trip_client(ClientMsg::IssueCommit {
            id: 6,
            app: "github".into(),
            item: "issue:octo/repo#7".into(),
            message: "Fix (refs #7)".into(),
        });

        // An empty field diff carries no field keys on the wire (form diff).
        let empty = serde_json::to_string(&IssueFieldEdit::default()).unwrap();
        assert_eq!(empty, "{}", "an empty diff serializes to no keys");
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
