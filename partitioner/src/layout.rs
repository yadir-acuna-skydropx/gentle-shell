//! Pure layout engine: maps the terminal area to independent partitions.
//! Every partition is rendered as its own region so a change in one pane
//! never forces the others to be rebuilt (S3).
use ratatui::layout::Rect;

/// Splits a workspace sidebar into the file tree (top) and the apps panel
/// (bottom, S12). The panel is a bordered box sized to its content when
/// collapsed (a header plus one row per app) or to `pct`% of the sidebar when
/// expanded (clamped to [`APPS_PANEL_MIN_PCT`]..=[`APPS_PANEL_MAX_PCT`], S16);
/// the tree always keeps at least [`APPS_TREE_MIN`] rows when the sidebar is
/// tall enough, so on a very short terminal the fraction yields to that
/// minimum. The two rects never overlap and together cover the whole sidebar.
pub fn split_sidebar(sidebar: Rect, app_count: u16, expanded: bool, pct: u16) -> (Rect, Rect) {
    // Bordered box: top+bottom border (2) + one row per app, at least 3 tall.
    let collapsed = (app_count + 2).max(3);
    let want = if expanded {
        let pct = pct.clamp(APPS_PANEL_MIN_PCT, APPS_PANEL_MAX_PCT);
        (sidebar.height * pct / 100).max(collapsed)
    } else {
        collapsed
    };
    let max_panel = sidebar.height.saturating_sub(APPS_TREE_MIN);
    let panel_h = want.min(max_panel);
    let tree_h = sidebar.height - panel_h;
    let tree = Rect::new(sidebar.x, sidebar.y, sidebar.width, tree_h);
    let apps = Rect::new(sidebar.x, sidebar.y + tree_h, sidebar.width, panel_h);
    (tree, apps)
}

/// Chat mode shows only the chat plus a lateral tool bar (S7);
/// workspace mode shows sidebar, editor | diff, and chat (S4-S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Chat,
    Workspace,
}

/// The four workspace panes that can be zoomed or minimized (S14). The sidebar
/// (file tree + apps panel) is treated as a single unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Editor,
    Diff,
    Chat,
}

impl Pane {
    /// The four panes in a stable order (focus / status listing / minimize bars).
    pub const ALL: [Pane; 4] = [Pane::Sidebar, Pane::Editor, Pane::Diff, Pane::Chat];
    /// Index into [`Layout::minimized`].
    pub fn index(self) -> usize {
        match self {
            Pane::Sidebar => 0,
            Pane::Editor => 1,
            Pane::Diff => 2,
            Pane::Chat => 3,
        }
    }
    /// Short lowercase name for the status line.
    pub fn name(self) -> &'static str {
        match self {
            Pane::Sidebar => "sidebar",
            Pane::Editor => "editor",
            Pane::Diff => "diff",
            Pane::Chat => "chat",
        }
    }
}

/// Zoom / minimize request applied on top of the base workspace layout (S14).
/// Zoom wins over minimize: a zoomed pane fills the whole body and the others
/// are hidden. Minimized panes collapse to a title bar and the rest grow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Panes {
    pub zoom: Option<Pane>,
    pub min_sidebar: bool,
    pub min_editor: bool,
    pub min_diff: bool,
    pub min_chat: bool,
}

/// Height of a horizontal pane (editor/diff/chat) collapsed to its title bar.
pub const MIN_BAR_HEIGHT: u16 = 1;
/// Width of the sidebar collapsed to its vertical icon strip.
pub const MIN_STRIP_WIDTH: u16 = 3;

