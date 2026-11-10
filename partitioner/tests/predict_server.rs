//! Black-box checks of the local predict server over a real TcpStream (S9).
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use gentle_partitioner::predict::Engine;
use gentle_partitioner::predict_server::PredictServer;

struct Response {
    status: u16,
    body: String,
}

/// Sends one raw HTTP request to the server and returns the status + body.
fn request(server: &PredictServer, method: &str, path: &str, token: Option<&str>, body: &str) -> Response {
    send_raw(server, &build_request(method, path, token, body, body.len()))
}

fn build_request(method: &str, path: &str, token: Option<&str>, body: &str, content_length: usize) -> String {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str(&format!("Content-Length: {content_length}\r\n\r\n"));
    req.push_str(body);
    req
}

fn send_raw(server: &PredictServer, raw: &str) -> Response {
    let mut stream = TcpStream::connect(server.addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    parse_response(&buf)
}

fn parse_response(raw: &str) -> Response {
    let status = raw
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Response { status, body }
}

fn start() -> (PredictServer, Arc<RwLock<Engine>>) {
    let engine = Arc::new(RwLock::new(Engine::new()));
    let server = PredictServer::start(engine.clone()).expect("start server");
    (server, engine)
}

#[test]
fn rejects_requests_without_the_bearer_token() {
    let (server, _engine) = start();
    let r = request(&server, "POST", "/predict", None, "{\"text\":\"hola\"}");
    assert_eq!(r.status, 401);
}

#[test]
fn binds_only_to_loopback() {
    let (server, _engine) = start();
    assert!(server.addr.ip().is_loopback());
    assert_eq!(server.addr.ip().to_string(), "127.0.0.1");
}

#[test]
fn learn_then_predict_reflects_the_new_counts() {
    let (server, _engine) = start();
    // 204 on learn.
    let learn = request(&server, "POST", "/learn", Some(&server.token), "{\"text\":\"configuración configuración\",\"mode\":\"chat\"}");
    assert_eq!(learn.status, 204);
    assert!(learn.body.is_empty());
    // 200 on predict, now reflecting what was learned.
    let predict = request(&server, "POST", "/predict", Some(&server.token), "{\"text\":\"config\",\"mode\":\"chat\"}");
    assert_eq!(predict.status, 200);
    assert_eq!(predict.body, "{\"suggestion\":\"uración\"}");
}

#[test]
fn json_escaping_round_trips_through_learn_and_predict() {
    let (server, _engine) = start();
    // Body exercises escaped quotes, a newline and Spanish accents.
    let body = "{\"text\":\"configuración \\\"especial\\\"\\nconfiguración\",\"mode\":\"code\"}";
    assert_eq!(request(&server, "POST", "/learn", Some(&server.token), body).status, 204);
    let predict = request(&server, "POST", "/predict", Some(&server.token), "{\"text\":\"config\",\"mode\":\"code\"}");
    assert_eq!(predict.status, 200);
    assert_eq!(predict.body, "{\"suggestion\":\"uración\"}");
}

#[test]
fn empty_suggestion_when_there_is_nothing_to_offer() {
    let (server, _engine) = start();
    let r = request(&server, "POST", "/predict", Some(&server.token), "{\"text\":\"\",\"mode\":\"chat\"}");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "{\"suggestion\":\"\"}");
}

#[test]
fn unknown_path_is_404() {
    let (server, _engine) = start();
    let r = request(&server, "POST", "/nope", Some(&server.token), "{}");
    assert_eq!(r.status, 404);
}

#[test]
fn wrong_method_is_405() {
    let (server, _engine) = start();
    let r = request(&server, "GET", "/predict", Some(&server.token), "");
    assert_eq!(r.status, 405);
}

#[test]
fn bad_json_is_400() {
    let (server, _engine) = start();
    let r = request(&server, "POST", "/predict", Some(&server.token), "not json");
    assert_eq!(r.status, 400);
}

#[test]
fn oversized_body_is_400() {
    let (server, _engine) = start();
    // Declare a Content-Length over 64 KiB without sending the full body; the
    // server must reject it on the header alone, not hang reading.
    let raw = build_request("POST", "/learn", Some(&server.token), "{}", 70_000);
    let r = send_raw(&server, &raw);
    assert_eq!(r.status, 400);
}
