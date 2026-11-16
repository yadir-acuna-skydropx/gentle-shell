//! Per-tool normalizers (S35): each turns one tool's parsed JSON output into a
//! `Vec<Finding>` in the shared model. Every normalizer is pure, defensive
//! (missing fields default, never panics) and finishes each finding through
//! [`Finding::finalize`], which maps CWE → OWASP and redacts + sanitizes the
//! text. Fixtures in `tests/` exercise these without any tool installed.
use serde_json::Value;

use super::{Finding, Severity};

/// Pulls `CWE-NNN` tokens from a JSON array of strings or a single string.
fn cwes_from(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Value::Array(a) => {
            for item in a {
                if let Some(s) = item.as_str() {
                    push_cwe(s, &mut out);
                }
            }
        }
        Value::String(s) => push_cwe(s, &mut out),
        _ => {}
    }
    out
}

/// Extracts the leading `CWE-NNN` from a token like `"CWE-79: Cross-site..."`.
fn push_cwe(token: &str, out: &mut Vec<String>) {
    let t = token.trim();
    let head = t.split([':', ' ']).next().unwrap_or(t);
    if let Some(num) = super::owasp::parse_cwe(head) {
        let id = format!("CWE-{num}");
        if !out.contains(&id) {
            out.push(id);
        }
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn u32_at(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

// ---------------------------------------------------------------------------
// Semgrep (`semgrep scan --json`)
// ---------------------------------------------------------------------------

/// Normalizes Semgrep `--json` output. Each `results[]` carries `check_id`,
/// `path`, `start.line` and an `extra` block with `severity`, `message`, `fix`
/// and `metadata` (`cwe`, `owasp`).
pub fn normalize_semgrep(v: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(results) = v.get("results").and_then(Value::as_array) else {
        return out;
    };
    for r in results {
        let extra = r.get("extra").cloned().unwrap_or(Value::Null);
        let meta = extra.get("metadata").cloned().unwrap_or(Value::Null);
        let severity = Severity::parse(str_at(&extra, "severity"));
        let cwe = cwes_from(meta.get("cwe").unwrap_or(&Value::Null));
        // Semgrep also ships an explicit OWASP tag; prefer it when present.
        let owasp = meta
            .get("owasp")
            .and_then(first_string)
            .and_then(|s| owasp_id(&s));
        let rule_id = str_at(r, "check_id");
        let message = str_at(&extra, "message");
        let fix = extra.get("fix").and_then(Value::as_str).filter(|s| !s.is_empty());
        out.push(
            Finding {
                tool: "semgrep".into(),
                rule_id: rule_id.into(),
                title: rule_short_title(rule_id, message),
                severity,
                cwe,
                owasp,
                file: str_at(r, "path").into(),
                line: r.get("start").map(|s| u32_at(s, "line")).unwrap_or(0),
                message: message.into(),
                remediation: fix.map(|s| format!("Suggested fix: {s}")),
            }
            .finalize(),
        );
    }
    out
}

fn first_string(v: &Value) -> Option<String> {
    match v {
        Value::Array(a) => a.first().and_then(Value::as_str).map(str::to_string),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Extracts an `A03:2021` id from a Semgrep OWASP tag like `"A03:2021 - Injection"`.
fn owasp_id(tag: &str) -> Option<String> {
    let head = tag.trim().split_whitespace().next().unwrap_or("");
    if head.len() >= 7 && head.as_bytes()[0] == b'A' && head.contains(":2021") {
        Some(head.trim_end_matches(['-', ' ']).to_string())
    } else {
        None
    }
}

/// A short title from a rule id (last dotted segment) with the message as a hint.
fn rule_short_title(rule_id: &str, message: &str) -> String {
    let short = rule_id.rsplit('.').next().unwrap_or(rule_id);
    if short.is_empty() {
        message.chars().take(80).collect()
    } else {
        short.replace(['-', '_'], " ")
    }
}

// ---------------------------------------------------------------------------
// OSV-Scanner (`osv-scanner --format json`)
// ---------------------------------------------------------------------------

/// Normalizes OSV-Scanner JSON. Shape: `results[].packages[].vulnerabilities[]`
/// with the lockfile under `results[].source.path`.
pub fn normalize_osv(v: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(results) = v.get("results").and_then(Value::as_array) else {
        return out;
    };
    for res in results {
        let source = res.get("source").map(|s| str_at(s, "path")).unwrap_or("").to_string();
        let Some(packages) = res.get("packages").and_then(Value::as_array) else {
            continue;
        };
        for pkg in packages {
            let pkg_name = pkg.get("package").map(|p| str_at(p, "name")).unwrap_or("");
            let Some(vulns) = pkg.get("vulnerabilities").and_then(Value::as_array) else {
                continue;
            };
            for vuln in vulns {
                let id = str_at(vuln, "id");
                let summary = {
                    let s = str_at(vuln, "summary");
                    if s.is_empty() { str_at(vuln, "details") } else { s }
                };
                let db = vuln.get("database_specific").cloned().unwrap_or(Value::Null);
                let severity = osv_severity(vuln, &db);
                let cwe = cwes_from(db.get("cwe_ids").unwrap_or(&Value::Null));
                out.push(
                    Finding {
                        tool: "osv-scanner".into(),
                        rule_id: id.into(),
                        title: format!("{pkg_name}: {id}"),
                        severity,
                        cwe,
                        owasp: None,
                        file: source.clone(),
                        line: 0,
                        message: summary.into(),
                        remediation: Some(format!("Update {pkg_name} to a fixed version")),
                    }
                    .finalize(),
                );
            }
        }
    }
    out
}

/// OSV severity: prefer `database_specific.severity` text, else a CVSS_V3 score.
fn osv_severity(vuln: &Value, db: &Value) -> Severity {
    let text = str_at(db, "severity");
    if !text.is_empty() {
        return Severity::parse(text);
    }
    if let Some(arr) = vuln.get("severity").and_then(Value::as_array) {
        for s in arr {
            if let Some(score) = cvss_base_score(str_at(s, "score")) {
                return Severity::from_cvss(score);
            }
        }
    }
    Severity::Medium
}

/// Pulls the base-score float out of a CVSS vector if it is suffixed (rare), or
/// parses a bare numeric score string. Returns `None` for a pure vector string.
fn cvss_base_score(score: &str) -> Option<f64> {
    score.trim().parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// cargo-audit (`cargo audit --json`)
// ---------------------------------------------------------------------------

/// Normalizes cargo-audit JSON. Vulnerabilities become High (RUSTSEC advisories)
/// and warnings (yanked/unmaintained) become Low. `lockfile` is the Cargo.lock
/// the audit ran against (cargo-audit does not echo a file path).
pub fn normalize_cargo_audit(v: &Value, lockfile: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(list) = v.get("vulnerabilities").and_then(|x| x.get("list")).and_then(Value::as_array) {
        for item in list {
            let adv = item.get("advisory").cloned().unwrap_or(Value::Null);
            let pkg = item.get("package").map(|p| str_at(p, "name")).unwrap_or("");
            let id = str_at(&adv, "id");
            let patched = item
                .get("versions")
                .and_then(|ver| ver.get("patched"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                .filter(|s| !s.is_empty());
            out.push(
                Finding {
                    tool: "cargo-audit".into(),
                    rule_id: id.into(),
                    title: format!("{pkg}: {}", str_at(&adv, "title")),
                    severity: Severity::High,
                    cwe: cwes_from(adv.get("cwe").unwrap_or(&Value::Null)),
                    owasp: Some("A06:2021".into()), // vulnerable/outdated component
                    file: lockfile.into(),
                    line: 0,
                    message: str_at(&adv, "description").into(),
                    remediation: patched.map(|p| format!("Upgrade {pkg} to {p}")),
                }
                .finalize(),
            );
        }
    }
    // Warnings: unmaintained, yanked, unsound.
    if let Some(warnings) = v.get("warnings").and_then(Value::as_object) {
        for (kind, arr) in warnings {
            let Some(items) = arr.as_array() else { continue };
            for item in items {
                let adv = item.get("advisory").cloned().unwrap_or(Value::Null);
                let pkg = item.get("package").map(|p| str_at(p, "name")).unwrap_or("");
                out.push(
                    Finding {
                        tool: "cargo-audit".into(),
                        rule_id: {
                            let id = str_at(&adv, "id");
                            if id.is_empty() { format!("cargo-audit-{kind}") } else { id.into() }
                        },
                        title: format!("{pkg}: {kind}"),
                        severity: Severity::Low,
                        cwe: vec![],
                        owasp: Some("A06:2021".into()),
                        file: lockfile.into(),
                        line: 0,
                        message: str_at(&adv, "title").into(),
                        remediation: None,
                    }
                    .finalize(),
                );
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Gitleaks (`gitleaks ... --report-format json`)
// ---------------------------------------------------------------------------

/// Normalizes Gitleaks JSON (a top-level array of findings). Secrets are always
/// High and mapped to CWE-798 / A07. Gitleaks is run with `--redact`, and the
/// message is additionally redacted by [`Finding::finalize`].
pub fn normalize_gitleaks(v: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(arr) = v.as_array() else {
        return out;
    };
    for f in arr {
        let rule = str_at(f, "RuleID");
        let desc = str_at(f, "Description");
        out.push(
            Finding {
                tool: "gitleaks".into(),
                rule_id: rule.into(),
                title: if desc.is_empty() { format!("secret: {rule}") } else { desc.into() },
                severity: Severity::High,
                cwe: vec!["CWE-798".into()],
                owasp: None, // filled to A07 by finalize via the CWE
                file: str_at(f, "File").into(),
                line: u32_at(f, "StartLine"),
                // NEVER include the Secret/Match fields — only the rule name.
                message: format!("Potential secret detected by rule '{rule}' (value redacted)"),
                remediation: Some("Remove the secret, rotate it, and store it in a secret manager".into()),
            }
            .finalize(),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Trivy (`trivy fs --format json`)
// ---------------------------------------------------------------------------

/// Normalizes Trivy `fs` JSON across its three result classes: package
/// vulnerabilities, misconfigurations and secrets.
pub fn normalize_trivy(v: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(results) = v.get("Results").and_then(Value::as_array) else {
        return out;
    };
    for res in results {
        let target = str_at(res, "Target");
        if let Some(vulns) = res.get("Vulnerabilities").and_then(Value::as_array) {
            for vuln in vulns {
                let id = str_at(vuln, "VulnerabilityID");
                let pkg = str_at(vuln, "PkgName");
                let fixed = vuln.get("FixedVersion").and_then(Value::as_str).filter(|s| !s.is_empty());
                out.push(
                    Finding {
                        tool: "trivy".into(),
                        rule_id: id.into(),
                        title: format!("{pkg}: {id}"),
                        severity: Severity::parse(str_at(vuln, "Severity")),
                        cwe: cwes_from(vuln.get("CweIDs").unwrap_or(&Value::Null)),
                        owasp: Some("A06:2021".into()),
                        file: target.into(),
                        line: 0,
                        message: {
                            let t = str_at(vuln, "Title");
                            if t.is_empty() { str_at(vuln, "Description") } else { t }.into()
                        },
                        remediation: fixed.map(|f| format!("Upgrade {pkg} to {f}")),
                    }
                    .finalize(),
                );
            }
        }
        if let Some(miscs) = res.get("Misconfigurations").and_then(Value::as_array) {
            for m in miscs {
                let line = m
                    .get("CauseMetadata")
                    .map(|c| u32_at(c, "StartLine"))
                    .unwrap_or(0);
                let resolution = m.get("Resolution").and_then(Value::as_str).filter(|s| !s.is_empty());
                out.push(
                    Finding {
                        tool: "trivy".into(),
                        rule_id: str_at(m, "ID").into(),
                        title: str_at(m, "Title").into(),
                        severity: Severity::parse(str_at(m, "Severity")),
                        cwe: vec![],
                        owasp: Some("A05:2021".into()), // misconfiguration
                        file: target.into(),
                        line,
                        message: str_at(m, "Description").into(),
                        remediation: resolution.map(str::to_string),
                    }
                    .finalize(),
                );
            }
        }
        if let Some(secrets) = res.get("Secrets").and_then(Value::as_array) {
            for s in secrets {
                let rule = str_at(s, "RuleID");
                out.push(
                    Finding {
                        tool: "trivy".into(),
                        rule_id: rule.into(),
                        title: {
                            let t = str_at(s, "Title");
                            if t.is_empty() { format!("secret: {rule}") } else { t.into() }
                        },
                        severity: Severity::parse(str_at(s, "Severity")).max(Severity::High),
                        cwe: vec!["CWE-798".into()],
                        owasp: None,
                        file: target.into(),
                        line: u32_at(s, "StartLine"),
                        message: format!("Potential secret detected by rule '{rule}' (value redacted)"),
                        remediation: Some("Remove the secret and rotate it".into()),
                    }
                    .finalize(),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semgrep_maps_cwe_severity_and_owasp() {
        let v = json!({
            "results": [{
                "check_id": "python.lang.security.audit.dangerous-exec",
                "path": "app.py",
                "start": {"line": 12},
                "extra": {
                    "severity": "ERROR",
                    "message": "Detected use of exec()",
                    "fix": "use ast.literal_eval",
                    "metadata": {"cwe": ["CWE-95: Eval Injection"], "owasp": ["A03:2021 - Injection"]}
                }
            }]
        });
        let f = normalize_semgrep(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].cwe, vec!["CWE-95"]);
        assert_eq!(f[0].owasp.as_deref(), Some("A03:2021"));
        assert_eq!(f[0].file, "app.py");
        assert_eq!(f[0].line, 12);
        assert!(f[0].remediation.as_deref().unwrap().contains("literal_eval"));
    }

    #[test]
    fn osv_reads_package_vulns_and_db_severity() {
        let v = json!({
            "results": [{
                "source": {"path": "Cargo.lock"},
                "packages": [{
                    "package": {"name": "time", "ecosystem": "crates.io"},
                    "vulnerabilities": [{
                        "id": "RUSTSEC-2020-0071",
                        "summary": "Potential segfault in the time crate",
                        "database_specific": {"severity": "HIGH", "cwe_ids": ["CWE-125"]}
                    }]
                }]
            }]
        });
        let f = normalize_osv(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].file, "Cargo.lock");
        assert_eq!(f[0].cwe, vec!["CWE-125"]);
        assert!(f[0].title.contains("time"));
    }

    #[test]
    fn cargo_audit_vulns_are_high_warnings_low() {
        let v = json!({
            "vulnerabilities": {"list": [{
                "advisory": {"id": "RUSTSEC-2021-0001", "title": "flaw", "description": "bad"},
                "package": {"name": "foo", "version": "1.0.0"},
                "versions": {"patched": [">=1.0.1"]}
            }]},
            "warnings": {"unmaintained": [{
                "advisory": {"id": "RUSTSEC-2020-0036", "title": "unmaintained"},
                "package": {"name": "bar", "version": "0.1.0"}
            }]}
        });
        let f = normalize_cargo_audit(&v, "Cargo.lock");
        assert_eq!(f.len(), 2);
        let vuln = f.iter().find(|x| x.rule_id == "RUSTSEC-2021-0001").unwrap();
        assert_eq!(vuln.severity, Severity::High);
        assert_eq!(vuln.owasp.as_deref(), Some("A06:2021"));
        assert!(vuln.remediation.as_deref().unwrap().contains("1.0.1"));
        let warn = f.iter().find(|x| x.rule_id == "RUSTSEC-2020-0036").unwrap();
        assert_eq!(warn.severity, Severity::Low);
    }

    #[test]
    fn gitleaks_never_includes_the_secret_value() {
        let v = json!([{
            "RuleID": "aws-access-token",
            "Description": "AWS Access Key",
            "File": "config.yml",
            "StartLine": 7,
            "Secret": "AKIAIOSFODNN7EXAMPLE",
            "Match": "aws_key = AKIAIOSFODNN7EXAMPLE"
        }]);
        let f = normalize_gitleaks(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].owasp.as_deref(), Some("A07:2021"));
        assert!(!f[0].message.contains("AKIAIOSFODNN7EXAMPLE"), "secret leaked: {}", f[0].message);
        assert!(!f[0].title.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn trivy_handles_vulns_misconfig_and_secrets() {
        let v = json!({
            "Results": [{
                "Target": "package-lock.json",
                "Vulnerabilities": [{
                    "VulnerabilityID": "CVE-2021-1234", "PkgName": "lodash",
                    "Severity": "CRITICAL", "Title": "proto pollution",
                    "CweIDs": ["CWE-1321"], "FixedVersion": "4.17.21"
                }],
                "Misconfigurations": [{
                    "ID": "DS002", "Title": "root user", "Severity": "HIGH",
                    "Description": "runs as root", "Resolution": "add USER",
                    "CauseMetadata": {"StartLine": 3}
                }],
                "Secrets": [{
                    "RuleID": "github-pat", "Severity": "CRITICAL", "Title": "GitHub PAT",
                    "StartLine": 2, "Match": "token=ghp_xxx"
                }]
            }]
        });
        let f = normalize_trivy(&v);
        assert_eq!(f.len(), 3);
        let vuln = f.iter().find(|x| x.rule_id == "CVE-2021-1234").unwrap();
        assert_eq!(vuln.severity, Severity::Critical);
        assert_eq!(vuln.remediation.as_deref().unwrap(), "Upgrade lodash to 4.17.21");
        let misc = f.iter().find(|x| x.rule_id == "DS002").unwrap();
        assert_eq!(misc.owasp.as_deref(), Some("A05:2021"));
        assert_eq!(misc.line, 3);
        let secret = f.iter().find(|x| x.rule_id == "github-pat").unwrap();
        assert!(!secret.message.contains("ghp_xxx"));
        assert_eq!(secret.owasp.as_deref(), Some("A07:2021"));
    }

    #[test]
    fn empty_or_missing_results_yield_nothing() {
        assert!(normalize_semgrep(&json!({})).is_empty());
        assert!(normalize_osv(&json!({"results": []})).is_empty());
        assert!(normalize_trivy(&json!({"Results": null})).is_empty());
        assert!(normalize_gitleaks(&json!({})).is_empty());
    }
}
