//! End-to-end lifecycle for gentle-session (S34), driving the real binary:
//! attach -> input/output -> disconnect -> server survives -> reattach ->
//! still alive -> kill removes the socket. Every read has a deadline so the
//! test can never hang.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_gentle-session");

struct TempEnv {
    root: PathBuf,
}

impl TempEnv {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "gs-e2e-{}-{}-{}",
            tag,
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        TempEnv { root }
    }
    fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }
}

impl Drop for TempEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Reads a child's stdout on a background thread into a shared buffer.
fn drain(child: &mut Child) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut out = child.stdout.take().expect("piped stdout");
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });
    buf
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn wait_for(buf: &Arc<Mutex<Vec<u8>>>, needle: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if contains(&buf.lock().unwrap(), needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

fn spawn_attach(env: &TempEnv) -> Child {
    Command::new(BIN)
        .args(["attach", "--dir"])
        .arg(env.work())
        .args(["--", "sh", "-c", "echo ready; cat"])
        .env("XDG_RUNTIME_DIR", env.runtime())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn attach")
}

fn session_dir(env: &TempEnv) -> PathBuf {
    env.runtime().join("gentle-session")
}

fn count_sockets(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
            .count(),
        Err(_) => 0,
    }
}

#[test]
fn attach_survives_disconnect_reattaches_and_kill_removes_socket() {
    let env = TempEnv::new("life");

    // --- first attach: input flows to the PTY and output comes back ---
    let mut a1 = spawn_attach(&env);
    let out1 = drain(&mut a1);
    assert!(wait_for(&out1, b"ready", Duration::from_secs(10)), "no 'ready' from the session");
    a1.stdin.as_mut().unwrap().write_all(b"hello-one\n").unwrap();
    assert!(wait_for(&out1, b"hello-one", Duration::from_secs(10)), "input not echoed back");

    // A live socket must exist, 0600, under a 0700 dir.
    let dir = session_dir(&env);
    assert_eq!(count_sockets(&dir), 1, "exactly one session socket");
    let dir_mode = std::fs::metadata(&dir).unwrap().mode_bits();
    assert_eq!(dir_mode, 0o700, "state dir is 0700");
    let sock = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
        .unwrap()
        .path();
    assert_eq!(std::fs::metadata(&sock).unwrap().mode_bits(), 0o600, "socket is 0600");

    // --- disconnect: killing the client must NOT kill the session ---
    a1.kill().unwrap();
    a1.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(count_sockets(&dir), 1, "server survived the client leaving");

    // --- reattach: the `cat` process is still alive and echoes again ---
    let mut a2 = spawn_attach(&env);
    let out2 = drain(&mut a2);
    a2.stdin.as_mut().unwrap().write_all(b"hello-two\n").unwrap();
    assert!(
        wait_for(&out2, b"hello-two", Duration::from_secs(10)),
        "reattached session did not echo (process not alive)"
    );

    // --- list shows the folder ---
    let listed = Command::new(BIN)
        .arg("list")
        .env("XDG_RUNTIME_DIR", env.runtime())
        .output()
        .unwrap();
    let list_text = String::from_utf8_lossy(&listed.stdout);
    assert!(list_text.contains("ATTACHED"), "list header: {list_text}");

    // --- kill terminates and removes the socket ---
    let killed = Command::new(BIN)
        .args(["kill", "--dir"])
        .arg(env.work())
        .env("XDG_RUNTIME_DIR", env.runtime())
        .output()
        .unwrap();
    assert!(killed.status.success(), "kill exited non-zero");
    let deadline = Instant::now() + Duration::from_secs(5);
    while count_sockets(&dir) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(count_sockets(&dir), 0, "kill removed the socket");

    // The reattached client should see the session gone and exit.
    let _ = a2.kill();
    let _ = a2.wait();

    // list now reports nothing.
    let listed = Command::new(BIN)
        .arg("list")
        .env("XDG_RUNTIME_DIR", env.runtime())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("no gentle-session sessions"),
        "session should be gone"
    );
}

// Small helper trait so the test reads mode bits without importing the ext at
// the top (keeps the unix-only import local).
trait ModeBits {
    fn mode_bits(&self) -> u32;
}
impl ModeBits for std::fs::Metadata {
    fn mode_bits(&self) -> u32 {
        use std::os::unix::fs::MetadataExt;
        self.mode() & 0o7777
    }
}
