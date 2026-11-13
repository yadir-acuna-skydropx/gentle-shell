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
use gentle_engines::arch_protocol::{ArchClientMsg as ArchReq, ArchEngineMsg as ArchMsg, KeyState as ArchKey};
use gentle_engines::protocol::{ClientMsg as ExplainReq, EngineMsg as ExplainMsg, KeyState as ExplainKey};

use crate::apps_panel::{AppsPanel, View};
use crate::editor::{Editor, SaveOutcome};
use crate::highlight::{self, Highlighter};
use crate::icons::{self, IconMode};
use crate::layout::{self, Layout, Mode, Pane};
use crate::pane::{self, PtyPane};
use crate::predict::{Engine, PredictMode};
use crate::tree::{self, Entry, GitStatus};
use crate::viewer::{self, DiffKind, Viewer};

/// Prefix chord for partitioner commands; everything else goes to the focused pane.
pub const PREFIX_LABEL: &str = "Ctrl+]";

/// Lateral tool bar entries in chat mode (S7): label and prefix key.
pub const TOOLS: [(&str, char); 7] = [
    ("Files", 't'),
    ("Changes", 'c'),
    ("Workspace", 'w'),
    ("Apps", 'a'),
    ("Arch", 'A'),
    ("Help", '?'),
    ("Quit", 'q'),
];

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
    /// The code-explainer popup (S17): streaming explanation + follow-up Q&A.
    Explain,
    /// The architecture engine overlay (S18): review the repo or design from a
    /// brief, streaming a markdown report.
    Arch,
}

/// The three stages of the architecture overlay (S18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArchStage {
    /// Choosing "Review this repo" or "Design from a brief".
    #[default]
    Menu,
    /// Typing a multi-line brief (Ctrl+Enter / Alt+Enter submits).
    Brief,
    /// Streaming / showing the report.
    Report,
}

/// State of the architecture overlay (S18). The API key never reaches here.
#[derive(Debug, Clone, Default)]
pub struct ArchUi {
    pub stage: ArchStage,
    /// Selected menu option: 0 = review, 1 = design.
    pub option: usize,
    /// The multi-line brief being typed.
    pub brief: String,
    /// Streamed report text so far.
    pub body: String,
    /// Id of the in-flight request.
    pub id: u64,
    next_id: u64,
    pub error: Option<String>,
    pub thinking: bool,
    pub in_flight: bool,
    /// Lines scrolled up from the bottom of the body.
    pub scroll: usize,
    pub key_missing: bool,
    pub model: String,
    pub kb_chunks: usize,
    pub offline: bool,
    /// The written report path, set on `done` (enables `o` to open it).
    pub report_path: Option<String>,
}

/// State of the explainer popup (S17). Lives beside [`Overlay::Explain`]; the
/// API key never reaches this struct.
#[derive(Debug, Clone, Default)]
pub struct ExplainUi {
    /// Id of the in-flight explain/ask request.
    pub id: u64,
    /// Monotonic request-id source.
    next_id: u64,
    /// "Explain · <path> L<a>-<b>".
    pub title: String,
    /// Streamed explanation text so far.
    pub body: String,
    /// An inline error (engine offline, API failure, missing key).
    pub error: Option<String>,
    /// True while waiting for the first delta.
    pub thinking: bool,
    /// True while a request is streaming.
    pub in_flight: bool,
    /// The one-line follow-up question input.
    pub question: String,
    /// Lines scrolled up from the bottom of the body.
    pub scroll: usize,
    /// True when no API key is stored (show the setup hint, send nothing).
    pub key_missing: bool,
    /// Resolved model id, for the header.
    pub model: String,
    /// True when the engine process is not running.
    pub offline: bool,
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
    /// A request for the explainer engine (explain / ask / cancel).
    Explain(ExplainReq),
    /// A request for the architecture engine (review / design / cancel).
    Arch(ArchReq),
    /// Open a finished report file with the system opener (xdg-open).
    OpenReport(String),
    Quit,
}

/// Clickable controls on the apps-panel header row (its top border): the `[_]`
/// collapse button, the `[□]` expand button, and the "Apps" title text that
/// toggles between the two. Captured during draw (L48 minimize fix). All three
/// are independent of the sidebar minimize button.
#[derive(Debug, Clone, Copy)]
pub struct AppsHeaderButtons {
    pub row: u16,
    /// `[start, end)` columns of the `[_]` collapse button.
    pub collapse: (u16, u16),
    /// `[start, end)` columns of the `[□]` expand button.
    pub expand: (u16, u16),
    /// `[start, end)` columns of the clickable "Apps" title text.
    pub header: (u16, u16),
}

/// A clickable minimize / zoom button on a pane's top border (S14). Captured
/// during draw so a click runs the action even when the pane is not focused.
#[derive(Debug, Clone, Copy)]
pub struct TitleButton {
    pub pane: Pane,
    pub row: u16,
    /// `[start, end)` columns of the minimize button.
    pub min_range: (u16, u16),
    /// `[start, end)` columns of the zoom button.
    pub zoom_range: (u16, u16),
}

/// The code region to explain plus its surrounding context (S17).
struct ExplainTarget {
    path: String,
    language: String,
    start_line: usize,
    end_line: usize,
    selection: String,
    before: String,
    after: String,
}

/// Maps a file path to a coarse language name for the explain prompt.
fn language_for(path: &Path) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "jsx" => "javascript",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" => "markdown",
        "sql" => "sql",
        other => other,
    };
    lang.to_string()
}

/// Id of the local "Arquitectura" row in the apps list (S18, L48). It is not a
/// gentle-apps connector: the partitioner renders it from the gentle-arch
/// status and opens the Arch overlay on it.
const ARCH_ROW_ID: &str = "__arch__";

/// Maps a focus target to the layout pane it belongs to (the apps panel lives
/// inside the sidebar unit).
fn focus_pane(focus: Focus) -> Pane {
    match focus {
        Focus::Sidebar | Focus::Apps => Pane::Sidebar,
        Focus::Editor => Pane::Editor,
        Focus::Diff => Pane::Diff,
        Focus::Chat => Pane::Chat,
    }
}

