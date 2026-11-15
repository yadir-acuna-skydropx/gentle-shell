//! End-to-end GitHub connector flows against an in-process fake GitHub API, an
//! injected token source and opener, and temp git repos. No real network, `gh`
//! or browser.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentle_apps::connector::Connector;
use gentle_apps::github::{GithubConfig, GithubConnector, Opener, TokenSource};
use gentle_apps::protocol::{EngineMsg, Status};

// ---------- fake HTTP server ----------

#[derive(Clone, Debug)]
#[allow(dead_code)] // method/body are captured for completeness; not all tests read them
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

// ---------- injected token source + opener ----------

struct FixedToken {
    tokens: Mutex<Vec<String>>,
    calls: Arc<AtomicUsize>,
}

impl FixedToken {
    fn single(token: &str) -> (Box<dyn TokenSource>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(FixedToken { tokens: Mutex::new(vec![token.to_string()]), calls: calls.clone() }),
            calls,
        )
    }
    /// Yields each token in turn, repeating the last.
    fn sequence(tokens: &[&str]) -> (Box<dyn TokenSource>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(FixedToken {
                tokens: Mutex::new(tokens.iter().rev().map(|s| s.to_string()).collect()),
                calls: calls.clone(),
            }),
            calls,
        )
    }
}

impl TokenSource for FixedToken {
    fn token(&self) -> Result<Option<String>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut toks = self.tokens.lock().unwrap();
        if toks.len() > 1 {
            Ok(Some(toks.pop().unwrap()))
        } else {
            Ok(toks.last().cloned())
        }
    }
}

