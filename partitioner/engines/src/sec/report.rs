//! Report model and persistence (S35).
//!
//! A [`Report`] is written as a triplet under `<repo>/.gentle/security/<id>/`:
//! `report.json` (the full model), `report.sarif` (SARIF 2.1.0) and
//! `summary.md` (counts by severity/OWASP, top findings, tools run/missing).
//! `.gentle/` is added to the repo's `.git/info/exclude` so a report is never
//! committed. [`list_reports`] and [`show_report`] back the `list`/`show`
//! subcommands and the apps connector.
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{owasp, Finding, Severity};

/// One tool that ran but errored (e.g. no network, a parse failure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub tool: String,
    pub message: String,
}

/// A full scan report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Directory id (the UTC timestamp, e.g. `20261115T213000Z`).
    pub id: String,
    /// ISO-8601 UTC timestamp (`2026-11-15T21:30:00Z`).
    pub generated_at: String,
    /// Absolute repo path the scan ran against.
    pub repo: String,
    pub findings: Vec<Finding>,
    /// Tools found on PATH and run.
    pub tools_run: Vec<String>,
    /// Tools not installed (reported, never fatal).
    pub tools_missing: Vec<String>,
    /// Tools that ran but failed.
    #[serde(default)]
    pub tool_errors: Vec<ToolError>,
}

impl Report {
    /// Count of findings at each severity (0s included), high → low ordering
    /// is applied by the caller.
    pub fn severity_counts(&self) -> BTreeMap<Severity, usize> {
        let mut m = BTreeMap::new();
        for s in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low, Severity::Info] {
            m.insert(s, 0);
        }
        for f in &self.findings {
            *m.entry(f.severity).or_insert(0) += 1;
        }
        m
    }

    /// Count of findings per OWASP category id (unmapped grouped under `"-"`).
    pub fn owasp_counts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for f in &self.findings {
            let key = f.owasp.clone().unwrap_or_else(|| "-".to_string());
            *m.entry(key).or_insert(0) += 1;
        }
        m
    }

    /// The highest severity present, or `Info` when there are no findings.
    pub fn max_severity(&self) -> Severity {
        self.findings.iter().map(|f| f.severity).max().unwrap_or(Severity::Info)
    }

    /// Whether any finding is at or above `threshold` (for `--fail-on`).
    pub fn fails(&self, threshold: Severity) -> bool {
        self.findings.iter().any(|f| f.severity >= threshold)
    }

    /// The report's `report.json` text.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// The SARIF 2.1.0 document.
    pub fn to_sarif(&self) -> Value {
        let mut rules: Vec<Value> = Vec::new();
        let mut rule_index: BTreeMap<String, usize> = BTreeMap::new();
        let mut results: Vec<Value> = Vec::new();
        for f in &self.findings {
            let key = format!("{}/{}", f.tool, f.rule_id);
            let idx = *rule_index.entry(key.clone()).or_insert_with(|| {
                let i = rules.len();
                rules.push(json!({
                    "id": key,
                    "name": f.rule_id,
                    "shortDescription": {"text": f.title},
                    "properties": {
                        "tags": f.cwe.clone(),
                        "tool": f.tool,
                    }
                }));
                i
            });
            let mut region = json!({});
            if f.line > 0 {
                region = json!({"startLine": f.line});
            }
            results.push(json!({
                "ruleId": key,
                "ruleIndex": idx,
                "level": f.severity.sarif_level(),
                "message": {"text": f.message},
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": {"uri": f.file},
                        "region": region
                    }
                }],
                "properties": {
                    "tool": f.tool,
                    "severity": f.severity.label(),
                    "cwe": f.cwe,
                    "owasp": f.owasp,
                }
            }));
        }
        json!({
            "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {
                    "name": "gentle-sec",
                    "informationUri": "https://github.com/",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules
                }},
                "results": results
            }]
        })
    }

    /// The `summary.md` text (counts, OWASP breakdown, top findings, tools).
    pub fn to_summary_md(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("# Security scan {}\n\n", self.id));
        s.push_str(&format!("- Repo: `{}`\n", self.repo));
        s.push_str(&format!("- Generated: {}\n", self.generated_at));
        s.push_str(&format!("- Findings: {}\n", self.findings.len()));
        s.push_str(&format!("- Highest severity: {}\n\n", self.max_severity().label()));

        s.push_str("## By severity\n\n");
        let counts = self.severity_counts();
        for sev in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low, Severity::Info] {
            s.push_str(&format!("- {}: {}\n", sev.label(), counts.get(&sev).copied().unwrap_or(0)));
        }
        s.push('\n');

        s.push_str("## By OWASP Top 10 2021\n\n");
        let owasp_counts = self.owasp_counts();
        if owasp_counts.is_empty() {
            s.push_str("- (none)\n");
        } else {
            for (cat, n) in &owasp_counts {
                let title = owasp::category_title(cat).unwrap_or("Unmapped");
                s.push_str(&format!("- {cat} {title}: {n}\n"));
            }
        }
        s.push('\n');

        s.push_str("## Top findings\n\n");
        let mut top: Vec<&Finding> = self.findings.iter().collect();
        top.sort_by(|a, b| b.severity.cmp(&a.severity));
        if top.is_empty() {
            s.push_str("No findings. 🎉\n\n");
        } else {
            for f in top.iter().take(20) {
                let loc = if f.line > 0 {
                    format!("{}:{}", f.file, f.line)
                } else if !f.file.is_empty() {
                    f.file.clone()
                } else {
                    "-".to_string()
                };
                let owasp = f.owasp.as_deref().unwrap_or("-");
                s.push_str(&format!(
                    "- **[{}]** `{}` {} — {} ({}, {})\n",
                    f.severity.label(),
                    f.tool,
                    f.title,
                    loc,
                    f.rule_id,
                    owasp
                ));
            }
            s.push('\n');
        }

        s.push_str("## Tools\n\n");
        s.push_str(&format!(
            "- Run: {}\n",
            if self.tools_run.is_empty() { "(none)".into() } else { self.tools_run.join(", ") }
        ));
        s.push_str(&format!(
            "- Not installed: {}\n",
            if self.tools_missing.is_empty() { "(none)".into() } else { self.tools_missing.join(", ") }
        ));
        for e in &self.tool_errors {
            s.push_str(&format!("- Error ({}): {}\n", e.tool, e.message));
        }
        s
    }
}

