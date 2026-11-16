//! Session metadata sidecar (S34): a tiny `key=value` file written by the
//! server so `list` and `kill` can report and target a session without
//! connecting. Hand-rolled parse keeps the crate dependency-free.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Parsed session metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub folder: String,
    pub server_pid: i32,
    pub child_pid: i32,
    pub started: u64,
}

impl Meta {
    pub fn serialize(&self) -> String {
        format!(
            "folder={}\nserver_pid={}\nchild_pid={}\nstarted={}\n",
            self.folder, self.server_pid, self.child_pid, self.started
        )
    }

    pub fn parse(text: &str) -> Meta {
        let mut m = Meta::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "folder" => m.folder = v.trim().to_string(),
                "server_pid" => m.server_pid = v.trim().parse().unwrap_or(0),
                "child_pid" => m.child_pid = v.trim().parse().unwrap_or(0),
                "started" => m.started = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        m
    }

    /// Writes the meta file with 0600 permissions.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        use std::io::Write;
        let mut f = opts.open(path)?;
        f.write_all(self.serialize().as_bytes())
    }

    pub fn read(path: &Path) -> io::Result<Meta> {
        Ok(Meta::parse(&fs::read_to_string(path)?))
    }
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Writes the attach-marker file ("1" attached / "0" detached), 0600.
pub fn write_attached(path: &Path, attached: bool) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    use std::io::Write;
    let mut f = opts.open(path)?;
    f.write_all(if attached { b"1" } else { b"0" })
}

/// Reads the attach marker; false when missing or unreadable.
pub fn read_attached(path: &Path) -> bool {
    fs::read_to_string(path).map(|s| s.trim() == "1").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips_through_serialize_parse() {
        let m = Meta {
            folder: "/home/u/proj".into(),
            server_pid: 4242,
            child_pid: 4243,
            started: 1700000000,
        };
        assert_eq!(Meta::parse(&m.serialize()), m);
    }

    #[test]
    fn parse_tolerates_blank_and_unknown_lines() {
        let text = "folder=/x\n\n# note\nserver_pid=9\nbogus\nchild_pid=10\nstarted=5\n";
        let m = Meta::parse(text);
        assert_eq!(m.folder, "/x");
        assert_eq!(m.server_pid, 9);
        assert_eq!(m.child_pid, 10);
        assert_eq!(m.started, 5);
    }
}
