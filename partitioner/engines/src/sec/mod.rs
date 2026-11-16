//! `gentle-sec` (S35): on-demand and pre-commit security scanning.
//!
//! The engine runs whichever security tools are installed on `PATH`
//! (Semgrep, OSV-Scanner, cargo-audit, Gitleaks, Trivy), normalizes every
//! result into a single [`Finding`] model (tool, rule, severity, CWE, OWASP
//! Top 10 2021 category, location, message, remediation), and writes a report
//! triplet (`report.json` + `report.sarif` + `summary.md`) under
//! `<repo>/.gentle/security/<UTC timestamp>/`.
//!
//! SECURITY: this engine handles untrusted scanner output that may quote secret
//! material (Gitleaks). No raw secret value is ever stored: Gitleaks is run with
//! `--redact`, and every text field that enters a report is additionally passed
//! through [`crate::scan::redact_secrets`] and [`sanitize`] (the T35 control/escape
//! strip) before it is written. All subprocesses run via `std::process::Command`
//! without a shell, with timeouts and output caps; nothing in this engine opens a
//! network connection of its own.
pub mod hook;
pub mod normalize;
pub mod owasp;
pub mod report;
pub mod run;
pub mod sanitize;

use serde::{Deserialize, Serialize};

/// Normalized severity, shared across every tool. `Ord` goes low → high so
/// `>=` compares thresholds for `--fail-on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Lower-case canonical label used in reports and the CLI.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// The SARIF 2.1.0 `level` for this severity (only error/warning/note/none).
    pub fn sarif_level(self) -> &'static str {
        match self {
            Severity::Critical | Severity::High => "error",
            Severity::Medium => "warning",
            Severity::Low => "note",
            Severity::Info => "none",
        }
    }

    /// Parses a free-form severity string from any tool (case-insensitive).
    /// Unknown or empty strings normalize to [`Severity::Info`].
    pub fn parse(s: &str) -> Severity {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" => Severity::Critical,
            "high" | "error" | "severe" => Severity::High,
            "medium" | "moderate" | "warning" | "warn" | "med" => Severity::Medium,
            "low" | "note" | "minor" => Severity::Low,
            _ => Severity::Info,
        }
    }

    /// Parses a `--fail-on` threshold, rejecting anything but the four real
    /// gates (info is not a gate).
    pub fn parse_threshold(s: &str) -> Result<Severity, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Severity::Low),
            "medium" => Ok(Severity::Medium),
            "high" => Ok(Severity::High),
            "critical" => Ok(Severity::Critical),
            other => Err(format!("invalid --fail-on value '{other}' (use low|medium|high|critical)")),
        }
    }

    /// Maps a CVSS v3 base score to a severity band (OSV / cargo-audit).
    pub fn from_cvss(score: f64) -> Severity {
        if score >= 9.0 {
            Severity::Critical
        } else if score >= 7.0 {
            Severity::High
        } else if score >= 4.0 {
            Severity::Medium
        } else if score > 0.0 {
            Severity::Low
        } else {
            Severity::Info
        }
    }
}

/// One normalized finding. Every text field is already redacted and sanitized
/// before a `Finding` is placed in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Producing tool: `semgrep`, `osv-scanner`, `cargo-audit`, `gitleaks`, `trivy`.
    pub tool: String,
    /// Tool rule / advisory id (e.g. `javascript.lang.security...`, `RUSTSEC-2021-0001`).
    pub rule_id: String,
    pub title: String,
    pub severity: Severity,
    /// CWE identifiers in `CWE-NNN` form (may be empty).
    #[serde(default)]
    pub cwe: Vec<String>,
    /// OWASP Top 10 2021 category id (e.g. `A03:2021`), when mappable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owasp: Option<String>,
    /// Repo-relative file path (may be empty for repo-wide findings).
    #[serde(default)]
    pub file: String,
    /// 1-based line, 0 when unknown.
    #[serde(default)]
    pub line: u32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl Finding {
    /// Fills [`Finding::owasp`] from the CWE list via the built-in table when it
    /// is not already set, and sanitizes + redacts every text field. Call this
    /// once, in the normalizer, before the finding reaches a report.
    pub fn finalize(mut self) -> Finding {
        if self.owasp.is_none() {
            self.owasp = owasp::owasp_for_cwes(&self.cwe);
        }
        self.tool = clean(&self.tool);
        self.rule_id = clean(&self.rule_id);
        self.title = clean(&self.title);
        self.file = clean(&self.file);
        self.message = clean(&self.message);
        self.remediation = self.remediation.as_deref().map(clean);
        self.cwe = self.cwe.iter().map(|c| clean(c)).filter(|c| !c.is_empty()).collect();
        self
    }
}

/// Redact inline secrets (via the shared [`crate::scan::redact_secrets`]) then
/// strip control/escape bytes (T35 rules). Order matters: redact first so a
/// secret can never survive, then sanitize so no escape sequence remains.
pub fn clean(s: &str) -> String {
    let (redacted, _) = crate::scan::redact_secrets(s);
    sanitize::sanitize_line(&redacted)
}

/// Multi-line variant of [`clean`] for message bodies that keep newlines.
pub fn clean_multiline(s: &str) -> String {
    let (redacted, _) = crate::scan::redact_secrets(s);
    sanitize::sanitize_display(&redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_parse_normalizes_across_tools() {
        assert_eq!(Severity::parse("CRITICAL"), Severity::Critical);
        assert_eq!(Severity::parse("Error"), Severity::High);
        assert_eq!(Severity::parse("WARNING"), Severity::Medium);
        assert_eq!(Severity::parse("moderate"), Severity::Medium);
        assert_eq!(Severity::parse("note"), Severity::Low);
        assert_eq!(Severity::parse("whatever"), Severity::Info);
        assert_eq!(Severity::parse(""), Severity::Info);
    }

    #[test]
    fn severity_orders_low_to_high_for_thresholds() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Info);
    }

    #[test]
    fn from_cvss_bands() {
        assert_eq!(Severity::from_cvss(9.8), Severity::Critical);
        assert_eq!(Severity::from_cvss(7.5), Severity::High);
        assert_eq!(Severity::from_cvss(5.0), Severity::Medium);
        assert_eq!(Severity::from_cvss(2.0), Severity::Low);
        assert_eq!(Severity::from_cvss(0.0), Severity::Info);
    }

    #[test]
    fn parse_threshold_rejects_info_and_garbage() {
        assert_eq!(Severity::parse_threshold("high").unwrap(), Severity::High);
        assert!(Severity::parse_threshold("info").is_err());
        assert!(Severity::parse_threshold("nonsense").is_err());
    }

    #[test]
    fn finalize_fills_owasp_and_redacts_fields() {
        let f = Finding {
            tool: "gitleaks".into(),
            rule_id: "generic-api-key".into(),
            title: "API key".into(),
            severity: Severity::High,
            cwe: vec!["CWE-79".into()],
            owasp: None,
            file: "src/a.rs".into(),
            line: 3,
            message: "token = ghp_abcdefghijklmnopqrstuvwxyz0123456789".into(),
            remediation: None,
        }
        .finalize();
        assert_eq!(f.owasp.as_deref(), Some("A03:2021"));
        assert!(!f.message.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "secret redacted: {}", f.message);
    }

    #[test]
    fn clean_strips_escape_sequences() {
        let out = clean("rule\x1b]0;pwn\x07 name");
        assert!(!out.contains('\x1b'), "{out}");
        assert!(out.contains("rule"));
    }
}
