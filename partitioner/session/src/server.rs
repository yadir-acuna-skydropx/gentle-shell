//! The detached session server (S34): owns the PTY running the user command
//! (e.g. gentle-partitioner) and relays raw bytes to at most one attached
//! client over a unix socket. It survives client disconnects and exits only
//! when its child exits, cleaning up the socket.

use std::fs;
use std::io::Read;
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use portable_pty::CommandBuilder;

use crate::meta::{Meta, now_secs, write_attached};
use crate::peer::authorize;
use crate::proto::{ClientMsg, ServerMsg};
use crate::pty::Pty;

/// Everything the detached server needs; passed on the re-exec command line.
pub struct ServerArgs {
    pub socket: PathBuf,
    pub meta: PathBuf,
    pub attached: PathBuf,
    pub folder: String,
    pub cmd: Vec<String>,
}

#[derive(Clone)]
struct ClientHandle {
    out: Arc<UnixStream>,
    generation: u64,
}

struct Shared {
    pty: Mutex<Pty>,
    client: Mutex<Option<ClientHandle>>,
    /// Output produced while no client is attached, flushed on the next attach
    /// so nothing is lost across a disconnect (bounded; oldest bytes drop).
    pending: Mutex<Vec<u8>>,
    attached: PathBuf,
}

const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
/// Cap on buffered detached output (1 MiB): a TUI repaints on reattach anyway.
const PENDING_CAP: usize = 1 << 20;

/// Binds the listener, removing a stale socket but refusing to steal a live one.
fn bind_listener(socket: &Path) -> Result<UnixListener> {
    match UnixListener::bind(socket) {
        Ok(l) => return Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e).context("binding session socket"),
    }
    // Someone may already be serving: if we can connect, step aside.
    if UnixStream::connect(socket).is_ok() {
        bail!("another gentle-session server already owns {}", socket.display());
    }
    // Stale socket file: remove and rebind.
    let _ = fs::remove_file(socket);
    UnixListener::bind(socket).context("rebinding session socket")
}

fn cleanup(args: &ServerArgs) {
    let _ = fs::remove_file(&args.socket);
    let _ = fs::remove_file(&args.meta);
    let _ = fs::remove_file(&args.attached);
}

/// Sends a control `msg` (Exit/Detach) to the current client, if any.
fn send_to_client(shared: &Shared, msg: &ServerMsg) {
    let handle = { shared.client.lock().unwrap().clone() };
    if let Some(h) = handle {
        let mut w: &UnixStream = &h.out;
        if msg.write_to(&mut w).is_err() {
            drop_client(shared, h.generation);
        }
    }
}

/// Delivers raw PTY output to the current client, or buffers it (bounded) when
/// none is attached so a reattach still sees it.
fn deliver(shared: &Shared, bytes: &[u8]) {
    let handle = { shared.client.lock().unwrap().clone() };
    if let Some(h) = handle {
        let mut w: &UnixStream = &h.out;
        if ServerMsg::Data(bytes.to_vec()).write_to(&mut w).is_err() {
            drop_client(shared, h.generation);
            buffer(shared, bytes);
        }
    } else {
        buffer(shared, bytes);
    }
}

fn buffer(shared: &Shared, bytes: &[u8]) {
    let mut pending = shared.pending.lock().unwrap();
    pending.extend_from_slice(bytes);
    let len = pending.len();
    if len > PENDING_CAP {
        pending.drain(0..len - PENDING_CAP);
    }
}

fn drop_client(shared: &Shared, generation: u64) {
    let mut guard = shared.client.lock().unwrap();
    if guard.as_ref().map(|c| c.generation) == Some(generation) {
        *guard = None;
        let _ = write_attached(&shared.attached, false);
    }
}

