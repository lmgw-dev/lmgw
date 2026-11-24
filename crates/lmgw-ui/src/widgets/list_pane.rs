//! The list pane of a page on the shared split view ([`super::nav_split`]):
//! the header bar (title, a live line under it, the page's actions), the
//! search, the listbox with its keys, sticky group headings, and what the
//! list says while it loads, failed or holds nothing. Every list + detail
//! page builds its pane out of these; the rows, their grouping, filters,
//! add menus and status pages stay the page's.
//!
//! The page makes its [`ListRefs`] first (Ctrl+F and a back button need
//! them before the pane is built), the pane its [`ListNav`] over the rows
//! it shows. [`Nav`] is a page's navigation, asking the unsaved-edits guard
//! first and carrying the list's own query from one selection to the next.

mod logic;
mod nav;
mod view;

pub use logic::{query_keeping, query_without};
pub use nav::{ListNav, ListRefs, Nav};
pub use view::{GroupHead, ListBox, ListEmpty, ListHead, ListNotice, ListSearch};

/// A path segment: what would end or split it is escaped.
pub fn seg(v: &str) -> String {
    js_sys::encode_uri_component(v).into()
}
