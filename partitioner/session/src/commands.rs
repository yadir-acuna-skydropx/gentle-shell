//! `attach`, `list` and `kill` subcommands (S34) plus the detached-server
//! spawn. `attach` re-execs this same binary as `__server …` with setsid and
//! /dev/null-or-log stdio, so the session keeps running after the terminal
//! closes; there is no fork-in-a-threaded-process and no shell invocation.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::client::run_client_stream;
use crate::meta::{self, Meta};
use crate::paths::{self, SessionPaths};

const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Attaches to the folder's session, spawning a detached server first if none
/// is live. Returns the exit code to propagate.
pub fn attach(dir: &Path, cmd: &[String]) -> Result<i32> {
    if cmd.is_empty() {
        bail!("attach requires a command after `--`");
    }
    let canonical = paths::canonical_dir(dir).context("resolving --dir")?;
    let session = paths::resolve(dir).context("resolving session paths")?;

    // Live session? Attach to it. This real connection is also the probe, so
    // the server flushes buffered output to us rather than to a throwaway.
    if let Ok(stream) = UnixStream::connect(&session.socket) {
        return run_client_stream(stream);
    }

    // No live session: start a detached server, then attach with the first
    // successful connection (again, no throwaway probe).
    spawn_detached_server(&session, &canonical, cmd).context("starting session server")?;
    let stream = connect_with_retry(&session.socket, SPAWN_CONNECT_TIMEOUT)
        .context("session server did not come up in time")?;
    run_client_stream(stream)
}

/// Re-execs this binary as the detached `__server`, with its own session
/// (setsid), stdin from /dev/null and stdout/stderr to the session log file.
fn spawn_detached_server(session: &SessionPaths, canonical: &Path, cmd: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("finding own executable")?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&session.log)
        .context("opening session log")?;
    let log_err = log.try_clone().context("cloning session log handle")?;
    let devnull = fs::OpenOptions::new().read(true).open("/dev/null").context("opening /dev/null")?;

    let mut command = Command::new(exe);
    command
        .arg("__server")
        .arg("--socket")
        .arg(&session.socket)
        .arg("--meta")
        .arg(&session.meta)
        .arg("--attached")
        .arg(&session.attached)
        .arg("--folder")
        .arg(canonical)
        .arg("--")
        .args(cmd)
        .stdin(devnull)
        .stdout(log)
        .stderr(log_err);
    // SAFETY: setsid in the child only detaches it into a new session; it is
    // async-signal-safe and touches no inherited locks.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Spawn and intentionally do not wait: the server lives on its own.
    command.spawn().context("spawning detached server")?;
    Ok(())
}

fn connect_with_retry(socket: &Path, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {}", socket.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `gentle-session list`: one row per live session.
pub fn list() -> Result<i32> {
    let dir = paths::ensure_state_dir()?;
    let mut rows: Vec<(Meta, bool, bool)> = Vec::new();
    for entry in fs::read_dir(&dir).context("reading state dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("meta") {
            continue;
        }
        let Ok(m) = Meta::read(&path) else { continue };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let socket = dir.join(format!("{stem}.sock"));
        let live = UnixStream::connect(&socket).is_ok() || pid_alive(m.server_pid);
        if !live {
            // Clean up stale sidecars so `list` stays accurate.
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(dir.join(format!("{stem}.attached")));
            let _ = fs::remove_file(&socket);
            continue;
        }
        let attached = meta::read_attached(&dir.join(format!("{stem}.attached")));
        rows.push((m, live, attached));
    }

    if rows.is_empty() {
        println!("no gentle-session sessions");
        return Ok(0);
    }
    println!("{:<40} {:>8} {:>10} {:>9}", "FOLDER", "PID", "AGE", "ATTACHED");
    let now = meta::now_secs();
    for (m, _live, attached) in rows {
        let age = now.saturating_sub(m.started);
        println!(
            "{:<40} {:>8} {:>10} {:>9}",
            truncate(&m.folder, 40),
            m.server_pid,
            human_age(age),
            if attached { "yes" } else { "no" }
        );
    }
    Ok(0)
}

/// `gentle-session kill [--dir DIR]`: terminate the session for a folder.
pub fn kill(dir: &Path) -> Result<i32> {
    let session = paths::resolve(dir).context("resolving session paths")?;
    let meta = match Meta::read(&session.meta) {
        Ok(m) => m,
        Err(_) => {
            // Nothing recorded: tidy any stray socket and report.
            let _ = fs::remove_file(&session.socket);
            println!("no session for {}", session.dir.display());
            return Ok(0);
        }
    };

    // Terminate the child's process group first; the server then observes the
    // exit, notifies the client, removes the socket and exits on its own.
    if meta.child_pid > 0 {
        send_signal(meta.child_pid, libc::SIGTERM);
        send_group_signal(meta.child_pid, libc::SIGTERM);
    }

    // Wait for the server to clean up the socket.
    let deadline = Instant::now() + KILL_CLEANUP_TIMEOUT;
    while session.socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Force-kill anything still alive and remove the sidecars.
    if session.socket.exists() || pid_alive(meta.server_pid) {
        if meta.child_pid > 0 {
            send_group_signal(meta.child_pid, libc::SIGKILL);
            send_signal(meta.child_pid, libc::SIGKILL);
        }
        if meta.server_pid > 0 {
            send_signal(meta.server_pid, libc::SIGKILL);
        }
        let _ = fs::remove_file(&session.socket);
        let _ = fs::remove_file(&session.meta);
        let _ = fs::remove_file(&session.attached);
    }
    println!("killed session for {}", session.dir.display());
    Ok(0)
}

fn send_signal(pid: i32, sig: libc::c_int) {
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

fn send_group_signal(pid: i32, sig: libc::c_int) {
    if pid > 0 {
        unsafe { libc::kill(-pid, sig) };
    }
}

/// True when `pid` exists (signal 0 probe).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let tail: String = s.chars().skip(count - keep).collect();
        format!("…{tail}")
    }
}

fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_age_buckets_by_unit() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(65), "1m");
        assert_eq!(human_age(3700), "1h");
        assert_eq!(human_age(90000), "1d");
    }

    #[test]
    fn truncate_keeps_short_strings_and_shortens_long_ones() {
        assert_eq!(truncate("short", 40), "short");
        let long = "a".repeat(80);
        let t = truncate(&long, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.starts_with('…'));
    }

    #[test]
    fn pid_alive_is_false_for_zero_and_true_for_self() {
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id() as i32));
    }
}