struct NoToken;
impl TokenSource for NoToken {
    fn token(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[derive(Default)]
struct RecordingOpener {
    urls: Arc<Mutex<Vec<String>>>,
}
impl Opener for RecordingOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        self.urls.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

fn opener() -> (Box<dyn Opener>, Arc<Mutex<Vec<String>>>) {
    let urls = Arc::new(Mutex::new(Vec::new()));
    (Box::new(RecordingOpener { urls: urls.clone() }), urls)
}

// ---------- helpers ----------

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn init_repo(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["remote", "add", "origin", origin]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn tempdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "gentle-gh-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// A repo at `<own parent>/repo`, so worktree siblings and cleanup stay inside
/// a per-test directory (never the system temp dir itself).
fn repo_in_own_parent(tag: &str) -> (PathBuf, PathBuf) {
    let parent = tempdir(tag);
    let repo = parent.join("repo");
    (parent, repo)
}

fn connector_with(server: &FakeServer, token: Box<dyn TokenSource>, root: PathBuf) -> (GithubConnector, Arc<Mutex<Vec<String>>>) {
    let (op, urls) = opener();
    let cfg = GithubConfig { api_base: server.base() };
    (GithubConnector::with_sources(cfg, root, token, op), urls)
}

/// A handler covering the top-level poll endpoints.
fn poll_handler(rec: &Recorded) -> FakeResp {
    if rec.path == "/search/issues" {
        // total_count varies by query so sections differ.
        let count = if rec.query.contains("review-requested") {
            3
        } else if rec.query.contains("author") {
            2
        } else {
            5
        };
        FakeResp::json(&format!(r#"{{"total_count":{count},"items":[]}}"#))
    } else if rec.path == "/user/repos" {
        FakeResp::json(
            r#"[{"full_name":"octo/repo","default_branch":"main","pushed_at":"2020-01-01T00:00:00Z","html_url":"https://github.com/octo/repo"}]"#,
        )
    } else {
        FakeResp::json("{}")
    }
}

// ---------- tests ----------

#[test]
fn poll_returns_six_sections_with_counts_and_few_requests() {
    let server = FakeServer::start(poll_handler);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("poll")); // not a git repo
    let items = c.poll().unwrap();
    assert_eq!(items.len(), 6);
    assert!(items.iter().all(|i| i.expandable));
    assert_eq!(items[0].title, "Review requested (3)");
    assert_eq!(items[1].title, "My PRs (2)");
    assert_eq!(items[2].title, "Assigned issues (5)");
    assert_eq!(items[3].title, "Repositories (1)");
    assert_eq!(items[4].title, "Worktrees (local) (0)");
    // No local GitHub repo here, so Workflows is empty and makes no request.
    assert_eq!(items[5].title, "Workflows (0)");
    assert_eq!(c.status(), Status::Connected);
    // 3 searches + 1 repos = 4 HTTP requests (no runs request); well under ~6.
    assert_eq!(server.requests().len(), 4);
}

#[test]
fn poll_counts_workflow_runs_for_a_local_repo_in_one_request() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/actions/runs" {
            FakeResp::json(
                r#"{"total_count":2,"workflow_runs":[
                  {"id":1,"name":"CI","head_branch":"main","status":"completed","conclusion":"success","html_url":"https://github.com/octo/repo/actions/runs/1","created_at":"2020-01-01T00:00:00Z"},
                  {"id":2,"name":"CI","head_branch":"main","status":"completed","conclusion":"failure","html_url":"https://github.com/octo/repo/actions/runs/2","created_at":"2020-01-01T00:00:00Z"}
                ]}"#,
            )
        } else {
            poll_handler(rec)
        }
    });
    let repo = tempdir("wf-poll");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let items = c.poll().unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(items[5].title, "Workflows (2)");
    // 3 searches + 1 repos + 1 runs = 5 requests; still under ~6.
    let runs_requests = server.requests().iter().filter(|r| r.path == "/repos/octo/repo/actions/runs").count();
    assert_eq!(runs_requests, 1, "exactly one runs request per poll");
    assert!(server.requests().len() <= 6);
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn expand_workflows_lists_runs_with_status_glyphs_and_rerun_on_failure() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/actions/runs" {
            FakeResp::json(
                r#"{"total_count":3,"workflow_runs":[
                  {"id":1,"name":"CI","head_branch":"main","status":"completed","conclusion":"success","html_url":"https://github.com/octo/repo/actions/runs/1","created_at":"2020-01-01T00:00:00Z"},
                  {"id":2,"name":"Deploy","head_branch":"dev","status":"completed","conclusion":"failure","html_url":"https://github.com/octo/repo/actions/runs/2","created_at":"2020-01-01T00:00:00Z"},
                  {"id":3,"name":"Nightly","head_branch":"main","status":"in_progress","conclusion":null,"html_url":"https://github.com/octo/repo/actions/runs/3","created_at":"2020-01-01T00:00:00Z"}
                ]}"#,
            )
        } else {
            poll_handler(rec)
        }
    });
    let repo = tempdir("wf-expand");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let items = c.expand("sec:workflows").unwrap();
    assert_eq!(items.len(), 3);
    assert!(items[0].title.starts_with("CI \u{b7} main \u{b7} \u{2713}"), "success \u{2713}: {}", items[0].title);
    assert!(items[1].title.starts_with("Deploy \u{b7} dev \u{b7} \u{2717}"), "failure \u{2717}: {}", items[1].title);
    assert!(items[2].title.contains("\u{25cf}"), "in_progress \u{25cf}: {}", items[2].title);
    assert!(items.iter().all(|i| i.expandable), "a run expands to its jobs");
    // rerun_failed is offered only on the failed run.
    assert!(items[0].actions.iter().all(|a| a.id != "rerun_failed"), "no rerun on success");
    let rerun = items[1].actions.iter().find(|a| a.id == "rerun_failed").expect("rerun on failure");
    assert!(rerun.confirm, "rerun asks for confirmation");
    assert!(items[2].actions.iter().all(|a| a.id != "rerun_failed"), "no rerun while in progress");
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn expand_workflows_reports_no_local_github_repository_outside_a_repo() {
    let server = FakeServer::start(poll_handler);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("wf-norepo"));
    let items = c.expand("sec:workflows").unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "no local GitHub repository");
    assert!(items[0].actions.is_empty());
}

#[test]
fn expand_a_run_lists_its_jobs_with_conclusions() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/actions/runs/2/jobs" {
            FakeResp::json(
                r#"{"jobs":[
                  {"id":10,"name":"build","status":"completed","conclusion":"success","html_url":"https://github.com/octo/repo/actions/runs/2/job/10"},
                  {"id":11,"name":"test","status":"completed","conclusion":"failure","html_url":"https://github.com/octo/repo/actions/runs/2/job/11"}
                ]}"#,
            )
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, urls) = connector_with(&server, token, tempdir("wf-jobs"));
    let jobs = c.expand("run|octo|repo|2").unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].title, "\u{2713} build");
    assert_eq!(jobs[1].title, "\u{2717} test");
    assert!(jobs.iter().all(|j| !j.expandable));
    // open on a job uses its html_url.
    c.action(&jobs[0].id, "open").unwrap();
    assert_eq!(urls.lock().unwrap().as_slice(), &["https://github.com/octo/repo/actions/runs/2/job/10".to_string()]);
}

