//! A minimal fake Anthropic API on 127.0.0.1 for integration tests. It records
//! every request and answers with a caller-supplied handler. One response per
//! connection (Connection: close), which is enough for ureq's blocking client.
#![allow(dead_code)]
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// One captured HTTP request.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// Header names are lowercased.
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// A response the handler returns.
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn json(status: u16, body: impl Into<String>) -> Resp {
        Resp { status, headers: vec![("content-type".into(), "application/json".into())], body: body.into() }
    }
    pub fn sse(body: impl Into<String>) -> Resp {
        Resp { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], body: body.into() }
    }
    pub fn header(mut self, k: &str, v: &str) -> Resp {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// A running fake server. Dropping it is fine; the accept thread is detached.
pub struct Fake {
    pub base: String,
    requests: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    /// Starts a server whose handler sees each request and the 1-based request
    /// count (to script pagination / retries).
    pub fn start<H>(handler: H) -> Fake
    where
        H: Fn(&Req, usize) -> Resp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        {
            let requests = requests.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let requests = requests.clone();
                    let handler = handler.clone();
                    thread::spawn(move || handle(stream, requests, handler));
                }
            });
        }
        Fake { base: format!("http://127.0.0.1:{port}"), requests }
    }

    /// All requests seen so far.
    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

fn handle(mut stream: TcpStream, requests: Arc<Mutex<Vec<Req>>>, handler: Arc<dyn Fn(&Req, usize) -> Resp + Send + Sync>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let len: usize = headers.get("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let req = Req { method, path, headers, body };
    let count = {
        let mut r = requests.lock().unwrap();
        r.push(req.clone());
        r.len()
    };
    let resp = handler(&req, count);

    let reason = match resp.status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        529 => "Overloaded",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {} {reason}\r\n", resp.status);
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(&resp.body);
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

/// A standard SSE body: two text deltas and a stop.
pub fn sse_hello() -> String {
    concat!(
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hola\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" mundo\"}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}
