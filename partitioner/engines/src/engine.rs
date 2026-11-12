//! The `gentle-explain` stdio engine: reads client NDJSON on `input`, runs one
//! request at a time against an [`ExplainBackend`] on a worker thread, and
//! writes engine NDJSON on `output`. A `cancel` for the in-flight id stops its
//! deltas. Logs go to stderr; the API key never reaches this layer's output.
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::protocol::{parse_client_line, ClientMsg, EngineMsg, KeyState, Parsed};

/// A shared cancel signal for the in-flight request.
#[derive(Clone)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    fn new() -> Self {
        CancelFlag(Arc::new(AtomicBool::new(false)))
    }
    /// True once the request has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The capped-less raw inputs of an explain request (the backend caps them).
pub struct ExplainInput {
    pub path: String,
    pub language: String,
    pub selection: String,
    pub context_before: String,
    pub context_after: String,
}

/// Produces explanation text. Implemented by the Anthropic backend and by test
/// mocks. One call is in flight at a time; `sink` streams text, `cancel` lets a
/// long call stop early.
pub trait ExplainBackend: Send + 'static {
    /// Whether a key is configured and the resolved model id (for the start
    /// `status` message).
    fn status(&self) -> (KeyState, String);
    /// Explains `input`, starting a fresh conversation.
    fn explain(&mut self, input: &ExplainInput, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<(), String>;
    /// Answers a follow-up in the conversation started by the last `explain`.
    fn ask(&mut self, question: &str, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<(), String>;
}

enum Req {
    Explain(ExplainInput),
    Ask(String),
}

enum Ev {
    Client(ClientMsg),
    Emit(EngineMsg),
    RequestDone,
    InputClosed,
}

/// Runs the engine until `shutdown` or input EOF.
pub fn run<B, R, W>(backend: B, input: R, mut output: W) -> std::io::Result<()>
where
    B: ExplainBackend,
    R: BufRead + Send + 'static,
    W: Write,
{
    let backend = Arc::new(Mutex::new(backend));
    let (key, model) = backend.lock().unwrap().status();
    write_msg(&mut output, &EngineMsg::Status { key, model })?;

    let (tx, rx) = mpsc::channel::<Ev>();
    spawn_reader(input, tx.clone());

    // Requests run one at a time, in order: `ask` continues the conversation the
    // previous `explain` started, so a new request never cancels the running
    // one — it queues. Only an explicit `cancel` stops the in-flight request.
    let mut queue: VecDeque<(u64, Req)> = VecDeque::new();
    let mut current: Option<(u64, CancelFlag)> = None;
    let mut worker: Option<thread::JoinHandle<()>> = None;
    let mut shutting_down = false;

    while let Ok(ev) = rx.recv() {
        match ev {
            Ev::Client(ClientMsg::Shutdown) | Ev::InputClosed => {
                shutting_down = true;
                if current.is_none() && queue.is_empty() {
                    break;
                }
            }
            Ev::Client(ClientMsg::Explain { id, path, language, selection, context_before, context_after }) => {
                if !shutting_down {
                    let input = ExplainInput { path, language, selection, context_before, context_after };
                    queue.push_back((id, Req::Explain(input)));
                    start_next(&backend, &tx, &mut queue, &mut current, &mut worker);
                }
            }
            Ev::Client(ClientMsg::Ask { id, question }) => {
                if !shutting_down {
                    queue.push_back((id, Req::Ask(question)));
                    start_next(&backend, &tx, &mut queue, &mut current, &mut worker);
                }
            }
            Ev::Client(ClientMsg::Cancel { id }) => {
                if let Some((cur, flag)) = &current {
                    if *cur == id {
                        flag.cancel();
                    }
                }
                // A still-queued request can be cancelled before it starts.
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

    // Let any in-flight worker finish and flush its tail.
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

/// Starts the next queued request when the worker is idle.
fn start_next<B: ExplainBackend>(
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
        let flag = CancelFlag::new();
        *current = Some((id, flag.clone()));
        *worker = Some(spawn_request(backend.clone(), tx.clone(), id, flag, req));
    }
}

fn spawn_request(
    backend: Arc<Mutex<impl ExplainBackend>>,
    tx: Sender<Ev>,
    id: u64,
    flag: CancelFlag,
    req: Req,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = {
            let sink_tx = tx.clone();
            // Deltas already produced are forwarded; cancellation stops further
            // production in the backend and suppresses the terminal Done/Error.
            let mut sink = move |text: &str| {
                let _ = sink_tx.send(Ev::Emit(EngineMsg::Delta { id, text: text.to_string() }));
            };
            let mut b = backend.lock().unwrap();
            match req {
                Req::Explain(input) => b.explain(&input, &mut sink, &flag),
                Req::Ask(q) => b.ask(&q, &mut sink, &flag),
            }
        };
        if !flag.is_cancelled() {
            let msg = match result {
                Ok(()) => EngineMsg::Done { id },
                Err(message) => EngineMsg::Error { id, message },
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
                Parsed::Oversize => eprintln!("gentle-explain: skipped an oversized input line"),
                Parsed::Invalid => eprintln!("gentle-explain: skipped an invalid input line"),
                Parsed::Ignore => {}
            }
        }
        let _ = tx.send(Ev::InputClosed);
    });
}

fn write_msg<W: Write>(output: &mut W, msg: &EngineMsg) -> std::io::Result<()> {
    output.write_all(msg.to_line().as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicU32;

    /// A controllable backend for engine tests.
    struct Mock {
        key: KeyState,
        model: String,
        /// Deltas to emit for explain/ask.
        deltas: Vec<String>,
        /// Count of explain + ask calls, to prove the conversation continues.
        calls: Arc<AtomicU32>,
        /// When true, explain loops until cancelled (for the cancel test).
        loop_until_cancel: bool,
        fail: bool,
    }

    impl ExplainBackend for Mock {
        fn status(&self) -> (KeyState, String) {
            (self.key, self.model.clone())
        }
        fn explain(&mut self, _input: &ExplainInput, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err("backend boom".into());
            }
            if self.loop_until_cancel {
                sink("one");
                while !cancel.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                return Ok(());
            }
            for d in &self.deltas {
                sink(d);
            }
            Ok(())
        }
        fn ask(&mut self, _q: &str, sink: &mut dyn FnMut(&str), _cancel: &CancelFlag) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            sink("answer");
            Ok(())
        }
    }

    fn run_collect(mock: Mock, input: &str) -> Vec<EngineMsg> {
        let mut out: Vec<u8> = Vec::new();
        run(mock, Cursor::new(input.to_string()), &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str::<EngineMsg>(l).unwrap()).collect()
    }

    fn mock(deltas: &[&str]) -> Mock {
        Mock {
            key: KeyState::Set,
            model: "claude-haiku".into(),
            deltas: deltas.iter().map(|s| s.to_string()).collect(),
            calls: Arc::new(AtomicU32::new(0)),
            loop_until_cancel: false,
            fail: false,
        }
    }

    #[test]
    fn status_is_emitted_first() {
        let msgs = run_collect(mock(&[]), "{\"type\":\"shutdown\"}\n");
        assert!(matches!(msgs[0], EngineMsg::Status { key: KeyState::Set, .. }));
    }

    #[test]
    fn explain_streams_deltas_then_done() {
        let input = concat!(
            "{\"type\":\"explain\",\"id\":1,\"path\":\"a\",\"language\":\"rust\",\"selection\":\"fn x(){}\"}\n",
            "{\"type\":\"shutdown\"}\n",
        );
        let msgs = run_collect(mock(&["Hola", " mundo"]), input);
        let deltas: Vec<_> = msgs
            .iter()
            .filter_map(|m| if let EngineMsg::Delta { id: 1, text } = m { Some(text.clone()) } else { None })
            .collect();
        assert_eq!(deltas, vec!["Hola", " mundo"]);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 1 })));
    }

    #[test]
    fn ask_runs_after_explain_in_the_same_conversation() {
        let calls = Arc::new(AtomicU32::new(0));
        let m = Mock {
            key: KeyState::Set,
            model: "m".into(),
            deltas: vec!["x".into()],
            calls: calls.clone(),
            loop_until_cancel: false,
            fail: false,
        };
        let input = concat!(
            "{\"type\":\"explain\",\"id\":1,\"path\":\"a\",\"language\":\"rust\",\"selection\":\"s\"}\n",
            "{\"type\":\"ask\",\"id\":2,\"question\":\"¿por qué?\"}\n",
            "{\"type\":\"shutdown\"}\n",
        );
        let msgs = run_collect(m, input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 1 })));
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Delta { id: 2, .. })));
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 2 })));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_failing_explain_reports_an_error() {
        let mut m = mock(&[]);
        m.fail = true;
        let input = concat!(
            "{\"type\":\"explain\",\"id\":9,\"path\":\"a\",\"language\":\"rust\",\"selection\":\"s\"}\n",
            "{\"type\":\"shutdown\"}\n",
        );
        let msgs = run_collect(m, input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Error { id: 9, message } if message == "backend boom")));
    }

    #[test]
    fn cancel_stops_the_deltas_and_suppresses_done() {
        let mut m = mock(&[]);
        m.loop_until_cancel = true;
        let input = concat!(
            "{\"type\":\"explain\",\"id\":1,\"path\":\"a\",\"language\":\"rust\",\"selection\":\"s\"}\n",
            "{\"type\":\"cancel\",\"id\":1}\n",
            "{\"type\":\"shutdown\"}\n",
        );
        let msgs = run_collect(m, input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Delta { id: 1, .. })));
        // A cancelled request emits neither Done nor Error.
        assert!(!msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 1 } | EngineMsg::Error { id: 1, .. })));
    }
}
