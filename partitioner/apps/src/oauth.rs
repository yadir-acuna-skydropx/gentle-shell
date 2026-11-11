//! Slack OAuth v2 user-token login with PKCE, through the browser (L25/L27).
//!
//! No client secret: `oauth.v2.access` is called with `code` + `code_verifier`.
//! A loopback server on the fixed port [`REDIRECT_PORT`] receives the redirect,
//! validates `state` in constant time, and the token exchange result is stored
//! in the keyring. The browser opener and the token store are injected so tests
//! run with a fake HTTP server, a fake opener and an in-memory store.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{self, SlackMeta};
use crate::http::{self};
use crate::pkce;
use crate::store::{SLACK_ACCOUNT, StoredToken, TokenStore};

/// Fixed loopback port; must equal the registered redirect URL exactly.
pub const REDIRECT_PORT: u16 = 47917;
/// The redirect URL registered in the Slack app manifest.
pub const REDIRECT_URI: &str = "http://localhost:47917/callback";
/// Percent-encoded form of [`REDIRECT_URI`] for the authorize URL.
pub const REDIRECT_URI_ENC: &str = "http%3A%2F%2Flocalhost%3A47917%2Fcallback";
/// User scopes requested at authorize time (comma-separated, Slack form).
pub const USER_SCOPES: &str = "search:read,chat:write,users:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history";

/// Default login deadline: accept callbacks for up to 5 minutes.
pub const LOGIN_DEADLINE: Duration = Duration::from_secs(300);

/// Base URLs, overridable for tests only via env.
#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Web API base, default `https://slack.com/api`.
    pub api_base: String,
    /// Authorize endpoint, default `https://slack.com/oauth/v2/authorize`.
    pub authorize_base: String,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            api_base: "https://slack.com/api".to_string(),
            authorize_base: "https://slack.com/oauth/v2/authorize".to_string(),
        }
    }
}

impl SlackConfig {
    /// Reads the test-only overrides `GENTLE_APPS_SLACK_API` and
    /// `GENTLE_APPS_SLACK_AUTHORIZE`, else the production defaults.
    pub fn from_env() -> Self {
        let d = SlackConfig::default();
        SlackConfig {
            api_base: std::env::var("GENTLE_APPS_SLACK_API").unwrap_or(d.api_base),
            authorize_base: std::env::var("GENTLE_APPS_SLACK_AUTHORIZE").unwrap_or(d.authorize_base),
        }
    }
}

/// Opens the system browser at a URL.
pub trait BrowserOpener {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// Real opener: `xdg-open <url>` (no crate).
pub struct XdgOpenBrowser;

impl BrowserOpener for XdgOpenBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("xdg-open failed: {e}"))
    }
}

