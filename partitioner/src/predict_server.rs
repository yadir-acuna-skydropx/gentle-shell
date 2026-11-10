//! Local HTTP server for the prediction engine (S9). std-only: a `TcpListener`
//! bound to 127.0.0.1 on a random port, handed each connection to a short-lived
//! worker thread with read/write timeouts so one stuck client cannot wedge it.
//! The engine is shared behind `Arc<RwLock<..>>`.
//!
//! Protocol:
//! - `POST /predict {"text":"...","mode":"chat"|"code"}` → 200 `{"suggestion":"..."}`
//! - `POST /learn   {"text":"...","mode"?}`             → 204
//! - missing/!`Bearer <token>` → 401, unknown path → 404, wrong method → 405,
//!   body > 64 KiB or bad JSON → 400.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::predict::{json_escape, parse_json_object, Engine, PredictMode};

/// Largest accepted request body.
const MAX_BODY: usize = 64 * 1024;
/// Per-connection read/write timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// A running predict server. Dropping this does not stop the listener thread
/// (the process keeps serving for its lifetime); it carries the bound address
/// and the bearer token the child must present.
pub struct PredictServer {
    pub addr: SocketAddr,
    pub token: String,
}

impl PredictServer {
    /// Binds 127.0.0.1:0, spawns the accept thread and returns the address and
    /// token. Never blocks the caller.
    pub fn start(engine: Arc<RwLock<Engine>>) -> std::io::Result<PredictServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let token = random_token();
        let thread_token = token.clone();
        std::thread::Builder::new().name("predict-server".into()).spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let engine = engine.clone();
                let token = thread_token.clone();
                // Each connection on its own worker so a slow client never
                // blocks the accept loop.
                let _ = std::thread::Builder::new().name("predict-conn".into()).spawn(move || {
                    let _ = handle_conn(stream, &engine, &token);
                });
            }
        })?;
        Ok(PredictServer { addr, token })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

/// 32 hex chars from `/dev/urandom` on unix, falling back to a non-crypto
/// time/pid hash only if that read fails.
fn random_token() -> String {
    #[cfg(unix)]
    {
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut b = [0u8; 16];
            if f.read_exact(&mut b).is_ok() {
                return b.iter().map(|x| format!("{x:02x}")).collect();
            }
        }
    }
    // Fallback: not cryptographic, but keeps the server usable.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    let a = h.finish();
    std::thread::current().id().hash(&mut h);
    let b = h.finish();
    format!("{a:016x}{b:016x}")
}

struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

enum ReadErr {
    TooLarge,
    Bad,
    Io,
}

fn handle_conn(mut stream: TcpStream, engine: &Arc<RwLock<Engine>>, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (status, body) = match read_request(&stream) {
        Ok(req) => route(&req, engine, token),
        Err(ReadErr::TooLarge) => (400, err("request too large")),
        // A torn or timed-out request is dropped without a reply.
        Err(ReadErr::Bad) => (400, err("bad request")),
        Err(ReadErr::Io) => return Ok(()),
    };
    write_response(&mut stream, status, &body)
}

fn read_request(stream: &TcpStream) -> Result<Request, ReadErr> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|_| ReadErr::Io)?);
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|_| ReadErr::Io)? == 0 {
        return Err(ReadErr::Bad);
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().ok_or(ReadErr::Bad)?.to_string();
    let path = parts.next().ok_or(ReadErr::Bad)?.to_string();

    let mut content_length = 0usize;
    let mut auth = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).map_err(|_| ReadErr::Io)? == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().map_err(|_| ReadErr::Bad)?;
            } else if k == "authorization" {
                auth = Some(v.to_string());
            }
        }
    }
    if content_length > MAX_BODY {
        return Err(ReadErr::TooLarge);
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).map_err(|_| ReadErr::Io)?;
    let body = String::from_utf8(buf).map_err(|_| ReadErr::Bad)?;
    Ok(Request { method, path, auth, body })
}

fn route(req: &Request, engine: &Arc<RwLock<Engine>>, token: &str) -> (u16, String) {
    // Auth first: no valid bearer token, no service.
    if req.auth.as_deref() != Some(format!("Bearer {token}").as_str()) {
        return (401, err("unauthorized"));
    }
    match req.path.as_str() {
        "/predict" => {
            if req.method != "POST" {
                return (405, err("method not allowed"));
            }
            let Some(fields) = parse_json_object(&req.body) else {
                return (400, err("bad json"));
            };
            let text = field(&fields, "text").unwrap_or_default();
            let mode = parse_mode(field(&fields, "mode").as_deref());
            let suggestion = engine.read().ok().and_then(|e| e.predict(&text, mode)).unwrap_or_default();
            (200, format!("{{\"suggestion\":\"{}\"}}", json_escape(&suggestion)))
        }
        "/learn" => {
            if req.method != "POST" {
                return (405, err("method not allowed"));
            }
            let Some(fields) = parse_json_object(&req.body) else {
                return (400, err("bad json"));
            };
            if let Some(text) = field(&fields, "text") {
                let mode = parse_mode(field(&fields, "mode").as_deref());
                if let Ok(mut e) = engine.write() {
                    e.learn(&text, mode);
                }
            }
            (204, String::new())
        }
        _ => (404, err("not found")),
    }
}

fn parse_mode(mode: Option<&str>) -> PredictMode {
    match mode {
        Some("code") => PredictMode::Code,
        _ => PredictMode::Chat,
    }
}

fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn err(msg: &str) -> String {
    format!("{{\"error\":\"{}\"}}", json_escape(msg))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let mut resp = format!("HTTP/1.1 {status} {reason}\r\n");
    if status == 204 {
        resp.push_str("Content-Length: 0\r\n\r\n");
    } else {
        resp.push_str("Content-Type: application/json\r\n");
        resp.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        resp.push_str(body);
    }
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}
