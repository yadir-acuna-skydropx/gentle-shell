//! Git pre-commit hook management (S35).
//!
//! `gentle-sec hook install` drops a `pre-commit` hook that runs
//! `gentle-sec scan --staged --fail-on high`; a non-zero exit blocks the commit
//! (`git commit --no-verify` bypasses it, documented in the status output). An
//! existing pre-commit hook is never overwritten silently: it is moved to
//! `pre-commit.gentle-orig` and invoked first (chained). `uninstall` restores
//! the original. The hooks directory honours `core.hooksPath`.
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Marker line identifying a hook written by us (so install/uninstall/status can
/// recognise it without clobbering a foreign hook).
pub const MARKER: &str = "# gentle-sec pre-commit hook (managed)";

/// Name of the chained-aside original hook.
pub const ORIG_NAME: &str = "pre-commit.gentle-orig";

/// The result of an install/uninstall, for the CLI message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    AlreadyInstalled,
    Chained,
    Uninstalled,
    Restored,
    NotInstalled,
}

/// The pre-commit hook script. It runs any chained original hook first, then
/// `gentle-sec` when it is on PATH (a missing binary never blocks a commit).
pub fn hook_script() -> String {
    format!(
        "#!/bin/sh\n\
{MARKER}\n\
# Installed by `gentle-sec hook install`. Bypass once with `git commit --no-verify`.\n\
HOOK_DIR=\"$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\"\n\
if [ -x \"$HOOK_DIR/{ORIG_NAME}\" ]; then\n\
    \"$HOOK_DIR/{ORIG_NAME}\" \"$@\" || exit $?\n\
fi\n\
if command -v gentle-sec >/dev/null 2>&1; then\n\
    gentle-sec scan --staged --fail-on high || exit $?\n\
fi\n"
    )
}

/// True when `content` is a hook we wrote.
pub fn is_ours(content: &str) -> bool {
    content.contains(MARKER)
}

/// Resolves the repo's hooks directory, honouring `core.hooksPath`, via
/// `git rev-parse --git-path hooks`. Falls back to `<repo>/.git/hooks`.
pub fn resolve_hooks_dir(repo_root: &Path) -> io::Result<PathBuf> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
    }
    Ok(repo_root.join(".git").join("hooks"))
}

/// Installs (or chains) the pre-commit hook in `hooks_dir`.
pub fn install(hooks_dir: &Path) -> io::Result<Outcome> {
    std::fs::create_dir_all(hooks_dir)?;
    let hook = hooks_dir.join("pre-commit");
    let mut chained = false;
    if hook.exists() {
        let current = std::fs::read_to_string(&hook).unwrap_or_default();
        if is_ours(&current) {
            // Refresh the script (idempotent) but report "already installed".
            write_executable(&hook, &hook_script())?;
            return Ok(Outcome::AlreadyInstalled);
        }
        // A foreign hook: move it aside so we can chain to it. Never clobber.
        let orig = hooks_dir.join(ORIG_NAME);
        if !orig.exists() {
            std::fs::rename(&hook, &orig)?;
        }
        chained = true;
    }
    write_executable(&hook, &hook_script())?;
    Ok(if chained { Outcome::Chained } else { Outcome::Installed })
}

/// Uninstalls our hook, restoring a chained original if present.
pub fn uninstall(hooks_dir: &Path) -> io::Result<Outcome> {
    let hook = hooks_dir.join("pre-commit");
    if !hook.exists() {
        return Ok(Outcome::NotInstalled);
    }
    let current = std::fs::read_to_string(&hook).unwrap_or_default();
    if !is_ours(&current) {
        // Not ours: leave it untouched.
        return Ok(Outcome::NotInstalled);
    }
    std::fs::remove_file(&hook)?;
    let orig = hooks_dir.join(ORIG_NAME);
    if orig.exists() {
        std::fs::rename(&orig, &hook)?;
        return Ok(Outcome::Restored);
    }
    Ok(Outcome::Uninstalled)
}

/// Describes the current hook state for `hook status`.
pub fn status(hooks_dir: &Path) -> String {
    let hook = hooks_dir.join("pre-commit");
    if !hook.exists() {
        return format!("pre-commit hook: not installed ({})", hooks_dir.display());
    }
    let current = std::fs::read_to_string(&hook).unwrap_or_default();
    if is_ours(&current) {
        let chained = hooks_dir.join(ORIG_NAME).exists();
        let base = format!("pre-commit hook: installed ({})", hooks_dir.display());
        let chain = if chained { " + chained original (pre-commit.gentle-orig)" } else { "" };
        format!("{base}{chain}\nBypass once with: git commit --no-verify")
    } else {
        format!(
            "pre-commit hook: a non-gentle hook is present ({}); `hook install` will chain it",
            hooks_dir.display()
        )
    }
}

/// Writes `content` to `path` with mode 0755 on Unix.
fn write_executable(path: &Path, content: &str) -> io::Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gsec-hook-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn install_writes_our_hook() {
        let dir = tempdir("install");
        assert_eq!(install(&dir).unwrap(), Outcome::Installed);
        let content = std::fs::read_to_string(dir.join("pre-commit")).unwrap();
        assert!(is_ours(&content));
        assert!(content.contains("gentle-sec scan --staged --fail-on high"));
        // Second install is idempotent.
        assert_eq!(install(&dir).unwrap(), Outcome::AlreadyInstalled);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_chains_an_existing_hook_and_uninstall_restores_it() {
        let dir = tempdir("chain");
        let existing = "#!/bin/sh\necho original\n";
        std::fs::write(dir.join("pre-commit"), existing).unwrap();
        assert_eq!(install(&dir).unwrap(), Outcome::Chained);
        // Original preserved.
        let orig = std::fs::read_to_string(dir.join(ORIG_NAME)).unwrap();
        assert_eq!(orig, existing);
        // Our hook calls the original first.
        let hook = std::fs::read_to_string(dir.join("pre-commit")).unwrap();
        assert!(is_ours(&hook));
        assert!(hook.contains(ORIG_NAME));
        // Uninstall restores the original and removes the aside copy.
        assert_eq!(uninstall(&dir).unwrap(), Outcome::Restored);
        let restored = std::fs::read_to_string(dir.join("pre-commit")).unwrap();
        assert_eq!(restored, existing);
        assert!(!dir.join(ORIG_NAME).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uninstall_leaves_a_foreign_hook_untouched() {
        let dir = tempdir("foreign");
        let existing = "#!/bin/sh\necho foreign\n";
        std::fs::write(dir.join("pre-commit"), existing).unwrap();
        assert_eq!(uninstall(&dir).unwrap(), Outcome::NotInstalled);
        assert_eq!(std::fs::read_to_string(dir.join("pre-commit")).unwrap(), existing);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uninstall_plain_removes_our_hook() {
        let dir = tempdir("plain-uninstall");
        install(&dir).unwrap();
        assert_eq!(uninstall(&dir).unwrap(), Outcome::Uninstalled);
        assert!(!dir.join("pre-commit").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn status_reports_states() {
        let dir = tempdir("status");
        assert!(status(&dir).contains("not installed"));
        install(&dir).unwrap();
        assert!(status(&dir).contains("installed"));
        assert!(status(&dir).contains("--no-verify"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
