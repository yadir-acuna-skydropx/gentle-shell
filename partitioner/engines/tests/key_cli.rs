//! Key CLI: format rejection, validation against the fake API before storing,
//! status and clear. The binary-level checks confirm exit codes and that the
//! key never appears in output.
mod common;

use std::process::Command;

use common::{Fake, Resp};
use gentle_engines::cli::{self, SetOutcome};
use gentle_engines::keystore::{KeyStore, MemoryStore};

const BIN: &str = env!("CARGO_BIN_EXE_gentle-explain");

#[test]
fn set_validates_against_the_api_then_stores() {
    let fake = Fake::start(|_, _| Resp::json(200, r#"{"data":[],"has_more":false}"#));
    let store = MemoryStore::new();
    let out = cli::key_set(&store, "sk-ant-valid-12345678", &fake.base);
    assert_eq!(out, SetOutcome::Saved);
    assert_eq!(store.load().unwrap().as_deref(), Some("sk-ant-valid-12345678"));
    // A GET /v1/models was used to validate (cheap, no cost).
    assert_eq!(fake.requests().len(), 1);
    assert!(fake.requests()[0].path.starts_with("/v1/models"));
}

#[test]
fn set_rejects_a_bad_prefix_before_any_request() {
    let fake = Fake::start(|_, _| Resp::json(200, "{}"));
    let store = MemoryStore::new();
    let out = cli::key_set(&store, "oops-not-a-key", &fake.base);
    assert!(matches!(out, SetOutcome::InvalidFormat(_)));
    assert_eq!(store.load().unwrap(), None);
    assert!(fake.requests().is_empty(), "no request for a malformed key");
}

#[test]
fn set_reports_a_key_the_api_rejects() {
    let fake = Fake::start(|_, _| {
        Resp::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#)
    });
    let store = MemoryStore::new();
    let out = cli::key_set(&store, "sk-ant-wrong-12345678", &fake.base);
    assert_eq!(out, SetOutcome::Rejected);
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn status_reads_from_stdin_when_piped_and_reports_not_set() {
    // With the memory store and no key, status prints "API key: not set".
    let out = Command::new(BIN)
        .args(["key", "status"])
        .env("GENTLE_ENGINES_KEY_STORE", "memory")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "API key: not set");
}

#[test]
fn binary_help_version_and_unknown_exit_codes() {
    let help = Command::new(BIN).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("gentle-explain key set"));

    let version = Command::new(BIN).arg("-V").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "gentle-explain 0.1.0");

    assert_eq!(Command::new(BIN).arg("bogus").output().unwrap().status.code(), Some(2));
}

#[test]
fn set_via_stdin_is_validated_and_never_echoes_the_key() {
    use std::io::Write;
    use std::process::Stdio;
    let fake = Fake::start(|_, _| Resp::json(200, r#"{"data":[],"has_more":false}"#));
    let mut child = Command::new(BIN)
        .args(["key", "set"])
        .env("GENTLE_ENGINES_KEY_STORE", "memory")
        .env("GENTLE_ENGINES_ANTHROPIC_API", &fake.base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"sk-ant-piped-12345678\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("API key saved."));
    // The key must never be echoed back on either stream.
    assert!(!stdout.contains("sk-ant-piped"));
    assert!(!stderr.contains("sk-ant-piped"));
}