/// The security reports directory for a repo: `<repo>/.gentle/security`.
pub fn reports_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".gentle").join("security")
}

/// Writes the three report files under `reports_dir/<id>/` and ensures
/// `.gentle/` is excluded from git. Returns the report directory.
pub fn write_report(repo_root: &Path, report: &Report) -> io::Result<PathBuf> {
    let dir = reports_dir(repo_root).join(&report.id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("report.json"), report.to_json())?;
    std::fs::write(dir.join("report.sarif"), serde_json::to_string_pretty(&report.to_sarif()).unwrap_or_default())?;
    std::fs::write(dir.join("summary.md"), report.to_summary_md())?;
    ensure_git_exclude(repo_root);
    Ok(dir)
}

/// Adds `.gentle/` to `<repo>/.git/info/exclude` if a `.git` dir exists and the
/// entry is not already present. Best-effort: a missing `.git` (not a repo, or a
/// worktree with a `.git` file) is silently skipped.
pub fn ensure_git_exclude(repo_root: &Path) {
    let git = repo_root.join(".git");
    if !git.is_dir() {
        return;
    }
    let info = git.join("info");
    let exclude = info.join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ".gentle/" || l.trim() == "/.gentle/") {
        return;
    }
    let _ = std::fs::create_dir_all(&info);
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".gentle/\n");
    let _ = std::fs::write(&exclude, next);
}

/// Lightweight listing entry for a report directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportMeta {
    pub id: String,
    pub generated_at: String,
    pub findings: usize,
    pub counts: BTreeMap<Severity, usize>,
}

impl ReportMeta {
    /// A one-line label: `2026-11-15 21:30 · 3 high · 5 medium` (non-zero bands).
    pub fn label(&self) -> String {
        let when = self.generated_at.replace('T', " ").trim_end_matches('Z').to_string();
        let when = when.get(0..16).unwrap_or(&when).to_string();
        let mut parts: Vec<String> = Vec::new();
        for sev in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low] {
            let n = self.counts.get(&sev).copied().unwrap_or(0);
            if n > 0 {
                parts.push(format!("{n} {}", sev.label()));
            }
        }
        if parts.is_empty() {
            format!("{when} · clean")
        } else {
            format!("{when} · {}", parts.join(" · "))
        }
    }
}

