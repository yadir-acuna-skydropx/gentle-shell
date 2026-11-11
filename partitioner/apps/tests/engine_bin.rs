//! Spawns the built `gentle-apps` binary and exercises the stdio protocol.
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_gentle-apps");

#[test]
fn first_apps_line_lists_slack_disconnected_then_shutdown_exits_zero() {
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gentle-apps");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // The first line is the full app list; Slack is present and disconnected.
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.contains("\"type\":\"apps\""), "first line: {first}");
    assert!(first.contains("\"id\":\"slack\""), "first line: {first}");
    assert!(first.contains("\"status\":\"disconnected\""), "first line: {first}");

    // A clean shutdown request exits 0.
    stdin.write_all(b"{\"type\":\"shutdown\"}\n").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "shutdown should exit 0");
}

#[test]
fn version_and_help_and_slack_login_placeholder() {
    let v = Command::new(BIN).arg("--version").output().unwrap();
    assert_eq!(v.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&v.stdout).contains("gentle-apps"));

    let h = Command::new(BIN).arg("--help").output().unwrap();
    assert_eq!(h.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&h.stdout).contains("apps engine"));

    let login = Command::new(BIN).args(["slack", "login"]).output().unwrap();
    assert_eq!(login.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&login.stderr).contains("not implemented yet (T13)"));
}
