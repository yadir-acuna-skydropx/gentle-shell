//! `gentle-arch` binary (S18): architecture evaluation engine.
//!
//! With no args it runs the stdio engine (NDJSON on stdin/stdout). Subcommands:
//!   - `kb update | status` — manage the knowledge base.
//!   - `review [PATH] [--deep] [--out FILE] [--no-tools]`
//!   - `design "<brief>" [--deep] [--out FILE] [--brief-file F]`
//!   - `key set | status | clear` — manage the Anthropic API key.
//! The API key is never printed.
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;

use gentle_engines::arch::{self, ArchBackend};
use gentle_engines::cli::{self, SetOutcome};
use gentle_engines::engine::CancelFlag;
use gentle_engines::{anthropic, arch_engine, kb, keystore, home_dir};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
gentle-arch — architecture evaluation engine for gentle-partitioner

USAGE:
    gentle-arch                         Run the stdio engine (NDJSON on stdin/stdout)
    gentle-arch kb update               Fetch the curated docs into the knowledge base
    gentle-arch kb status               Show sources, chunks, age and failed fetches
    gentle-arch review [PATH]           Review a repo's infrastructure (default: cwd)
    gentle-arch design \"<brief>\"        Design an architecture from a brief
    gentle-arch key set|status|clear    Manage the Anthropic API key

REVIEW OPTIONS:
    --deep            Use the opus model line
    --out FILE        Write the report to FILE (never overwrites)
    --no-tools        Skip Infracost / Checkov

DESIGN OPTIONS:
    --deep            Use the opus model line
    --out FILE        Write the report to FILE (never overwrites)
    --brief-file F    Read the brief from a file (or stdin when no brief is given)

OPTIONS:
    -h, --help        Print help
    -V, --version     Print version
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => run_engine(),
        Some("-h") | Some("--help") => {
            print!("{HELP}");
            0
        }
        Some("-V") | Some("--version") => {
            println!("gentle-arch {VERSION}");
            0
        }
        Some("kb") => run_kb(&args[1..]),
        Some("review") => run_review(&args[1..]),
        Some("design") => run_design(&args[1..]),
        Some("key") => run_key(&args[1..]),
        Some(other) => {
            eprintln!("gentle-arch: unknown command '{other}'\n");
            eprint!("{HELP}");
            2
        }
    };
    std::process::exit(code);
}

fn run_engine() -> i32 {
    let store = keystore::select_store();
    let backend = ArchBackend::new(store.as_ref(), &home_dir(), &anthropic::base_url());
    let stdin = BufReader::new(io::stdin());
    match arch_engine::run(backend, stdin, io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gentle-arch: engine error: {e}");
            1
        }
    }
}

// --- kb ---------------------------------------------------------------------

fn run_kb(args: &[String]) -> i32 {
    let dir = kb::kb_dir(&home_dir());
    match args.first().map(String::as_str) {
        Some("update") => {
            let ts = arch::timestamp_now();
            eprintln!("gentle-arch: fetching {} sources into {}", kb::SOURCES.len(), dir.display());
            match kb::update(&dir, kb::SOURCES, &ts) {
                Ok(index) => {
                    println!(
                        "kb update: {} sources fetched, {} failed, {} chunks",
                        index.sources.len(),
                        index.failures.len(),
                        index.chunk_count()
                    );
                    for f in &index.failures {
                        eprintln!("  failed: {} — {}", f.url, f.reason);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("gentle-arch: kb update failed: {e}");
                    1
                }
            }
        }
        Some("status") => {
            let index = kb::load(&dir);
            println!("Knowledge base: {}", dir.display());
            println!("Sources: {}", index.sources.len());
            println!("Chunks: {}", index.chunk_count());
            if let Some(first) = index.sources.first() {
                println!("Last fetched: {}", first.fetched_at);
            }
            for s in &index.sources {
                println!("  {} ({} chunks) — {}", s.url, s.chunks, s.title);
            }
            if !index.failures.is_empty() {
                println!("Failed fetches: {}", index.failures.len());
                for f in &index.failures {
                    println!("  {} — {}", f.url, f.reason);
                }
            }
            0
        }
        _ => {
            eprintln!("gentle-arch: usage: gentle-arch kb <update|status>");
            2
        }
    }
}

// --- review / design --------------------------------------------------------

struct RunOpts {
    deep: bool,
    out: Option<PathBuf>,
    no_tools: bool,
    positional: Vec<String>,
    brief_file: Option<PathBuf>,
}

fn parse_run_opts(args: &[String]) -> Result<RunOpts, String> {
    let mut opts = RunOpts { deep: false, out: None, no_tools: false, positional: Vec::new(), brief_file: None };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--deep" => opts.deep = true,
            "--no-tools" => opts.no_tools = true,
            "--out" => {
                i += 1;
                opts.out = Some(PathBuf::from(args.get(i).ok_or("--out needs a FILE")?));
            }
            "--brief-file" => {
                i += 1;
                opts.brief_file = Some(PathBuf::from(args.get(i).ok_or("--brief-file needs a FILE")?));
            }
            other if other.starts_with("--") => return Err(format!("unknown option '{other}'")),
            other => opts.positional.push(other.to_string()),
        }
        i += 1;
    }
    Ok(opts)
}

