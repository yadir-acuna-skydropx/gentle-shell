//! Supervises the independent `gentle-explain` engine process (S17).
//!
//! A manager thread spawns `gentle-explain`, reads its NDJSON on a reader thread
//! (parsing each line into an [`ExplainEvent`] for the UI channel so redraws
//! happen only on events, S11) and writes requests. If the child exits or is
//! missing the popup goes offline and the child is restarted with exponential
//! backoff. It never blocks or crashes the UI; the child is killed on quit.
//!
//! This mirrors [`crate::apps_client`] and reuses the same backoff schedule.
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use gentle_engines::protocol::{ClientMsg, EngineMsg, MAX_LINE};

/// Events the client feeds into the UI channel.
#[derive(Debug, Clone)]
pub enum ExplainEvent {
    /// A decoded message from the engine.
    Msg(EngineMsg),
    /// The engine is not running (missing binary or it exited).
    Offline,
}

enum Manage {
    Send(ClientMsg),
    ChildDied,
    Shutdown,
}

/// Handle to the explainer engine supervisor.
pub struct ExplainClient {
    tx: Sender<Manage>,
    child: Arc<Mutex<Option<Child>>>,
}

impl ExplainClient {
    /// Spawns the supervisor. Engine events are sent on `events`.
    pub fn spawn(events: Sender<ExplainEvent>) -> ExplainClient {
        ExplainClient::spawn_with(events, spawn_engine)
    }

    fn spawn_with(
        events: Sender<ExplainEvent>,
        spawn_fn: impl Fn() -> Option<Child> + Send + Sync + 'static,
    ) -> ExplainClient {
        let (tx, rx) = mpsc::channel::<Manage>();
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let spawn_fn = Arc::new(spawn_fn);
        {
            let tx = tx.clone();
            let child = child.clone();
            thread::Builder::new()
                .name("explain-manager".into())
                .spawn(move || manager(rx, tx, child, events, spawn_fn))
                .expect("spawn explain manager");
        }
        ExplainClient { tx, child }
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
pub fn backoff_delay(attempt: u32) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(5);
    std::time::Duration::from_secs((1u64 << shift).min(30))
}

/// Resolves the `gentle-explain` binary: next to the current executable first,
/// then on PATH.
pub fn resolve_engine() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("gentle-explain");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("gentle-explain");
        candidate.is_file().then_some(candidate)
    })
}

fn manager(
    rx: Receiver<Manage>,
    self_tx: Sender<Manage>,
    child_slot: Arc<Mutex<Option<Child>>>,
    events: Sender<ExplainEvent>,
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

                loop {
                    match rx.recv() {
                        Ok(Manage::Send(msg)) => {
                            let ok = stdin
                                .as_mut()
                                .map(|w| {
                                    let line = msg.to_line();
                                    w.write_all(line.as_bytes())
                                        .and_then(|_| w.write_all(b"\n"))
                                        .and_then(|_| w.flush())
                                        .is_ok()
                                })
                                .unwrap_or(false);
                            if !ok {
                                break;
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

                if let Some(mut c) = child_slot.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = events.send(ExplainEvent::Offline);
            }
            None => {
                let _ = events.send(ExplainEvent::Offline);
            }
        }

        attempt += 1;
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

fn reader(stdout: std::process::ChildStdout, events: Sender<ExplainEvent>, died_tx: Sender<Manage>) {
    let mut buf = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.len() > MAX_LINE {
                    eprintln!("gentle-partitioner: skipped an oversized explain line");
                    continue;
                }
                match serde_json::from_str::<EngineMsg>(line.trim()) {
                    Ok(msg) => {
                        if events.send(ExplainEvent::Msg(msg)).is_err() {
                            break;
                        }
                    }
                    Err(_) => eprintln!("gentle-partitioner: skipped an invalid explain line"),
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
    use std::time::Duration;

    #[test]
    fn backoff_schedule_matches_apps_client() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(6), Duration::from_secs(30));
        assert_eq!(backoff_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn a_missing_engine_reports_offline_without_panicking() {
        let (tx, rx) = mpsc::channel::<ExplainEvent>();
        let client = ExplainClient::spawn_with(tx, || None);
        let ev = rx.recv_timeout(Duration::from_secs(2)).expect("an offline event");
        assert!(matches!(ev, ExplainEvent::Offline));
        client.send(ClientMsg::Cancel { id: 1 });
        client.shutdown();
    }
}
