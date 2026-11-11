//! End-to-end Slack flows against an in-process fake HTTP server, a fake browser
//! opener and an in-memory token store. No real network, keyring or browser.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentle_apps::connector::Connector;
use gentle_apps::oauth::{self, BrowserOpener, SlackConfig};
use gentle_apps::protocol::Status;
use gentle_apps::slack::SlackConnector;
use gentle_apps::store::{MemoryStore, StoredToken, TokenStore, SLACK_ACCOUNT};

// ---------- fake HTTP server ----------

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    query: String,
    body: String,
    auth: Option<String>,
}

struct FakeResp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl FakeResp {
    fn json(body: &str) -> Self {
        FakeResp { status: 200, headers: vec![], body: body.to_string() }
    }
}

struct FakeServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeServer {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(&Recorded) -> FakeResp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let reqs = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let h = handler.clone();
                let r = reqs.clone();
                std::thread::spawn(move || serve(stream, h, r));
            }
        });
        FakeServer { addr, requests }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(
    mut stream: TcpStream,
    handler: Arc<dyn Fn(&Recorded) -> FakeResp + Send + Sync>,
    requests: Arc<Mutex<Vec<Recorded>>>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until end of headers.
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let header_text = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = header_text
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:").or_else(|| l.strip_prefix("content-length:")))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = pos + 4;
            while buf.len() < body_start + content_length {
                let n = match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = String::from_utf8_lossy(&buf[body_start..(body_start + content_length).min(buf.len())]).to_string();
            let mut lines = header_text.lines();
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let target = parts.next().unwrap_or("");
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p.to_string(), q.to_string()),
                None => (target.to_string(), String::new()),
            };
            let auth = header_text
                .lines()
                .find_map(|l| l.strip_prefix("Authorization:").or_else(|| l.strip_prefix("authorization:")))
                .map(|v| v.trim().to_string());
            let rec = Recorded { method, path, query, body, auth };
            requests.lock().unwrap().push(rec.clone());
            let resp = handler(&rec);
            let mut head = format!("HTTP/1.1 {} X\r\n", resp.status);
            let mut has_ct = false;
            for (k, v) in &resp.headers {
                if k.eq_ignore_ascii_case("content-type") {
                    has_ct = true;
                }
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            if !has_ct {
                head.push_str("Content-Type: application/json\r\n");
            }
            head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", resp.body.len()));
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(resp.body.as_bytes());
            let _ = stream.flush();
            return;
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parses a url-encoded form body into a decoded key→value map.
fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|s| !s.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = (b[i + 1] as char).to_digit(16);
                let l = (b[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn cfg(base: &str) -> SlackConfig {
    SlackConfig { api_base: base.to_string(), authorize_base: "https://auth.example/x".to_string() }
}

fn seeded_store(token: StoredToken) -> Box<dyn TokenStore> {
    let store = MemoryStore::new();
    store.save(SLACK_ACCOUNT, &token).unwrap();
    Box::new(store)
}

fn plain(access: &str) -> StoredToken {
    StoredToken { access_token: access.to_string(), refresh_token: None, expires_at: None }
}

// ---------- fake browser ----------

/// On open, extracts `state` from the URL and fires the loopback callback.
struct FakeBrowser {
    port: u16,
    code: String,
}

impl BrowserOpener for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        let state = url
            .split_once("state=")
            .and_then(|(_, rest)| rest.split('&').next())
            .unwrap_or("")
            .to_string();
        let port = self.port;
        let code = self.code.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = s.write_all(
                    format!("GET /callback?code={code}&state={state} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
                );
                let mut sink = String::new();
                let _ = s.read_to_string(&mut sink);
            }
        });
        Ok(())
    }
}

// ---------- tests ----------

#[test]
fn token_exchange_posts_exact_form_without_client_secret() {
    let server = FakeServer::start(|_req| {
        FakeResp::json(
            r#"{"ok":true,"team":{"id":"T1","name":"Acme"},"authed_user":{"id":"U1","access_token":"xoxp-secret-token","scope":"search:read"}}"#,
        )
    });
    let result = oauth::exchange_code(&cfg(&server.base()), "123.456", "THECODE", "VERIFIER").unwrap();
    assert_eq!(result.token.access_token, "xoxp-secret-token");
    assert_eq!(result.user_id, "U1");
    assert_eq!(result.team_name, "Acme");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/oauth.v2.access");
    let form = parse_form(&reqs[0].body);
    assert_eq!(form.get("code").map(String::as_str), Some("THECODE"));
    assert_eq!(form.get("client_id").map(String::as_str), Some("123.456"));
    assert_eq!(form.get("code_verifier").map(String::as_str), Some("VERIFIER"));
    assert_eq!(form.get("redirect_uri").map(String::as_str), Some("http://localhost:47917/callback"));
    assert!(!form.contains_key("client_secret"), "must never send a client_secret");
}

