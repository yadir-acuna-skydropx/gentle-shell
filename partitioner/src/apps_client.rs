//! Supervises the independent `gentle-apps` engine process (S12).
//!
//! A manager thread spawns `gentle-apps`, reads its NDJSON on a reader thread
//! (parsing each line into an [`AppsEvent`] for the UI channel so redraws happen
//! only on events, S11) and writes requests with incrementing ids. If the child
//! exits or is missing the panel goes offline and the child is restarted with
//! exponential backoff. It never blocks or crashes the UI; the child is killed
//! on quit.
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gentle_apps::protocol::{ClientMsg, EngineMsg, MAX_LINE};

/// Events the client feeds into the UI channel.
#[derive(Debug, Clone)]
pub enum AppsEvent {
    /// A decoded message from the engine.
    Msg(EngineMsg),
    /// The engine is not running (missing binary or it exited).
    Offline,
}

/// Commands to the manager thread.
enum Manage {
    Send(ClientMsg),
    ChildDied,
    Shutdown,
}

/// Handle to the apps engine supervisor.
pub struct AppsClient {
    tx: Sender<Manage>,
    child: Arc<Mutex<Option<Child>>>,
}

impl AppsClient {
    /// Spawns the supervisor. Engine events are sent on `events`; the receiver
    /// (the UI loop) must drain them.
    pub fn spawn(events: Sender<AppsEvent>) -> AppsClient {
        AppsClient::spawn_with(events, spawn_engine)
    }

    /// Supervisor with an injectable child spawner (for tests).
    fn spawn_with(
        events: Sender<AppsEvent>,
        spawn_fn: impl Fn() -> Option<Child> + Send + Sync + 'static,
    ) -> AppsClient {
        let (tx, rx) = mpsc::channel::<Manage>();
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let spawn_fn = Arc::new(spawn_fn);
        {
            let tx = tx.clone();
            let child = child.clone();
            thread::Builder::new()
                .name("apps-manager".into())
                .spawn(move || manager(rx, tx, child, events, spawn_fn))
                .expect("spawn apps manager");
        }
        AppsClient { tx, child }
    }

    /// Queues a request for the engine (ignored while offline).
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(Manage::Send(msg));
    }

    /// Stops the supervisor and kills the child (called on quit).
    pub fn shutdown(&self) {
        let _ = self.tx.send(Manage::Shutdown);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Exponential backoff before restarting the engine: 1, 2, 4, 8, 16, then 30s.
/// `attempt` starts at 1.
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_secs((1u64 << shift).min(30))
}

/// Resolves the `gentle-apps` binary: next to the current executable first,
/// then on PATH.
pub fn resolve_engine() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("gentle-apps");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("gentle-apps");
        candidate.is_file().then_some(candidate)
    })
}

fn manager(
    rx: Receiver<Manage>,
    self_tx: Sender<Manage>,
    child_slot: Arc<Mutex<Option<Child>>>,
    events: Sender<AppsEvent>,
    spawn_fn: Arc<dyn Fn() -> Option<Child> + Send + Sync>,
) {
    let mut attempt: u32 = 0;
    loop {
        match spawn_fn() {
            Some(mut child) => {
                attempt = 0;
                let mut stdin = child.stdin.take();
                if let Some(stdout) = child.stdout.take() {
                    let events = events.clone();
                    let died_tx = self_tx.clone();
                    thread::spawn(move || reader(stdout, events, died_tx));
                }
                *child_slot.lock().unwrap() = Some(child);

                // Serve requests until the child dies or we are told to stop.
                loop {
                    match rx.recv() {
                        Ok(Manage::Send(msg)) => {
                            let ok = stdin
                                .as_mut()
                                .map(|w| {
                                    let line = msg.to_line();
                                    w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.flush()).is_ok()
                                })
                                .unwrap_or(false);
                            if !ok {
                                break; // treat a write failure as a dead child
                            }
                        }
                        Ok(Manage::ChildDied) => break,
                        Ok(Manage::Shutdown) | Err(_) => {
                            if let Some(mut c) = child_slot.lock().unwrap().take() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            return;
                        }
                    }
                }

                // Child gone: reap it and report offline.
                if let Some(mut c) = child_slot.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = events.send(AppsEvent::Offline);
            }
            None => {
                let _ = events.send(AppsEvent::Offline);
            }
        }

        attempt += 1;
        // Back off before restarting, but honour a shutdown that arrives meanwhile.
        match rx.recv_timeout(backoff_delay(attempt)) {
            Ok(Manage::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            _ => {}
        }
    }
}

/// Spawns the engine with piped stdio; stderr is discarded so engine logs never
/// corrupt the alternate-screen TUI.
fn spawn_engine() -> Option<Child> {
    let bin = resolve_engine()?;
    Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Reads NDJSON from the engine, decoding each line into an [`AppsEvent::Msg`].
/// Oversized or invalid lines are skipped. On EOF it signals the manager.
fn reader(stdout: std::process::ChildStdout, events: Sender<AppsEvent>, died_tx: Sender<Manage>) {
    let mut buf = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                if line.len() > MAX_LINE {
                    eprintln!("gentle-partitioner: skipped an oversized apps line");
                    continue;
                }
                match serde_json::from_str::<EngineMsg>(line.trim()) {
                    Ok(msg) => {
                        if events.send(AppsEvent::Msg(msg)).is_err() {
                            break;
                        }
                    }
                    Err(_) => eprintln!("gentle-partitioner: skipped an invalid apps line"),
                }
            }
            Err(_) => break,
        }
    }
    let _ = died_tx.send(Manage::ChildDied);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_is_one_two_four_capped_at_thirty() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(16));
        assert_eq!(backoff_delay(6), Duration::from_secs(30));
        assert_eq!(backoff_delay(7), Duration::from_secs(30));
        assert_eq!(backoff_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn a_missing_engine_reports_offline_without_panicking() {
        // Injected spawner always fails, as if the binary were missing: the
        // client must report Offline and never panic.
        let (tx, rx) = mpsc::channel::<AppsEvent>();
        let client = AppsClient::spawn_with(tx, || None);
        let ev = rx.recv_timeout(Duration::from_secs(2)).expect("an offline event");
        assert!(matches!(ev, AppsEvent::Offline));
        // Sending while offline is a no-op, not a crash.
        client.send(ClientMsg::Refresh { id: 1, app: "slack".into() });
        client.shutdown();
    }
}
