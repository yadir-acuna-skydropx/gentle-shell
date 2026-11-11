//! Placeholder Slack connector (T12). It reports `Disconnected` with a hint to
//! run `gentle-apps slack login`; the real connector lands in T13.
use crate::connector::{Connector, Result};
use crate::protocol::{Item, Status};

/// Hint shown in the panel until the Slack token is configured (T13).
pub const DISCONNECTED_DETAIL: &str = "not connected — run `gentle-apps slack login`";

#[derive(Default)]
pub struct SlackConnector;

impl Connector for SlackConnector {
    fn id(&self) -> &str {
        "slack"
    }
    fn name(&self) -> &str {
        "Slack"
    }
    fn status(&self) -> Status {
        Status::Disconnected
    }
    fn detail(&self) -> Option<String> {
        Some(DISCONNECTED_DETAIL.to_string())
    }
    fn poll(&mut self) -> Result<Vec<Item>> {
        // No credentials yet: nothing to show.
        Ok(Vec::new())
    }
    fn reply(&mut self, _item_id: &str, _text: &str) -> Result<()> {
        Err(DISCONNECTED_DETAIL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_stub_is_disconnected_with_a_login_hint() {
        let mut c = SlackConnector;
        assert_eq!(c.id(), "slack");
        assert_eq!(c.name(), "Slack");
        assert_eq!(c.status(), Status::Disconnected);
        assert!(c.detail().unwrap().contains("slack login"));
        assert_eq!(c.poll().unwrap(), Vec::new());
        assert!(c.reply("i", "x").is_err());
    }
}