#[test]
fn run_login_stores_token_and_metadata_without_leaking_the_token() {
    let server = FakeServer::start(|_req| {
        FakeResp::json(
            r#"{"ok":true,"team":{"id":"T9","name":"Globex"},"authed_user":{"id":"U9","access_token":"xoxp-login"}}"#,
        )
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let opener = FakeBrowser { port, code: "AUTHCODE".into() };
    let store = MemoryStore::new();
    let home = std::env::temp_dir().join(format!("gentle-apps-login-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    let printed = Arc::new(Mutex::new(String::new()));
    let p2 = printed.clone();
    let result = oauth::run_login(
        &cfg(&server.base()),
        "cid-1",
        &listener,
        &opener,
        &store,
        &home,
        Instant::now() + Duration::from_secs(5),
        &|url| p2.lock().unwrap().push_str(url),
    )
    .unwrap();

    assert_eq!(result.user_id, "U9");
    assert_eq!(store.load(SLACK_ACCOUNT).unwrap().unwrap().access_token, "xoxp-login");
    let meta = gentle_apps::config::load(&home);
    assert_eq!(meta.team_name.as_deref(), Some("Globex"));
    assert_eq!(meta.user_id.as_deref(), Some("U9"));
    // The printed URL (for manual paste) must not contain the token.
    assert!(!printed.lock().unwrap().contains("xoxp-login"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn status_connected_after_auth_test() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/auth.test" => FakeResp::json(r#"{"ok":true,"user_id":"U1","team":"Acme"}"#),
        "/search.messages" => FakeResp::json(r#"{"ok":true,"messages":{"matches":[]}}"#),
        _ => FakeResp::json(r#"{"ok":false,"error":"unexpected"}"#),
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    let items = c.poll().unwrap();
    assert!(items.is_empty());
    assert_eq!(c.status(), Status::Connected);
}

#[test]
fn status_error_on_invalid_auth() {
    let server = FakeServer::start(|_req| FakeResp::json(r#"{"ok":false,"error":"invalid_auth"}"#));
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    let err = c.poll().unwrap_err();
    assert_eq!(err, "invalid_auth");
    assert_eq!(c.status(), Status::Error);
    assert!(c.detail().unwrap().contains("token revoked"));
}

#[test]
fn poll_renders_items_and_dedupes() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/auth.test" => FakeResp::json(r#"{"ok":true,"user_id":"U1"}"#),
        "/search.messages" => FakeResp::json(
            r#"{"ok":true,"messages":{"matches":[
                {"ts":"2.0","username":"ana","text":"hey <@U1> see <#C9|general>","permalink":"https://x?cid=C1","channel":{"id":"C1","name":"general","is_im":false,"is_private":false}},
                {"ts":"2.0","username":"ana","text":"dup","permalink":"https://x?cid=C1","channel":{"id":"C1","name":"general"}},
                {"ts":"1.0","username":"bob","text":"older <@U1>","permalink":"https://x?cid=C2","channel":{"id":"C2","name":"random"}}
            ]}}"#,
        ),
        _ => FakeResp::json(r#"{"ok":false,"error":"unexpected"}"#),
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    let items = c.poll().unwrap();
    // Two unique channel:ts (the duplicate C1:2.0 is collapsed), newest first.
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].id, "C1:2.0");
    assert_eq!(items[0].title, "#general · ana");
    assert_eq!(items[0].text, "hey @U1 see #general");
    assert_eq!(items[1].id, "C2:1.0");
}

#[test]
fn poll_honors_429_retry_after() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let server = FakeServer::start(move |req| match req.path.as_str() {
        "/auth.test" => FakeResp::json(r#"{"ok":true,"user_id":"U1"}"#),
        "/search.messages" => {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                FakeResp { status: 429, headers: vec![("Retry-After".into(), "1".into())], body: r#"{"ok":false,"error":"ratelimited"}"#.into() }
            } else {
                FakeResp::json(r#"{"ok":true,"messages":{"matches":[{"ts":"3.0","username":"ana","text":"hi <@U1>","permalink":"https://x?cid=C1","channel":{"id":"C1","name":"general"}}]}}"#)
            }
        }
        _ => FakeResp::json(r#"{"ok":false,"error":"unexpected"}"#),
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    let start = Instant::now();
    let items = c.poll().unwrap();
    assert!(start.elapsed() >= Duration::from_millis(900), "should have waited ~1s for Retry-After");
    assert_eq!(items.len(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 2, "retried once after the 429");
}

fn search_one(channel_json: &str, permalink: &str) -> String {
    format!(
        r#"{{"ok":true,"messages":{{"matches":[{{"ts":"100.5","username":"ana","text":"hi <@U1>","permalink":"{permalink}","channel":{channel_json}}}]}}}}"#
    )
}

fn reply_server<F>(extra: F) -> FakeServer
where
    F: Fn(&Recorded) -> Option<FakeResp> + Send + Sync + 'static,
{
    FakeServer::start(move |req| {
        if let Some(r) = extra(req) {
            return r;
        }
        match req.path.as_str() {
            "/auth.test" => FakeResp::json(r#"{"ok":true,"user_id":"U1"}"#),
            "/chat.postMessage" => FakeResp::json(r#"{"ok":true,"ts":"200.0"}"#),
            _ => FakeResp::json(r#"{"ok":false,"error":"unexpected"}"#),
        }
    })
}

#[test]
fn reply_uses_thread_ts_from_the_permalink() {
    let server = reply_server(|req| match req.path.as_str() {
        "/search.messages" => Some(FakeResp::json(&search_one(
            r#"{"id":"C1","name":"general"}"#,
            "https://x.slack.com/archives/C1/p100?thread_ts=99.5&cid=C1",
        ))),
        _ => None,
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    c.poll().unwrap();
    c.reply("C1:100.5", "hola").unwrap();

    let post = server.requests().into_iter().find(|r| r.path == "/chat.postMessage").unwrap();
    let form = parse_form(&post.body);
    assert_eq!(form.get("channel").map(String::as_str), Some("C1"));
    assert_eq!(form.get("thread_ts").map(String::as_str), Some("99.5"));
    assert_eq!(form.get("text").map(String::as_str), Some("hola"));
    assert_eq!(post.auth.as_deref(), Some("Bearer xoxp-x"));
    // Exactly the three documented params.
    assert_eq!(form.len(), 3, "{form:?}");
}

#[test]
fn reply_falls_back_to_conversations_replies() {
    let server = reply_server(|req| match req.path.as_str() {
        "/search.messages" => Some(FakeResp::json(&search_one(
            r#"{"id":"C1","name":"general"}"#,
            "https://x.slack.com/archives/C1/p100?cid=C1",
        ))),
        "/conversations.replies" => {
            assert!(req.query.contains("channel=C1"), "{}", req.query);
            Some(FakeResp::json(r#"{"ok":true,"messages":[{"ts":"100.5","thread_ts":"77.1"}]}"#))
        }
        _ => None,
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    c.poll().unwrap();
    c.reply("C1:100.5", "hola").unwrap();
    let post = server.requests().into_iter().find(|r| r.path == "/chat.postMessage").unwrap();
    assert_eq!(parse_form(&post.body).get("thread_ts").map(String::as_str), Some("77.1"));
}

#[test]
fn reply_falls_back_to_the_messages_own_ts() {
    let server = reply_server(|req| match req.path.as_str() {
        "/search.messages" => Some(FakeResp::json(&search_one(
            r#"{"id":"C1","name":"general"}"#,
            "https://x.slack.com/archives/C1/p100?cid=C1",
        ))),
        "/conversations.replies" => Some(FakeResp::json(r#"{"ok":true,"messages":[{"ts":"100.5"}]}"#)),
        _ => None,
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    c.poll().unwrap();
    c.reply("C1:100.5", "hola").unwrap();
    let post = server.requests().into_iter().find(|r| r.path == "/chat.postMessage").unwrap();
    assert_eq!(parse_form(&post.body).get("thread_ts").map(String::as_str), Some("100.5"));
}

#[test]
fn reply_propagates_a_slack_error() {
    let server = reply_server(|req| match req.path.as_str() {
        "/search.messages" => Some(FakeResp::json(&search_one(
            r#"{"id":"C1","name":"general"}"#,
            "https://x.slack.com/archives/C1/p100?thread_ts=99.5&cid=C1",
        ))),
        "/chat.postMessage" => Some(FakeResp::json(r#"{"ok":false,"error":"channel_not_found"}"#)),
        _ => None,
    });
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), seeded_store(plain("xoxp-x")), "/tmp".into());
    c.poll().unwrap();
    assert_eq!(c.reply("C1:100.5", "hola").unwrap_err(), "channel_not_found");
}

#[test]
fn token_is_refreshed_when_near_expiry() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/oauth.v2.access" => {
            let form = parse_form(&req.body);
            assert_eq!(form.get("grant_type").map(String::as_str), Some("refresh_token"));
            assert_eq!(form.get("refresh_token").map(String::as_str), Some("xoxe-old"));
            assert!(!form.contains_key("client_secret"));
            FakeResp::json(r#"{"ok":true,"access_token":"xoxp-NEW","refresh_token":"xoxe-new","expires_in":43200}"#)
        }
        "/auth.test" => {
            // The refreshed token must be the one used downstream.
            assert_eq!(req.auth.as_deref(), Some("Bearer xoxp-NEW"));
            FakeResp::json(r#"{"ok":true,"user_id":"U1"}"#)
        }
        "/search.messages" => FakeResp::json(r#"{"ok":true,"messages":{"matches":[]}}"#),
        _ => FakeResp::json(r#"{"ok":false,"error":"unexpected"}"#),
    });
    let store = MemoryStore::new();
    let near = oauth::now_unix() + 10; // within the 300s refresh skew
    store
        .save(
            SLACK_ACCOUNT,
            &StoredToken { access_token: "xoxp-old".into(), refresh_token: Some("xoxe-old".into()), expires_at: Some(near) },
        )
        .unwrap();
    let mut c = SlackConnector::new(cfg(&server.base()), Some("cid".into()), Box::new(store.clone()), "/tmp".into());
    c.poll().unwrap();
    assert_eq!(store.load(SLACK_ACCOUNT).unwrap().unwrap().access_token, "xoxp-NEW");
}
