//! `gentle-engines`: independent Rust engines for the partitioner.
//!
//! The first engine is `gentle-explain` (S17): it streams Claude Haiku
//! explanations of selected code over NDJSON on stdio (see [`protocol`]). The
//! library is split so the T18 `gentle-arch` binary can reuse the Anthropic
//! client, keystore and context helpers:
//! - [`anthropic`] — the Messages API client (SSE streaming, model resolution,
//!   bounded retries). The API key stays in the `x-api-key` header only.
//! - [`keystore`] — the API key in the system keyring (service `gentle-engines`).
//! - [`protocol`] — the NDJSON types the partitioner speaks to an engine.
//! - [`context`] — selection/context capping and prompt assembly.
//! - [`engine`] — the stdio request loop around a pluggable [`engine::ExplainBackend`].
//! - [`backend`] — the real Anthropic-backed explainer.
pub mod anthropic;
pub mod arch;
pub mod arch_engine;
pub mod arch_protocol;
pub mod backend;
pub mod cli;
pub mod config;
pub mod context;
pub mod engine;
pub mod kb;
pub mod keystore;
pub mod scan;
pub mod tools;
pub mod protocol;

use std::path::PathBuf;

/// The user's home directory (for `engines.toml` lookups).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}
