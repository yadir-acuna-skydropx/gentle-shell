//! Scan orchestration (S35): detects which security tools are on PATH, runs
//! each installed one via `std::process::Command` (no shell) with a timeout and
//! an output cap, normalizes the results, and assembles a [`Report`]. A missing
//! tool is reported, never fatal; a tool that errors (e.g. offline) is recorded
//! in `tool_errors`. Nothing here opens a network connection of its own.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::normalize;
use super::report::{self, Report, ToolError};
use super::Finding;

/// The tools gentle-sec knows how to run, in report order.
pub const TOOLS: &[&str] = &["semgrep", "osv-scanner", "cargo-audit", "gitleaks", "trivy"];

/// Default per-tool timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Hard cap on a tool's captured stdout (64 MiB).
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Cap on Cargo.lock files audited per scan.
const MAX_LOCKFILES: usize = 20;

/// Options for one scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// The directory to scan (defaults to the repo root).
    pub path: PathBuf,
    /// Scan only staged files (pre-commit).
    pub staged: bool,
    pub timeout: Duration,
}

impl ScanOptions {
    pub fn new(path: PathBuf) -> Self {
        ScanOptions { path, staged: false, timeout: DEFAULT_TIMEOUT }
    }
}

/// True when `name` is an executable on PATH (honouring PATHEXT-less Unix).
pub fn tool_available(name: &str) -> bool {
    tool_available_in(name, std::env::var_os("PATH").as_deref())
}

/// [`tool_available`] against an explicit PATH value (injected for tests).
pub fn tool_available_in(name: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

/// Splits [`TOOLS`] into (available, missing) by PATH lookup.
pub fn detect_tools() -> (Vec<String>, Vec<String>) {
    detect_tools_in(std::env::var_os("PATH").as_deref())
}

/// [`detect_tools`] against an explicit PATH value (injected for tests).
pub fn detect_tools_in(path: Option<&std::ffi::OsStr>) -> (Vec<String>, Vec<String>) {
    let mut run = Vec::new();
    let mut missing = Vec::new();
    for t in TOOLS {
        if tool_available_in(t, path) {
            run.push((*t).to_string());
        } else {
            missing.push((*t).to_string());
        }
    }
    (run, missing)
}

/// Runs a scan and assembles the report (not yet written to disk).
pub fn scan(opts: &ScanOptions) -> Report {
    let (available, missing) = detect_tools();
    scan_detected(opts, available, missing)
}

/// [`scan`] with the tool split already decided (injected for tests).
pub fn scan_detected(opts: &ScanOptions, available: Vec<String>, missing: Vec<String>) -> Report {
    let mut findings: Vec<Finding> = Vec::new();
    let mut errors: Vec<ToolError> = Vec::new();

    let staged_files = if opts.staged { staged_files(&opts.path) } else { Vec::new() };

    for tool in &available {
        let result = match tool.as_str() {
            "semgrep" => run_semgrep(opts, &staged_files),
            "osv-scanner" => run_osv(opts),
            "cargo-audit" => run_cargo_audit(opts),
            "gitleaks" => run_gitleaks(opts),
            "trivy" => run_trivy(opts),
            _ => continue,
        };
        match result {
            Ok(mut fs) => findings.append(&mut fs),
            Err(e) => errors.push(ToolError { tool: tool.clone(), message: super::clean(&e) }),
        }
    }

    // Stable, useful ordering: severity desc, then tool, then file.
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.tool.cmp(&b.tool))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    let (id, iso) = report::now_ids();
    Report {
        id,
        generated_at: iso,
        repo: opts.path.display().to_string(),
        findings,
        tools_run: available,
        tools_missing: missing,
        tool_errors: errors,
    }
}

// ---------------------------------------------------------------------------
// Per-tool runners
// ---------------------------------------------------------------------------

fn run_semgrep(opts: &ScanOptions, staged: &[String]) -> Result<Vec<Finding>, String> {
    let mut cmd = Command::new("semgrep");
    cmd.arg("scan")
        .arg("--json")
        .arg("--quiet")
        .arg("--config")
        .arg("p/owasp-top-ten")
        .arg("--config")
        .arg("p/cwe-top-25")
        .current_dir(&opts.path);
    if opts.staged {
        if staged.is_empty() {
            return Ok(Vec::new());
        }
        for f in staged {
            cmd.arg(f);
        }
    } else {
        cmd.arg(".");
    }
    let out = run_capped(cmd, opts.timeout)?;
    let v = parse_json(&out.stdout, &out.stderr, "semgrep")?;
    Ok(normalize::normalize_semgrep(&v))
}

fn run_osv(opts: &ScanOptions) -> Result<Vec<Finding>, String> {
    let mut cmd = Command::new("osv-scanner");
    cmd.args(["scan", "source", "-r", "--format", "json"]).arg(".").current_dir(&opts.path);
    let out = run_capped(cmd, opts.timeout)?;
    // OSV exits non-zero when vulns are found; JSON on stdout is still valid.
    let v = parse_json(&out.stdout, &out.stderr, "osv-scanner")?;
    Ok(normalize::normalize_osv(&v))
}

