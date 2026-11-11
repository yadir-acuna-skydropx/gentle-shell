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
fn poll_returns_five_sections_with_counts_and_few_requests() {
    let server = FakeServer::start(poll_handler);
    let (token, _) = FixedToken::single("tok");
    let (mut c, _) = connector_with(&server, token, tempdir("poll")); // not a git repo
    let items = c.poll().unwrap();
    assert_eq!(items.len(), 5);
    assert!(items.iter().all(|i| i.expandable));
    assert_eq!(items[0].title, "Review requested (3)");
    assert_eq!(items[1].title, "My PRs (2)");
    assert_eq!(items[2].title, "Assigned issues (5)");
    assert_eq!(items[3].title, "Repositories (1)");
    assert_eq!(items[4].title, "Worktrees (local) (0)");
    assert_eq!(c.status(), Status::Connected);
    // 3 searches + 1 repos = 4 HTTP requests; well under ~10.
    assert_eq!(server.requests().len(), 4);
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
    assert_eq!(items.len(), 5);
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
    assert_eq!(items.len(), 5);
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
