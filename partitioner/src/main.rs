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
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::CommandBuilder;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use gentle_engines::protocol::ClientMsg;
use gentle_partitioner::app::{draw, Action, App, Focus};
use gentle_partitioner::apps_client::{AppsClient, AppsEvent};
use gentle_partitioner::arch_client::{ArchClient, ArchEvent};
use gentle_partitioner::explain_client::{ExplainClient, ExplainEvent};
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
        --no-apps    Disable the apps engine and the bottom-left apps panel
        --no-explain Disable the code-explainer engine (gentle-explain)
        --no-arch    Disable the architecture engine (gentle-arch)

Without a command it runs `gentle-shell` inside a partitioned terminal.
Everything after `--` is launched instead of the default command.
With prediction on, the child sees GENTLE_PREDICT_URL and GENTLE_PREDICT_TOKEN.
The apps engine (gentle-apps) runs as a separate process unless --no-apps.
The explainer engine (gentle-explain) runs as a separate process unless --no-explain.
The architecture engine (gentle-arch) runs as a separate process unless --no-arch.
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
    /// An event from the independent apps engine (S12).
    Apps(AppsEvent),
    /// An event from the independent explainer engine (S17).
    Explain(ExplainEvent),
    /// An event from the independent architecture engine (S18).
    Arch(ArchEvent),
    /// Periodic tick that drives the animated explainer caret (S21). It only
    /// repaints while the explain popup takes input, so the UI stays idle
    /// otherwise (S11).
    Tick,
    /// The Agent-changes interactive shell produced output (coalesced, S24).
    ShellOutput,
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
    let result = event_loop(&mut terminal, &command, opts.predict, opts.apps, opts.explain, opts.arch);
    // Always restore the terminal, even when the loop failed.
    let _ = restore_terminal();
    result.map(|_| 0)
}

struct RunOpts {
    command: Vec<String>,
    predict: bool,
    apps: bool,
    explain: bool,
    arch: bool,
}

enum ParseOutcome {
    Run(RunOpts),
    Exit(i32),
}

