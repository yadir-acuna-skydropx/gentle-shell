//! The real Slack connector (T13): polls `search.messages` for mentions of the
//! logged-in user and replies in the mention's thread as that user.
//!
//! Status, polling and replies all run on the connector's own thread (see
//! `engine`). The token comes from the injected [`TokenStore`]; the base URLs
//! come from [`SlackConfig`] (overridable for tests). The access token is never
//! printed, logged or written to the protocol.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::config;
use crate::connector::{Connector, Result};
use crate::http::{self, HttpResponse};
use crate::oauth::{self, SlackConfig};
use crate::protocol::{Item, ItemAction, Status};
use crate::store::{SLACK_ACCOUNT, StoredToken, TokenStore};

/// Polling interval for Slack mentions (Tier-2 friendly).
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Max items kept after dedupe (newest first).
pub const MAX_ITEMS: usize = 50;
/// Cap on how long we honor a `Retry-After` before giving up this cycle.
const MAX_BACKOFF: u64 = 60;
/// Refresh a rotating token this many seconds before it expires.
const REFRESH_SKEW: u64 = 300;

/// Hint shown in the panel until the Slack token is configured.
pub const DISCONNECTED_DETAIL: &str = "run `gentle-apps slack login`";

/// A remembered search match, so a later reply can target the exact thread.
#[derive(Debug, Clone)]
struct MatchRef {
    channel: String,
    ts: String,
    permalink: String,
}

/// Opens a link and reports whether the Slack desktop app is installed.
/// Injected so tests exercise both the deep-link and the permalink fallback
/// (S16, L42: "Instalar Slack Desktop + enlaces").
pub trait SlackLinkOpener: Send {
    fn open(&self, url: &str) -> std::result::Result<(), String>;
    /// True when a Slack desktop app can handle `slack://` links.
    fn slack_desktop_installed(&self) -> bool;
}

/// Production opener: `xdg-open` to launch (no shell), detecting the desktop app
/// via a `slack` binary on `PATH` or an `x-scheme-handler/slack` default.
pub struct DesktopOpener;

impl SlackLinkOpener for DesktopOpener {
    fn open(&self, url: &str) -> std::result::Result<(), String> {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("xdg-open: {e}"))
    }
    fn slack_desktop_installed(&self) -> bool {
        slack_on_path() || slack_scheme_handler()
    }
}

/// True when a `slack` executable is on `PATH` (no shell).
fn slack_on_path() -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&paths).any(|dir| dir.join("slack").is_file())
}

