//! Sidebar model (S4): folders, files, formats, sizes and git worktree status.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl GitStatus {
    pub fn marker(self) -> char {
        match self {
            GitStatus::Modified => 'M',
            GitStatus::Added => 'A',
            GitStatus::Deleted => 'D',
            GitStatus::Renamed => 'R',
            GitStatus::Untracked => '?',
            GitStatus::Conflicted => '!',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the scanned root.
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub size: u64,
    /// Lower-case extension, `dir` for directories, `-` when there is none.
    pub format: String,
    /// For directories: set when any descendant has a change.
    pub status: Option<GitStatus>,
}

/// Directories never listed: VCS metadata and build/dependency output.
pub const SKIPPED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// Parses `git status --porcelain=v1` output into paths relative to the repo root.
pub fn parse_porcelain(output: &str) -> HashMap<PathBuf, GitStatus> {
    let mut map = HashMap::new();
    for line in output.lines() {
        if line.len() < 4 || !line.is_char_boundary(3) {
            continue;
        }
        let (code, rest) = (&line[..2], &line[3..]);
        let status = match code {
            "??" => GitStatus::Untracked,
            c if c.contains('U') || c == "AA" || c == "DD" => GitStatus::Conflicted,
            c if c.contains('R') || c.contains('C') => GitStatus::Renamed,
            c if c.contains('A') => GitStatus::Added,
            c if c.contains('D') => GitStatus::Deleted,
            _ => GitStatus::Modified,
        };
        let path = match rest.rsplit_once(" -> ") {
            Some((_, new)) if status == GitStatus::Renamed => new,
            _ => rest,
        };
        map.insert(PathBuf::from(unquote(path)), status);
    }
    map
}

fn unquote(path: &str) -> String {
    match path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => path.to_string(),
    }
}

/// Lists `root`, descending only into directories in `expanded` (relative paths).
/// Directories first, then files, each group sorted by name.
pub fn scan(root: &Path, expanded: &HashSet<PathBuf>, statuses: &HashMap<PathBuf, GitStatus>) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(root, Path::new(""), 0, expanded, statuses, &mut out);
    out
}

fn walk(
    root: &Path,
    rel: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    statuses: &HashMap<PathBuf, GitStatus>,
    out: &mut Vec<Entry>,
) {
    let Ok(read) = std::fs::read_dir(root.join(rel)) else { return };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(meta) = item.metadata() else { continue };
        let path = rel.join(&name);
        if meta.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                let status = statuses.keys().any(|k| k.starts_with(&path)).then_some(GitStatus::Modified);
                dirs.push(Entry { path, name, depth, is_dir: true, size: 0, format: "dir".into(), status });
            }
        } else {
            let format = Path::new(&name)
                .extension()
                .map_or_else(|| "-".to_string(), |e| e.to_string_lossy().to_lowercase());
            let status = statuses.get(&path).copied();
            files.push(Entry { path, name, depth, is_dir: false, size: meta.len(), format, status });
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for dir in dirs {
        let open = expanded.contains(&dir.path);
        let path = dir.path.clone();
        out.push(dir);
        if open {
            walk(root, &path, depth + 1, expanded, statuses, out);
        }
    }
    out.extend(files);
}

pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    for unit in ["K", "M", "G"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} T")
}

/// Runs `git status` in `root`; an empty map when it is not a repository.
pub fn git_status(root: &Path) -> HashMap<PathBuf, GitStatus> {
    std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_porcelain(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn porcelain_codes_map_to_statuses() {
        let out = " M src/a.rs\nM  src/b.rs\nA  c.rs\n?? notes/d.txt\n D e.rs\nR  old.rs -> new.rs\nUU f.rs\n";
        let s = parse_porcelain(out);
        assert_eq!(s.get(&p("src/a.rs")), Some(&GitStatus::Modified));
        assert_eq!(s.get(&p("src/b.rs")), Some(&GitStatus::Modified));
        assert_eq!(s.get(&p("c.rs")), Some(&GitStatus::Added));
        assert_eq!(s.get(&p("notes/d.txt")), Some(&GitStatus::Untracked));
        assert_eq!(s.get(&p("e.rs")), Some(&GitStatus::Deleted));
        assert_eq!(s.get(&p("new.rs")), Some(&GitStatus::Renamed));
        assert_eq!(s.get(&p("old.rs")), None);
        assert_eq!(s.get(&p("f.rs")), Some(&GitStatus::Conflicted));
        assert_eq!(s.len(), 7);
    }

    #[test]
    fn porcelain_ignores_blank_and_short_lines() {
        assert!(parse_porcelain("\n\nM\n").is_empty());
    }

    #[test]
    fn porcelain_unquotes_paths_with_spaces() {
        let s = parse_porcelain("?? \"my file.txt\"\n");
        assert_eq!(s.get(&p("my file.txt")), Some(&GitStatus::Untracked));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 K");
        assert_eq!(human_size(1_048_576), "1.0 M");
        assert_eq!(human_size(3 * 1_073_741_824), "3.0 G");
    }

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gentle-partitioner-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("README.md"), "hello").unwrap();
        fs::write(root.join("Makefile"), "").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/deep/x.TXT"), "x").unwrap();
        root
    }

    #[test]
    fn scan_lists_dirs_first_and_skips_vcs_and_build_output() {
        let root = fixture("top");
        let entries = scan(&root, &HashSet::new(), &HashMap::new());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["docs", "src", "Makefile", "README.md"]);
        let readme = &entries[3];
        assert_eq!((readme.size, readme.format.as_str(), readme.depth, readme.is_dir), (5, "md", 0, false));
        assert_eq!(entries[2].format, "-");
        assert_eq!(entries[1].format, "dir");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_descends_only_into_expanded_dirs() {
        let root = fixture("expand");
        let expanded: HashSet<PathBuf> = [p("src")].into();
        let entries = scan(&root, &expanded, &HashMap::new());
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, [p("docs"), p("src"), p("src/deep"), p("src/main.rs"), p("Makefile"), p("README.md")]);
        assert_eq!(entries[3].depth, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_marks_files_and_their_parent_dirs_with_git_status() {
        let root = fixture("status");
        let statuses: HashMap<PathBuf, GitStatus> =
            [(p("src/deep/x.TXT"), GitStatus::Untracked), (p("README.md"), GitStatus::Modified)].into();
        let expanded: HashSet<PathBuf> = [p("src"), p("src/deep")].into();
        let entries = scan(&root, &expanded, &statuses);
        let get = |path: &str| entries.iter().find(|e| e.path == p(path)).unwrap();
        assert_eq!(get("README.md").status, Some(GitStatus::Modified));
        assert_eq!(get("src/deep/x.TXT").status, Some(GitStatus::Untracked));
        assert_eq!(get("src/deep/x.TXT").format, "txt");
        assert_eq!(get("src").status, Some(GitStatus::Modified));
        assert_eq!(get("src/deep").status, Some(GitStatus::Modified));
        assert_eq!(get("docs").status, None);
        assert_eq!(get("src/main.rs").status, None);
        fs::remove_dir_all(root).unwrap();
    }
}
