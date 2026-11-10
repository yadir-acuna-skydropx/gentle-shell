//! Binary-level checks for argument handling; they never open a terminal.
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_gentle-partitioner");

#[test]
fn missing_command_exits_one_with_message() {
    let out = Command::new(BIN).args(["--", "no-such-cmd-xyz"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("command not found: no-such-cmd-xyz"), "{err}");
}

#[test]
fn help_version_and_unknown_option_exit_codes() {
    let help = Command::new(BIN).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("gentle-partitioner [OPTIONS] [-- <command> [args...]]"));
    let version = Command::new(BIN).arg("-V").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "gentle-partitioner 0.1.0");
    assert_eq!(Command::new(BIN).arg("--bogus").output().unwrap().status.code(), Some(2));
}
