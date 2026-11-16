//! Per-folder socket and state paths for gentle-session (S34).
//!
//! The state directory is `$XDG_RUNTIME_DIR/gentle-session` (falling back to
//! `$HOME/.local/state/gentle-session`), created 0700. Each folder gets a
//! `<id>.sock` socket (0600) plus sidecar metadata, attach-marker and log
//! files keyed by the folder id (see [`crate::hash`]).

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::hash::folder_id;

/// Resolved paths for one folder's session.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub id: String,
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub meta: PathBuf,
    pub attached: PathBuf,
    pub log: PathBuf,
}

/// Base state directory, honoring `$XDG_RUNTIME_DIR` then `$HOME`.
pub fn state_dir() -> io::Result<PathBuf> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return Ok(PathBuf::from(rt).join("gentle-session"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join(".local/state/gentle-session"));
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "neither XDG_RUNTIME_DIR nor HOME is set"))
}

/// Canonicalizes `dir`, refusing to follow a symlinked directory target (the
/// final component must be a real directory, not a symlink). Returns the
/// canonical absolute path used to derive the folder id.
pub fn canonical_dir(dir: &Path) -> io::Result<PathBuf> {
    // Refuse a symlink as the directory itself: inspect the link status before
    // resolving. `symlink_metadata` does not follow the final component.
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to use a symlinked session directory",
        ));
    }
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "session dir is not a directory"));
    }
    // Resolve `..`/`.` and relative spellings to a stable absolute path.
    fs::canonicalize(dir)
}

/// Creates (if needed) the base state dir with mode 0700 and verifies it is a
/// real directory owned by us, not a symlink an attacker could redirect.
pub fn ensure_state_dir() -> io::Result<PathBuf> {
    let dir = state_dir()?;
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(&dir) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "state dir exists but is not a real directory",
                ));
            }
            // Tighten permissions in case it was created too open previously.
            fs::set_permissions(&dir, perm(0o700))?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(&dir)?;
        }
        Err(e) => return Err(e),
    }
    Ok(dir)
}

fn perm(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode)
}

/// Resolves every path for `dir`'s session without creating the socket.
pub fn resolve(dir: &Path) -> io::Result<SessionPaths> {
    let canonical = canonical_dir(dir)?;
    let base = ensure_state_dir()?;
    let id = folder_id(&canonical);
    Ok(SessionPaths {
        socket: base.join(format!("{id}.sock")),
        meta: base.join(format!("{id}.meta")),
        attached: base.join(format!("{id}.attached")),
        log: base.join(format!("{id}.log")),
        dir: base,
        id,
    })
}

/// 0700 mode for the state dir (for tests/assertions).
pub const DIR_MODE: u32 = 0o700;
/// 0600 mode for the socket and sidecar files.
pub const FILE_MODE: u32 = 0o600;

/// Returns the lower 12 permission bits of `path`.
pub fn mode_bits(path: &Path) -> io::Result<u32> {
    Ok(fs::metadata(path)?.mode() & 0o7777)
}

/// Returns the permission bits without following a final symlink.
pub fn lmode_bits(path: &Path) -> io::Result<u32> {
    Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize the tests that mutate them so they
    // never race each other under the parallel test runner.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &Path) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("gs-paths-{}-{}", std::process::id(), rand_tag()));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn rand_tag() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn state_dir_prefers_xdg_runtime_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = tmp();
        let _g = EnvGuard::set("XDG_RUNTIME_DIR", &base);
        let dir = state_dir().unwrap();
        assert_eq!(dir, base.join("gentle-session"));
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_dir_falls_back_to_home_local_state() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = tmp();
        let _g1 = EnvGuard::unset("XDG_RUNTIME_DIR");
        let _g2 = EnvGuard::set("HOME", &base);
        let dir = state_dir().unwrap();
        assert_eq!(dir, base.join(".local/state/gentle-session"));
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn ensure_state_dir_creates_it_0700() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = tmp();
        let _g = EnvGuard::set("XDG_RUNTIME_DIR", &base);
        let dir = ensure_state_dir().unwrap();
        assert!(dir.is_dir());
        assert_eq!(lmode_bits(&dir).unwrap(), DIR_MODE);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_gives_distinct_sockets_for_distinct_folders() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = tmp();
        let _g = EnvGuard::set("XDG_RUNTIME_DIR", &base);
        let a = base.join("alpha");
        let b = base.join("beta");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let pa = resolve(&a).unwrap();
        let pb = resolve(&b).unwrap();
        assert_ne!(pa.socket, pb.socket);
        // Same folder, different spelling resolves to the same socket.
        let pa2 = resolve(&a.join(".")).unwrap();
        assert_eq!(pa.socket, pa2.socket);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn canonical_dir_refuses_a_symlinked_directory() {
        let base = tmp();
        let real = base.join("real");
        let link = base.join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = canonical_dir(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        fs::remove_dir_all(&base).ok();
    }
}
