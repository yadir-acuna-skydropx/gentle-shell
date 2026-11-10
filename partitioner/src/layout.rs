//! Pure layout engine: maps the terminal area to independent partitions.
//! Every partition is rendered as its own region so a change in one pane
//! never forces the others to be rebuilt (S3).
use ratatui::layout::Rect;

/// Chat mode shows only the chat plus a lateral tool bar (S7);
/// workspace mode shows sidebar, editor | diff, and chat (S4-S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Chat,
    Workspace,
}

/// Minimum terminal size for the partitioned workspace; below it the
/// layout degrades to chat mode so the chat stays usable (S11).
pub const MIN_WORKSPACE_WIDTH: u16 = 80;
pub const MIN_WORKSPACE_HEIGHT: u16 = 20;
pub const TOOLBAR_WIDTH: u16 = 14;
pub const SIDEBAR_MIN: u16 = 24;
pub const SIDEBAR_MAX: u16 = 40;
pub const CHAT_MIN_HEIGHT: u16 = 8;

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
}

pub fn compute(area: Rect, requested: Mode) -> Layout {
    let status_h = area.height.min(1);
    let body_h = area.height - status_h;
    let status = Rect::new(area.x, area.y + body_h, area.width, status_h);

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
        };
    }

    let side_w = (area.width / 4).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    let right_x = area.x + side_w;
    let right_w = area.width - side_w;
    // Upper viewer takes ~60% of the body, never squeezing the chat below its minimum.
    let upper_h = (body_h * 3 / 5).min(body_h.saturating_sub(CHAT_MIN_HEIGHT));
    let editor_w = right_w / 2;
    Layout {
        mode: Mode::Workspace,
        toolbar: None,
        sidebar: Some(Rect::new(area.x, area.y, side_w, body_h)),
        editor: Some(Rect::new(right_x, area.y, editor_w, upper_h)),
        diff: Some(Rect::new(right_x + editor_w, area.y, right_w - editor_w, upper_h)),
        chat: Rect::new(right_x, area.y + upper_h, right_w, body_h - upper_h),
        status,
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
    fn tiny_terminal_does_not_panic() {
        let l = compute(area(10, 2), Mode::Chat);
        assert_eq!(l.status.height, 1);
        let l = compute(area(0, 0), Mode::Workspace);
        assert_eq!(l.chat, Rect::new(0, 0, 0, 0));
    }
}