fn run_review(args: &[String]) -> i32 {
    let opts = match parse_run_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gentle-arch: {e}");
            return 2;
        }
    };
    let path = opts.positional.first().cloned().unwrap_or_default();
    let root = arch::resolve_root(&path);
    let store = keystore::select_store();
    let mut backend = ArchBackend::new(store.as_ref(), &home_dir(), &anthropic::base_url());
    let ts = arch::timestamp_now();
    let flag = CancelFlag::new_flag();
    let cancelled = || flag.is_cancelled();
    let mut stdout = io::stdout();
    let mut sink = |t: &str| {
        let _ = stdout.write_all(t.as_bytes());
        let _ = stdout.flush();
    };
    match backend.review(&root, opts.deep, !opts.no_tools, &ts, opts.out.as_deref(), &mut sink, &cancelled) {
        Ok(out) => {
            println!("\n\nReport written to {}", out.path.display());
            0
        }
        Err(e) => {
            eprintln!("\ngentle-arch: {e}");
            1
        }
    }
}

fn run_design(args: &[String]) -> i32 {
    let opts = match parse_run_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gentle-arch: {e}");
            return 2;
        }
    };
    let brief = match resolve_brief(&opts) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gentle-arch: {e}");
            return 2;
        }
    };
    if brief.trim().is_empty() {
        eprintln!("gentle-arch: an empty brief — pass \"<brief>\", --brief-file F, or pipe it on stdin");
        return 2;
    }
    let root = arch::resolve_root("");
    let store = keystore::select_store();
    let mut backend = ArchBackend::new(store.as_ref(), &home_dir(), &anthropic::base_url());
    let ts = arch::timestamp_now();
    let flag = CancelFlag::new_flag();
    let cancelled = || flag.is_cancelled();
    let mut stdout = io::stdout();
    let mut sink = |t: &str| {
        let _ = stdout.write_all(t.as_bytes());
        let _ = stdout.flush();
    };
    match backend.design(&root, &brief, opts.deep, &ts, opts.out.as_deref(), &mut sink, &cancelled) {
        Ok(out) => {
            println!("\n\nReport written to {}", out.path.display());
            0
        }
        Err(e) => {
            eprintln!("\ngentle-arch: {e}");
            1
        }
    }
}

/// Resolves the brief from the positional arg, `--brief-file`, or stdin.
fn resolve_brief(opts: &RunOpts) -> Result<String, String> {
    if let Some(f) = &opts.brief_file {
        return std::fs::read_to_string(f).map_err(|e| format!("could not read the brief file: {e}"));
    }
    if let Some(b) = opts.positional.first() {
        return Ok(b.clone());
    }
    if !io::stdin().is_terminal() {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        return Ok(s);
    }
    Ok(String::new())
}

// --- key --------------------------------------------------------------------

fn run_key(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("set") => key_set(),
        Some("status") => key_status(),
        Some("clear") => key_clear(),
        _ => {
            eprintln!("gentle-arch: usage: gentle-arch key <set|status|clear>");
            2
        }
    }
}

fn read_key() -> io::Result<String> {
    if io::stdin().is_terminal() {
        rpassword::prompt_password("Anthropic API key: ")
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }
}

fn key_set() -> i32 {
    let key = match read_key() {
        Ok(k) => k.trim().to_string(),
        Err(e) => {
            eprintln!("gentle-arch: could not read the key: {e}");
            return 1;
        }
    };
    let store = keystore::select_store();
    match cli::key_set(store.as_ref(), &key, &anthropic::base_url()) {
        SetOutcome::Saved => {
            println!("API key saved.");
            0
        }
        SetOutcome::InvalidFormat(reason) => {
            eprintln!("gentle-arch: {reason}");
            2
        }
        SetOutcome::Rejected => {
            eprintln!("gentle-arch: invalid API key — the key was rejected by Anthropic");
            1
        }
        SetOutcome::Error(e) => {
            eprintln!("gentle-arch: {e}");
            1
        }
    }
}

fn key_status() -> i32 {
    let store = keystore::select_store();
    match cli::key_status_line(store.as_ref()) {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(e) => {
            eprintln!("gentle-arch: could not read the keyring: {e}");
            1
        }
    }
}

fn key_clear() -> i32 {
    let store = keystore::select_store();
    match cli::key_clear(store.as_ref()) {
        Ok(()) => {
            println!("API key removed.");
            0
        }
        Err(e) => {
            eprintln!("gentle-arch: could not remove the key: {e}");
            1
        }
    }
}
