//! Editable buffer for the left viewer pane (S5, T6). Loads the RAW file,
//! preserving tabs, the CRLF/LF line-ending style and whether the file ends
//! with a newline, so a save with no edits is byte-identical. Tabs are
//! expanded to four columns only when rendering and the cursor column is
//! mapped through that expansion. Binary, unreadable and oversized files are
//! kept read-only.
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const TAB_WIDTH: usize = 4;

/// Outcome of a save attempt, so the app can report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written atomically; the dirty flag was cleared.
    Saved,
    /// The file changed on disk since it was loaded; nothing was written.
    DiskChanged,
    /// The buffer is read-only (binary, oversized or unreadable).
    ReadOnly,
    /// An I/O error; the message is for the status line.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    /// Path relative to the project root (shown in the title).
    pub path: PathBuf,
    /// Absolute path used for disk I/O.
    abs: PathBuf,
    /// Lines without their terminators; tabs are preserved verbatim.
    pub lines: Vec<String>,
    /// Line-ending style restored on save.
    crlf: bool,
    /// Whether the original file ended with a newline.
    trailing_newline: bool,
    pub cursor_line: usize,
    /// Cursor column in characters within the line (not expanded columns).
    pub cursor_col: usize,
    pub scroll_row: usize,
    pub scroll_col: usize,
    pub dirty: bool,
    /// Rows per page, refreshed on draw for PageUp/PageDown.
    pub page: usize,
    /// Content hash at load, used to detect concurrent disk edits.
    load_hash: u64,
    /// Original unix mode, restored after an atomic save.
    mode: Option<u32>,
    /// `Some(reason)` when the buffer cannot be edited.
    readonly: Option<String>,
}

/// Expands tabs to [`TAB_WIDTH`] columns for display.
pub fn expand_tabs(s: &str) -> String {
    let mut out = String::new();
    let mut vis = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (vis % TAB_WIDTH);
            out.push_str(&" ".repeat(n));
            vis += n;
        } else {
            out.push(ch);
            vis += 1;
        }
    }
    out
}

/// Visual column (tab-expanded) of character column `col` within `line`.
fn expanded_col(line: &str, col: usize) -> usize {
    let mut vis = 0usize;
    for ch in line.chars().take(col) {
        if ch == '\t' {
            vis += TAB_WIDTH - (vis % TAB_WIDTH);
        } else {
            vis += 1;
        }
    }
    vis
}

/// Character column nearest a target visual (tab-expanded) column.
fn col_from_visual(line: &str, target: usize) -> usize {
    let mut vis = 0usize;
    let mut idx = 0usize;
    for ch in line.chars() {
        if vis >= target {
            break;
        }
        vis += if ch == '\t' { TAB_WIDTH - (vis % TAB_WIDTH) } else { 1 };
        idx += 1;
    }
    idx
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn byte_idx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

fn content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

#[cfg(unix)]
fn file_mode(p: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).ok().map(|m| m.permissions().mode())
}
#[cfg(not(unix))]
fn file_mode(_p: &Path) -> Option<u32> {
    None
}

