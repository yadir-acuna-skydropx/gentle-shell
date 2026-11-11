//! `gentle-apps` binary: with no args it runs the stdio apps engine; logs go to
//! stderr so stdout stays a clean NDJSON stream. The `slack` subcommands manage
//! the Slack browser login, token and metadata. The token is never printed.
use std::io::{self, BufReader};
use std::time::Instant;

use gentle_apps::config::{self, SlackMeta};
use gentle_apps::oauth::{self, SlackConfig, XdgOpenBrowser};
use gentle_apps::store::SLACK_ACCOUNT;
use gentle_apps::{home_dir, select_store, slack};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
gentle-apps — apps engine for gentle-partitioner

USAGE:
    gentle-apps                          Run the stdio engine (NDJSON on stdin/stdout)
    gentle-apps slack login [--client-id ID]   Log in to Slack in the browser (OAuth + PKCE)
    gentle-apps slack logout             Remove the stored Slack token and metadata
    gentle-apps slack status             Show whether Slack is connected

OPTIONS:
    -h, --help       Print help
    -V, --version    Print version
";

const SETUP_INSTRUCTIONS: &str = "\
No Slack client id configured.

Create the app and copy its Client ID first:
  1. Go to https://api.slack.com/apps → \"Create New App\" → \"From an app manifest\".
  2. Choose your workspace and paste partitioner/apps/slack-manifest.yaml.
  3. Verify the redirect URL http://localhost:47917/callback and that PKCE is enabled.
  4. Copy the Client ID from \"Basic Information\".

Then run:
  gentle-apps slack login --client-id <ID>
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
        Some("slack") => run_slack(&args[1..]),
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

fn run_slack(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("login") => slack_login(&args[1..]),
        Some("logout") => slack_logout(),
        Some("status") => {
            let store = select_store();
            slack::status_line(store.as_ref(), &home_dir())
        }
        _ => {
            eprintln!("gentle-apps: usage: gentle-apps slack <login|logout|status>");
            2
        }
    }
}

/// Resolves the client id from `--client-id`, else `apps.toml`. When given on
/// the command line it is persisted to `apps.toml`.
fn resolve_client_id(args: &[String], home: &std::path::Path) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--client-id" {
            if let Some(id) = iter.next() {
                let mut meta = config::load(home);
                meta.client_id = Some(id.clone());
                if let Err(e) = config::save(home, &meta) {
                    eprintln!("gentle-apps: could not save client id: {e}");
                }
                return Some(id.clone());
            }
        } else if let Some(id) = a.strip_prefix("--client-id=") {
            let mut meta = config::load(home);
            meta.client_id = Some(id.to_string());
            if let Err(e) = config::save(home, &meta) {
                eprintln!("gentle-apps: could not save client id: {e}");
            }
            return Some(id.to_string());
        }
    }
    config::load(home).client_id
}

fn slack_login(args: &[String]) -> i32 {
    let home = home_dir();
    let client_id = match resolve_client_id(args, &home) {
        Some(id) => id,
        None => {
            print!("{SETUP_INSTRUCTIONS}");
            return 2;
        }
    };

    let listener = match oauth::bind_redirect() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("gentle-apps: {e}");
            return 1;
        }
    };

    let cfg = SlackConfig::from_env();
    let store = select_store();
    let opener = XdgOpenBrowser;
    let deadline = Instant::now() + oauth::LOGIN_DEADLINE;

    println!("Opening your browser to authorize gentle-apps with Slack…");
    let result = oauth::run_login(
        &cfg,
        &client_id,
        &listener,
        &opener,
        store.as_ref(),
        &home,
        deadline,
        &|url| println!("If the browser does not open, paste this URL:\n{url}"),
    );
    match result {
        Ok(r) => {
            let team = if r.team_name.is_empty() { "your workspace".to_string() } else { r.team_name };
            println!("Logged in to Slack workspace {team} as {}", r.user_id);
            0
        }
        Err(e) => {
            eprintln!("gentle-apps: slack login failed: {e}");
            1
        }
    }
}

fn slack_logout() -> i32 {
    let home = home_dir();
    let store = select_store();
    let mut code = 0;
    if let Err(e) = store.delete(SLACK_ACCOUNT) {
        eprintln!("gentle-apps: could not delete keyring token: {e}");
        code = 1;
    }
    // Drop the workspace/user metadata but keep the non-secret client id so a
    // re-login does not need --client-id again.
    let kept = SlackMeta { client_id: config::load(&home).client_id, ..Default::default() };
    if let Err(e) = config::save(&home, &kept) {
        eprintln!("gentle-apps: could not clear metadata: {e}");
        code = 1;
    }
    if code == 0 {
        println!("Logged out of Slack.");
    }
    code
}
