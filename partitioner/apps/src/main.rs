//! `gentle-apps` binary: with no args it runs the stdio apps engine; logs go to
//! stderr so stdout stays a clean NDJSON stream. Subcommands (e.g.
//! `slack login`) are placeholders until T13.
use std::io::{self, BufReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
gentle-apps — apps engine for gentle-partitioner

USAGE:
    gentle-apps                Run the stdio engine (NDJSON on stdin/stdout)
    gentle-apps slack login    Configure the Slack connector (not implemented yet)

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
            println!("gentle-apps {VERSION}");
            0
        }
        Some("slack") if args.get(1).map(String::as_str) == Some("login") => {
            eprintln!("not implemented yet (T13)");
            2
        }
        Some(other) => {
            eprintln!("gentle-apps: unknown command '{other}'\n");
            eprint!("{HELP}");
            2
        }
    };
    std::process::exit(code);
}

fn run_engine() -> i32 {
    let stdin = BufReader::new(io::stdin());
    let stdout = io::stdout();
    match gentle_apps::engine::run(gentle_apps::default_connectors(), stdin, stdout.lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gentle-apps: engine error: {e}");
            1
        }
    }
}
