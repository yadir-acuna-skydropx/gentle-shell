//! The `gentle-arch` stdio engine (S18): reads client NDJSON, runs one
//! review/design at a time against an [`ArchRunner`] on a worker thread, and
//! writes engine NDJSON. A `cancel` for the in-flight id stops its deltas. Logs
//! go to stderr; no secret reaches this layer's output.
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::arch_protocol::{parse_client_line, ArchClientMsg, ArchEngineMsg, KeyState, Parsed};
use crate::engine::CancelFlag;

/// Runs architecture reviews and designs. One call is in flight at a time.
pub trait ArchRunner: Send + 'static {
    /// Key presence, resolved default model, and KB chunk count.
    fn status(&mut self) -> (KeyState, String, usize);
    /// Reviews the repo at `path` (empty = cwd). Returns the report file path.
    fn review(&mut self, path: &str, deep: bool, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<String, String>;
    /// Designs from `brief`. Returns the report file path.
    fn design(&mut self, brief: &str, deep: bool, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<String, String>;
}

enum Req {
    Review { path: String, deep: bool },
    Design { brief: String, deep: bool },
}

enum Ev {
    Client(ArchClientMsg),
    Emit(ArchEngineMsg),
    RequestDone,
    InputClosed,
}

/// Runs the engine until `shutdown` or input EOF.
pub fn run<B, R, W>(backend: B, input: R, mut output: W) -> std::io::Result<()>
where
    B: ArchRunner,
    R: BufRead + Send + 'static,
    W: Write,
{
    let backend = Arc::new(Mutex::new(backend));
    let (key, model, kb_chunks) = backend.lock().unwrap().status();
    write_msg(&mut output, &ArchEngineMsg::Status { key, model, kb_chunks })?;

    let (tx, rx) = mpsc::channel::<Ev>();
    spawn_reader(input, tx.clone());

    let mut queue: VecDeque<(u64, Req)> = VecDeque::new();
    let mut current: Option<(u64, CancelFlag)> = None;
    let mut worker: Option<thread::JoinHandle<()>> = None;
    let mut shutting_down = false;

    while let Ok(ev) = rx.recv() {
        match ev {
            Ev::Client(ArchClientMsg::Shutdown) | Ev::InputClosed => {
                shutting_down = true;
                if current.is_none() && queue.is_empty() {
                    break;
                }
            }
            Ev::Client(ArchClientMsg::Review { id, path, deep }) => {
                if !shutting_down {
                    queue.push_back((id, Req::Review { path, deep }));
                    start_next(&backend, &tx, &mut queue, &mut current, &mut worker);
                }
            }
            Ev::Client(ArchClientMsg::Design { id, brief, deep }) => {
                if !shutting_down {
                    queue.push_back((id, Req::Design { brief, deep }));
                    start_next(&backend, &tx, &mut queue, &mut current, &mut worker);
                }
            }
            Ev::Client(ArchClientMsg::Cancel { id }) => {
                if let Some((cur, flag)) = &current {
                    if *cur == id {
                        flag.cancel_now();
                    }
                }
                queue.retain(|(qid, _)| *qid != id);
            }
            Ev::Emit(msg) => write_msg(&mut output, &msg)?,
            Ev::RequestDone => {
                if let Some(h) = worker.take() {
                    let _ = h.join();
                }
                current = None;
                start_next(&backend, &tx, &mut queue, &mut current, &mut worker);
                if shutting_down && current.is_none() && queue.is_empty() {
                    break;
                }
            }
        }
    }

    if let Some(h) = worker.take() {
        let _ = h.join();
    }
    while let Ok(ev) = rx.try_recv() {
        if let Ev::Emit(msg) = ev {
            let _ = write_msg(&mut output, &msg);
        }
    }
    Ok(())
}

fn start_next<B: ArchRunner>(
    backend: &Arc<Mutex<B>>,
    tx: &Sender<Ev>,
    queue: &mut VecDeque<(u64, Req)>,
    current: &mut Option<(u64, CancelFlag)>,
    worker: &mut Option<thread::JoinHandle<()>>,
) {
    if current.is_some() {
        return;
    }
    if let Some((id, req)) = queue.pop_front() {
        let flag = CancelFlag::new_flag();
        *current = Some((id, flag.clone()));
        *worker = Some(spawn_request(backend.clone(), tx.clone(), id, flag, req));
    }
}

fn spawn_request(
    backend: Arc<Mutex<impl ArchRunner>>,
    tx: Sender<Ev>,
    id: u64,
    flag: CancelFlag,
    req: Req,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = {
            let sink_tx = tx.clone();
            let mut sink = move |text: &str| {
                let _ = sink_tx.send(Ev::Emit(ArchEngineMsg::Delta { id, text: text.to_string() }));
            };
            let mut b = backend.lock().unwrap();
            match req {
                Req::Review { path, deep } => b.review(&path, deep, &mut sink, &flag),
                Req::Design { brief, deep } => b.design(&brief, deep, &mut sink, &flag),
            }
        };
        if !flag.is_cancelled() {
            let msg = match result {
                Ok(path) => ArchEngineMsg::Done { id, path },
                Err(message) => ArchEngineMsg::Error { id, message },
            };
            let _ = tx.send(Ev::Emit(msg));
        }
        let _ = tx.send(Ev::RequestDone);
    })
}

