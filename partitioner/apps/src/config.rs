//! `~/.config/gentle-partitioner/apps.toml` — non-secret Slack metadata.
//!
//! Only the **client id** (not a secret) and display metadata (team name/id,
//! user id) live here. The token never touches this file. Parsed by hand to
//! avoid a TOML dependency; the file only ever holds a `[slack]` table of
//! `key = "value"` string entries.
use std::io::Write;
use std::path::{Path, PathBuf};

/// Non-secret Slack metadata persisted alongside the keyring token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackMeta {
    pub client_id: Option<String>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
}

impl SlackMeta {
    /// True when nothing is configured (no client id and no metadata).
    pub fn is_empty(&self) -> bool {
        self.client_id.is_none()
            && self.team_id.is_none()
            && self.team_name.is_none()
            && self.user_id.is_none()
    }
}

/// Directory holding `apps.toml`: `<home>/.config/gentle-partitioner`.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("gentle-partitioner")
}

/// Path to `apps.toml`.
pub fn config_path(home: &Path) -> PathBuf {
    config_dir(home).join("apps.toml")
}

/// Reads the `[slack]` metadata. A missing file yields an empty [`SlackMeta`].
pub fn load(home: &Path) -> SlackMeta {
    let text = match std::fs::read_to_string(config_path(home)) {
        Ok(t) => t,
        Err(_) => return SlackMeta::default(),
    };
    parse(&text)
}

/// Parses the minimal `key = "value"` TOML we write (only the `[slack]` table).
pub fn parse(text: &str) -> SlackMeta {
    let mut meta = SlackMeta::default();
    let mut in_slack = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_slack = line == "[slack]";
            continue;
        }
        if !in_slack {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = unquote(value.trim());
        let Some(value) = value else { continue };
        match key {
            "client_id" => meta.client_id = Some(value),
            "team_id" => meta.team_id = Some(value),
            "team_name" => meta.team_name = Some(value),
            "user_id" => meta.user_id = Some(value),
            _ => {}
        }
    }
    meta
}

/// Strips surrounding double quotes from a value, unescaping `\"` and `\\`.
/// Returns `None` if the value is not a quoted string.
fn unquote(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Escapes a value for the `"..."` TOML form (backslash and quote only).
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Serializes [`SlackMeta`] to the `[slack]` table text we persist.
pub fn render(meta: &SlackMeta) -> String {
    let mut out = String::from("[slack]\n");
    let fields = [
        ("client_id", &meta.client_id),
        ("team_id", &meta.team_id),
        ("team_name", &meta.team_name),
        ("user_id", &meta.user_id),
    ];
    for (key, value) in fields {
        if let Some(v) = value {
            out.push_str(&format!("{key} = \"{}\"\n", escape(v)));
        }
    }
    out
}

/// Writes `apps.toml` with the directory at `0700` and the file at `0600`.
pub fn save(home: &Path, meta: &SlackMeta) -> Result<(), String> {
    let dir = config_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    set_mode(&dir, 0o700)?;
    let path = config_path(home);
    let mut file = create_private(&path)?;
    file.write_all(render(meta).as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Deletes `apps.toml` (logout). A missing file is not an error.
pub fn clear(home: &Path) -> Result<(), String> {
    let path = config_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::create(path).map_err(|e| format!("open {}: {e}", path.display()))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_id_and_metadata_from_slack_table() {
        let text = "\
# comment
[slack]
client_id = \"123.456\"
team_id = \"T0001\"
team_name = \"Acme Inc\"
user_id = \"U0001\"
";
        let meta = parse(text);
        assert_eq!(meta.client_id.as_deref(), Some("123.456"));
        assert_eq!(meta.team_id.as_deref(), Some("T0001"));
        assert_eq!(meta.team_name.as_deref(), Some("Acme Inc"));
        assert_eq!(meta.user_id.as_deref(), Some("U0001"));
    }

    #[test]
    fn ignores_keys_outside_the_slack_table() {
        let meta = parse("[other]\nclient_id = \"nope\"\n");
        assert_eq!(meta.client_id, None);
    }

    #[test]
    fn render_then_parse_round_trips() {
        let meta = SlackMeta {
            client_id: Some("c\"id".into()),
            team_id: Some("T1".into()),
            team_name: Some("Back\\slash".into()),
            user_id: None,
        };
        assert_eq!(parse(&render(&meta)), meta);
    }

    #[test]
    fn save_creates_private_file_and_dir() {
        let tmp = std::env::temp_dir().join(format!("gentle-apps-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let meta = SlackMeta { client_id: Some("abc".into()), ..Default::default() };
        save(&tmp, &meta).unwrap();
        assert_eq!(load(&tmp), meta);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fmode = std::fs::metadata(config_path(&tmp)).unwrap().permissions().mode() & 0o777;
            let dmode = std::fs::metadata(config_dir(&tmp)).unwrap().permissions().mode() & 0o777;
            assert_eq!(fmode, 0o600, "file mode");
            assert_eq!(dmode, 0o700, "dir mode");
        }
        clear(&tmp).unwrap();
        assert!(load(&tmp).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