/// True when `slack://` has a registered default handler via `xdg-mime`.
fn slack_scheme_handler() -> bool {
    std::process::Command::new("xdg-mime")
        .args(["query", "default", "x-scheme-handler/slack"])
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// The `open` action offered on every Slack mention (S16).
fn open_action() -> ItemAction {
    ItemAction { id: "open".into(), label: "Open in Slack".into(), confirm: false }
}

/// The Slack connector.
pub struct SlackConnector {
    cfg: SlackConfig,
    client_id: Option<String>,
    store: Box<dyn TokenStore>,
    opener: Box<dyn SlackLinkOpener>,
    agent: ureq::Agent,
    status: Status,
    detail: Option<String>,
    user_id: Option<String>,
    /// The workspace team id (from `auth.test`), used to build `slack://` links.
    team_id: Option<String>,
    users: HashMap<String, String>,
    matches: HashMap<String, MatchRef>,
}

impl SlackConnector {
    /// Builds a connector. Starts `Disconnected` with a login hint until the
    /// first poll confirms the token.
    pub fn new(
        cfg: SlackConfig,
        client_id: Option<String>,
        store: Box<dyn TokenStore>,
        home: PathBuf,
    ) -> Self {
        Self::with_opener(cfg, client_id, store, home, Box::new(DesktopOpener))
    }

    /// Builds a connector with an injected opener (tests use this to control the
    /// desktop-installed detection and record the opened URL).
    pub fn with_opener(
        cfg: SlackConfig,
        client_id: Option<String>,
        store: Box<dyn TokenStore>,
        _home: PathBuf,
        opener: Box<dyn SlackLinkOpener>,
    ) -> Self {
        SlackConnector {
            cfg,
            client_id,
            store,
            opener,
            agent: http::agent(),
            status: Status::Disconnected,
            detail: Some(DISCONNECTED_DETAIL.to_string()),
            user_id: None,
            team_id: None,
            users: HashMap::new(),
            matches: HashMap::new(),
        }
    }

    /// Opens a mention: the Slack Desktop deep link when the app is installed and
    /// the team id is known, else the https permalink (S16). The `message`
    /// parameter is not documented for `slack://channel` in Slack's deep-linking
    /// reference, so the channel deep link is used with the permalink fallback.
    fn open_item(&mut self, item_id: &str) -> std::result::Result<String, String> {
        let mref = self
            .matches
            .get(item_id)
            .cloned()
            .or_else(|| {
                item_id.split_once(':').map(|(c, t)| MatchRef {
                    channel: c.to_string(),
                    ts: t.to_string(),
                    permalink: String::new(),
                })
            })
            .ok_or_else(|| format!("unknown item: {item_id}"))?;
        let url = match (self.opener.slack_desktop_installed(), self.team_id.as_deref()) {
            (true, Some(team)) if !team.is_empty() => {
                format!("slack://channel?team={team}&id={}", mref.channel)
            }
            _ => {
                if mref.permalink.is_empty() {
                    return Err("no permalink to open".to_string());
                }
                mref.permalink.clone()
            }
        };
        self.opener.open(&url)?;
        Ok(format!("opened {url}"))
    }

    /// Loads the token, refreshing it first when it is near expiry.
    fn fresh_token(&mut self) -> std::result::Result<Option<StoredToken>, String> {
        let Some(token) = self.store.load(SLACK_ACCOUNT)? else {
            return Ok(None);
        };
        let needs_refresh = match (token.refresh_token.as_ref(), token.expires_at) {
            (Some(_), Some(exp)) => oauth::now_unix() + REFRESH_SKEW >= exp,
            _ => false,
        };
        if needs_refresh {
            if let (Some(client_id), Some(refresh)) = (self.client_id.clone(), token.refresh_token.clone())
            {
                match oauth::refresh(&self.cfg, &client_id, &refresh) {
                    Ok(fields) => {
                        let stored = oauth::to_stored(&fields);
                        self.store.save(SLACK_ACCOUNT, &stored)?;
                        return Ok(Some(stored));
                    }
                    Err(e) => {
                        // Refresh failed: fall back to the current token; the API
                        // call will surface auth errors if it is truly expired.
                        eprintln!("gentle-apps: slack token refresh failed: {e}");
                    }
                }
            }
        }
        Ok(Some(token))
    }

    /// GET a Slack method, honoring one `Retry-After` (HTTP 429) backoff.
    fn get(&self, method: &str, query: &str, token: &str) -> std::result::Result<HttpResponse, String> {
        let url = if query.is_empty() {
            format!("{}/{}", self.cfg.api_base, method)
        } else {
            format!("{}/{}?{}", self.cfg.api_base, method, query)
        };
        let resp = http::get(&self.agent, &url, Some(token))?;
        if let Some(wait) = rate_limit_wait(&resp) {
            std::thread::sleep(Duration::from_secs(wait));
            return http::get(&self.agent, &url, Some(token));
        }
        Ok(resp)
    }

    /// auth.test → the user id; also learns the team id for `slack://` links.
    /// Sets status on failure.
    fn auth_test(&mut self, token: &str) -> std::result::Result<String, String> {
        let resp = self.get("auth.test", "", token)?;
        let v = &resp.body;
        if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
            self.team_id =
                v.get("team_id").and_then(|t| t.as_str()).filter(|s| !s.is_empty()).map(String::from);
            Ok(v.get("user_id").and_then(|u| u.as_str()).unwrap_or_default().to_string())
        } else {
            Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("auth_failed").to_string())
        }
    }

    /// Resolves display names for any `<@U..>` ids not already cached.
    fn resolve_users(&mut self, ids: &[String], token: &str) {
        for id in ids {
            if self.users.contains_key(id) {
                continue;
            }
            if let Ok(resp) = self.get("users.info", &format!("user={id}"), token) {
                let name = resp
                    .body
                    .get("user")
                    .and_then(|u| u.get("profile").and_then(|p| p.get("display_name")).and_then(|d| d.as_str()).filter(|s| !s.is_empty())
                        .or_else(|| u.get("name").and_then(|n| n.as_str())))
                    .map(String::from);
                if let Some(name) = name {
                    self.users.insert(id.clone(), name);
                }
            }
        }
    }

    /// Derives the parent thread ts for a reply.
    fn thread_ts(&self, m: &MatchRef, token: &str) -> String {
        if let Some(t) = thread_ts_from_permalink(&m.permalink) {
            return t;
        }
        if let Ok(resp) = self.get(
            "conversations.replies",
            &format!("channel={}&ts={}&limit=1", m.channel, m.ts),
            token,
        ) {
            if let Some(t) = resp
                .body
                .get("messages")
                .and_then(|msgs| msgs.get(0))
                .and_then(|m0| m0.get("thread_ts"))
                .and_then(|t| t.as_str())
            {
                return t.to_string();
            }
        }
        m.ts.clone()
    }
}

