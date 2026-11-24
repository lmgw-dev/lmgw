//! The list pane's moving parts: the cursor and the keys that move it, the
//! scroll to the selected row, and the page's guarded navigation.

use std::sync::Arc;

use leptos::html;
use leptos::prelude::*;

use super::logic::{
    list_key, origin, reveal_now, search_key, step_to, ListKey, SearchKey, Seen, Step,
};
use crate::widgets::{use_dirty_guard, use_slash_focus, DirtyGuard};

/// What identifies a row: a path, an id, a page's own selection type.
pub trait RowId: Clone + PartialEq + Send + Sync + 'static {}
impl<T: Clone + PartialEq + Send + Sync + 'static> RowId for T {}

/// What the page holds of its list pane before the pane is built: Ctrl+F
/// goes to `search`, a back button hands the focus to `list`.
pub struct ListRefs<K: 'static> {
    /// What ↑ ↓ stand on: the selection in a wide window, the highlight in
    /// a narrow one, where the detail opens on Enter.
    pub cursor: RwSignal<Option<K>>,
    pub search: NodeRef<html::Input>,
    pub list: NodeRef<html::Div>,
    /// The list header bar draws its line once the list scrolled.
    pub scrolled: RwSignal<bool>,
}

impl<K> Clone for ListRefs<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K> Copy for ListRefs<K> {}

impl<K: RowId> ListRefs<K> {
    pub fn new() -> Self {
        Self {
            cursor: RwSignal::new(None),
            search: NodeRef::new(),
            list: NodeRef::new(),
            scrolled: RwSignal::new(false),
        }
    }
}

impl<K: RowId> Default for ListRefs<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> ListRefs<K> {
    pub fn focus_list(self) {
        if let Some(l) = self.list.try_get_untracked().flatten() {
            let _ = l.focus();
        }
    }

    /// The focus to the list on the next frame, when it shows again (a
    /// narrow window's back button); the page can be gone by then.
    pub fn focus_later(self) {
        request_animation_frame(move || self.focus_list());
    }
}

/// One list pane's keys and cursor over the rows `order` holds.
///
/// Keys, with the focus in the list or the search: ↑ ↓ Home End move the
/// selection (in a narrow window only the highlight; Enter opens it), Enter
/// opens the detail; `/` and Ctrl+F go to the search, Esc clears it, a
/// second Esc goes back to the list.
pub struct ListNav<K: RowId = String> {
    pub cursor: RwSignal<Option<K>>,
    pub search: NodeRef<html::Input>,
    pub list: NodeRef<html::Div>,
    pub scrolled: RwSignal<bool>,
    /// The rows' ids in the list's order.
    order: Signal<Vec<K>>,
    /// The selection the address names; the cursor follows it.
    sel: Signal<Option<K>>,
    /// What a wide window shows as selected (`sel`, unless the page says).
    wide: Signal<Option<K>>,
    /// The search lists hits rather than the rows ([`ListNav::held`]).
    held: Option<Signal<bool>>,
    narrow: Memo<bool>,
    /// Open a row: its id, and whether the move replaces the history entry
    /// (a key move in a wide window).
    open: Callback<(K, bool)>,
    /// The row's `data-key` from its id.
    key_of: fn(&K) -> String,
}

impl<K: RowId> Clone for ListNav<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: RowId> Copy for ListNav<K> {}