fn spawn_reader<R: BufRead + Send + 'static>(input: R, tx: Sender<Ev>) {
    thread::spawn(move || {
        for line in input.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            match parse_client_line(&line) {
                Parsed::Msg(msg) => {
                    if tx.send(Ev::Client(msg)).is_err() {
                        return;
                    }
                }
                Parsed::Oversize => eprintln!("gentle-arch: skipped an oversized input line"),
                Parsed::Invalid => eprintln!("gentle-arch: skipped an invalid input line"),
                Parsed::Ignore => {}
            }
        }
        let _ = tx.send(Ev::InputClosed);
    });
}

fn write_msg<W: Write>(output: &mut W, msg: &ArchEngineMsg) -> std::io::Result<()> {
    output.write_all(msg.to_line().as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Mock {
        loop_until_cancel: bool,
        fail: bool,
        calls: Arc<AtomicU32>,
    }

    impl ArchRunner for Mock {
        fn status(&mut self) -> (KeyState, String, usize) {
            (KeyState::Set, "claude-sonnet".into(), 7)
        }
        fn review(&mut self, _path: &str, _deep: bool, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err("scan boom".into());
            }
            if self.loop_until_cancel {
                sink("partial");
                while !cancel.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                return Ok("unused".into());
            }
            sink("# Report");
            Ok("./.gentle-arch/review-ts.md".into())
        }
        fn design(&mut self, _brief: &str, _deep: bool, sink: &mut dyn FnMut(&str), _cancel: &CancelFlag) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            sink("# Design");
            Ok("./.gentle-arch/design-ts.md".into())
        }
    }

    fn run_collect(mock: Mock, input: &str) -> Vec<ArchEngineMsg> {
        let mut out: Vec<u8> = Vec::new();
        run(mock, Cursor::new(input.to_string()), &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str::<ArchEngineMsg>(l).unwrap()).collect()
    }

    fn mock() -> Mock {
        Mock { loop_until_cancel: false, fail: false, calls: Arc::new(AtomicU32::new(0)) }
    }

    #[test]
    fn status_is_emitted_first_with_kb_chunks() {
        let msgs = run_collect(mock(), "{\"type\":\"shutdown\"}\n");
        assert!(matches!(msgs[0], ArchEngineMsg::Status { key: KeyState::Set, kb_chunks: 7, .. }));
    }

    #[test]
    fn review_streams_then_done_with_path() {
        let input = concat!("{\"type\":\"review\",\"id\":1,\"path\":\"\"}\n", "{\"type\":\"shutdown\"}\n");
        let msgs = run_collect(mock(), input);
        assert!(msgs.iter().any(|m| matches!(m, ArchEngineMsg::Delta { id: 1, text } if text == "# Report")));
        assert!(msgs.iter().any(|m| matches!(m, ArchEngineMsg::Done { id: 1, path } if path.ends_with("review-ts.md"))));
    }

    #[test]
    fn design_runs_and_reports_its_file() {
        let input = concat!("{\"type\":\"design\",\"id\":2,\"brief\":\"banco\"}\n", "{\"type\":\"shutdown\"}\n");
        let msgs = run_collect(mock(), input);
        assert!(msgs.iter().any(|m| matches!(m, ArchEngineMsg::Done { id: 2, path } if path.ends_with("design-ts.md"))));
    }

    #[test]
    fn a_failing_review_reports_an_error() {
        let mut m = mock();
        m.fail = true;
        let input = concat!("{\"type\":\"review\",\"id\":9}\n", "{\"type\":\"shutdown\"}\n");
        let msgs = run_collect(m, input);
        assert!(msgs.iter().any(|m| matches!(m, ArchEngineMsg::Error { id: 9, message } if message == "scan boom")));
    }

    #[test]
    fn cancel_stops_deltas_and_suppresses_done() {
        let mut m = mock();
        m.loop_until_cancel = true;
        let input = concat!(
            "{\"type\":\"review\",\"id\":1}\n",
            "{\"type\":\"cancel\",\"id\":1}\n",
            "{\"type\":\"shutdown\"}\n",
        );
        let msgs = run_collect(m, input);
        assert!(msgs.iter().any(|m| matches!(m, ArchEngineMsg::Delta { id: 1, .. })));
        assert!(!msgs.iter().any(|m| matches!(m, ArchEngineMsg::Done { id: 1, .. } | ArchEngineMsg::Error { id: 1, .. })));
    }
}
