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
use crate::protocol::{
    FieldOption, IssueFieldEdit, IssueSchema, Item, ItemAction, MilestoneOption, NewIssue, ProjectFieldEdit,
    ProjectFieldSchema, ProjectSchema, Status,
};
use serde_json::{json, Value};

/// Poll every 60 seconds (S15).
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Cap on tree entries shown per level, with a trailing "… N more".
pub const MAX_ENTRIES: usize = 500;
/// Pages of branches to follow via the `Link` header before marking the rest
/// truncated (100 per page = up to 500 entries, S16).
pub const MAX_BRANCH_PAGES: usize = 5;
/// Latest workflow runs fetched per poll (S16).
pub const MAX_WORKFLOW_RUNS: usize = 20;
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

/// The in-terminal reader action offered first on issue/PR items (S27). The
/// engine turns a `view` action into a `Detail` message rather than running a
/// side effect, so it never needs confirmation.
fn view_action() -> ItemAction {
    ItemAction { id: "view".into(), label: "View in terminal".into(), confirm: false }
}

/// Cap on the rendered reader body (~200 KB); longer content is truncated with
/// a marker so the overlay stays bounded and a huge issue never floods the
/// protocol (S27).
pub const MAX_DETAIL_BYTES: usize = 200 * 1024;
/// At most this many comments are rendered; the rest are summarised (S27).
pub const MAX_DETAIL_COMMENTS: usize = 50;

/// `issue:owner/name#number` or `pr:owner/name#number` → `(is_pr, owner, name,
/// number)`. Returns `None` for any other id.
fn parse_issue_or_pr_id(id: &str) -> Option<(bool, String, String, u64)> {
    let (is_pr, rest) = if let Some(r) = id.strip_prefix("issue:") {
        (false, r)
    } else if let Some(r) = id.strip_prefix("pr:") {
        (true, r)
    } else {
        return None;
    };
    let (repo, num) = rest.split_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let number: u64 = num.parse().ok()?;
    Some((is_pr, owner.to_string(), name.to_string(), number))
}

/// Truncates `s` to at most `max` bytes on a char boundary (no panic on
/// multi-byte text).
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Builds the in-terminal reader content for an issue or PR from its API JSON
/// and its comments (S27). Pure: no network, so it is unit-tested with fixtures.
/// Returns `(window_title, body)` where `body` is plain markdown text (header
/// line, title, URL, description, then each comment's author + body), bounded
/// to [`MAX_DETAIL_BYTES`] and [`MAX_DETAIL_COMMENTS`].
pub fn build_detail(
    repo: &str,
    number: u64,
    is_pr: bool,
    obj: &serde_json::Value,
    comments: &[serde_json::Value],
) -> (String, String) {
    let kind = if is_pr { "PR" } else { "issue" };
    let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("(no title)");
    let state = obj.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let author = obj.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("");
    let url = obj.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
    let body = obj.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let labels: Vec<String> = obj
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.get("name").and_then(|n| n.as_str()).map(String::from).or_else(|| l.as_str().map(String::from))
                })
                .collect()
        })
        .unwrap_or_default();

    let window_title = format!("{repo}#{number} \u{b7} {title}");

    let mut out = String::new();
    let mut header = format!("{repo}#{number} \u{b7} {kind} \u{b7} {state} \u{b7} @{author}");
    if !labels.is_empty() {
        header.push_str(&format!(" \u{b7} {}", labels.join(", ")));
    }
    out.push_str(&header);
    out.push_str("\n\n");
    out.push_str(title);
    out.push_str("\n\n");
    out.push_str(url);
    out.push_str("\n\n");
    if body.trim().is_empty() {
        out.push_str("(no description)");
    } else {
        out.push_str(body);
    }

    out.push_str(&format!("\n\n\u{2500}\u{2500} Comments ({}) \u{2500}\u{2500}", comments.len()));
    let shown = comments.len().min(MAX_DETAIL_COMMENTS);
    for c in comments.iter().take(MAX_DETAIL_COMMENTS) {
        let ca = c.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("");
        let cb = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("\n\n@{ca}:\n{cb}"));
    }
    if comments.len() > shown {
        out.push_str(&format!("\n\n\u{2026} {} more comments", comments.len() - shown));
    }

    // Defensive layer 1 (T35, S31, F1): the title, body and comments are
    // attacker-controllable, so strip terminal control/escape bytes and bidi
    // overrides before the text enters the protocol. Multi-line body keeps
    // newlines; the window title is a single line.
    let mut out = crate::sanitize::sanitize_display(&out);
    let window_title = crate::sanitize::sanitize_line(&window_title);

    if out.len() > MAX_DETAIL_BYTES {
        truncate_on_char_boundary(&mut out, MAX_DETAIL_BYTES);
        out.push_str("\n\n\u{2026} (truncated)");
    }

    (window_title, out)
}

/// The hint shown when the `gh` token lacks the Projects (v2) scope (S30). The
/// user grants it with this command; project fields never crash the editor.
pub const PROJECT_SCOPE_HINT: &str = "Project fields need: gh auth refresh -s project";

/// The edit action offered on an issue item (S30); the UI turns it into the
/// editor overlay rather than a side effect.
fn edit_action() -> ItemAction {
    ItemAction { id: "edit".into(), label: "Edit issue".into(), confirm: false }
}

/// Builds the PATCH body for an issue from a form diff (S30): only the changed
/// issue fields (title/body/state/assignees/labels/milestone) are included.
/// `comment` and project fields are applied separately. Returns `None` when no
/// PATCH-able field changed, so no request is sent.
pub fn issue_edit_payload(edit: &IssueFieldEdit) -> Option<String> {
    let mut obj = serde_json::Map::new();
    if let Some(t) = &edit.title {
        obj.insert("title".into(), json!(t));
    }
    if let Some(b) = &edit.body {
        obj.insert("body".into(), json!(b));
    }
    if let Some(s) = &edit.state {
        obj.insert("state".into(), json!(s));
    }
    if let Some(a) = &edit.assignees {
        obj.insert("assignees".into(), json!(a));
    }
    if let Some(l) = &edit.labels {
        obj.insert("labels".into(), json!(l));
    }
    match edit.milestone {
        // Some(0) clears the milestone (null); Some(n) sets it; None leaves it.
        Some(0) => {
            obj.insert("milestone".into(), Value::Null);
        }
        Some(n) => {
            obj.insert("milestone".into(), json!(n));
        }
        None => {}
    }
    if obj.is_empty() {
        None
    } else {
        Some(Value::Object(obj).to_string())
    }
}

