//! The "Ciberseguridad" connector (S35): surfaces the security reports of the
//! current repo as apps items, lets the user open a report summary or a finding
//! detail in the Reader (via the `view` action → `EngineMsg::Detail`), run a new
//! scan, and install the pre-commit hook.
//!
//! Report reading reuses the `gentle-engines::sec::report` model so the on-disk
//! schema has exactly one definition. A scan runs the `gentle-sec` binary off
//! the UI thread (on this connector's own thread); the new report appears on the
//! next poll. The status line shows which scanner tools are installed.
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use gentle_engines::sec::report::{self, Report};
use gentle_engines::sec::run::detect_tools;
use gentle_engines::sec::{Finding, Severity};

use crate::connector::{Connector, Result};
use crate::protocol::{Item, ItemAction, Status};
use crate::sanitize::{sanitize_display, sanitize_line};

/// Poll every 30 seconds so a scan triggered elsewhere shows up without a manual
/// refresh.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Severity bands shown as expandable groups under a report, high → low.
const BANDS: &[Severity] = &[
    Severity::Critical,
    Severity::High,
    Severity::Medium,
    Severity::Low,
    Severity::Info,
];

/// The connector. All state is the repo root; reports are read from disk on
/// demand.
pub struct SecConnector {
    repo_root: PathBuf,
}

impl SecConnector {
    pub fn new(repo_root: PathBuf) -> Self {
        SecConnector { repo_root }
    }

