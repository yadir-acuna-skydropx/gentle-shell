//! gentle-session CLI (S34): `attach`, `list`, `kill` and the internal
//! `__server` entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use gentle_session::commands;
use gentle_session::server::{self, ServerArgs};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("gentle-session: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        print_usage();
        return Ok(2);
    };
    match sub {
        "attach" => {
            let (dir, cmd) = parse_attach(&args[1..])?;
            commands::attach(&dir, &cmd)
        }
        "list" => commands::list(),
        "kill" => {
            let dir = parse_dir(&args[1..])?;
            commands::kill(&dir)
        }
        "__server" => run_internal_server(&args[1..]),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(0)
        }
        other => {
            bail!("unknown subcommand `{other}` (try attach|list|kill)");
        }
    }
}

fn print_usage() {
    eprintln!(
        "gentle-session — per-folder background sessions (dtach-like)\n\n\
         USAGE:\n\
         \x20 gentle-session attach [--dir DIR] -- <cmd...>   attach or create\n\
         \x20 gentle-session list                            list live sessions\n\
         \x20 gentle-session kill [--dir DIR]                terminate a session\n\n\
         Detach without killing: press Ctrl+\\ twice.\n\
         Opt out from the launcher with GENTLE_NO_SESSION=1."
    );
}

/// Parses `[--dir DIR] -- <cmd...>`.
fn parse_attach(args: &[String]) -> Result<(PathBuf, Vec<String>)> {
    let mut dir = std::env::current_dir()?;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?;
                dir = PathBuf::from(v);
                i += 2;
            }
            "--" => {
                return Ok((dir, args[i + 1..].to_vec()));
            }
            other => bail!("unexpected argument `{other}` before `--`"),
        }
    }
    bail!("attach requires `-- <cmd...>`");
}

/// Parses an optional `[--dir DIR]`, defaulting to the current directory.
fn parse_dir(args: &[String]) -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?;
                dir = PathBuf::from(v);
                i += 2;
            }
            other => bail!("unexpected argument `{other}`"),
        }
    }
    Ok(dir)
}

/// Parses `__server --socket P --meta M --attached A --folder F -- <cmd...>`.
fn run_internal_server(args: &[String]) -> Result<i32> {
    let mut socket = None;
    let mut meta = None;
    let mut attached = None;
    let mut folder = String::new();
    let mut cmd = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(need(args, i + 1, "--socket")?));
                i += 2;
            }
            "--meta" => {
                meta = Some(PathBuf::from(need(args, i + 1, "--meta")?));
                i += 2;
            }
            "--attached" => {
                attached = Some(PathBuf::from(need(args, i + 1, "--attached")?));
                i += 2;
            }
            "--folder" => {
                folder = need(args, i + 1, "--folder")?.clone();
                i += 2;
            }
            "--" => {
                cmd = args[i + 1..].to_vec();
                break;
            }
            other => bail!("unexpected __server argument `{other}`"),
        }
    }
    let server_args = ServerArgs {
        socket: socket.ok_or_else(|| anyhow::anyhow!("--socket required"))?,
        meta: meta.ok_or_else(|| anyhow::anyhow!("--meta required"))?,
        attached: attached.ok_or_else(|| anyhow::anyhow!("--attached required"))?,
        folder,
        cmd,
    };
    server::run_server(server_args)?;
    Ok(0)
}

fn need<'a>(args: &'a [String], idx: usize, flag: &str) -> Result<&'a String> {
    args.get(idx).ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}