/// Lists reports newest-first by reading each `report.json`. Directories without
/// a readable report are skipped.
pub fn list_reports(repo_root: &Path) -> Vec<ReportMeta> {
    let dir = reports_dir(repo_root);
    let mut metas: Vec<ReportMeta> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return metas;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let json_path = entry.path().join("report.json");
        let Ok(text) = std::fs::read_to_string(&json_path) else {
            continue;
        };
        let Ok(report) = serde_json::from_str::<Report>(&text) else {
            continue;
        };
        metas.push(ReportMeta {
            id: report.id.clone(),
            generated_at: report.generated_at.clone(),
            findings: report.findings.len(),
            counts: report.severity_counts(),
        });
    }
    // Newest first: the id is a sortable UTC timestamp.
    metas.sort_by(|a, b| b.id.cmp(&a.id));
    metas
}

/// Rejects any report id that is not a single plain directory name, so `show`,
/// `load` and the Ciberseguridad connector can never read outside the reports
/// dir (no `/`, `\`, `..`, leading `.`, empty or control characters).
fn checked_report_dir(repo_root: &Path, id: &str) -> io::Result<std::path::PathBuf> {
    let ok = !id.is_empty()
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && {
            let mut comps = Path::new(id).components();
            matches!(comps.next(), Some(std::path::Component::Normal(_))) && comps.next().is_none()
        };
    if !ok {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid report id"));
    }
    Ok(reports_dir(repo_root).join(id))
}

/// Reads one report's full model by id.
pub fn load_report(repo_root: &Path, id: &str) -> io::Result<Report> {
    let path = checked_report_dir(repo_root, id)?.join("report.json");
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Reads one report's `summary.md` by id.
pub fn show_report(repo_root: &Path, id: &str) -> io::Result<String> {
    let path = checked_report_dir(repo_root, id)?.join("summary.md");
    std::fs::read_to_string(path)
}

// ---------------------------------------------------------------------------
// UTC timestamp helpers (no chrono dependency)
// ---------------------------------------------------------------------------

/// The current report id (`YYYYMMDDThhmmssZ`) and ISO timestamp
/// (`YYYY-MM-DDThh:mm:ssZ`) from the system clock.
pub fn now_ids() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    ids_from_unix(secs)
}

/// Deterministic form of [`now_ids`] for tests.
pub fn ids_from_unix(secs: i64) -> (String, String) {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    let id = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
    let iso = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    (id, iso)
}