#[test]
fn open_and_rerun_failed_hit_the_run_endpoints() {
    let reran = Arc::new(AtomicUsize::new(0));
    let reran2 = reran.clone();
    let server = FakeServer::start(move |rec| {
        if rec.path == "/repos/octo/repo/actions/runs" {
            FakeResp::json(
                r#"{"total_count":1,"workflow_runs":[{"id":2,"name":"CI","head_branch":"main","status":"completed","conclusion":"failure","html_url":"https://github.com/octo/repo/actions/runs/2","created_at":"2020-01-01T00:00:00Z"}]}"#,
            )
        } else if rec.path == "/repos/octo/repo/actions/runs/2/rerun-failed-jobs" {
            reran2.fetch_add(1, Ordering::SeqCst);
            FakeResp { status: 201, headers: vec![], body: "{}".into() }
        } else {
            poll_handler(rec)
        }
    });
    let repo = tempdir("wf-actions");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let (token, _) = FixedToken::single("tok");
    let (mut c, urls) = connector_with(&server, token, repo.clone());
    let runs = c.expand("sec:workflows").unwrap();
    let run_id = runs[0].id.clone();
    // open uses the run html_url.
    c.action(&run_id, "open").unwrap();
    assert_eq!(urls.lock().unwrap().as_slice(), &["https://github.com/octo/repo/actions/runs/2".to_string()]);
    // rerun_failed POSTs to the rerun-failed-jobs endpoint.
    let msg = c.action(&run_id, "rerun_failed").unwrap();
    assert!(msg.contains("re-running failed jobs"), "{msg}");
    assert_eq!(reran.load(Ordering::SeqCst), 1);
    assert_eq!(c.action(&run_id, "rerun_failed").is_ok(), true);
    std::fs::remove_dir_all(&repo).ok();
}

