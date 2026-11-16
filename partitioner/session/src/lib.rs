//! gentle-session: a dtach-like per-folder background session (S34).
//!
//! A detached server runs the user command (e.g. gentle-partitioner) inside a
//! PTY and listens on a per-folder unix socket. Clients attach and relay raw
//! bytes so the TUI keeps working across terminal close / SSH drop; reopening
//! in the same folder reattaches. The modules split cleanly between pure,
//! unit-testable logic (hash, paths, proto, peer, meta) and the process/IO
//! machinery (pty, server, client, commands).

pub mod client;
pub mod commands;
pub mod hash;
pub mod meta;
pub mod paths;
pub mod peer;
pub mod proto;
pub mod pty;
pub mod server;