/// Builds the authorize URL with exactly the spec's params and encodings.
pub fn authorize_url(authorize_base: &str, client_id: &str, state: &str, code_challenge: &str) -> String {
    format!(
        "{authorize_base}?client_id={}&user_scope={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        percent_encode(client_id),
        USER_SCOPES,
        REDIRECT_URI_ENC,
        percent_encode(state),
        percent_encode(code_challenge),
    )
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` and `+` in a query component.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
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

/// Compares two byte strings without an early exit on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Binds the fixed loopback redirect port for the login callback.
pub fn bind_redirect() -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|e| {
        format!("cannot bind 127.0.0.1:{REDIRECT_PORT} for the login callback: {e}")
    })
}

/// Outcome of waiting for the OAuth redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackError {
    /// The IdP returned `error=<code>` (e.g. `access_denied`).
    Denied(String),
    /// `state` did not match: a 400 was sent and the login aborted.
    StateMismatch,
    /// No valid callback arrived before the deadline.
    TimedOut,
    /// A low-level I/O failure on the listener.
    Io(String),
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackError::Denied(e) => write!(f, "Slack returned error: {e}"),
            CallbackError::StateMismatch => write!(f, "state mismatch (possible CSRF); login aborted"),
            CallbackError::TimedOut => write!(f, "login timed out before the browser redirect arrived"),
            CallbackError::Io(e) => write!(f, "callback server error: {e}"),
        }
    }
}

const PAGE_OK: &str = "<!doctype html><html><body style=\"font-family:sans-serif\"><h2>Login complete</h2><p>You can close this tab.</p></body></html>";
const PAGE_ERR: &str = "<!doctype html><html><body style=\"font-family:sans-serif\"><h2>Login failed</h2><p>You can close this tab and try again.</p></body></html>";

/// Accepts connections on `listener` until a valid `/callback` arrives, the
/// `state` mismatches (aborts), or `deadline` passes. Non-`/callback` paths get
/// a 404 and the server keeps waiting. Returns the authorization `code`.
pub fn await_callback(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
) -> Result<String, CallbackError> {
    listener.set_nonblocking(true).map_err(|e| CallbackError::Io(e.to_string()))?;
    loop {
        if Instant::now() >= deadline {
            return Err(CallbackError::TimedOut);
        }
        match listener.accept() {
            Ok((stream, _)) => match handle_callback(stream, expected_state) {
                CallbackStep::Code(code) => return Ok(code),
                CallbackStep::Denied(e) => return Err(CallbackError::Denied(e)),
                CallbackStep::StateMismatch => return Err(CallbackError::StateMismatch),
                CallbackStep::KeepWaiting => continue,
            },
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(CallbackError::Io(e.to_string())),
        }
    }
}

enum CallbackStep {
    Code(String),
    Denied(String),
    StateMismatch,
    KeepWaiting,
}

/// Handles one connection: parses the request line, routes `/callback`.
fn handle_callback(mut stream: TcpStream, expected_state: &str) -> CallbackStep {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let request_line = match read_request_line(&mut stream) {
        Some(l) => l,
        None => {
            respond(&mut stream, "400 Bad Request", "text/plain", "bad request");
            return CallbackStep::KeepWaiting;
        }
    };
    // "GET /callback?code=..&state=.. HTTP/1.1"
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != "/callback" {
        respond(&mut stream, "404 Not Found", "text/plain", "not found");
        return CallbackStep::KeepWaiting;
    }
    let params = parse_query(query);
    let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());

    if let Some(err) = get("error") {
        respond(&mut stream, "200 OK", "text/html", PAGE_ERR);
        return CallbackStep::Denied(err);
    }
    let state = get("state").unwrap_or_default();
    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        respond(&mut stream, "400 Bad Request", "text/html", PAGE_ERR);
        return CallbackStep::StateMismatch;
    }
    match get("code") {
        Some(code) if !code.is_empty() => {
            respond(&mut stream, "200 OK", "text/html", PAGE_OK);
            CallbackStep::Code(code)
        }
        _ => {
            respond(&mut stream, "400 Bad Request", "text/html", PAGE_ERR);
            CallbackStep::Denied("missing code".to_string())
        }
    }
}

/// Reads the full request header block (until the blank line) and returns the
/// request line. Draining the whole request avoids a TCP RST when we close the
/// socket with unconsumed client bytes still in flight.
fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while buf.len() < 16384 {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let first = text.lines().next().unwrap_or("").to_string();
    if first.is_empty() { None } else { Some(first) }
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes());
    let _ = stream.flush();
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Access-token fields parsed from a token or refresh response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenFields {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
}

/// Full result of a login token exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginResult {
    pub token: TokenFields,
    pub user_id: String,
    pub team_id: String,
    pub team_name: String,
}

/// Reads access/refresh/expiry from `authed_user` (login) falling back to the
/// response root (refresh), so both Slack shapes parse.
fn parse_token_fields(v: &serde_json::Value) -> Option<TokenFields> {
    let src = v.get("authed_user").filter(|u| u.get("access_token").is_some()).unwrap_or(v);
    let access_token = src.get("access_token")?.as_str()?.to_string();
    let refresh_token = src.get("refresh_token").and_then(|r| r.as_str()).map(String::from);
    let expires_in = src.get("expires_in").and_then(|e| e.as_u64());
    Some(TokenFields { access_token, refresh_token, expires_in })
}

/// Exchanges an authorization `code` for a user token (no client secret).
pub fn exchange_code(
    cfg: &SlackConfig,
    client_id: &str,
    code: &str,
    code_verifier: &str,
) -> Result<LoginResult, String> {
    let url = format!("{}/oauth.v2.access", cfg.api_base);
    let agent = http::agent();
    let resp = http::post_form(
        &agent,
        &url,
        &[
            ("code", code),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
        None,
    )?;
    let v = resp.body;
    if !v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown_error");
        return Err(err.to_string());
    }
    let token = parse_token_fields(&v).ok_or("response missing authed_user.access_token")?;
    let user_id = v
        .get("authed_user")
        .and_then(|u| u.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    let team = v.get("team");
    let team_id = team.and_then(|t| t.get("id")).and_then(|i| i.as_str()).unwrap_or_default().to_string();
    let team_name = team
        .and_then(|t| t.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(LoginResult { token, user_id, team_id, team_name })
}

/// Refreshes a rotating user token with `grant_type=refresh_token`.
pub fn refresh(cfg: &SlackConfig, client_id: &str, refresh_token: &str) -> Result<TokenFields, String> {
    let url = format!("{}/oauth.v2.access", cfg.api_base);
    let agent = http::agent();
    let resp = http::post_form(
        &agent,
        &url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
        None,
    )?;
    let v = resp.body;
    if !v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown_error");
        return Err(err.to_string());
    }
    parse_token_fields(&v).ok_or_else(|| "refresh response missing access_token".to_string())
}

/// Converts [`TokenFields`] to a [`StoredToken`], resolving `expires_in` to an
/// absolute expiry using the current wall clock.
pub fn to_stored(token: &TokenFields) -> StoredToken {
    let expires_at = token.expires_in.map(|secs| now_unix().saturating_add(secs));
    StoredToken {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at,
    }
}

/// Current unix time in seconds.
pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Runs the full browser login on an already-bound `listener`. Generates PKCE,
/// opens the browser (and prints the URL via `on_url`), waits for the callback,
/// exchanges the code, stores the token in `store` and metadata in `apps.toml`.
/// The token is never printed.
#[allow(clippy::too_many_arguments)]
pub fn run_login(
    cfg: &SlackConfig,
    client_id: &str,
    listener: &TcpListener,
    opener: &dyn BrowserOpener,
    store: &dyn TokenStore,
    home: &Path,
    deadline: Instant,
    on_url: &dyn Fn(&str),
) -> Result<LoginResult, String> {
    let verifier = pkce::generate_code_verifier();
    let challenge = pkce::code_challenge(&verifier);
    let state = pkce::generate_state();
    let url = authorize_url(&cfg.authorize_base, client_id, &state, &challenge);

    on_url(&url);
    if let Err(e) = opener.open(&url) {
        // Not fatal: the URL was printed for manual paste.
        eprintln!("gentle-apps: could not open the browser automatically ({e}); paste the URL above");
    }

    let code = await_callback(listener, &state, deadline).map_err(|e| e.to_string())?;
    let result = exchange_code(cfg, client_id, &code, &verifier)?;

    store.save(SLACK_ACCOUNT, &to_stored(&result.token))?;
    let meta = SlackMeta {
        client_id: Some(client_id.to_string()),
        team_id: Some(result.team_id.clone()),
        team_name: Some(result.team_name.clone()),
        user_id: Some(result.user_id.clone()),
    };
    config::save(home, &meta)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn authorize_url_has_exact_params_and_encodings() {
        let url = authorize_url(
            "https://slack.com/oauth/v2/authorize",
            "123.456",
            "STATE123",
            "CHALLENGE_abc",
        );
        assert_eq!(
            url,
            "https://slack.com/oauth/v2/authorize?client_id=123.456&user_scope=search:read,chat:write,users:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history&redirect_uri=http%3A%2F%2Flocalhost%3A47917%2Fcallback&state=STATE123&code_challenge=CHALLENGE_abc&code_challenge_method=S256"
        );
    }

    /// Sends one raw GET request line and returns (status_line, body).
    fn http_get(addr: std::net::SocketAddr, target: &str) -> (String, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
        s.flush().unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status = resp.lines().next().unwrap_or("").to_string();
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    #[test]
    fn callback_with_correct_state_returns_the_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let h = std::thread::spawn(move || await_callback(&listener, "good", deadline));
        let (status, body) = http_get(addr, "/callback?code=THECODE&state=good");
        assert!(status.contains("200"), "{status}");
        assert!(body.contains("Login complete"));
        assert_eq!(h.join().unwrap(), Ok("THECODE".to_string()));
    }

    #[test]
    fn callback_with_wrong_state_returns_400_and_aborts() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let h = std::thread::spawn(move || await_callback(&listener, "expected", deadline));
        let (status, _) = http_get(addr, "/callback?code=X&state=wrong");
        assert!(status.contains("400"), "{status}");
        assert_eq!(h.join().unwrap(), Err(CallbackError::StateMismatch));
    }

    #[test]
    fn unknown_path_gets_404_and_login_keeps_waiting() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let h = std::thread::spawn(move || await_callback(&listener, "good", deadline));
        let (status, _) = http_get(addr, "/favicon.ico");
        assert!(status.contains("404"), "{status}");
        // The server is still waiting: a valid callback now succeeds.
        let (ok_status, _) = http_get(addr, "/callback?code=C2&state=good");
        assert!(ok_status.contains("200"));
        assert_eq!(h.join().unwrap(), Ok("C2".to_string()));
    }

    #[test]
    fn callback_error_access_denied_is_a_clean_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let h = std::thread::spawn(move || await_callback(&listener, "good", deadline));
        let (status, _) = http_get(addr, "/callback?error=access_denied&state=good");
        assert!(status.contains("200"), "{status}");
        assert_eq!(h.join().unwrap(), Err(CallbackError::Denied("access_denied".to_string())));
    }

    #[test]
    fn deadline_elapses_without_a_callback() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(150);
        let result = await_callback(&listener, "good", deadline);
        assert_eq!(result, Err(CallbackError::TimedOut));
    }

    #[test]
    fn parse_token_fields_reads_authed_user_then_root() {
        let login: serde_json::Value = serde_json::from_str(
            r#"{"ok":true,"authed_user":{"access_token":"xoxp-a","refresh_token":"xoxe-r","expires_in":43200,"id":"U1"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_token_fields(&login),
            Some(TokenFields {
                access_token: "xoxp-a".into(),
                refresh_token: Some("xoxe-r".into()),
                expires_in: Some(43200),
            })
        );
        let refreshed: serde_json::Value =
            serde_json::from_str(r#"{"ok":true,"access_token":"xoxp-b","refresh_token":"xoxe-r2","expires_in":43200}"#)
                .unwrap();
        assert_eq!(parse_token_fields(&refreshed).unwrap().access_token, "xoxp-b");
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
