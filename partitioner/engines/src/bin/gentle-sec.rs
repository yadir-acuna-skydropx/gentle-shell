//! `gentle-sec` binary (S35): cybersecurity scanning engine.
//!
//! Subcommands:
//!   - `scan [PATH] [--staged] [--fail-on low|medium|high|critical] [--json]`
//!   - `list`                      List saved reports (newest first)
//!   - `show <id>`                 Print a report's summary.md
//!   - `hook install|uninstall|status`   Manage the pre-commit hook
//!
//! Reports are written under `<repo>/.gentle/security/<UTC timestamp>/`.
//! No raw secret value is ever printed or stored.
use std::path::{Path, PathBuf};
use std::process::Command;

use gentle_engines::sec::report::{self, Report};
use gentle_engines::sec::run::{self, ScanOptions};
use gentle_engines::sec::{hook, Severity};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
gentle-sec — cybersecurity scanning engine for gentle-partitioner

USAGE:
    gentle-sec scan [PATH] [OPTIONS]    Scan a repo with the installed tools
    gentle-sec list                     List saved reports (newest first)
    gentle-sec show <id>                Print a report's summary
    gentle-sec hook install             Install the pre-commit hook (chains an existing one)
    gentle-sec hook uninstall           Remove the pre-commit hook (restores a chained one)
    gentle-sec hook status              Show the pre-commit hook state

SCAN OPTIONS:
    --staged                 Scan only staged files (for pre-commit)
    --fail-on <level>        Exit non-zero if a finding is >= level (low|medium|high|critical)
    --json                   Print the report JSON to stdout instead of the summary

OPTIONS:
    -h, --help       Print help
    -V, --version    Print version

Tools run when installed on PATH: Semgrep, OSV-Scanner, cargo-audit, Gitleaks, Trivy.
Missing tools are reported, never fatal. Bypass the hook once with `git commit --no-verify`.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print!("{HELP}");
            0
        }
        Some("-V") | Some("--version") => {
            println!("gentle-sec {VERSION}");
            0
        }
        Some("scan") => run_scan(&args[1..]),
        Some("list") => run_list(),
        Some("show") => run_show(&args[1..]),
        Some("hook") => run_hook(&args[1..]),
        Some(other) => {
            eprintln!("gentle-sec: unknown command '{other}'\n");
            eprint!("{HELP}");
            2
        }
    };
    std::process::exit(code);
}

/// Resolves the git toplevel of `path` (where reports live), else `path` itself.
fn repo_root(path: &Path) -> PathBuf {
    let out = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    path.to_path_buf()
}

fn run_scan(args: &[String]) -> i32 {
    let mut path: Option<String> = None;
    let mut staged = false;
    let mut fail_on: Option<Severity> = None;
    let mut as_json = false;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--staged" => staged = true,
            "--json" => as_json = true,
            "--fail-on" => match iter.next() {
                Some(level) => match Severity::parse_threshold(level) {
                    Ok(s) => fail_on = Some(s),
                    Err(e) => {
                        eprintln!("gentle-sec: {e}");
                        return 2;
                    }
                },
                None => {
                    eprintln!("gentle-sec: --fail-on needs a level (low|medium|high|critical)");
                    return 2;
                }
            },
            other if other.starts_with("--") => {
                eprintln!("gentle-sec: unknown scan option '{other}'");
                return 2;
            }
            other => path = Some(other.to_string()),
        }
    }

    let target = PathBuf::from(path.unwrap_or_else(|| ".".to_string()));
    if !target.exists() {
        eprintln!("gentle-sec: path does not exist: {}", target.display());
        return 2;
    }
    let root = repo_root(&target);

    let mut opts = ScanOptions::new(target);
    opts.staged = staged;
    let report = run::scan(&opts);

    // Persist the report under the repo root.
    match report::write_report(&root, &report) {
        Ok(dir) => {
            if as_json {
                println!("{}", report.to_json());
            } else {
                print_summary(&report, &dir);
            }
        }
        Err(e) => {
            // Still print the result even if persistence failed.
            eprintln!("gentle-sec: could not write report: {e}");
            if as_json {
                println!("{}", report.to_json());
            } else {
                print_summary(&report, Path::new("(unsaved)"));
            }
        }
    }

    if let Some(threshold) = fail_on {
        if report.fails(threshold) {
            return 1;
        }
    }
    0
}

fn print_summary(report: &Report, dir: &Path) {
    let counts = report.severity_counts();
    println!("Security scan {} — {} findings", report.id, report.findings.len());
    println!(
        "  critical {}  high {}  medium {}  low {}  info {}",
        counts.get(&Severity::Critical).copied().unwrap_or(0),
        counts.get(&Severity::High).copied().unwrap_or(0),
        counts.get(&Severity::Medium).copied().unwrap_or(0),
        counts.get(&Severity::Low).copied().unwrap_or(0),
        counts.get(&Severity::Info).copied().unwrap_or(0),
    );
    if !report.tools_run.is_empty() {
        println!("  tools: {}", report.tools_run.join(", "));
    }
    if !report.tools_missing.is_empty() {
        println!("  not installed: {}", report.tools_missing.join(", "));
    }
    for e in &report.tool_errors {
        println!("  error ({}): {}", e.tool, e.message);
    }
    println!("  report: {}", dir.display());
}

fn run_list() -> i32 {
    let root = repo_root(Path::new("."));
    let metas = report::list_reports(&root);
    if metas.is_empty() {
        println!("No security reports yet. Run `gentle-sec scan`.");
        return 0;
    }
    for m in metas {
        println!("{}  {}", m.id, m.label());
    }
    0
}

fn run_show(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("gentle-sec: usage: gentle-sec show <id>");
        return 2;
    };
    let root = repo_root(Path::new("."));
    match report::show_report(&root, id) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("gentle-sec: no such report '{id}': {e}");
            1
        }
    }
}

fn run_hook(args: &[String]) -> i32 {
    let root = repo_root(Path::new("."));
    let hooks_dir = match hook::resolve_hooks_dir(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("gentle-sec: could not resolve hooks dir: {e}");
            return 1;
        }
    };
    match args.first().map(String::as_str) {
        Some("install") => match hook::install(&hooks_dir) {
            Ok(hook::Outcome::Installed) => {
                println!("Installed pre-commit hook in {}", hooks_dir.display());
                0
            }
            Ok(hook::Outcome::Chained) => {
                println!("Installed pre-commit hook, chaining the existing one (pre-commit.gentle-orig)");
                0
            }
            Ok(hook::Outcome::AlreadyInstalled) => {
                println!("pre-commit hook already installed (refreshed)");
                0
            }
            Ok(_) => 0,
            Err(e) => {
                eprintln!("gentle-sec: hook install failed: {e}");
                1
            }
        },
        Some("uninstall") => match hook::uninstall(&hooks_dir) {
            Ok(hook::Outcome::Restored) => {
                println!("Removed pre-commit hook and restored the chained original");
                0
            }
            Ok(hook::Outcome::Uninstalled) => {
                println!("Removed pre-commit hook");
                0
            }
            Ok(hook::Outcome::NotInstalled) => {
                println!("No gentle-sec pre-commit hook to remove");
                0
            }
            Ok(_) => 0,
            Err(e) => {
                eprintln!("gentle-sec: hook uninstall failed: {e}");
                1
            }
        },
        Some("status") => {
            println!("{}", hook::status(&hooks_dir));
            0
        }
        _ => {
            eprintln!("gentle-sec: usage: gentle-sec hook <install|uninstall|status>");
            2
        }
    }
}