fn run_cargo_audit(opts: &ScanOptions) -> Result<Vec<Finding>, String> {
    let locks = find_lockfiles(&opts.path, "Cargo.lock");
    if locks.is_empty() {
        return Ok(Vec::new());
    }
    let mut findings = Vec::new();
    for lock in locks {
        let rel = lock.strip_prefix(&opts.path).unwrap_or(&lock).display().to_string();
        let mut cmd = Command::new("cargo");
        cmd.args(["audit", "--json", "-f"]).arg(&lock).current_dir(&opts.path);
        let out = run_capped(cmd, opts.timeout)?;
        let v = parse_json(&out.stdout, &out.stderr, "cargo-audit")?;
        findings.append(&mut normalize::normalize_cargo_audit(&v, &rel));
    }
    Ok(findings)
}

fn run_gitleaks(opts: &ScanOptions) -> Result<Vec<Finding>, String> {
    let mut cmd = Command::new("gitleaks");
    if opts.staged {
        cmd.args(["git", "--staged"]);
    } else {
        cmd.arg("dir");
    }
    cmd.args(["--redact", "--report-format", "json", "--report-path", "-"]);
    if !opts.staged {
        cmd.arg(".");
    }
    cmd.current_dir(&opts.path);
    let out = run_capped(cmd, opts.timeout)?;
    // gitleaks exits 1 when leaks are found; JSON is on stdout regardless.
    let v = parse_json(&out.stdout, &out.stderr, "gitleaks")?;
    Ok(normalize::normalize_gitleaks(&v))
}

fn run_trivy(opts: &ScanOptions) -> Result<Vec<Finding>, String> {
    let mut cmd = Command::new("trivy");
    cmd.args(["fs", "--scanners", "vuln,misconfig,secret", "--format", "json", "--quiet"])
        .arg(".")
        .current_dir(&opts.path);
    let out = run_capped(cmd, opts.timeout)?;
    let v = parse_json(&out.stdout, &out.stderr, "trivy")?;
    Ok(normalize::normalize_trivy(&v))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Captured stdout/stderr of a finished (or killed) child.
struct CapturedOut {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `cmd`, capturing capped stdout/stderr, killing it after `timeout`.
fn run_capped(mut cmd: Command, timeout: Duration) -> Result<CapturedOut, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;

    // Drain both pipes on their own threads so a full pipe never deadlocks.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let (otx, orx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(MAX_OUTPUT as u64).read_to_end(&mut buf);
        }
        let _ = otx.send(buf);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.take((1024 * 1024) as u64).read_to_end(&mut buf);
        }
        let _ = etx.send(buf);
    });

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    }
    let stdout = orx.recv().unwrap_or_default();
    let stderr = erx.recv().unwrap_or_default();
    Ok(CapturedOut { stdout, stderr })
}

/// Parses captured stdout as JSON; on failure, surfaces a trimmed stderr tail.
fn parse_json(stdout: &[u8], stderr: &[u8], tool: &str) -> Result<Value, String> {
    // Some tools print a banner before the JSON; find the first JSON token.
    let text = String::from_utf8_lossy(stdout);
    let start = text.find(['{', '[']);
    if let Some(i) = start {
        if let Ok(v) = serde_json::from_str::<Value>(text[i..].trim_end()) {
            return Ok(v);
        }
    }
    let err = String::from_utf8_lossy(stderr);
    let tail: String = err.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("; ");
    if tail.is_empty() {
        Err(format!("{tool}: no JSON output"))
    } else {
        Err(format!("{tool}: {tail}"))
    }
}

/// Returns staged files (ACM) relative to `repo`, via git (no shell).
fn staged_files(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACM"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Finds up to [`MAX_LOCKFILES`] files named `name`, skipping `.git`, `target`,
/// and `node_modules`.
fn find_lockfiles(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_LOCKFILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(base, ".git" | "target" | "node_modules" | ".gentle") {
                    continue;
                }
                stack.push(path);
            } else if path.file_name().and_then(|s| s.to_str()) == Some(name) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_tools_reports_missing_with_empty_path() {
        let (run, missing) = detect_tools_in(Some(std::ffi::OsStr::new("")));
        assert!(run.is_empty(), "no tools should be found on an empty PATH: {run:?}");
        assert_eq!(missing.len(), TOOLS.len());
        // A nonexistent dir is also empty.
        let (run2, _) = detect_tools_in(Some(std::ffi::OsStr::new("/nonexistent-gsec-dir-xyz")));
        assert!(run2.is_empty());
    }

    #[test]
    fn scan_with_no_tools_yields_empty_report_with_all_missing() {
        let opts = ScanOptions::new(std::env::temp_dir());
        let missing: Vec<String> = TOOLS.iter().map(|s| s.to_string()).collect();
        let report = scan_detected(&opts, Vec::new(), missing);
        assert!(report.findings.is_empty());
        assert!(report.tools_run.is_empty());
        assert_eq!(report.tools_missing.len(), TOOLS.len());
        assert!(report.tool_errors.is_empty());
    }

    #[test]
    fn parse_json_finds_json_after_a_banner() {
        let v = parse_json(b"Scanning...\n{\"results\":[]}\n", b"", "semgrep").unwrap();
        assert!(v.get("results").is_some());
    }

    #[test]
    fn parse_json_reports_stderr_on_failure() {
        let e = parse_json(b"not json", b"fatal: boom\n", "trivy").unwrap_err();
        assert!(e.contains("trivy"));
        assert!(e.contains("boom"));
    }
}
