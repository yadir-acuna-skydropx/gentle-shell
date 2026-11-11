//! Application state and rendering. Pure state transitions live in
//! `handle_key`/`handle_mouse` so they are testable without a terminal;
//! the event loop in `main.rs` only wires threads, PTY and terminal.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use gentle_apps::protocol::{ClientMsg, EngineMsg, Status as AppState};

use crate::apps_panel::{AppsPanel, View};
use crate::editor::{Editor, SaveOutcome};
use crate::highlight::{self, Highlighter};
use crate::icons::{self, IconMode};
use crate::layout::{self, Layout, Mode};
use crate::pane::{self, PtyPane};
use crate::predict::{Engine, PredictMode};
use crate::tree::{self, Entry, GitStatus};
use crate::viewer::{self, DiffKind, Viewer};

/// Prefix chord for partitioner commands; everything else goes to the focused pane.
pub const PREFIX_LABEL: &str = "Ctrl+]";

/// Lateral tool bar entries in chat mode (S7): label and prefix key.
pub const TOOLS: [(&str, char); 6] =
    [("Files", 't'), ("Changes", 'c'), ("Workspace", 'w'), ("Apps", 'a'), ("Help", '?'), ("Quit", 'q')];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Apps,
    Editor,
    Diff,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Files,
    Changes,
    Help,
    Apps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Bytes for the chat PTY.
    Forward(Vec<u8>),
    /// Scroll the chat scrollback (positive = older output).
    ScrollChat(isize),
    /// A request for the apps engine (reply / refresh).
    Apps(ClientMsg),
    Quit,
}

