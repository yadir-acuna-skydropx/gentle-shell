//! `gentle-explain` binary: with no args it runs the stdio explainer engine
//! (NDJSON on stdin/stdout, logs on stderr). The `key` subcommands manage the
//! Anthropic API key in the system keyring. The key is never printed.
use std::io::{self, BufRead, BufReader, IsTerminal};

use gentle_engines::backend::AnthropicBackend;
use gentle_engines::cli::{self, SetOutcome};
use gentle_engines::keystore;
use gentle_engines::{anthropic, engine, home_dir};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
gentle-explain — code explainer engine for gentle-partitioner

USAGE:
    gentle-explain                 Run the stdio engine (NDJSON on stdin/stdout)
    gentle-explain key set         Store your Anthropic API key (validated first)
    gentle-explain key status      Show whether an API key is stored
    gentle-explain key clear       Remove the stored API key

OPTIONS:
    -h, --help       Print help
    -V, --version    Print version
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
            println!("gentle-explain {VERSION}");
            0
        }
        Some("key") => run_key(&args[1..]),
        Some(other) => {
            eprintln!("gentle-explain: unknown command '{other}'\n");
            eprint!("{HELP}");
            2
        }
    };
    std::process::exit(code);
}

fn run_engine() -> i32 {
    let store = keystore::select_store();
    let backend = AnthropicBackend::new(store.as_ref(), &home_dir());
    let stdin = BufReader::new(io::stdin());
    match engine::run(backend, stdin, io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gentle-explain: engine error: {e}");
            1
        }
    }
}

fn run_key(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("set") => key_set(),
        Some("status") => key_status(),
        Some("clear") => key_clear(),
        _ => {
            eprintln!("gentle-explain: usage: gentle-explain key <set|status|clear>");
            2
        }
    }
}

/// Reads the key without echo from a TTY, or one line from stdin when piped.
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
            eprintln!("gentle-explain: could not read the key: {e}");
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
            eprintln!("gentle-explain: {reason}");
            2
        }
        SetOutcome::Rejected => {
            eprintln!("gentle-explain: invalid API key — the key was rejected by Anthropic");
            1
        }
        SetOutcome::Error(e) => {
            eprintln!("gentle-explain: {e}");
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
            eprintln!("gentle-explain: could not read the keyring: {e}");
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
            eprintln!("gentle-explain: could not remove the key: {e}");
            1
        }
    }
}