impl<K: RowId> ListNav<K> {
    /// Made with the pane: the cursor follows the selection (a click, a
    /// deep link, Back), and the selected row is scrolled into view once
    /// the list has it.
    pub fn new(
        refs: ListRefs<K>,
        order: impl Into<Signal<Vec<K>>>,
        sel: impl Into<Signal<Option<K>>>,
        narrow: Memo<bool>,
        open: Callback<(K, bool)>,
        key_of: fn(&K) -> String,
    ) -> Self {
        let (order, sel) = (order.into(), sel.into());
        let nav = Self {
            cursor: refs.cursor,
            search: refs.search,
            list: refs.list,
            scrolled: refs.scrolled,
            order,
            sel,
            wide: sel,
            held: None,
            narrow,
            open,
            key_of,
        };
        Effect::new(move |prev: Option<Seen<K>>| {
            let s = sel.get();
            narrow.track();
            let have = order.with(|o| s.as_ref().is_some_and(|s| o.contains(s)));
            let now = Seen {
                sel: s,
                have,
                narrow: narrow.get_untracked(),
            };
            if let Some(s) = now.sel.as_ref().filter(|_| have) {
                if nav.cursor.with_untracked(|c| c.as_ref() != Some(s)) {
                    nav.cursor.set(Some(s.clone()));
                }
                if reveal_now(prev.as_ref(), &now) {
                    nav.reveal(s);
                }
            }
            now
        });
        use_slash_focus(nav.search);
        nav
    }

    /// What a wide window shows as selected, where it is not what the
    /// address names (Settings shows Network at a bare `/settings`).
    pub fn wide_from(mut self, wide: impl Into<Signal<Option<K>>>) -> Self {
        self.wide = wide.into();
        self
    }

    /// While `held` the search lists hits rather than rows (Settings'
    /// search): its ↑ ↓ do nothing, nor do the list's in a narrow window.
    pub fn held(mut self, held: impl Into<Signal<bool>>) -> Self {
        self.held = Some(held.into());
        self
    }

    fn is_held(self) -> bool {
        self.held.is_some_and(|h| h.get_untracked())
    }

    /// Scroll a row into view.
    pub fn reveal(self, id: &K) {
        let Some(list) = self.list.get_untracked() else {
            return;
        };
        let key = (self.key_of)(id);
        request_animation_frame(move || {
            let sel = format!("[data-key=\"{}\"]", key.replace('"', "\\\""));
            if let Ok(Some(el)) = list.query_selector(&sel) {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        });
    }

    /// Whether a row has the highlight: the selection in a wide window, the
    /// cursor in a narrow one. Tracked.
    pub fn lit(self, id: &K) -> bool {
        if self.narrow.get() {
            self.cursor.with(|c| c.as_ref() == Some(id))
        } else {
            self.wide.with(|s| s.as_ref() == Some(id))
        }
    }

    /// A click on a row.
    pub fn click(self, id: K) {
        self.cursor.set(Some(id.clone()));
        self.open.run((id, false));
    }

    fn step(self, step: Step) {
        let narrow = self.narrow.get_untracked();
        if narrow && self.is_held() {
            return;
        }
        let o = self.order.get_untracked();
        let at = origin(
            narrow,
            self.cursor.get_untracked(),
            self.sel.get_untracked(),
            self.wide.get_untracked(),
        );
        let from = at.and_then(|c| o.iter().position(|x| *x == c));
        let Some(to) = step_to(o.len(), from, step) else {
            return;
        };
        let next = o[to].clone();
        self.cursor.set(Some(next.clone()));
        self.reveal(&next);
        if !narrow {
            self.open.run((next, true));
        }
    }

    /// The list's keydown.
    pub fn on_list_key(self, ev: web_sys::KeyboardEvent) {
        let modified = ev.ctrl_key() || ev.alt_key() || ev.meta_key();
        match list_key(&ev.key(), modified) {
            Some(ListKey::Move(s)) => {
                ev.prevent_default();
                self.step(s);
            }
            Some(ListKey::Open) => {
                if let Some(c) = self.cursor.get_untracked() {
                    ev.prevent_default();
                    self.open.run((c, false));
                }
            }
            None => {}
        }
    }

    pub fn focus_list(self) {
        if let Some(l) = self.list.try_get_untracked().flatten() {
            let _ = l.focus();
        }
    }

    /// The search box's keydown: Esc clears (a second goes to the list),
    /// the arrows and Enter go on in the list.
    pub fn on_search_key(self, query: RwSignal<String>, ev: web_sys::KeyboardEvent) {
        let empty = query.with_untracked(String::is_empty);
        match search_key(&ev.key(), empty) {
            Some(SearchKey::ToList) => {
                ev.prevent_default();
                self.focus_list();
            }
            Some(SearchKey::Clear) => {
                ev.prevent_default();
                query.set(String::new());
            }
            Some(SearchKey::Move(s)) if !self.is_held() => {
                ev.prevent_default();
                self.step(s);
                self.focus_list();
            }
            Some(SearchKey::Enter) if !self.is_held() => {
                ev.prevent_default();
                self.focus_list();
                if self.cursor.with_untracked(Option::is_none) {
                    self.step(Step::Down);
                }
            }
            _ => {}
        }
    }
}

/// A move in the app: a path, and whether it replaces the history entry.
type Navigate = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// A page's way to move, with the unsaved-edits guard asked first.
#[derive(Clone, Copy)]
pub struct Nav {
    navigate: StoredValue<Navigate>,
    guard: DirtyGuard,
    pathname: Memo<String>,
    hash: Memo<String>,
    /// The router's query string, without its `?`.
    search: Memo<String>,
}

impl Nav {
    /// Made where the router's context is (a page's component).
    pub fn new() -> Self {
        let navigate = leptos_router::hooks::use_navigate();
        let nav: Navigate = Arc::new(move |href: &str, replace: bool| {
            navigate(
                href,
                leptos_router::NavigateOptions {
                    replace,
                    scroll: false,
                    ..Default::default()
                },
            )
        });
        let location = leptos_router::hooks::use_location();
        Self {
            navigate: StoredValue::new(nav),
            guard: use_dirty_guard(),
            pathname: location.pathname,
            hash: location.hash,
            search: location.search,
        }
    }

