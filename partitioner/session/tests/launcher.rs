//! Launcher dispatch for the gentle-shell function (S34). Sources the real
//! launcher script with stub binaries on PATH and asserts which one runs under
//! each environment, in particular that GENTLE_SESSION set never nests a
//! session.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn launcher_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/gentle-shell-launcher.sh")
}

struct Stubs {
    dir: PathBuf,
}

impl Stubs {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gs-launch-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        for name in ["gentle-partitioner", "gentle-session", "gentle-shell"] {
            let p = dir.join(name);
            fs::write(&p, format!("#!/bin/sh\necho \"STUB:{name} $*\"\n")).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Stubs { dir }
    }

    /// Runs `gentle-shell foo` under the launcher with the given extra env and
    /// returns stdout.
    fn run(&self, env: &[(&str, &str)]) -> String {
        let script = format!(
            ". '{}'\ngentle-shell foo\n",
            launcher_path().display()
        );
        let path = format!("{}:{}", self.dir.display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(script).env("PATH", path);
        // Clear the session-related vars, then apply the case.
        for k in ["GENTLE_PARTITIONER", "GENTLE_SESSION", "GENTLE_NO_SESSION"] {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run bash");
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Stubs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn default_wraps_the_partitioner_in_a_session() {
    let stubs = Stubs::new();
    let out = stubs.run(&[]);
    assert!(out.contains("STUB:gentle-session attach --dir"), "should attach a session: {out}");
    assert!(out.contains("-- gentle-partitioner foo"), "should run the partitioner inside: {out}");
}

#[test]
fn gentle_session_env_does_not_nest_a_session() {
    let stubs = Stubs::new();
    let out = stubs.run(&[("GENTLE_SESSION", "1")]);
    assert!(out.contains("STUB:gentle-partitioner foo"), "runs the partitioner directly: {out}");
    assert!(!out.contains("STUB:gentle-session"), "must not start a nested session: {out}");
}

#[test]
fn opt_out_env_skips_the_session() {
    let stubs = Stubs::new();
    let out = stubs.run(&[("GENTLE_NO_SESSION", "1")]);
    assert!(out.contains("STUB:gentle-partitioner foo"), "runs the partitioner directly: {out}");
    assert!(!out.contains("STUB:gentle-session"), "opt-out must skip the session: {out}");
}

#[test]
fn inside_the_partitioner_runs_the_real_engine() {
    let stubs = Stubs::new();
    let out = stubs.run(&[("GENTLE_PARTITIONER", "1")]);
    assert!(out.contains("STUB:gentle-shell foo"), "runs the real gentle-shell: {out}");
    assert!(!out.contains("gentle-partitioner"), "must not relaunch the partitioner: {out}");
}