pub fn run_server(args: ServerArgs) -> Result<()> {
    if args.cmd.is_empty() {
        bail!("server requires a command to run");
    }
    let listener = bind_listener(&args.socket)?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600)).context("chmod socket 0600")?;

    // Build the child command: inherit the environment, mark it as inside a
    // session so the launcher never nests, run it in the folder.
    let mut cmd = CommandBuilder::new(&args.cmd[0]);
    for a in &args.cmd[1..] {
        cmd.arg(a);
    }
    if !args.folder.is_empty() {
        cmd.cwd(&args.folder);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("GENTLE_SESSION", "1");

    let pty = Pty::spawn(cmd, INITIAL_ROWS, INITIAL_COLS).context("spawning session PTY")?;
    let reader = pty.reader().context("cloning PTY reader")?;
    let child_pid = pty.child_pid().map(|p| p as i32).unwrap_or(0);

    // Record metadata for `list` / `kill`.
    let meta = Meta {
        folder: args.folder.clone(),
        server_pid: std::process::id() as i32,
        child_pid,
        started: now_secs(),
    };
    let _ = meta.write(&args.meta);
    let _ = write_attached(&args.attached, false);

    let shared = Arc::new(Shared {
        pty: Mutex::new(pty),
        client: Mutex::new(None),
        pending: Mutex::new(Vec::new()),
        attached: args.attached.clone(),
    });

    // Relay thread: PTY master -> current client, raw. On EOF the child has
    // exited; reap it, tell the client, clean up and exit the process.
    let relay_shared = shared.clone();
    let relay_args = ServerArgs {
        socket: args.socket.clone(),
        meta: args.meta.clone(),
        attached: args.attached.clone(),
        folder: args.folder.clone(),
        cmd: Vec::new(),
    };
    std::thread::Builder::new()
        .name("session-relay".into())
        .spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => deliver(&relay_shared, &buf[..n]),
                }
            }
            let code = relay_shared.pty.lock().unwrap().wait();
            send_to_client(&relay_shared, &ServerMsg::Exit(code));
            cleanup(&relay_args);
            std::process::exit(0);
        })
        .context("spawning relay thread")?;

    // Accept loop: one client at a time; a new attach takes over the old one.
    let generation = AtomicU64::new(0);
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !authorize(&stream) {
            // Different user (or creds unreadable): refuse.
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        let stream = Arc::new(stream);

        // Take over from any existing client. Under the client lock we also
        // flush buffered detached output to the new stream BEFORE storing it,
        // so the relay only appends live bytes afterwards (order preserved).
        {
            let mut guard = shared.client.lock().unwrap();
            if let Some(old) = guard.take() {
                let mut w: &UnixStream = &old.out;
                let _ = ServerMsg::Detach.write_to(&mut w);
                let _ = old.out.shutdown(Shutdown::Both);
            }
            let pend = std::mem::take(&mut *shared.pending.lock().unwrap());
            if !pend.is_empty() {
                let mut w: &UnixStream = &stream;
                let _ = ServerMsg::Data(pend).write_to(&mut w);
            }
            *guard = Some(ClientHandle { out: stream.clone(), generation: generation_id });
        }
        let _ = write_attached(&shared.attached, true);

        // Per-connection input thread: client -> PTY, plus resize handling.
        let input_shared = shared.clone();
        let input_stream = stream.clone();
        std::thread::Builder::new()
            .name("session-input".into())
            .spawn(move || {
                let mut r: &UnixStream = &input_stream;
                let mut first_resize = true;
                loop {
                    match ClientMsg::read_from(&mut r) {
                        Ok(Some(ClientMsg::Data(bytes))) => {
                            input_shared.pty.lock().unwrap().write_input(&bytes);
                        }
                        Ok(Some(ClientMsg::Resize { rows, cols })) => {
                            let mut pty = input_shared.pty.lock().unwrap();
                            pty.resize(rows, cols);
                            // On attach force a full repaint even if the size is
                            // unchanged (S34).
                            if first_resize {
                                pty.force_redraw();
                                first_resize = false;
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                // This client went away: if it is still the active one, mark
                // detached. The server (and PTY) keep running.
                let mut guard = input_shared.client.lock().unwrap();
                if guard.as_ref().map(|c| c.generation) == Some(generation_id) {
                    *guard = None;
                    let _ = write_attached(&input_shared.attached, false);
                }
            })
            .context("spawning input thread")?;
    }
    cleanup(&args);
    Ok(())
}
