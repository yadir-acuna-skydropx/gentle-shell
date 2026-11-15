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
    Expand { id: u64, item: String },
    Action { id: u64, item: String, action: String },
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
            Ev::Client(ClientMsg::Expand { id, app, item }) => match senders.iter().find(|(a, _)| *a == app) {
                Some((_, s)) => {
                    let _ = s.send(Cmd::Expand { id, item });
                }
                None => write_msg(&mut output, &unknown_app(id, &app))?,
            },
            Ev::Client(ClientMsg::Action { id, app, item, action }) => match senders.iter().find(|(a, _)| *a == app) {
                Some((_, s)) => {
                    let _ = s.send(Cmd::Action { id, item, action });
                }
                None => write_msg(&mut output, &unknown_app_action(id, &app))?,
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

fn unknown_app_action(id: u64, app: &str) -> EngineMsg {
    EngineMsg::ActionResult { id, ok: false, message: None, error: Some(format!("unknown app: {app}")) }
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
            Ok(Cmd::Expand { id: rid, item }) => {
                match c.expand(&item) {
                    Ok(items) => {
                        let _ = tx.send(Ev::Emit(EngineMsg::Children {
                            id: rid,
                            app: id.clone(),
                            item,
                            items,
                        }));
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::Emit(EngineMsg::Result { id: rid, ok: false, error: Some(e) }));
                    }
                }
                report_status(&mut c, &id, &tx, &mut last_status);
            }
            Ok(Cmd::Action { id: rid, item, action }) => {
                // The `view` action returns reader content as a Detail message
                // (S27) rather than a plain action result.
                let msg = if action == "view" {
                    match c.view(&item) {
                        Ok((title, body)) => {
                            EngineMsg::Detail { id: rid, app: id.clone(), item, title, body }
                        }
                        Err(e) => EngineMsg::Result { id: rid, ok: false, error: Some(e) },
                    }
                } else {
                    match c.action(&item, &action) {
                        Ok(detail) => EngineMsg::ActionResult {
                            id: rid,
                            ok: true,
                            message: Some(detail),
                            error: None,
                        },
                        Err(e) => EngineMsg::ActionResult { id: rid, ok: false, message: None, error: Some(e) },
                    }
                };
                let _ = tx.send(Ev::Emit(msg));
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
        fn expand(&mut self, item_id: &str) -> crate::connector::Result<Vec<Item>> {
            if item_id == "bad" {
                return Err("cannot expand".into());
            }
            Ok(vec![item(&format!("{item_id}/child"))])
        }
        fn action(&mut self, item_id: &str, action_id: &str) -> crate::connector::Result<String> {
            if action_id == "boom" {
                return Err("action failed".into());
            }
            Ok(format!("did {action_id} on {item_id}"))
        }
        fn view(&mut self, item_id: &str) -> crate::connector::Result<(String, String)> {
            if item_id == "missing" {
                return Err("no detail".into());
            }
            Ok((format!("title {item_id}"), format!("body {item_id}")))
        }
    }

    fn item(id: &str) -> Item {
        Item { id: id.into(), title: "t".into(), text: "x".into(), ts: "1".into(), link: "l".into(), ..Default::default() }
    }
    // Same as `item`, under a distinct name so a `Children { item, .. }` match arm
    // (which binds a local `item`) can still build the expected child.
    fn item_named(id: &str) -> Item {
        item(id)
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
    fn expand_routes_to_the_connector_and_returns_children() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = concat!(
            "{\"type\":\"expand\",\"id\":21,\"app\":\"mock\",\"item\":\"sec\"}\n",
            "{\"type\":\"shutdown\"}\n"
        );
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(
            m,
            EngineMsg::Children { id: 21, app, item, items }
                if app == "mock" && item == "sec" && items == &vec![item_named("sec/child")]
        )));
    }

    #[test]
    fn a_failing_expand_is_reported_as_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"expand\",\"id\":22,\"app\":\"mock\",\"item\":\"bad\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 22, ok: false, error: Some(e) } if e == "cannot expand")));
    }

    #[test]
    fn action_routes_and_returns_an_action_result_with_a_message() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"action\",\"id\":23,\"app\":\"mock\",\"item\":\"i\",\"action\":\"open\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(
            m,
            EngineMsg::ActionResult { id: 23, ok: true, message: Some(msg), .. } if msg == "did open on i"
        )));
    }

    #[test]
    fn a_failing_action_is_reported_as_an_error_action_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"action\",\"id\":24,\"app\":\"mock\",\"item\":\"i\",\"action\":\"boom\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::ActionResult { id: 24, ok: false, error: Some(e), .. } if e == "action failed")));
    }

    #[test]
    fn a_view_action_returns_a_detail_message_with_title_and_body() {
        // S27: the `view` action yields a Detail (not an ActionResult).
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = concat!(
            "{\"type\":\"action\",\"id\":30,\"app\":\"mock\",\"item\":\"issue:o/r#1\",\"action\":\"view\"}\n",
            "{\"type\":\"shutdown\"}\n"
        );
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(
            m,
            EngineMsg::Detail { id: 30, app, item, title, body }
                if app == "mock" && item == "issue:o/r#1"
                    && title == "title issue:o/r#1" && body == "body issue:o/r#1"
        )));
    }

    #[test]
    fn a_failing_view_action_is_reported_as_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"action\",\"id\":31,\"app\":\"mock\",\"item\":\"missing\",\"action\":\"view\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 31, ok: false, error: Some(e) } if e == "no detail")));
    }

    #[test]
    fn an_action_on_an_unknown_app_yields_an_action_result_error() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let input = "{\"type\":\"action\",\"id\":25,\"app\":\"ghost\",\"item\":\"i\",\"action\":\"open\"}\n";
        let msgs = run_collect(vec![Box::new(mock)], input);
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::ActionResult { id: 25, ok: false, error: Some(e), .. } if e.contains("ghost"))));
    }

    #[test]
    fn an_unknown_message_type_with_an_id_gets_an_error_result() {
        let mock = Mock { id: "mock".into(), items: vec![], replies: Arc::default(), fail_reply: false };
        let msgs = run_collect(vec![Box::new(mock)], "{\"type\":\"frobnicate\",\"id\":11}\n");
        assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Result { id: 11, ok: false, .. })));
    }
}
