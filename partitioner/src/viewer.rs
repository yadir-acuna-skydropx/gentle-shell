//! Center viewer (S5): the selected file on the left and the changes the
//! agents made to it (git diff against HEAD) on the right. Read-only in this
//! phase; manual editing is T6.
use std::path::{Path, PathBuf};
use std::process::Command;

/// Files larger than this are shown truncated.
pub const MAX_BYTES: usize = 1024 * 1024;
const TAB: &str = "    ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Meta,
    Hunk,
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub path: PathBuf,
    pub content: Vec<String>,
    pub diff: Vec<DiffLine>,
    pub content_scroll: usize,
    pub diff_scroll: usize,
}

pub fn classify_diff(text: &str) -> Vec<DiffLine> {
    text.lines()
        .map(|line| {
            let kind = if line.starts_with("@@") {
                DiffKind::Hunk
            } else if line.starts_with("+++ ") || line.starts_with("--- ") {
                DiffKind::Meta
            } else if line.starts_with('+') {
                DiffKind::Added
            } else if line.starts_with('-') {
                DiffKind::Removed
            } else if line.is_empty() || line.starts_with(' ') {
                DiffKind::Context
            } else {
                DiffKind::Meta
            };
            DiffLine { kind, text: line.replace('\t', TAB) }
        })
        .collect()
}

/// Reads a file for display: binary files and oversized files are reported
/// instead of dumped, tabs are expanded.
pub fn read_text(path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return vec!["[cannot read file]".into()];
    };
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return vec![format!("[binary file, {}]", crate::tree::human_size(bytes.len() as u64))];
    }
    let truncated = bytes.len() > MAX_BYTES;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
    let mut lines: Vec<String> = text.lines().map(|l| l.replace('\t', TAB)).collect();
    if truncated {
        lines.push(format!("[truncated at {}]", crate::tree::human_size(MAX_BYTES as u64)));
    }
    lines
}

fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(root).output().ok()?;
    // `diff --no-index` exits 1 when the files differ.
    matches!(out.status.code(), Some(0 | 1)).then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Agent changes for `rel` relative to HEAD, including untracked files.
pub fn git_diff(root: &Path, rel: &Path) -> Vec<DiffLine> {
    let meta = |t: &str| vec![DiffLine { kind: DiffKind::Meta, text: t.into() }];
    let rel_s = rel.to_string_lossy();
    let Some(tracked) = git_out(root, &["diff", "--no-color", "--no-ext-diff", "HEAD", "--", &rel_s]) else {
        return meta("not a git repository");
    };
    let mut text = tracked;
    if text.trim().is_empty() {
        let untracked = git_out(root, &["ls-files", "--others", "--exclude-standard", "--", &rel_s]).unwrap_or_default();
        if !untracked.trim().is_empty() {
            text = git_out(root, &["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null", &rel_s])
                .unwrap_or_default();
        }
    }
    if text.trim().is_empty() {
        return meta("no changes against HEAD");
    }
    classify_diff(&text)
}

impl Viewer {
    pub fn open(root: &Path, rel: &Path) -> Self {
        Self {
            path: rel.to_path_buf(),
            content: read_text(&root.join(rel)),
            diff: git_diff(root, rel),
            content_scroll: 0,
            diff_scroll: 0,
        }
    }

    /// Scrolls both sides, clamped to their length.
    pub fn scroll(&mut self, delta: isize) {
        let clamp = |pos: usize, len: usize| (pos as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        self.content_scroll = clamp(self.content_scroll, self.content.len());
        self.diff_scroll = clamp(self.diff_scroll, self.diff.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gentle-partitioner-viewer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn diff_lines_are_classified() {
        let d = classify_diff("diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n\\ No newline at end of file\n");
        let kinds: Vec<DiffKind> = d.iter().map(|l| l.kind).collect();
        use DiffKind::*;
        assert_eq!(kinds, [Meta, Meta, Meta, Meta, Hunk, Context, Removed, Added, Meta]);
        assert_eq!(d[7].text, "+new");
    }

    #[test]
    fn text_files_expand_tabs() {
        let d = tmp("text");
        fs::write(d.join("a.rs"), "fn x() {\n\tret\n}\n").unwrap();
        assert_eq!(read_text(&d.join("a.rs")), ["fn x() {", "    ret", "}"]);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn binary_and_missing_files_are_reported() {
        let d = tmp("bin");
        fs::write(d.join("img.png"), [0x89, b'P', 0, 1, 2]).unwrap();
        assert_eq!(read_text(&d.join("img.png")), ["[binary file, 5 B]"]);
        assert_eq!(read_text(&d.join("nope")), ["[cannot read file]"]);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn oversized_files_are_truncated() {
        let d = tmp("big");
        fs::write(d.join("big.txt"), "x\n".repeat(MAX_BYTES)).unwrap();
        let lines = read_text(&d.join("big.txt"));
        assert_eq!(lines.len(), MAX_BYTES / 2 + 1);
        assert_eq!(lines.last().unwrap(), "[truncated at 1.0 M]");
        fs::remove_dir_all(d).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn git_diff_shows_tracked_changes_untracked_files_and_clean_state() {
        let d = tmp("git");
        git(&d, &["init", "-q"]);
        fs::write(d.join("a.txt"), "one\n").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "init"]);
        assert_eq!(git_diff(&d, Path::new("a.txt")), [DiffLine { kind: DiffKind::Meta, text: "no changes against HEAD".into() }]);

        fs::write(d.join("a.txt"), "two\n").unwrap();
        let diff = git_diff(&d, Path::new("a.txt"));
        assert!(diff.contains(&DiffLine { kind: DiffKind::Removed, text: "-one".into() }));
        assert!(diff.contains(&DiffLine { kind: DiffKind::Added, text: "+two".into() }));

        fs::write(d.join("new.txt"), "fresh\n").unwrap();
        let diff = git_diff(&d, Path::new("new.txt"));
        assert!(diff.contains(&DiffLine { kind: DiffKind::Added, text: "+fresh".into() }));

        let v = Viewer::open(&d, Path::new("a.txt"));
        assert_eq!(v.content, ["two"]);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn scroll_is_clamped_per_side() {
        let mut v = Viewer {
            path: "x".into(),
            content: vec!["a".into(); 10],
            diff: vec![DiffLine { kind: DiffKind::Context, text: " a".into() }; 3],
            content_scroll: 0,
            diff_scroll: 0,
        };
        v.scroll(5);
        assert_eq!((v.content_scroll, v.diff_scroll), (5, 2));
        v.scroll(-50);
        assert_eq!((v.content_scroll, v.diff_scroll), (0, 0));
    }
}
