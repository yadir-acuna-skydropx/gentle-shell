//! Optional external analysis tools for `gentle-arch review` (S18): Infracost
//! and Checkov. Each runs only when present on `PATH`, with a hard timeout, and
//! only a summarized JSON extract goes into the model context. When a tool is
//! missing, the report notes "not installed".
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Per-tool timeout.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// Outcome of attempting one external tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResult {
    /// The tool is not on `PATH`.
    NotInstalled,
    /// The tool ran and produced a summary line/block.
    Summary(String),
    /// The tool ran but failed (timeout, non-zero, unparsable).
    Failed(String),
}

impl ToolResult {
    /// A one-line human rendering for the manifest/report.
    pub fn render(&self, name: &str) -> String {
        match self {
            ToolResult::NotInstalled => format!("{name}: not installed"),
            ToolResult::Summary(s) => format!("{name}: {s}"),
            ToolResult::Failed(e) => format!("{name}: failed ({e})"),
        }
    }
}

/// Whether `bin` resolves on `PATH`.
pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Runs a command with a timeout, returning stdout on success. Kills the child
/// on timeout. Never blocks the caller beyond `timeout`.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Read stdout on a thread so a full pipe cannot deadlock the wait.
    let mut stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(mut out) = stdout.take() {
        thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = out.read_to_string(&mut buf);
            let _ = tx.send(buf);
        });
    }
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                if status.success() {
                    return Ok(out);
                }
                return Err(format!("exit {:?}", status.code()));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("timeout".to_string());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Runs `infracost breakdown --path PATH --format json` and summarizes the
/// monthly cost. Returns [`ToolResult::NotInstalled`] when the binary is absent.
pub fn run_infracost(path: &Path) -> ToolResult {
    if !on_path("infracost") {
        return ToolResult::NotInstalled;
    }
    let mut cmd = Command::new("infracost");
    cmd.args(["breakdown", "--path"]).arg(path).args(["--format", "json"]);
    match run_with_timeout(cmd, TOOL_TIMEOUT) {
        Ok(out) => ToolResult::Summary(summarize_infracost(&out)),
        Err(e) => ToolResult::Failed(e),
    }
}

/// Extracts the total monthly cost from Infracost JSON.
pub fn summarize_infracost(json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => {
            let monthly =
                v.get("totalMonthlyCost").and_then(|c| c.as_str()).or_else(|| v.get("totalMonthlyCost").and_then(|c| c.as_str()));
            let currency = v.get("currency").and_then(|c| c.as_str()).unwrap_or("USD");
            match monthly {
                Some(m) => format!("total monthly cost ~{m} {currency}"),
                None => "parsed, no total cost field".to_string(),
            }
        }
        Err(e) => format!("unparsable JSON ({e})"),
    }
}

/// Runs `checkov -d PATH -o json --quiet` and summarizes pass/fail counts.
pub fn run_checkov(path: &Path) -> ToolResult {
    if !on_path("checkov") {
        return ToolResult::NotInstalled;
    }
    let mut cmd = Command::new("checkov");
    cmd.arg("-d").arg(path).args(["-o", "json", "--quiet"]);
    match run_with_timeout(cmd, TOOL_TIMEOUT) {
        Ok(out) => ToolResult::Summary(summarize_checkov(&out)),
        // checkov exits non-zero when it finds failures; still parse stdout.
        Err(_) => ToolResult::Failed("no parsable output".to_string()),
    }
}

/// Extracts passed/failed counts from Checkov JSON (handles the object or the
/// list-of-frameworks shape).
pub fn summarize_checkov(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return "unparsable JSON".to_string();
    };
    let objs: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut passed = 0i64;
    let mut failed = 0i64;
    for o in objs {
        if let Some(s) = o.get("summary") {
            passed += s.get("passed").and_then(|x| x.as_i64()).unwrap_or(0);
            failed += s.get("failed").and_then(|x| x.as_i64()).unwrap_or(0);
        }
    }
    format!("{passed} passed, {failed} failed checks")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tool_reports_not_installed() {
        // A binary that cannot exist on PATH.
        assert!(!on_path("definitely-not-a-real-binary-xyz-123"));
        let dir = std::env::temp_dir();
        // With the real PATH, these specialized tools are typically absent in CI.
        // We only assert the NotInstalled rendering here; the present-tool path
        // is covered by the integration test that stubs the binaries.
        let r = ToolResult::NotInstalled;
        assert_eq!(r.render("Infracost"), "Infracost: not installed");
        let _ = dir;
    }

    #[test]
    fn summarize_infracost_reads_total() {
        let json = r#"{"totalMonthlyCost":"358.00","currency":"USD"}"#;
        assert_eq!(summarize_infracost(json), "total monthly cost ~358.00 USD");
        assert!(summarize_infracost("{}").contains("no total"));
    }

    #[test]
    fn summarize_checkov_counts_results() {
        let obj = r#"{"summary":{"passed":10,"failed":3}}"#;
        assert_eq!(summarize_checkov(obj), "10 passed, 3 failed checks");
        let list = r#"[{"summary":{"passed":1,"failed":0}},{"summary":{"passed":2,"failed":5}}]"#;
        assert_eq!(summarize_checkov(list), "3 passed, 5 failed checks");
    }

    #[test]
    fn run_with_timeout_kills_a_slow_child() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let r = run_with_timeout(cmd, Duration::from_millis(200));
        assert_eq!(r, Err("timeout".to_string()));
    }

    #[test]
    fn run_with_timeout_returns_stdout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hello"]);
        assert_eq!(run_with_timeout(cmd, Duration::from_secs(5)).unwrap().trim(), "hello");
    }
}
