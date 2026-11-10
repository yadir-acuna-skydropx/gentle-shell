//! Application state and rendering. Pure state transitions live in
//! `handle_key`/`handle_mouse` so they are testable without a terminal;
//! the event loop in `main.rs` only wires threads, PTY and terminal.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::layout::{self, Layout, Mode};
use crate::pane::{self, PtyPane};
use crate::tree::{self, Entry, GitStatus};
use crate::viewer::{DiffKind, Viewer};

/// Prefix chord for partitioner commands; everything else goes to the focused pane.
pub const PREFIX_LABEL: &str = "Ctrl+]";

/// Lateral tool bar entries in chat mode (S7): label and prefix key.
pub const TOOLS: [(&str, char); 5] = [("Files", 't'), ("Changes", 'c'), ("Workspace", 'w'), ("Help", '?'), ("Quit", 'q')];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Editor,
    Diff,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Files,
    Changes,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Bytes for the chat PTY.
    Forward(Vec<u8>),
    /// Scroll the chat scrollback (positive = older output).
    ScrollChat(isize),
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
    pub prefix: bool,
    /// Child DECCKM state, updated by the loop before each key.
    pub app_cursor: bool,
    /// First tree row shown in the sidebar / files overlay.
    pub tree_offset: usize,
    pub layout: Option<Layout>,
    pub overlay_area: Option<Rect>,
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
            prefix: false,
            app_cursor: false,
            tree_offset: 0,
            layout: None,
            overlay_area: None,
        };
        app.rescan();
        app
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
        self.viewer = Some(Viewer::open(&self.root, rel));
        self.mode = Mode::Workspace;
        self.focus = Focus::Editor;
        self.overlay = None;
    }

    /// Translates a key event into either a partitioner command or bytes for
    /// the chat PTY. Pure state transition so it can be unit tested.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Only react to presses; Release (and auto Repeat on some terminals)
        // never reach the child or move the UI.
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }

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
            Focus::Editor | Focus::Diff => self.viewer_key(key),
        }
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
            KeyCode::Char('?') => {
                self.toggle_overlay(Overlay::Help);
                Action::None
            }
            KeyCode::Char('q') => Action::Quit,
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
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => self.overlay_move(overlay, -1),
            KeyCode::Down | KeyCode::Char('j') => self.overlay_move(overlay, 1),
            KeyCode::Enter => match overlay {
                Overlay::Files => self.toggle_or_open_selected(),
                Overlay::Changes => self.open_selected_change(),
                Overlay::Help => {}
            },
            _ => {}
        }
        Action::None
    }

    fn overlay_move(&mut self, overlay: Overlay, delta: isize) {
        match overlay {
            Overlay::Files => self.selected = clamp_index(self.selected, delta, self.entries.len()),
            Overlay::Changes => self.change_selected = clamp_index(self.change_selected, delta, self.changes().len()),
            Overlay::Help => {}
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

    fn viewer_key(&mut self, key: KeyEvent) -> Action {
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
            v.scroll(delta);
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
        if let Some(side) = l.sidebar {
            if rect_contains(side, col, row) {
                self.click_sidebar(side, row);
                return Action::None;
            }
        }
        if let Some(ed) = l.editor {
            if rect_contains(ed, col, row) {
                self.focus = Focus::Editor;
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
            Overlay::Help => {}
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
            self.expand_or_open_selected();
        }
    }

    fn mouse_wheel(&mut self, col: u16, row: u16, dir: isize) -> Action {
        let Some(l) = self.layout else { return Action::None };
        if let Some(side) = l.sidebar {
            if rect_contains(side, col, row) {
                self.selected = clamp_index(self.selected, dir, self.entries.len());
                return Action::None;
            }
        }
        for pane in [l.editor, l.diff].into_iter().flatten() {
            if rect_contains(pane, col, row) {
                if let Some(v) = &mut self.viewer {
                    v.scroll(dir * 3);
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

fn tree_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
    app.entries
        .iter()
        .enumerate()
        .skip(app.tree_offset)
        .take(height)
        .map(|(i, e)| {
            let icon = if e.is_dir { if app.expanded.contains(&e.path) { "▾ " } else { "▸ " } } else { "  " };
            let mark = e.status.map_or(' ', |s| s.marker());
            let left = format!("{mark} {}{icon}{}", "  ".repeat(e.depth), e.name);
            let right = if e.is_dir { String::new() } else { tree::human_size(e.size) };
            let pad = width.saturating_sub(left.chars().count() + right.len()).max(1);
            let mut style = status_style(e.status);
            if e.is_dir {
                style = style.add_modifier(Modifier::BOLD);
            }
            if i == app.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::styled(format!("{left}{}{right}", " ".repeat(pad)), style)
        })
        .collect()
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

    if let Some(side) = l.sidebar {
        let block = pane_block(format!(" Files · {} ", app.root.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())), focus == Focus::Sidebar);
        let inner = block.inner(side);
        let list_h = inner.height.saturating_sub(1) as usize;
        app.tree_offset = keep_visible(app.selected, app.tree_offset, list_h);
        let mut lines = tree_lines(app, list_h, inner.width as usize);
        lines.resize(list_h, Line::raw(""));
        if let Some(e) = app.entries.get(app.selected) {
            lines.push(Line::styled(entry_details(e), Style::default().fg(Color::DarkGray)));
        }
        frame.render_widget(Paragraph::new(lines).block(block), side);
    }

    if let (Some(ed), Some(df)) = (l.editor, l.diff) {
        match &app.viewer {
            Some(v) => {
                let title = format!(" {} ", v.path.display());
                let block = pane_block(title, focus == Focus::Editor);
                let num_w = v.content.len().to_string().len();
                let lines: Vec<Line> = v
                    .content
                    .iter()
                    .enumerate()
                    .skip(v.content_scroll)
                    .take(ed.height as usize)
                    .map(|(i, t)| Line::from(vec![Span::styled(format!("{:>num_w$} ", i + 1), Style::default().fg(Color::DarkGray)), Span::raw(t.clone())]))
                    .collect();
                frame.render_widget(Paragraph::new(lines).block(block), ed);
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
                let hint = Paragraph::new("Select a file in the sidebar (Enter or click).").style(Style::default().fg(Color::DarkGray));
                frame.render_widget(hint.block(pane_block(" Editor ".into(), focus == Focus::Editor)), ed);
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
            Overlay::Help => (" Help · Esc close ", help_lines()),
        };
        frame.render_widget(Paragraph::new(lines).block(block.title(title)), area);
    }

    let status = if app.prefix {
        Line::styled(format!(" {PREFIX_LABEL} … w workspace · t files · c changes · Tab/1-4 focus · ? help · q quit · ] send {PREFIX_LABEL}"), Style::default().fg(Color::Black).bg(Color::Cyan))
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
        "  Tab    next pane   1-4 sidebar / editor / changes / chat".into(),
        "  q      quit (stops gentle-shell)".into(),
        format!("  ]      send a literal {PREFIX_LABEL} to the chat"),
        String::new(),
        "Sidebar: ↑↓ move · Enter/→ open · ← collapse · Esc back to chat".into(),
        "Editor / changes: ↑↓ PgUp PgDn scroll · Esc back to chat".into(),
        "Mouse: click selects panes, files and tools; wheel scrolls.".into(),
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
}
