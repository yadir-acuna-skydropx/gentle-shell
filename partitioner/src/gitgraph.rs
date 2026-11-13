//! Local git commit/branch DAG and worktrees for the graphical git overlay
//! (S20, T21a). The partitioner computes this in-process from `git` so it does
//! not depend on the gentle-apps GitHub connector (which has no commit
//! topology). The parsing and lane assignment are pure and unit-tested; only
//! [`collect`] shells out.

use std::path::Path;
use std::process::Command;

/// How many commits to read, bounding render cost on large repos.
const MAX_COMMITS: usize = 200;

/// One commit parsed from `git log --parents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub full: String,
    pub short: String,
    pub parents: Vec<String>,
    /// Ref labels on this commit (branch/tag names), `HEAD -> ` and `tag: `
    /// prefixes normalized away (tags keep a `tag:` marker).
    pub refs: Vec<String>,
    pub subject: String,
}

/// A commit placed on a lane, with the lanes occupied on its row so the overlay
/// can draw vertical continuity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    /// Column (lane index) where this commit's node sits.
    pub col: usize,
    /// Which lanes are occupied on this row (drawn as `│`), including `col`.
    pub active: Vec<bool>,
    pub commit: Commit,
}

/// A local worktree from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    /// Checked-out branch short name, or empty when detached.
    pub branch: String,
    pub detached: bool,
}

/// Everything the overlay needs; empty + `error` set when git is unavailable.
#[derive(Debug, Default, Clone)]
pub struct GitGraphData {
    pub rows: Vec<GraphRow>,
    pub worktrees: Vec<Worktree>,
    pub error: Option<String>,
}

/// Parses one ref-decoration string like `HEAD -> main, origin/main, tag: v1`.
fn parse_refs(decor: &str) -> Vec<String> {
    decor
        .split(',')
        .filter_map(|raw| {
            let r = raw.trim();
            if r.is_empty() {
                return None;
            }
            let r = r.strip_prefix("HEAD -> ").unwrap_or(r);
            if let Some(tag) = r.strip_prefix("tag: ") {
                Some(format!("tag:{tag}"))
            } else {
                Some(r.to_string())
            }
        })
        .collect()
}

/// Parses `git log --parents` output formatted as
/// `%H\0%h\0%P\0%D\0%s` lines into commits, in the given order.
pub fn parse_log(output: &str) -> Vec<Commit> {
    let mut out = Vec::new();
    for line in output.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\u{0}').collect();
        if f.len() < 5 {
            continue;
        }
        out.push(Commit {
            full: f[0].to_string(),
            short: f[1].to_string(),
            parents: f[2].split_whitespace().map(|s| s.to_string()).collect(),
            refs: parse_refs(f[3]),
            // Subject is the last field; it never contains a NUL.
            subject: f[4].to_string(),
        });
    }
    out
}

/// Assigns commits to lanes left to right. Each lane holds the hash of the next
/// commit expected in it; a commit takes the first lane waiting for it (new lane
/// if none), merges collapse the other lanes pointing at it, and the commit's
/// lane is replaced by its first parent while extra parents open new lanes.
pub fn build_rows(commits: Vec<Commit>) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let col = match lanes.iter().position(|l| l.as_deref() == Some(c.full.as_str())) {
            Some(i) => i,
            None => {
                lanes.push(Some(c.full.clone()));
                lanes.len() - 1
            }
        };
        // Snapshot of occupied lanes for this row (the node's lane is occupied).
        let active: Vec<bool> = lanes.iter().map(|l| l.is_some()).collect();
        // Collapse any other lanes that were also waiting for this commit (merge).
        for (i, l) in lanes.iter_mut().enumerate() {
            if i != col && l.as_deref() == Some(c.full.as_str()) {
                *l = None;
            }
        }
        // The node's lane continues with the first parent; extra parents branch.
        match c.parents.first() {
            Some(first) => lanes[col] = Some(first.clone()),
            None => lanes[col] = None,
        }
        for p in c.parents.iter().skip(1) {
            if let Some(slot) = lanes.iter_mut().find(|l| l.is_none()) {
                *slot = Some(p.clone());
            } else {
                lanes.push(Some(p.clone()));
            }
        }
        rows.push(GraphRow { col, active, commit: c });
    }
    rows
}

