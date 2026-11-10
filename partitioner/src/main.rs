//! gentle-partitioner binary: wires the terminal, the PTY running gentle-shell
//! and the worker threads around the pure state machine in `app`.
//!
//! Threads (S2): an input thread (`crossterm::event::read`), the PTY reader
//! thread (inside `PtyPane::spawn`) and a git-status worker. The UI thread
//! blocks on a single channel and repaints only when an event arrives — it
//! never polls and never repaints while idle (S11).
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as CtEvent,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use portable_pty::CommandBuilder;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use gentle_partitioner::app::{draw, Action, App, Focus};
use gentle_partitioner::pane::PtyPane;
use gentle_partitioner::predict::{self, Engine};
use gentle_partitioner::predict_server::PredictServer;
use gentle_partitioner::tree::{self, GitStatus};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_COMMAND: &str = "gentle-shell";
/// Minimum gap between git-status refreshes triggered by PTY output.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_millis(800);

const HELP: &str = "\
gentle-partitioner — partitioned terminal host for gentle-shell

USAGE:
    gentle-partitioner [OPTIONS] [-- <command> [args...]]

OPTIONS:
    -h, --help       Print help
    -V, --version    Print version
        --no-predict Disable the local n-gram prediction engine and server

Without a command it runs `gentle-shell` inside a partitioned terminal.
Everything after `--` is launched instead of the default command.
With prediction on, the child sees GENTLE_PREDICT_URL and GENTLE_PREDICT_TOKEN.
";

type Backend = CrosstermBackend<io::Stdout>;

/// Events multiplexed onto the single UI channel.
enum Event {
    /// A terminal input event from the input thread.
    Input(CtEvent),
    /// The PTY produced output (coalesced: at most one pending at a time).
    PtyOutput,
    /// Fresh git statuses computed off the UI thread.
    Statuses(HashMap<PathBuf, GitStatus>),
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("gentle-partitioner: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let opts = match parse_args(std::env::args().skip(1).collect())? {
        ParseOutcome::Run(opts) => opts,
        ParseOutcome::Exit(code) => return Ok(code),
    };
    let command = opts.command;

    // Resolve before touching the terminal so a missing command yields a clean,
    // readable error instead of a corrupted screen.
    let program = command[0].clone();
    if resolve_program(&program).is_none() {
        anyhow::bail!("command not found: {program} (is {DEFAULT_COMMAND} installed and on your PATH?)");
    }

    install_panic_hook();
    let mut terminal = setup_terminal().context("entering raw mode / alternate screen")?;
    let result = event_loop(&mut terminal, &command, opts.predict);
    // Always restore the terminal, even when the loop failed.
    let _ = restore_terminal();
    result.map(|_| 0)
}

struct RunOpts {
    command: Vec<String>,
    predict: bool,
}

enum ParseOutcome {
    Run(RunOpts),
    Exit(i32),
}

fn parse_args(args: Vec<String>) -> Result<ParseOutcome> {
    let mut iter = args.into_iter();
    let mut command: Vec<String> = Vec::new();
    let mut predict = true;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(ParseOutcome::Exit(0));
            }
            "-V" | "--version" => {
                println!("gentle-partitioner {VERSION}");
                return Ok(ParseOutcome::Exit(0));
            }
            "--no-predict" => {
                predict = false;
            }
            "--" => {
                command.extend(iter.by_ref());
                break;
            }
            other => {
                eprintln!("gentle-partitioner: unexpected argument '{other}'\n");
                eprint!("{HELP}");
                return Ok(ParseOutcome::Exit(2));
            }
        }
    }
    if command.is_empty() {
        command.push(DEFAULT_COMMAND.to_string());
    }
    Ok(ParseOutcome::Run(RunOpts { command, predict }))
}

/// Shared handles for the prediction engine and its local server.
struct PredictHandles {
    engine: Arc<RwLock<Engine>>,
    url: String,
    token: String,
    /// Kept alive for the process lifetime (the listener thread serves on).
    _server: PredictServer,
}

/// Starts the engine and its server, then seeds both models off the UI thread
/// (repo text for code, shared prompt history for chat).
fn setup_predict(root: &Path) -> Result<PredictHandles> {
    let engine = Arc::new(RwLock::new(Engine::new()));
    let server = PredictServer::start(engine.clone()).context("starting the predict server")?;
    let url = server.url();
    let token = server.token.clone();
    {
        let engine = engine.clone();
        let root = root.to_path_buf();
        std::thread::spawn(move || predict::seed_code_repo(&engine, &root));
    }
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            let _ = predict::seed_chat_history(&engine);
        });
    }
    Ok(PredictHandles { engine, url, token, _server: server })
}

/// Finds `program` on PATH (or validates an explicit path). Returns the
/// resolved path when the command can be launched.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() || program.contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

fn setup_terminal() -> Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default(info);
    }));
}