pub struct App {
    pub root: PathBuf,
    /// Mode the user asked for; the layout may degrade it on small terminals.
    pub mode: Mode,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub expanded: HashSet<PathBuf>,
    pub statuses: HashMap<PathBuf, GitStatus>,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub change_selected: usize,
    pub viewer: Option<Viewer>,
    /// Editable buffer for the left pane; `None` until a file is opened.
    pub editor: Option<Editor>,
    /// Transient message shown in the status line until the next key.
    pub status_msg: Option<String>,
    /// Target of a blocked open while the editor is dirty; a second open of the
    /// same target discards the edits.
    pub pending_discard: Option<PathBuf>,
    /// Set after a dirty-guarded quit warning; a second prefix-q quits.
    pub pending_quit: bool,
    /// Set when a save needs the main loop to refresh git status.
    pub needs_git_refresh: bool,
    pub prefix: bool,
    /// Child DECCKM state, updated by the loop before each key.
    pub app_cursor: bool,
    /// First tree row shown in the sidebar / files overlay.
    pub tree_offset: usize,
    pub layout: Option<Layout>,
    pub overlay_area: Option<Rect>,
    /// Shared n-gram engine (S9); `None` disables ghost suggestions and learning.
    pub predict: Option<Arc<RwLock<Engine>>>,
    /// Dim ghost suffix shown after the editor cursor, when any.
    pub ghost: Option<String>,
    /// Icon set for the sidebar tree and Files overlay (T10).
    pub icons: IconMode,
    /// Cached syntax highlighter for the open editor buffer (T11); `None`
    /// until a file is opened.
    pub highlighter: Option<Highlighter>,
    /// Bottom-left apps panel state (S12).
    pub apps: AppsPanel,
    /// Incrementing id for apps-engine requests.
    apps_req_id: u64,
    /// File-tree region of the sidebar, captured in the last draw.
    pub tree_area: Option<Rect>,
    /// Apps-panel region of the sidebar, captured in the last draw.
    pub apps_area: Option<Rect>,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let mut app = Self {
            root,
            mode: Mode::Chat,
            focus: Focus::Chat,
            overlay: None,
            expanded: HashSet::new(),
            statuses: HashMap::new(),
            entries: Vec::new(),
            selected: 0,
            change_selected: 0,
            viewer: None,
            editor: None,
            status_msg: None,
            pending_discard: None,
            pending_quit: false,
            needs_git_refresh: false,
            prefix: false,
            app_cursor: false,
            tree_offset: 0,
            layout: None,
            overlay_area: None,
            predict: None,
            ghost: None,
            icons: IconMode::from_env(),
            highlighter: None,
            apps: AppsPanel::default(),
            apps_req_id: 0,
            tree_area: None,
            apps_area: None,
        };
        app.rescan();
        app
    }

    /// Wires the shared prediction engine (S9) for in-process ghost text.
    pub fn set_predict(&mut self, engine: Arc<RwLock<Engine>>) {
        self.predict = Some(engine);
    }

    /// Recomputes the editor ghost: only when the editor is focused, editable,
    /// no overlay is open and the cursor sits at the END of a non-blank line.
    fn refresh_ghost(&mut self) {
        self.ghost = None;
        let Some(engine) = self.predict.clone() else { return };
        if self.effective_focus() != Focus::Editor || self.overlay.is_some() {
            return;
        }
        let before = match &self.editor {
            Some(e) if !e.is_read_only() => {
                let line = &e.lines[e.cursor_line];
                if e.cursor_col != line.chars().count() {
                    return; // cursor is not at the end of the line
                }
                line.clone()
            }
            _ => return,
        };
        if before.trim().is_empty() {
            return;
        }
        if let Ok(eng) = engine.read() {
            self.ghost = eng.predict(&before, PredictMode::Code);
        }
    }

    /// Rebuilds the tree from disk with the current git statuses.
    pub fn rescan(&mut self) {
        self.entries = tree::scan(&self.root, &self.expanded, &self.statuses);
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    /// Applies fresh git statuses (computed off the UI thread) and reloads the viewer diff.
    pub fn apply_statuses(&mut self, statuses: HashMap<PathBuf, GitStatus>) {
        self.statuses = statuses;
        self.rescan();
        if let Some(v) = &mut self.viewer {
            let (cs, ds) = (v.content_scroll, v.diff_scroll);
            *v = Viewer::open(&self.root, &v.path.clone());
            v.content_scroll = cs.min(v.content.len().saturating_sub(1));
            v.diff_scroll = ds.min(v.diff.len().saturating_sub(1));
        }
    }

    pub fn changes(&self) -> Vec<(PathBuf, GitStatus)> {
        let mut c: Vec<_> = self.statuses.iter().map(|(p, s)| (p.clone(), *s)).collect();
        c.sort_by(|a, b| a.0.cmp(&b.0));
        c
    }

    /// Layout actually rendered for `area`.
    pub fn compute_layout(&self, area: Rect) -> Layout {
        layout::compute(area, self.mode)
    }

    /// Area the chat PTY occupies (inside its border in workspace mode).
    pub fn chat_inner(&self, area: Rect) -> Rect {
        let l = self.compute_layout(area);
        match l.mode {
            Mode::Workspace => Block::default().borders(Borders::ALL).inner(l.chat),
            Mode::Chat => l.chat,
        }
    }

    /// Focus actually in effect: panes hidden by the layout fall back to the chat.
    pub fn effective_focus(&self) -> Focus {
        match self.layout.map(|l| l.mode) {
            Some(Mode::Chat) => Focus::Chat,
            _ if self.mode == Mode::Chat => Focus::Chat,
            _ => self.focus,
        }
    }

    pub fn open_file(&mut self, rel: &Path) {
        // A dirty buffer is not discarded on the first open of a new target:
        // warn and only switch when the same target is opened again.
        if self.editor.as_ref().is_some_and(|e| e.dirty) && self.pending_discard.as_deref() != Some(rel) {
            self.pending_discard = Some(rel.to_path_buf());
            self.status_msg = Some("unsaved changes — Ctrl+S save, open again to discard".into());
            return;
        }
        self.pending_discard = None;
        self.pending_quit = false;
        self.editor = Some(Editor::open(&self.root, rel));
        self.highlighter = Some(Highlighter::new(lang_for_path(rel)));
        self.viewer = Some(Viewer::open(&self.root, rel));
        self.mode = Mode::Workspace;
        self.focus = Focus::Editor;
        self.overlay = None;
        self.refresh_ghost();
    }

    /// Drops cached highlight lines from `line` down after an edit.
    fn invalidate_hl(&mut self, line: usize) {
        if let Some(h) = &mut self.highlighter {
            h.invalidate_from(line);
        }
    }

    /// Whether the main loop should ask the git worker to refresh (after a save).
    pub fn take_git_refresh(&mut self) -> bool {
        std::mem::take(&mut self.needs_git_refresh)
    }

    /// Saves the editor (Ctrl+S): atomic write, clear dirty, refresh the diff
    /// pane and request a git status refresh.
    fn save_editor(&mut self) {
        let outcome = match &mut self.editor {
            Some(e) => e.save(),
            None => return,
        };
        match outcome {
            SaveOutcome::Saved => {
                self.status_msg = Some("saved".into());
                self.pending_quit = false;
                self.pending_discard = None;
                self.needs_git_refresh = true;
                // Learn the saved buffer into the code model (S9).
                if let (Some(engine), Some(e)) = (self.predict.clone(), &self.editor) {
                    let text = e.lines.join("\n");
                    if let Ok(mut eng) = engine.write() {
                        eng.learn(&text, PredictMode::Code);
                    }
                }
                if let Some(v) = &mut self.viewer {
                    let p = v.path.clone();
                    v.diff = viewer::git_diff(&self.root, &p);
                    v.diff_scroll = v.diff_scroll.min(v.diff.len().saturating_sub(1));
                }
            }
            SaveOutcome::DiskChanged => {
                self.status_msg = Some("file changed on disk — Ctrl+R reloads (discards your edits)".into());
            }
            SaveOutcome::ReadOnly => {
                self.status_msg = self.editor.as_ref().and_then(|e| e.read_only_reason()).map(str::to_string);
            }
            SaveOutcome::Error(msg) => self.status_msg = Some(format!("save failed: {msg}")),
        }
    }

    /// Reloads the editor from disk (Ctrl+R), discarding edits.
    fn reload_editor(&mut self) {
        if let Some(e) = &mut self.editor {
            e.reload();
        }
        if let Some(v) = &mut self.viewer {
            let p = v.path.clone();
            *v = Viewer::open(&self.root, &p);
        }
        // The whole buffer was replaced: drop the entire highlight cache.
        self.invalidate_hl(0);
        self.pending_discard = None;
        self.status_msg = Some("reloaded from disk".into());
    }

    /// Translates a key event into either a partitioner command or bytes for
    /// the chat PTY. Pure state transition so it can be unit tested.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Only react to presses; Release (and auto Repeat on some terminals)
        // never reach the child or move the UI.
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        // The transient status message lives only until the next key.
        self.status_msg = None;

        // A pending prefix consumes exactly one key, then clears.
        if self.prefix {
            self.prefix = false;
            return self.prefix_command(key);
        }
        // Ctrl+] opens the prefix chord (handled before overlays so it still works there).
        if key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.prefix = true;
            return Action::None;
        }

        if let Some(overlay) = self.overlay {
            return self.overlay_key(overlay, key);
        }

        match self.effective_focus() {
            Focus::Chat => Action::Forward(crate::keys::encode(key, self.app_cursor)),
            Focus::Sidebar => self.sidebar_key(key),
            Focus::Apps => self.apps_key(key),
            Focus::Editor => self.editor_key(key),
            Focus::Diff => self.diff_key(key),
        }
    }

    /// Applies a decoded engine message to the panel (called by the event loop).
    pub fn apply_apps_msg(&mut self, msg: EngineMsg) {
        self.apps.apply(msg);
    }

    /// Marks the apps engine offline (missing or exited).
    pub fn set_apps_offline(&mut self) {
        self.apps.set_offline();
    }

    fn next_apps_id(&mut self) -> u64 {
        self.apps_req_id += 1;
        self.apps_req_id
    }

    /// Prefix `a`: in workspace, toggle the panel expanded/collapsed and focus
    /// it; in chat mode, open the panel as an overlay.
    fn toggle_apps_panel(&mut self) {
        if self.mode == Mode::Workspace {
            self.overlay = None;
            if self.apps.view == View::Apps {
                self.apps.view = View::Items;
                self.apps.item_sel = 0;
            } else {
                self.apps.view = View::Apps;
            }
            self.focus = Focus::Apps;
        } else {
            self.apps.view = View::Apps;
            self.toggle_overlay(Overlay::Apps);
        }
    }

    /// Leaves the apps panel: closes its overlay, or returns focus to the chat.
    fn leave_apps(&mut self) {
        if self.overlay == Some(Overlay::Apps) {
            self.overlay = None;
        }
        self.apps.view = View::Apps;
        self.focus = Focus::Chat;
    }

    /// Panel navigation: Up/Down select; Enter drills in (app → items → reply);
    /// Esc steps back one level, then to the chat.
    fn apps_key(&mut self, key: KeyEvent) -> Action {
        match self.apps.view {
            View::Reply => self.apps_reply_key(key),
            View::Items => {
                let len = self.apps.current_items().len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.apps.item_sel = clamp_index(self.apps.item_sel, -1, len),
                    KeyCode::Down | KeyCode::Char('j') => self.apps.item_sel = clamp_index(self.apps.item_sel, 1, len),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if self.apps.selected_item_id().is_some() {
                            self.apps.view = View::Reply;
                            self.apps.reply.clear();
                            self.apps.reply_status = None;
                        }
                    }
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.apps.view = View::Apps,
                    _ => {}
                }
                Action::None
            }
            View::Apps => {
                let len = self.apps.apps.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.apps.app_sel = clamp_index(self.apps.app_sel, -1, len),
                    KeyCode::Down | KeyCode::Char('j') => self.apps.app_sel = clamp_index(self.apps.app_sel, 1, len),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(id) = self.apps.selected_app_id() {
                            self.apps.view = View::Items;
                            self.apps.item_sel = 0;
                            let rid = self.next_apps_id();
                            return Action::Apps(ClientMsg::Refresh { id: rid, app: id });
                        }
                    }
                    KeyCode::Esc => self.leave_apps(),
                    _ => {}
                }
                Action::None
            }
        }
    }

    /// Reply editor: type to edit a single line; Enter sends, Esc cancels back
    /// to the item list.
    fn apps_reply_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.apps.view = View::Items;
                self.apps.reply.clear();
                self.apps.reply_status = None;
            }
            KeyCode::Enter => {
                let text = self.apps.reply.text.clone();
                if text.trim().is_empty() {
                    return Action::None;
                }
                let (Some(app), Some(item)) = (self.apps.selected_app_id(), self.apps.selected_item_id()) else {
                    return Action::None;
                };
                let rid = self.next_apps_id();
                self.apps.pending_reply = Some(rid);
                self.apps.reply_status = Some("sending\u{2026}".into());
                return Action::Apps(ClientMsg::Reply { id: rid, app, item, text });
            }
            KeyCode::Backspace => self.apps.reply.backspace(),
            KeyCode::Left => self.apps.reply.left(),
            KeyCode::Right => self.apps.reply.right(),
            KeyCode::Char(c) => self.apps.reply.insert(c),
            _ => {}
        }
        Action::None
    }

    /// Runs a prefix command (the key after `Ctrl+]`).
    fn prefix_command(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('w') => {
                self.toggle_mode();
                Action::None
            }
            KeyCode::Char('t') => {
                self.toggle_overlay(Overlay::Files);
                Action::None
            }
            KeyCode::Char('c') => {
                self.change_selected = 0;
                self.toggle_overlay(Overlay::Changes);
                Action::None
            }
            KeyCode::Char('a') => {
                self.toggle_apps_panel();
                Action::None
            }
            KeyCode::Char('?') => {
                self.toggle_overlay(Overlay::Help);
                Action::None
            }
            KeyCode::Char('q') => {
                // A dirty buffer warns once before quitting; a second q quits.
                if self.editor.as_ref().is_some_and(|e| e.dirty) && !self.pending_quit {
                    self.pending_quit = true;
                    self.status_msg = Some("unsaved changes — Ctrl+S save, press q again to quit".into());
                    Action::None
                } else {
                    Action::Quit
                }
            }
            KeyCode::Char(']') => Action::Forward(vec![0x1d]),
            KeyCode::Tab => {
                self.cycle_focus();
                Action::None
            }
            KeyCode::Char(c @ '1'..='4') if self.mode == Mode::Workspace => {
                self.focus = match c {
                    '1' => Focus::Sidebar,
                    '2' => Focus::Editor,
                    '3' => Focus::Diff,
                    _ => Focus::Chat,
                };
                Action::None
            }
            _ => Action::None,
        }
    }

    fn toggle_mode(&mut self) {
        match self.mode {
            Mode::Chat => {
                self.mode = Mode::Workspace;
                self.focus = Focus::Sidebar;
            }
            Mode::Workspace => {
                self.mode = Mode::Chat;
                self.focus = Focus::Chat;
            }
        }
    }

    fn toggle_overlay(&mut self, overlay: Overlay) {
        self.overlay = if self.overlay == Some(overlay) { None } else { Some(overlay) };
    }

    /// Visible panes in tab order; hidden panes (small terminals, chat mode) are skipped.
    fn visible_focuses(&self) -> Vec<Focus> {
        let mut v = Vec::new();
        if let Some(l) = self.layout {
            if l.sidebar.is_some() {
                v.push(Focus::Sidebar);
            }
            if l.editor.is_some() {
                v.push(Focus::Editor);
            }
            if l.diff.is_some() {
                v.push(Focus::Diff);
            }
        }
        v.push(Focus::Chat);
        v
    }

    fn cycle_focus(&mut self) {
        let order = self.visible_focuses();
        let idx = order.iter().position(|&f| f == self.focus).unwrap_or(0);
        self.focus = order[(idx + 1) % order.len()];
    }

    fn overlay_key(&mut self, overlay: Overlay, key: KeyEvent) -> Action {
        // The help overlay is purely informational: any key dismisses it.
        if overlay == Overlay::Help {
            self.overlay = None;
            return Action::None;
        }
        // The apps overlay shares the panel reducer so chat mode behaves exactly
        // like the workspace panel.
        if overlay == Overlay::Apps {
            return self.apps_key(key);
        }
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => self.overlay_move(overlay, -1),
            KeyCode::Down | KeyCode::Char('j') => self.overlay_move(overlay, 1),
            KeyCode::Enter => match overlay {
                Overlay::Files => self.toggle_or_open_selected(),
                Overlay::Changes => self.open_selected_change(),
                Overlay::Help | Overlay::Apps => {}
            },
            _ => {}
        }
        Action::None
    }

    fn overlay_move(&mut self, overlay: Overlay, delta: isize) {
        match overlay {
            Overlay::Files => self.selected = clamp_index(self.selected, delta, self.entries.len()),
            Overlay::Changes => self.change_selected = clamp_index(self.change_selected, delta, self.changes().len()),
            Overlay::Help | Overlay::Apps => {}
        }
    }

    fn sidebar_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = clamp_index(self.selected, -1, self.entries.len()),
            KeyCode::Down | KeyCode::Char('j') => self.selected = clamp_index(self.selected, 1, self.entries.len()),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.expand_or_open_selected(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_or_parent(),
            KeyCode::Esc => self.focus = Focus::Chat,
            _ => {}
        }
        Action::None
    }

    /// Editor focus: editing keys go to the buffer; Ctrl+S saves, Ctrl+R
    /// reloads, Esc returns to the chat. Read-only buffers explain why typing
    /// does nothing.
    fn editor_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.ghost = None;
                self.focus = Focus::Chat;
                return Action::None;
            }
            KeyCode::Char('s') if ctrl => {
                self.save_editor();
                self.refresh_ghost();
                return Action::None;
            }
            KeyCode::Char('r') if ctrl => {
                self.reload_editor();
                self.ghost = None;
                return Action::None;
            }
            _ => {}
        }
        // A shown ghost is accepted by Tab or Right; otherwise those keys keep
        // their usual meaning (literal tab / move right).
        if self.ghost.is_some() && !ctrl && matches!(key.code, KeyCode::Tab | KeyCode::Right) {
            let suffix = self.ghost.take().unwrap();
            let mut from = None;
            if let Some(e) = &mut self.editor {
                from = Some(e.cursor_line);
                e.insert_str(&suffix);
            }
            if let Some(line) = from {
                self.invalidate_hl(line);
            }
            self.refresh_ghost();
            return Action::None;
        }
        let mut readonly_reason = None;
        let mut edited_from = None;
        if let Some(e) = &mut self.editor {
            if e.is_read_only() {
                if is_edit_key(&key) {
                    readonly_reason = e.read_only_reason().map(str::to_string);
                }
            } else {
                let before = e.cursor_line;
                e.handle_key(key);
                if is_edit_key(&key) {
                    edited_from = Some(before.min(e.cursor_line));
                }
            }
        }
        if let Some(line) = edited_from {
            self.invalidate_hl(line);
        }
        if let Some(reason) = readonly_reason {
            self.status_msg = Some(reason);
        }
        self.refresh_ghost();
        Action::None
    }

    /// Diff focus: read-only scroll of the agent-changes pane.
    fn diff_key(&mut self, key: KeyEvent) -> Action {
        let delta = match key.code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -10,
            KeyCode::PageDown => 10,
            KeyCode::Esc => {
                self.focus = Focus::Chat;
                return Action::None;
            }
            _ => return Action::None,
        };
        if let Some(v) = &mut self.viewer {
            let len = v.diff.len();
            v.diff_scroll = (v.diff_scroll as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        }
        Action::None
    }

    /// Sidebar activation: directories expand (idempotent), files open.
    fn expand_or_open_selected(&mut self) {
        let Some(e) = self.entries.get(self.selected).cloned() else { return };
        if e.is_dir {
            if self.expanded.insert(e.path.clone()) {
                self.rescan();
            }
        } else {
            self.open_file(&e.path);
        }
    }

    /// Files-overlay activation: directories toggle expansion, files open.
    fn toggle_or_open_selected(&mut self) {
        let Some(e) = self.entries.get(self.selected).cloned() else { return };
        if e.is_dir {
            if !self.expanded.remove(&e.path) {
                self.expanded.insert(e.path.clone());
            }
            self.rescan();
        } else {
            self.open_file(&e.path);
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(e) = self.entries.get(self.selected).cloned() else { return };
        if e.is_dir && self.expanded.remove(&e.path) {
            self.rescan();
            return;
        }
        if let Some(parent) = e.path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Some(idx) = self.entries.iter().position(|x| x.path == parent) {
                    self.selected = idx;
                }
            }
        }
    }

    fn open_selected_change(&mut self) {
        if let Some((path, _)) = self.changes().get(self.change_selected) {
            self.open_file(&path.clone());
        }
    }

    /// Translates a mouse event against the layout captured in the last draw.
    pub fn handle_mouse(&mut self, ev: MouseEvent) -> Action {
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => self.mouse_click(ev.column, ev.row),
            MouseEventKind::ScrollUp => self.mouse_wheel(ev.column, ev.row, -1),
            MouseEventKind::ScrollDown => self.mouse_wheel(ev.column, ev.row, 1),
            _ => Action::None,
        }
    }

    fn mouse_click(&mut self, col: u16, row: u16) -> Action {
        if let Some(overlay) = self.overlay {
            match self.overlay_area {
                Some(area) if rect_contains(area, col, row) => self.click_overlay(overlay, area, row),
                // A click anywhere outside the open overlay dismisses it.
                _ => self.overlay = None,
            }
            return Action::None;
        }
        let Some(l) = self.layout else { return Action::None };
        if let Some(tb) = l.toolbar {
            if rect_contains(tb, col, row) {
                // Tools are listed from the third row of the bar (title + blank line).
                if row >= tb.y + 2 {
                    if let Some((_, key)) = TOOLS.get((row - tb.y - 2) as usize) {
                        return self.prefix_command(KeyEvent::new(KeyCode::Char(*key), KeyModifiers::NONE));
                    }
                }
                return Action::None;
            }
        }
        if let Some(tree) = self.tree_area {
            if rect_contains(tree, col, row) {
                self.click_sidebar(tree, row);
                return Action::None;
            }
        }
        if let Some(ap) = self.apps_area {
            if rect_contains(ap, col, row) {
                self.click_apps(ap, col, row);
                return Action::None;
            }
        }
        if let Some(ed) = l.editor {
            if rect_contains(ed, col, row) {
                self.focus = Focus::Editor;
                if let Some(e) = &mut self.editor {
                    let gutter = e.gutter_width();
                    e.click(ed.x + 1, ed.y + 1, gutter, col, row);
                }
                self.refresh_ghost();
                return Action::None;
            }
        }
        if let Some(df) = l.diff {
            if rect_contains(df, col, row) {
                self.focus = Focus::Diff;
                return Action::None;
            }
        }
        if rect_contains(l.chat, col, row) {
            self.focus = Focus::Chat;
        }
        Action::None
    }

    fn click_overlay(&mut self, overlay: Overlay, area: Rect, row: u16) {
        // Skip the top/bottom borders of the overlay box.
        if row <= area.y || row + 1 >= area.y + area.height {
            return;
        }
        let rel = (row - area.y - 1) as usize;
        match overlay {
            Overlay::Files => {
                let idx = rel + self.tree_offset;
                if idx < self.entries.len() {
                    self.selected = idx;
                    self.toggle_or_open_selected();
                }
            }
            Overlay::Changes => {
                if rel < self.changes().len() {
                    self.change_selected = rel;
                    self.open_selected_change();
                }
            }
            Overlay::Apps => self.click_apps(area, (area.x + 1).max(0), row),
            Overlay::Help => {}
        }
    }

    /// A click inside the apps panel selects the app or item under the cursor
    /// and focuses the panel. The panel body starts one row below the border.
    fn click_apps(&mut self, area: Rect, _col: u16, row: u16) {
        self.focus = Focus::Apps;
        if row <= area.y || row + 1 >= area.y + area.height {
            return;
        }
        let rel = (row - area.y - 1) as usize;
        match self.apps.view {
            View::Apps => {
                if rel < self.apps.apps.len() {
                    self.apps.app_sel = rel;
                }
            }
            View::Items | View::Reply => {
                // Row 0 is the app header; items start at row 1.
                if rel >= 1 {
                    let idx = rel - 1;
                    if idx < self.apps.current_items().len() {
                        self.apps.item_sel = idx;
                    }
                }
            }
        }
    }

    fn click_sidebar(&mut self, side: Rect, row: u16) {
        // Ignore the border rows and the single details row at the bottom.
        if row <= side.y || row + 1 >= side.y + side.height {
            return;
        }
        let list_h = side.height.saturating_sub(3) as usize;
        let rel = (row - side.y - 1) as usize;
        if rel >= list_h {
            return;
        }
        let idx = rel + self.tree_offset;
        if idx < self.entries.len() {
            self.selected = idx;
            // A click toggles folders, so a second click closes an open one.
            self.toggle_or_open_selected();
        }
    }

    fn mouse_wheel(&mut self, col: u16, row: u16, dir: isize) -> Action {
        let Some(l) = self.layout else { return Action::None };
        if let Some(tree) = self.tree_area {
            if rect_contains(tree, col, row) {
                self.selected = clamp_index(self.selected, dir, self.entries.len());
                return Action::None;
            }
        }
        if let Some(ap) = self.apps_area {
            if rect_contains(ap, col, row) {
                if self.apps.is_expanded() {
                    let len = self.apps.current_items().len();
                    self.apps.item_sel = clamp_index(self.apps.item_sel, dir, len);
                } else {
                    self.apps.app_sel = clamp_index(self.apps.app_sel, dir, self.apps.apps.len());
                }
                return Action::None;
            }
        }
        if let Some(ed) = l.editor {
            if rect_contains(ed, col, row) {
                if let Some(e) = &mut self.editor {
                    e.scroll_lines(dir * 3);
                }
                return Action::None;
            }
        }
        if let Some(df) = l.diff {
            if rect_contains(df, col, row) {
                if let Some(v) = &mut self.viewer {
                    let len = v.diff.len();
                    v.diff_scroll = (v.diff_scroll as isize + dir * 3).clamp(0, len.saturating_sub(1) as isize) as usize;
                }
                return Action::None;
            }
        }
        if rect_contains(l.chat, col, row) {
            // Positive = scroll back to older output.
            return Action::ScrollChat(-dir * 3);
        }
        Action::None
    }
}