fn parse_args(args: Vec<String>) -> Result<ParseOutcome> {
    let mut iter = args.into_iter();
    let mut command: Vec<String> = Vec::new();
    let mut predict = true;
    let mut apps = true;
    let mut explain = true;
    let mut arch = true;
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
            "--no-apps" => {
                apps = false;
            }
            "--no-explain" => {
                explain = false;
            }
            "--no-arch" => {
                arch = false;
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
    Ok(ParseOutcome::Run(RunOpts { command, predict, apps, explain, arch }))
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

/// Tracks whether we pushed keyboard-enhancement flags so `restore_terminal`
/// pops them exactly once. Enabling the flags lets terminals that speak the
/// Kitty keyboard protocol report Enter+modifier chords (Alt+Enter / Ctrl+Enter)
/// that the Arch brief uses to submit (S23); without them the modifier is lost
/// and Enter only inserts a newline. Release events stay ignored in
/// `App::handle_key`, so turning this on never double-processes keys.
static KBD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn setup_terminal() -> Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    // Opt into key-chord disambiguation when the terminal supports it; fall back
    // silently otherwise so legacy terminals behave exactly as before.
    if supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    {
        KBD_ENHANCED.store(true, Ordering::SeqCst);
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    if KBD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
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

fn event_loop(
    terminal: &mut Terminal<Backend>,
    command: &[String],
    predict_enabled: bool,
    apps_enabled: bool,
    explain_enabled: bool,
    arch_enabled: bool,
) -> Result<()> {
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
    spawn_tick_thread(tx.clone());
    let git_tx = spawn_git_worker(tx.clone(), root.clone());

    // The apps engine runs as a separate process; its events flow through the
    // same UI channel so redraws happen only on events (S11, S12).
    let apps = if apps_enabled {
        // Bridge the apps engine's own channel onto the single UI channel.
        let (apps_tx, apps_rx) = mpsc::channel::<AppsEvent>();
        let ui_tx = tx.clone();
        std::thread::Builder::new()
            .name("apps-bridge".into())
            .spawn(move || {
                while let Ok(ev) = apps_rx.recv() {
                    if ui_tx.send(Event::Apps(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn apps bridge");
        Some(AppsClient::spawn(apps_tx))
    } else {
        None
    };

    // The explainer engine also runs as a separate process (S17), bridged onto
    // the single UI channel so redraws happen only on events (S11).
    let explain = if explain_enabled {
        let (ex_tx, ex_rx) = mpsc::channel::<ExplainEvent>();
        let ui_tx = tx.clone();
        std::thread::Builder::new()
            .name("explain-bridge".into())
            .spawn(move || {
                while let Ok(ev) = ex_rx.recv() {
                    if ui_tx.send(Event::Explain(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn explain bridge");
        Some(ExplainClient::spawn(ex_tx))
    } else {
        None
    };

    // The architecture engine runs as a separate process (S18), bridged onto
    // the single UI channel so redraws happen only on events (S11).
    let arch = if arch_enabled {
        let (ar_tx, ar_rx) = mpsc::channel::<ArchEvent>();
        let ui_tx = tx.clone();
        std::thread::Builder::new()
            .name("arch-bridge".into())
            .spawn(move || {
                while let Ok(ev) = ar_rx.recv() {
                    if ui_tx.send(Event::Arch(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn arch bridge");
        Some(ArchClient::spawn(ar_tx))
    } else {
        None
    };

    // Coalesce PTY wake-ups: the reader thread flips the flag and only sends a
    // PtyOutput when none is pending, so the UI never floods on fast output.
    // Each conversation is its own gentle-shell child in its own PTY (S22,
    // Option A). The vector stays index-aligned with `app.conversations`; the
    // active one is `app.active_conversation`.
    let pty_pending = Arc::new(AtomicBool::new(false));
    let mut ptys: Vec<PtyPane> =
        vec![spawn_pty(terminal, &app, command, &root, tx.clone(), pty_pending.clone(), predict.as_ref())?];

    // The Agent-changes interactive shell (S24) runs in its own PTY, coalesced
    // like the main one. It is best-effort: a spawn failure leaves the terminal
    // box empty but never stops gentle-shell.
    let shell_pending = Arc::new(AtomicBool::new(false));
    let mut shell = spawn_shell(&root, tx.clone(), shell_pending.clone()).ok();

    // Prime the first git refresh and paint the initial frame.
    let _ = git_tx.send(());
    let mut last_git_refresh = Instant::now() - GIT_REFRESH_INTERVAL;
    {
        let active = app.active_conversation;
        redraw(terminal, &mut app, &mut ptys[active], shell.as_mut())?;
    }

    while let Ok(ev) = rx.recv() {
        let mut needs_redraw = true;
        match ev {
            Event::Input(CtEvent::Key(key)) => {
                // The active child's DECCKM state decides how arrows are encoded.
                ptys[app.active_conversation].with_screen(|s| app.app_cursor = s.application_cursor());
                match app.handle_key(key) {
                    Action::None => {}
                    Action::Forward(bytes) => {
                        let a = app.active_conversation;
                        ptys[a].write(&bytes);
                        ptys[a].reset_scroll();
                    }
                    Action::ForwardShell(bytes) => {
                        if let Some(sh) = &mut shell {
                            sh.write(&bytes);
                            sh.reset_scroll();
                        }
                    }
                    Action::NewConversation => {
                        if let Ok(p) =
                            spawn_pty(terminal, &app, command, &root, tx.clone(), pty_pending.clone(), predict.as_ref())
                        {
                            ptys.push(p);
                        }
                    }
                    Action::CloseConversation(i) => {
                        if i < ptys.len() {
                            ptys.remove(i);
                        }
                    }
                    Action::ScrollChat(delta) => ptys[app.active_conversation].scroll_chat(delta),
                    Action::Apps(msg) => {
                        if let Some(a) = &apps {
                            a.send(msg);
                        }
                    }
                    Action::Explain(msg) => {
                        if let Some(e) = &explain {
                            e.send(msg);
                        }
                    }
                    Action::Helper { id, question } => {
                        if let Some(e) = &explain {
                            let recent = shell
                                .as_ref()
                                .filter(|sh| sh.is_alive())
                                .map(|sh| sh.with_screen(|s| s.contents()))
                                .unwrap_or_default();
                            e.send(ClientMsg::Suggest {
                                id,
                                command_line: String::new(),
                                recent_output: recent,
                                question,
                            });
                        }
                    }
                    Action::Arch(msg) => {
                        if let Some(a) = &arch {
                            a.send(msg);
                        }
                    }
                    Action::OpenReport(path) => open_with_system(&path),
                    Action::Copy(text) => copy_to_clipboard(&text),
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
                    let a = app.active_conversation;
                    ptys[a].write(&bytes);
                    ptys[a].reset_scroll();
                }
                Action::ForwardShell(bytes) => {
                    if let Some(sh) = &mut shell {
                        sh.write(&bytes);
                        sh.reset_scroll();
                    }
                }
                Action::NewConversation => {
                    if let Ok(p) =
                        spawn_pty(terminal, &app, command, &root, tx.clone(), pty_pending.clone(), predict.as_ref())
                    {
                        ptys.push(p);
                    }
                }
                Action::CloseConversation(i) => {
                    if i < ptys.len() {
                        ptys.remove(i);
                    }
                }
                Action::ScrollChat(delta) => ptys[app.active_conversation].scroll_chat(delta),
                Action::Apps(msg) => {
                    if let Some(a) = &apps {
                        a.send(msg);
                    }
                }
                Action::Explain(msg) => {
                    if let Some(e) = &explain {
                        e.send(msg);
                    }
                }
                Action::Helper { id, question } => {
                    if let Some(e) = &explain {
                        let recent = shell
                            .as_ref()
                            .filter(|sh| sh.is_alive())
                            .map(|sh| sh.with_screen(|s| s.contents()))
                            .unwrap_or_default();
                        e.send(ClientMsg::Suggest { id, command_line: String::new(), recent_output: recent, question });
                    }
                }
                Action::Arch(msg) => {
                    if let Some(a) = &arch {
                        a.send(msg);
                    }
                }
                Action::OpenReport(path) => open_with_system(&path),
                Action::Copy(text) => copy_to_clipboard(&text),
                Action::Quit => break,
            },
            Event::Input(CtEvent::Paste(text)) => {
                if app.overlay.is_none() {
                    let focus = app.effective_focus();
                    let mut bytes = b"\x1b[200~".to_vec();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    if focus == Focus::Chat {
                        let a = app.active_conversation;
                        ptys[a].write(&bytes);
                        ptys[a].reset_scroll();
                    } else if focus == Focus::Terminal {
                        if let Some(sh) = &mut shell {
                            sh.write(&bytes);
                            sh.reset_scroll();
                        }
                    }
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
            Event::Apps(AppsEvent::Msg(msg)) => app.apply_apps_msg(msg),
            Event::Apps(AppsEvent::Offline) => app.set_apps_offline(),
            Event::Explain(ExplainEvent::Msg(msg)) => app.apply_explain_msg(msg),
            Event::Explain(ExplainEvent::Offline) => app.set_explain_offline(),
            Event::Arch(ArchEvent::Msg(msg)) => app.apply_arch_msg(msg),
            Event::Arch(ArchEvent::Offline) => app.set_arch_offline(),
            // Blink the explainer caret only while it takes input; stay idle
            // (no repaint) otherwise so S11 holds.
            Event::Tick => needs_redraw = app.tick_blink(),
            Event::ShellOutput => shell_pending.store(false, Ordering::Release),
        }

        // A conversation's child owning the session: when the ACTIVE child
        // exits, close that conversation (like the user would); quitting only
        // when no conversations remain (S22). The Agent-changes shell is
        // independent: its exit leaves the box empty.
        let active = app.active_conversation;
        if ptys.get(active).map(|p| !p.is_alive()).unwrap_or(true) {
            if active < ptys.len() {
                ptys.remove(active);
            }
            if let Action::Quit = app.close_conversation(active) {
                break;
            }
        }
        if ptys.is_empty() {
            break;
        }
        if needs_redraw {
            let active = app.active_conversation;
            redraw(terminal, &mut app, &mut ptys[active], shell.as_mut())?;
        }
    }
    // Kill the apps and explainer engines on quit so no child is left behind.
    if let Some(a) = &apps {
        a.shutdown();
    }
    if let Some(e) = &explain {
        e.shutdown();
    }
    if let Some(a) = &arch {
        a.shutdown();
    }
    Ok(())
}

/// Opens `path` with the system handler (xdg-open), detached. Best-effort: a
/// missing opener never disturbs the TUI.
fn open_with_system(path: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Standard base64 alphabet (RFC 4648). Tiny local encoder so the reader's
/// OSC 52 copy needs no new dependency (S27 constraint).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Copies `text` to the system clipboard with the OSC 52 escape sequence,
/// written directly to the real terminal (S27). Works over SSH in terminals
/// that support OSC 52; a terminal that does not simply ignores it.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Emits a `Tick` roughly twice a second to drive the animated explainer
/// caret (S21). The UI thread decides whether a tick repaints, so this never
/// forces work while the popup is closed.
fn spawn_tick_thread(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("tick".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            if tx.send(Event::Tick).is_err() {
                break;
            }
        })
        .expect("spawn tick thread");
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
    let full = Rect::new(0, 0, size.width, size.height);
    // At spawn nothing is minimized or zoomed, so this is always Some.
    let inner = app.chat_inner(full).unwrap_or(full);

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
fn redraw(terminal: &mut Terminal<Backend>, app: &mut App, pty: &mut PtyPane, shell: Option<&mut PtyPane>) -> Result<()> {
    let size = terminal.size()?;
    // When the chat is minimized or hidden by a zoomed pane, keep the PTY at its
    // last real size so gentle-shell does not reflow (S4, S14).
    if let Some(inner) = app.chat_inner(Rect::new(0, 0, size.width, size.height)) {
        pty.resize(inner.height, inner.width);
    }
    // Resize the shell to the terminal box captured on the previous frame (S24).
    let shell_ref = shell.map(|s| {
        if let Some(area) = app.term_area {
            s.resize(area.height.max(1), area.width.max(1));
        }
        &*s
    });
    terminal.draw(|f| draw(f, app, Some(&*pty), shell_ref))?;
    Ok(())
}

/// Spawns the user's $SHELL in its own PTY for the Agent-changes terminal (S24).
/// Reuses the generic [`PtyPane`] infrastructure; background jobs run normally
/// under the PTY's process group.
fn spawn_shell(root: &Path, tx: Sender<Event>, pending: Arc<AtomicBool>) -> Result<PtyPane> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.cwd(root);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GENTLE_PARTITIONER", "1");
    let notify = move || {
        if !pending.swap(true, Ordering::AcqRel) {
            let _ = tx.send(Event::ShellOutput);
        }
    };
    // A placeholder size; the first redraw resizes it to the real terminal box.
    PtyPane::spawn(cmd, 10, 40, notify).context("spawning the agent-changes shell")
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_the_rfc4648_test_vectors() {
        // S27: the OSC 52 copy relies on a correct standard base64 encoding.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