/// Builds a JSON array of `n` branch objects named `<prefix><i>`.
fn branch_page(prefix: &str, n: usize) -> String {
    let mut body = String::from("[");
    for i in 0..n {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(r#"{{"name":"{prefix}{i}","commit":{{"sha":"s{prefix}{i}"}}}}"#));
    }
    body.push(']');
    body
}

#[test]
fn expand_repo_paginates_branches_across_pages() {
    // A full first page (100) triggers a second request; the short second page
    // (< 100) ends the walk with everything collected and no marker.
    let server = FakeServer::start(|rec| {
        if rec.path == "/user/repos" {
            poll_handler(rec)
        } else if rec.path == "/search/issues" {
            FakeResp::json(r#"{"total_count":0,"items":[]}"#)
        } else if rec.path == "/repos/octo/repo/pulls" {
            FakeResp::json("[]")
        } else if rec.path == "/repos/octo/repo/branches" {
            if rec.query.contains("page=2") {
                FakeResp::json(&branch_page("q", 3))
            } else {
                FakeResp::json(&branch_page("p", 100))
            }
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("branch-pages"));
    c.poll().unwrap();
    let items = c.expand("repo|octo|repo").unwrap();
    assert_eq!(items.len(), 103, "100 from page 1 + 3 from page 2, no marker");
    assert_eq!(items.first().unwrap().title, "p0");
    assert_eq!(items.last().unwrap().title, "q2");
    let branch_requests = server.requests().iter().filter(|r| r.path == "/repos/octo/repo/branches").count();
    assert_eq!(branch_requests, 2, "two pages fetched");
}

#[test]
fn branches_beyond_the_page_cap_get_a_more_marker() {
    // Every page returns a full 100 branches, so the 5-page cap is hit and a
    // "\u2026 more branches" marker is appended.
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/branches" {
            FakeResp::json(&branch_page("b", 100))
        } else {
            FakeResp::json("[]")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("branch-cap"));
    let items = c.expand("repo|octo|repo").unwrap();
    // Exactly 5 pages fetched (100 each) = 500, capped, plus the marker.
    let branch_requests = server.requests().iter().filter(|r| r.path == "/repos/octo/repo/branches").count();
    assert_eq!(branch_requests, 5, "stops at the 5-page cap");
    assert_eq!(items.len(), 501, "500 branches + the marker");
    assert_eq!(items.last().unwrap().title, "\u{2026} more branches");
}

#[test]
fn my_prs_graphql_rereads_the_token_once_on_401() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/graphql" {
            if rec.auth.as_deref() == Some("Bearer stale") {
                FakeResp { status: 401, headers: vec![], body: r#"{"message":"Bad credentials"}"#.into() }
            } else {
                FakeResp::json(
                    r#"{"data":{"search":{"nodes":[{"number":1,"title":"ok","url":"https://github.com/octo/repo/pull/1","repository":{"nameWithOwner":"octo/repo"},"author":{"login":"ann"},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}]}}}"#,
                )
            }
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, calls) = FixedToken::sequence(&["stale", "fresh"]);
    let (mut c, _) = connector_with(&server, token, tempdir("gql-401"));
    let items = c.expand("sec:mine").unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "token read for stale, then re-read once");
}



#[test]
fn the_token_never_appears_in_the_emitted_items_or_status() {
    let server = FakeServer::start(poll_handler);
    let secret = "super-secret-token-xyz";
    let (token, _) = FixedToken::single(secret);
    let (mut c, _) = connector_with(&server, token, tempdir("secret"));
    let items = c.poll().unwrap();
    let msg = EngineMsg::Items { app: "github".into(), items };
    let line = msg.to_line();
    assert!(!line.contains(secret), "token must never reach the protocol");
    assert!(!c.detail().unwrap_or_default().contains(secret));
    // The server did receive it as a Bearer header (that is the point of auth).
    assert!(server.requests().iter().all(|r| r.auth.as_deref() == Some(&format!("Bearer {secret}"))));
}

#[test]
fn gh_missing_or_logged_out_is_disconnected_with_the_detail() {
    let server = FakeServer::start(poll_handler);
    let (mut c, _) = connector_with(&server, Box::new(NoToken), tempdir("notok"));
    let items = c.poll().unwrap();
    assert!(items.is_empty());
    assert_eq!(c.status(), Status::Disconnected);
    assert_eq!(c.detail().as_deref(), Some("run `gh auth login`"));
    assert_eq!(server.requests().len(), 0, "no requests without a token");
}

#[test]
fn a_401_rereads_the_token_once_then_succeeds() {
    let server = FakeServer::start(|rec| {
        if rec.auth.as_deref() == Some("Bearer stale") {
            FakeResp { status: 401, headers: vec![], body: r#"{"message":"Bad credentials"}"#.into() }
        } else {
            poll_handler(rec)
        }
    });
    let (token, calls) = FixedToken::sequence(&["stale", "fresh"]);
    let (mut c, _) = connector_with(&server, token, tempdir("reauth"));
    let items = c.poll().unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(c.status(), Status::Connected);
    // The source was read for "stale", then re-read once for "fresh".
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_rate_limited_response_backs_off_and_retries() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = FakeServer::start(move |rec| {
        if rec.path == "/search/issues" && rec.query.contains("review-requested") {
            let n = hits2.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return FakeResp {
                    status: 403,
                    headers: vec![
                        ("Retry-After".into(), "0".into()),
                        ("X-RateLimit-Remaining".into(), "0".into()),
                    ],
                    body: r#"{"message":"rate limited"}"#.into(),
                };
            }
        }
        poll_handler(rec)
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("rl"));
    let items = c.poll().unwrap();
    assert_eq!(items.len(), 6);
    // The first review-requested search was retried after the back-off.
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[test]
fn expand_mine_maps_ci_states_from_graphql() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/graphql" {
            FakeResp::json(
                r#"{"data":{"search":{"nodes":[
                  {"number":1,"title":"ok","url":"https://github.com/octo/repo/pull/1","repository":{"nameWithOwner":"octo/repo"},"author":{"login":"ann"},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
                  {"number":2,"title":"bad","url":"https://github.com/octo/repo/pull/2","repository":{"nameWithOwner":"octo/repo"},"author":{"login":"bob"},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}},
                  {"number":3,"title":"none","url":"https://github.com/octo/repo/pull/3","repository":{"nameWithOwner":"octo/repo"},"author":{"login":"cat"},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}
                ]}}}"#,
            )
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("mine"));
    let items = c.expand("sec:mine").unwrap();
    assert_eq!(items.len(), 3);
    assert!(items[0].title.starts_with('\u{2713}'), "success → ✓: {}", items[0].title);
    assert!(items[1].title.starts_with('\u{2717}'), "failure → ✗: {}", items[1].title);
    assert!(items[2].title.starts_with('\u{2013}'), "none → –: {}", items[2].title);
    assert_eq!(items[0].id, "pr:octo/repo#1");
}