/// Moves `index` by `delta`, clamped to `[0, len)`.
fn clamp_index(index: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (index as isize + delta).clamp(0, len as isize - 1) as usize
}

fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    r.contains(Position::new(col, row))
}

/// Keys that mutate the buffer (used to explain a read-only buffer).
fn is_edit_key(key: &KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab
        )
}

fn status_style(s: Option<GitStatus>) -> Style {
    match s {
        Some(GitStatus::Added | GitStatus::Untracked) => Style::default().fg(Color::Green),
        Some(GitStatus::Deleted | GitStatus::Conflicted) => Style::default().fg(Color::Red),
        Some(_) => Style::default().fg(Color::Yellow),
        None => Style::default(),
    }
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
    Block::default().borders(Borders::ALL).border_style(style).title(title)
}

/// Colour of the status dot: green connected, yellow connecting, gray
/// disconnected, red on error (S12).
fn app_dot_style(status: AppState) -> Style {
    let color = match status {
        AppState::Connected => Color::Green,
        AppState::Connecting => Color::Yellow,
        AppState::Disconnected => Color::DarkGray,
        AppState::Error => Color::Red,
    };
    Style::default().fg(color)
}

/// Truncates `s` to `w` columns (char-based; glyphs here are single-width).
fn fit(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

/// Builds the apps-panel content lines for a pane (or overlay) `w` columns wide.
fn apps_content_lines(app: &mut App, w: usize, _h: usize, focused: bool) -> Vec<Line<'static>> {
    let w = w.max(1);
    if app.apps.offline {
        return vec![Line::styled("apps engine offline", Style::default().fg(Color::DarkGray))];
    }
    if app.apps.apps.is_empty() {
        return vec![Line::styled("connecting\u{2026}", Style::default().fg(Color::DarkGray))];
    }
    match app.apps.view {
        View::Apps => apps_list_lines(app, w, focused),
        View::Items | View::Reply => apps_items_lines(app, w, focused),
    }
}