/// Builds the POST body that creates a new issue (S30).
pub fn create_issue_payload(issue: &NewIssue) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("title".into(), json!(issue.title));
    if !issue.body.is_empty() {
        obj.insert("body".into(), json!(issue.body));
    }
    if !issue.assignees.is_empty() {
        obj.insert("assignees".into(), json!(issue.assignees));
    }
    if !issue.labels.is_empty() {
        obj.insert("labels".into(), json!(issue.labels));
    }
    if let Some(n) = issue.milestone {
        obj.insert("milestone".into(), json!(n));
    }
    Value::Object(obj).to_string()
}

/// Builds the GraphQL mutation body (query + variables) that sets one Project
/// (v2) field value, or clears it when `value` is empty (S30).
pub fn project_field_mutation(edit: &ProjectFieldEdit) -> String {
    if edit.value.is_empty() {
        let q = "mutation($p:ID!,$i:ID!,$f:ID!){clearProjectV2ItemFieldValue(input:{projectId:$p,itemId:$i,fieldId:$f}){projectV2Item{id}}}";
        return json!({"query": q, "variables": {"p": edit.project_id, "i": edit.item_id, "f": edit.field_id}}).to_string();
    }
    let value = match edit.kind.as_str() {
        "number" => json!({ "number": edit.value.parse::<f64>().unwrap_or(0.0) }),
        "single_select" => json!({ "singleSelectOptionId": edit.value }),
        "iteration" => json!({ "iterationId": edit.value }),
        "date" => json!({ "date": edit.value }),
        _ => json!({ "text": edit.value }),
    };
    let q = "mutation($p:ID!,$i:ID!,$f:ID!,$v:ProjectV2FieldValue!){updateProjectV2ItemFieldValue(input:{projectId:$p,itemId:$i,fieldId:$f,value:$v}){projectV2Item{id}}}";
    json!({"query": q, "variables": {"p": edit.project_id, "i": edit.item_id, "f": edit.field_id, "v": value}}).to_string()
}

