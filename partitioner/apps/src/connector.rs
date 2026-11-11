//! The connector abstraction. Each app (Slack, and future engines) implements
//! [`Connector`]; the engine runs every connector on its own thread so a slow
//! app never blocks the others or the stdin loop.
use std::time::Duration;

use crate::protocol::{Item, Status};

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
}