/// Collapsed view: one row per app with a status dot and item count.
fn apps_list_lines(app: &App, w: usize, focused: bool) -> Vec<Line<'static>> {
    app.apps
        .apps
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let glyph = icons::app_icon(&a.id, app.icons);
            let count = app.apps.item_count(&a.id);
            let selected = focused && i == app.apps.app_sel;
            let mut label_style = Style::default();
            if selected {
                label_style = label_style.add_modifier(Modifier::REVERSED);
            }
            let label = fit(&format!("{glyph} {}  ({count})", a.name), w.saturating_sub(2));
            Line::from(vec![
                Span::styled("\u{25cf} ", app_dot_style(a.status)),
                Span::styled(label, label_style),
            ])
        })
        .collect()
}

/// Expanded view: the selected app's items (title + one-line preview) and, when
/// replying, a reply input box at the bottom.
fn apps_items_lines(app: &mut App, w: usize, focused: bool) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let (dot_style, header) = match app.apps.apps.get(app.apps.app_sel) {
        Some(a) => (
            app_dot_style(a.status),
            format!("{} {}", icons::app_icon(&a.id, app.icons), a.name),
        ),
        None => (Style::default(), String::new()),
    };
    out.push(Line::from(vec![
        Span::styled("\u{25cf} ", dot_style),
        Span::styled(fit(&header, w.saturating_sub(2)), Style::default().add_modifier(Modifier::BOLD)),
    ]));

    let items = app.apps.current_items().to_vec();
    if items.is_empty() {
        out.push(Line::styled("  no messages", Style::default().fg(Color::DarkGray)));
    }
    for (i, it) in items.iter().enumerate() {
        let mut title_style = Style::default();
        if focused && i == app.apps.item_sel {
            title_style = title_style.add_modifier(Modifier::REVERSED);
        }
        out.push(Line::styled(fit(&it.title, w), title_style));
        let preview: String = it.text.lines().next().unwrap_or("").to_string();
        out.push(Line::styled(format!("  {}", fit(&preview, w.saturating_sub(2))), Style::default().fg(Color::DarkGray)));
    }

    if app.apps.view == View::Reply {
        out.push(Line::styled("\u{2500}".repeat(w), Style::default().fg(Color::DarkGray)));
        let box_w = w.saturating_sub(2);
        app.apps.reply.ensure_visible(box_w);
        let visible = app.apps.reply.visible(box_w);
        out.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(visible),
        ]));
        if let Some(status) = &app.apps.reply_status {
            out.push(Line::styled(fit(status, w), Style::default().fg(Color::Yellow)));
        }
    }
    out
}