/// Converts Unix seconds to civil UTC `(year, month, day, hour, min, sec)`
/// via Howard Hinnant's algorithm. Valid for the proleptic Gregorian calendar.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: Severity, cwe: &str, owasp: Option<&str>) -> Finding {
        Finding {
            tool: "semgrep".into(),
            rule_id: "r1".into(),
            title: "t".into(),
            severity: sev,
            cwe: if cwe.is_empty() { vec![] } else { vec![cwe.into()] },
            owasp: owasp.map(str::to_string),
            file: "a.rs".into(),
            line: 5,
            message: "m".into(),
            remediation: None,
        }
    }

    fn report(findings: Vec<Finding>) -> Report {
        Report {
            id: "20261115T213000Z".into(),
            generated_at: "2026-11-15T21:30:00Z".into(),
            repo: "/tmp/repo".into(),
            findings,
            tools_run: vec!["semgrep".into()],
            tools_missing: vec!["trivy".into()],
            tool_errors: vec![],
        }
    }

    #[test]
    fn severity_counts_and_fails() {
        let r = report(vec![
            finding(Severity::High, "CWE-79", Some("A03:2021")),
            finding(Severity::Medium, "", None),
            finding(Severity::High, "CWE-89", Some("A03:2021")),
        ]);
        let c = r.severity_counts();
        assert_eq!(c[&Severity::High], 2);
        assert_eq!(c[&Severity::Medium], 1);
        assert_eq!(c[&Severity::Critical], 0);
        assert!(r.fails(Severity::High));
        assert!(!r.fails(Severity::Critical));
        assert_eq!(r.max_severity(), Severity::High);
    }

    #[test]
    fn sarif_shape_is_2_1_0_with_rules_and_results() {
        let r = report(vec![finding(Severity::Critical, "CWE-79", Some("A03:2021"))]);
        let sarif = r.to_sarif();
        assert_eq!(sarif["version"], "2.1.0");
        assert_eq!(sarif["runs"][0]["tool"]["driver"]["name"], "gentle-sec");
        let res = &sarif["runs"][0]["results"][0];
        assert_eq!(res["level"], "error");
        assert_eq!(res["locations"][0]["physicalLocation"]["region"]["startLine"], 5);
        assert_eq!(res["properties"]["owasp"], "A03:2021");
        assert_eq!(sarif["runs"][0]["tool"]["driver"]["rules"][0]["name"], "r1");
    }

    #[test]
    fn summary_md_lists_counts_owasp_and_tools() {
        let r = report(vec![finding(Severity::High, "CWE-79", Some("A03:2021"))]);
        let md = r.to_summary_md();
        assert!(md.contains("# Security scan 20261115T213000Z"));
        assert!(md.contains("high: 1"));
        assert!(md.contains("A03:2021 Injection: 1"));
        assert!(md.contains("Run: semgrep"));
        assert!(md.contains("Not installed: trivy"));
    }

    #[test]
    fn report_meta_label_shows_nonzero_bands() {
        let meta = ReportMeta {
            id: "20261115T213000Z".into(),
            generated_at: "2026-11-15T21:30:00Z".into(),
            findings: 8,
            counts: {
                let mut m = BTreeMap::new();
                m.insert(Severity::High, 3);
                m.insert(Severity::Medium, 5);
                m
            },
        };
        assert_eq!(meta.label(), "2026-11-15 21:30 · 3 high · 5 medium");
    }

    #[test]
    fn clean_report_label() {
        let meta = ReportMeta {
            id: "20261115T213000Z".into(),
            generated_at: "2026-11-15T21:30:00Z".into(),
            findings: 0,
            counts: BTreeMap::new(),
        };
        assert_eq!(meta.label(), "2026-11-15 21:30 · clean");
    }

    #[test]
    fn ids_from_unix_is_utc() {
        // 2026-11-15T21:30:00Z = 1794785400 - 2h = 1794778200
        let (id, iso) = ids_from_unix(1_794_778_200);
        assert_eq!(iso, "2026-11-15T21:30:00Z");
        assert_eq!(id, "20261115T213000Z");
        // Epoch.
        let (_, iso0) = ids_from_unix(0);
        assert_eq!(iso0, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn write_report_creates_triplet_and_git_exclude() {
        let base = std::env::temp_dir().join(format!("gsec-wr-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(base.join(".git/info")).unwrap();
        let r = report(vec![finding(Severity::High, "CWE-79", Some("A03:2021"))]);
        let dir = write_report(&base, &r).unwrap();
        assert!(dir.join("report.json").is_file());
        assert!(dir.join("report.sarif").is_file());
        assert!(dir.join("summary.md").is_file());
        let excl = std::fs::read_to_string(base.join(".git/info/exclude")).unwrap();
        assert!(excl.contains(".gentle/"), "{excl}");
        // Idempotent: a second write does not duplicate the exclude line.
        write_report(&base, &r).unwrap();
        let excl2 = std::fs::read_to_string(base.join(".git/info/exclude")).unwrap();
        assert_eq!(excl2.matches(".gentle/").count(), 1);
        // list + show round-trip.
        let metas = list_reports(&base);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, r.id);
        assert!(show_report(&base, &r.id).unwrap().contains("# Security scan"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn show_and_load_reject_path_traversal_ids() {
        let base = std::env::temp_dir().join(format!("gsec-trav-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(reports_dir(&base)).unwrap();
        // A summary.md one level above the reports dir must NOT be reachable.
        std::fs::create_dir_all(base.join(".gentle/other")).unwrap();
        std::fs::write(base.join(".gentle/other/summary.md"), "OUTSIDE").unwrap();
        assert!(show_report(&base, "../other").is_err(), "traversal must be rejected");
        assert!(load_report(&base, "../../etc").is_err());
        assert!(show_report(&base, "a/b").is_err());
        assert!(show_report(&base, "..").is_err());
        assert!(show_report(&base, "").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