/// Returns the number of seconds to wait if `resp` indicates a rate limit.
fn rate_limit_wait(resp: &HttpResponse) -> Option<u64> {
    let is_429 = resp.status == 429;
    let is_ratelimited = resp.body.get("error").and_then(|e| e.as_str()) == Some("ratelimited");
    if is_429 || is_ratelimited {
        let secs = resp
            .retry_after
            .as_deref()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .min(MAX_BACKOFF);
        Some(secs)
    } else {
        None
    }
}

/// Maps a Slack error code to the detail shown in the panel.
fn error_detail(code: &str) -> String {
    match code {
        "invalid_auth" | "token_revoked" | "account_inactive" => {
            "token revoked — run `gentle-apps slack login`".to_string()
        }
        other => format!("Slack error: {other}"),
    }
}

/// Extracts the `thread_ts` query parameter from a permalink, if present.
pub fn thread_ts_from_permalink(permalink: &str) -> Option<String> {
    let query = permalink.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        if let Some(("thread_ts", v)) = pair.split_once('=') {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Builds an [`Item`] from one search match, rendering its text.
fn match_to_item(m: &serde_json::Value, users: &HashMap<String, String>) -> Option<(Item, MatchRef)> {
    let ts = m.get("ts").and_then(|t| t.as_str())?.to_string();
    let channel = m.get("channel")?;
    let channel_id = channel.get("id").and_then(|i| i.as_str())?.to_string();
    let channel_name = channel.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let is_im = channel.get("is_im").and_then(|b| b.as_bool()).unwrap_or(false);
    let is_private = channel.get("is_private").and_then(|b| b.as_bool()).unwrap_or(false)
        || channel.get("is_group").and_then(|b| b.as_bool()).unwrap_or(false);
    let author = m
        .get("username")
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            m.get("user").and_then(|u| u.as_str()).map(|id| users.get(id).cloned().unwrap_or_else(|| id.to_string()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let title = if is_im {
        format!("DM · {author}")
    } else if is_private {
        format!("🔒#{channel_name}")
    } else {
        format!("#{channel_name} · {author}")
    };

    let raw = m.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let text = render_text(raw, users);
    let permalink = m.get("permalink").and_then(|p| p.as_str()).unwrap_or("").to_string();
    let id = format!("{channel_id}:{ts}");
    Some((
        Item {
            id,
            title,
            text,
            ts: ts.clone(),
            link: permalink.clone(),
            actions: vec![open_action()],
            ..Default::default()
        },
        MatchRef { channel: channel_id, ts, permalink },
    ))
}

/// Collects the `<@U..>` user ids referenced in a raw message text.
fn mentioned_user_ids(raw: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find('>') {
            let inner = &rest[..end];
            let id = inner.split('|').next().unwrap_or(inner);
            if !id.is_empty() {
                ids.push(id.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    ids
}

/// Renders Slack mrkdwn entities to plain text (mentions, channels, links,
/// specials), unescapes HTML entities and strips Slack's private-use markers.
pub fn render_text(raw: &str, users: &HashMap<String, String>) -> String {
    let cleaned: String = raw.chars().filter(|&c| c != '\u{E000}' && c != '\u{E001}').collect();
    let mut out = String::new();
    let mut rest = cleaned.as_str();
    while let Some(start) = rest.find('<') {
        out.push_str(&unescape_entities(&rest[..start]));
        rest = &rest[start..];
        match rest.find('>') {
            Some(end) => {
                out.push_str(&render_token(&rest[1..end], users));
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(&unescape_entities(rest));
                rest = "";
                break;
            }
        }
    }
    out.push_str(&unescape_entities(rest));
    out
}

fn render_token(token: &str, users: &HashMap<String, String>) -> String {
    let (head, label) = match token.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (token, None),
    };
    match head.chars().next() {
        Some('@') => {
            let id = &head[1..];
            let name = label
                .map(|l| unescape_entities(l))
                .or_else(|| users.get(id).cloned())
                .unwrap_or_else(|| id.to_string());
            format!("@{name}")
        }
        Some('#') => {
            let id = &head[1..];
            let name = label.map(|l| unescape_entities(l)).unwrap_or_else(|| id.to_string());
            format!("#{name}")
        }
        Some('!') => {
            let special = &head[1..];
            let name = label.map(|l| unescape_entities(l)).unwrap_or_else(|| special.to_string());
            format!("@{name}")
        }
        _ => label.map(|l| unescape_entities(l)).unwrap_or_else(|| unescape_entities(head)),
    }
}

/// Unescapes the three mrkdwn-safe HTML entities (`&amp; &lt; &gt;`).
fn unescape_entities(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

impl Connector for SlackConnector {
    fn id(&self) -> &str {
        "slack"
    }
    fn name(&self) -> &str {
        "Slack"
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
        let token = match self.fresh_token() {
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
        let access = token.access_token.clone();

        // Confirm auth and learn the user id (once).
        if self.user_id.is_none() {
            match self.auth_test(&access) {
                Ok(uid) => self.user_id = Some(uid),
                Err(code) => {
                    self.status = Status::Error;
                    self.detail = Some(error_detail(&code));
                    return Err(code);
                }
            }
        }
        let uid = self.user_id.clone().unwrap_or_default();

        // search.messages for mentions of the user.
        let query = format!("query=%3C%40{uid}%3E&sort=timestamp&sort_dir=desc&count=50");
        let resp = self.get("search.messages", &query, &access)?;
        let v = &resp.body;
        if !v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
            let code = v.get("error").and_then(|e| e.as_str()).unwrap_or("search_failed").to_string();
            self.status = Status::Error;
            self.detail = Some(error_detail(&code));
            return Err(code);
        }
        self.status = Status::Connected;
        self.detail = self.user_id.as_ref().map(|u| format!("connected as {u}"));

        let matches = v
            .get("messages")
            .and_then(|m| m.get("matches"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();

        // Resolve any mentioned users for a nicer text render.
        let mut unknown: Vec<String> = Vec::new();
        for m in &matches {
            if let Some(text) = m.get("text").and_then(|t| t.as_str()) {
                for id in mentioned_user_ids(text) {
                    if !self.users.contains_key(&id) && !unknown.contains(&id) {
                        unknown.push(id);
                    }
                }
            }
        }
        if !unknown.is_empty() {
            self.resolve_users(&unknown, &access);
        }

        // Build, dedupe by channel+ts, keep the newest MAX_ITEMS.
        let mut items: Vec<Item> = Vec::new();
        let mut seen: HashMap<String, ()> = HashMap::new();
        self.matches.clear();
        for m in &matches {
            if let Some((item, mref)) = match_to_item(m, &self.users) {
                if seen.insert(item.id.clone(), ()).is_none() {
                    self.matches.insert(item.id.clone(), mref);
                    items.push(item);
                }
            }
        }
        items.sort_by(|a, b| b.ts.cmp(&a.ts));
        items.truncate(MAX_ITEMS);
        Ok(items)
    }

    fn reply(&mut self, item_id: &str, text: &str) -> Result<()> {
        let token = match self.fresh_token()? {
            Some(t) => t,
            None => return Err(DISCONNECTED_DETAIL.to_string()),
        };
        let access = token.access_token.clone();

        let mref = self
            .matches
            .get(item_id)
            .cloned()
            .or_else(|| {
                item_id.split_once(':').map(|(c, t)| MatchRef {
                    channel: c.to_string(),
                    ts: t.to_string(),
                    permalink: String::new(),
                })
            })
            .ok_or_else(|| format!("unknown item: {item_id}"))?;

        let thread_ts = self.thread_ts(&mref, &access);

        let url = format!("{}/chat.postMessage", self.cfg.api_base);
        let resp = http::post_form(
            &self.agent,
            &url,
            &[
                ("channel", mref.channel.as_str()),
                ("thread_ts", thread_ts.as_str()),
                ("text", text),
            ],
            Some(&access),
        )?;
        if resp.body.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            Err(resp.body.get("error").and_then(|e| e.as_str()).unwrap_or("post_failed").to_string())
        }
    }

    fn action(&mut self, item_id: &str, action_id: &str) -> Result<String> {
        match action_id {
            "open" => self.open_item(item_id),
            other => Err(format!("unknown action: {other}")),
        }
    }
}

/// Prints the Slack connection status for the CLI (`slack status`). Never
/// prints the token. Returns the process exit code (always 0).
pub fn status_line(store: &dyn TokenStore, home: &std::path::Path) -> i32 {
    let meta = config::load(home);
    match store.load(SLACK_ACCOUNT) {
        Ok(Some(_)) => {
            let team = meta.team_name.as_deref().unwrap_or("unknown workspace");
            let user = meta.user_id.as_deref().unwrap_or("unknown user");
            println!("connected — workspace {team}, user {user}");
        }
        _ => println!("not connected — run `gentle-apps slack login`"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn users() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("U1".to_string(), "alice".to_string());
        m
    }

    #[test]
    fn renders_mentions_channels_links_specials_and_entities() {
        let u = users();
        assert_eq!(render_text("hi <@U1> there", &u), "hi @alice there");
        assert_eq!(render_text("ping <@U2|bob>", &u), "ping @bob");
        assert_eq!(render_text("in <#C1|general>", &u), "in #general");
        assert_eq!(render_text("see <https://ex.com|docs>", &u), "see docs");
        assert_eq!(render_text("raw <https://ex.com>", &u), "raw https://ex.com");
        assert_eq!(render_text("<!here> all", &u), "@here all");
        assert_eq!(render_text("a &amp; b &lt;x&gt;", &u), "a & b <x>");
    }

    #[test]
    fn strips_private_use_markers_around_entities() {
        let u = users();
        assert_eq!(render_text("\u{E000}<@U1>\u{E001} hi", &u), "@alice hi");
    }

    #[test]
    fn unknown_mention_falls_back_to_the_id() {
        let u = HashMap::new();
        assert_eq!(render_text("<@U9>", &u), "@U9");
    }

    #[test]
    fn thread_ts_is_read_from_the_permalink_param() {
        assert_eq!(
            thread_ts_from_permalink("https://x.slack.com/archives/C1/p1700000000000100?thread_ts=1699999999.000100&cid=C1"),
            Some("1699999999.000100".to_string())
        );
        assert_eq!(thread_ts_from_permalink("https://x.slack.com/archives/C1/p170?cid=C1"), None);
        assert_eq!(thread_ts_from_permalink("https://x.slack.com/archives/C1/p170"), None);
    }

    #[test]
    fn mentioned_user_ids_extracted() {
        assert_eq!(mentioned_user_ids("hi <@U1> and <@U2|bob> end"), vec!["U1", "U2"]);
        assert_eq!(mentioned_user_ids("none"), Vec::<String>::new());
    }

    #[test]
    fn match_to_item_builds_title_text_and_dm_private_variants() {
        let u = users();
        let normal: serde_json::Value = serde_json::from_str(
            r#"{"ts":"1.2","username":"ana","text":"hey <@U1>","permalink":"https://x?cid=C1","channel":{"id":"C1","name":"general","is_im":false,"is_private":false}}"#,
        )
        .unwrap();
        let (item, mref) = match_to_item(&normal, &u).unwrap();
        assert_eq!(item.id, "C1:1.2");
        assert_eq!(item.title, "#general · ana");
        assert_eq!(item.text, "hey @alice");
        assert_eq!(mref.channel, "C1");

        let dm: serde_json::Value = serde_json::from_str(
            r#"{"ts":"3.4","username":"ana","text":"hi","permalink":"p","channel":{"id":"D1","name":"","is_im":true}}"#,
        )
        .unwrap();
        assert_eq!(match_to_item(&dm, &u).unwrap().0.title, "DM · ana");

        let private: serde_json::Value = serde_json::from_str(
            r#"{"ts":"5.6","username":"ana","text":"hi","permalink":"p","channel":{"id":"G1","name":"secret","is_private":true}}"#,
        )
        .unwrap();
        assert_eq!(match_to_item(&private, &u).unwrap().0.title, "🔒#secret");
    }

    #[test]
    fn rate_limit_wait_honors_retry_after_and_ratelimited() {
        let r429 = HttpResponse {
            status: 429,
            retry_after: Some("2".into()),
            body: serde_json::Value::Null,
        };
        assert_eq!(rate_limit_wait(&r429), Some(2));
        let rl: HttpResponse = HttpResponse {
            status: 200,
            retry_after: None,
            body: serde_json::json!({"ok": false, "error": "ratelimited"}),
        };
        assert_eq!(rate_limit_wait(&rl), Some(1));
        let ok = HttpResponse { status: 200, retry_after: None, body: serde_json::json!({"ok": true}) };
        assert_eq!(rate_limit_wait(&ok), None);
    }

    #[test]
    fn poll_is_disconnected_without_a_token() {
        let store = Box::new(MemoryStore::new());
        let mut c = SlackConnector::new(SlackConfig::default(), Some("cid".into()), store, PathBuf::from("/tmp"));
        assert_eq!(c.poll().unwrap(), Vec::new());
        assert_eq!(c.status(), Status::Disconnected);
        assert!(c.detail().unwrap().contains("slack login"));
    }

    #[test]
    fn error_detail_maps_invalid_auth() {
        assert!(error_detail("invalid_auth").contains("token revoked"));
        assert!(error_detail("some_other").contains("some_other"));
    }
}
