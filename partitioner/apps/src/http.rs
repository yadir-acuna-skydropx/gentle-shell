//! Thin blocking HTTP helper over `ureq` (rustls, no OpenSSL) shared by the
//! OAuth login and the Slack connector.
//!
//! The agent is configured with `http_status_as_error(false)` so a 4xx/5xx (and
//! Slack's `429` rate limit) is a normal response we can inspect, not an `Err`.
//! Slack itself replies `200` with `{"ok":false,"error":"…"}`, so callers read
//! the parsed JSON body in every case.
use std::time::Duration;

/// A Slack HTTP response reduced to what the connector and login need.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// The `Retry-After` header value (seconds), when present (HTTP 429).
    pub retry_after: Option<String>,
    pub body: serde_json::Value,
}

/// Builds a blocking agent with sane timeouts and status-as-error disabled.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

fn finish(mut resp: ureq::http::Response<ureq::Body>) -> Result<HttpResponse, String> {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // Slack always answers JSON; an empty/non-JSON body becomes Null.
    let text = resp.body_mut().read_to_string().map_err(|e| format!("read body: {e}"))?;
    let body: serde_json::Value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| format!("parse body: {e}"))?
    };
    Ok(HttpResponse { status, retry_after, body })
}

/// POSTs a URL-encoded form, optionally with a Bearer token.
pub fn post_form(
    agent: &ureq::Agent,
    url: &str,
    fields: &[(&str, &str)],
    bearer: Option<&str>,
) -> Result<HttpResponse, String> {
    let mut req = agent.post(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send_form(fields.iter().copied()).map_err(|e| e.to_string())?;
    finish(resp)
}

/// POSTs a JSON body with a Bearer token.
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    body: &str,
    bearer: &str,
) -> Result<HttpResponse, String> {
    let resp = agent
        .post(url)
        .header("Authorization", format!("Bearer {bearer}"))
        .header("Content-Type", "application/json; charset=utf-8")
        .send(body)
        .map_err(|e| e.to_string())?;
    finish(resp)
}

/// GETs a URL, optionally with a Bearer token.
pub fn get(agent: &ureq::Agent, url: &str, bearer: Option<&str>) -> Result<HttpResponse, String> {
    let mut req = agent.get(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    finish(resp)
}

/// A GitHub REST/GraphQL response reduced to what the connector needs,
/// including the rate-limit headers used to back off.
#[derive(Debug, Clone)]
pub struct GithubResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    /// `X-RateLimit-Remaining`, when present.
    pub rate_remaining: Option<i64>,
    /// `X-RateLimit-Reset` (unix seconds), when present.
    pub rate_reset: Option<u64>,
    pub body: serde_json::Value,
}

fn header_str(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn finish_github(mut resp: ureq::http::Response<ureq::Body>) -> Result<GithubResponse, String> {
    let status = resp.status().as_u16();
    let retry_after = header_str(&resp, "retry-after");
    let rate_remaining = header_str(&resp, "x-ratelimit-remaining").and_then(|s| s.trim().parse::<i64>().ok());
    let rate_reset = header_str(&resp, "x-ratelimit-reset").and_then(|s| s.trim().parse::<u64>().ok());
    let text = resp.body_mut().read_to_string().map_err(|e| format!("read body: {e}"))?;
    let body: serde_json::Value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| format!("parse body: {e}"))?
    };
    Ok(GithubResponse { status, retry_after, rate_remaining, rate_reset, body })
}

/// GETs a GitHub REST URL with the required headers.
pub fn github_get(agent: &ureq::Agent, url: &str, token: &str) -> Result<GithubResponse, String> {
    let resp = agent
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "gentle-apps")
        .call()
        .map_err(|e| e.to_string())?;
    finish_github(resp)
}

/// POSTs a JSON body to a GitHub URL (used for GraphQL) with the required headers.
pub fn github_post_json(agent: &ureq::Agent, url: &str, body: &str, token: &str) -> Result<GithubResponse, String> {
    let resp = agent
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "gentle-apps")
        .header("Content-Type", "application/json; charset=utf-8")
        .send(body)
        .map_err(|e| e.to_string())?;
    finish_github(resp)
}

/// PATCHes a JSON body to a GitHub REST URL (issue field edits, S30).
pub fn github_patch_json(agent: &ureq::Agent, url: &str, body: &str, token: &str) -> Result<GithubResponse, String> {
    let resp = agent
        .patch(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "gentle-apps")
        .header("Content-Type", "application/json; charset=utf-8")
        .send(body)
        .map_err(|e| e.to_string())?;
    finish_github(resp)
}
