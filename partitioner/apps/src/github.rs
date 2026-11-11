//! The GitHub connector (T15, S15): surfaces review-requested PRs, your PRs with
//! CI state, assigned issues, your repositories → branches → trees, and the
//! local worktrees of the current repo. It offers read + simple actions (open in
//! the browser, add a worktree for a branch, checkout a branch on a clean tree).
//!
//! Auth reuses the `gh` CLI session: the token comes from `gh auth token`
//! (injected [`TokenSource`]) and is kept only in memory — never logged, printed,
//! `Debug`-formatted or sent over the protocol. HTTP uses the shared ureq helper
//! with the GitHub headers; the base URL is overridable for tests via
//! `GENTLE_APPS_GITHUB_API`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::connector::{Connector, Result};
use crate::http::{self, GithubResponse};
use crate::protocol::{Item, ItemAction, Status};

/// Poll every 60 seconds (S15).
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Cap on tree entries shown per level, with a trailing "… N more".
pub const MAX_ENTRIES: usize = 500;
/// Longest we honour a rate-limit back-off before giving up this cycle.
const MAX_BACKOFF: u64 = 60;
/// Timeout for a local git command.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Hint shown when there is no usable `gh` session.
pub const DISCONNECTED_DETAIL: &str = "run `gh auth login`";

/// Overridable API base (production default, or a fake server in tests).
#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub api_base: String,
}

impl Default for GithubConfig {
    fn default() -> Self {
        GithubConfig { api_base: "https://api.github.com".to_string() }
    }
}

impl GithubConfig {
    /// Reads `GENTLE_APPS_GITHUB_API`, else the production default.
    pub fn from_env() -> Self {
        let d = Self::default();
        GithubConfig { api_base: std::env::var("GENTLE_APPS_GITHUB_API").unwrap_or(d.api_base) }
    }
}

/// Where the access token comes from. Injected so tests never shell out to `gh`.
pub trait TokenSource: Send {
    /// Returns the token, or `None` when `gh` is missing or logged out.
    fn token(&self) -> std::result::Result<Option<String>, String>;
}

/// Production token source: `gh auth token` (no shell). The token is only ever
/// returned to the caller to be held in memory; it is never printed here.
pub struct GhCliTokenSource;

impl TokenSource for GhCliTokenSource {
    fn token(&self) -> std::result::Result<Option<String>, String> {
        match Command::new("gh").args(["auth", "token"]).output() {
            Ok(out) if out.status.success() => {
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                Ok(if t.is_empty() { None } else { Some(t) })
            }
            // Non-zero exit: logged out. Spawn error: gh missing. Both → no token.
            Ok(_) | Err(_) => Ok(None),
        }
    }
}

/// Opens a URL in the user's browser. Injected so tests record the URL.
pub trait Opener: Send {
    fn open(&self, url: &str) -> std::result::Result<(), String>;
}

/// Production opener: `xdg-open <url>` (no shell).
pub struct XdgOpener;

impl Opener for XdgOpener {
    fn open(&self, url: &str) -> std::result::Result<(), String> {
        Command::new("xdg-open").arg(url).spawn().map(|_| ()).map_err(|e| format!("xdg-open: {e}"))
    }
}

/// Maps a GraphQL `statusCheckRollup.state` to the CI glyph (S15).
pub fn ci_glyph(state: Option<&str>) -> char {
    match state.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("SUCCESS") => '\u{2713}',                                      // ✓
        Some("FAILURE") | Some("ERROR") => '\u{2717}',                      // ✗
        Some("PENDING") | Some("EXPECTED") | Some("IN_PROGRESS") | Some("QUEUED") => '\u{25cf}', // ●
        _ => '\u{2013}',                                                     // – (none)
    }
}

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    /// The branch short name, or `None` when detached.
    pub branch: Option<String>,
    pub detached: bool,
}

/// Parses `git worktree list --porcelain` into worktrees (skips `bare`).
pub fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut bare = false;
    let flush = |path: &mut Option<String>, branch: &mut Option<String>, detached: &mut bool, bare: &mut bool, out: &mut Vec<Worktree>| {
        if let Some(p) = path.take() {
            if !*bare {
                out.push(Worktree { path: p, branch: branch.take(), detached: *detached });
            }
        }
        *branch = None;
        *detached = false;
        *bare = false;
    };
    for line in porcelain.lines() {
        if line.is_empty() {
            flush(&mut path, &mut branch, &mut detached, &mut bare, &mut out);
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut detached, &mut bare, &mut out);
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.trim_start_matches("refs/heads/").to_string());
        } else if line == "detached" {
            detached = true;
        } else if line == "bare" {
            bare = true;
        }
    }
    flush(&mut path, &mut branch, &mut detached, &mut bare, &mut out);
    out
}

