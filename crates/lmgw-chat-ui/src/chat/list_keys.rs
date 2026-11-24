//! The thread list's arrow keys: ↑ ↓ move the focus from row to row (the
//! conversations and the folders' heads, as they are listed), Home and End
//! to the first and the last. The focus moves, nothing opens: Enter opens a
//! conversation (or folds a folder), as a click does, so walking the list
//! never switches the open thread or stops a reply.

use wasm_bindgen::JsCast;

/// The rows the keys walk, in the list's order.
const ROWS: &str = ".thread-row, .thread-folder";

/// Where a key moves from row `at` of `n`: `None` for a key that is not
/// the list's.
pub(crate) fn step(key: &str, at: Option<usize>, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    match (key, at) {
        ("ArrowDown", Some(i)) => Some((i + 1).min(n - 1)),
        ("ArrowUp", Some(i)) => Some(i.saturating_sub(1)),
        ("ArrowDown", None) | ("Home", _) => Some(0),
        ("ArrowUp", None) | ("End", _) => Some(n - 1),
        _ => None,
    }
}

/// A key pressed inside the list: on a row itself (not in a rename box or
/// a row's menu), the arrows move the focus.
pub(crate) fn on_key(ev: &web_sys::KeyboardEvent) {
    if ev.ctrl_key() || ev.alt_key() || ev.meta_key() || ev.shift_key() {
        return;
    }
    let Some(list) = ev
        .current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let Some(target) = ev
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    if !target.matches(ROWS).unwrap_or(false) {
        return;
    }
    let Ok(nodes) = list.query_selector_all(ROWS) else {
        return;
    };
    let rows: Vec<web_sys::HtmlElement> = (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        .collect();
    let at = rows
        .iter()
        .position(|r| AsRef::<web_sys::Element>::as_ref(r) == &target);
    if let Some(to) = step(&ev.key(), at, rows.len()) {
        ev.prevent_default();
        let row = &rows[to];
        let _ = row.focus();
        let opts = web_sys::ScrollIntoViewOptions::new();
        opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
        row.scroll_into_view_with_scroll_into_view_options(&opts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrows_walk_the_rows_and_stop_at_the_ends() {
        assert_eq!(step("ArrowDown", Some(0), 3), Some(1));
        assert_eq!(step("ArrowDown", Some(2), 3), Some(2));
        assert_eq!(step("ArrowUp", Some(0), 3), Some(0));
        assert_eq!(step("ArrowUp", Some(2), 3), Some(1));
        assert_eq!(step("Home", Some(2), 3), Some(0));
        assert_eq!(step("End", Some(0), 3), Some(2));
        assert_eq!(step("Enter", Some(0), 3), None);
        assert_eq!(step("ArrowDown", None, 0), None);
    }
}
