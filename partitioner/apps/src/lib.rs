//! `gentle-apps`: an independent apps engine for the partitioner (S12).
//!
//! The partitioner spawns the `gentle-apps` binary and talks to it over NDJSON
//! on stdio (see [`protocol`]). Each app is a [`connector::Connector`] driven on
//! its own thread by the [`engine`]. This library target lets the partitioner
//! share the exact protocol types instead of duplicating them.
pub mod connector;
pub mod engine;
pub mod protocol;
pub mod slack;

/// Builds the default connector set. Only the Slack placeholder for now (T12).
pub fn default_connectors() -> Vec<Box<dyn connector::Connector>> {
    vec![Box::new(slack::SlackConnector)]
}