/// Normalizes an `origin` remote URL to `(owner, name)` for https/ssh forms.
pub fn normalize_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let rest = if let Some(r) = url.strip_prefix("git@github.com:") {
        r.to_string()
    } else if let Some(r) = url.strip_prefix("ssh://git@github.com/") {
        r.to_string()
    } else if let Some(r) = url.strip_prefix("https://github.com/") {
        r.to_string()
    } else if let Some(r) = url.strip_prefix("http://github.com/") {
        r.to_string()
    } else {
        return None;
    };
    let rest = rest.strip_suffix(".git").unwrap_or(&rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner, name))
}

/// True when two repo identities match (owner case-insensitive, name exact).
pub fn repo_matches(a: &(String, String), owner: &str, name: &str) -> bool {
    a.0.eq_ignore_ascii_case(owner) && a.1 == name
}

/// Sanitizes a branch name into a filesystem-safe worktree suffix.
pub fn sanitize_branch(branch: &str) -> String {
    branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Minimal percent-encoding for a query value.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Seconds to wait when `resp` signals a rate limit (403/429), else `None`.
pub fn rate_limit_wait(resp: &GithubResponse, now: u64) -> Option<u64> {
    if resp.status != 403 && resp.status != 429 {
        return None;
    }
    if let Some(ra) = resp.retry_after.as_deref().and_then(|s| s.trim().parse::<u64>().ok()) {
        return Some(ra.min(MAX_BACKOFF));
    }
    if resp.rate_remaining == Some(0) {
        if let Some(reset) = resp.rate_reset {
            return Some(reset.saturating_sub(now).min(MAX_BACKOFF));
        }
        return Some(1);
    }
    None
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parses `YYYY-MM-DDTHH:MM:SSZ` into Unix seconds (best effort).
pub fn parse_iso8601(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '.']).next().unwrap_or(time);
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let se: i64 = t.next().unwrap_or("0").parse().ok()?;
    let days = days_from_civil(y, mo, da);
    let secs = days * 86400 + h * 3600 + mi * 60 + se;
    if secs < 0 { None } else { Some(secs as u64) }
}

