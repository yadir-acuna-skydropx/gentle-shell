//! The connector abstraction. Each app (Slack, and future engines) implements
//! [`Connector`]; the engine runs every connector on its own thread so a slow
//! app never blocks the others or the stdin loop.
use std::time::Duration;

use crate::protocol::{IssueFieldEdit, IssueSchema, Item, NewIssue, Status};

/// Connector errors are plain strings: the engine has no error dependency and
/// only forwards the message to the partitioner in a `result`.
pub type Result<T> = std::result::Result<T, String>;

/// Default polling interval for a connector that does not override it.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(25);

/// One connected (or connectable) app.
pub trait Connector: Send {
    /// Stable id used to route messages (e.g. `"slack"`).
    fn id(&self) -> &str;
    /// Human name shown in the panel (e.g. `"Slack"`).
    fn name(&self) -> &str;
    /// Current connection state.
    fn status(&self) -> Status;
    /// Optional one-line detail shown next to the status (e.g. a hint).
    fn detail(&self) -> Option<String> {
        None
    }
    /// How often the engine calls [`Connector::poll`].
    fn interval(&self) -> Duration {
        DEFAULT_INTERVAL
    }
    /// Fetches the full current item list. Called on the connector's thread.
    fn poll(&mut self) -> Result<Vec<Item>>;
    /// Sends a reply to `item_id`. Called on the connector's thread.
    fn reply(&mut self, item_id: &str, text: &str) -> Result<()>;
    /// Returns the children of a hierarchical item. The default refuses, so a
    /// flat connector (Slack) needs no change beyond compiling.
    fn expand(&mut self, _item_id: &str) -> Result<Vec<Item>> {
        Err("not supported".to_string())
    }
    /// Runs an action on an item, returning a success detail (e.g. a created
    /// path). The default refuses.
    fn action(&mut self, _item_id: &str, _action_id: &str) -> Result<String> {
        Err("not supported".to_string())
    }
    /// Returns a read-only detail view of an item as `(title, body)` plain
    /// markdown text for the in-terminal reader (S27). Called off the UI thread
    /// when the `view` action runs. The default refuses, so a connector that
    /// offers no `view` action needs no change.
    fn view(&mut self, _item_id: &str) -> Result<(String, String)> {
        Err("not supported".to_string())
    }
    /// Returns the editable schema of an existing issue (S30), fetched off the
    /// UI thread. The default refuses.
    fn issue_form(&mut self, _item_id: &str) -> Result<IssueSchema> {
        Err("not supported".to_string())
    }
    /// Returns the blank editable schema (options) for a new issue in `repo`
    /// (`owner/name`), S30. The default refuses.
    fn new_issue_form(&mut self, _repo: &str) -> Result<IssueSchema> {
        Err("not supported".to_string())
    }
    /// Applies a form diff to an existing issue (S30), returning a summary. The
    /// default refuses.
    fn issue_edit(&mut self, _item_id: &str, _edit: &IssueFieldEdit) -> Result<String> {
        Err("not supported".to_string())
    }
    /// Creates a new issue in `repo` (`owner/name`), returning a summary (S30).
    fn issue_create(&mut self, _repo: &str, _issue: &NewIssue) -> Result<String> {
        Err("not supported".to_string())
    }
    /// Creates or switches to the issue's local branch (S30). The default
    /// refuses.
    fn issue_branch(&mut self, _item_id: &str) -> Result<String> {
        Err("not supported".to_string())
    }
    /// Commits the already-staged changes with `message` (S30). The default
    /// refuses.
    fn issue_commit(&mut self, _item_id: &str, _message: &str) -> Result<String> {
        Err("not supported".to_string())
    }
}