#[test]
fn expand_repo_lists_branches_with_default_and_pr_marks() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/user/repos" {
            poll_handler(rec)
        } else if rec.path == "/search/issues" {
            FakeResp::json(r#"{"total_count":0,"items":[]}"#)
        } else if rec.path == "/repos/octo/repo/pulls" {
            FakeResp::json(r#"[{"head":{"ref":"feature"}}]"#)
        } else if rec.path == "/repos/octo/repo/branches" {
            FakeResp::json(
                r#"[{"name":"main","commit":{"sha":"aaa"}},{"name":"feature","commit":{"sha":"bbb"}},{"name":"old","commit":{"sha":"ccc"}}]"#,
            )
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("branches"));
    c.poll().unwrap(); // populates default_branch (main)
    let items = c.expand("repo|octo|repo").unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].title, "main (default)");
    assert_eq!(items[1].title, "feature (PR)");
    assert_eq!(items[2].title, "old");
    assert!(items.iter().all(|i| i.expandable));
    assert_eq!(items[0].id, "branch|octo|repo|aaa|main");
}

#[test]
fn expand_branch_then_dir_walks_the_tree_lazily() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/git/trees/aaa" {
            FakeResp::json(
                r#"{"tree":[{"path":"src","type":"tree","sha":"sub1"},{"path":"README.md","type":"blob","sha":"blob1","size":2048}]}"#,
            )
        } else if rec.path == "/repos/octo/repo/git/trees/sub1" {
            FakeResp::json(r#"{"tree":[{"path":"main.rs","type":"blob","sha":"blob2","size":10}]}"#)
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("tree"));
    let root = c.expand("branch|octo|repo|aaa|main").unwrap();
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].title, "src/");
    assert!(root[0].expandable);
    assert_eq!(root[0].id, "tree|octo|repo|sub1|main|src");
    assert_eq!(root[1].title, "README.md  (2.0K)");
    assert!(!root[1].expandable);
    assert_eq!(root[1].link, "https://github.com/octo/repo/blob/main/README.md");

    let sub = c.expand("tree|octo|repo|sub1|main|src").unwrap();
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0].title, "main.rs  (10B)");
    assert_eq!(sub[0].link, "https://github.com/octo/repo/blob/main/src/main.rs");
}

#[test]
fn a_tree_level_is_capped_at_500_with_a_more_marker() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/git/trees/big" {
            let mut entries = String::new();
            for i in 0..600 {
                if i > 0 {
                    entries.push(',');
                }
                entries.push_str(&format!(r#"{{"path":"f{i}","type":"blob","sha":"s{i}","size":1}}"#));
            }
            FakeResp::json(&format!(r#"{{"tree":[{entries}]}}"#))
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("cap"));
    let items = c.expand("branch|octo|repo|big|main").unwrap();
    assert_eq!(items.len(), 501);
    assert_eq!(items[500].title, "\u{2026} 100 more");
}

#[test]
fn worktrees_section_lists_paths_branches_and_dirty_state() {
    let server = FakeServer::start(poll_handler);
    let repo = tempdir("wt-main");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let items = c.expand("sec:worktrees").unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].title.contains("main"));
    assert!(items[0].title.ends_with("clean"), "clean tree: {}", items[0].title);

    // Make the tree dirty and re-expand.
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    let items = c.expand("sec:worktrees").unwrap();
    assert!(items[0].title.ends_with("dirty"), "dirty tree: {}", items[0].title);
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn worktrees_section_reports_no_local_repository_outside_a_repo() {
    let server = FakeServer::start(poll_handler);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("norepo"));
    let items = c.expand("sec:worktrees").unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "no local repository");
    assert!(items[0].actions.is_empty());
}