fn keep_visible(selected: usize, offset: usize, height: usize) -> usize {
    if height == 0 {
        offset
    } else if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

impl Editor {
    pub fn open(root: &Path, rel: &Path) -> Self {
        Self::open_abs(rel.to_path_buf(), root.join(rel))
    }

    fn read_only(path: PathBuf, abs: PathBuf, reason: &str) -> Self {
        Self {
            lines: crate::viewer::read_text(&abs),
            path,
            abs,
            crlf: false,
            trailing_newline: false,
            cursor_line: 0,
            cursor_col: 0,
            scroll_row: 0,
            scroll_col: 0,
            dirty: false,
            page: 10,
            load_hash: 0,
            mode: None,
            readonly: Some(reason.to_string()),
        }
    }

    fn open_abs(path: PathBuf, abs: PathBuf) -> Self {
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => return Self::read_only(path, abs, "cannot read file — read only"),
        };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            return Self::read_only(path, abs, "binary file — read only");
        }
        if bytes.len() > crate::viewer::MAX_BYTES {
            return Self::read_only(path, abs, "file too large to edit — read only");
        }
        // Lossy decoding would silently rewrite bytes on save, so non-UTF-8 text stays read-only.
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Self::read_only(path, abs, "not UTF-8 text — read only");
        };
        let load_hash = content_hash(&bytes);
        let mode = file_mode(&abs);
        let text = std::borrow::Cow::Borrowed(text);
        let crlf = text.contains("\r\n");
        let trailing_newline = text.ends_with('\n');
        let body = if crlf { text.replace("\r\n", "\n") } else { text.into_owned() };
        let mut lines: Vec<String> = body.split('\n').map(String::from).collect();
        if trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            path,
            abs,
            lines,
            crlf,
            trailing_newline,
            cursor_line: 0,
            cursor_col: 0,
            scroll_row: 0,
            scroll_col: 0,
            dirty: false,
            page: 10,
            load_hash,
            mode,
            readonly: None,
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.readonly.is_some()
    }

    pub fn read_only_reason(&self) -> Option<&str> {
        self.readonly.as_deref()
    }

    /// Reconstructs the file bytes, restoring the line endings and the trailing
    /// newline exactly as loaded.
    pub fn to_bytes(&self) -> Vec<u8> {
        let sep = if self.crlf { "\r\n" } else { "\n" };
        let mut s = self.lines.join(sep);
        if self.trailing_newline {
            s.push_str(sep);
        }
        s.into_bytes()
    }

    /// Visual (tab-expanded) column of the cursor.
    pub fn cursor_visual_col(&self) -> usize {
        expanded_col(&self.lines[self.cursor_line], self.cursor_col)
    }

    pub fn gutter_width(&self) -> usize {
        self.lines.len().to_string().len() + 1
    }

    /// Applies an editing key. Returns true when the buffer content changed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.readonly.is_some() || key.kind == KeyEventKind::Release {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                true
            }
            KeyCode::Tab => {
                self.insert_char('\t');
                true
            }
            KeyCode::Enter => {
                self.split_line();
                true
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => {
                self.move_left();
                false
            }
            KeyCode::Right => {
                self.move_right();
                false
            }
            KeyCode::Up => {
                self.move_vert(-1);
                false
            }
            KeyCode::Down => {
                self.move_vert(1);
                false
            }
            KeyCode::Home if ctrl => {
                self.cursor_line = 0;
                self.cursor_col = 0;
                false
            }
            KeyCode::End if ctrl => {
                self.cursor_line = self.lines.len() - 1;
                self.cursor_col = char_len(&self.lines[self.cursor_line]);
                false
            }
            KeyCode::Home => {
                self.cursor_col = 0;
                false
            }
            KeyCode::End => {
                self.cursor_col = char_len(&self.lines[self.cursor_line]);
                false
            }
            KeyCode::PageUp => {
                self.move_vert(-(self.page.max(1) as isize));
                false
            }
            KeyCode::PageDown => {
                self.move_vert(self.page.max(1) as isize);
                false
            }
            _ => false,
        }
    }

    /// Inserts a literal string at the cursor (used to accept a ghost
    /// suggestion). Newlines split the line; read-only buffers are untouched.
    pub fn insert_str(&mut self, s: &str) {
        if self.readonly.is_some() {
            return;
        }
        for c in s.chars() {
            if c == '\n' {
                self.split_line();
            } else {
                self.insert_char(c);
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_line];
        let bi = byte_idx(line, self.cursor_col);
        line.insert(bi, c);
        self.cursor_col += 1;
        self.dirty = true;
    }

    fn split_line(&mut self) {
        let line = &mut self.lines[self.cursor_line];
        let bi = byte_idx(line, self.cursor_col);
        let rest = line.split_off(bi);
        self.lines.insert(self.cursor_line + 1, rest);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) -> bool {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let start = byte_idx(line, self.cursor_col - 1);
            let end = byte_idx(line, self.cursor_col);
            line.replace_range(start..end, "");
            self.cursor_col -= 1;
            self.dirty = true;
            true
        } else if self.cursor_line > 0 {
            let cur = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = char_len(&self.lines[self.cursor_line]);
            self.lines[self.cursor_line].push_str(&cur);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    fn delete(&mut self) -> bool {
        let len = char_len(&self.lines[self.cursor_line]);
        if self.cursor_col < len {
            let line = &mut self.lines[self.cursor_line];
            let start = byte_idx(line, self.cursor_col);
            let end = byte_idx(line, self.cursor_col + 1);
            line.replace_range(start..end, "");
            self.dirty = true;
            true
        } else if self.cursor_line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = char_len(&self.lines[self.cursor_line]);
        }
    }

    fn move_right(&mut self) {
        let len = char_len(&self.lines[self.cursor_line]);
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn move_vert(&mut self, delta: isize) {
        let target = (self.cursor_line as isize + delta).clamp(0, self.lines.len() as isize - 1) as usize;
        self.cursor_line = target;
        self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_line]));
    }

    /// Moves the cursor by `delta` lines (mouse wheel over the editor).
    pub fn scroll_lines(&mut self, delta: isize) {
        self.move_vert(delta);
    }

    /// Places the cursor from a click inside the editor's inner rect.
    pub fn click(&mut self, inner_x: u16, inner_y: u16, gutter: usize, col: u16, row: u16) {
        if self.readonly.is_some() {
            return;
        }
        let line = (self.scroll_row + row.saturating_sub(inner_y) as usize).min(self.lines.len() - 1);
        self.cursor_line = line;
        let vis = self.scroll_col + (col.saturating_sub(inner_x) as usize).saturating_sub(gutter);
        self.cursor_col = col_from_visual(&self.lines[line], vis).min(char_len(&self.lines[line]));
    }

    /// Keeps the cursor visible within a text area of `height` rows and `width`
    /// columns (after the line-number gutter).
    pub fn ensure_visible(&mut self, height: usize, width: usize) {
        self.scroll_row = keep_visible(self.cursor_line, self.scroll_row, height);
        let vcol = self.cursor_visual_col();
        if vcol < self.scroll_col {
            self.scroll_col = vcol;
        } else if width > 0 && vcol >= self.scroll_col + width {
            self.scroll_col = vcol + 1 - width;
        }
    }

    /// Reloads from disk, discarding edits.
    pub fn reload(&mut self) {
        *self = Self::open_abs(self.path.clone(), self.abs.clone());
    }

    /// Saves atomically if the disk copy still matches the loaded hash.
    pub fn save(&mut self) -> SaveOutcome {
        if self.readonly.is_some() {
            return SaveOutcome::ReadOnly;
        }
        let disk = match std::fs::read(&self.abs) {
            Ok(b) => b,
            Err(e) => return SaveOutcome::Error(e.to_string()),
        };
        if content_hash(&disk) != self.load_hash {
            return SaveOutcome::DiskChanged;
        }
        let bytes = self.to_bytes();
        // Write through symlinks: renaming over the link itself would detach it from its target.
        let target = std::fs::canonicalize(&self.abs).unwrap_or_else(|_| self.abs.clone());
        if let Err(e) = atomic_write(&target, &bytes, self.mode) {
            return SaveOutcome::Error(e);
        }
        self.load_hash = content_hash(&bytes);
        self.dirty = false;
        SaveOutcome::Saved
    }
}