/// Human relative time like "just now", "5m ago", "3h ago", "2d ago", "4mo ago".
pub fn relative_time(delta_secs: i64) -> String {
    let d = delta_secs.max(0);
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else if d < 2_592_000 {
        format!("{}d ago", d / 86400)
    } else {
        format!("{}mo ago", d / 2_592_000)
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `owner/name` parsed from a search result's `repository_url`.
fn owner_name_from_repo_url(url: &str) -> Option<(String, String)> {
    let rest = url.rsplit("/repos/").next()?;
    let mut parts = rest.split('/');
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

/// Cached repository summary (from `/user/repos`).
#[derive(Debug, Clone)]
struct RepoInfo {
    owner: String,
    name: String,
    default_branch: String,
    pushed_at: Option<u64>,
    html_url: String,
}

fn open_action() -> ItemAction {
    ItemAction { id: "open".into(), label: "Open in browser".into(), confirm: false }
}

fn branch_actions() -> Vec<ItemAction> {
    vec![
        open_action(),
        ItemAction { id: "worktree_add".into(), label: "Create worktree".into(), confirm: true },
        ItemAction { id: "checkout".into(), label: "Checkout branch".into(), confirm: true },
    ]
}

/// The GitHub connector.
pub struct GithubConnector {
    cfg: GithubConfig,
    token_source: Box<dyn TokenSource>,
    opener: Box<dyn Opener>,
    repo_root: PathBuf,
    agent: ureq::Agent,
    status: Status,
    detail: Option<String>,
    /// In-memory token (never logged). Re-read from the source on a 401.
    token: Option<String>,
    repos: Vec<RepoInfo>,
    default_branch: HashMap<String, String>,
    /// Open URL for each addressable item id.
    links: HashMap<String, String>,
}

impl GithubConnector {
    /// Builds the connector with the production token source and opener.
    pub fn new(cfg: GithubConfig, repo_root: PathBuf) -> Self {
        Self::with_sources(cfg, repo_root, Box::new(GhCliTokenSource), Box::new(XdgOpener))
    }

    /// Builds the connector with injected sources (tests).
    pub fn with_sources(
        cfg: GithubConfig,
        repo_root: PathBuf,
        token_source: Box<dyn TokenSource>,
        opener: Box<dyn Opener>,
    ) -> Self {
        GithubConnector {
            cfg,
            token_source,
            opener,
            repo_root,
            agent: http::agent(),
            status: Status::Disconnected,
            detail: Some(DISCONNECTED_DETAIL.to_string()),
            token: None,
            repos: Vec::new(),
            default_branch: HashMap::new(),
            links: HashMap::new(),
        }
    }

    fn ensure_token(&mut self) -> std::result::Result<Option<String>, String> {
        if self.token.is_none() {
            self.token = self.token_source.token()?;
        }
        Ok(self.token.clone())
    }

    /// GET a REST path (already including any query). Honors one rate-limit
    /// back-off and re-reads the token once on a 401.
    fn api_get(&mut self, path: &str) -> std::result::Result<GithubResponse, String> {
        let token = match self.ensure_token()? {
            Some(t) => t,
            None => return Err(DISCONNECTED_DETAIL.to_string()),
        };
        let url = format!("{}{}", self.cfg.api_base, path);
        let resp = http::github_get(&self.agent, &url, &token)?;
        let resp = self.maybe_backoff(resp, &url, &token)?;
        if resp.status == 401 {
            // Re-read the token once (the gh session may have rotated).
            self.token = self.token_source.token()?;
            if let Some(t2) = self.token.clone() {
                let r2 = http::github_get(&self.agent, &url, &t2)?;
                return self.maybe_backoff(r2, &url, &t2);
            }
        }
        Ok(resp)
    }

    fn maybe_backoff(
        &self,
        resp: GithubResponse,
        url: &str,
        token: &str,
    ) -> std::result::Result<GithubResponse, String> {
        if let Some(wait) = rate_limit_wait(&resp, now_unix()) {
            thread::sleep(Duration::from_secs(wait));
            return http::github_get(&self.agent, url, token);
        }
        Ok(resp)
    }

    /// `total_count` for a search query.
    fn search_count(&mut self, q: &str) -> std::result::Result<u64, String> {
        let path = format!("/search/issues?q={}&per_page=1", urlenc(q));
        let resp = self.api_get(&path)?;
        Ok(resp.body.get("total_count").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    /// The full search result items for a query.
    fn search_items(&mut self, q: &str) -> std::result::Result<Vec<serde_json::Value>, String> {
        let path = format!("/search/issues?q={}&per_page=50", urlenc(q));
        let resp = self.api_get(&path)?;
        Ok(resp
            .body
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default())
    }

    fn fetch_repos(&mut self) -> std::result::Result<(), String> {
        let path = "/user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page=50";
        let resp = self.api_get(path)?;
        let arr = resp.body.as_array().cloned().unwrap_or_default();
        let mut repos = Vec::new();
        self.default_branch.clear();
        for r in &arr {
            let full = r.get("full_name").and_then(|v| v.as_str()).unwrap_or("");
            let (owner, name) = match full.split_once('/') {
                Some((o, n)) => (o.to_string(), n.to_string()),
                None => continue,
            };
            let default_branch = r.get("default_branch").and_then(|v| v.as_str()).unwrap_or("main").to_string();
            let pushed_at = r.get("pushed_at").and_then(|v| v.as_str()).and_then(parse_iso8601);
            let html_url = r.get("html_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            self.default_branch.insert(format!("{owner}/{name}"), default_branch.clone());
            repos.push(RepoInfo { owner, name, default_branch, pushed_at, html_url });
        }
        self.repos = repos;
        Ok(())
    }

    fn worktrees(&self) -> std::result::Result<Option<Vec<Worktree>>, String> {
        match run_git(&self.repo_root, &["worktree".into(), "list".into(), "--porcelain".into()]) {
            Ok(out) => Ok(Some(parse_worktrees(&out))),
            // Not a git repo (or git unavailable): the section shows a hint.
            Err(_) => Ok(None),
        }
    }

    fn record(&mut self, item: Item) -> Item {
        if !item.link.is_empty() {
            self.links.insert(item.id.clone(), item.link.clone());
        }
        item
    }

    /// Children of a top-level section or a repo/branch/tree node.
    fn expand_item(&mut self, item_id: &str) -> std::result::Result<Vec<Item>, String> {
        match item_id {
            "sec:review" => self.expand_review(),
            "sec:mine" => self.expand_mine(),
            "sec:issues" => self.expand_issues(),
            "sec:repos" => self.expand_repos(),
            "sec:worktrees" => self.expand_worktrees(),
            other if other.starts_with("repo|") => self.expand_repo(other),
            other if other.starts_with("branch|") => self.expand_branch(other),
            other if other.starts_with("tree|") => self.expand_tree(other),
            other => Err(format!("cannot expand: {other}")),
        }
    }

    fn expand_review(&mut self) -> std::result::Result<Vec<Item>, String> {
        let items = self.search_items("is:pr is:open review-requested:@me archived:false")?;
        Ok(items.iter().filter_map(|v| self.pr_item_from_search(v, None)).collect())
    }

    fn expand_issues(&mut self) -> std::result::Result<Vec<Item>, String> {
        let items = self.search_items("is:issue is:open assignee:@me archived:false")?;
        Ok(items.iter().filter_map(|v| self.issue_item_from_search(v)).collect())
    }

    fn expand_mine(&mut self) -> std::result::Result<Vec<Item>, String> {
        let token = match self.ensure_token()? {
            Some(t) => t,
            None => return Err(DISCONNECTED_DETAIL.to_string()),
        };
        let query = "query($q:String!){search(query:$q,type:ISSUE,first:50){nodes{... on PullRequest{number title url repository{nameWithOwner} author{login} commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}}}";
        let body = serde_json::json!({
            "query": query,
            "variables": { "q": "is:pr is:open author:@me archived:false" }
        })
        .to_string();
        let url = format!("{}/graphql", self.cfg.api_base);
        let resp = http::github_post_json(&self.agent, &url, &body, &token)?;
        let nodes = resp
            .body
            .pointer("/data/search/nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for n in &nodes {
            if let Some(item) = self.pr_item_from_graphql(n) {
                out.push(item);
            }
        }
        Ok(out)
    }

    fn expand_repos(&mut self) -> std::result::Result<Vec<Item>, String> {
        if self.repos.is_empty() {
            self.fetch_repos()?;
        }
        let repos = self.repos.clone();
        let now = now_unix() as i64;
        let mut out = Vec::new();
        for r in &repos {
            let rel = r.pushed_at.map(|p| relative_time(now - p as i64)).unwrap_or_else(|| "unknown".to_string());
            let title = format!("{}/{} \u{b7} {} \u{b7} pushed {}", r.owner, r.name, r.default_branch, rel);
            let item = Item {
                id: format!("repo|{}|{}", r.owner, r.name),
                title,
                text: String::new(),
                ts: String::new(),
                link: r.html_url.clone(),
                kind: Some("repo".into()),
                expandable: true,
                actions: vec![open_action()],
            };
            out.push(self.record(item));
        }
        Ok(out)
    }

    fn expand_worktrees(&mut self) -> std::result::Result<Vec<Item>, String> {
        let Some(worktrees) = self.worktrees()? else {
            return Ok(vec![Item {
                id: "wt:none".into(),
                title: "no local repository".into(),
                text: String::new(),
                ts: String::new(),
                link: String::new(),
                kind: Some("worktree".into()),
                expandable: false,
                actions: vec![],
            }]);
        };
        let mut out = Vec::new();
        for w in &worktrees {
            let branch = if w.detached {
                "detached".to_string()
            } else {
                w.branch.clone().unwrap_or_else(|| "detached".to_string())
            };
            let dirty = match run_git(Path::new(&w.path), &["status".into(), "--porcelain".into()]) {
                Ok(s) if s.trim().is_empty() => "clean",
                Ok(_) => "dirty",
                Err(_) => "unknown",
            };
            out.push(Item {
                id: format!("wt:{}", w.path),
                title: format!("{} \u{b7} {} \u{b7} {}", w.path, branch, dirty),
                text: String::new(),
                ts: String::new(),
                link: String::new(),
                kind: Some("worktree".into()),
                expandable: false,
                actions: vec![],
            });
        }
        Ok(out)
    }

    fn expand_repo(&mut self, id: &str) -> std::result::Result<Vec<Item>, String> {
        let parts: Vec<&str> = id.splitn(3, '|').collect();
        let (owner, name) = (parts.get(1).copied().unwrap_or(""), parts.get(2).copied().unwrap_or(""));
        if owner.is_empty() || name.is_empty() {
            return Err("bad repo id".into());
        }
        let default = self
            .default_branch
            .get(&format!("{owner}/{name}"))
            .cloned()
            .unwrap_or_else(|| "main".to_string());

        // Open PRs: collect head refs so we can mark branches with a PR.
        let pulls = self.api_get(&format!("/repos/{owner}/{name}/pulls?state=open&per_page=100"))?;
        let mut pr_branches = std::collections::HashSet::new();
        if let Some(arr) = pulls.body.as_array() {
            for p in arr {
                if let Some(r) = p.pointer("/head/ref").and_then(|v| v.as_str()) {
                    pr_branches.insert(r.to_string());
                }
            }
        }

        let resp = self.api_get(&format!("/repos/{owner}/{name}/branches?per_page=100"))?;
        let arr = resp.body.as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for b in arr.iter().take(MAX_ENTRIES) {
            let bname = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let sha = b.pointer("/commit/sha").and_then(|v| v.as_str()).unwrap_or("");
            if bname.is_empty() || sha.is_empty() {
                continue;
            }
            let mut marks = String::new();
            if bname == default {
                marks.push_str(" (default)");
            }
            if pr_branches.contains(bname) {
                marks.push_str(" (PR)");
            }
            let item = Item {
                id: format!("branch|{owner}|{name}|{sha}|{bname}"),
                title: format!("{bname}{marks}"),
                text: String::new(),
                ts: String::new(),
                link: format!("https://github.com/{owner}/{name}/tree/{bname}"),
                kind: Some("branch".into()),
                expandable: true,
                actions: branch_actions(),
            };
            out.push(self.record(item));
        }
        if arr.len() > MAX_ENTRIES {
            out.push(more_item(arr.len() - MAX_ENTRIES));
        }
        Ok(out)
    }

    fn expand_branch(&mut self, id: &str) -> std::result::Result<Vec<Item>, String> {
        // branch|owner|name|sha|branch(tail)
        let parts: Vec<&str> = id.splitn(5, '|').collect();
        let (owner, name, sha, bref) = (
            parts.get(1).copied().unwrap_or(""),
            parts.get(2).copied().unwrap_or(""),
            parts.get(3).copied().unwrap_or(""),
            parts.get(4).copied().unwrap_or(""),
        );
        let resp = self.api_get(&format!("/repos/{owner}/{name}/git/trees/{sha}"))?;
        Ok(self.build_tree_items(&resp.body, owner, name, bref, ""))
    }

    fn expand_tree(&mut self, id: &str) -> std::result::Result<Vec<Item>, String> {
        // tree|owner|name|sha|ref|path(tail)
        let parts: Vec<&str> = id.splitn(6, '|').collect();
        let (owner, name, sha, bref, path) = (
            parts.get(1).copied().unwrap_or(""),
            parts.get(2).copied().unwrap_or(""),
            parts.get(3).copied().unwrap_or(""),
            parts.get(4).copied().unwrap_or(""),
            parts.get(5).copied().unwrap_or(""),
        );
        let resp = self.api_get(&format!("/repos/{owner}/{name}/git/trees/{sha}"))?;
        Ok(self.build_tree_items(&resp.body, owner, name, bref, path))
    }

    fn build_tree_items(
        &mut self,
        body: &serde_json::Value,
        owner: &str,
        name: &str,
        bref: &str,
        parent: &str,
    ) -> Vec<Item> {
        let entries = body.get("tree").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for e in entries.iter().take(MAX_ENTRIES) {
            let ename = e.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let etype = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let esha = e.get("sha").and_then(|v| v.as_str()).unwrap_or("");
            if ename.is_empty() {
                continue;
            }
            let full = if parent.is_empty() { ename.to_string() } else { format!("{parent}/{ename}") };
            if etype == "tree" {
                let item = Item {
                    id: format!("tree|{owner}|{name}|{esha}|{bref}|{full}"),
                    title: format!("{ename}/"),
                    text: String::new(),
                    ts: String::new(),
                    link: format!("https://github.com/{owner}/{name}/tree/{bref}/{full}"),
                    kind: Some("dir".into()),
                    expandable: true,
                    actions: vec![open_action()],
                };
                out.push(self.record(item));
            } else if etype == "blob" {
                let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let item = Item {
                    id: format!("blob|{owner}|{name}|{esha}|{size}|{bref}|{full}"),
                    title: format!("{ename}  ({})", human_size(size)),
                    text: String::new(),
                    ts: String::new(),
                    link: format!("https://github.com/{owner}/{name}/blob/{bref}/{full}"),
                    kind: Some("file".into()),
                    expandable: false,
                    actions: vec![open_action()],
                };
                out.push(self.record(item));
            }
        }
        if entries.len() > MAX_ENTRIES {
            out.push(more_item(entries.len() - MAX_ENTRIES));
        }
        out
    }

    fn pr_item_from_search(&mut self, v: &serde_json::Value, glyph: Option<char>) -> Option<Item> {
        let number = v.get("number").and_then(|n| n.as_u64())?;
        let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let author = v.pointer("/user/login").and_then(|a| a.as_str()).unwrap_or("");
        let html_url = v.get("html_url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let repo_url = v.get("repository_url").and_then(|u| u.as_str()).unwrap_or("");
        let (owner, name) = owner_name_from_repo_url(repo_url).unwrap_or_else(|| ("".into(), "".into()));
        let head = match glyph {
            Some(g) => format!("{g} "),
            None => String::new(),
        };
        let item = Item {
            id: format!("pr:{owner}/{name}#{number}"),
            title: format!("{head}{name}#{number} \u{b7} {title} \u{b7} {author}"),
            text: String::new(),
            ts: String::new(),
            link: html_url,
            kind: Some("pr".into()),
            expandable: false,
            actions: vec![open_action()],
        };
        Some(self.record(item))
    }

    fn pr_item_from_graphql(&mut self, n: &serde_json::Value) -> Option<Item> {
        let number = n.get("number").and_then(|x| x.as_u64())?;
        let title = n.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let url = n.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let full = n.pointer("/repository/nameWithOwner").and_then(|r| r.as_str()).unwrap_or("");
        let (owner, name) = full.split_once('/').unwrap_or(("", full));
        let author = n.pointer("/author/login").and_then(|a| a.as_str()).unwrap_or("");
        let state = n
            .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
            .and_then(|s| s.as_str());
        let glyph = ci_glyph(state);
        let item = Item {
            id: format!("pr:{owner}/{name}#{number}"),
            title: format!("{glyph} {name}#{number} \u{b7} {title} \u{b7} {author}"),
            text: String::new(),
            ts: String::new(),
            link: url,
            kind: Some("pr".into()),
            expandable: false,
            actions: vec![open_action()],
        };
        Some(self.record(item))
    }

    fn issue_item_from_search(&mut self, v: &serde_json::Value) -> Option<Item> {
        let number = v.get("number").and_then(|n| n.as_u64())?;
        let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let html_url = v.get("html_url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let repo_url = v.get("repository_url").and_then(|u| u.as_str()).unwrap_or("");
        let (owner, name) = owner_name_from_repo_url(repo_url).unwrap_or_else(|| ("".into(), "".into()));
        let item = Item {
            id: format!("issue:{owner}/{name}#{number}"),
            title: format!("{name}#{number} \u{b7} {title}"),
            text: String::new(),
            ts: String::new(),
            link: html_url,
            kind: Some("issue".into()),
            expandable: false,
            actions: vec![open_action()],
        };
        Some(self.record(item))
    }

    /// Runs an action. Returns a success detail (e.g. the created worktree path).
    fn run_action(&mut self, item_id: &str, action_id: &str) -> std::result::Result<String, String> {
        match action_id {
            "open" => {
                let url = self.links.get(item_id).cloned().ok_or_else(|| format!("nothing to open for {item_id}"))?;
                self.opener.open(&url)?;
                Ok(format!("opened {url}"))
            }
            "worktree_add" => {
                let (owner, name, branch) = parse_branch_id(item_id).ok_or("worktree_add needs a branch")?;
                self.worktree_add(&owner, &name, &branch)
            }
            "checkout" => {
                let (owner, name, branch) = parse_branch_id(item_id).ok_or("checkout needs a branch")?;
                self.checkout(&owner, &name, &branch)
            }
            other => Err(format!("unknown action: {other}")),
        }
    }

    /// The local repo's origin identity, or an error when it is not a matching
    /// GitHub repo.
    fn require_matching_repo(&self, owner: &str, name: &str) -> std::result::Result<(), String> {
        let origin = run_git(&self.repo_root, &["remote".into(), "get-url".into(), "origin".into()])
            .map_err(|_| "no local repository with an origin remote".to_string())?;
        let id = normalize_remote(&origin).ok_or("origin remote is not a GitHub repository")?;
        if repo_matches(&id, owner, name) {
            Ok(())
        } else {
            Err(format!("origin remote {}/{} does not match {owner}/{name}", id.0, id.1))
        }
    }

    fn worktree_add(&self, owner: &str, name: &str, branch: &str) -> std::result::Result<String, String> {
        self.require_matching_repo(owner, name)?;
        let parent = self.repo_root.parent().ok_or("repository has no parent directory")?;
        let repo_name = self
            .repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let target = parent.join(format!("{repo_name}-{}", sanitize_branch(branch)));
        if target.exists() {
            return Err(format!("target directory already exists: {}", target.display()));
        }
        // Fetch the branch so a fresh checkout is possible. Best-effort: if the
        // remote is unreachable but a local branch already exists, the worktree
        // add below still succeeds; a truly missing ref fails there with a clear
        // git error.
        let _ = run_git(&self.repo_root, &["fetch".into(), "origin".into(), branch.into()]);
        let target_str = target.display().to_string();
        let local_exists = run_git(
            &self.repo_root,
            &["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}")],
        )
        .is_ok();
        if local_exists {
            run_git(&self.repo_root, &["worktree".into(), "add".into(), target_str.clone(), branch.into()])?;
        } else {
            run_git(
                &self.repo_root,
                &[
                    "worktree".into(),
                    "add".into(),
                    "-b".into(),
                    branch.into(),
                    target_str.clone(),
                    format!("origin/{branch}"),
                ],
            )?;
        }
        Ok(target_str)
    }

    fn checkout(&self, owner: &str, name: &str, branch: &str) -> std::result::Result<String, String> {
        self.require_matching_repo(owner, name)?;
        let status = run_git(&self.repo_root, &["status".into(), "--porcelain".into()])?;
        if !status.trim().is_empty() {
            return Err("working tree has changes — commit or stash first".to_string());
        }
        // Never --force; never discards data.
        run_git(&self.repo_root, &["checkout".into(), branch.into()])?;
        Ok(format!("checked out {branch}"))
    }
}

fn more_item(n: usize) -> Item {
    Item {
        id: "more".into(),
        title: format!("\u{2026} {n} more"),
        text: String::new(),
        ts: String::new(),
        link: String::new(),
        kind: Some("more".into()),
        expandable: false,
        actions: vec![],
    }
}

/// `branch|owner|name|sha|branch` → `(owner, name, branch)`.
fn parse_branch_id(id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = id.splitn(5, '|').collect();
    if parts.first() != Some(&"branch") || parts.len() < 5 {
        return None;
    }
    Some((parts[1].to_string(), parts[2].to_string(), parts[4].to_string()))
}

/// Human byte size (matches the tree renderer's compact units).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// Runs `git -C <root> <args>` with a timeout, capturing stderr into the error.
fn run_git(root: &Path, args: &[String]) -> std::result::Result<String, String> {
    let root = root.to_path_buf();
    let args = args.to_vec();
    let (tx, rx) = mpsc::channel();
    let joined = args.join(" ");
    thread::spawn(move || {
        let out = Command::new("git").arg("-C").arg(&root).args(&args).output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(GIT_TIMEOUT) {
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                Err(format!("git {joined}: {}", String::from_utf8_lossy(&out.stderr).trim()))
            }
        }
        Ok(Err(e)) => Err(format!("git {joined}: {e}")),
        Err(_) => Err(format!("git {joined}: timed out")),
    }
}

impl Connector for GithubConnector {
    fn id(&self) -> &str {
        "github"
    }
    fn name(&self) -> &str {
        "GitHub"
    }
    fn status(&self) -> Status {
        self.status
    }
    fn detail(&self) -> Option<String> {
        self.detail.clone()
    }
    fn interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<Vec<Item>> {
        self.links.clear();
        let token = match self.ensure_token() {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.status = Status::Disconnected;
                self.detail = Some(DISCONNECTED_DETAIL.to_string());
                return Ok(Vec::new());
            }
            Err(e) => {
                self.status = Status::Error;
                self.detail = Some(e.clone());
                return Err(e);
            }
        };
        let _ = token;

        let review = self.search_count("is:pr is:open review-requested:@me archived:false")?;
        let mine = self.search_count("is:pr is:open author:@me archived:false")?;
        let issues = self.search_count("is:issue is:open assignee:@me archived:false")?;
        self.fetch_repos()?;
        let repo_count = self.repos.len();
        let wt_count = self.worktrees()?.map(|w| w.len()).unwrap_or(0);

        self.status = Status::Connected;
        self.detail = Some("connected".to_string());

        let section = |id: &str, title: &str, count: u64| Item {
            id: id.to_string(),
            title: format!("{title} ({count})"),
            text: String::new(),
            ts: String::new(),
            link: String::new(),
            kind: Some("section".into()),
            expandable: true,
            actions: vec![],
        };
        Ok(vec![
            section("sec:review", "Review requested", review),
            section("sec:mine", "My PRs", mine),
            section("sec:issues", "Assigned issues", issues),
            section("sec:repos", "Repositories", repo_count as u64),
            section("sec:worktrees", "Worktrees (local)", wt_count as u64),
        ])
    }

    fn reply(&mut self, _item_id: &str, _text: &str) -> Result<()> {
        Err("GitHub items are not replyable".to_string())
    }

    fn expand(&mut self, item_id: &str) -> Result<Vec<Item>> {
        self.expand_item(item_id)
    }

    fn action(&mut self, item_id: &str, action_id: &str) -> Result<String> {
        self.run_action(item_id, action_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_glyph_maps_rollup_states() {
        assert_eq!(ci_glyph(Some("SUCCESS")), '\u{2713}');
        assert_eq!(ci_glyph(Some("failure")), '\u{2717}');
        assert_eq!(ci_glyph(Some("ERROR")), '\u{2717}');
        assert_eq!(ci_glyph(Some("PENDING")), '\u{25cf}');
        assert_eq!(ci_glyph(None), '\u{2013}');
        assert_eq!(ci_glyph(Some("weird")), '\u{2013}');
    }

    #[test]
    fn parses_worktrees_including_detached_and_skips_bare() {
        let porcelain = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /repo/wt1\nHEAD def\ndetached\n\nworktree /bare\nbare\n";
        let wts = parse_worktrees(porcelain);
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0], Worktree { path: "/repo".into(), branch: Some("main".into()), detached: false });
        assert_eq!(wts[1], Worktree { path: "/repo/wt1".into(), branch: None, detached: true });
    }

    #[test]
    fn normalizes_https_and_ssh_remotes() {
        let want = Some(("octo".to_string(), "repo".to_string()));
        assert_eq!(normalize_remote("https://github.com/octo/repo.git"), want);
        assert_eq!(normalize_remote("https://github.com/octo/repo"), want);
        assert_eq!(normalize_remote("git@github.com:octo/repo.git"), want);
        assert_eq!(normalize_remote("ssh://git@github.com/octo/repo.git"), want);
        assert_eq!(normalize_remote("https://gitlab.com/octo/repo.git"), None);
    }

    #[test]
    fn repo_matches_is_owner_case_insensitive() {
        let id = ("Octo".to_string(), "repo".to_string());
        assert!(repo_matches(&id, "octo", "repo"));
        assert!(!repo_matches(&id, "octo", "Repo"));
    }

    #[test]
    fn sanitizes_branch_names_for_worktree_dirs() {
        assert_eq!(sanitize_branch("feat/github"), "feat-github");
        assert_eq!(sanitize_branch("release_1.2"), "release_1.2");
        assert_eq!(sanitize_branch("a b*c"), "a-b-c");
    }

    #[test]
    fn rate_limit_wait_reads_retry_after_then_reset() {
        let r = GithubResponse { status: 403, retry_after: Some("5".into()), rate_remaining: Some(0), rate_reset: Some(100), body: serde_json::Value::Null };
        assert_eq!(rate_limit_wait(&r, 0), Some(5));
        let r = GithubResponse { status: 429, retry_after: None, rate_remaining: Some(0), rate_reset: Some(30), body: serde_json::Value::Null };
        assert_eq!(rate_limit_wait(&r, 10), Some(20));
        let ok = GithubResponse { status: 200, retry_after: None, rate_remaining: Some(10), rate_reset: None, body: serde_json::Value::Null };
        assert_eq!(rate_limit_wait(&ok, 0), None);
    }

    #[test]
    fn parses_iso8601_and_formats_relative_time() {
        let t = parse_iso8601("1970-01-02T00:00:00Z").unwrap();
        assert_eq!(t, 86400);
        assert_eq!(relative_time(30), "just now");
        assert_eq!(relative_time(120), "2m ago");
        assert_eq!(relative_time(7200), "2h ago");
        assert_eq!(relative_time(172800), "2d ago");
    }

    #[test]
    fn parse_branch_id_extracts_owner_name_branch() {
        assert_eq!(
            parse_branch_id("branch|octo|repo|abc123|feat/x"),
            Some(("octo".into(), "repo".into(), "feat/x".into()))
        );
        assert_eq!(parse_branch_id("repo|octo|repo"), None);
    }
}