#[test]
fn worktree_add_creates_the_worktree_and_refuses_an_existing_dir() {
    let server = FakeServer::start(poll_handler);
    let (own, repo) = repo_in_own_parent("wtadd");
    init_repo(&repo, "https://github.com/octo/repo.git");
    git(&repo, &["branch", "feature"]);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let created = c.action("branch|octo|repo|bbb|feature", "worktree_add").unwrap();
    assert!(Path::new(&created).is_dir(), "worktree dir exists: {created}");
    assert!(created.ends_with(&format!("{}-feature", repo.file_name().unwrap().to_string_lossy())));
    assert!(Path::new(&created).starts_with(&own), "worktree stays inside the test dir");
    // Refuse when the target already exists.
    let err = c.action("branch|octo|repo|bbb|feature", "worktree_add").unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    std::fs::remove_dir_all(&own).ok();
}

#[test]
fn worktree_add_refuses_a_non_matching_origin() {
    let server = FakeServer::start(poll_handler);
    let repo = tempdir("wtadd-mismatch");
    init_repo(&repo, "https://github.com/someoneelse/other.git");
    git(&repo, &["branch", "feature"]);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let err = c.action("branch|octo|repo|bbb|feature", "worktree_add").unwrap_err();
    assert!(err.contains("does not match"), "{err}");
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn checkout_succeeds_on_clean_and_refuses_on_dirty_without_changing_files() {
    let server = FakeServer::start(poll_handler);
    let repo = tempdir("checkout");
    init_repo(&repo, "https://github.com/octo/repo.git");
    // Create another branch with different content, back on main.
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(repo.join("a.txt"), "feature-content\n").unwrap();
    git(&repo, &["commit", "-qam", "feature"]);
    git(&repo, &["checkout", "-q", "main"]);

    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());

    // Clean tree: checkout to feature succeeds and the file changes to feature.
    let ok = c.action("branch|octo|repo|bbb|feature", "checkout").unwrap();
    assert!(ok.contains("checked out feature"), "{ok}");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "feature-content\n");

    // Dirty tree: refuse and leave the file untouched.
    std::fs::write(repo.join("a.txt"), "uncommitted edit\n").unwrap();
    let err = c.action("branch|octo|repo|bbb|main", "checkout").unwrap_err();
    assert_eq!(err, "working tree has changes — commit or stash first");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "uncommitted edit\n");
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn open_action_calls_the_opener_with_the_exact_url() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/git/trees/aaa" {
            FakeResp::json(r#"{"tree":[{"path":"README.md","type":"blob","sha":"b1","size":5}]}"#)
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, urls) = connector_with(&server, token, tempdir("open"));
    let files = c.expand("branch|octo|repo|aaa|main").unwrap();
    let file_id = files[0].id.clone();
    let msg = c.action(&file_id, "open").unwrap();
    assert!(msg.contains("https://github.com/octo/repo/blob/main/README.md"));
    assert_eq!(
        urls.lock().unwrap().as_slice(),
        &["https://github.com/octo/repo/blob/main/README.md".to_string()]
    );
}

#[test]
fn view_action_fetches_issue_detail_and_comments_off_the_ui_thread() {
    // S27: the `view` action returns the reader content built from the issue
    // JSON + its comments.
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/issues/7" {
            FakeResp::json(
                r#"{"title":"Crash on save","state":"open","user":{"login":"ann"},"html_url":"https://github.com/octo/repo/issues/7","body":"It crashes.","labels":[{"name":"bug"}]}"#,
            )
        } else if rec.path == "/repos/octo/repo/issues/7/comments" {
            FakeResp::json(r#"[{"user":{"login":"bob"},"body":"Reproduced."}]"#)
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("view"));
    let (title, body) = c.view("issue:octo/repo#7").unwrap();
    assert_eq!(title, "octo/repo#7 \u{b7} Crash on save");
    assert!(body.contains("octo/repo#7 \u{b7} issue \u{b7} open \u{b7} @ann \u{b7} bug"), "{body}");
    assert!(body.contains("https://github.com/octo/repo/issues/7"));
    assert!(body.contains("It crashes."));
    assert!(body.contains("@bob:\nReproduced."));
}

