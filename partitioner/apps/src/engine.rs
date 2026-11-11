//! The stdio engine: reads client NDJSON on `input`, drives every connector on
//! its own thread, and writes engine NDJSON on `output`. Logs go to stderr.
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;

use crate::connector::Connector;
use crate::protocol::{parse_client_line, AppStatus, ClientMsg, EngineMsg, Parsed};

/// A command delivered to one connector's thread.
enum Cmd {
    Reply { id: u64, item: String, text: String },
    Refresh { id: u64 },
    Stop,
}

/// Internal events multiplexed onto the engine's main channel.
enum Ev {
    /// A message ready to be written to `output`.
    Emit(EngineMsg),
    /// A connector reported a (possibly) new status.
    Status { id: String, status: crate::protocol::Status, detail: Option<String> },
    /// A parsed client message to route.
    Client(ClientMsg),
    /// The input stream reached EOF.
    InputClosed,
}

/// Runs the engine until a `shutdown` message or input EOF, then stops every
/// connector and drains pending output. Returns once everything is flushed.
pub fn run<R, W>(connectors: Vec<Box<dyn Connector>>, input: R, mut output: W) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (tx, rx) = mpsc::channel::<Ev>();

    // Build the initial app list and spawn one thread per connector.
    let mut apps: Vec<AppStatus> = Vec::new();
    let mut senders: Vec<(String, Sender<Cmd>)> = Vec::new();
    let mut handles = Vec::new();
    for connector in connectors {
        apps.push(AppStatus {
            id: connector.id().to_string(),
            name: connector.name().to_string(),
            status: connector.status(),
            detail: connector.detail(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        senders.push((connector.id().to_string(), cmd_tx));
        let ev_tx = tx.clone();
        handles.push(thread::spawn(move || connector_loop(connector, cmd_rx, ev_tx)));
    }

    // The input reader runs on its own thread so a slow connector never blocks it.
    spawn_reader(input, tx.clone());

    // Announce the full app list up front.
    write_msg(&mut output, &EngineMsg::Apps { apps: apps.clone() })?;

    while let Ok(ev) = rx.recv() {
        match ev {
            Ev::Emit(msg) => write_msg(&mut output, &msg)?,
            Ev::Status { id, status, detail } => {
                if let Some(a) = apps.iter_mut().find(|a| a.id == id) {
                    if a.status != status || a.detail != detail {
                        a.status = status;
                        a.detail = detail;
                        write_msg(&mut output, &EngineMsg::Apps { apps: apps.clone() })?;
                    }
                }
            }
            Ev::Client(ClientMsg::Shutdown) => break,
            Ev::InputClosed => break,
            Ev::Client(ClientMsg::Reply { id, app, item, text }) => {
                match senders.iter().find(|(a, _)| *a == app) {
                    Some((_, s)) => {
                        let _ = s.send(Cmd::Reply { id, item, text });
                    }
                    None => write_msg(&mut output, &unknown_app(id, &app))?,
                }
            }
            Ev::Client(ClientMsg::Refresh { id, app }) => match senders.iter().find(|(a, _)| *a == app) {
                Some((_, s)) => {
                    let _ = s.send(Cmd::Refresh { id });
                }
                None => write_msg(&mut output, &unknown_app(id, &app))?,
            },
        }
    }

    // Stop every connector and let any in-flight reply finish and report.
    for (_, s) in &senders {
        let _ = s.send(Cmd::Stop);
    }
    senders.clear();
    for h in handles {
        let _ = h.join();
    }
    while let Ok(ev) = rx.try_recv() {
        match ev {
            Ev::Emit(msg) => write_msg(&mut output, &msg)?,
            Ev::Status { id, status, detail } => {
                if let Some(a) = apps.iter_mut().find(|a| a.id == id) {
                    if a.status != status || a.detail != detail {
                        a.status = status;
                        a.detail = detail;
                        write_msg(&mut output, &EngineMsg::Apps { apps: apps.clone() })?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn unknown_app(id: u64, app: &str) -> EngineMsg {
    EngineMsg::Result { id, ok: false, error: Some(format!("unknown app: {app}")) }
}

fn write_msg<W: Write>(output: &mut W, msg: &EngineMsg) -> std::io::Result<()> {
    output.write_all(msg.to_line().as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

/// Reads NDJSON lines, parsing each and routing it to the main channel. An
/// oversized or invalid line is logged to stderr and skipped, never a panic.
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
                Parsed::UnknownWithId(id) => {
                    let _ = tx.send(Ev::Emit(EngineMsg::Result {
                        id,
                        ok: false,
                        error: Some("unknown message type".into()),
                    }));
                }
                Parsed::Oversize => eprintln!("gentle-apps: skipped an oversized input line"),
                Parsed::Invalid => eprintln!("gentle-apps: skipped an invalid input line"),
                Parsed::Ignore => {}
            }
        }
        let _ = tx.send(Ev::InputClosed);
    });
}

/// One connector's thread: polls on its own interval, serves reply/refresh
/// commands, and reports status changes. Emits the connector's initial items
/// immediately so the panel fills on start.
fn connector_loop(mut c: Box<dyn Connector>, cmd_rx: Receiver<Cmd>, tx: Sender<Ev>) {
    let id = c.id().to_string();
    let interval = c.interval();
    let mut last_status = c.status();
    let mut last_items: Option<Vec<crate::protocol::Item>> = None;

    // Initial poll so the panel shows items without waiting a full interval.
    poll_once(&mut c, &id, &tx, &mut last_items);
    report_status(&mut c, &id, &tx, &mut last_status);

    loop {
        match cmd_rx.recv_timeout(interval) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Cmd::Reply { id: rid, item, text }) => {
                let result = c.reply(&item, &text);
                let _ = tx.send(Ev::Emit(result_msg(rid, result)));
                report_status(&mut c, &id, &tx, &mut last_status);
            }
            Ok(Cmd::Refresh { id: rid }) => {
                match c.poll() {
                    Ok(items) => {
                        last_items = Some(items.clone());
                        let _ = tx.send(Ev::Emit(EngineMsg::Items { app: id.clone(), items }));
                        let _ = tx.send(Ev::Emit(EngineMsg::Result { id: rid, ok: true, error: None }));
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::Emit(EngineMsg::Result { id: rid, ok: false, error: Some(e) }));
                    }
                }
                report_status(&mut c, &id, &tx, &mut last_status);
            }
            Err(RecvTimeoutError::Timeout) => {
                poll_once(&mut c, &id, &tx, &mut last_items);
                report_status(&mut c, &id, &tx, &mut last_status);
            }
        }
    }
}

fn poll_once(
    c: &mut Box<dyn Connector>,
    id: &str,
    tx: &Sender<Ev>,
    last_items: &mut Option<Vec<crate::protocol::Item>>,
) {
    match c.poll() {
        Ok(items) => {
            // Emit only when the list actually changed, so an idle app is quiet.
            if last_items.as_ref() != Some(&items) {
                *last_items = Some(items.clone());
                let _ = tx.send(Ev::Emit(EngineMsg::Items { app: id.to_string(), items }));
            }
        }
        Err(e) => eprintln!("gentle-apps: {id} poll failed: {e}"),
    }
}

fn report_status(
    c: &mut Box<dyn Connector>,
    id: &str,
    tx: &Sender<Ev>,
    last: &mut crate::protocol::Status,
) {
    let now = c.status();
    let detail = c.detail();
    if now != *last {
        *last = now;
    }
    let _ = tx.send(Ev::Status { id: id.to_string(), status: now, detail });
}

fn result_msg(id: u64, result: crate::connector::Result<()>) -> EngineMsg {
    match result {
        Ok(()) => EngineMsg::Result { id, ok: true, error: None },
        Err(e) => EngineMsg::Result { id, ok: false, error: Some(e) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Item, Status};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A controllable connector for in-process engine tests.
    struct Mock {
        id: String,
        items: Vec<Item>,
        replies: Arc<Mutex<Vec<(String, String)>>>,
        fail_reply: bool,
    }

    impl Connector for Mock {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            "Mock"
        }
        fn status(&self) -> Status {
            Status::Connected
        }
        fn interval(&self) -> std::time::Duration {
            Duration::from_secs(3600)
        }
        fn poll(&mut self) -> crate::connector::Result<Vec<Item>> {
            Ok(self.items.clone())
        }
        fn reply(&mut self, item_id: &str, text: &str) -> crate::connector::Result<()> {
            if self.fail_reply {
                return Err("reply rejected".into());
            }
            self.replies.lock().unwrap().push((item_id.to_string(), text.to_string()));
            Ok(())
        }
    }

    fn item(id: &str) -> Item {
        Item { id: id.into(), title: "t".into(), text: "x".into(), ts: "1".into(), link: "l".into() }
    }

    fn run_collect(connectors: Vec<Box<dyn Connector>>, input: &str) -> Vec<EngineMsg> {
        let mut out: Vec<u8> = Vec::new();
        run(connectors, Cursor::new(input.to_string()), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<EngineMsg>(l).unwrap())
            .collect()
    }

    #[test]
    fn mock_connector_items_are_emitted_on_start() {
        let mock = Mock { id: "mock".into(), items: vec![item("a")], replies: Arc::default(), fail_reply: false };
        let msgs = run_collect(vec![Box::new(mock)], "");
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Apps { apps } if apps[0].id == "mock")));
        assert!(msgs.iter().any(
            |m| matches!(m, EngineMsg::Items { app, items } if app == "mock" && items == &vec![item("a")])
        ));
    }

    #[test]
    fn reply_routes_to_the_connector_and_is_correlated_by_id() {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock { id: "mock".into(), items: vec![], replies: replies.clone(), fail_reply: false };
        let input = concat!(
            "{\"type\":\"reply\",\"id\":42,\"app\":\"mock\",\"item\":\"i1\",\"text\":\"hola\"}\n",
            "{\"type\":\"shutdown\"}\n"
        );
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 42, ok: true, .. })));
        assert_eq!(*replies.lock().unwrap(), vec![("i1".to_string(), "hola".to_string())]);
    }

    #[test]
    fn reply_to_an_unknown_app_yields_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"reply\",\"id\":5,\"app\":\"ghost\",\"item\":\"i\",\"text\":\"x\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 5, ok: false, error: Some(e) } if e.contains("ghost"))));
    }

    #[test]
    fn a_failing_reply_is_reported_as_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: true };
        let input = "{\"type\":\"reply\",\"id\":9,\"app\":\"mock\",\"item\":\"i\",\"text\":\"x\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 9, ok: false, error: Some(e) } if e == "reply rejected")));
    }

    #[test]
    fn an_unknown_message_type_with_an_id_gets_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let msgs = run_collect(vec![Box::new(mock)], "{\"type\":\"frobnicate\",\"id\":11}\n");
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 11, ok: false, .. })));
    }
}