/// Draws the bottom-left apps panel into `area` (S12).
fn draw_apps_panel(frame: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    if area.height == 0 {
        return;
    }
    let block = pane_block(" Apps ".into(), focused);
    let inner = block.inner(area);
    let lines = apps_content_lines(app, inner.width as usize, inner.height as usize, focused);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    // Place the caret in the reply box so typing is visible.
    if focused && app.apps.view == View::Reply {
        let box_w = inner.width.saturating_sub(2) as usize;
        let caret = app.apps.reply.cursor.saturating_sub(app.apps.reply.scroll).min(box_w) as u16;
        let cx = inner.x + 2 + caret;
        let cy = inner.y + inner.height.saturating_sub(if app.apps.reply_status.is_some() { 2 } else { 1 });
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            frame.set_cursor_position(Position::new(cx, cy));
        }
    }
}

fn tree_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
    app.entries
        .iter()
        .enumerate()
        .skip(app.tree_offset)
        .take(height)
        .map(|(i, e)| {
            let expanded = e.is_dir && app.expanded.contains(&e.path);
            let (glyph, gcolor) = icons::icon_for(e, expanded, app.icons);
            // The icon cell is always two columns: glyph + space, or two spaces
            // when there is no glyph (the ASCII fallback for files).
            let icon_cell = if glyph.is_empty() { "  ".to_string() } else { format!("{glyph} ") };
            let mark = e.status.map_or(' ', |s| s.marker());
            let prefix = format!("{mark} {}", "  ".repeat(e.depth));
            let right = if e.is_dir { String::new() } else { tree::human_size(e.size) };
            // Each glyph is one column, so char count equals column width here.
            let left_len = prefix.chars().count() + icon_cell.chars().count() + e.name.chars().count();
            let pad = width.saturating_sub(left_len + right.len()).max(1);
            let mut base = status_style(e.status);
            if e.is_dir {
                base = base.add_modifier(Modifier::BOLD);
            }
            if i == app.selected {
                base = base.add_modifier(Modifier::REVERSED);
            }
            // Colour the glyph with its brand colour (Nerd set only); the ASCII
            // fallback keeps the base style so it renders exactly as before.
            let icon_style = if app.icons == IconMode::Nerd && !glyph.is_empty() { base.fg(gcolor) } else { base };
            Line::from(vec![
                Span::styled(prefix, base),
                Span::styled(icon_cell, icon_style),
                Span::styled(format!("{}{}{}", e.name, " ".repeat(pad), right), base),
            ])
        })
        .collect()
}

/// Language for the open file, from its extension.
fn lang_for_path(rel: &Path) -> highlight::Lang {
    let ext = rel.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    highlight::lang_for_ext(&ext)
}

/// The bracket (if any) at or just before `col` on `line`, paired with its
/// match. Returns `[cursor_bracket, matching_bracket]` positions.
fn bracket_pair(lines: &[String], line: usize, col: usize) -> Option<[(usize, usize); 2]> {
    let mut cols = vec![col];
    if col > 0 {
        cols.push(col - 1);
    }
    for c in cols {
        if let Some(m) = highlight::match_bracket(lines, line, c) {
            return Some([(line, c), m]);
        }
    }
    None
}

/// Builds the visible, highlighted spans of one code line: it slices the
/// tokenised line to the `[scroll_col, scroll_col + text_w)` window and bolds +
/// underlines any column in `emph` (the matching-bracket pair).
fn code_spans(segs: &[highlight::Seg], scroll_col: usize, text_w: usize, emph: &[usize]) -> Vec<Span<'static>> {
    let visible_end = scroll_col + text_w;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_style: Option<Style> = None;
    fn flush(spans: &mut Vec<Span<'static>>, cur: &mut String, style: Option<Style>) {
        if !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(cur), style.unwrap_or_default()));
        }
    }
    let mut col = 0usize;
    for seg in segs {
        let base = highlight::class_style(seg.class);
        for ch in seg.text.chars() {
            if col >= scroll_col && col < visible_end {
                let mut st = base;
                if emph.contains(&col) {
                    st = st.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                if cur_style != Some(st) {
                    flush(&mut spans, &mut cur, cur_style);
                    cur_style = Some(st);
                }
                cur.push(ch);
            }
            col += 1;
        }
    }
    flush(&mut spans, &mut cur, cur_style);
    spans
}