/// Minimum terminal size for the partitioned workspace; below it the
/// layout degrades to chat mode so the chat stays usable (S11).
pub const MIN_WORKSPACE_WIDTH: u16 = 80;
pub const MIN_WORKSPACE_HEIGHT: u16 = 20;
pub const TOOLBAR_WIDTH: u16 = 14;
pub const SIDEBAR_MIN: u16 = 24;
pub const SIDEBAR_MAX: u16 = 40;
pub const CHAT_MIN_HEIGHT: u16 = 8;
/// Rows the tree keeps when the apps panel shares the sidebar (S12).
pub const APPS_TREE_MIN: u16 = 5;
/// Bounds and default for the expanded apps-panel height, as a share of the
/// sidebar (percent). The user adjusts it in [`APPS_PANEL_STEP`] steps within
/// [`APPS_PANEL_MIN_PCT`]..=[`APPS_PANEL_MAX_PCT`] (S16).
pub const APPS_PANEL_MIN_PCT: u16 = 40;
pub const APPS_PANEL_MAX_PCT: u16 = 70;
/// Default expanded share: 45% of the sidebar (L48 lowered it from 60%).
pub const APPS_PANEL_DEFAULT_PCT: u16 = 45;
pub const APPS_PANEL_STEP: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Mode actually rendered (may differ from the requested one).
    pub mode: Mode,
    pub toolbar: Option<Rect>,
    pub sidebar: Option<Rect>,
    pub editor: Option<Rect>,
    pub diff: Option<Rect>,
    pub chat: Rect,
    pub status: Rect,
    /// Whether the chat region is present on screen (false only when another
    /// pane is zoomed and hides the chat). When minimized the chat still has a
    /// title-bar region, so this stays true.
    pub show_chat: bool,
    /// The zoomed pane, if any (S14).
    pub zoom: Option<Pane>,
    /// Per-pane minimized flags, indexed by [`Pane::index`].
    pub minimized: [bool; 4],
}

impl Layout {
    pub fn is_minimized(&self, pane: Pane) -> bool {
        self.minimized[pane.index()]
    }
}

/// Base layout with no zoom or minimize (kept for the existing call sites and
/// tests; delegates to [`compute_panes`]).
pub fn compute(area: Rect, requested: Mode) -> Layout {
    compute_panes(area, requested, Panes::default())
}