/// Writes `bytes` to a temp file in the same directory, restores the mode, then
/// renames it over `target`.
fn atomic_write(target: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(".{name}.gentle-tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m));
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gentle-partitioner-editor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn roundtrip(name: &str, raw: &[u8]) {
        let d = tmp(name);
        fs::write(d.join("f.txt"), raw).unwrap();
        let mut e = Editor::open(&d, Path::new("f.txt"));
        assert_eq!(e.save(), SaveOutcome::Saved);
        assert_eq!(fs::read(d.join("f.txt")).unwrap(), raw, "byte-identical save for {name}");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn unedited_save_is_byte_identical() {
        roundtrip("lf", b"one\ntwo\n");
        roundtrip("crlf", b"one\r\ntwo\r\n");
        roundtrip("no-trailing", b"one\ntwo");
        roundtrip("tabs", b"fn x() {\n\tret\n}\n");
        roundtrip("empty", b"");
    }

    #[test]
    fn insert_split_join_and_utf8_cursor() {
        let d = tmp("edit");
        fs::write(d.join("f.txt"), "ab\ncd\n").unwrap();
        let mut e = Editor::open(&d, Path::new("f.txt"));
        // insert at start of line
        assert!(e.handle_key(ch('X')));
        assert_eq!(e.lines[0], "Xab");
        assert_eq!(e.cursor_col, 1);
        // Enter splits the line
        e.handle_key(key(KeyCode::Right)); // after 'a'
        assert!(e.handle_key(key(KeyCode::Enter)));
        assert_eq!(e.lines[0], "Xa");
        assert_eq!(e.lines[1], "b");
        assert_eq!((e.cursor_line, e.cursor_col), (1, 0));
        // Backspace at column 0 joins with the previous line
        assert!(e.handle_key(key(KeyCode::Backspace)));
        assert_eq!(e.lines[0], "Xab");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 2));
        // Delete at end of line joins the next line
        e.handle_key(key(KeyCode::End));
        assert!(e.handle_key(key(KeyCode::Delete)));
        assert_eq!(e.lines[0], "Xabcd");
        assert!(e.dirty);

        // UTF-8 cursor movement counts characters, not bytes
        fs::write(d.join("u.txt"), "ñ界x\n").unwrap();
        let mut u = Editor::open(&d, Path::new("u.txt"));
        u.handle_key(key(KeyCode::Right));
        u.handle_key(key(KeyCode::Right));
        assert_eq!(u.cursor_col, 2);
        u.handle_key(ch('Z'));
        assert_eq!(u.lines[0], "ñ界Zx");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn tab_expands_cursor_column() {
        let d = tmp("tabcol");
        fs::write(d.join("t.txt"), "\tx\n").unwrap();
        let mut e = Editor::open(&d, Path::new("t.txt"));
        assert_eq!(e.cursor_visual_col(), 0);
        e.handle_key(key(KeyCode::Right)); // past the tab
        assert_eq!(e.cursor_col, 1);
        assert_eq!(e.cursor_visual_col(), 4);
        assert_eq!(expand_tabs("\tx"), "    x");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn save_refused_when_disk_changed_then_reload() {
        let d = tmp("conflict");
        fs::write(d.join("f.txt"), "one\n").unwrap();
        let mut e = Editor::open(&d, Path::new("f.txt"));
        e.handle_key(ch('Z'));
        assert!(e.dirty);
        // The agent edits the file underneath us.
        fs::write(d.join("f.txt"), "agent\n").unwrap();
        assert_eq!(e.save(), SaveOutcome::DiskChanged);
        assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"agent\n", "disk left untouched");
        assert!(e.dirty, "buffer stays dirty after a refused save");
        // Ctrl+R reloads from disk.
        e.reload();
        assert_eq!(e.lines, ["agent"]);
        assert!(!e.dirty);
        assert_eq!(e.save(), SaveOutcome::Saved);
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perms");
        let f = d.join("s.sh");
        fs::write(&f, "echo hi\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        let mut e = Editor::open(&d, Path::new("s.sh"));
        e.handle_key(ch('#'));
        assert_eq!(e.save(), SaveOutcome::Saved);
        let mode = fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(fs::read(&f).unwrap(), b"#echo hi\n");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn binary_and_oversized_files_are_read_only() {
        let d = tmp("ro");
        fs::write(d.join("img.png"), [0x89, b'P', 0, 1, 2]).unwrap();
        let mut bin = Editor::open(&d, Path::new("img.png"));
        assert!(bin.is_read_only());
        let before = bin.lines.clone();
        assert!(!bin.handle_key(ch('x')));
        assert!(!bin.handle_key(key(KeyCode::Enter)));
        assert_eq!(bin.lines, before);
        assert_eq!(bin.save(), SaveOutcome::ReadOnly);

        fs::write(d.join("big.txt"), "x\n".repeat(crate::viewer::MAX_BYTES)).unwrap();
        let mut big = Editor::open(&d, Path::new("big.txt"));
        assert!(big.is_read_only());
        let before = big.lines.clone();
        assert!(!big.handle_key(ch('y')));
        assert_eq!(big.lines, before);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn non_utf8_text_is_read_only_so_saving_cannot_corrupt_it() {
        let d = tmp("latin1");
        let original = b"caf\xe9\n".to_vec();
        fs::write(d.join("l1.txt"), &original).unwrap();
        let mut ed = Editor::open(&d, Path::new("l1.txt"));
        assert!(ed.is_read_only());
        assert_eq!(ed.read_only_reason(), Some("not UTF-8 text — read only"));
        assert!(!ed.handle_key(ch('x')));
        assert_eq!(ed.save(), SaveOutcome::ReadOnly);
        assert_eq!(fs::read(d.join("l1.txt")).unwrap(), original);
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_symlink_writes_the_target_and_keeps_the_link() {
        let d = tmp("symlink");
        fs::create_dir_all(d.join("real")).unwrap();
        fs::write(d.join("real/target.txt"), "original\n").unwrap();
        std::os::unix::fs::symlink("real/target.txt", d.join("link.txt")).unwrap();
        let mut ed = Editor::open(&d, Path::new("link.txt"));
        assert!(ed.handle_key(ch('Z')));
        assert_eq!(ed.save(), SaveOutcome::Saved);
        assert!(fs::symlink_metadata(d.join("link.txt")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(d.join("real/target.txt")).unwrap(), "Zoriginal\n");
        let leftovers: Vec<_> = fs::read_dir(d.join("real")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, [std::ffi::OsString::from("target.txt")]);
        fs::remove_dir_all(d).unwrap();
    }
}
