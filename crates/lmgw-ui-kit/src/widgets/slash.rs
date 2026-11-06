//! `/` focuses the page's first visible search box (one marked
//! `data-slash`): every list's filter answers to the same key, the
//! dashboard's FilterBar and the Chat's thread filter alike.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// Is a text field, a select or an editable element focused, or a modal
/// up? Then `/` is a character, not a shortcut.
fn typing_somewhere() -> bool {
    let doc = document();
    if matches!(doc.query_selector("dialog[open]"), Ok(Some(_))) {
        return true;
    }
    let Some(el) = doc.active_element() else {
        return false;
    };
    let tag = el.tag_name().to_ascii_lowercase();
    matches!(tag.as_str(), "input" | "textarea" | "select")
        || el
            .dyn_ref::<web_sys::HtmlElement>()
            .is_some_and(|h| h.is_content_editable())
}

/// `/` focuses `input` when nothing is being typed into and no modal is up,
/// if it is the first visible search box on the page — one marked
/// `data-slash`. Every list's filter answers to the same key: a FilterBar,
/// and the custom boxes (Chat's thread filter, Settings search, Agents).
pub fn use_slash_focus(input: NodeRef<html::Input>) {
    let slash = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() != "/"
            || ev.ctrl_key()
            || ev.meta_key()
            || ev.alt_key()
            || ev.default_prevented()
        {
            return;
        }
        let Some(el) = input.get_untracked() else {
            return;
        };
        if typing_somewhere() || !is_first_box(&el) {
            return;
        }
        ev.prevent_default();
        let _ = el.focus();
        el.select();
    });
    on_cleanup(move || slash.remove());
}

fn is_first_box(el: &web_sys::HtmlInputElement) -> bool {
    let Ok(boxes) = document().query_selector_all("[data-slash]") else {
        return false;
    };
    // A box inside something hidden (the rail's search below the narrow
    // breakpoint) has no offset parent.
    (0..boxes.length())
        .filter_map(|i| boxes.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        .find(|h| h.offset_parent().is_some())
        .is_some_and(|first| first.is_same_node(Some(el)))
}