#[test]
fn view_action_uses_the_pulls_endpoint_for_a_pr() {
    let server = FakeServer::start(|rec| {
        if rec.path == "/repos/octo/repo/pulls/9" {
            FakeResp::json(
                r#"{"title":"A PR","state":"open","user":{"login":"dan"},"html_url":"https://github.com/octo/repo/pull/9","body":"work"}"#,
            )
        } else if rec.path == "/repos/octo/repo/issues/9/comments" {
            FakeResp::json("[]")
        } else {
            FakeResp::json("{}")
        }
    });
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("view-pr"));
    let (title, body) = c.view("pr:octo/repo#9").unwrap();
    assert_eq!(title, "octo/repo#9 \u{b7} A PR");
    assert!(body.contains("octo/repo#9 \u{b7} PR \u{b7} open \u{b7} @dan"), "{body}");
    // The pulls endpoint was hit (not issues/9).
    assert!(server.requests().iter().any(|r| r.path == "/repos/octo/repo/pulls/9"));
}

#[test]
fn an_unknown_action_is_an_error() {
    let server = FakeServer::start(poll_handler);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("unknown"));
    let err = c.action("branch|octo|repo|aaa|main", "frobnicate").unwrap_err();
    assert!(err.contains("unknown action"), "{err}");
}

fn head_branch(repo: &Path) -> String {
    let out = std::process::Command::new("git").args(["-C"]).arg(repo).args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn checkout_rejects_option_like_branch_names_and_keeps_head() {
    let server = FakeServer::start(poll_handler);
    let repo = tempdir("checkout-inject");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    for evil in ["--orphan=x", "-b", "-", "--detach"] {
        let err = c.action(&format!("branch|octo|repo|s|{evil}"), "checkout").unwrap_err();
        assert!(err.contains("invalid branch name"), "{evil}: {err}");
        assert_eq!(head_branch(&repo), "main", "{evil} must not move HEAD");
    }
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn worktree_add_rejects_option_like_branch_names_and_creates_nothing() {
    let server = FakeServer::start(poll_handler);
    let (parent, repo) = repo_in_own_parent("wtadd-inject");
    init_repo(&repo, "https://github.com/octo/repo.git");
    let before = std::fs::read_dir(&parent).unwrap().count();
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    for evil in ["-rf", "--upload-pack=touch /tmp/pwned", "--orphan=x"] {
        let err = c.action(&format!("branch|octo|repo|s|{evil}"), "worktree_add").unwrap_err();
        assert!(err.contains("invalid branch name"), "{evil}: {err}");
    }
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), before, "nothing created");
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
fn checkout_treats_untracked_only_as_dirty() {
    let server = FakeServer::start(poll_handler);
    let repo = tempdir("checkout-untracked");
    init_repo(&repo, "https://github.com/octo/repo.git");
    git(&repo, &["branch", "feature"]);
    std::fs::write(repo.join("new.txt"), "keep me\n").unwrap();
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, repo.clone());
    let err = c.action("branch|octo|repo|s|feature", "checkout").unwrap_err();
    assert_eq!(err, "working tree has changes — commit or stash first");
    assert_eq!(std::fs::read_to_string(repo.join("new.txt")).unwrap(), "keep me\n");
    assert_eq!(head_branch(&repo), "main");
    std::fs::remove_dir_all(&repo).ok();
}

// ---------- T35 / S31 / F1: control-char sanitization ----------

#[test]
fn build_detail_strips_terminal_control_and_escape_sequences() {
    // A malicious issue with OSC window-title, OSC 52 clipboard, CSI clear and a
    // bidi override in the title, body and a comment.
    let obj = serde_json::json!({
        "title": "pwn\x1b]0;hijack\x07\u{202e}title",
        "state": "open",
        "user": {"login": "attacker\x1b[31m"},
        "html_url": "https://example/1",
        "body": "line1\x1b[2J\nline2\x1b]52;c;ZXZpbA==\x07end",
    });
    let comments = vec![serde_json::json!({
        "user": {"login": "c\x07"},
        "body": "hi\x1bthere",
    })];
    let (title, body) = gentle_apps::github::build_detail("octo/repo", 7, false, &obj, &comments);
    assert!(!title.contains('\x1b'), "title: {title:?}");
    assert!(!title.contains('\n'), "window title must stay one line: {title:?}");
    assert!(title.chars().all(|c| !gentle_apps::sanitize::is_unsafe(c)));
    // The body keeps newlines but no other control/escape/bidi bytes.
    assert!(body.contains('\n'));
    assert!(body.chars().all(|c| c == '\n' || !gentle_apps::sanitize::is_unsafe(c)), "body: {body:?}");
    assert!(!body.contains('\x1b'));
}
