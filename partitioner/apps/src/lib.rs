//! `gentle-apps`: an independent apps engine for the partitioner (S12).
//!
//! The partitioner spawns the `gentle-apps` binary and talks to it over NDJSON
//! on stdio (see [`protocol`]). Each app is a [`connector::Connector`] driven on
//! its own thread by the [`engine`]. This library target lets the partitioner
//! share the exact protocol types instead of duplicating them.
pub mod config;
pub mod connector;
pub mod engine;
pub mod http;
pub mod oauth;
pub mod pkce;
pub mod protocol;
pub mod slack;
pub mod store;

use std::path::PathBuf;

use store::{MemoryStore, TokenStore};

/// The user's home directory (for `apps.toml` and keyring metadata lookups).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Chooses the token store: in-memory when `GENTLE_APPS_TOKEN_STORE=memory`
/// (tests and the status smoke), else the system keyring.
pub fn select_store() -> Box<dyn TokenStore> {
    if std::env::var("GENTLE_APPS_TOKEN_STORE").as_deref() == Ok("memory") {
        Box::new(MemoryStore::new())
    } else {
        Box::new(store::KeyringStore)
    }
}

/// Builds the default connector set (Slack, reading its client id from
/// `apps.toml` and its token from the selected store).
pub fn default_connectors() -> Vec<Box<dyn connector::Connector>> {
    let home = home_dir();
    let cfg = oauth::SlackConfig::from_env();
    let meta = config::load(&home);
    let store = select_store();
    vec![Box::new(slack::SlackConnector::new(cfg, meta.client_id, store, home))]
}