fn event_loop(terminal: &mut Terminal<Backend>, command: &[String], predict_enabled: bool) -> Result<()> {
    let root = std::env::current_dir().context("reading the current directory")?;
    let mut app = App::new(root.clone());

    // Start prediction before the child so its env can carry the URL/token.
    let predict = if predict_enabled {
        match setup_predict(&root) {
            Ok(h) => Some(h),
            // Prediction is best-effort: a failure must not stop the shell.
            Err(_) => None,
        }
    } else {
        None
    };
    if let Some(h) = &predict {
        app.set_predict(h.engine.clone());
    }

    let (tx, rx) = mpsc::channel::<Event>();
    spawn_input_thread(tx.clone());
    let git_tx = spawn_git_worker(tx.clone(), root.clone());

    // Coalesce PTY wake-ups: the reader thread flips the flag and only sends a
    // PtyOutput when none is pending, so the UI never floods on fast output.
    let pty_pending = Arc::new(AtomicBool::new(false));
    let mut pty = spawn_pty(terminal, &mut app, command, &root, tx.clone(), pty_pending.clone(), predict.as_ref())?;

    // Prime the first git refresh and paint the initial frame.
    let _ = git_tx.send(());
    let mut last_git_refresh = Instant::now() - GIT_REFRESH_INTERVAL;
    redraw(terminal, &mut app, &mut pty)?;

    while let Ok(ev) = rx.recv() {
        let mut needs_redraw = true;
        match ev {
            Event::Input(CtEvent::Key(key)) => {
                // The child's DECCKM state decides how arrows are encoded.
                pty.with_screen(|s| app.app_cursor = s.application_cursor());
                match app.handle_key(key) {
                    Action::None => {}
                    Action::Forward(bytes) => {
                        pty.write(&bytes);
                        pty.reset_scroll();
                    }
                    Action::ScrollChat(delta) => pty.scroll_chat(delta),
                    Action::Quit => break,
                }
                // A save asks for a fresh git status off the UI thread.
                if app.take_git_refresh() {
                    last_git_refresh = Instant::now();
                    let _ = git_tx.send(());
                }
            }
            Event::Input(CtEvent::Mouse(mouse)) => match app.handle_mouse(mouse) {
                Action::None => {}
                Action::Forward(bytes) => {
                    pty.write(&bytes);
                    pty.reset_scroll();
                }
                Action::ScrollChat(delta) => pty.scroll_chat(delta),
                Action::Quit => break,
            },
            Event::Input(CtEvent::Paste(text)) => {
                if app.effective_focus() == Focus::Chat && app.overlay.is_none() {
                    let mut bytes = b"\x1b[200~".to_vec();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    pty.write(&bytes);
                    pty.reset_scroll();
                }
            }
            Event::Input(CtEvent::Resize(_, _)) => {}
            Event::Input(_) => needs_redraw = false,
            Event::PtyOutput => {
                pty_pending.store(false, Ordering::Release);
                // Throttle git refreshes so busy output cannot hammer `git status`.
                if last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
                    last_git_refresh = Instant::now();
                    let _ = git_tx.send(());
                }
            }
            Event::Statuses(statuses) => app.apply_statuses(statuses),
        }

        // The child owns the session lifetime: when it exits, so do we.
        if !pty.is_alive() {
            break;
        }
        if needs_redraw {
            redraw(terminal, &mut app, &mut pty)?;
        }
    }
    Ok(())
}

fn spawn_input_thread(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || loop {
            match event::read() {
                Ok(ev) => {
                    if tx.send(Event::Input(ev)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        })
        .expect("spawn input thread");
}

/// Runs `tree::git_status` off the UI thread; requests are coalesced so a burst
/// collapses into one scan.
fn spawn_git_worker(tx: Sender<Event>, root: PathBuf) -> Sender<()> {
    let (req_tx, req_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
    std::thread::Builder::new()
        .name("git-status".into())
        .spawn(move || {
            while req_rx.recv().is_ok() {
                while req_rx.try_recv().is_ok() {}
                let statuses = tree::git_status(&root);
                if tx.send(Event::Statuses(statuses)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn git worker");
    req_tx
}

fn spawn_pty(
    terminal: &Terminal<Backend>,
    app: &App,
    command: &[String],
    root: &Path,
    tx: Sender<Event>,
    pending: Arc<AtomicBool>,
    predict: Option<&PredictHandles>,
) -> Result<PtyPane> {
    let size = terminal.size()?;
    let inner = app.chat_inner(Rect::new(0, 0, size.width, size.height));

    let mut cmd = CommandBuilder::new(&command[0]);
    for arg in &command[1..] {
        cmd.arg(arg);
    }
    cmd.cwd(root);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GENTLE_PARTITIONER", "1");
    // The gentle-shell child uses these to reach the in-process engine (S9).
    if let Some(h) = predict {
        cmd.env("GENTLE_PREDICT_URL", &h.url);
        cmd.env("GENTLE_PREDICT_TOKEN", &h.token);
    }

    let notify = move || {
        // Send only when no PtyOutput is already queued.
        if !pending.swap(true, Ordering::AcqRel) {
            let _ = tx.send(Event::PtyOutput);
        }
    };
    PtyPane::spawn(cmd, inner.height.max(1), inner.width.max(1), notify)
        .with_context(|| format!("spawning '{}'", command.join(" ")))
}

/// Resizes the PTY to the chat area and repaints. ratatui diffs the buffer so
/// only changed cells reach the real terminal (S3).
fn redraw(terminal: &mut Terminal<Backend>, app: &mut App, pty: &mut PtyPane) -> Result<()> {
    let size = terminal.size()?;
    let inner = app.chat_inner(Rect::new(0, 0, size.width, size.height));
    pty.resize(inner.height, inner.width);
    terminal.draw(|f| draw(f, app, Some(&*pty)))?;
    Ok(())
}