/// Default focus for a pane.
fn pane_focus(pane: Pane) -> Focus {
    match pane {
        Pane::Sidebar => Focus::Sidebar,
        Pane::Editor => Focus::Editor,
        Pane::Diff => Focus::Diff,
        Pane::Chat => Focus::Chat,
    }
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
    /// Expanded apps-panel height as a share of the sidebar, in percent; the
    /// user adjusts it with `+`/`-` within [40, 70]; the default is 45% (S16, L48).
    pub apps_panel_pct: u16,
    /// Incrementing id for apps-engine requests.
    apps_req_id: u64,
    /// File-tree region of the sidebar, captured in the last draw.
    pub tree_area: Option<Rect>,
    /// Apps-panel region of the sidebar, captured in the last draw.
    pub apps_area: Option<Rect>,
    /// Apps-panel header controls captured in the last draw (L48 minimize fix).
    pub apps_header: Option<AppsHeaderButtons>,
    /// Screen rect of the mascot "ask about the selection" badge, captured in
    /// the last draw; a click on it opens the explain popup (S17, L48). `None`
    /// when there is no selection or it is scrolled fully out of view.
    pub selection_badge: Option<Rect>,
    /// Zoomed pane (S14); `None` for the normal split.
    pub zoom: Option<Pane>,
    /// Per-pane minimized flags (S14).
    pub min_sidebar: bool,
    pub min_editor: bool,
    pub min_diff: bool,
    pub min_chat: bool,
    /// Minimize/zoom buttons captured in the last draw (S14).
    pub title_buttons: Vec<TitleButton>,
    /// Minimized pane regions captured in the last draw; a click restores them.
    pub minimized_regions: Vec<(Pane, Rect)>,
    /// Code-explainer popup state (S17).
    pub explain: ExplainUi,
    /// Architecture overlay state (S18).
    pub arch: ArchUi,
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
            apps_panel_pct: layout::APPS_PANEL_DEFAULT_PCT,
            apps_req_id: 0,
            tree_area: None,
            apps_area: None,
            apps_header: None,
            selection_badge: None,
            zoom: None,
            min_sidebar: false,
            min_editor: false,
            min_diff: false,
            min_chat: false,
            title_buttons: Vec::new(),
            minimized_regions: Vec::new(),
            explain: ExplainUi::default(),
            arch: ArchUi::default(),
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

    /// Zoom / minimize state to feed the layout engine (S14).
    pub fn panes(&self) -> layout::Panes {
        layout::Panes {
            zoom: self.zoom,
            min_sidebar: self.min_sidebar,
            min_editor: self.min_editor,
            min_diff: self.min_diff,
            min_chat: self.min_chat,
        }
    }

    /// Layout actually rendered for `area`.
    pub fn compute_layout(&self, area: Rect) -> Layout {
        layout::compute_panes(area, self.mode, self.panes())
    }

    /// Inner area the chat PTY should be resized to, or `None` when the chat
    /// must keep its last real size: minimized (collapsed to a title bar) or
    /// hidden because another pane is zoomed (S14, S4). When the chat is zoomed
    /// this returns the full body inner area.
    pub fn chat_inner(&self, area: Rect) -> Option<Rect> {
        let l = self.compute_layout(area);
        if !l.show_chat || l.is_minimized(Pane::Chat) {
            return None;
        }
        Some(match l.mode {
            Mode::Workspace => Block::default().borders(Borders::ALL).inner(l.chat),
            Mode::Chat => l.chat,
        })
    }

    /// Focus actually in effect: panes hidden by the layout fall back to the
    /// chat, and while a pane is zoomed the focus follows the zoomed pane.
    pub fn effective_focus(&self) -> Focus {
        match self.layout.map(|l| l.mode) {
            Some(Mode::Chat) => Focus::Chat,
            _ if self.mode == Mode::Chat => Focus::Chat,
            _ => match self.zoom {
                Some(z) if focus_pane(self.focus) != z => pane_focus(z),
                _ => self.focus,
            },
        }
    }

    fn is_min(&self, pane: Pane) -> bool {
        match pane {
            Pane::Sidebar => self.min_sidebar,
            Pane::Editor => self.min_editor,
            Pane::Diff => self.min_diff,
            Pane::Chat => self.min_chat,
        }
    }

    fn set_min(&mut self, pane: Pane, value: bool) {
        match pane {
            Pane::Sidebar => self.min_sidebar = value,
            Pane::Editor => self.min_editor = value,
            Pane::Diff => self.min_diff = value,
            Pane::Chat => self.min_chat = value,
        }
    }

    /// Number of workspace panes that are not minimized.
    fn unminimized_count(&self) -> usize {
        Pane::ALL.iter().filter(|p| !self.is_min(**p)).count()
    }

    /// Next non-minimized pane after `from`, as a focus target.
    fn next_unminimized_focus(&self, from: Pane) -> Focus {
        let idx = Pane::ALL.iter().position(|p| *p == from).unwrap_or(0);
        for off in 1..=4 {
            let p = Pane::ALL[(idx + off) % 4];
            if !self.is_min(p) {
                return pane_focus(p);
            }
        }
        Focus::Chat
    }

    /// Reports the "available in workspace mode" status when not in workspace
    /// (S14 item 7). Returns true when the command should be refused.
    fn not_workspace(&mut self) -> bool {
        if self.mode != Mode::Workspace {
            self.status_msg = Some("available in workspace mode".into());
            true
        } else {
            false
        }
    }

    /// prefix `z`: toggle zoom on the focused pane (S14).
    pub fn zoom_focused(&mut self) {
        if self.not_workspace() {
            return;
        }
        self.toggle_zoom(focus_pane(self.effective_focus()));
    }

    /// Toggles zoom on a specific pane and focuses it when zooming in.
    pub fn toggle_zoom(&mut self, pane: Pane) {
        if self.zoom == Some(pane) {
            self.zoom = None;
        } else {
            self.zoom = Some(pane);
            self.focus = pane_focus(pane);
        }
    }

    /// prefix `m`: minimize the focused pane (S14).
    pub fn minimize_focused(&mut self) {
        if self.not_workspace() {
            return;
        }
        self.toggle_minimize(focus_pane(self.effective_focus()));
    }

    /// Minimizes a pane, or restores it when already minimized. Minimizing the
    /// last visible pane is refused; focus moves off a pane we minimize.
    pub fn toggle_minimize(&mut self, pane: Pane) {
        if self.is_min(pane) {
            self.set_min(pane, false);
            return;
        }
        if self.unminimized_count() <= 1 {
            self.status_msg = Some("can\u{2019}t minimize the last visible pane".into());
            return;
        }
        self.set_min(pane, true);
        if focus_pane(self.focus) == pane {
            self.focus = self.next_unminimized_focus(pane);
        }
    }

    /// prefix `d`: the "changes only" view — focus the diff and zoom it (S14).
    pub fn changes_only(&mut self) {
        if self.not_workspace() {
            return;
        }
        if self.zoom == Some(Pane::Diff) {
            self.zoom = None;
        } else {
            self.focus = Focus::Diff;
            self.zoom = Some(Pane::Diff);
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

        // Esc restores a zoomed pane when not inside an editing context (the
        // editor and the chat keep Esc for themselves); the diff and sidebar
        // are read-only, so Esc backs out of "changes only" (S14).
        if key.code == KeyCode::Esc
            && self.zoom.is_some()
            && self.overlay.is_none()
            && matches!(self.effective_focus(), Focus::Sidebar | Focus::Diff)
        {
            self.zoom = None;
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

    /// Applies a decoded explainer-engine message to the popup (S17).
    pub fn apply_explain_msg(&mut self, msg: ExplainMsg) {
        match msg {
            ExplainMsg::Delta { id, text } => {
                if id == self.explain.id {
                    self.explain.body.push_str(&text);
                    self.explain.thinking = false;
                }
            }
            ExplainMsg::Done { id } => {
                if id == self.explain.id {
                    self.explain.in_flight = false;
                    self.explain.thinking = false;
                }
            }
            ExplainMsg::Error { id, message } => {
                if id == self.explain.id {
                    self.explain.error = Some(message);
                    self.explain.in_flight = false;
                    self.explain.thinking = false;
                }
            }
            ExplainMsg::Status { key, model } => {
                self.explain.key_missing = key == ExplainKey::Missing;
                self.explain.model = model;
                self.explain.offline = false;
            }
        }
    }

    /// Marks the explainer engine offline (missing or exited).
    pub fn set_explain_offline(&mut self) {
        self.explain.offline = true;
        if self.explain.in_flight {
            self.explain.in_flight = false;
            self.explain.thinking = false;
            self.explain.error = Some("explainer engine offline — retrying…".into());
        }
    }

    /// Builds the explain target from the editor selection, or the current line
    /// ±10 lines when there is no selection (S17).
    fn explain_target(&self) -> Option<ExplainTarget> {
        let e = self.editor.as_ref()?;
        let path = e.path.display().to_string();
        let language = language_for(&e.path);
        let total = e.lines.len();
        let (sl, el, selection, ctx) = match e.selection_range() {
            Some(((sl, _), (el, _))) => (sl, el, e.selected_text().unwrap_or_default(), 40usize),
            None => {
                let c = e.cursor_line;
                (c, c, e.lines.get(c).cloned().unwrap_or_default(), 10usize)
            }
        };
        let bstart = sl.saturating_sub(ctx);
        let before = e.lines[bstart..sl].join("\n");
        let aend = (el + 1 + ctx).min(total);
        let after = if el + 1 < total { e.lines[el + 1..aend].join("\n") } else { String::new() };
        Some(ExplainTarget { path, language, start_line: sl, end_line: el, selection, before, after })
    }

    /// Opens the explainer popup for the current selection / line (prefix `e`
    /// or a right-click in the editor). Sends nothing when no key is stored.
    fn open_explain(&mut self) -> Action {
        let Some(t) = self.explain_target() else {
            self.status_msg = Some("open a file in the editor to explain code".into());
            return Action::None;
        };
        self.explain.title = format!("Explain · {} L{}-{}", t.path, t.start_line + 1, t.end_line + 1);
        self.explain.body.clear();
        self.explain.error = None;
        self.explain.scroll = 0;
        self.explain.question.clear();
        self.overlay = Some(Overlay::Explain);
        if self.explain.key_missing {
            self.explain.in_flight = false;
            self.explain.thinking = false;
            return Action::None;
        }
        self.explain.next_id += 1;
        let id = self.explain.next_id;
        self.explain.id = id;
        self.explain.in_flight = true;
        self.explain.thinking = true;
        Action::Explain(ExplainReq::Explain {
            id,
            path: t.path,
            language: t.language,
            selection: t.selection,
            context_before: t.before,
            context_after: t.after,
        })
    }

    /// A right-click inside the editor opens the explainer popup (S17).
    fn mouse_right(&mut self, col: u16, row: u16) -> Action {
        if self.overlay.is_some() {
            return Action::None;
        }
        let Some(l) = self.layout else { return Action::None };
        if let Some(ed) = l.editor {
            if rect_contains(ed, col, row) {
                self.focus = Focus::Editor;
                return self.open_explain();
            }
        }
        Action::None
    }

    /// Number of selectable rows in the collapsed apps list: the engine apps
    /// plus the local "Arquitectura" row that always trails them (S18, L48).
    fn apps_list_count(&self) -> usize {
        self.apps.apps.len() + 1
    }

    /// Whether `idx` addresses the trailing "Arquitectura" row.
    fn is_arch_row(&self, idx: usize) -> bool {
        idx == self.apps.apps.len()
    }

    /// Content rows the collapsed apps panel must show: the selectable rows plus
    /// the non-selectable engine-state note (offline / connecting), so the note
    /// is never clipped out of a short collapsed panel (S18, L48).
    fn apps_display_rows(&self) -> usize {
        let note = usize::from(self.apps.offline || self.apps.apps.is_empty());
        self.apps_list_count() + note
    }

    fn next_apps_id(&mut self) -> u64 {
        self.apps_req_id += 1;
        self.apps_req_id
    }

    /// `+` on the focused apps panel: grow the expanded height one 5% step,
    /// clamped to 70% (S16). The new value is echoed in the status line.
    pub fn grow_apps_panel(&mut self) {
        self.apps_panel_pct = (self.apps_panel_pct + layout::APPS_PANEL_STEP).min(layout::APPS_PANEL_MAX_PCT);
        self.status_msg = Some(format!("apps panel {}%", self.apps_panel_pct));
    }

    /// `-` on the focused apps panel: shrink the expanded height one 5% step,
    /// clamped to 40% (S16). The new value is echoed in the status line.
    pub fn shrink_apps_panel(&mut self) {
        self.apps_panel_pct =
            self.apps_panel_pct.saturating_sub(layout::APPS_PANEL_STEP).max(layout::APPS_PANEL_MIN_PCT);
        self.status_msg = Some(format!("apps panel {}%", self.apps_panel_pct));
    }

    /// Enters the selected app: expand to its item list and request a refresh
    /// (shared by Enter on an app row and a click on an app row, S16).
    fn enter_selected_app(&mut self) -> Action {
        if let Some(id) = self.apps.selected_app_id() {
            self.apps.view = View::Items;
            self.apps.item_sel = 0;
            self.apps.reset_nav();
            let rid = self.next_apps_id();
            return Action::Apps(ClientMsg::Refresh { id: rid, app: id });
        }
        Action::None
    }

    /// Collapses the apps panel to its collapsed app list (the `[_]` header
    /// control, L48). Independent of the sidebar minimize state.
    fn collapse_apps_panel(&mut self) {
        self.focus = Focus::Apps;
        self.apps.view = View::Apps;
        self.apps.reset_nav();
    }

    /// Clicking the "Apps" header text toggles collapsed/expanded (L48). When
    /// collapsed it enters the selected app (returning its refresh request).
    fn toggle_apps_collapsed(&mut self) -> Action {
        self.focus = Focus::Apps;
        if self.apps.view == View::Apps {
            self.enter_selected_app()
        } else {
            self.apps.view = View::Apps;
            self.apps.reset_nav();
            Action::None
        }
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
        self.apps.reset_nav();
        self.focus = Focus::Chat;
    }

    /// Panel navigation: Up/Down select; Enter drills in (app → items →
    /// children, or the reply editor / action menu on a leaf); Esc steps back
    /// one level, then to the chat. A pending confirmation or an open action
    /// menu take priority over the level navigation.
    fn apps_key(&mut self, key: KeyEvent) -> Action {
        // `+`/`-` adjust the expanded panel height while the panel has focus,
        // except while typing a reply (where they are literal characters, S16).
        if self.apps.view != View::Reply {
            match key.code {
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    self.grow_apps_panel();
                    return Action::None;
                }
                KeyCode::Char('-') => {
                    self.shrink_apps_panel();
                    return Action::None;
                }
                _ => {}
            }
        }
        if self.apps.confirm.is_some() {
            return self.apps_confirm_key(key);
        }
        if self.apps.menu.is_some() {
            return self.apps_menu_key(key);
        }
        match self.apps.view {
            View::Reply => self.apps_reply_key(key),
            View::Items => self.apps_items_key(key),
            View::Apps => {
                // The list includes the trailing "Arquitectura" row (S18, L48).
                let len = self.apps_list_count();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.apps.app_sel = clamp_index(self.apps.app_sel, -1, len),
                    KeyCode::Down | KeyCode::Char('j') => self.apps.app_sel = clamp_index(self.apps.app_sel, 1, len),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if self.is_arch_row(self.apps.app_sel) {
                            return self.open_arch();
                        }
                        return self.enter_selected_app();
                    }
                    KeyCode::Esc => self.leave_apps(),
                    _ => {}
                }
                Action::None
            }
        }
    }

    /// Item-level navigation: expand an expandable item (with a breadcrumb),
    /// open the action menu on a leaf with actions, or open the reply editor on
    /// a flat item (Slack, unchanged). `a` opens the action menu; `r` refreshes
    /// the current level; Esc/Left steps up one level then back to the app list.
    fn apps_items_key(&mut self, key: KeyEvent) -> Action {
        let app = self.apps.selected_app_id().unwrap_or_default();
        let len = self.apps.visible_items().len();
        let sel = self.apps.visible_sel();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.apps.set_visible_sel(clamp_index(sel, -1, len)),
            KeyCode::Down | KeyCode::Char('j') => self.apps.set_visible_sel(clamp_index(sel, 1, len)),
            KeyCode::Char('a') => {
                if let Some(it) = self.apps.selected_visible_item() {
                    self.apps.open_menu(&it);
                }
            }
            KeyCode::Char('r') => {
                let rid = self.next_apps_id();
                if let Some(frame) = self.apps.nav.last() {
                    let parent = frame.parent_id.clone();
                    self.apps.reload_current_level(rid);
                    return Action::Apps(ClientMsg::Expand { id: rid, app, item: parent });
                } else {
                    return Action::Apps(ClientMsg::Refresh { id: rid, app });
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(it) = self.apps.selected_visible_item() {
                    if it.expandable {
                        let rid = self.next_apps_id();
                        self.apps.push_nav(&it, rid);
                        return Action::Apps(ClientMsg::Expand { id: rid, app, item: it.id });
                    } else if it.kind.is_none() && self.apps.nav.is_empty() {
                        // A flat item (Slack): Enter opens the reply editor as
                        // before; its actions (e.g. open) are reached with `a`.
                        self.apps.view = View::Reply;
                        self.apps.reply.clear();
                        self.apps.reply_status = None;
                    } else if !it.actions.is_empty() {
                        // A hierarchical leaf (GitHub PR/issue/file): action menu.
                        self.apps.open_menu(&it);
                    }
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                if !self.apps.pop_nav() {
                    self.apps.view = View::Apps;
                    self.apps.reset_nav();
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Action-menu navigation: Up/Down select; Enter runs the action (asking for
    /// confirmation first when the action requires it); Esc closes the menu.
    fn apps_menu_key(&mut self, key: KeyEvent) -> Action {
        let (len, sel) = match &self.apps.menu {
            Some(m) => (m.actions.len(), m.sel),
            None => return Action::None,
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(m) = self.apps.menu.as_mut() {
                    m.sel = clamp_index(sel, -1, len);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(m) = self.apps.menu.as_mut() {
                    m.sel = clamp_index(sel, 1, len);
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.apps.menu = None,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let app = self.apps.selected_app_id().unwrap_or_default();
                if let Some(m) = self.apps.menu.take() {
                    if let Some(action) = m.actions.get(m.sel).cloned() {
                        if action.confirm {
                            self.apps.confirm = Some(crate::apps_panel::Confirm {
                                item_id: m.item_id,
                                action_id: action.id,
                                label: action.label,
                            });
                        } else {
                            let rid = self.next_apps_id();
                            self.apps.pending_action = Some(rid);
                            self.apps.action_status = Some("running\u{2026}".into());
                            return Action::Apps(ClientMsg::Action {
                                id: rid,
                                app,
                                item: m.item_id,
                                action: action.id,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Confirmation prompt: `y` runs the action, `n`/Esc cancels.
    fn apps_confirm_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let app = self.apps.selected_app_id().unwrap_or_default();
                if let Some(c) = self.apps.confirm.take() {
                    let rid = self.next_apps_id();
                    self.apps.pending_action = Some(rid);
                    self.apps.action_status = Some("running\u{2026}".into());
                    return Action::Apps(ClientMsg::Action { id: rid, app, item: c.item_id, action: c.action_id });
                }
                Action::None
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.apps.confirm = None;
                self.apps.action_status = Some("cancelled".into());
                Action::None
            }
            _ => Action::None,
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
            KeyCode::Char('z') => {
                self.zoom_focused();
                Action::None
            }
            KeyCode::Char('m') => {
                self.minimize_focused();
                Action::None
            }
            KeyCode::Char('d') => {
                self.changes_only();
                Action::None
            }
            KeyCode::Char('?') => {
                self.toggle_overlay(Overlay::Help);
                Action::None
            }
            KeyCode::Char('e') => self.open_explain(),
            KeyCode::Char('A') => self.open_arch(),
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
        // Tab skips minimized panes (they are title bars); prefix 1-4 can still
        // focus them explicitly to restore with prefix m.
        let order: Vec<Focus> = self.visible_focuses().into_iter().filter(|f| !self.is_min(focus_pane(*f))).collect();
        if order.is_empty() {
            return;
        }
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
        // The explainer popup has its own reducer (streaming text + question input).
        if overlay == Overlay::Explain {
            return self.explain_key(key);
        }
        // The architecture overlay has its own reducer (menu / brief / report).
        if overlay == Overlay::Arch {
            return self.arch_key(key);
        }
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => self.overlay_move(overlay, -1),
            KeyCode::Down | KeyCode::Char('j') => self.overlay_move(overlay, 1),
            KeyCode::Enter => match overlay {
                Overlay::Files => self.toggle_or_open_selected(),
                Overlay::Changes => self.open_selected_change(),
                Overlay::Help | Overlay::Apps | Overlay::Explain | Overlay::Arch => {}
            },
            _ => {}
        }
        Action::None
    }

    fn overlay_move(&mut self, overlay: Overlay, delta: isize) {
        match overlay {
            Overlay::Files => self.selected = clamp_index(self.selected, delta, self.entries.len()),
            Overlay::Changes => self.change_selected = clamp_index(self.change_selected, delta, self.changes().len()),
            Overlay::Help | Overlay::Apps | Overlay::Explain | Overlay::Arch => {}
        }
    }

    /// Explainer popup reducer (S17): Enter sends a follow-up `ask`, Esc closes
    /// (cancelling any in-flight request), typing edits the question line and the
    /// arrows/PageUp/PageDown scroll the streamed text.
    fn explain_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                if self.explain.in_flight {
                    self.explain.in_flight = false;
                    self.explain.thinking = false;
                    return Action::Explain(ExplainReq::Cancel { id: self.explain.id });
                }
                Action::None
            }
            KeyCode::Enter => {
                let q = self.explain.question.trim().to_string();
                if q.is_empty() || self.explain.key_missing {
                    return Action::None;
                }
                self.explain.next_id += 1;
                let id = self.explain.next_id;
                self.explain.id = id;
                self.explain.in_flight = true;
                self.explain.thinking = true;
                self.explain.error = None;
                self.explain.scroll = 0;
                self.explain.body.push_str(&format!("\n\n── you: {q}\n\n"));
                self.explain.question.clear();
                Action::Explain(ExplainReq::Ask { id, question: q })
            }
            KeyCode::Backspace => {
                self.explain.question.pop();
                Action::None
            }
            KeyCode::Up => {
                self.explain.scroll = self.explain.scroll.saturating_add(1);
                Action::None
            }
            KeyCode::Down => {
                self.explain.scroll = self.explain.scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.explain.scroll = self.explain.scroll.saturating_add(10);
                Action::None
            }
            KeyCode::PageDown => {
                self.explain.scroll = self.explain.scroll.saturating_sub(10);
                Action::None
            }
            KeyCode::Char(c) if !ctrl => {
                self.explain.question.push(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Opens the architecture overlay (prefix `A` / the "Arch" tool), starting
    /// on the two-option menu. The engine's start `status` populates the key /
    /// model / KB-chunk fields.
    fn open_arch(&mut self) -> Action {
        self.overlay = Some(Overlay::Arch);
        self.arch.stage = ArchStage::Menu;
        self.arch.option = 0;
        self.arch.error = None;
        self.arch.scroll = 0;
        Action::None
    }

    /// Architecture overlay reducer (S18): the menu selects review/design, the
    /// brief stage edits a multi-line brief (Ctrl+Enter / Alt+Enter submits,
    /// Esc returns to the menu), and the report stage scrolls the stream and
    /// opens the finished report with `o`.
    fn arch_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match self.arch.stage {
            ArchStage::Menu => match key.code {
                KeyCode::Esc => {
                    self.overlay = None;
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.arch.option = self.arch.option.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.arch.option = (self.arch.option + 1).min(1);
                    Action::None
                }
                KeyCode::Enter => {
                    if self.arch.option == 0 {
                        self.start_arch_review()
                    } else {
                        self.arch.stage = ArchStage::Brief;
                        Action::None
                    }
                }
                _ => Action::None,
            },
            ArchStage::Brief => match key.code {
                KeyCode::Esc => {
                    self.arch.stage = ArchStage::Menu;
                    Action::None
                }
                // Ctrl+Enter or Alt+Enter submits the brief.
                KeyCode::Enter if ctrl || alt => self.start_arch_design(),
                KeyCode::Enter => {
                    self.arch.brief.push('\n');
                    Action::None
                }
                KeyCode::Backspace => {
                    self.arch.brief.pop();
                    Action::None
                }
                KeyCode::Char(c) if !ctrl => {
                    self.arch.brief.push(c);
                    Action::None
                }
                _ => Action::None,
            },
            ArchStage::Report => match key.code {
                KeyCode::Esc => {
                    self.overlay = None;
                    if self.arch.in_flight {
                        self.arch.in_flight = false;
                        self.arch.thinking = false;
                        return Action::Arch(ArchReq::Cancel { id: self.arch.id });
                    }
                    Action::None
                }
                KeyCode::Char('o') => {
                    if let Some(path) = self.arch.report_path.clone() {
                        return Action::OpenReport(path);
                    }
                    Action::None
                }
                KeyCode::Up => {
                    self.arch.scroll = self.arch.scroll.saturating_add(1);
                    Action::None
                }
                KeyCode::Down => {
                    self.arch.scroll = self.arch.scroll.saturating_sub(1);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.arch.scroll = self.arch.scroll.saturating_add(10);
                    Action::None
                }
                KeyCode::PageDown => {
                    self.arch.scroll = self.arch.scroll.saturating_sub(10);
                    Action::None
                }
                _ => Action::None,
            },
        }
    }

    fn start_arch_review(&mut self) -> Action {
        if self.arch.key_missing {
            self.arch.stage = ArchStage::Report;
            return Action::None;
        }
        self.arch.stage = ArchStage::Report;
        self.arch.body.clear();
        self.arch.error = None;
        self.arch.report_path = None;
        self.arch.scroll = 0;
        self.arch.next_id += 1;
        let id = self.arch.next_id;
        self.arch.id = id;
        self.arch.in_flight = true;
        self.arch.thinking = true;
        Action::Arch(ArchReq::Review { id, path: self.root.display().to_string(), deep: false })
    }

    fn start_arch_design(&mut self) -> Action {
        let brief = self.arch.brief.trim().to_string();
        if brief.is_empty() {
            return Action::None;
        }
        if self.arch.key_missing {
            self.arch.stage = ArchStage::Report;
            return Action::None;
        }
        self.arch.stage = ArchStage::Report;
        self.arch.body.clear();
        self.arch.error = None;
        self.arch.report_path = None;
        self.arch.scroll = 0;
        self.arch.next_id += 1;
        let id = self.arch.next_id;
        self.arch.id = id;
        self.arch.in_flight = true;
        self.arch.thinking = true;
        Action::Arch(ArchReq::Design { id, brief, deep: false })
    }

    /// Applies a decoded architecture-engine message to the overlay (S18).
    pub fn apply_arch_msg(&mut self, msg: ArchMsg) {
        match msg {
            ArchMsg::Delta { id, text } => {
                if id == self.arch.id {
                    self.arch.body.push_str(&text);
                    self.arch.thinking = false;
                }
            }
            ArchMsg::Done { id, path } => {
                if id == self.arch.id {
                    self.arch.in_flight = false;
                    self.arch.thinking = false;
                    self.arch.report_path = Some(path.clone());
                    self.status_msg = Some(format!("report written to {path} — press o to open"));
                }
            }
            ArchMsg::Error { id, message } => {
                if id == self.arch.id {
                    self.arch.error = Some(message);
                    self.arch.in_flight = false;
                    self.arch.thinking = false;
                }
            }
            ArchMsg::Status { key, model, kb_chunks } => {
                self.arch.key_missing = key == ArchKey::Missing;
                self.arch.model = model;
                self.arch.kb_chunks = kb_chunks;
                self.arch.offline = false;
            }
        }
    }

    /// Marks the architecture engine offline (missing or exited).
    pub fn set_arch_offline(&mut self) {
        self.arch.offline = true;
        if self.arch.in_flight {
            self.arch.in_flight = false;
            self.arch.thinking = false;
            self.arch.error = Some("architecture engine offline — retrying…".into());
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
                // Esc first clears an active selection; a second Esc leaves the pane.
                if self.editor.as_ref().is_some_and(|e| e.has_selection()) {
                    if let Some(e) = &mut self.editor {
                        e.clear_selection();
                    }
                    return Action::None;
                }
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
            // Ctrl+E asks about the current selection (S17, L48). Ctrl+E is not
            // otherwise bound in the editor; it is a no-op without a selection.
            KeyCode::Char('e') | KeyCode::Char('E') if ctrl => {
                if self.editor.as_ref().is_some_and(|e| e.has_selection()) {
                    return self.open_explain();
                }
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
            MouseEventKind::Down(MouseButton::Right) => self.mouse_right(ev.column, ev.row),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(ev.column, ev.row),
            MouseEventKind::ScrollUp => self.mouse_wheel(ev.column, ev.row, -1),
            MouseEventKind::ScrollDown => self.mouse_wheel(ev.column, ev.row, 1),
            _ => Action::None,
        }
    }

    /// A left-drag inside the editor extends the text selection from the drag
    /// anchor (S17 editor).
    fn mouse_drag(&mut self, col: u16, row: u16) -> Action {
        if self.overlay.is_some() {
            return Action::None;
        }
        let Some(l) = self.layout else { return Action::None };
        if let Some(ed) = l.editor {
            if rect_contains(ed, col, row) {
                self.focus = Focus::Editor;
                if let Some(e) = &mut self.editor {
                    let gutter = e.gutter_width();
                    e.drag(ed.x + 1, ed.y + 1, gutter, col, row);
                }
                return Action::None;
            }
        }
        Action::None
    }

    fn mouse_click(&mut self, col: u16, row: u16) -> Action {
        if let Some(overlay) = self.overlay {
            match self.overlay_area {
                Some(area) if rect_contains(area, col, row) => return self.click_overlay(overlay, area, row),
                // A click anywhere outside the open overlay dismisses it.
                _ => self.overlay = None,
            }
            return Action::None;
        }
        // Title-bar minimize / zoom buttons act on their pane even when it is
        // not focused (S14 item 3).
        for b in self.title_buttons.clone() {
            if row == b.row {
                if col >= b.min_range.0 && col < b.min_range.1 {
                    self.toggle_minimize(b.pane);
                    return Action::None;
                }
                if col >= b.zoom_range.0 && col < b.zoom_range.1 {
                    self.toggle_zoom(b.pane);
                    return Action::None;
                }
            }
        }
        // A click on a minimized pane's title bar or strip restores it (S14).
        for (pane, rect) in self.minimized_regions.clone() {
            if rect_contains(rect, col, row) {
                self.set_min(pane, false);
                self.focus = pane_focus(pane);
                return Action::None;
            }
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
                // Header-row controls: [_] collapse, [□] expand, title toggles
                // (L48 minimize fix). The sidebar [_] is a separate button.
                if let Some(h) = self.apps_header {
                    if row == h.row {
                        if in_col_range(col, h.collapse) {
                            self.collapse_apps_panel();
                            return Action::None;
                        }
                        if in_col_range(col, h.expand) {
                            self.focus = Focus::Apps;
                            return self.enter_selected_app();
                        }
                        if in_col_range(col, h.header) {
                            return self.toggle_apps_collapsed();
                        }
                    }
                }
                return self.click_apps(ap, col, row);
            }
        }
        // A click on the mascot badge asks about the current selection (S17,
        // L48). Checked before the editor click so the selection is preserved.
        if let Some(badge) = self.selection_badge {
            if rect_contains(badge, col, row) {
                self.focus = Focus::Editor;
                return self.open_explain();
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

    fn click_overlay(&mut self, overlay: Overlay, area: Rect, row: u16) -> Action {
        // Skip the top/bottom borders of the overlay box.
        if row <= area.y || row + 1 >= area.y + area.height {
            return Action::None;
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
            Overlay::Apps => return self.click_apps(area, (area.x + 1).max(0), row),
            Overlay::Help | Overlay::Explain | Overlay::Arch => {}
        }
        Action::None
    }

    /// A click inside the apps panel selects the app or item under the cursor
    /// and focuses the panel. The panel body starts one row below the border.
    /// A click on an app row also expands the panel and enters that app (S16),
    /// returning the resulting refresh request.
    fn click_apps(&mut self, area: Rect, _col: u16, row: u16) -> Action {
        self.focus = Focus::Apps;
        if row <= area.y || row + 1 >= area.y + area.height {
            return Action::None;
        }
        let rel = (row - area.y - 1) as usize;
        match self.apps.view {
            View::Apps => {
                if rel < self.apps_list_count() {
                    self.apps.app_sel = rel;
                    // The trailing "Arquitectura" row opens the Arch overlay (S18, L48).
                    if self.is_arch_row(rel) {
                        return self.open_arch();
                    }
                    return self.enter_selected_app();
                }
            }
            View::Items | View::Reply => {
                // Row 0 is the app/breadcrumb header; items start at row 1.
                if rel >= 1 {
                    let idx = rel - 1;
                    if idx < self.apps.visible_items().len() {
                        self.apps.set_visible_sel(idx);
                    }
                }
            }
        }
        Action::None
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
                    let len = self.apps.visible_items().len();
                    let sel = self.apps.visible_sel();
                    self.apps.set_visible_sel(clamp_index(sel, dir, len));
                } else {
                    self.apps.app_sel = clamp_index(self.apps.app_sel, dir, self.apps_list_count());
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

/// Whether `col` falls in the half-open `[start, end)` range.
fn in_col_range(col: u16, range: (u16, u16)) -> bool {
    col >= range.0 && col < range.1
}

/// The mascot badge drawn after a code selection (S17, L48): Gentle Shell's
/// flower ✿ with a question mark, or the ASCII fallback `(?)`. Both are three
/// columns wide so the placement math is identical.
fn mascot_badge(icons: IconMode) -> &'static str {
    if icons == IconMode::Ascii {
        "(?)"
    } else {
        "✿ ?"
    }
}

/// Status-line hint shown while a code selection exists (S17, L48).
fn mascot_hint(icons: IconMode) -> &'static str {
    if icons == IconMode::Ascii {
        "(?) click or Ctrl+E to ask about the selection"
    } else {
        "✿ click or Ctrl+E to ask about the selection"
    }
}

/// The apps-panel block title (carries the height share while expanded, S16).
fn apps_panel_title(expanded: bool, pct: u16) -> String {
    if expanded {
        format!(" Apps · {pct}% ")
    } else {
        " Apps ".to_string()
    }
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

/// Truncates `s` to `w` columns and right-pads it with spaces to exactly `w`.
fn fit_pad(s: &str, w: usize) -> String {
    let mut out: String = s.chars().take(w).collect();
    let len = out.chars().count();
    if len < w {
        out.push_str(&" ".repeat(w - len));
    }
    out
}

/// Labels for the title-bar minimize and zoom buttons (S14 item 3). The ASCII
/// fallback (GENTLE_PARTITIONER_ICONS=ascii) avoids the box-drawing glyph.
fn button_labels(icons: IconMode) -> (&'static str, &'static str) {
    if icons == IconMode::Ascii {
        ("[_]", "[^]")
    } else {
        ("[_]", "[\u{25a1}]")
    }
}

/// Draws the minimize/zoom buttons at the right end of a pane's top border and
/// returns their click target, or `None` when the pane is too narrow (the title
/// is kept readable; the buttons are dropped first, S14 item 3).
fn draw_title_buttons(frame: &mut Frame, pane: Pane, rect: Rect, icons: IconMode, focused: bool) -> Option<TitleButton> {
    let (min_s, zoom_s) = button_labels(icons);
    let min_w = min_s.chars().count() as u16;
    let total = min_w + zoom_s.chars().count() as u16;
    // Leave room for the title and the border corner before showing buttons.
    if rect.height == 0 || rect.width < total + 6 {
        return None;
    }
    let row = rect.y;
    let start = rect.x + rect.width - 1 - total;
    let style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
    let buf = frame.buffer_mut();
    buf.set_string(start, row, min_s, style);
    buf.set_string(start + min_w, row, zoom_s, style);
    Some(TitleButton { pane, row, min_range: (start, start + min_w), zoom_range: (start + min_w, start + total) })
}

/// Draws the apps-panel header controls on its top border and returns their
/// click ranges (L48 minimize fix): `[_]` collapses the panel to its collapsed
/// view and `[□]` expands it, both on the right; the "Apps" title on the left
/// is a clickable collapse/expand toggle. When the box is too narrow the two
/// buttons are dropped (the title stays clickable). `title_cols` is the width of
/// the already-rendered block title (" Apps " / " Apps · NN% ").
fn draw_apps_header_buttons(
    frame: &mut Frame,
    area: Rect,
    title_cols: u16,
    icons: IconMode,
    focused: bool,
) -> Option<AppsHeaderButtons> {
    if area.height == 0 || area.width < 2 {
        return None;
    }
    let row = area.y;
    let (collapse_s, expand_s) = button_labels(icons);
    let cw = collapse_s.chars().count() as u16;
    let ew = expand_s.chars().count() as u16;
    let total = cw + ew;
    // The title text is rendered by the block starting one cell after the corner.
    let header_start = area.x + 1;
    let mut header_end = header_start + title_cols;
    let style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
    // Buttons need room for the title plus both corners; drop them when tight.
    let (collapse, expand) = if area.width >= title_cols + total + 2 {
        let start = area.x + area.width - 1 - total;
        let buf = frame.buffer_mut();
        buf.set_string(start, row, collapse_s, style);
        buf.set_string(start + cw, row, expand_s, style);
        header_end = header_end.min(start);
        ((start, start + cw), (start + cw, start + total))
    } else {
        // No buttons: keep the header text within the box.
        header_end = header_end.min(area.x + area.width.saturating_sub(1));
        ((0, 0), (0, 0))
    };
    Some(AppsHeaderButtons { row, collapse, expand, header: (header_start, header_end) })
}

/// Draws a minimized horizontal pane (editor/diff/chat) as a one-row title bar
/// (S14 item 2). The whole bar is clickable to restore it.
fn draw_min_bar(frame: &mut Frame, title: &str, rect: Rect, focused: bool, icons: IconMode) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let marker = if icons == IconMode::Ascii { "_" } else { "\u{25ac}" };
    let style = if focused {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text = fit_pad(&format!(" {marker} {title} · click to restore "), rect.width as usize);
    frame.render_widget(Paragraph::new(Line::styled(text, style)), rect);
}

/// Draws the minimized sidebar as a 3-column vertical strip showing its icon
/// (S14 item 2). The whole strip is clickable to restore it.
fn draw_min_strip(frame: &mut Frame, rect: Rect, focused: bool, icons: IconMode) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let icon = if icons == IconMode::Ascii { "F" } else { "\u{f07b}" };
    let style = if focused {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let w = rect.width as usize;
    let lines: Vec<Line<'static>> = (0..rect.height)
        .map(|i| {
            let s = if i == 0 { format!(" {icon}") } else { String::new() };
            Line::styled(fit_pad(&s, w), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rect);
}

/// Builds the apps-panel content lines for a pane (or overlay) `w` columns wide.
fn apps_content_lines(app: &mut App, w: usize, _h: usize, focused: bool) -> Vec<Line<'static>> {
    let w = w.max(1);
    match app.apps.view {
        // The collapsed list always ends with the local "Arquitectura" row, so
        // it renders even while the apps engine is offline or connecting (S18).
        View::Apps => apps_list_lines(app, w, focused),
        View::Items | View::Reply => {
            if app.apps.offline {
                return vec![Line::styled("apps engine offline", Style::default().fg(Color::DarkGray))];
            }
            if app.apps.apps.is_empty() {
                return vec![Line::styled("connecting\u{2026}", Style::default().fg(Color::DarkGray))];
            }
            apps_items_lines(app, w, focused)
        }
    }
}

/// Colour state and optional detail for the local "Arquitectura" row, derived
/// from the gentle-arch status (S18, L48): green when the key is set and the KB
/// has chunks, yellow when the key is set but the KB is empty, gray without a
/// key, red when the architecture engine is offline.
fn arch_app_state(arch: &ArchUi) -> (AppState, Option<&'static str>) {
    if arch.offline {
        (AppState::Error, Some("architecture engine offline"))
    } else if arch.key_missing {
        (AppState::Disconnected, None)
    } else if arch.kb_chunks == 0 {
        (AppState::Connecting, Some("run `gentle-arch kb update`"))
    } else {
        (AppState::Connected, None)
    }
}

/// The local "Arquitectura" row (S18, L48), rendered by the partitioner from the
/// gentle-arch status rather than a gentle-apps connector. Its index in the list
/// is always `apps.len()` (it trails the engine apps).
fn arch_row_line(app: &App, w: usize, focused: bool) -> Line<'static> {
    let selected = focused && app.apps.app_sel == app.apps.apps.len();
    let (state, detail) = arch_app_state(&app.arch);
    let glyph = icons::app_icon(ARCH_ROW_ID, app.icons);
    let mut label_style = Style::default();
    if selected {
        label_style = label_style.add_modifier(Modifier::REVERSED);
    }
    let text = match detail {
        Some(d) => format!("{glyph} Arquitectura — {d}"),
        None => format!("{glyph} Arquitectura"),
    };
    let label = fit(&text, w.saturating_sub(2));
    Line::from(vec![Span::styled("\u{25cf} ", app_dot_style(state)), Span::styled(label, label_style)])
}

/// Collapsed view: one row per app with a status dot and item count, then the
/// local "Arquitectura" row (S18, L48). When the apps engine is offline or
/// still connecting, a trailing non-selectable note is appended after the
/// Arquitectura row so the row indices (0..=apps.len()) stay stable.
fn apps_list_lines(app: &App, w: usize, focused: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = app
        .apps
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
        .collect();
    // The local "Arquitectura" entry trails the engine apps (index apps.len()).
    lines.push(arch_row_line(app, w, focused));
    if app.apps.offline {
        lines.push(Line::styled("apps engine offline", Style::default().fg(Color::DarkGray)));
    } else if app.apps.apps.is_empty() {
        lines.push(Line::styled("connecting\u{2026}", Style::default().fg(Color::DarkGray)));
    }
    lines
}

/// Expanded view: the selected app's items. A flat app (Slack) keeps the
/// title + one-line preview; a hierarchical app (GitHub) shows a breadcrumb
/// header and kind icons, plus the action menu / confirmation / result line.
fn apps_items_lines(app: &mut App, w: usize, focused: bool) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let app_name = app.apps.apps.get(app.apps.app_sel).map(|a| a.name.clone()).unwrap_or_default();
    let app_id = app.apps.apps.get(app.apps.app_sel).map(|a| a.id.clone()).unwrap_or_default();
    let dot_style = app.apps.apps.get(app.apps.app_sel).map(|a| app_dot_style(a.status)).unwrap_or_default();
    let nav_depth = app.apps.nav.len();

    // Header: the app name at the base level, else the breadcrumb.
    let header = if nav_depth == 0 {
        format!("{} {}", icons::app_icon(&app_id, app.icons), app_name)
    } else {
        app.apps.breadcrumb(&app_name)
    };
    out.push(Line::from(vec![
        Span::styled("\u{25cf} ", dot_style),
        Span::styled(fit(&header, w.saturating_sub(2)), Style::default().add_modifier(Modifier::BOLD)),
    ]));

    // A loading or errored level shows a short notice instead of stale items.
    if let Some(frame) = app.apps.nav.last() {
        if frame.loading {
            out.push(Line::styled("  loading\u{2026}", Style::default().fg(Color::DarkGray)));
            return out;
        }
        if let Some(err) = &frame.error {
            out.push(Line::styled(format!("  {}", fit(err, w.saturating_sub(2))), Style::default().fg(Color::Red)));
            return out;
        }
    }

    let items = app.apps.visible_items().to_vec();
    let sel = app.apps.visible_sel();
    if items.is_empty() {
        out.push(Line::styled("  (empty)", Style::default().fg(Color::DarkGray)));
    }
    for (i, it) in items.iter().enumerate() {
        let mut title_style = Style::default();
        if focused && i == sel {
            title_style = title_style.add_modifier(Modifier::REVERSED);
        }
        if nav_depth == 0 && it.kind.is_none() {
            // Flat Slack item: title + one-line preview (unchanged).
            out.push(Line::styled(fit(&it.title, w), title_style));
            let preview: String = it.text.lines().next().unwrap_or("").to_string();
            out.push(Line::styled(
                format!("  {}", fit(&preview, w.saturating_sub(2))),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            let glyph = icons::kind_icon(it.kind.as_deref(), app.icons);
            let marker = if it.expandable { "\u{203a}" } else { " " };
            let label = fit(&format!("{marker}{glyph} {}", it.title), w);
            out.push(Line::styled(label, title_style));
        }
    }

    // The reply editor (Slack) at the bottom.
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

    // The action menu, confirmation prompt, and last action result.
    if let Some(menu) = &app.apps.menu {
        out.push(Line::styled("\u{2500}".repeat(w), Style::default().fg(Color::DarkGray)));
        out.push(Line::styled("Actions", Style::default().add_modifier(Modifier::BOLD)));
        for (i, a) in menu.actions.iter().enumerate() {
            let mut st = Style::default();
            if i == menu.sel {
                st = st.add_modifier(Modifier::REVERSED);
            }
            out.push(Line::styled(fit(&format!("  {}", a.label), w), st));
        }
    }
    if let Some(c) = &app.apps.confirm {
        out.push(Line::styled(
            fit(&format!("Run {}? y/n", c.label), w),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(status) = &app.apps.action_status {
        out.push(Line::styled(fit(status, w), Style::default().fg(Color::Cyan)));
    }
    out
}

/// Draws the bottom-left apps panel into `area` (S12).
fn draw_apps_panel(frame: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    if area.height == 0 {
        return;
    }
    // While expanded, the title carries the current height share so adjusting
    // it with `+`/`-` is visible (S16).
    let expanded = app.apps.is_expanded();
    let title = apps_panel_title(expanded, app.apps_panel_pct);
    let title_cols = title.chars().count() as u16;
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    let lines = apps_content_lines(app, inner.width as usize, inner.height as usize, focused);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    // Paint the header controls on the top border: [_] collapse and [□] expand
    // on the right, with the "Apps" title as a clickable toggle (L48 fix). The
    // buttons drop first when the panel is too narrow; the title stays clickable.
    app.apps_header = draw_apps_header_buttons(frame, area, title_cols, app.icons, focused);
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
    // Cleared each frame; set only when the editor draws its selection badge.
    app.selection_badge = None;

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
    app.apps_header = None;
    if let Some(side) = l.sidebar {
        if l.is_minimized(Pane::Sidebar) {
            // Collapsed to a vertical icon strip (S14); the tree/apps click
            // regions are left empty so only a restore click lands.
            draw_min_strip(frame, side, focus == Focus::Sidebar || focus == Focus::Apps, app.icons);
        } else {
        // The sidebar is split into the file tree (top) and the apps panel (bottom, S12).
        let (tree_area, apps_area) =
            layout::split_sidebar(side, app.apps_display_rows() as u16, app.apps.is_expanded(), app.apps_panel_pct);
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
    }

    if let Some(ed) = l.editor {
        if l.is_minimized(Pane::Editor) {
            draw_min_bar(frame, "Editor", ed, focus == Focus::Editor, app.icons);
        } else {
        // Left pane: the editable buffer. Take the highlighter out so it can be
        // borrowed mutably alongside the editor; it is restored below.
        let ghost = if focus == Focus::Editor { app.ghost.clone() } else { None };
        let icons = app.icons;
        // Screen rect of the mascot badge, set while drawing the selection.
        let mut editor_badge: Option<Rect> = None;
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
                // Overlay the selection highlight on top of the syntax colors by
                // tinting the background of the selected cells (S17 editor).
                if focus == Focus::Editor {
                    if let Some(((sl, sv), (el, ev))) = e.selection_visual() {
                        let buf = frame.buffer_mut();
                        for row in sl..=el {
                            if row < scroll_row || row >= last {
                                continue;
                            }
                            let line_vcols = expanded_lines[row].chars().count();
                            let (cstart, cend) = if sl == el {
                                (sv, ev)
                            } else if row == sl {
                                (sv, line_vcols + 1)
                            } else if row == el {
                                (0, ev)
                            } else {
                                (0, line_vcols + 1)
                            };
                            let y = inner.y + (row - scroll_row) as u16;
                            if y >= inner.y + inner.height {
                                break;
                            }
                            for vcol in cstart..cend {
                                if vcol < scroll_col {
                                    continue;
                                }
                                let x = inner.x + gutter as u16 + (vcol - scroll_col) as u16;
                                if x >= inner.x + inner.width {
                                    break;
                                }
                                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                                    cell.set_bg(Color::Rgb(55, 65, 95));
                                }
                            }
                        }
                        // Mascot badge: Gentle Shell's flower ✿ + "?" placed
                        // right after the selection end on its last line so the
                        // user can click it to ask about exactly that selection
                        // (S17, L48). It never overwrites selected text and is
                        // clamped inside the editor's inner area and width.
                        let badge = mascot_badge(icons);
                        let bw = badge.chars().count() as u16;
                        let badge_style = Style::default()
                            .fg(Color::Magenta)
                            .bg(Color::Rgb(40, 28, 52))
                            .add_modifier(Modifier::BOLD);
                        let right = inner.x + inner.width;
                        let bottom = inner.y + inner.height;
                        let mut placed: Option<Rect> = None;
                        // Primary placement: just after the selection end.
                        if el >= scroll_row && el < last && ev >= scroll_col {
                            let x = inner.x + gutter as u16 + (ev - scroll_col) as u16;
                            let y = inner.y + (el - scroll_row) as u16;
                            if x + bw <= right && y < bottom {
                                placed = Some(Rect::new(x, y, bw, 1));
                            }
                        }
                        // Fallback: the end of a visible selection line (or just
                        // above the first line), when the end is scrolled out or
                        // the badge would exceed the width.
                        if placed.is_none() && inner.width >= bw {
                            let line = if el >= scroll_row && el < last {
                                Some(el)
                            } else if sl >= scroll_row && sl < last {
                                Some(sl)
                            } else {
                                None
                            };
                            let y = match line {
                                Some(ln) => inner.y + (ln - scroll_row) as u16,
                                None if sl > scroll_row => inner.y + (sl - 1 - scroll_row) as u16,
                                None => inner.y,
                            };
                            if y < bottom {
                                placed = Some(Rect::new(right - bw, y, bw, 1));
                            }
                        }
                        if let Some(r) = placed {
                            frame.buffer_mut().set_string(r.x, r.y, badge, badge_style);
                            editor_badge = Some(r);
                        }
                    }
                }
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
        app.selection_badge = editor_badge;
        }
    }

    if let Some(df) = l.diff {
        if l.is_minimized(Pane::Diff) {
            draw_min_bar(frame, "Agent changes", df, focus == Focus::Diff, app.icons);
        } else {
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
    }

    // Chat: full pane (render the PTY), a minimized title bar (no PTY render,
    // keeps its last size per S4), or hidden while another pane is zoomed.
    let chat = match l.mode {
        Mode::Chat => Some(l.chat),
        Mode::Workspace => {
            if l.is_minimized(Pane::Chat) {
                draw_min_bar(frame, "Chat", l.chat, focus == Focus::Chat, app.icons);
                None
            } else if l.show_chat {
                let block = pane_block(" Chat · gentle-shell ".into(), focus == Focus::Chat);
                let inner = block.inner(l.chat);
                frame.render_widget(block, l.chat);
                Some(inner)
            } else {
                None
            }
        }
    };
    if let (Some(chat), Some(pty)) = (chat, pty) {
        pty.with_screen(|screen| {
            pane::render_screen(screen, chat, frame.buffer_mut());
            let (row, col) = screen.cursor_position();
            if focus == Focus::Chat && app.overlay.is_none() && screen.scrollback() == 0 && !screen.hide_cursor() {
                frame.set_cursor_position(Position::new(chat.x + col.min(chat.width.saturating_sub(1)), chat.y + row.min(chat.height.saturating_sub(1))));
            }
        });
    }

    // Capture the minimize/zoom buttons (normal panes) and the clickable
    // regions of minimized panes, and paint the buttons over the top borders
    // (S14 item 3). Workspace only; chat mode is unaffected (S7).
    app.title_buttons.clear();
    app.minimized_regions.clear();
    if l.mode == Mode::Workspace {
        let icons = app.icons;
        let mut buttons = Vec::new();
        let mut regions = Vec::new();
        let panes = [
            (Pane::Sidebar, l.sidebar, focus == Focus::Sidebar || focus == Focus::Apps),
            (Pane::Editor, l.editor, focus == Focus::Editor),
            (Pane::Diff, l.diff, focus == Focus::Diff),
            (Pane::Chat, if l.show_chat { Some(l.chat) } else { None }, focus == Focus::Chat),
        ];
        for (pane, rect, foc) in panes {
            let Some(rect) = rect else { continue };
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            if l.is_minimized(pane) {
                regions.push((pane, rect));
            } else if let Some(tb) = draw_title_buttons(frame, pane, rect, icons, foc) {
                buttons.push(tb);
            }
        }
        app.title_buttons = buttons;
        app.minimized_regions = regions;
    }

    app.overlay_area = None;
    if let Some(overlay) = app.overlay {
        let area = centered(l.chat, 80, 80);
        app.overlay_area = Some(area);
        frame.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan));
        if overlay == Overlay::Explain {
            let title = format!(" {} · Enter ask · Esc close ", app.explain.title);
            let w = area.width.saturating_sub(2) as usize;
            let h = area.height.saturating_sub(2) as usize;
            let lines = explain_lines(&app.explain, w, h);
            frame.render_widget(Paragraph::new(lines).block(block.title(title)), area);
        } else if overlay == Overlay::Arch {
            let title = " Arch · architecture engine · Esc close ".to_string();
            let w = area.width.saturating_sub(2) as usize;
            let h = area.height.saturating_sub(2) as usize;
            let lines = arch_lines(&app.arch, w, h);
            frame.render_widget(Paragraph::new(lines).block(block.title(title)), area);
        } else {
        let (title, lines) = match overlay {
            Overlay::Explain | Overlay::Arch => unreachable!("handled above"),
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
    }

    let status = if app.prefix {
        Line::styled(format!(" {PREFIX_LABEL} … w workspace · t files · c changes · a apps · z zoom · m min · d changes-only · Tab/1-4 focus · ? help · q quit · ] send {PREFIX_LABEL}"), Style::default().fg(Color::Black).bg(Color::Cyan))
    } else if let Some(msg) = &app.status_msg {
        Line::styled(format!(" {msg}"), Style::default().fg(Color::Black).bg(Color::Yellow))
    } else if focus == Focus::Editor && app.editor.as_ref().is_some_and(|e| e.has_selection()) {
        // While a selection exists, point to the mascot badge (S17, L48).
        Line::styled(format!(" {}", mascot_hint(app.icons)), Style::default().fg(Color::Magenta))
    } else {
        let mode = match l.mode {
            Mode::Chat => "chat",
            Mode::Workspace => "workspace",
        };
        // Report the zoomed pane and the minimized panes (S14 item 6).
        let mut extra = String::new();
        if let Some(z) = app.zoom {
            if l.mode == Mode::Workspace {
                extra.push_str(&format!(" · zoom: {}", z.name()));
            }
        }
        let mins: Vec<&str> =
            Pane::ALL.iter().filter(|p| l.mode == Mode::Workspace && app.is_min(**p)).map(|p| p.name()).collect();
        if !mins.is_empty() {
            extra.push_str(&format!(" · min: {}", mins.join(",")));
        }
        Line::styled(
            format!(" gentle-partitioner · {mode} · focus {:?}{extra} · {PREFIX_LABEL} then ? for help", focus)
                .to_lowercase(),
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
        "  z      zoom the focused pane (maximize / restore)".into(),
        "  m      minimize the focused pane (restore with m or a click)".into(),
        "  d      changes only (focus + zoom the agent-changes pane)".into(),
        "  Tab    next pane   1-4 sidebar / editor / changes / chat".into(),
        "  q      quit (stops gentle-shell)".into(),
        format!("  ]      send a literal {PREFIX_LABEL} to the chat"),
        String::new(),
        "Sidebar: ↑↓ move · Enter/→ open · ← collapse · Esc back to chat".into(),
        "Editor: type to edit · Enter/Backspace/Delete · Tab · arrows · Home/End".into(),
        "        PageUp/PageDown · Ctrl+Home/End · Ctrl+S save · Ctrl+R reload".into(),
        "        Shift+arrows select · drag selects · Ctrl+C copy · ● unsaved · Esc back".into(),
        format!("  e      explain the selection / current line (also right-click)"),
        "        with a selection: Ctrl+E or click the ✿ ? mascot badge to ask".into(),
        "Agent changes: ↑↓ PgUp PgDn scroll · Esc back / restore zoom".into(),
        "Apps: ↑↓ select · Enter app→items→reply · a actions (open) · Esc back".into(),
        "      +/- resize the expanded panel (40-70%) · click an app to open it".into(),
        "      header: [_] collapse · [□] expand · click “Apps” to toggle".into(),
        "      Arquitectura row opens the architecture engine (Arch overlay)".into(),
        "Panes: [_] minimize · [□] zoom on each title bar; click a minimized".into(),
        "       title bar or strip to restore it.".into(),
        "Mouse: click selects panes and places the cursor; wheel scrolls.".into(),
    ]
    .into_iter()
    .map(Line::raw)
    .collect()
}

/// Wraps `s` to width `w` on word boundaries, preserving blank lines.
fn wrap_text(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for para in s.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ') {
            if line.is_empty() {
                line = word.to_string();
            } else if line.chars().count() + 1 + word.chars().count() <= w {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// Renders the architecture overlay: the two-option menu, the brief editor, or
/// the streaming report, plus a status header (model / KB chunks / state).
fn arch_lines(a: &ArchUi, w: usize, h: usize) -> Vec<Line<'static>> {
    let status = if a.offline {
        "offline".to_string()
    } else if a.key_missing {
        "no API key".to_string()
    } else if !a.model.is_empty() {
        format!("{} · KB {} chunks", a.model, a.kb_chunks)
    } else {
        format!("KB {} chunks", a.kb_chunks)
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::styled(format!("engine: {status}"), Style::default().fg(Color::DarkGray)));

    match a.stage {
        ArchStage::Menu => {
            out.push(Line::raw(""));
            out.push(Line::raw("What would you like to do?"));
            out.push(Line::raw(""));
            for (i, label) in ["Review this repo", "Design from a brief"].iter().enumerate() {
                let marker = if i == a.option { "> " } else { "  " };
                let style = if i == a.option {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                out.push(Line::styled(format!("{marker}{label}"), style));
            }
            out.push(Line::raw(""));
            out.push(Line::styled("↑/↓ select · Enter choose · Esc close", Style::default().fg(Color::DarkGray)));
        }
        ArchStage::Brief => {
            out.push(Line::raw(""));
            out.push(Line::raw("Describe the system to design (Ctrl+Enter or Alt+Enter to submit):"));
            out.push(Line::styled("─".repeat(w), Style::default().fg(Color::DarkGray)));
            for line in wrap_text(&a.brief, w) {
                out.push(Line::raw(line));
            }
            out.push(Line::styled("▌", Style::default().fg(Color::Cyan)));
        }
        ArchStage::Report => {
            let body_rows = h.saturating_sub(2).max(1);
            let mut content = String::new();
            if a.key_missing {
                content.push_str("Set your Anthropic API key: `gentle-arch key set`");
            } else {
                content.push_str(&a.body);
                if a.thinking {
                    if !a.body.is_empty() {
                        content.push('\n');
                    }
                    content.push_str("working…");
                }
            }
            let mut wrapped = wrap_text(&content, w);
            if let Some(err) = &a.error {
                wrapped.push(format!("⚠ {err}"));
            }
            if let Some(path) = &a.report_path {
                wrapped.push(format!("✓ report: {path}  (press o to open)"));
            }
            let total = wrapped.len();
            let max_up = total.saturating_sub(body_rows);
            let up = a.scroll.min(max_up);
            let start = max_up - up;
            let end = (start + body_rows).min(total);
            for line in &wrapped[start..end] {
                out.push(Line::raw(line.clone()));
            }
        }
    }
    out
}

/// Builds the explainer popup body: a model header, the scrolled + word-wrapped
/// streaming text (or the key-missing hint / inline error), a separator and the
/// one-line question input (S17).
fn explain_lines(e: &ExplainUi, w: usize, h: usize) -> Vec<Line<'static>> {
    let status = if e.offline {
        "offline".to_string()
    } else if e.key_missing {
        "no API key".to_string()
    } else if !e.model.is_empty() {
        e.model.clone()
    } else {
        "…".to_string()
    };
    let body_rows = h.saturating_sub(3).max(1);
    let mut content = String::new();
    if e.key_missing {
        content.push_str("Set your Anthropic API key: `gentle-explain key set`");
    } else {
        content.push_str(&e.body);
        if e.thinking {
            if !e.body.is_empty() {
                content.push('\n');
            }
            content.push_str("thinking…");
        }
    }
    let mut wrapped = wrap_text(&content, w);
    if let Some(err) = &e.error {
        wrapped.push(format!("⚠ {err}"));
    }
    let total = wrapped.len();
    let max_up = total.saturating_sub(body_rows);
    let up = e.scroll.min(max_up);
    let start = max_up - up;
    let end = (start + body_rows).min(total);

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::styled(format!("model: {status}"), Style::default().fg(Color::DarkGray)));
    for line in &wrapped[start..end] {
        out.push(Line::raw(line.clone()));
    }
    for _ in (end - start)..body_rows {
        out.push(Line::raw(""));
    }
    out.push(Line::styled("─".repeat(w), Style::default().fg(Color::DarkGray)));
    let prompt = if e.key_missing { String::new() } else { format!("> {}", e.question) };
    out.push(Line::styled(prompt, Style::default().fg(Color::Cyan)));
    out
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

    // --- Arch overlay (S18) ---------------------------------------------
    use gentle_engines::arch_protocol::{ArchClientMsg as ArReq, ArchEngineMsg as ArMsg, KeyState as ArKey};

    fn arch_app(name: &str) -> (PathBuf, App) {
        let root = fixture(name);
        let mut app = App::new(root.clone());
        render(&mut app, 120, 40);
        // The engine reports a key + model + KB chunks on connect.
        app.apply_arch_msg(ArMsg::Status { key: ArKey::Set, model: "claude-sonnet".into(), kb_chunks: 42 });
        (root, app)
    }

    fn open_arch(app: &mut App) {
        app.handle_key(prefix());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
    }

    #[test]
    fn arch_overlay_opens_with_two_options() {
        let (root, mut app) = arch_app("arch-open");
        open_arch(&mut app);
        assert_eq!(app.overlay, Some(Overlay::Arch));
        assert_eq!(app.arch.stage, ArchStage::Menu);
        let t = render(&mut app, 120, 40);
        let text = screen_text(&t);
        assert!(text.contains("Review this repo"), "{text}");
        assert!(text.contains("Design from a brief"));
        assert!(text.contains("KB 42 chunks"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_review_sends_a_review_request() {
        let (root, mut app) = arch_app("arch-review");
        open_arch(&mut app);
        let action = app.handle_key(key(KeyCode::Enter));
        match action {
            Action::Arch(ArReq::Review { deep, .. }) => assert!(!deep),
            other => panic!("expected a review request, got {other:?}"),
        }
        assert_eq!(app.arch.stage, ArchStage::Report);
        assert!(app.arch.in_flight);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_design_edits_a_brief_and_submits_on_ctrl_enter() {
        let (root, mut app) = arch_app("arch-design");
        open_arch(&mut app);
        // Move to "Design from a brief" and enter the brief editor.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.arch.stage, ArchStage::Brief);
        for c in "banco".chars() {
            app.handle_key(ch(c));
        }
        // A plain Enter inserts a newline, it does not submit.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.arch.stage, ArchStage::Brief);
        // Ctrl+Enter submits.
        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        match action {
            Action::Arch(ArReq::Design { brief, .. }) => assert_eq!(brief, "banco"),
            other => panic!("expected a design request, got {other:?}"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_brief_esc_returns_to_the_menu() {
        let (root, mut app) = arch_app("arch-esc");
        open_arch(&mut app);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.arch.stage, ArchStage::Brief);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.arch.stage, ArchStage::Menu);
        assert_eq!(app.overlay, Some(Overlay::Arch));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_streams_and_opens_the_report() {
        let (root, mut app) = arch_app("arch-stream");
        open_arch(&mut app);
        let id = match app.handle_key(key(KeyCode::Enter)) {
            Action::Arch(ArReq::Review { id, .. }) => id,
            other => panic!("expected review, got {other:?}"),
        };
        app.apply_arch_msg(ArMsg::Delta { id, text: "# Architecture Review".into() });
        app.apply_arch_msg(ArMsg::Done { id, path: "./.gentle-arch/review-x.md".into() });
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("Architecture Review"));
        assert_eq!(app.arch.report_path.as_deref(), Some("./.gentle-arch/review-x.md"));
        // `o` opens the finished report.
        let action = app.handle_key(ch('o'));
        assert_eq!(action, Action::OpenReport("./.gentle-arch/review-x.md".into()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_without_a_key_shows_the_setup_hint_and_sends_nothing() {
        let (root, mut app) = arch_app("arch-nokey");
        app.apply_arch_msg(ArMsg::Status { key: ArKey::Missing, model: String::new(), kb_chunks: 0 });
        open_arch(&mut app);
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Action::None, "no request without a key");
        assert_eq!(app.arch.stage, ArchStage::Report);
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("gentle-arch key set"), "the setup hint is shown");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_esc_cancels_an_in_flight_request() {
        let (root, mut app) = arch_app("arch-cancel");
        open_arch(&mut app);
        let id = match app.handle_key(key(KeyCode::Enter)) {
            Action::Arch(ArReq::Review { id, .. }) => id,
            other => panic!("expected review, got {other:?}"),
        };
        let action = app.handle_key(key(KeyCode::Esc));
        assert_eq!(action, Action::Arch(ArReq::Cancel { id }));
        assert_eq!(app.overlay, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arch_offline_marks_an_error() {
        let (root, mut app) = arch_app("arch-offline");
        open_arch(&mut app);
        app.handle_key(key(KeyCode::Enter));
        app.set_arch_offline();
        assert!(app.arch.offline);
        assert!(!app.arch.in_flight);
        assert!(app.arch.error.is_some());
        fs::remove_dir_all(root).unwrap();
    }

    // --- Explain popup (S17) --------------------------------------------
    use gentle_engines::protocol::{ClientMsg as ExReq, EngineMsg as ExMsg, KeyState as ExKey};

    fn app_with_editor(name: &str) -> (PathBuf, App) {
        let root = fixture(name);
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.editor = Some(Editor::open(&root, Path::new("src/main.rs")));
        app.focus = Focus::Editor;
        render(&mut app, 120, 40);
        (root, app)
    }

    fn prefix_e(app: &mut App) -> Action {
        app.handle_key(prefix());
        app.handle_key(ch('e'))
    }

    #[test]
    fn explain_popup_shows_key_missing_hint_and_sends_nothing() {
        let (root, mut app) = app_with_editor("explain-nokey");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Missing, model: String::new() });
        assert_eq!(prefix_e(&mut app), Action::None, "no request is sent without a key");
        assert_eq!(app.overlay, Some(Overlay::Explain));
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("gentle-explain key set"), "the setup hint is shown");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explain_popup_streams_renders_and_scrolls() {
        let (root, mut app) = app_with_editor("explain-stream");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "claude-haiku-4-5".into() });
        let action = prefix_e(&mut app);
        let id = match &action {
            Action::Explain(ExReq::Explain { id, selection, .. }) => {
                assert!(selection.contains("fn main"));
                *id
            }
            other => panic!("expected an explain request, got {other:?}"),
        };
        assert!(app.explain.in_flight && app.explain.thinking);
        app.apply_explain_msg(ExMsg::Delta { id, text: "Hello ".into() });
        app.apply_explain_msg(ExMsg::Delta { id, text: "world".into() });
        assert_eq!(app.explain.body, "Hello world");
        assert!(!app.explain.thinking);
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("Hello world"));
        app.apply_explain_msg(ExMsg::Done { id });
        assert!(!app.explain.in_flight);
        // Scroll the streamed text.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.explain.scroll, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.explain.scroll, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explain_popup_esc_cancels_the_in_flight_request() {
        let (root, mut app) = app_with_editor("explain-cancel");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        let id = match prefix_e(&mut app) {
            Action::Explain(ExReq::Explain { id, .. }) => id,
            other => panic!("expected an explain request, got {other:?}"),
        };
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Explain(ExReq::Cancel { id }));
        assert_eq!(app.overlay, None);
        assert!(!app.explain.in_flight);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explain_popup_enter_asks_a_follow_up_in_the_same_conversation() {
        let (root, mut app) = app_with_editor("explain-ask");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        let first = match prefix_e(&mut app) {
            Action::Explain(ExReq::Explain { id, .. }) => id,
            other => panic!("expected an explain request, got {other:?}"),
        };
        app.apply_explain_msg(ExMsg::Done { id: first });
        // Type a question into the popup input, then Enter sends `ask`.
        for c in "why?".chars() {
            app.handle_key(ch(c));
        }
        assert_eq!(app.explain.question, "why?");
        match app.handle_key(key(KeyCode::Enter)) {
            Action::Explain(ExReq::Ask { id, question }) => {
                assert!(id > first, "a new request id");
                assert_eq!(question, "why?");
            }
            other => panic!("expected an ask request, got {other:?}"),
        }
        assert!(app.explain.in_flight && app.explain.question.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn right_click_in_the_editor_opens_the_explain_popup() {
        let (root, mut app) = app_with_editor("explain-rclick");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        let ed = app.layout.unwrap().editor.expect("an editor pane");
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: ed.x + 2,
            row: ed.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(app.handle_mouse(ev), Action::Explain(ExReq::Explain { .. })));
        assert_eq!(app.overlay, Some(Overlay::Explain));
        fs::remove_dir_all(root).unwrap();
    }

    // --- Mascot badge on a selection (S17, L48) -------------------------

    fn shift_right(app: &mut App, n: usize) {
        for _ in 0..n {
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        }
    }

    #[test]
    fn mascot_badge_appears_after_a_shift_selection_and_click_opens_explain() {
        let (root, mut app) = app_with_editor("mascot-shift");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        shift_right(&mut app, 4); // select "fn m"
        assert_eq!(app.editor.as_ref().unwrap().selected_text().as_deref(), Some("fn m"));
        assert!(!app.editor.as_ref().unwrap().dirty, "a selection never edits the buffer");
        let t = render(&mut app, 120, 40);
        let badge = app.selection_badge.expect("badge present with a selection");
        // The flower glyph sits in the first badge cell.
        assert_eq!(t.backend().buffer()[(badge.x, badge.y)].symbol(), "\u{273f}");
        let ed = app.layout.unwrap().editor.unwrap();
        assert_eq!(badge.y, ed.y + 1, "badge on the selection line (first visible row)");
        assert_eq!(badge.width, 3, "the badge is three columns wide");
        // Clicking the badge opens the explain popup for exactly that selection.
        match app.handle_mouse(click(badge.x, badge.y)) {
            Action::Explain(ExReq::Explain { selection, .. }) => assert_eq!(selection, "fn m"),
            other => panic!("expected explain, got {other:?}"),
        }
        assert_eq!(app.overlay, Some(Overlay::Explain));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mascot_badge_toggles_with_the_selection_and_ctrl_e_asks() {
        let (root, mut app) = app_with_editor("mascot-ctrle");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        render(&mut app, 120, 40);
        assert_eq!(app.selection_badge, None, "no badge without a selection");
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(ctrl_e), Action::None, "Ctrl+E is a no-op without a selection");
        // Select: the badge appears and Ctrl+E asks about it.
        shift_right(&mut app, 4);
        render(&mut app, 120, 40);
        assert!(app.selection_badge.is_some(), "badge present with a selection");
        match app.handle_key(ctrl_e) {
            Action::Explain(ExReq::Explain { selection, .. }) => assert_eq!(selection, "fn m"),
            other => panic!("expected explain, got {other:?}"),
        }
        // Close the popup, clear the selection: the badge is gone.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Left));
        render(&mut app, 120, 40);
        assert_eq!(app.selection_badge, None, "the badge disappears when the selection clears");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mascot_badge_appears_after_a_mouse_drag() {
        let (root, mut app) = app_with_editor("mascot-drag");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        render(&mut app, 120, 40);
        let ed = app.layout.unwrap().editor.unwrap();
        // Drag across a few characters on the first line.
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: ed.x + 10,
            row: ed.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(drag);
        render(&mut app, 120, 40);
        assert!(app.editor.as_ref().unwrap().has_selection(), "the drag made a selection");
        let badge = app.selection_badge.expect("badge after a drag");
        match app.handle_mouse(click(badge.x, badge.y)) {
            Action::Explain(ExReq::Explain { .. }) => {}
            other => panic!("expected explain, got {other:?}"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mascot_badge_stays_within_a_narrow_editor() {
        let (root, mut app) = app_with_editor("mascot-narrow");
        app.apply_explain_msg(ExMsg::Status { key: ExKey::Set, model: "m".into() });
        shift_right(&mut app, 4);
        render(&mut app, 80, 20); // the minimum workspace size
        if let Some(b) = app.selection_badge {
            let ed = app.layout.unwrap().editor.unwrap();
            let inner = Block::default().borders(Borders::ALL).inner(ed);
            assert!(b.x >= inner.x && b.x + b.width <= inner.x + inner.width, "badge within width: {b:?} in {inner:?}");
            assert!(b.y >= inner.y && b.y < inner.y + inner.height, "badge within height: {b:?} in {inner:?}");
        }
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
        AppItem { id: id.into(), title: title.into(), text: "body line".into(), ts: "1".into(), link: "l".into(), ..Default::default() }
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
    fn plus_and_minus_adjust_the_apps_panel_height_within_40_to_70() {
        let root = fixture("apps-height");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        render(&mut app, 120, 40);
        assert_eq!(app.apps_panel_pct, layout::APPS_PANEL_DEFAULT_PCT, "default is 45%");
        assert_eq!(app.apps_panel_pct, 45, "the expanded default is 45% (L48)");
        // `+` grows in 5% steps and clamps at 70%.
        app.handle_key(ch('+'));
        assert_eq!(app.apps_panel_pct, 50);
        assert_eq!(app.status_msg.as_deref(), Some("apps panel 50%"));
        for _ in 0..10 {
            app.handle_key(ch('+'));
        }
        assert_eq!(app.apps_panel_pct, layout::APPS_PANEL_MAX_PCT, "clamped at 70%");
        // `-` shrinks in 5% steps and clamps at 40%.
        for _ in 0..10 {
            app.handle_key(ch('-'));
        }
        assert_eq!(app.apps_panel_pct, layout::APPS_PANEL_MIN_PCT, "clamped at 40%");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clicking_an_app_row_expands_the_panel_and_enters_the_app() {
        let root = fixture("apps-click-expand");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one")] });
        render(&mut app, 120, 40);
        assert_eq!(app.apps.view, View::Apps, "starts collapsed");
        let ap = app.apps_area.expect("apps panel region");
        // Click the first app row (one row below the panel border).
        let act = app.handle_mouse(click(ap.x + 2, ap.y + 1));
        assert_eq!(app.focus, Focus::Apps, "the click focuses the panel");
        assert_eq!(app.apps.view, View::Items, "the click expands into the app");
        assert!(matches!(act, Action::Apps(ClientMsg::Refresh { app, .. }) if app == "slack"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apps_panel_header_buttons_collapse_and_expand_independently_of_sidebar_minimize() {
        // L48 minimize bug: the apps panel gets its own [_] collapse and [□]
        // expand controls; the sidebar [_] still minimizes the whole sidebar.
        for (w, h) in [(160u16, 50u16), (120u16, 40u16)] {
            let root = fixture("apps-hdr-buttons");
            let mut app = App::new(root.clone());
            app.mode = Mode::Workspace;
            app.focus = Focus::Apps;
            app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
            app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one")] });
            render(&mut app, w, h);
            let hdr = app.apps_header.expect("apps header buttons captured");
            // The panel starts collapsed; the [□] expand control enters the app.
            let act = app.handle_mouse(click(hdr.expand.0, hdr.row));
            assert_eq!(app.apps.view, View::Items, "[□] expands the panel ({w}x{h})");
            assert!(matches!(act, Action::Apps(ClientMsg::Refresh { app, .. }) if app == "slack"));
            assert!(!app.min_sidebar, "expanding the apps panel never minimizes the sidebar");
            // The [_] collapse control returns it to the collapsed view.
            render(&mut app, w, h);
            let hdr = app.apps_header.expect("apps header buttons captured");
            app.handle_mouse(click(hdr.collapse.0, hdr.row));
            assert_eq!(app.apps.view, View::Apps, "[_] collapses the apps panel ({w}x{h})");
            assert!(!app.min_sidebar, "collapsing the apps panel never minimizes the sidebar");
            // The SIDEBAR [_] minimizes the whole sidebar, leaving the apps view
            // state untouched (the two are independent, T14 stays green).
            render(&mut app, w, h);
            let before = app.apps.view;
            let side = app.title_buttons.iter().find(|b| b.pane == Pane::Sidebar).copied().expect("sidebar button");
            app.handle_mouse(click(side.min_range.0, side.row));
            assert!(app.min_sidebar, "the sidebar [_] minimizes the whole sidebar ({w}x{h})");
            assert_eq!(app.apps.view, before, "the sidebar minimize leaves the apps view unchanged");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn clicking_the_apps_header_text_toggles_collapsed() {
        let root = fixture("apps-hdr-toggle");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one")] });
        render(&mut app, 120, 40);
        let hdr = app.apps_header.expect("apps header");
        assert_eq!(app.apps.view, View::Apps, "starts collapsed");
        // Clicking the "Apps" title expands it.
        let act = app.handle_mouse(click(hdr.header.0, hdr.row));
        assert_eq!(app.apps.view, View::Items, "header click expands");
        assert!(matches!(act, Action::Apps(ClientMsg::Refresh { .. })));
        // Clicking it again collapses it.
        render(&mut app, 120, 40);
        let hdr = app.apps_header.expect("apps header");
        app.handle_mouse(click(hdr.header.0, hdr.row));
        assert_eq!(app.apps.view, View::Apps, "header click collapses");
        fs::remove_dir_all(root).unwrap();
    }

    fn expandable(id: &str, title: &str, kind: &str) -> AppItem {
        AppItem {
            id: id.into(),
            title: title.into(),
            kind: Some(kind.into()),
            expandable: true,
            ..Default::default()
        }
    }
    fn branch_item(id: &str, title: &str) -> AppItem {
        use gentle_apps::protocol::ItemAction;
        AppItem {
            id: id.into(),
            title: title.into(),
            kind: Some("branch".into()),
            expandable: true,
            actions: vec![
                ItemAction { id: "open".into(), label: "Open".into(), confirm: false },
                ItemAction { id: "checkout".into(), label: "Checkout".into(), confirm: true },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn github_panel_expands_with_a_breadcrumb_and_esc_steps_up() {
        let root = fixture("gh-nav");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("github", "GitHub", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items {
            app: "github".into(),
            items: vec![expandable("sec:repos", "Repositories (1)", "section")],
        });
        render(&mut app, 120, 40);
        app.handle_key(key(KeyCode::Enter)); // enter the app → Items + Refresh
        assert_eq!(app.apps.view, View::Items);

        // Enter on the expandable section sends an expand with a new level.
        let act = app.handle_key(key(KeyCode::Enter));
        let id = match act {
            Action::Apps(ClientMsg::Expand { id, app: a, item }) => {
                assert_eq!((a.as_str(), item.as_str()), ("github", "sec:repos"));
                id
            }
            other => panic!("expected an expand, got {other:?}"),
        };
        assert_eq!(app.apps.nav.len(), 1);
        // Children fill the level; the breadcrumb shows the path.
        app.apply_apps_msg(EngineMsg::Children {
            id,
            app: "github".into(),
            item: "sec:repos".into(),
            items: vec![branch_item("branch|o|r|sha|main", "main (default)")],
        });
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("GitHub › Repositories (1)"), "breadcrumb missing");

        // Esc pops back to the section list, then to the app list.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.apps.nav.len(), 0);
        assert_eq!(app.apps.view, View::Items);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.apps.view, View::Apps);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn github_action_menu_and_confirmation_run_an_action() {
        let root = fixture("gh-action");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("github", "GitHub", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items {
            app: "github".into(),
            items: vec![branch_item("branch|o|r|sha|main", "main (default)")],
        });
        render(&mut app, 120, 40);
        app.handle_key(key(KeyCode::Enter)); // enter app → Items

        // `a` opens the action menu for the selected branch.
        app.handle_key(ch('a'));
        assert!(app.apps.menu.is_some());
        // Move to the "Checkout" action (confirm = true) and select it.
        app.handle_key(key(KeyCode::Down));
        let act = app.handle_key(key(KeyCode::Enter));
        assert_eq!(act, Action::None, "a confirm action waits for y/n");
        assert!(app.apps.confirm.is_some());
        assert!(app.apps.menu.is_none());

        // `y` dispatches the action request.
        let act = app.handle_key(ch('y'));
        let id = match act {
            Action::Apps(ClientMsg::Action { id, app: a, item, action }) => {
                assert_eq!((a.as_str(), item.as_str(), action.as_str()), ("github", "branch|o|r|sha|main", "checkout"));
                id
            }
            other => panic!("expected an action, got {other:?}"),
        };
        // The correlated result is shown in the status line.
        app.apply_apps_msg(EngineMsg::ActionResult { id, ok: true, message: Some("checked out main".into()), error: None });
        assert_eq!(app.apps.action_status.as_deref(), Some("checked out main"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn github_confirmation_can_be_declined() {
        let root = fixture("gh-decline");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("github", "GitHub", AppStatusKind::Connected)] });
        app.apply_apps_msg(EngineMsg::Items {
            app: "github".into(),
            items: vec![branch_item("branch|o|r|sha|main", "main")],
        });
        render(&mut app, 120, 40);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ch('a'));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter)); // select checkout → confirm
        assert!(app.apps.confirm.is_some());
        assert_eq!(app.handle_key(ch('n')), Action::None);
        assert!(app.apps.confirm.is_none());
        assert_eq!(app.apps.action_status.as_deref(), Some("cancelled"));
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

    // --- Arquitectura apps-list entry (S18, L48) ------------------------

    #[test]
    fn arch_app_state_maps_each_gentle_arch_status() {
        let root = fixture("arch-row-state");
        let mut app = App::new(root.clone());
        // Offline wins: red.
        app.arch.offline = true;
        assert_eq!(arch_app_state(&app.arch).0, AppStatusKind::Error);
        // No key: gray.
        app.arch.offline = false;
        app.arch.key_missing = true;
        assert_eq!(arch_app_state(&app.arch).0, AppStatusKind::Disconnected);
        // Key set but KB empty: yellow with the update hint.
        app.arch.key_missing = false;
        app.arch.kb_chunks = 0;
        let (state, detail) = arch_app_state(&app.arch);
        assert_eq!(state, AppStatusKind::Connecting);
        assert_eq!(detail, Some("run `gentle-arch kb update`"));
        // Key set and KB populated: green, no detail.
        app.arch.kb_chunks = 12;
        assert_eq!(arch_app_state(&app.arch), (AppStatusKind::Connected, None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arquitectura_row_renders_below_the_engine_apps() {
        let root = fixture("arch-row-render");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        app.apply_arch_msg(ArMsg::Status { key: ArKey::Set, model: "claude-sonnet".into(), kb_chunks: 7 });
        let t = render(&mut app, 120, 40);
        let text = screen_text(&t);
        assert!(text.contains("Slack"), "the engine app still renders: {text}");
        assert!(text.contains("Arquitectura"), "the local Arquitectura row renders: {text}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_on_the_arquitectura_row_opens_the_arch_overlay() {
        let root = fixture("arch-row-enter");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        render(&mut app, 120, 40);
        // Move down from Slack (index 0) to the Arquitectura row (index 1).
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.apps.app_sel, 1, "selection reaches the Arquitectura row");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.overlay, Some(Overlay::Arch), "Enter opens the Arch overlay");
        assert_eq!(app.apps.view, View::Apps, "the apps panel did not expand into an engine app");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clicking_the_arquitectura_row_opens_the_arch_overlay() {
        let root = fixture("arch-row-click");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        render(&mut app, 120, 40);
        let ap = app.apps_area.expect("apps panel region");
        // Row 0 (below the border) is Slack; row 1 is Arquitectura.
        app.handle_mouse(click(ap.x + 2, ap.y + 2));
        assert_eq!(app.overlay, Some(Overlay::Arch), "clicking Arquitectura opens the Arch overlay");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arquitectura_row_does_not_break_slack_github_navigation() {
        let root = fixture("arch-row-nav");
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        app.focus = Focus::Apps;
        app.apply_apps_msg(EngineMsg::Apps {
            apps: vec![
                app_status("slack", "Slack", AppStatusKind::Connected),
                app_status("github", "GitHub", AppStatusKind::Connected),
            ],
        });
        app.apply_apps_msg(EngineMsg::Items { app: "slack".into(), items: vec![app_item("a", "#one")] });
        render(&mut app, 120, 40);
        // Down twice: Slack(0) → GitHub(1) → Arquitectura(2); it does not overshoot.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.apps.app_sel, 2, "Arquitectura is the third row after two engine apps");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.apps.app_sel, 2, "selection clamps at the Arquitectura row");
        // Clicking the first engine row still enters Slack, not Arch.
        let ap = app.apps_area.expect("apps panel region");
        let act = app.handle_mouse(click(ap.x + 2, ap.y + 1));
        assert_eq!(app.overlay, None, "clicking Slack does not open the Arch overlay");
        assert_eq!(app.apps.view, View::Items, "clicking Slack enters the app");
        assert!(matches!(act, Action::Apps(ClientMsg::Refresh { app, .. }) if app == "slack"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arquitectura_row_appears_in_the_chat_apps_overlay() {
        let root = fixture("arch-row-overlay");
        let mut app = App::new(root.clone()); // chat mode
        app.apply_apps_msg(EngineMsg::Apps { apps: vec![app_status("slack", "Slack", AppStatusKind::Connected)] });
        render(&mut app, 120, 40);
        // Prefix `a` opens the apps overlay in chat mode.
        app.handle_key(prefix());
        app.handle_key(ch('a'));
        assert_eq!(app.overlay, Some(Overlay::Apps));
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("Arquitectura"), "the overlay lists Arquitectura");
        fs::remove_dir_all(root).unwrap();
    }

    // ---- Pane zoom / minimize (S14) ----

    fn workspace(name: &str) -> (PathBuf, App) {
        let root = fixture(name);
        let mut app = App::new(root.clone());
        app.mode = Mode::Workspace;
        render(&mut app, 120, 40);
        (root, app)
    }

    fn prefix_key(app: &mut App, c: char) {
        app.handle_key(prefix());
        app.handle_key(ch(c));
    }

    #[test]
    fn prefix_z_toggles_zoom_on_the_focused_pane() {
        let (root, mut app) = workspace("zoom-toggle");
        prefix_key(&mut app, '2'); // focus editor
        assert_eq!(app.focus, Focus::Editor);
        prefix_key(&mut app, 'z');
        assert_eq!(app.zoom, Some(Pane::Editor));
        render(&mut app, 120, 40);
        let l = app.layout.unwrap();
        assert_eq!(l.editor, Some(Rect::new(0, 0, 120, 39)), "editor fills the body");
        assert_eq!((l.sidebar, l.diff, l.show_chat), (None, None, false));
        prefix_key(&mut app, 'z');
        assert_eq!(app.zoom, None, "second z restores");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_d_is_changes_only_and_esc_restores_it() {
        let (root, mut app) = workspace("changes-only");
        prefix_key(&mut app, 'd');
        assert_eq!((app.focus, app.zoom), (Focus::Diff, Some(Pane::Diff)));
        render(&mut app, 120, 40);
        assert_eq!(app.layout.unwrap().diff, Some(Rect::new(0, 0, 120, 39)));
        // Esc on the (read-only) diff restores the zoom.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.zoom, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_m_minimizes_and_moves_focus_off_the_pane() {
        let (root, mut app) = workspace("min-focus");
        prefix_key(&mut app, '2'); // focus editor
        prefix_key(&mut app, 'm');
        assert!(app.min_editor, "editor minimized");
        assert_eq!(app.focus, Focus::Diff, "focus moved to the next visible pane");
        render(&mut app, 120, 40);
        // The diff now owns the full right width (editor is a 1-row bar).
        let l = app.layout.unwrap();
        assert_eq!(l.editor.unwrap().height, 1);
        assert_eq!(l.diff.unwrap().width, 120 - l.sidebar.unwrap().width);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn minimizing_the_last_visible_pane_is_refused() {
        let (root, mut app) = workspace("min-last");
        prefix_key(&mut app, '1');
        prefix_key(&mut app, 'm'); // sidebar
        prefix_key(&mut app, '2');
        prefix_key(&mut app, 'm'); // editor
        prefix_key(&mut app, '3');
        prefix_key(&mut app, 'm'); // diff
        assert_eq!(app.unminimized_count(), 1);
        prefix_key(&mut app, '4'); // focus chat (the last visible)
        prefix_key(&mut app, 'm');
        assert!(!app.min_chat, "the last visible pane cannot be minimized");
        assert_eq!(app.status_msg.as_deref(), Some("can\u{2019}t minimize the last visible pane"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_m_on_a_minimized_pane_restores_it() {
        let (root, mut app) = workspace("min-restore");
        prefix_key(&mut app, '2');
        prefix_key(&mut app, 'm');
        assert!(app.min_editor);
        prefix_key(&mut app, '2'); // explicitly re-focus the minimized editor
        prefix_key(&mut app, 'm');
        assert!(!app.min_editor, "m on a minimized pane restores it");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zoom_and_minimize_are_workspace_only() {
        let root = fixture("zoom-chat-mode");
        let mut app = App::new(root.clone()); // chat mode
        render(&mut app, 120, 40);
        for c in ['z', 'm', 'd'] {
            prefix_key(&mut app, c);
            assert_eq!(app.status_msg.as_deref(), Some("available in workspace mode"));
            assert_eq!((app.zoom, app.min_editor), (None, false));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clicking_the_title_buttons_minimizes_and_zooms_an_unfocused_pane() {
        let (root, mut app) = workspace("title-buttons");
        assert_eq!(app.effective_focus(), Focus::Chat, "editor is not focused");
        let btn = app.title_buttons.iter().find(|b| b.pane == Pane::Editor).copied().expect("editor buttons");
        // Click the minimize button → editor minimized even though unfocused.
        app.handle_mouse(click(btn.min_range.0, btn.row));
        assert!(app.min_editor);
        render(&mut app, 120, 40);
        // Click the diff zoom button → diff zoomed.
        let z = app.title_buttons.iter().find(|b| b.pane == Pane::Diff).copied().expect("diff buttons");
        app.handle_mouse(click(z.zoom_range.0, z.row));
        assert_eq!(app.zoom, Some(Pane::Diff));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clicking_a_minimized_strip_restores_it() {
        let (root, mut app) = workspace("min-strip-click");
        prefix_key(&mut app, '1');
        prefix_key(&mut app, 'm'); // minimize sidebar
        render(&mut app, 120, 40);
        let (_, strip) = app.minimized_regions.iter().find(|(p, _)| *p == Pane::Sidebar).copied().expect("strip");
        assert_eq!(strip.width, layout::MIN_STRIP_WIDTH);
        app.handle_mouse(click(strip.x, strip.y + 2));
        assert!(!app.min_sidebar, "clicking the strip restores the sidebar");
        assert_eq!(app.focus, Focus::Sidebar);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chat_inner_is_none_when_minimized_and_full_when_zoomed() {
        let (root, mut app) = workspace("chat-pty");
        let area = Rect::new(0, 0, 120, 40);
        assert!(app.chat_inner(area).is_some(), "normal chat resizes the PTY");
        app.min_chat = true;
        assert_eq!(app.chat_inner(area), None, "minimized chat keeps its last size");
        app.min_chat = false;
        app.zoom = Some(Pane::Chat);
        let inner = app.chat_inner(area).expect("zoomed chat resizes");
        // Full body (39 rows) minus the pane border.
        assert_eq!(inner, Rect::new(1, 1, 118, 37));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_line_reports_zoom_and_minimized_panes() {
        let (root, mut app) = workspace("status-s14");
        prefix_key(&mut app, '1');
        prefix_key(&mut app, 'm'); // minimize sidebar
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("min: sidebar"), "status lists minimized panes");
        prefix_key(&mut app, '3');
        prefix_key(&mut app, 'z'); // zoom diff
        let t = render(&mut app, 120, 40);
        assert!(screen_text(&t).contains("zoom: diff"), "status reports the zoomed pane");
        fs::remove_dir_all(root).unwrap();
    }
}