fn entry_details(e: &Entry) -> String {
    let status = e.status.map_or("clean".to_string(), |s| format!("{s:?}").to_lowercase());
    if e.is_dir {
        format!("dir · {status}")
    } else {
        format!("{} · {} · {status}", e.format, tree::human_size(e.size))
    }
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

pub fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width as u32 * pct_w as u32 / 100) as u16;
    let h = (area.height as u32 * pct_h as u32 / 100) as u16;
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// Draws every partition. Each pane writes only into its own rectangle and
/// ratatui flushes only the cells that changed since the previous frame (S3).
pub fn draw(frame: &mut Frame, app: &mut App, pty: Option<&PtyPane>) {
    let area = frame.area();
    let l = app.compute_layout(area);
    app.layout = Some(l);
    let focus = app.effective_focus();

    if let Some(bar) = l.toolbar {
        let mut lines = vec![Line::styled(" TOOLS", Style::default().add_modifier(Modifier::BOLD)), Line::raw("")];
        for (label, key) in TOOLS {
            lines.push(Line::from(vec![Span::raw(format!(" {label:<9}")), Span::styled(key.to_string(), Style::default().fg(Color::Cyan))]));
        }
        let block = Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(lines).block(block), bar);
    }

    app.tree_area = None;
    app.apps_area = None;
    if let Some(side) = l.sidebar {
        // The sidebar is split into the file tree (top) and the apps panel (bottom, S12).
        let (tree_area, apps_area) =
            layout::split_sidebar(side, app.apps.apps.len() as u16, app.apps.is_expanded());
        app.tree_area = Some(tree_area);
        app.apps_area = Some(apps_area);
        let block = pane_block(format!(" Files · {} ", app.root.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())), focus == Focus::Sidebar);
        let inner = block.inner(tree_area);
        let list_h = inner.height.saturating_sub(1) as usize;
        app.tree_offset = keep_visible(app.selected, app.tree_offset, list_h);
        let mut lines = tree_lines(app, list_h, inner.width as usize);
        lines.resize(list_h, Line::raw(""));
        if let Some(e) = app.entries.get(app.selected) {
            lines.push(Line::styled(entry_details(e), Style::default().fg(Color::DarkGray)));
        }
        frame.render_widget(Paragraph::new(lines).block(block), tree_area);
        draw_apps_panel(frame, app, apps_area, focus == Focus::Apps);
    }

    if let (Some(ed), Some(df)) = (l.editor, l.diff) {
        // Left pane: the editable buffer. Take the highlighter out so it can be
        // borrowed mutably alongside the editor; it is restored below.
        let ghost = if focus == Focus::Editor { app.ghost.clone() } else { None };
        let mut highlighter = app.highlighter.take();
        match &mut app.editor {
            Some(e) => {
                let mark = if e.dirty { "\u{25cf} " } else { "" };
                let title = format!(" {}{} ", mark, e.path.display());
                let block = pane_block(title, focus == Focus::Editor);
                let inner = block.inner(ed);
                let gutter = e.gutter_width();
                let num_w = gutter.saturating_sub(1);
                let text_h = inner.height as usize;
                let text_w = (inner.width as usize).saturating_sub(gutter);
                e.page = text_h.max(1);
                e.ensure_visible(text_h, text_w);
                let (scroll_row, scroll_col) = (e.scroll_row, e.scroll_col);
                let cursor_line = e.cursor_line;
                let cursor_vcol = e.cursor_visual_col();
                // Highlight runs on the tab-expanded lines, matching what the
                // cursor column and the rendered slice use.
                let expanded_lines: Vec<String> = e.lines.iter().map(|t| crate::editor::expand_tabs(t)).collect();
                if let Some(h) = &mut highlighter {
                    h.ensure(&expanded_lines, scroll_row + text_h);
                }
                // The bracket under/after the cursor and its match (if any).
                let matched = if focus == Focus::Editor {
                    bracket_pair(&expanded_lines, cursor_line, cursor_vcol)
                } else {
                    None
                };
                let text_w = text_w.max(1);
                let last = (scroll_row + text_h).min(expanded_lines.len());
                let lines: Vec<Line> = (scroll_row..last)
                    .map(|i| {
                        let mut spans =
                            vec![Span::styled(format!("{:>num_w$} ", i + 1), Style::default().fg(Color::DarkGray))];
                        let emph: Vec<usize> =
                            matched.iter().flatten().filter(|(l, _)| *l == i).map(|(_, c)| *c).collect();
                        match &highlighter {
                            Some(h) => spans.extend(code_spans(h.segs(i), scroll_col, text_w, &emph)),
                            None => {
                                let shown: String =
                                    expanded_lines[i].chars().skip(scroll_col).take(text_w).collect();
                                spans.push(Span::raw(shown));
                            }
                        }
                        // Dim ghost suggestion rendered after the cursor (S9).
                        if i == cursor_line {
                            if let Some(g) = &ghost {
                                spans.push(Span::styled(g.clone(), Style::default().fg(Color::DarkGray)));
                            }
                        }
                        Line::from(spans)
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines).block(block), ed);
                if focus == Focus::Editor && app.overlay.is_none() {
                    let cx = inner.x + gutter as u16 + e.cursor_visual_col().saturating_sub(scroll_col) as u16;
                    let cy = inner.y + e.cursor_line.saturating_sub(scroll_row) as u16;
                    if cx < inner.x + inner.width && cy < inner.y + inner.height {
                        frame.set_cursor_position(Position::new(cx, cy));
                    }
                }
            }
            None => {
                let hint = Paragraph::new("Select a file in the sidebar (Enter or click).").style(Style::default().fg(Color::DarkGray));
                frame.render_widget(hint.block(pane_block(" Editor ".into(), focus == Focus::Editor)), ed);
            }
        }
        app.highlighter = highlighter;
        // Right pane: the agent's changes (read-only).
        match &app.viewer {
            Some(v) => {
                let lines: Vec<Line> = v
                    .diff
                    .iter()
                    .skip(v.diff_scroll)
                    .take(df.height as usize)
                    .map(|d| {
                        let style = match d.kind {
                            DiffKind::Added => Style::default().fg(Color::Green),
                            DiffKind::Removed => Style::default().fg(Color::Red),
                            DiffKind::Hunk => Style::default().fg(Color::Cyan),
                            DiffKind::Meta => Style::default().fg(Color::DarkGray),
                            DiffKind::Context => Style::default(),
                        };
                        Line::styled(d.text.clone(), style)
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines).block(pane_block(" Agent changes ".into(), focus == Focus::Diff)), df);
            }
            None => {
                frame.render_widget(Paragraph::new("").block(pane_block(" Agent changes ".into(), focus == Focus::Diff)), df);
            }
        }
    }

    let chat = match l.mode {
        Mode::Workspace => {
            let block = pane_block(" Chat · gentle-shell ".into(), focus == Focus::Chat);
            let inner = block.inner(l.chat);
            frame.render_widget(block, l.chat);
            inner
        }
        Mode::Chat => l.chat,
    };
    if let Some(pty) = pty {
        pty.with_screen(|screen| {
            pane::render_screen(screen, chat, frame.buffer_mut());
            let (row, col) = screen.cursor_position();
            if focus == Focus::Chat && app.overlay.is_none() && screen.scrollback() == 0 && !screen.hide_cursor() {
                frame.set_cursor_position(Position::new(chat.x + col.min(chat.width.saturating_sub(1)), chat.y + row.min(chat.height.saturating_sub(1))));
            }
        });
    }

    app.overlay_area = None;
    if let Some(overlay) = app.overlay {
        let area = centered(l.chat, 80, 80);
        app.overlay_area = Some(area);
        frame.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan));
        let (title, lines) = match overlay {
            Overlay::Files => {
                let h = area.height.saturating_sub(2) as usize;
                app.tree_offset = keep_visible(app.selected, app.tree_offset, h);
                (" Files · Enter open · Esc close ", tree_lines(app, h, area.width.saturating_sub(2) as usize))
            }
            Overlay::Changes => {
                let changes = app.changes();
                let lines = if changes.is_empty() {
                    vec![Line::raw("No changes in the worktree.")]
                } else {
                    changes
                        .iter()
                        .enumerate()
                        .map(|(i, (p, s))| {
                            let mut style = status_style(Some(*s));
                            if i == app.change_selected {
                                style = style.add_modifier(Modifier::REVERSED);
                            }
                            Line::styled(format!("{} {}", s.marker(), p.display()), style)
                        })
                        .collect()
                };
                (" Changes · Enter open · Esc close ", lines)
            }
            Overlay::Apps => {
                let w = area.width.saturating_sub(2) as usize;
                let h = area.height.saturating_sub(2) as usize;
                (" Apps · Enter open · Esc back ", apps_content_lines(app, w, h, true))
            }
            Overlay::Help => (" Help · Esc close ", help_lines()),
        };
        frame.render_widget(Paragraph::new(lines).block(block.title(title)), area);
    }

    let status = if app.prefix {
        Line::styled(format!(" {PREFIX_LABEL} … w workspace · t files · c changes · a apps · Tab/1-4 focus · ? help · q quit · ] send {PREFIX_LABEL}"), Style::default().fg(Color::Black).bg(Color::Cyan))
    } else if let Some(msg) = &app.status_msg {
        Line::styled(format!(" {msg}"), Style::default().fg(Color::Black).bg(Color::Yellow))
    } else {
        let mode = match l.mode {
            Mode::Chat => "chat",
            Mode::Workspace => "workspace",
        };
        Line::styled(
            format!(" gentle-partitioner · {mode} · focus {:?} · {PREFIX_LABEL} then ? for help", focus).to_lowercase(),
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Paragraph::new(status), l.status);
}

