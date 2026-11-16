//! The gentle-session client (S34): a thin dtach-like relay. It puts the real
//! terminal in raw mode and shuttles raw bytes both ways, so CSI-u/kitty keys,
//! mouse, bracketed paste and OSC 52 pass through unchanged (no emulation). It
//! sends the window size on attach and on SIGWINCH. When the terminal closes
//! the client exits and the server keeps running. A detach key sequence
//! (Ctrl+\ Ctrl+\) detaches without killing the session.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::proto::{ClientMsg, ServerMsg};

/// The client-side detach sequence: Ctrl+\ pressed twice (0x1c 0x1c). Chosen so
/// it does NOT collide with the partitioner prefix (Ctrl+], 0x1d). Documented
/// in the README and the launcher.
pub const DETACH_BYTE: u8 = 0x1c;

/// Restores the terminal's original attributes on drop.
struct RawGuard {
    fd: i32,
    orig: Option<libc::termios>,
}

impl RawGuard {
    fn enter(fd: i32) -> Self {
        // Only a real terminal can go raw; a pipe (as in tests) is left alone.
        if unsafe { libc::isatty(fd) } != 1 {
            return Self { fd, orig: None };
        }
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return Self { fd, orig: None };
        }
        let mut raw = orig;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        Self { fd, orig: Some(orig) }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.orig {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &orig) };
        }
    }
}

/// Queries the terminal window size; falls back to 24x80 when there is no tty.
fn window_size(fd: i32) -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

/// Global write end of the SIGWINCH self-pipe (async-signal-safe handler only
/// calls write()).
static WINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn winch_handler(_sig: libc::c_int) {
    let fd = WINCH_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [1u8];
        unsafe { libc::write(fd, byte.as_ptr() as *const libc::c_void, 1) };
    }
}

/// Shared write side of the socket, guarded so the input and SIGWINCH threads
/// never interleave frames.
struct Writer {
    stream: Arc<UnixStream>,
    lock: Mutex<()>,
}

impl Writer {
    fn send(&self, msg: &ClientMsg) {
        let _g = self.lock.lock().unwrap();
        let mut w: &UnixStream = &self.stream;
        let _ = msg.write_to(&mut w);
    }
}

/// Connects to `socket` and relays. Convenience wrapper over
/// [`run_client_stream`] for the already-live path.
pub fn run_client(socket: &Path) -> Result<i32> {
    let stream = UnixStream::connect(socket).context("connecting to session socket")?;
    run_client_stream(stream)
}

/// Relays on an already-connected `stream` until the terminal closes, a detach
/// key is pressed, or the child exits. Returns the exit code to propagate (the
/// child's status when it exits, else 0 for a plain detach).
///
/// The caller passes the real client connection (never a throwaway probe): the
/// server flushes any buffered detached output to the first connection it sees,
/// so a probe would steal it.
pub fn run_client_stream(stream: UnixStream) -> Result<i32> {
    let stream = Arc::new(stream);
    let stdin_fd = libc::STDIN_FILENO;
    let _raw = RawGuard::enter(stdin_fd);

    let writer = Arc::new(Writer { stream: stream.clone(), lock: Mutex::new(()) });

    // Announce our size immediately so the server resizes + repaints on attach.
    let (rows, cols) = window_size(stdin_fd);
    writer.send(&ClientMsg::Resize { rows, cols });

    let done = Arc::new(AtomicBool::new(false));
    let got_exit = Arc::new(AtomicBool::new(false));
    let exit_code = Arc::new(AtomicI32::new(0));

    // Output thread: server -> stdout, raw. Handles the exit/detach notices.
    let out_stream = stream.clone();
    let out_done = done.clone();
    let out_got_exit = got_exit.clone();
    let out_code = exit_code.clone();
    let out_thread = std::thread::Builder::new()
        .name("session-stdout".into())
        .spawn(move || {
            let mut r: &UnixStream = &out_stream;
            let mut stdout = std::io::stdout();
            loop {
                match ServerMsg::read_from(&mut r) {
                    Ok(Some(ServerMsg::Data(bytes))) => {
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                    Ok(Some(ServerMsg::Exit(code))) => {
                        out_code.store(code, Ordering::SeqCst);
                        out_got_exit.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(Some(ServerMsg::Detach)) => {
                        let _ = stdout.write_all(b"\r\n[gentle-session] detached: another client attached\r\n");
                        let _ = stdout.flush();
                        break;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            out_done.store(true, Ordering::SeqCst);
        })
        .context("spawning stdout thread")?;

    // SIGWINCH self-pipe + forwarder thread.
    install_winch(&writer, stdin_fd, done.clone());

    // Main input loop: stdin -> server, with detach detection. Poll so we also
    // notice `done` (set by the output thread) promptly without blocking.
    let mut pending_detach = false;
    let mut buf = [0u8; 8192];
    loop {
        if done.load(Ordering::SeqCst) {
            break;
        }
        let mut pfd = libc::pollfd { fd: stdin_fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 200) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if r == 0 || pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // EOF / HUP: the terminal closed. Detach; the server keeps running.
            break;
        }
        let mut out = Vec::with_capacity(n as usize);
        let mut detach = false;
        for &b in &buf[..n as usize] {
            if pending_detach {
                pending_detach = false;
                if b == DETACH_BYTE {
                    detach = true;
                    break;
                }
                out.push(DETACH_BYTE);
                out.push(b);
            } else if b == DETACH_BYTE {
                pending_detach = true;
            } else {
                out.push(b);
            }
        }
        if !out.is_empty() {
            writer.send(&ClientMsg::Data(out));
        }
        if detach {
            // Clean detach: drop our connection; the session survives.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            print_detached_notice();
            return Ok(0);
        }
    }

    // Unblock the output thread and reap it.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = out_thread.join();

    if got_exit.load(Ordering::SeqCst) {
        Ok(exit_code.load(Ordering::SeqCst))
    } else {
        Ok(0)
    }
}

fn print_detached_notice() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\r\n[gentle-session] detached; session still running\r\n");
    let _ = stdout.flush();
}

/// Installs the SIGWINCH handler and a forwarder thread that re-sends the
/// current window size whenever the terminal is resized.
fn install_winch(writer: &Arc<Writer>, fd: i32, done: Arc<AtomicBool>) {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    WINCH_WRITE_FD.store(write_fd, Ordering::SeqCst);

    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = winch_handler as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    action.sa_flags = libc::SA_RESTART;
    unsafe { libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) };

    let writer = writer.clone();
    std::thread::Builder::new()
        .name("session-winch".into())
        .spawn(move || {
            let mut byte = [0u8; 64];
            loop {
                let n = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, byte.len()) };
                if n <= 0 || done.load(Ordering::SeqCst) {
                    break;
                }
                let (rows, cols) = window_size(fd);
                writer.send(&ClientMsg::Resize { rows, cols });
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_byte_does_not_collide_with_the_partitioner_prefix() {
        // Partitioner prefix is Ctrl+] (0x1d); our detach is Ctrl+\ (0x1c).
        assert_eq!(DETACH_BYTE, 0x1c);
        assert_ne!(DETACH_BYTE, 0x1d);
    }

    #[test]
    fn window_size_falls_back_without_a_tty() {
        // A pipe fd has no winsize; we must fall back rather than send 0x0.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (rows, cols) = window_size(fds[0]);
        assert_eq!((rows, cols), (24, 80));
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
