//! State for the bottom-left apps panel (S12). Pure data and transitions so
//! the panel can be unit tested without a terminal; the key routing lives in
//! `app.rs` and rendering reads these fields.
use std::collections::HashMap;

use gentle_apps::protocol::{AppStatus, EngineMsg, Item, ItemAction};

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

/// One level of hierarchical navigation inside an app (GitHub sections → repos →
/// branches → trees). The base app-item list is nav-depth 0 (an empty stack);
/// each `expand` pushes a frame whose children are cached for the session.
#[derive(Debug, Clone, Default)]
pub struct NavFrame {
    /// The id of the item that was expanded to produce this level.
    pub parent_id: String,
    /// Breadcrumb label for this level (the expanded item's title).
    pub crumb: String,
    /// Cached children (empty while loading).
    pub items: Vec<Item>,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Request id of the in-flight `expand` filling this level.
    pub pending: Option<u64>,
}

/// A transient action menu for the selected item.
#[derive(Debug, Clone)]
pub struct ActionMenu {
    pub item_id: String,
    pub actions: Vec<ItemAction>,
    pub sel: usize,
}

/// A pending "Run <label>? y/n" confirmation.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub item_id: String,
    pub action_id: String,
    pub label: String,
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
    /// Hierarchical navigation stack (empty = the app's base item list).
    pub nav: Vec<NavFrame>,
    /// The open action menu, if any.
    pub menu: Option<ActionMenu>,
    /// The pending confirmation, if any.
    pub confirm: Option<Confirm>,
    /// Request id of the in-flight action, if any.
    pub pending_action: Option<u64>,
    /// Transient action result/error shown in the status line.
    pub action_status: Option<String>,
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
            nav: Vec::new(),
            menu: None,
            confirm: None,
            pending_action: None,
            action_status: None,
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
                } else if let Some(frame) = self.nav.iter_mut().find(|f| f.pending == Some(id)) {
                    // An `expand` that failed is reported as a plain Result.
                    if !ok {
                        frame.loading = false;
                        frame.pending = None;
                        frame.error = Some(error.unwrap_or_else(|| "expand failed".to_string()));
                    }
                }
            }
            EngineMsg::Children { id, items, .. } => {
                if let Some(frame) = self.nav.iter_mut().find(|f| f.pending == Some(id)) {
                    frame.items = items;
                    frame.loading = false;
                    frame.pending = None;
                    frame.error = None;
                    frame.sel = 0;
                }
            }
            EngineMsg::ActionResult { id, ok, message, error } => {
                if self.pending_action == Some(id) {
                    self.pending_action = None;
                    self.action_status = Some(if ok {
                        message.unwrap_or_else(|| "done".to_string())
                    } else {
                        error.unwrap_or_else(|| "action failed".to_string())
                    });
                }
            }
            // The in-terminal reader detail (S27) is handled in `app.rs` before
            // it reaches the panel, so the panel ignores it here.
            EngineMsg::Detail { .. } => {}
        }
    }

    /// Items at the current navigation depth (base list, or the deepest frame).
    pub fn visible_items(&self) -> &[Item] {
        match self.nav.last() {
            Some(f) => &f.items,
            None => self.current_items(),
        }
    }

    /// Selection index at the current navigation depth.
    pub fn visible_sel(&self) -> usize {
        self.nav.last().map(|f| f.sel).unwrap_or(self.item_sel)
    }

    /// Sets the selection index at the current navigation depth.
    pub fn set_visible_sel(&mut self, sel: usize) {
        match self.nav.last_mut() {
            Some(f) => f.sel = sel,
            None => self.item_sel = sel,
        }
    }

    /// The selected item at the current navigation depth.
    pub fn selected_visible_item(&self) -> Option<Item> {
        self.visible_items().get(self.visible_sel()).cloned()
    }

    /// Pushes a loading navigation level for `item`, to be filled by a later
    /// `Children` message correlated by `req_id`.
    pub fn push_nav(&mut self, item: &Item, req_id: u64) {
        self.nav.push(NavFrame {
            parent_id: item.id.clone(),
            crumb: item.title.clone(),
            items: Vec::new(),
            sel: 0,
            loading: true,
            error: None,
            pending: Some(req_id),
        });
    }

    /// Re-requests the current level under `req_id` (the refresh key).
    pub fn reload_current_level(&mut self, req_id: u64) {
        if let Some(f) = self.nav.last_mut() {
            f.loading = true;
            f.error = None;
            f.pending = Some(req_id);
        }
    }

    /// Pops one navigation level. Returns true if a level was popped.
    pub fn pop_nav(&mut self) -> bool {
        self.nav.pop().is_some()
    }

    /// Breadcrumb like "GitHub › Repositories › owner/name › main".
    pub fn breadcrumb(&self, app_name: &str) -> String {
        let mut parts = vec![app_name.to_string()];
        for f in &self.nav {
            parts.push(f.crumb.clone());
        }
        parts.join(" › ")
    }

    /// Resets all hierarchical/menu state (e.g. when leaving the app).
    pub fn reset_nav(&mut self) {
        self.nav.clear();
        self.menu = None;
        self.confirm = None;
        self.action_status = None;
    }

    /// Opens the action menu for `item` when it offers any actions.
    pub fn open_menu(&mut self, item: &Item) -> bool {
        if item.actions.is_empty() {
            return false;
        }
        self.menu = Some(ActionMenu { item_id: item.id.clone(), actions: item.actions.clone(), sel: 0 });
        true
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
        Item { id: id.into(), title: "t".into(), text: "x".into(), ts: "1".into(), link: "l".into(), ..Default::default() }
    }
    fn section(id: &str, title: &str) -> Item {
        Item { id: id.into(), title: title.into(), expandable: true, ..Default::default() }
    }
    fn leaf_with_action(id: &str) -> Item {
        Item {
            id: id.into(),
            title: "file.rs".into(),
            actions: vec![ItemAction { id: "open".into(), label: "Open".into(), confirm: false }],
            ..Default::default()
        }
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
    fn expand_pushes_a_level_filled_by_children_and_esc_pops() {
        let mut p = AppsPanel::default();
        p.apply(EngineMsg::Apps { apps: vec![app("github")] });
        p.apply(EngineMsg::Items { app: "github".into(), items: vec![section("sec:repos", "Repositories (1)")] });
        // Base level shows the section.
        assert_eq!(p.visible_items().len(), 1);
        let sec = p.selected_visible_item().unwrap();
        p.push_nav(&sec, 7);
        assert_eq!(p.nav.len(), 1);
        assert!(p.nav[0].loading);
        // Children arrive and fill the level.
        p.apply(EngineMsg::Children {
            id: 7,
            app: "github".into(),
            item: "sec:repos".into(),
            items: vec![section("repo|o|r", "o/r")],
        });
        assert!(!p.nav[0].loading);
        assert_eq!(p.visible_items().len(), 1);
        assert_eq!(p.breadcrumb("GitHub"), "GitHub › Repositories (1)");
        assert!(p.pop_nav());
        assert_eq!(p.nav.len(), 0);
        // Back to the base section list.
        assert_eq!(p.visible_items()[0].id, "sec:repos");
    }

    #[test]
    fn a_failed_expand_sets_the_level_error() {
        let mut p = AppsPanel::default();
        p.apply(EngineMsg::Apps { apps: vec![app("github")] });
        p.apply(EngineMsg::Items { app: "github".into(), items: vec![section("sec:repos", "Repositories")] });
        let sec = p.selected_visible_item().unwrap();
        p.push_nav(&sec, 8);
        p.apply(EngineMsg::Result { id: 8, ok: false, error: Some("boom".into()) });
        assert_eq!(p.nav[0].error.as_deref(), Some("boom"));
        assert!(!p.nav[0].loading);
    }

    #[test]
    fn action_menu_opens_only_for_items_with_actions_and_result_shows_in_status() {
        let mut p = AppsPanel::default();
        assert!(!p.open_menu(&item("flat")), "a flat item has no menu");
        assert!(p.open_menu(&leaf_with_action("blob|o|r|s|1|main|f.rs")));
        assert!(p.menu.is_some());
        p.pending_action = Some(5);
        p.apply(EngineMsg::ActionResult { id: 5, ok: true, message: Some("opened url".into()), error: None });
        assert_eq!(p.action_status.as_deref(), Some("opened url"));
        assert_eq!(p.pending_action, None);
    }

    #[test]
    fn a_slack_shaped_item_stays_flat_and_replyable() {
        // PRESERVE: a flat item (no expandable, no actions) behaves as today.
        let mut p = AppsPanel::default();
        p.apply(EngineMsg::Apps { apps: vec![app("slack")] });
        p.apply(EngineMsg::Items { app: "slack".into(), items: vec![item("C1:1.2")] });
        let it = p.selected_visible_item().unwrap();
        assert!(!it.expandable);
        assert!(it.actions.is_empty());
        // No navigation levels are involved for Slack.
        assert!(p.nav.is_empty());
        assert_eq!(p.visible_items().len(), 1);
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