/// Lowercase alphanumeric slug with single dashes, capped to 40 chars (S30).
fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_ascii_lowercase().to_string().chars());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 40 {
        out.truncate(40);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Derives the local branch name for an issue (S30): `<n>-<slug-of-title>`,
/// passed through [`sanitize_branch`] so it is always filesystem/ref safe.
pub fn issue_branch_name(number: u64, title: &str) -> String {
    let slug = slugify(title);
    let name = if slug.is_empty() { number.to_string() } else { format!("{number}-{slug}") };
    sanitize_branch(&name)
}

/// Formats a Project number field value without a trailing `.0`.
fn trim_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// True when a GraphQL response's `errors` indicate the token lacks the
/// Projects scope (S30), so the editor shows [`PROJECT_SCOPE_HINT`] instead of
/// crashing.
pub fn project_scope_missing(body: &serde_json::Value) -> bool {
    let Some(errors) = body.get("errors").and_then(|e| e.as_array()) else {
        return false;
    };
    errors.iter().any(|e| {
        let ty = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
        ty.eq_ignore_ascii_case("INSUFFICIENT_SCOPES")
            || ty.eq_ignore_ascii_case("FORBIDDEN")
            || msg.to_ascii_lowercase().contains("scope")
    })
}

/// Parses the `projectItems` nodes of an issue's GraphQL response into editable
/// [`ProjectSchema`]s (S30). Pure: unit-tested with fixtures.
pub fn parse_project_items(body: &serde_json::Value) -> Vec<ProjectSchema> {
    let Some(nodes) = body.pointer("/data/repository/issue/projectItems/nodes").and_then(|n| n.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pi in nodes {
        let item_id = pi.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let project_id = pi.pointer("/project/id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = pi.pointer("/project/title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Current value per field id, read from the item's fieldValues.
        let mut current: HashMap<String, String> = HashMap::new();
        if let Some(vals) = pi.pointer("/fieldValues/nodes").and_then(|v| v.as_array()) {
            for v in vals {
                let Some(fid) = v.pointer("/field/id").and_then(|x| x.as_str()) else { continue };
                let display = v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .or_else(|| v.get("number").and_then(|x| x.as_f64()).map(trim_number))
                    .or_else(|| v.get("title").and_then(|x| x.as_str()).map(String::from))
                    .or_else(|| v.get("text").and_then(|x| x.as_str()).map(String::from));
                if let Some(d) = display {
                    current.insert(fid.to_string(), d);
                }
            }
        }
        // Field definitions (+ options / iterations) from the project.
        let mut fields = Vec::new();
        if let Some(defs) = pi.pointer("/project/fields/nodes").and_then(|v| v.as_array()) {
            for f in defs {
                let field_id = f.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if field_id.is_empty() {
                    continue;
                }
                let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let kind = match f.get("dataType").and_then(|v| v.as_str()).unwrap_or("") {
                    "SINGLE_SELECT" => "single_select",
                    "NUMBER" => "number",
                    "ITERATION" => "iteration",
                    "DATE" => "date",
                    "TEXT" | "TITLE" => "text",
                    _ => continue, // assignees, labels, etc. are edited elsewhere
                };
                let mut options = Vec::new();
                if let Some(opts) = f.get("options").and_then(|v| v.as_array()) {
                    for o in opts {
                        options.push(FieldOption {
                            id: o.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            name: o.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        });
                    }
                }
                if let Some(iters) = f.pointer("/configuration/iterations").and_then(|v| v.as_array()) {
                    for it in iters {
                        options.push(FieldOption {
                            id: it.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            name: it.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        });
                    }
                }
                fields.push(ProjectFieldSchema {
                    field_id: field_id.to_string(),
                    name,
                    kind: kind.to_string(),
                    current: current.get(field_id).cloned(),
                    options,
                });
            }
        }
        out.push(ProjectSchema { project_id, item_id, title, fields });
    }
    out
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

    /// POST a JSON body to a REST path (GraphQL and the rerun endpoint),
    /// re-reading the token once on a 401 exactly like [`Self::api_get`].
    fn api_post(&mut self, path: &str, body: &str) -> std::result::Result<GithubResponse, String> {
        let token = match self.ensure_token()? {
            Some(t) => t,
            None => return Err(DISCONNECTED_DETAIL.to_string()),
        };
        let url = format!("{}{}", self.cfg.api_base, path);
        let resp = http::github_post_json(&self.agent, &url, body, &token)?;
        if resp.status == 401 {
            self.token = self.token_source.token()?;
            if let Some(t2) = self.token.clone() {
                return http::github_post_json(&self.agent, &url, body, &t2);
            }
        }
        Ok(resp)
    }

    /// PATCH a JSON body to a REST path, re-reading the token once on a 401.
    fn api_patch(&mut self, path: &str, body: &str) -> std::result::Result<GithubResponse, String> {
        let token = match self.ensure_token()? {
            Some(t) => t,
            None => return Err(DISCONNECTED_DETAIL.to_string()),
        };
        let url = format!("{}{}", self.cfg.api_base, path);
        let resp = http::github_patch_json(&self.agent, &url, body, &token)?;
        if resp.status == 401 {
            self.token = self.token_source.token()?;
            if let Some(t2) = self.token.clone() {
                return http::github_patch_json(&self.agent, &url, body, &t2);
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

    fn record(&mut self, mut item: Item) -> Item {
        // Defensive layer 1 (T35, S31, F1): item titles and previews embed
        // attacker-controllable GitHub strings (titles, authors, branch/ref and
        // worktree names); strip terminal control/escape bytes and bidi
        // overrides before they enter the protocol.
        item.title = crate::sanitize::sanitize_line(&item.title);
        if !item.text.is_empty() {
            item.text = crate::sanitize::sanitize_display(&item.text);
        }
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
            "sec:workflows" => self.expand_workflows(),
            other if other.starts_with("repo|") => self.expand_repo(other),
            other if other.starts_with("branch|") => self.expand_branch(other),
            other if other.starts_with("tree|") => self.expand_tree(other),
            other if other.starts_with("run|") => self.expand_run(other),
            other => Err(format!("cannot expand: {other}")),
        }
    }

    fn expand_review(&mut self) -> std::result::Result<Vec<Item>, String> {
        let items = self.search_items("is:pr is:open review-requested:@me archived:false")?;
        Ok(items.iter().filter_map(|v| self.pr_item_from_search(v, None)).collect())
    }

    fn expand_issues(&mut self) -> std::result::Result<Vec<Item>, String> {
        let items = self.search_items("is:issue is:open assignee:@me archived:false")?;
        let mut out: Vec<Item> = items.iter().filter_map(|v| self.issue_item_from_search(v)).collect();
        // A "\u{ff0b} New issue" row (S30): opens the empty editor with a repo
        // picker defaulting to the local GitHub remote when there is one.
        let default_repo = self.local_repo().map(|(o, n)| format!("{o}/{n}")).unwrap_or_default();
        out.insert(
            0,
            self.record(Item {
                id: format!("issue:new|{default_repo}"),
                title: "\u{ff0b} New issue".into(),
                text: String::new(),
                ts: String::new(),
                link: String::new(),
                kind: Some("newissue".into()),
                expandable: false,
                actions: vec![ItemAction { id: "new".into(), label: "New issue".into(), confirm: false }],
            }),
        );
        Ok(out)
    }

    fn expand_mine(&mut self) -> std::result::Result<Vec<Item>, String> {
        let query = "query($q:String!){search(query:$q,type:ISSUE,first:50){nodes{... on PullRequest{number title url repository{nameWithOwner} author{login} commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}}}";
        let body = serde_json::json!({
            "query": query,
            "variables": { "q": "is:pr is:open author:@me archived:false" }
        })
        .to_string();
        // GraphQL re-reads the token once on a 401, like api_get.
        let resp = self.api_post("/graphql", &body)?;
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
            let item = Item {
                id: format!("wt:{}", w.path),
                title: format!("{} \u{b7} {} \u{b7} {}", w.path, branch, dirty),
                text: String::new(),
                ts: String::new(),
                link: String::new(),
                kind: Some("worktree".into()),
                expandable: false,
                actions: vec![],
            };
            out.push(self.record(item));
        }
        Ok(out)
    }

    /// The local repo's `(owner, name)` from its `origin` remote, when the
    /// engine's cwd is a GitHub repository.
    fn local_repo(&self) -> Option<(String, String)> {
        let origin = run_git(&self.repo_root, &["remote".into(), "get-url".into(), "origin".into()]).ok()?;
        normalize_remote(&origin)
    }

    /// The latest workflow runs of the local GitHub repo (S16). Outside a local
    /// GitHub repo it shows a single hint row. One HTTP request.
    fn expand_workflows(&mut self) -> std::result::Result<Vec<Item>, String> {
        match self.local_repo() {
            Some((owner, name)) => self.fetch_workflow_runs(&owner, &name),
            None => Ok(vec![no_local_github_item()]),
        }
    }

    /// `GET /repos/{o}/{r}/actions/runs?per_page=20` → the latest runs as items.
    fn fetch_workflow_runs(&mut self, owner: &str, name: &str) -> std::result::Result<Vec<Item>, String> {
        let resp = self.api_get(&format!("/repos/{owner}/{name}/actions/runs?per_page={MAX_WORKFLOW_RUNS}"))?;
        let runs = resp.body.get("workflow_runs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let now = now_unix() as i64;
        let mut out = Vec::new();
        for r in &runs {
            let wf = r.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("workflow");
            let branch = r.get("head_branch").and_then(|v| v.as_str()).unwrap_or("");
            let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let conclusion = r.get("conclusion").and_then(|v| v.as_str());
            let run_id = r.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let html_url = r.get("html_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let created = r.get("run_started_at").or_else(|| r.get("created_at")).and_then(|v| v.as_str()).and_then(parse_iso8601);
            let rel = created.map(|c| relative_time(now - c as i64)).unwrap_or_else(|| "unknown".to_string());
            let glyph = run_glyph(status, conclusion);
            let mut actions = vec![open_action()];
            if conclusion == Some("failure") {
                actions.push(rerun_failed_action());
            }
            let item = Item {
                id: format!("run|{owner}|{name}|{run_id}"),
                title: format!("{wf} \u{b7} {branch} \u{b7} {glyph} \u{b7} {rel}"),
                text: String::new(),
                ts: String::new(),
                link: html_url,
                kind: Some("run".into()),
                expandable: true,
                actions,
            };
            out.push(self.record(item));
        }
        Ok(out)
    }

    /// Jobs of a workflow run (`GET /…/actions/runs/{id}/jobs`), each with its
    /// conclusion glyph.
    fn expand_run(&mut self, id: &str) -> std::result::Result<Vec<Item>, String> {
        // run|owner|name|runid
        let parts: Vec<&str> = id.splitn(4, '|').collect();
        let (owner, name, run_id) = (
            parts.get(1).copied().unwrap_or(""),
            parts.get(2).copied().unwrap_or(""),
            parts.get(3).copied().unwrap_or(""),
        );
        let resp = self.api_get(&format!("/repos/{owner}/{name}/actions/runs/{run_id}/jobs"))?;
        let jobs = resp.body.get("jobs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for j in &jobs {
            let jname = j.get("name").and_then(|v| v.as_str()).unwrap_or("job");
            let status = j.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let conclusion = j.get("conclusion").and_then(|v| v.as_str());
            let job_id = j.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let html_url = j.get("html_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let glyph = run_glyph(status, conclusion);
            let item = Item {
                id: format!("job|{owner}|{name}|{job_id}"),
                title: format!("{glyph} {jname}"),
                text: String::new(),
                ts: String::new(),
                link: html_url,
                kind: Some("job".into()),
                expandable: false,
                actions: vec![open_action()],
            };
            out.push(self.record(item));
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

        let (arr, truncated) = self.fetch_branches(owner, name)?;
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
        if truncated {
            out.push(more_pages_item());
        }
        Ok(out)
    }

    /// Fetches a repo's branches a page at a time (`?per_page=100&page=N`) up to
    /// [`MAX_BRANCH_PAGES`] pages / [`MAX_ENTRIES`] entries. Returns the entries
    /// and whether more remain beyond the cap (for a `… more` marker, S16). A
    /// short final page (< 100) ends the walk; a full last page at the cap is
    /// treated as possibly having more.
    fn fetch_branches(&mut self, owner: &str, name: &str) -> std::result::Result<(Vec<serde_json::Value>, bool), String> {
        const PER_PAGE: usize = 100;
        let mut all: Vec<serde_json::Value> = Vec::new();
        let mut truncated = false;
        for page in 1..=MAX_BRANCH_PAGES {
            let resp = self.api_get(&format!("/repos/{owner}/{name}/branches?per_page={PER_PAGE}&page={page}"))?;
            let arr = resp.body.as_array().cloned().unwrap_or_default();
            let n = arr.len();
            all.extend(arr);
            if n < PER_PAGE {
                break; // short page: this was the last one
            }
            if page == MAX_BRANCH_PAGES || all.len() >= MAX_ENTRIES {
                // A full page at the cap: there may be more beyond it.
                truncated = true;
                break;
            }
        }
        if all.len() > MAX_ENTRIES {
            all.truncate(MAX_ENTRIES);
            truncated = true;
        }
        Ok((all, truncated))
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
            actions: vec![view_action(), open_action()],
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
            actions: vec![view_action(), open_action()],
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
            actions: vec![view_action(), edit_action(), open_action()],
        };
        Some(self.record(item))
    }

    /// Fetches an issue/PR and its comments off the UI thread and renders the
    /// in-terminal reader content (S27). Comments use the issues endpoint,
    /// which serves both issues and PRs. The token is never logged.
    fn view_item(&mut self, item_id: &str) -> std::result::Result<(String, String), String> {
        let (is_pr, owner, name, number) =
            parse_issue_or_pr_id(item_id).ok_or_else(|| format!("not a viewable item: {item_id}"))?;
        let repo = format!("{owner}/{name}");
        let path = if is_pr {
            format!("/repos/{owner}/{name}/pulls/{number}")
        } else {
            format!("/repos/{owner}/{name}/issues/{number}")
        };
        let obj = self.api_get(&path)?.body;
        let comments = self
            .api_get(&format!("/repos/{owner}/{name}/issues/{number}/comments?per_page=100"))?
            .body
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(build_detail(&repo, number, is_pr, &obj, &comments))
    }

    /// Fetches the editable schema of an existing issue (S30): current values
    /// plus the repo's labels, assignable users, milestones and the issue's
    /// Project (v2) fields. All off the UI thread; project errors downgrade to
    /// a note (never crash).
    fn issue_form(&mut self, item_id: &str) -> std::result::Result<IssueSchema, String> {
        let (is_pr, owner, name, number) =
            parse_issue_or_pr_id(item_id).ok_or_else(|| format!("not an issue: {item_id}"))?;
        if is_pr {
            return Err("only issues are editable".to_string());
        }
        let obj = self.api_get(&format!("/repos/{owner}/{name}/issues/{number}"))?.body;
        let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let body = obj.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = obj.get("state").and_then(|v| v.as_str()).unwrap_or("open").to_string();
        let assignees = obj
            .get("assignees")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|u| u.get("login").and_then(|l| l.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        let labels = obj
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        let milestone = obj.pointer("/milestone/number").and_then(|v| v.as_u64());
        let (assignable_users, repo_labels, milestones) = self.fetch_issue_options(&owner, &name)?;
        let (projects, note) = self.fetch_project_items(&owner, &name, number);
        Ok(IssueSchema {
            repo: format!("{owner}/{name}"),
            number: Some(number),
            title,
            body,
            state,
            assignees,
            labels,
            milestone,
            assignable_users,
            repo_labels,
            milestones,
            projects,
            note,
        })
    }

    /// Fetches the blank editable schema (options only) for a new issue in
    /// `repo` (`owner/name`), S30.
    fn new_issue_form(&mut self, repo: &str) -> std::result::Result<IssueSchema, String> {
        let (owner, name) = repo.split_once('/').ok_or_else(|| format!("not owner/name: {repo}"))?;
        let (assignable_users, repo_labels, milestones) = self.fetch_issue_options(owner, name)?;
        Ok(IssueSchema {
            repo: repo.to_string(),
            number: None,
            state: "open".into(),
            assignable_users,
            repo_labels,
            milestones,
            ..Default::default()
        })
    }

    /// Repo labels, assignable users and open milestones for the editor options.
    fn fetch_issue_options(
        &mut self,
        owner: &str,
        name: &str,
    ) -> std::result::Result<(Vec<String>, Vec<String>, Vec<MilestoneOption>), String> {
        let labels = self
            .api_get(&format!("/repos/{owner}/{name}/labels?per_page=100"))?
            .body
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        let users = self
            .api_get(&format!("/repos/{owner}/{name}/assignees?per_page=100"))?
            .body
            .as_array()
            .map(|a| a.iter().filter_map(|u| u.get("login").and_then(|l| l.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        let milestones = self
            .api_get(&format!("/repos/{owner}/{name}/milestones?state=open&per_page=100"))?
            .body
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some(MilestoneOption {
                            number: m.get("number").and_then(|n| n.as_u64())?,
                            title: m.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((users, labels, milestones))
    }

    /// The issue's Project (v2) items and their fields via GraphQL (S30).
    /// Returns `(projects, note)`; a missing Projects scope yields no projects
    /// and the [`PROJECT_SCOPE_HINT`] note instead of an error.
    fn fetch_project_items(&mut self, owner: &str, name: &str, number: u64) -> (Vec<ProjectSchema>, Option<String>) {
        let query = "query($o:String!,$n:String!,$num:Int!){repository(owner:$o,name:$n){issue(number:$num){projectItems(first:20){nodes{id project{id title fields(first:50){nodes{... on ProjectV2FieldCommon{id name dataType} ... on ProjectV2SingleSelectField{options{id name}} ... on ProjectV2IterationField{configuration{iterations{id title}}}}}} fieldValues(first:50){nodes{... on ProjectV2ItemFieldSingleSelectValue{name field{... on ProjectV2FieldCommon{id}}} ... on ProjectV2ItemFieldNumberValue{number field{... on ProjectV2FieldCommon{id}}} ... on ProjectV2ItemFieldIterationValue{title field{... on ProjectV2FieldCommon{id}}} ... on ProjectV2ItemFieldTextValue{text field{... on ProjectV2FieldCommon{id}}}}}}}}}}";
        let body = json!({ "query": query, "variables": { "o": owner, "n": name, "num": number } }).to_string();
        match self.api_post("/graphql", &body) {
            Ok(resp) => {
                if project_scope_missing(&resp.body) {
                    (Vec::new(), Some(PROJECT_SCOPE_HINT.to_string()))
                } else {
                    (parse_project_items(&resp.body), None)
                }
            }
            // Never crash the editor on a project fetch failure.
            Err(_) => (Vec::new(), Some(PROJECT_SCOPE_HINT.to_string())),
        }
    }

    /// Applies a form diff to an existing issue (S30): the PATCH (only changed
    /// fields), then any project-field updates, then an optional comment. The
    /// token is never logged.
    fn issue_edit(&mut self, item_id: &str, edit: &IssueFieldEdit) -> std::result::Result<String, String> {
        let (is_pr, owner, name, number) =
            parse_issue_or_pr_id(item_id).ok_or_else(|| format!("not an issue: {item_id}"))?;
        if is_pr {
            return Err("only issues are editable".to_string());
        }
        let mut applied: Vec<String> = Vec::new();
        if let Some(payload) = issue_edit_payload(edit) {
            let resp = self.api_patch(&format!("/repos/{owner}/{name}/issues/{number}"), &payload)?;
            if !(200..300).contains(&resp.status) {
                let msg = resp.body.get("message").and_then(|v| v.as_str()).unwrap_or("edit failed");
                return Err(format!("edit failed ({}): {msg}", resp.status));
            }
            applied.push("fields".into());
        }
        for pf in &edit.project_fields {
            let resp = self.api_post("/graphql", &project_field_mutation(pf))?;
            if project_scope_missing(&resp.body) {
                return Err(PROJECT_SCOPE_HINT.to_string());
            }
            if let Some(err) = resp.body.pointer("/errors/0/message").and_then(|v| v.as_str()) {
                return Err(format!("project field failed: {err}"));
            }
            applied.push("project field".into());
        }
        if let Some(comment) = &edit.comment {
            if !comment.trim().is_empty() {
                let payload = json!({ "body": comment }).to_string();
                let resp = self.api_post(&format!("/repos/{owner}/{name}/issues/{number}/comments"), &payload)?;
                if !(200..300).contains(&resp.status) {
                    let msg = resp.body.get("message").and_then(|v| v.as_str()).unwrap_or("comment failed");
                    return Err(format!("comment failed ({}): {msg}", resp.status));
                }
                applied.push("comment".into());
            }
        }
        if applied.is_empty() {
            Ok("no changes".to_string())
        } else {
            Ok(format!("updated {name}#{number}: {}", applied.join(", ")))
        }
    }

    /// Creates a new issue in `repo` (`owner/name`), S30.
    fn issue_create(&mut self, repo: &str, issue: &NewIssue) -> std::result::Result<String, String> {
        let (owner, name) = repo.split_once('/').ok_or_else(|| format!("not owner/name: {repo}"))?;
        if issue.title.trim().is_empty() {
            return Err("a new issue needs a title".to_string());
        }
        let payload = create_issue_payload(issue);
        let resp = self.api_post(&format!("/repos/{owner}/{name}/issues"), &payload)?;
        if !(200..300).contains(&resp.status) {
            let msg = resp.body.get("message").and_then(|v| v.as_str()).unwrap_or("create failed");
            return Err(format!("create failed ({}): {msg}", resp.status));
        }
        let num = resp.body.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(format!("created {name}#{num}"))
    }

    /// Creates or switches to the issue's local branch in the partitioner root
    /// (S30). Switching refuses on a dirty tree (like checkout); never --force.
    fn issue_branch(&mut self, item_id: &str) -> std::result::Result<String, String> {
        let (is_pr, owner, name, number) =
            parse_issue_or_pr_id(item_id).ok_or_else(|| format!("not an issue: {item_id}"))?;
        if is_pr {
            return Err("issue branches are only for issues".to_string());
        }
        let title = self
            .api_get(&format!("/repos/{owner}/{name}/issues/{number}"))?
            .body
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let branch = issue_branch_name(number, &title);
        validate_branch(&self.repo_root, &branch)?;
        let exists = run_git(
            &self.repo_root,
            &["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}")],
        )
        .is_ok();
        if exists {
            let status = run_git(&self.repo_root, &["status".into(), "--porcelain".into()])?;
            if !status.trim().is_empty() {
                return Err("working tree has changes \u{2014} commit or stash first".to_string());
            }
            run_git(&self.repo_root, &["checkout".into(), branch.clone(), "--".into()])?;
            Ok(format!("switched to {branch}"))
        } else {
            // Create at HEAD, carrying any uncommitted work (never --force).
            run_git(&self.repo_root, &["checkout".into(), "-b".into(), branch.clone(), "--".into()])?;
            Ok(format!("created {branch}"))
        }
    }

    /// Commits ONLY the already-staged changes with `message` (S30). Refuses
    /// when nothing is staged; never stages automatically, never pushes.
    fn issue_commit(&mut self, _item_id: &str, message: &str) -> std::result::Result<String, String> {
        if message.trim().is_empty() {
            return Err("commit message is empty".to_string());
        }
        let staged = run_git(&self.repo_root, &["diff".into(), "--cached".into(), "--name-only".into()])?;
        if staged.trim().is_empty() {
            return Err("nothing staged \u{2014} stage changes first with `git add` (this never stages automatically)".to_string());
        }
        run_git(&self.repo_root, &["commit".into(), "-m".into(), message.to_string()])?;
        Ok("committed staged changes".to_string())
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
            "rerun_failed" => {
                let (owner, name, run_id) = parse_run_id(item_id).ok_or("rerun_failed needs a run")?;
                self.rerun_failed(&owner, &name, &run_id)
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
        validate_branch(&self.repo_root, branch)?;
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
        let _ = run_git(
            &self.repo_root,
            &["fetch".into(), "origin".into(), "--".into(), format!("refs/heads/{branch}:refs/remotes/origin/{branch}")],
        );
        let target_str = target.display().to_string();
        let local_exists = run_git(
            &self.repo_root,
            &["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}")],
        )
        .is_ok();
        if local_exists {
            run_git(&self.repo_root, &["worktree".into(), "add".into(), "--".into(), target_str.clone(), branch.into()])?;
        } else {
            run_git(
                &self.repo_root,
                &[
                    "worktree".into(),
                    "add".into(),
                    "-b".into(),
                    branch.into(),
                    "--".into(),
                    target_str.clone(),
                    format!("origin/{branch}"),
                ],
            )?;
        }
        Ok(target_str)
    }

    /// Re-runs the failed jobs of a run (`POST /…/runs/{id}/rerun-failed-jobs`).
    /// Offered only on a failed run and confirmed in the panel first (S16).
    fn rerun_failed(&mut self, owner: &str, name: &str, run_id: &str) -> std::result::Result<String, String> {
        let resp = self.api_post(&format!("/repos/{owner}/{name}/actions/runs/{run_id}/rerun-failed-jobs"), "{}")?;
        if (200..300).contains(&resp.status) {
            Ok(format!("re-running failed jobs for run {run_id}"))
        } else {
            let msg = resp.body.get("message").and_then(|v| v.as_str()).unwrap_or("rerun failed");
            Err(format!("rerun failed ({}): {msg}", resp.status))
        }
    }

    fn checkout(&self, owner: &str, name: &str, branch: &str) -> std::result::Result<String, String> {
        validate_branch(&self.repo_root, branch)?;
        self.require_matching_repo(owner, name)?;
        let status = run_git(&self.repo_root, &["status".into(), "--porcelain".into()])?;
        if !status.trim().is_empty() {
            return Err("working tree has changes — commit or stash first".to_string());
        }
        // Never --force; never discards data.
        run_git(&self.repo_root, &["checkout".into(), branch.into(), "--".into()])?;
        Ok(format!("checked out {branch}"))
    }
}

/// Rejects branch names that git could parse as options or that are not
/// valid branch names, before any git command sees them.
fn validate_branch(repo_root: &Path, branch: &str) -> std::result::Result<(), String> {
    let invalid = || format!("invalid branch name: {branch:?}");
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(|c| c.is_control()) {
        return Err(invalid());
    }
    run_git(repo_root, &["check-ref-format".into(), "--branch".into(), branch.into()]).map_err(|_| invalid())?;
    Ok(())
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

/// Marker shown when branches exceed the pagination cap (count unknown, S16).
fn more_pages_item() -> Item {
    Item {
        id: "more".into(),
        title: "\u{2026} more branches".into(),
        text: String::new(),
        ts: String::new(),
        link: String::new(),
        kind: Some("more".into()),
        expandable: false,
        actions: vec![],
    }
}

/// Hint row when the engine's cwd is not a local GitHub repository (S16).
fn no_local_github_item() -> Item {
    Item {
        id: "wf:none".into(),
        title: "no local GitHub repository".into(),
        text: String::new(),
        ts: String::new(),
        link: String::new(),
        kind: Some("run".into()),
        expandable: false,
        actions: vec![],
    }
}

fn rerun_failed_action() -> ItemAction {
    ItemAction { id: "rerun_failed".into(), label: "Re-run failed jobs".into(), confirm: true }
}

/// Status/conclusion glyph for a workflow run or job (S16): ✓ success,
/// ✗ failure, ● in_progress/queued, ⊘ cancelled, – other.
pub fn run_glyph(status: &str, conclusion: Option<&str>) -> char {
    if status != "completed" {
        return '\u{25cf}'; // ● queued / in_progress
    }
    match conclusion {
        Some("success") => '\u{2713}',   // ✓
        Some("failure") => '\u{2717}',   // ✗
        Some("cancelled") => '\u{2298}', // ⊘
        _ => '\u{2013}',                 // – other
    }
}

/// `run|owner|name|runid` → `(owner, name, runid)`.
fn parse_run_id(id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = id.splitn(4, '|').collect();
    if parts.first() != Some(&"run") || parts.len() < 4 {
        return None;
    }
    Some((parts[1].to_string(), parts[2].to_string(), parts[3].to_string()))
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
        // The latest workflow runs of the LOCAL repo: one request when the cwd
        // is a GitHub repo, none otherwise (keeps the poll ≤ ~6 requests, S16).
        let workflow_count = match self.local_repo() {
            Some((owner, name)) => self.fetch_workflow_runs(&owner, &name)?.len(),
            None => 0,
        };

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
            section("sec:workflows", "Workflows", workflow_count as u64),
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

    fn view(&mut self, item_id: &str) -> Result<(String, String)> {
        self.view_item(item_id)
    }

    fn issue_form(&mut self, item_id: &str) -> Result<IssueSchema> {
        self.issue_form(item_id)
    }

    fn new_issue_form(&mut self, repo: &str) -> Result<IssueSchema> {
        self.new_issue_form(repo)
    }

    fn issue_edit(&mut self, item_id: &str, edit: &IssueFieldEdit) -> Result<String> {
        self.issue_edit(item_id, edit)
    }

    fn issue_create(&mut self, repo: &str, issue: &NewIssue) -> Result<String> {
        self.issue_create(repo, issue)
    }

    fn issue_branch(&mut self, item_id: &str) -> Result<String> {
        self.issue_branch(item_id)
    }

    fn issue_commit(&mut self, item_id: &str, message: &str) -> Result<String> {
        self.issue_commit(item_id, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_edit_payload_only_sends_changed_fields_and_clears_milestone() {
        // Nothing changed -> no PATCH at all (form diff).
        assert_eq!(issue_edit_payload(&IssueFieldEdit::default()), None);
        // Only the title and labels changed.
        let edit = IssueFieldEdit {
            title: Some("New".into()),
            labels: Some(vec!["bug".into(), "p1".into()]),
            ..Default::default()
        };
        let v: Value = serde_json::from_str(&issue_edit_payload(&edit).unwrap()).unwrap();
        assert_eq!(v, json!({"title": "New", "labels": ["bug", "p1"]}));
        // Milestone 0 clears it (null); a comment is never part of the PATCH.
        let clear = IssueFieldEdit { milestone: Some(0), comment: Some("hi".into()), ..Default::default() };
        let v: Value = serde_json::from_str(&issue_edit_payload(&clear).unwrap()).unwrap();
        assert_eq!(v, json!({"milestone": Value::Null}));
        // Only a comment / project change => no PATCH body.
        let comment_only = IssueFieldEdit { comment: Some("hi".into()), ..Default::default() };
        assert_eq!(issue_edit_payload(&comment_only), None);
        // State and milestone set.
        let set = IssueFieldEdit { state: Some("closed".into()), milestone: Some(4), ..Default::default() };
        let v: Value = serde_json::from_str(&issue_edit_payload(&set).unwrap()).unwrap();
        assert_eq!(v, json!({"state": "closed", "milestone": 4}));
    }

    #[test]
    fn create_issue_payload_includes_only_nonempty_fields() {
        let v: Value = serde_json::from_str(&create_issue_payload(&NewIssue {
            title: "T".into(),
            body: String::new(),
            assignees: vec![],
            labels: vec!["bug".into()],
            milestone: Some(2),
        }))
        .unwrap();
        assert_eq!(v, json!({"title": "T", "labels": ["bug"], "milestone": 2}));
    }

    #[test]
    fn project_field_mutation_shapes_each_value_kind_and_clears() {
        let single = ProjectFieldEdit {
            project_id: "P".into(),
            item_id: "I".into(),
            field_id: "F".into(),
            kind: "single_select".into(),
            value: "opt1".into(),
        };
        let v: Value = serde_json::from_str(&project_field_mutation(&single)).unwrap();
        assert_eq!(v.pointer("/variables/v/singleSelectOptionId").unwrap(), "opt1");
        assert!(v.get("query").unwrap().as_str().unwrap().contains("updateProjectV2ItemFieldValue"));

        let number = ProjectFieldEdit { kind: "number".into(), value: "3".into(), ..single.clone() };
        let v: Value = serde_json::from_str(&project_field_mutation(&number)).unwrap();
        assert_eq!(v.pointer("/variables/v/number").unwrap(), 3.0);

        let iter = ProjectFieldEdit { kind: "iteration".into(), value: "it9".into(), ..single.clone() };
        let v: Value = serde_json::from_str(&project_field_mutation(&iter)).unwrap();
        assert_eq!(v.pointer("/variables/v/iterationId").unwrap(), "it9");

        let clear = ProjectFieldEdit { value: String::new(), ..single.clone() };
        let v: Value = serde_json::from_str(&project_field_mutation(&clear)).unwrap();
        assert!(v.get("query").unwrap().as_str().unwrap().contains("clearProjectV2ItemFieldValue"));
    }

    #[test]
    fn missing_project_scope_maps_to_the_hint() {
        let forbidden = json!({"errors": [{"type": "FORBIDDEN", "message": "Resource not accessible"}]});
        assert!(project_scope_missing(&forbidden));
        let scope = json!({"errors": [{"message": "Your token has not been granted the required scopes"}]});
        assert!(project_scope_missing(&scope));
        assert_eq!(PROJECT_SCOPE_HINT, "Project fields need: gh auth refresh -s project");
        // A normal data response is not a scope error.
        assert!(!project_scope_missing(&json!({"data": {}})));
    }

    #[test]
    fn parse_project_items_reads_fields_options_and_current_values() {
        let body = json!({"data": {"repository": {"issue": {"projectItems": {"nodes": [{
            "id": "PVTI_1",
            "project": {"id": "PVT_1", "title": "Roadmap", "fields": {"nodes": [
                {"id": "F_status", "name": "Status", "dataType": "SINGLE_SELECT", "options": [{"id": "o1", "name": "Todo"}, {"id": "o2", "name": "Done"}]},
                {"id": "F_est", "name": "Estimate", "dataType": "NUMBER"},
                {"id": "F_it", "name": "Iteration", "dataType": "ITERATION", "configuration": {"iterations": [{"id": "i1", "title": "Sprint 1"}]}},
                {"id": "F_assignee", "name": "Assignees", "dataType": "ASSIGNEES"}
            ]}},
            "fieldValues": {"nodes": [
                {"name": "Todo", "field": {"id": "F_status"}},
                {"number": 5.0, "field": {"id": "F_est"}}
            ]}
        }]}}}}});
        let projects = parse_project_items(&body);
        assert_eq!(projects.len(), 1);
        let p = &projects[0];
        assert_eq!(p.project_id, "PVT_1");
        assert_eq!(p.item_id, "PVTI_1");
        // ASSIGNEES is skipped; the other three are kept.
        assert_eq!(p.fields.len(), 3);
        let status = p.fields.iter().find(|f| f.name == "Status").unwrap();
        assert_eq!(status.kind, "single_select");
        assert_eq!(status.current.as_deref(), Some("Todo"));
        assert_eq!(status.options.len(), 2);
        let est = p.fields.iter().find(|f| f.name == "Estimate").unwrap();
        assert_eq!(est.current.as_deref(), Some("5"));
        let it = p.fields.iter().find(|f| f.name == "Iteration").unwrap();
        assert_eq!(it.options[0].name, "Sprint 1");
    }

    #[test]
    fn issue_branch_name_slugifies_and_bounds() {
        assert_eq!(issue_branch_name(42, "Fix the Login Bug!"), "42-fix-the-login-bug");
        assert_eq!(issue_branch_name(7, "   "), "7");
        assert_eq!(issue_branch_name(7, "***"), "7");
        // Very long titles are capped (<= 40 slug chars).
        let long = issue_branch_name(1, &"word ".repeat(30));
        assert!(long.len() <= 2 + 40, "{long}");
        assert!(!long.ends_with('-'));
    }

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

    #[test]
    fn run_glyph_maps_status_and_conclusion() {
        assert_eq!(run_glyph("completed", Some("success")), '\u{2713}');
        assert_eq!(run_glyph("completed", Some("failure")), '\u{2717}');
        assert_eq!(run_glyph("completed", Some("cancelled")), '\u{2298}');
        assert_eq!(run_glyph("completed", Some("timed_out")), '\u{2013}');
        assert_eq!(run_glyph("completed", None), '\u{2013}');
        assert_eq!(run_glyph("in_progress", None), '\u{25cf}');
        assert_eq!(run_glyph("queued", None), '\u{25cf}');
    }

    #[test]
    fn parse_run_id_extracts_owner_name_run() {
        assert_eq!(parse_run_id("run|octo|repo|42"), Some(("octo".into(), "repo".into(), "42".into())));
        assert_eq!(parse_run_id("branch|octo|repo|s|x"), None);
    }

    #[test]
    fn parse_issue_or_pr_id_reads_issues_and_prs() {
        assert_eq!(parse_issue_or_pr_id("issue:octo/repo#7"), Some((false, "octo".into(), "repo".into(), 7)));
        assert_eq!(parse_issue_or_pr_id("pr:octo/repo#42"), Some((true, "octo".into(), "repo".into(), 42)));
        assert_eq!(parse_issue_or_pr_id("branch|octo|repo|s|x"), None);
        assert_eq!(parse_issue_or_pr_id("issue:octo/repo"), None);
        assert_eq!(parse_issue_or_pr_id("issue:/repo#1"), None);
        assert_eq!(parse_issue_or_pr_id("issue:octo/repo#x"), None);
    }

    #[test]
    fn build_detail_renders_header_title_url_body_and_comments() {
        // S27: pure render from fixture JSON (no network).
        let obj = serde_json::json!({
            "title": "Crash on save",
            "state": "open",
            "user": {"login": "ann"},
            "html_url": "https://github.com/octo/repo/issues/7",
            "body": "It crashes.",
            "labels": [{"name": "bug"}, {"name": "p1"}]
        });
        let comments = vec![
            serde_json::json!({"user": {"login": "bob"}, "body": "Reproduced."}),
            serde_json::json!({"user": {"login": "cat"}, "body": "Fixing."}),
        ];
        let (title, body) = build_detail("octo/repo", 7, false, &obj, &comments);
        assert_eq!(title, "octo/repo#7 \u{b7} Crash on save");
        assert!(body.starts_with("octo/repo#7 \u{b7} issue \u{b7} open \u{b7} @ann \u{b7} bug, p1"), "{body}");
        assert!(body.contains("Crash on save"));
        assert!(body.contains("https://github.com/octo/repo/issues/7"));
        assert!(body.contains("It crashes."));
        assert!(body.contains("Comments (2)"));
        assert!(body.contains("@bob:\nReproduced."));
        assert!(body.contains("@cat:\nFixing."));
    }

    #[test]
    fn build_detail_handles_a_missing_body_and_no_labels_for_a_pr() {
        let obj = serde_json::json!({
            "title": "A PR",
            "state": "closed",
            "user": {"login": "dan"},
            "html_url": "https://github.com/octo/repo/pull/9",
            "body": null
        });
        let (title, body) = build_detail("octo/repo", 9, true, &obj, &[]);
        assert_eq!(title, "octo/repo#9 \u{b7} A PR");
        assert!(body.starts_with("octo/repo#9 \u{b7} PR \u{b7} closed \u{b7} @dan"), "{body}");
        // No labels segment when there are none.
        assert!(!body.lines().next().unwrap().contains(" \u{b7} bug"));
        assert!(body.contains("(no description)"));
        assert!(body.contains("Comments (0)"));
    }

    #[test]
    fn build_detail_bounds_oversized_bodies_and_comment_counts() {
        // A body well over the cap is truncated with a marker.
        let big = "x".repeat(MAX_DETAIL_BYTES + 10_000);
        let obj = serde_json::json!({
            "title": "big",
            "state": "open",
            "user": {"login": "ann"},
            "html_url": "u",
            "body": big
        });
        let (_t, body) = build_detail("o/r", 1, false, &obj, &[]);
        assert!(body.len() <= MAX_DETAIL_BYTES + 32, "len {}", body.len());
        assert!(body.ends_with("\u{2026} (truncated)"));

        // More than 50 comments: only 50 are rendered and the rest summarised.
        let comments: Vec<serde_json::Value> = (0..60)
            .map(|i| serde_json::json!({"user": {"login": format!("u{i}")}, "body": "c"}))
            .collect();
        let small = serde_json::json!({"title": "t", "state": "open", "user": {"login": "a"}, "html_url": "u", "body": "b"});
        let (_t2, body2) = build_detail("o/r", 2, false, &small, &comments);
        assert!(body2.contains("Comments (60)"));
        assert!(body2.contains("@u49:"), "the 50th comment is shown");
        assert!(!body2.contains("@u50:"), "the 51st comment is not shown");
        assert!(body2.contains("\u{2026} 10 more comments"));
    }
}