/// Workspace layout with zoom / minimize applied (S14). Chat mode is unaffected
/// by `panes` (S7): the tool bar + chat layout is returned unchanged.
pub fn compute_panes(area: Rect, requested: Mode, panes: Panes) -> Layout {
    let status_h = area.height.min(1);
    let body_h = area.height - status_h;
    let status = Rect::new(area.x, area.y + body_h, area.width, status_h);
    let body = Rect::new(area.x, area.y, area.width, body_h);

    let fits = area.width >= MIN_WORKSPACE_WIDTH && area.height >= MIN_WORKSPACE_HEIGHT;
    if requested == Mode::Chat || !fits {
        let bar_w = TOOLBAR_WIDTH.min(area.width);
        return Layout {
            mode: Mode::Chat,
            toolbar: Some(Rect::new(area.x, area.y, bar_w, body_h)),
            sidebar: None,
            editor: None,
            diff: None,
            chat: Rect::new(area.x + bar_w, area.y, area.width - bar_w, body_h),
            status,
            show_chat: true,
            zoom: None,
            minimized: [false; 4],
        };
    }

    let minimized = [panes.min_sidebar, panes.min_editor, panes.min_diff, panes.min_chat];

    // Zoom wins: the focused pane fills the whole body; the others are hidden.
    if let Some(z) = panes.zoom {
        let mut l = Layout {
            mode: Mode::Workspace,
            toolbar: None,
            sidebar: None,
            editor: None,
            diff: None,
            chat: Rect::new(body.x, body.y, 0, 0),
            status,
            show_chat: false,
            zoom: Some(z),
            minimized: [false; 4],
        };
        match z {
            Pane::Sidebar => l.sidebar = Some(body),
            Pane::Editor => l.editor = Some(body),
            Pane::Diff => l.diff = Some(body),
            Pane::Chat => {
                l.chat = body;
                l.show_chat = true;
            }
        }
        return l;
    }

    // Minimized horizontal panes (editor/diff/chat) dock as full-width title
    // bars stacked at the bottom of the body; the rest of the body (the top
    // region) holds the sidebar plus the non-minimized panes, which grow.
    let bar_count = [panes.min_editor, panes.min_diff, panes.min_chat].iter().filter(|b| **b).count() as u16;
    let bars_h = bar_count.min(body_h);
    let top_h = body_h - bars_h;
    let top = Rect::new(body.x, body.y, body.width, top_h);

    let has_e = !panes.min_editor;
    let has_d = !panes.min_diff;
    let has_c = !panes.min_chat;
    let any_right = has_e || has_d || has_c;

    // Sidebar column. When it is the only non-minimized pane it fills the whole
    // top region; otherwise a 3-column strip when minimized, else its usual width.
    let side_w = if !any_right {
        top.width
    } else if panes.min_sidebar {
        MIN_STRIP_WIDTH.min(top.width)
    } else {
        (area.width / 4).clamp(SIDEBAR_MIN, SIDEBAR_MAX).min(top.width)
    };
    let sidebar = Rect::new(top.x, top.y, side_w, top_h);
    let right = Rect::new(top.x + side_w, top.y, top.width - side_w, top_h);

    let mut editor = None;
    let mut diff = None;
    let mut chat_rect: Option<Rect> = None;

    if right.width > 0 {
        // Vertical split inside the right region: the editor/diff upper band and
        // the chat below. When chat is minimized the upper band fills the height.
        let upper = if has_c && (has_e || has_d) {
            let upper_h = (right.height * 3 / 5).min(right.height.saturating_sub(CHAT_MIN_HEIGHT));
            let chat_h = right.height - upper_h;
            chat_rect = Some(Rect::new(right.x, right.y + upper_h, right.width, chat_h));
            Rect::new(right.x, right.y, right.width, upper_h)
        } else if has_e || has_d {
            // No visible chat here: editor/diff take the whole right height.
            right
        } else {
            // Only the chat is visible on the right: it fills the region.
            chat_rect = Some(right);
            Rect::new(right.x, right.y, right.width, 0)
        };
        // Horizontal split of the upper band between editor and diff.
        if upper.height > 0 || upper.width > 0 {
            match (has_e, has_d) {
                (true, true) => {
                    let editor_w = upper.width / 2;
                    editor = Some(Rect::new(upper.x, upper.y, editor_w, upper.height));
                    diff = Some(Rect::new(upper.x + editor_w, upper.y, upper.width - editor_w, upper.height));
                }
                (true, false) => editor = Some(upper),
                (false, true) => diff = Some(upper),
                (false, false) => {}
            }
        }
    }

    // Dock the minimized horizontal bars at the bottom, stacked in pane order.
    let mut bar_y = body.y + top_h;
    for pane in [Pane::Editor, Pane::Diff, Pane::Chat] {
        let min = match pane {
            Pane::Editor => panes.min_editor,
            Pane::Diff => panes.min_diff,
            Pane::Chat => panes.min_chat,
            Pane::Sidebar => false,
        };
        if !min {
            continue;
        }
        let bar = Rect::new(body.x, bar_y, body.width, MIN_BAR_HEIGHT);
        match pane {
            Pane::Editor => editor = Some(bar),
            Pane::Diff => diff = Some(bar),
            Pane::Chat => chat_rect = Some(bar),
            Pane::Sidebar => {}
        }
        bar_y += MIN_BAR_HEIGHT;
    }

    let chat = chat_rect.unwrap_or(Rect::new(body.x, body.y, 0, 0));
    Layout {
        mode: Mode::Workspace,
        toolbar: None,
        sidebar: Some(sidebar),
        editor,
        diff,
        chat,
        status,
        show_chat: chat_rect.is_some(),
        zoom: None,
        minimized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    fn cells(r: Option<Rect>) -> u32 {
        r.map_or(0, |r| r.width as u32 * r.height as u32)
    }

    #[test]
    fn chat_mode_shows_toolbar_chat_and_status_only() {
        let l = compute(area(120, 40), Mode::Chat);
        assert_eq!(l.mode, Mode::Chat);
        assert_eq!(l.toolbar, Some(Rect::new(0, 0, TOOLBAR_WIDTH, 39)));
        assert_eq!(l.chat, Rect::new(TOOLBAR_WIDTH, 0, 120 - TOOLBAR_WIDTH, 39));
        assert_eq!(l.status, Rect::new(0, 39, 120, 1));
        assert_eq!((l.sidebar, l.editor, l.diff), (None, None, None));
    }

    #[test]
    fn workspace_mode_partitions_sidebar_editor_diff_chat() {
        let l = compute(area(160, 50), Mode::Workspace);
        assert_eq!(l.mode, Mode::Workspace);
        assert_eq!(l.toolbar, None);
        let sidebar = l.sidebar.unwrap();
        assert_eq!(sidebar, Rect::new(0, 0, 40, 49));
        let editor = l.editor.unwrap();
        let diff = l.diff.unwrap();
        // editor and diff split the upper right area side by side
        assert_eq!(editor.x, 40);
        assert_eq!(editor.y, 0);
        assert_eq!(diff.x, editor.x + editor.width);
        assert_eq!(diff.y, 0);
        assert_eq!(editor.height, diff.height);
        assert_eq!(editor.width + diff.width, 120);
        // chat sits below them, same width
        assert_eq!(l.chat, Rect::new(40, editor.height, 120, 49 - editor.height));
        assert!(l.chat.height >= CHAT_MIN_HEIGHT);
        assert_eq!(l.status, Rect::new(0, 49, 160, 1));
    }

    #[test]
    fn sidebar_width_is_clamped() {
        let narrow = compute(area(80, 30), Mode::Workspace);
        assert_eq!(narrow.sidebar.unwrap().width, SIDEBAR_MIN);
        let wide = compute(area(400, 30), Mode::Workspace);
        assert_eq!(wide.sidebar.unwrap().width, SIDEBAR_MAX);
    }

    #[test]
    fn small_terminal_degrades_workspace_to_chat() {
        let too_narrow = compute(area(MIN_WORKSPACE_WIDTH - 1, 40), Mode::Workspace);
        assert_eq!(too_narrow.mode, Mode::Chat);
        let too_short = compute(area(120, MIN_WORKSPACE_HEIGHT - 1), Mode::Workspace);
        assert_eq!(too_short.mode, Mode::Chat);
        assert_eq!(too_short.sidebar, None);
    }

    #[test]
    fn partitions_never_overlap_and_cover_the_area() {
        for (w, h) in [(80, 20), (97, 33), (160, 50), (255, 80)] {
            for mode in [Mode::Chat, Mode::Workspace] {
                let l = compute(area(w, h), mode);
                let parts = [l.toolbar, l.sidebar, l.editor, l.diff, Some(l.chat), Some(l.status)];
                let total: u32 = parts.iter().map(|p| cells(*p)).sum();
                assert_eq!(total, w as u32 * h as u32, "{w}x{h} {mode:?}");
                let present: Vec<Rect> = parts.iter().flatten().copied().collect();
                for (i, a) in present.iter().enumerate() {
                    for b in &present[i + 1..] {
                        assert!(!a.intersects(*b), "{a:?} overlaps {b:?} at {w}x{h}");
                    }
                }
            }
        }
    }

    #[test]
    fn sidebar_split_never_overlaps_and_covers_the_sidebar() {
        for h in [10u16, 20, 30, 49, 80] {
            for apps in [1u16, 3, 8] {
                for expanded in [false, true] {
                    let side = Rect::new(0, 0, 30, h);
                    let (tree, panel) = split_sidebar(side, apps, expanded, APPS_PANEL_DEFAULT_PCT);
                    // Cover exactly, stacked, no overlap.
                    assert_eq!(tree.height + panel.height, h, "cover {h} {apps} {expanded}");
                    assert_eq!(tree.y, side.y);
                    assert_eq!(panel.y, tree.y + tree.height);
                    assert!(!tree.intersects(panel) || panel.height == 0, "overlap {h} {apps} {expanded}");
                    assert_eq!((tree.width, panel.width), (30, 30));
                    // The tree keeps its minimum whenever the sidebar can afford it.
                    if h >= APPS_TREE_MIN + 3 {
                        assert!(tree.height >= APPS_TREE_MIN, "tree min {h} {apps} {expanded}");
                    }
                }
            }
        }
    }

    #[test]
    fn collapsed_panel_fits_its_app_rows_and_expands_larger() {
        let side = Rect::new(0, 0, 30, 40);
        let (_, collapsed) = split_sidebar(side, 3, false, APPS_PANEL_DEFAULT_PCT);
        assert_eq!(collapsed.height, 5, "3 apps + 2 borders");
        let (_, expanded) = split_sidebar(side, 3, true, APPS_PANEL_DEFAULT_PCT);
        assert!(expanded.height > collapsed.height, "expanding grows the panel");
        assert!(expanded.height <= 40 * APPS_PANEL_MAX_PCT / 100);
    }

    #[test]
    fn expanded_panel_takes_the_requested_fraction_at_40_60_and_70() {
        // A 50-row sidebar can afford any fraction while keeping the 5-row tree.
        let side = Rect::new(0, 0, 30, 50);
        for pct in [APPS_PANEL_MIN_PCT, APPS_PANEL_DEFAULT_PCT, APPS_PANEL_MAX_PCT] {
            let (tree, panel) = split_sidebar(side, 3, true, pct);
            assert_eq!(panel.height, 50 * pct / 100, "panel is {pct}% of the sidebar");
            assert_eq!(tree.height, 50 - panel.height);
            assert!(tree.height >= APPS_TREE_MIN, "tree keeps its minimum at {pct}%");
        }
    }

    #[test]
    fn out_of_range_fractions_clamp_to_the_40_and_70_bounds() {
        let side = Rect::new(0, 0, 30, 100);
        let (_, low) = split_sidebar(side, 3, true, 10);
        assert_eq!(low.height, 100 * APPS_PANEL_MIN_PCT / 100, "below 40% clamps to 40%");
        let (_, high) = split_sidebar(side, 3, true, 95);
        assert_eq!(high.height, 100 * APPS_PANEL_MAX_PCT / 100, "above 70% clamps to 70%");
    }

    #[test]
    fn a_short_sidebar_yields_the_fraction_to_the_five_row_tree_minimum() {
        // 8 rows: 70% would be ~5 panel rows, but the tree keeps 5 so the
        // panel is capped at height - 5 = 3 and the tree keeps its minimum.
        let side = Rect::new(0, 0, 30, 8);
        let (tree, panel) = split_sidebar(side, 3, true, APPS_PANEL_MAX_PCT);
        assert!(tree.height >= APPS_TREE_MIN, "tree keeps its 5-row minimum");
        assert_eq!(tree.height + panel.height, 8, "still covers the sidebar");
    }

    // ---- Zoom / minimize (S14) ----

    fn ws(w: u16, h: u16, p: Panes) -> Layout {
        compute_panes(area(w, h), Mode::Workspace, p)
    }

    /// All present workspace rects (chat only when shown) plus the status line.
    fn present(l: &Layout) -> Vec<Rect> {
        let mut v: Vec<Rect> = [l.sidebar, l.editor, l.diff].into_iter().flatten().collect();
        if l.show_chat {
            v.push(l.chat);
        }
        v.push(l.status);
        v
    }

    fn assert_tiles(l: &Layout, w: u16, h: u16, label: &str) {
        let rects = present(l);
        let total: u32 = rects.iter().map(|r| r.width as u32 * r.height as u32).sum();
        assert_eq!(total, w as u32 * h as u32, "coverage {label}");
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.intersects(*b), "{a:?} overlaps {b:?} ({label})");
            }
        }
    }

    #[test]
    fn zoom_gives_the_focused_pane_the_whole_body_and_hides_the_others() {
        let body = Rect::new(0, 0, 160, 49);
        let pickers: [(Pane, fn(&Layout) -> Option<Rect>); 4] = [
            (Pane::Sidebar, |l| l.sidebar),
            (Pane::Editor, |l| l.editor),
            (Pane::Diff, |l| l.diff),
            (Pane::Chat, |l| if l.show_chat { Some(l.chat) } else { None }),
        ];
        for (pane, pick) in pickers {
            let l = ws(160, 50, Panes { zoom: Some(pane), ..Default::default() });
            assert_eq!(pick(&l), Some(body), "zoomed {pane:?} fills the body");
            assert_eq!(l.zoom, Some(pane));
            // Exactly the zoomed pane plus the status line are present.
            assert_eq!(present(&l), vec![body, l.status]);
            assert_tiles(&l, 160, 50, "zoom");
        }
    }

    #[test]
    fn un_zooming_returns_the_exact_previous_layout() {
        let base = ws(160, 50, Panes::default());
        let zoomed = ws(160, 50, Panes { zoom: Some(Pane::Editor), ..Default::default() });
        assert_ne!(zoomed, base);
        let restored = ws(160, 50, Panes { zoom: None, ..Default::default() });
        assert_eq!(restored, base, "clearing zoom returns the exact base rects");
    }

    #[test]
    fn minimizing_the_editor_gives_the_diff_its_width_and_vice_versa() {
        let base = ws(160, 50, Panes::default());
        let base_diff = base.diff.unwrap();
        let min_e = ws(160, 50, Panes { min_editor: true, ..Default::default() });
        let diff = min_e.diff.unwrap();
        assert_eq!(diff.x, base.sidebar.unwrap().x + base.sidebar.unwrap().width, "diff starts at the right column");
        assert_eq!(diff.width, base.editor.unwrap().width + base_diff.width, "diff takes the editor width too");
        assert_eq!(min_e.editor.unwrap().height, MIN_BAR_HEIGHT, "editor is a title bar");

        let min_d = ws(160, 50, Panes { min_diff: true, ..Default::default() });
        let editor = min_d.editor.unwrap();
        assert_eq!(editor.width, base.editor.unwrap().width + base_diff.width, "editor takes the diff width too");
        assert_eq!(min_d.diff.unwrap().height, MIN_BAR_HEIGHT);
    }

    #[test]
    fn minimizing_the_chat_lets_the_editor_and_diff_take_the_height() {
        let base = ws(160, 50, Panes::default());
        let l = ws(160, 50, Panes { min_chat: true, ..Default::default() });
        assert_eq!(l.chat.height, MIN_BAR_HEIGHT, "chat collapses to a title bar");
        assert!(l.show_chat, "the chat title bar is still shown");
        assert!(l.editor.unwrap().height > base.editor.unwrap().height, "editor grew taller");
        assert_eq!(l.editor.unwrap().height, l.diff.unwrap().height);
    }

    #[test]
    fn minimizing_the_sidebar_is_a_three_column_strip_and_the_right_grows() {
        let base = ws(160, 50, Panes::default());
        let l = ws(160, 50, Panes { min_sidebar: true, ..Default::default() });
        assert_eq!(l.sidebar.unwrap().width, MIN_STRIP_WIDTH, "sidebar is a 3-column strip");
        assert_eq!(l.sidebar.unwrap().height, base.sidebar.unwrap().height);
        // The editor/diff/chat region starts after the strip and is wider now.
        assert_eq!(l.editor.unwrap().x, MIN_STRIP_WIDTH);
        assert!(l.diff.unwrap().width + l.editor.unwrap().width > base.editor.unwrap().width + base.diff.unwrap().width);
    }

    #[test]
    fn every_combination_of_minimized_panes_tiles_the_body_without_overlap() {
        for (w, h) in [(80, 20), (100, 33), (160, 50), (255, 80)] {
            // All 15 non-empty subsets of the four panes (bitmask over 1..=15).
            for mask in 1u8..=15 {
                let panes = Panes {
                    zoom: None,
                    min_sidebar: mask & 1 != 0,
                    min_editor: mask & 2 != 0,
                    min_diff: mask & 4 != 0,
                    min_chat: mask & 8 != 0,
                };
                let l = ws(w, h, panes);
                assert_tiles(&l, w, h, &format!("{w}x{h} mask {mask}"));
            }
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let l = compute(area(10, 2), Mode::Chat);
        assert_eq!(l.status.height, 1);
        let l = compute(area(0, 0), Mode::Workspace);
        assert_eq!(l.chat, Rect::new(0, 0, 0, 0));
    }
}