    pub fn guard(self) -> DirtyGuard {
        self.guard
    }

    /// Go to `to`; a move that would leave unsaved edits behind asks first,
    /// as a link does ("Leave and discard" then makes it). Whether it moved
    /// now.
    pub fn go(self, to: &str, replace: bool) -> bool {
        if self.guard.ask_before(to) {
            return false;
        }
        self.go_unasked(to, replace);
        true
    }

    /// Go to `to` without asking: what would be asked about is gone already
    /// (the edits of a row that was deleted, or the form a create just saved).
    pub fn go_unasked(self, to: &str, replace: bool) {
        let _ = self.navigate.try_with_value(|nav| nav(to, replace));
    }

    /// The address's query string with its `?` (empty without one),
    /// untracked.
    pub fn search(self) -> String {
        let s = self.search.get_untracked();
        if s.is_empty() {
            s
        } else {
            format!("?{s}")
        }
    }

    /// The list's own query (its keys `keep`) out of the address.
    pub fn kept(self, keep: &[&str]) -> String {
        super::logic::query_keeping(&self.search(), keep)
    }

    /// Open `path` with the list's own query carried along, unless the
    /// address (hash included) is there already.
    pub fn open(self, path: &str, keep: &[&str], replace: bool) {
        let to = format!("{path}{}", self.kept(keep));
        let hash = self.hash.get_untracked();
        let hash = if hash.is_empty() || hash.starts_with('#') {
            hash
        } else {
            format!("#{hash}")
        };
        let here = format!("{}{}{hash}", self.pathname.get_untracked(), self.search());
        if to != here {
            self.go(&to, replace);
        }
    }

    /// A narrow window's back button: the list at `base` with its query,
    /// and the focus into it.
    pub fn back<K>(self, base: &str, keep: &[&str], refs: ListRefs<K>) {
        self.go(&format!("{base}{}", self.kept(keep)), false);
        refs.focus_later();
    }
}

impl Default for Nav {
    fn default() -> Self {
        Self::new()
    }
}