    /// Builds the top-level item list: two action rows (scan, hook) then one row
    /// per saved report, newest first.
    fn top_items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        items.push(Item {
            id: "sec:scan".into(),
            title: "▶ Ejecutar análisis de seguridad".into(),
            text: "Runs the installed scanners on this repo".into(),
            kind: Some("action".into()),
            actions: vec![ItemAction { id: "scan".into(), label: "Scan now".into(), confirm: false }],
            ..Default::default()
        });
        items.push(Item {
            id: "sec:hook".into(),
            title: "⚙ Instalar hook pre-commit".into(),
            text: "Scan staged files before each commit (blocks on high findings)".into(),
            kind: Some("action".into()),
            actions: vec![ItemAction { id: "hook".into(), label: "Install pre-commit hook".into(), confirm: true }],
            ..Default::default()
        });
        for meta in report::list_reports(&self.repo_root) {
            items.push(Item {
                id: format!("sec:report:{}", meta.id),
                title: sanitize_line(&meta.label()),
                text: format!("{} findings", meta.findings),
                ts: meta.generated_at.clone(),
                kind: Some("report".into()),
                expandable: true,
                actions: vec![
                    ItemAction { id: "view".into(), label: "View summary".into(), confirm: false },
                    ItemAction { id: "scan".into(), label: "Scan now".into(), confirm: false },
                ],
                ..Default::default()
            });
        }
        items
    }

    /// Expands a report into per-severity group rows (only non-empty bands).
    fn report_groups(&self, report_id: &str) -> Result<Vec<Item>> {
        let report = self.load(report_id)?;
        let mut items = Vec::new();
        for band in BANDS {
            let n = report.findings.iter().filter(|f| f.severity == *band).count();
            if n == 0 {
                continue;
            }
            items.push(Item {
                id: format!("sec:group:{report_id}:{}", band.label()),
                title: format!("{} ({n})", band.label()),
                kind: Some("group".into()),
                expandable: true,
                ..Default::default()
            });
        }
        if items.is_empty() {
            items.push(Item {
                id: format!("sec:empty:{report_id}"),
                title: "No findings 🎉".into(),
                kind: Some("info".into()),
                ..Default::default()
            });
        }
        Ok(items)
    }

    /// Expands a severity group into its finding rows.
    fn group_findings(&self, report_id: &str, band_label: &str) -> Result<Vec<Item>> {
        let report = self.load(report_id)?;
        let mut items = Vec::new();
        for (i, f) in report.findings.iter().enumerate() {
            if f.severity.label() != band_label {
                continue;
            }
            items.push(finding_item(report_id, i, f));
        }
        Ok(items)
    }

    /// Loads a report by id, mapping IO/JSON errors to a connector string.
    fn load(&self, report_id: &str) -> Result<Report> {
        report::load_report(&self.repo_root, report_id).map_err(|e| format!("report {report_id}: {e}"))
    }

    /// Runs `gentle-sec scan --json` in the repo, off the UI thread.
    fn run_scan(&self) -> Result<String> {
        let out = Command::new("gentle-sec")
            .args(["scan", "--json"])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("gentle-sec not available: {e}"))?;
        if out.status.success() || out.status.code() == Some(1) {
            // Count findings from the JSON for a friendly message.
            let text = String::from_utf8_lossy(&out.stdout);
            let n = serde_json::from_str::<Report>(&text).map(|r| r.findings.len()).unwrap_or(0);
            Ok(format!("Scan complete: {n} findings (refresh to see the new report)"))
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(sanitize_line(err.trim()))
        }
    }

    /// Installs the pre-commit hook via `gentle-sec hook install`.
    fn install_hook(&self) -> Result<String> {
        let out = Command::new("gentle-sec")
            .args(["hook", "install"])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("gentle-sec not available: {e}"))?;
        if out.status.success() {
            Ok(sanitize_line(String::from_utf8_lossy(&out.stdout).trim()))
        } else {
            Err(sanitize_line(String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
}

/// Builds a finding leaf item with a `view` action.
fn finding_item(report_id: &str, index: usize, f: &Finding) -> Item {
    let loc = if f.line > 0 {
        format!("{}:{}", f.file, f.line)
    } else if !f.file.is_empty() {
        f.file.clone()
    } else {
        "-".to_string()
    };
    Item {
        id: format!("sec:finding:{report_id}:{index}"),
        title: sanitize_line(&format!("[{}] {} — {}", f.tool, f.title, loc)),
        text: sanitize_line(&f.message),
        kind: Some("finding".into()),
        actions: vec![ItemAction { id: "view".into(), label: "View detail".into(), confirm: false }],
        ..Default::default()
    }
}

/// Renders a finding as the Reader body (already redacted/sanitized at source).
fn finding_detail(f: &Finding) -> (String, String) {
    let title = sanitize_line(&format!("[{}] {}", f.severity.label(), f.title));
    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", title));
    body.push_str(&format!("- Tool: {}\n", f.tool));
    body.push_str(&format!("- Rule: {}\n", f.rule_id));
    body.push_str(&format!("- Severity: {}\n", f.severity.label()));
    if !f.cwe.is_empty() {
        body.push_str(&format!("- CWE: {}\n", f.cwe.join(", ")));
    }
    if let Some(o) = &f.owasp {
        let title = gentle_engines::sec::owasp::category_title(o).unwrap_or("");
        body.push_str(&format!("- OWASP: {o} {title}\n"));
    }
    let loc = if f.line > 0 { format!("{}:{}", f.file, f.line) } else { f.file.clone() };
    if !loc.is_empty() {
        body.push_str(&format!("- Location: {loc}\n"));
    }
    body.push_str(&format!("\n{}\n", sanitize_display(&f.message)));
    if let Some(r) = &f.remediation {
        body.push_str(&format!("\n## Remediation\n\n{}\n", sanitize_display(r)));
    }
    (title, body)
}

impl Connector for SecConnector {
    fn id(&self) -> &str {
        "sec"
    }

    fn name(&self) -> &str {
        "Ciberseguridad"
    }

    fn status(&self) -> Status {
        // The engine is local, so it is always "connected"; the detail shows
        // which scanners are available.
        Status::Connected
    }

    fn detail(&self) -> Option<String> {
        let (run, _missing) = detect_tools();
        if run.is_empty() {
            Some("no scanners installed".into())
        } else {
            Some(format!("tools: {}", run.join(", ")))
        }
    }

    fn interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<Vec<Item>> {
        Ok(self.top_items())
    }

    fn reply(&mut self, _item_id: &str, _text: &str) -> Result<()> {
        Err("the security connector has no reply".into())
    }

    fn expand(&mut self, item_id: &str) -> Result<Vec<Item>> {
        if let Some(rest) = item_id.strip_prefix("sec:report:") {
            return self.report_groups(rest);
        }
        if let Some(rest) = item_id.strip_prefix("sec:group:") {
            // `sec:group:<reportid>:<band>`
            let (report_id, band) = rest.rsplit_once(':').ok_or("bad group id")?;
            return self.group_findings(report_id, band);
        }
        Err("not expandable".into())
    }

    fn action(&mut self, item_id: &str, action_id: &str) -> Result<String> {
        match action_id {
            "scan" => self.run_scan(),
            "hook" => self.install_hook(),
            _ => Err(format!("unknown action '{action_id}' on {item_id}")),
        }
    }

    fn view(&mut self, item_id: &str) -> Result<(String, String)> {
        if let Some(rest) = item_id.strip_prefix("sec:report:") {
            let md = report::show_report(&self.repo_root, rest).map_err(|e| format!("report {rest}: {e}"))?;
            return Ok((format!("Security report {rest}"), sanitize_display(&md)));
        }
        if let Some(rest) = item_id.strip_prefix("sec:finding:") {
            let (report_id, idx) = rest.rsplit_once(':').ok_or("bad finding id")?;
            let index: usize = idx.parse().map_err(|_| "bad finding index")?;
            let report = self.load(report_id)?;
            let f = report.findings.get(index).ok_or("finding not found")?;
            return Ok(finding_detail(f));
        }
        Err("no detail for this item".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gentle_engines::sec::report::write_report;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gsec-conn-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_report(id: &str) -> Report {
        Report {
            id: id.into(),
            generated_at: "2026-11-15T21:30:00Z".into(),
            repo: "/tmp/repo".into(),
            findings: vec![
                Finding {
                    tool: "semgrep".into(),
                    rule_id: "r-high".into(),
                    title: "SQL injection".into(),
                    severity: Severity::High,
                    cwe: vec!["CWE-89".into()],
                    owasp: Some("A03:2021".into()),
                    file: "app.py".into(),
                    line: 10,
                    message: "tainted query".into(),
                    remediation: Some("parameterize".into()),
                },
                Finding {
                    tool: "trivy".into(),
                    rule_id: "r-med".into(),
                    title: "outdated dep".into(),
                    severity: Severity::Medium,
                    cwe: vec![],
                    owasp: Some("A06:2021".into()),
                    file: "package-lock.json".into(),
                    line: 0,
                    message: "upgrade".into(),
                    remediation: None,
                },
            ],
            tools_run: vec!["semgrep".into()],
            tools_missing: vec!["trivy".into()],
            tool_errors: vec![],
        }
    }

    #[test]
    fn poll_lists_action_rows_then_reports_newest_first() {
        let dir = tempdir();
        write_report(&dir, &sample_report("20261115T213000Z")).unwrap();
        write_report(&dir, &sample_report("20261116T090000Z")).unwrap();
        let mut c = SecConnector::new(dir.clone());
        let items = c.poll().unwrap();
        assert_eq!(items[0].id, "sec:scan");
        assert_eq!(items[1].id, "sec:hook");
        // Reports follow, newest id first.
        assert_eq!(items[2].id, "sec:report:20261116T090000Z");
        assert_eq!(items[3].id, "sec:report:20261115T213000Z");
        assert!(items[2].expandable);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expand_report_into_groups_then_findings() {
        let dir = tempdir();
        write_report(&dir, &sample_report("20261115T213000Z")).unwrap();
        let mut c = SecConnector::new(dir.clone());
        let groups = c.expand("sec:report:20261115T213000Z").unwrap();
        let labels: Vec<_> = groups.iter().map(|g| g.title.clone()).collect();
        assert!(labels.iter().any(|t| t.starts_with("high (1)")), "{labels:?}");
        assert!(labels.iter().any(|t| t.starts_with("medium (1)")));
        // Expand the high group into its single finding.
        let high_id = groups.iter().find(|g| g.title.starts_with("high")).unwrap().id.clone();
        let findings = c.expand(&high_id).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("SQL injection"));
        assert!(findings[0].actions.iter().any(|a| a.id == "view"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn view_report_returns_summary_and_view_finding_returns_detail() {
        let dir = tempdir();
        write_report(&dir, &sample_report("20261115T213000Z")).unwrap();
        let mut c = SecConnector::new(dir.clone());
        let (title, body) = c.view("sec:report:20261115T213000Z").unwrap();
        assert!(title.contains("20261115T213000Z"));
        assert!(body.contains("By severity"));
        let (ftitle, fbody) = c.view("sec:finding:20261115T213000Z:0").unwrap();
        assert!(ftitle.contains("SQL injection"));
        assert!(fbody.contains("CWE-89"));
        assert!(fbody.contains("A03:2021"));
        assert!(fbody.contains("parameterize"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_reports_dir_still_offers_scan_and_hook() {
        let dir = tempdir();
        let mut c = SecConnector::new(dir.clone());
        let items = c.poll().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "sec:scan");
        assert_eq!(items[1].id, "sec:hook");
        assert!(items[1].actions[0].confirm, "hook install asks for confirmation");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn status_is_connected_with_a_tool_detail() {
        let c = SecConnector::new(std::env::temp_dir());
        assert_eq!(c.status(), Status::Connected);
        assert!(c.detail().is_some());
    }
}
