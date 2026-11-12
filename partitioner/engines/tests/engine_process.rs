//! The explainer engine end-to-end against the fake API: explain streams deltas
//! and finishes, a follow-up `ask` carries the prior turns, and the context
//! caps are applied to the request body.
mod common;

use std::io::Cursor;

use common::{Fake, Resp};
use gentle_engines::backend::AnthropicBackend;
use gentle_engines::engine;
use gentle_engines::keystore::{KeyStore, MemoryStore};
use gentle_engines::protocol::EngineMsg;

fn models_or_sse(req: &common::Req) -> Resp {
    if req.path.starts_with("/v1/messages") {
        Resp::sse(common::sse_hello())
    } else {
        Resp::json(200, r#"{"data":[{"id":"claude-haiku-4-5","created_at":"2025-10-01T00:00:00Z","line":"haiku"}],"has_more":false}"#)
    }
}

fn run(fake: &Fake, input: &str) -> Vec<EngineMsg> {
    let store = MemoryStore::new();
    store.save("sk-ant-ok-12345678").unwrap();
    let home = std::env::temp_dir().join(format!("gentle-engines-it-{}", std::process::id()));
    let backend = AnthropicBackend::new_with_base(&store, &home, &fake.base);
    let mut out: Vec<u8> = Vec::new();
    engine::run(backend, Cursor::new(input.to_string()), &mut out).unwrap();
    String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str::<EngineMsg>(l).unwrap()).collect()
}

#[test]
fn explain_streams_deltas_and_reports_status_and_done() {
    let fake = Fake::start(|req, _| models_or_sse(req));
    let input = concat!(
        "{\"type\":\"explain\",\"id\":1,\"path\":\"src/a.rs\",\"language\":\"rust\",\"selection\":\"fn x() {}\"}\n",
        "{\"type\":\"shutdown\"}\n",
    );
    let msgs = run(&fake, input);
    assert!(matches!(msgs[0], EngineMsg::Status { .. }));
    let text: String = msgs
        .iter()
        .filter_map(|m| if let EngineMsg::Delta { id: 1, text } = m { Some(text.clone()) } else { None })
        .collect();
    assert_eq!(text, "Hola mundo");
    assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 1 })));
}

#[test]
fn ask_includes_the_prior_conversation_turns() {
    let fake = Fake::start(|req, _| models_or_sse(req));
    let input = concat!(
        "{\"type\":\"explain\",\"id\":1,\"path\":\"src/a.rs\",\"language\":\"rust\",\"selection\":\"fn total() {}\"}\n",
        "{\"type\":\"ask\",\"id\":2,\"question\":\"¿y si falla?\"}\n",
        "{\"type\":\"shutdown\"}\n",
    );
    let msgs = run(&fake, input);
    assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 2 })));

    let messages: Vec<_> = fake.requests().into_iter().filter(|r| r.path.starts_with("/v1/messages")).collect();
    assert_eq!(messages.len(), 2, "one request per turn");
    let ask_body = &messages[1].body;
    // The follow-up request carries the original selection, the assistant's
    // prior answer, and the new question — i.e. the whole conversation.
    assert!(ask_body.contains("fn total()"), "prior user turn present");
    assert!(ask_body.contains("Hola mundo"), "prior assistant turn present");
    assert!(ask_body.contains("y si falla"), "new question present");
}

#[test]
fn oversized_selection_is_capped_in_the_request() {
    let fake = Fake::start(|req, _| models_or_sse(req));
    let big = (0..1000).map(|i| format!("line number {i}")).collect::<Vec<_>>().join("\\n");
    let input = format!(
        "{{\"type\":\"explain\",\"id\":1,\"path\":\"big.rs\",\"language\":\"rust\",\"selection\":\"{big}\"}}\n{{\"type\":\"shutdown\"}}\n"
    );
    let msgs = run(&fake, &input);
    assert!(msgs.iter().any(|m| matches!(m, EngineMsg::Done { id: 1 })));
    let body = fake.requests().into_iter().find(|r| r.path.starts_with("/v1/messages")).unwrap().body;
    // The prompt says the selection was truncated, and the far tail line is gone.
    assert!(body.contains("truncated"));
    assert!(!body.contains("line number 999"));
}