pub fn help_lines() -> Vec<Line<'static>> {
    [
        format!("{PREFIX_LABEL} then:"),
        "  w      toggle workspace (files | editor | agent changes | chat)".into(),
        "  t      files tool (overlay)".into(),
        "  c      worktree changes tool (overlay)".into(),
        "  a      apps panel (toggle in workspace / overlay in chat)".into(),
        "  Tab    next pane   1-4 sidebar / editor / changes / chat".into(),
        "  q      quit (stops gentle-shell)".into(),
        format!("  ]      send a literal {PREFIX_LABEL} to the chat"),
        String::new(),
        "Sidebar: ↑↓ move · Enter/→ open · ← collapse · Esc back to chat".into(),
        "Editor: type to edit · Enter/Backspace/Delete · Tab · arrows · Home/End".into(),
        "        PageUp/PageDown · Ctrl+Home/End · Ctrl+S save · Ctrl+R reload".into(),
        "        ● marks unsaved edits · Esc back to chat".into(),
        "Agent changes: ↑↓ PgUp PgDn scroll · Esc back to chat".into(),
        "Apps: ↑↓ select · Enter app→items→reply · type to reply · Esc back".into(),
        "Mouse: click selects panes and places the cursor; wheel scrolls.".into(),
    ]
    .into_iter()
    .map(Line::raw)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    fn prefix() -> KeyEvent {
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)
    }

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gentle-partitioner-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("README.md"), "# hi\n").unwrap();
        root
    }

    /// Renders once so the app knows its layout, as the event loop does.
    fn render(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, None)).unwrap();
        t
    }

    fn screen_text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn chat_focus_forwards_keys_to_the_pty() {
        let root = fixture("fwd");
        let mut app = App::new(root.clone());
        render(&mut app, 120, 40);
        assert_eq!(app.handle_key(ch('a')), Action::Forward(b"a".to_vec()));
        app.app_cursor = true;
        assert_eq!(app.handle_key(key(KeyCode::Up)), Action::Forward(b"\x1bOA".to_vec()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_commands_toggle_workspace_quit_and_send_literal_prefix() {
        let root = fixture("prefix");
        let mut app = App::new(root.clone());
        render(&mut app, 120, 40);
        assert_eq!(app.handle_key(prefix()), Action::None);
        assert!(app.prefix);
        assert_eq!(app.handle_key(ch('w')), Action::None);
        assert!(!app.prefix);
        assert_eq!((app.mode, app.focus), (Mode::Workspace, Focus::Sidebar));
        app.handle_key(prefix());
        app.handle_key(ch('w'));
        assert_eq!((app.mode, app.focus), (Mode::Chat, Focus::Chat));
        app.handle_key(prefix());
        assert_eq!(app.handle_key(ch(']')), Action::Forward(vec![0x1d]));
        app.handle_key(prefix());
        assert_eq!(app.handle_key(ch('q')), Action::Quit);
        // unknown prefix command is swallowed, not forwarded
        app.handle_key(prefix());
        assert_eq!(app.handle_key(ch('z')), Action::None);
        assert!(!app.prefix);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn key_release_events_are_ignored() {
        let root = fixture("release");
        let mut app = App::new(root.clone());
        let mut k = ch('a');
        k.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(k), Action::None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn focus_cycles_only_through_visible_panes() {
        let root = fixture("focus");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Sidebar;
        render(&mut app, 120, 40);
        let mut seen = vec![];
        for _ in 0..4 {
            app.handle_key(prefix());
            app.handle_key(key(KeyCode::Tab));
            seen.push(app.focus);
        }
        assert_eq!(seen, [Focus::Editor, Focus::Diff, Focus::Chat, Focus::Sidebar]);
        app.handle_key(prefix());
        app.handle_key(ch('4'));
        assert_eq!(app.focus, Focus::Chat);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sidebar_navigation_expands_dirs_and_opens_files_in_the_viewer() {
        let root = fixture("nav");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Sidebar;
        render(&mut app, 120, 40);
        assert_eq!(app.entries[0].name, "src");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(app.expanded.contains(Path::new("src")));
        assert_eq!(app.entries[1].path, PathBuf::from("src/main.rs"));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let v = app.viewer.as_ref().unwrap();
        assert_eq!(v.path, PathBuf::from("src/main.rs"));
        assert_eq!(v.content, ["fn main() {}"]);
        assert_eq!(app.focus, Focus::Editor);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Chat);
        // back to sidebar: Left on a file jumps to its parent, Left on an open dir collapses it
        app.focus = Focus::Sidebar;
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Left));
        assert!(!app.expanded.contains(Path::new("src")));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0, "selection clamps at the top");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_overlay_opens_over_chat_and_switches_to_workspace_on_open() {
        let root = fixture("overlay");
        let mut app = App::new(root.clone());
        render(&mut app, 120, 40);
        app.handle_key(prefix());
        app.handle_key(ch('t'));
        assert_eq!(app.overlay, Some(Overlay::Files));
        // keys go to the overlay, not to the PTY
        assert_eq!(app.handle_key(key(KeyCode::Down)), Action::None);
        assert_eq!(app.entries[app.selected].name, "README.md");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.overlay, None);
        assert_eq!(app.mode, Mode::Workspace);
        assert_eq!(app.viewer.as_ref().unwrap().path, PathBuf::from("README.md"));
        app.handle_key(prefix());
        app.handle_key(ch('?'));
        assert_eq!(app.overlay, Some(Overlay::Help));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changes_overlay_lists_statuses_and_opens_the_selected_one() {
        let root = fixture("changes");
        let mut app = App::new(root.clone());
        app.apply_statuses([(PathBuf::from("src/main.rs"), GitStatus::Modified), (PathBuf::from("README.md"), GitStatus::Added)].into());
        render(&mut app, 120, 40);
        app.handle_key(prefix());
        app.handle_key(ch('c'));
        assert_eq!(app.overlay, Some(Overlay::Changes));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.change_selected, 1, "clamped to the last change");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.viewer.as_ref().unwrap().path, PathBuf::from("src/main.rs"));
        fs::remove_dir_all(root).unwrap();
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn mouse_clicks_select_tools_files_and_panes_and_wheel_scrolls_chat() {
        let root = fixture("mouse");
        let mut app = App::new(root.clone());
        render(&mut app, 120, 40);
        // toolbar row 2 is the first tool ("Files")
        assert_eq!(app.handle_mouse(click(2, 2)), Action::None);
        assert_eq!(app.overlay, Some(Overlay::Files));
        app.handle_key(key(KeyCode::Esc));
        let wheel = MouseEvent { kind: MouseEventKind::ScrollUp, column: 60, row: 10, modifiers: KeyModifiers::NONE };
        assert_eq!(app.handle_mouse(wheel), Action::ScrollChat(3));

        app.handle_key(prefix());
        app.handle_key(ch('w'));
        render(&mut app, 120, 40);
        let side = app.layout.unwrap().sidebar.unwrap();
        // first tree row is inside the border: click README.md (row index 1)
        app.handle_mouse(click(side.x + 3, side.y + 2));
        assert_eq!(app.viewer.as_ref().unwrap().path, PathBuf::from("README.md"));
        let chat = app.layout.unwrap().chat;
        app.handle_mouse(click(chat.x + 5, chat.y + 2));
        assert_eq!(app.focus, Focus::Chat);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn render_shows_toolbar_in_chat_mode_and_partitions_in_workspace() {
        let root = fixture("render");
        let mut app = App::new(root.clone());
        let t = render(&mut app, 120, 30);
        let text = screen_text(&t);
        assert!(text.contains("TOOLS"));
        assert!(text.contains("Files"));
        assert!(text.contains("gentle-partitioner · chat"));
        app.mode = Mode::Workspace;
        let t = render(&mut app, 120, 30);
        let text = screen_text(&t);
        assert!(text.contains("README.md"));
        assert!(text.contains("Agent changes"));
        assert!(text.contains("Chat · gentle-shell"));
        assert!(!text.contains("TOOLS"));
        fs::remove_dir_all(root).unwrap();
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn git(dir: &Path, args: &[&str]) {
        use std::process::Command;
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
    fn dirty_guard_blocks_opening_another_file_until_the_second_attempt() {
        let root = fixture("dirty-open");
        let mut app = App::new(root.clone());
        app.open_file(Path::new("README.md"));
        render(&mut app, 120, 40);
        assert_eq!(app.handle_key(ch('x')), Action::None);
        assert!(app.editor.as_ref().unwrap().dirty);
        // first open of a new target is refused and warns
        app.open_file(Path::new("src/main.rs"));
        assert_eq!(app.editor.as_ref().unwrap().path, PathBuf::from("README.md"));
        assert_eq!(app.status_msg.as_deref(), Some("unsaved changes \u{2014} Ctrl+S save, open again to discard"));
        // opening the same target again discards the edits and switches
        app.open_file(Path::new("src/main.rs"));
        assert_eq!(app.editor.as_ref().unwrap().path, PathBuf::from("src/main.rs"));
        assert!(!app.editor.as_ref().unwrap().dirty);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_guard_warns_before_quitting_then_quits_on_the_second_q() {
        let root = fixture("dirty-quit");
        let mut app = App::new(root.clone());
        app.open_file(Path::new("README.md"));
        render(&mut app, 120, 40);
        app.handle_key(ch('x'));
        assert!(app.editor.as_ref().unwrap().dirty);
        app.handle_key(prefix());
        assert_eq!(app.handle_key(ch('q')), Action::None);
        assert!(app.pending_quit);
        app.handle_key(prefix());
        assert_eq!(app.handle_key(ch('q')), Action::Quit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saving_writes_the_file_and_refreshes_the_diff() {
        let root = std::env::temp_dir().join(format!("gentle-partitioner-app-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let mut app = App::new(root.clone());
        app.open_file(Path::new("a.txt"));
        render(&mut app, 120, 40);
        // clean file: no diff against HEAD yet
        assert!(app.viewer.as_ref().unwrap().diff.iter().all(|d| d.kind != DiffKind::Added));
        app.handle_key(key(KeyCode::End));
        app.handle_key(ch('X'));
        assert!(app.editor.as_ref().unwrap().dirty);
        app.handle_key(ctrl('s'));
        assert!(!app.editor.as_ref().unwrap().dirty, "dirty flag cleared after save");
        assert!(app.take_git_refresh(), "a git refresh was requested");
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"oneX\n");
        let diff = &app.viewer.as_ref().unwrap().diff;
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added && d.text == "+oneX"), "diff refreshed: {diff:?}");
        fs::remove_dir_all(root).unwrap();
    }

    fn engine_with(text: &str) -> Arc<RwLock<Engine>> {
        let engine = Arc::new(RwLock::new(Engine::new()));
        engine.write().unwrap().learn(text, PredictMode::Code);
        engine
    }

    /// Opens README, drops onto a fresh empty line and types `typed`, leaving
    /// the cursor at the end of the line so a ghost can appear.
    fn editor_typing(app: &mut App, typed: &str) {
        app.open_file(Path::new("README.md"));
        render(app, 120, 40);
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Enter));
        for c in typed.chars() {
            app.handle_key(ch(c));
        }
    }

    #[test]
    fn ghost_is_shown_and_tab_accepts_it() {
        let root = fixture("ghost-tab");
        let mut app = App::new(root.clone());
        app.set_predict(engine_with("configuración configuración"));
        editor_typing(&mut app, "configura");
        assert_eq!(app.ghost.as_deref(), Some("ción"));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.editor.as_ref().unwrap().lines[1], "configuración");
        // Completed word: nothing longer to offer, so the ghost is gone.
        assert_eq!(app.ghost, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn right_accepts_the_ghost() {
        let root = fixture("ghost-right");
        let mut app = App::new(root.clone());
        app.set_predict(engine_with("configuración configuración"));
        editor_typing(&mut app, "configura");
        assert_eq!(app.ghost.as_deref(), Some("ción"));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.editor.as_ref().unwrap().lines[1], "configuración");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_key_clears_the_ghost() {
        let root = fixture("ghost-clear");
        let mut app = App::new(root.clone());
        app.set_predict(engine_with("configuración configuración"));
        editor_typing(&mut app, "configura");
        assert!(app.ghost.is_some());
        // Moving the cursor off the line end clears the suggestion.
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.ghost, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tab_and_right_are_unchanged_without_a_ghost() {
        let root = fixture("no-ghost");
        let mut app = App::new(root.clone());
        // No engine wired: the ghost is always None, so Tab/Right keep their
        // usual meaning exactly as before (literal tab, move right).
        app.open_file(Path::new("README.md"));
        render(&mut app, 120, 40);
        assert_eq!(app.handle_key(key(KeyCode::Tab)), Action::None);
        assert_eq!(app.editor.as_ref().unwrap().lines[0], "\t# hi");
        let before = app.editor.as_ref().unwrap().cursor_col;
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.editor.as_ref().unwrap().cursor_col, before + 1);
        assert_eq!(app.ghost, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hidden_panes_fall_back_to_chat_focus_on_small_terminals() {
        let root = fixture("small");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Sidebar;
        render(&mut app, 60, 15);
        assert_eq!(app.effective_focus(), Focus::Chat);
        assert_eq!(app.handle_key(ch('x')), Action::Forward(b"x".to_vec()));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn sidebar_renders_the_nerd_glyph_for_a_py_file() {
        let root = std::env::temp_dir().join(format!("gentle-partitioner-app-pyicon-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("app.py"), "print(1)\n").unwrap();
        let mut app = App::new(root.clone());
        app.icons = IconMode::Nerd;
        app.mode = Mode::Workspace;
        let t = render(&mut app, 120, 40);
        let py = icons::icon_for(&app.entries[0], false, IconMode::Nerd).0;
        assert_eq!(app.entries[0].name, "app.py");
        assert!(screen_text(&t).contains(py), "sidebar should render the python glyph {py:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn editor_highlights_code_without_losing_any_text() {
        let root = fixture("hl-render");
        let mut app = App::new(root.clone());
        app.open_file(Path::new("src/main.rs"));
        let t = render(&mut app, 120, 40);
        // The buffer text survives highlighting: keyword, name and brackets all show.
        let text = screen_text(&t);
        assert!(text.contains("fn main() {}"), "highlighted code keeps its text: {text:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bracket_pair_matches_on_or_right_after_the_cursor() {
        let lines = vec!["fn main() {".to_string(), "    x".to_string(), "}".to_string()];
        // Cursor just after the open brace at (0,10): pairs with (2,0).
        assert_eq!(bracket_pair(&lines, 0, 11), Some([(0, 10), (2, 0)]));
        // Cursor directly on the close brace at (2,0): pairs back to (0,10).
        assert_eq!(bracket_pair(&lines, 2, 0), Some([(2, 0), (0, 10)]));
        // Not near a bracket: nothing to highlight.
        assert_eq!(bracket_pair(&lines, 1, 2), None);
    }

    #[test]
    fn clicking_an_expanded_folder_in_the_sidebar_collapses_it() {
        let root = fixture("toggle-click");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Sidebar;
        render(&mut app, 120, 40);
        let side = app.layout.unwrap().sidebar.unwrap();
        assert_eq!(app.entries[0].name, "src");
        app.handle_mouse(click(side.x + 3, side.y + 1));
        assert!(app.expanded.contains(Path::new("src")), "first click expands");
        assert_eq!(app.entries[1].path, PathBuf::from("src/main.rs"));
        render(&mut app, 120, 40);
        app.handle_mouse(click(side.x + 3, side.y + 1));
        assert!(!app.expanded.contains(Path::new("src")), "second click collapses");
        assert!(app.entries.iter().all(|e| e.path != PathBuf::from("src/main.rs")));
        fs::remove_dir_all(root).unwrap();
    }

    // ---- Apps panel (S12) ----
    use gentle_apps::protocol::{AppStatus, Item as AppItem, Status as AppStatusKind};

    fn app_status(id: &str, name: &str, status: AppStatusKind) -> AppStatus {
        AppStatus { id: id.into(), name: name.into(), status, detail: None }
    }
    fn app_item(id: &str, title: &str) -> AppItem {
        AppItem { id: id.into(), title: title.into(), text: "body line".into(), ts: "1".into(), link: "l".into() }
    }

    #[test]
    fn apps_panel_expand_select_reply_and_esc_levels() {
        let root = fixture("apps-reducer");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one"), app_item("b", "#two")] });
        render(&mut app, 120, 40);
        assert_eq!(app.effective_focus(), Focus::Apps);

        // Enter on the app expands its item list and asks for a refresh.
        let act = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.apps.view, View::Items);
        assert!(matches!(act, Action::Apps(ClientMsg::Refresh { app, .. }) if app == "slack"));

        // Select the second item and open the reply box.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.apps.item_sel, 1);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.apps.view, View::Reply);

        // Type a reply and send it.
        for c in "hola".chars() {
            app.handle_key(ch(c));
        }
        assert_eq!(app.apps.reply.text, "hola");
        let act = app.handle_key(key(KeyCode::Enter));
        let id = match act {
            Action::Apps(ClientMsg::Reply { id, app: a, item, text }) => {
                assert_eq!((a.as_str(), item.as_str(), text.as_str()), ("slack", "b", "hola"));
                id
            }
            other => panic!("expected a reply action, got {other:?}"),
        };
        assert_eq!(app.apps.reply_status.as_deref(), Some("sending\u{2026}"));

        // The correlated result flips the status to "sent".
        app.apply_apps_msg(EngineMsg::Result { id, ok: true, error: None });
        assert_eq!(app.apps.reply_status.as_deref(), Some("sent"));

        // Esc steps back one level each time, then returns to the chat.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.apps.view, View::Items);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.apps.view, View::Apps);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Chat);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_reply_is_not_sent() {
        let root = fixture("apps-empty-reply");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one")] });
        render(&mut app, 120, 40);
        app.handle_key(key(KeyCode::Enter)); // expand
        app.handle_key(key(KeyCode::Enter)); // open reply
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None, "empty reply sends nothing");
        assert_eq!(app.apps.pending_reply, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_a_in_chat_opens_the_apps_overlay() {
        let root = fixture("apps-overlay");
        let mut app = App::new(root.clone());
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Disconnected)] });
        render(&mut app, 120, 40);
        app.handle_key(prefix());
        app.handle_key(ch('a'));
        assert_eq!(app.overlay, Some(Overlay::Apps));
        // Keys route to the panel, not the PTY.
        assert_eq!(app.handle_key(key(KeyCode::Down)), Action::None);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn render_shows_the_apps_header_and_the_slack_row() {
        let root = fixture("apps-render");
        let mut app = App::new(root.clone());
        app.icons = IconMode::Ascii;
        app.mode = Mode::Workspace;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Disconnected)] });
        let t = render(&mut app, 120, 40);
        let text = screen_text(&t);
        assert!(text.contains("Apps"), "apps header missing: {text:?}");
        assert!(text.contains("Slack"), "slack row missing");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apps_offline_shows_a_message_without_panicking() {
        let root = fixture("apps-offline");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.set_apps_offline();
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("apps engine offline"));
        fs::remove_dir_all(root).unwrap();
    }
}