/// Parses `git worktree list --porcelain` into worktrees.
pub fn parse_worktrees(output: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut branch = String::new();
    let mut detached = false;
    let flush = |path: &mut Option<String>, branch: &mut String, detached: &mut bool, out: &mut Vec<Worktree>| {
        if let Some(p) = path.take() {
            out.push(Worktree { path: p, branch: std::mem::take(branch), detached: *detached });
        }
        *detached = false;
    };
    for line in output.split('\n') {
        let line = line.trim_end();
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut detached, &mut out);
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if line == "detached" {
            detached = true;
        } else if line.is_empty() {
            flush(&mut path, &mut branch, &mut detached, &mut out);
        }
    }
    flush(&mut path, &mut branch, &mut detached, &mut out);
    out
}

/// Runs git in `repo_root` and builds the graph data; never panics — any
/// failure is reported through `GitGraphData::error`.
pub fn collect(repo_root: &Path) -> GitGraphData {
    let log = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "log",
            "--all",
            "--parents",
            "--date-order",
            &format!("--max-count={MAX_COMMITS}"),
            "--pretty=format:%H%x00%h%x00%P%x00%D%x00%s",
        ])
        .output();
    let log = match log {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            return GitGraphData {
                error: Some(format!("git log failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                ..Default::default()
            }
        }
        Err(e) => return GitGraphData { error: Some(format!("git not available: {e}")), ..Default::default() },
    };
    let rows = build_rows(parse_log(&log));
    let worktrees = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_worktrees(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    GitGraphData { rows, worktrees, error: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_reads_fields_parents_and_refs() {
        // A ref-decorated head, a normal commit, a merge (two parents) and a
        // root commit (no parents).
        let out = "aaa\0aaa\0bbb ccc\0HEAD -> main, origin/main, tag: v1\0merge both\n\
                   bbb\0bbb\0ddd\0feature\0a feature\n\
                   ccc\0ccc\0ddd\0\0another\n\
                   ddd\0ddd\0\0\0root commit";
        let c = parse_log(out);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].parents, vec!["bbb", "ccc"]);
        assert_eq!(c[0].refs, vec!["main", "origin/main", "tag:v1"]);
        assert_eq!(c[0].subject, "merge both");
        assert_eq!(c[1].refs, vec!["feature"]);
        assert!(c[3].parents.is_empty(), "root commit has no parents");
    }

    #[test]
    fn build_rows_keeps_a_straight_chain_in_lane_zero() {
        let commits = parse_log(
            "aaa\0aaa\0bbb\0\0c3\n\
             bbb\0bbb\0ccc\0\0c2\n\
             ccc\0ccc\0\0\0c1",
        );
        let rows = build_rows(commits);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.col == 0), "a linear history stays in lane 0");
        assert_eq!(rows[0].active, vec![true]);
    }

    #[test]
    fn build_rows_opens_and_collapses_a_lane_for_a_merge() {
        // m merges a and b; a and b both descend from base r.
        let commits = parse_log(
            "mmm\0mmm\0aaa bbb\0\0merge\n\
             aaa\0aaa\0rrr\0\0left\n\
             bbb\0bbb\0rrr\0\0right\n\
             rrr\0rrr\0\0\0base",
        );
        let rows = build_rows(commits);
        // Merge node sits in lane 0 and opens a second lane for its 2nd parent.
        assert_eq!(rows[0].col, 0);
        assert_eq!(rows[0].commit.subject, "merge");
        // The right parent gets its own lane, so some row spans two lanes.
        assert!(rows.iter().any(|r| r.active.len() >= 2), "a merge introduces a 2nd lane");
        // The base commit collapses back to a single lane.
        let base = rows.last().unwrap();
        assert_eq!(base.commit.subject, "base");
        assert_eq!(base.col, 0);
    }

    #[test]
    fn parse_worktrees_reads_branches_and_detached() {
        let out = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                   worktree /repo/wt\nHEAD bbb\ndetached\n";
        let wt = parse_worktrees(out);
        assert_eq!(wt.len(), 2);
        assert_eq!(wt[0].path, "/repo");
        assert_eq!(wt[0].branch, "main");
        assert!(!wt[0].detached);
        assert_eq!(wt[1].path, "/repo/wt");
        assert!(wt[1].detached);
        assert_eq!(wt[1].branch, "");
    }
}
