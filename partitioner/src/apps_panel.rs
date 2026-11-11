//! State for the bottom-left apps panel (S12). Pure data and transitions so
//! the panel can be unit tested without a terminal; the key routing lives in
//! `app.rs` and rendering reads these fields.
use std::collections::HashMap;

use gentle_apps::protocol::{AppStatus, EngineMsg, Item};

/// Which level of the panel the user is navigating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The app list (one row per app).
    Apps,
    /// The selected app's items.
    Items,
    /// The reply editor for the selected item.
    Reply,
}

/// A single-line reply input with horizontal scroll and UTF-8 editing.
#[derive(Debug, Default, Clone)]
pub struct ReplyBox {
    pub text: String,
    /// Cursor position as a char index into `text`.
    pub cursor: usize,
    /// First visible char column (horizontal scroll).
    pub scroll: usize,
}

impl ReplyBox {
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.scroll = 0;
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of char index `i` (UTF-8 safe).
    fn byte_of(&self, i: usize) -> usize {
        self.text.char_indices().nth(i).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_of(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_of(self.cursor - 1);
        let end = self.byte_of(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    /// Keeps the cursor visible within `width` columns, updating `scroll`.
    pub fn ensure_visible(&mut self, width: usize) {
        let width = width.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + width {
            self.scroll = self.cursor + 1 - width;
        }
    }

    /// The visible slice `[scroll, scroll+width)` of the text.
    pub fn visible(&self, width: usize) -> String {
        self.text.chars().skip(self.scroll).take(width.max(1)).collect()
    }
}

/// All state for the apps panel.
pub struct AppsPanel {
    pub apps: Vec<AppStatus>,
    pub items: HashMap<String, Vec<Item>>,
    /// True while the engine process is not running.
    pub offline: bool,
    pub view: View,
    pub app_sel: usize,
    pub item_sel: usize,
    pub reply: ReplyBox,
    /// Request id of the in-flight reply, if any.
    pub pending_reply: Option<u64>,
    /// Transient reply feedback ("sending…", "sent", or an error).
    pub reply_status: Option<String>,
}

impl Default for AppsPanel {
    fn default() -> Self {
        Self {
            apps: Vec::new(),
            items: HashMap::new(),
            offline: false,
            view: View::Apps,
            app_sel: 0,
            item_sel: 0,
            reply: ReplyBox::default(),
            pending_reply: None,
            reply_status: None,
        }
    }
}

impl AppsPanel {
    /// Applies one decoded engine message.
    pub fn apply(&mut self, msg: EngineMsg) {
        self.offline = false;
        match msg {
            EngineMsg::Apps { apps } => {
                self.apps = apps;
                if self.app_sel >= self.apps.len() {
                    self.app_sel = self.apps.len().saturating_sub(1);
                }
            }
            EngineMsg::Items { app, items } => {
                self.items.insert(app, items);
                let max = self.current_items().len();
                if self.item_sel >= max {
                    self.item_sel = max.saturating_sub(1);
                }
            }
            EngineMsg::Result { id, ok, error } => {
                if self.pending_reply == Some(id) {
                    self.pending_reply = None;
                    self.reply_status = Some(if ok {
                        "sent".to_string()
                    } else {
                        error.unwrap_or_else(|| "reply failed".to_string())
                    });
                }
            }
        }
    }

    /// Marks the engine offline; items are kept but greyed by the renderer.
    pub fn set_offline(&mut self) {
        self.offline = true;
    }

    /// Whether the panel shows the selected app's items (vs just the app list).
    pub fn is_expanded(&self) -> bool {
        self.view != View::Apps
    }

    pub fn selected_app_id(&self) -> Option<String> {
        self.apps.get(self.app_sel).map(|a| a.id.clone())
    }

    pub fn current_items(&self) -> &[Item] {
        match self.apps.get(self.app_sel) {
            Some(a) => self.items.get(&a.id).map(Vec::as_slice).unwrap_or(&[]),
            None => &[],
        }
    }

    pub fn item_count(&self, id: &str) -> usize {
        self.items.get(id).map(Vec::len).unwrap_or(0)
    }

    pub fn selected_item_id(&self) -> Option<String> {
        self.current_items().get(self.item_sel).map(|i| i.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gentle_apps::protocol::{AppStatus, Item, Status};

    fn app(id: &str) -> AppStatus {
        AppStatus { id: id.into(), name: id.into(), status: Status::Connected, detail: None }
    }
    fn item(id: &str) -> Item {
        Item { id: id.into(), title: "t".into(), text: "x".into(), ts: "1".into(), link: "l".into() }
    }

    #[test]
    fn apply_updates_apps_items_and_correlates_reply_result() {
        let mut p = AppsPanel::default();
        p.apply(EngineMsg::Apps { apps: vec![app("slack")] });
        assert_eq!(p.apps.len(), 1);
        p.apply(EngineMsg::Items { app: "slack".into(), items: vec![item("a"), item("b")] });
        assert_eq!(p.current_items().len(), 2);
        assert_eq!(p.item_count("slack"), 2);

        p.pending_reply = Some(7);
        p.reply_status = Some("sending…".into());
        p.apply(EngineMsg::Result { id: 7, ok: true, error: None });
        assert_eq!(p.pending_reply, None);
        assert_eq!(p.reply_status.as_deref(), Some("sent"));

        p.pending_reply = Some(8);
        p.apply(EngineMsg::Result { id: 8, ok: false, error: Some("nope".into()) });
        assert_eq!(p.reply_status.as_deref(), Some("nope"));
    }

    #[test]
    fn reply_box_edits_utf8_with_horizontal_scroll() {
        let mut r = ReplyBox::default();
        for c in "héllo".chars() {
            r.insert(c);
        }
        assert_eq!(r.text, "héllo");
        assert_eq!(r.cursor, 5);
        // Backspacing over the multi-byte 'é' must stay UTF-8 safe.
        for _ in 0..4 {
            r.backspace();
        }
        assert_eq!(r.text, "h");
        // Scrolling keeps the cursor visible in a 3-wide box.
        for c in "ello world".chars() {
            r.insert(c);
        }
        assert_eq!(r.cursor, 11);
        r.ensure_visible(3);
        // Cursor sits just past 'd'; the window scrolled to keep it visible.
        assert_eq!(r.scroll, 9);
        assert_eq!(r.visible(3), "ld");
        assert!(r.scroll > 0);
    }
}
