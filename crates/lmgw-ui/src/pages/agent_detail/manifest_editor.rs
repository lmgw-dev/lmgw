//! The Definition tab's manifest editor: the agent's `agent.json`, soft
//! wrapped and as tall as its wrapped text, so the pane around it is the one
//! scroller (ux:U-2).
//!
//! A manifest is JSON, but what makes its lines long is prose: a field's
//! `description` runs to a thousand characters on one line. Unwrapped, that
//! line was 8000 px wide, and the box's horizontal scrollbar sat under its
//! last line, a whole manifest below the text it scrolled. Wrapping is
//! visual only (`pre-wrap` keeps every newline and every indent, and the text
//! saved is the text typed), so the structure still reads by its indents
//! while every description reads in place.
//!
//! The height is measured, not taken from `rows`: a wrapped line is several
//! lines tall, and how many depends on the pane's width, so the box is
//! refitted on every change of the text and of the pane's width (the sidebar
//! folding, the window, the pane's own scrollbar coming and going).

use leptos::html;
use leptos::prelude::*;

/// Fit the box to its wrapped text. Collapsed for the measure (no `min-height`,
/// no flex growth, no height), `scrollHeight` is the text and the padding;
/// the border comes on top, as `.input` is border-box. The CSS `min-height`
/// and flex growth apply again afterwards, so a short manifest still reaches
/// the bottom of the pane. The collapse shrinks the pane's content for the
/// measure, which has two side effects that are undone: the pane's scroll
/// offset would be clamped, so it is put back; and the pane's classic
/// scrollbar (html.tauri) would go away and widen the box by the bar, so its
/// width is held for the measure — measured wider, the text wraps into fewer
/// lines than it has beside the bar, and the box came out a line short.
fn fit(ta: &web_sys::HtmlTextAreaElement) {
    let pane = ta.parent_element();
    let top = pane.as_ref().map(|p| p.scroll_top());
    let style = web_sys::HtmlElement::style(ta);
    let width = ta.get_bounding_client_rect().width();
    let _ = style.set_property("width", &format!("{width}px"));
    let _ = style.set_property("min-height", "0");
    let _ = style.set_property("flex-grow", "0");
    let _ = style.set_property("height", "0");
    let text = ta.scroll_height();
    let border = ta.offset_height() - ta.client_height();
    let _ = style.set_property("height", &format!("{}px", text + border));
    let _ = style.remove_property("width");
    let _ = style.remove_property("min-height");
    let _ = style.remove_property("flex-grow");
    ta.set_scroll_top(0);
    if let (Some(p), Some(top)) = (pane, top) {
        p.set_scroll_top(top);
    }
}

/// The editor itself. `pane` is the scroller it sits in, whose width decides
/// where the text wraps.
#[component]
pub(super) fn ManifestEditor(text: RwSignal<String>, pane: NodeRef<html::Div>) -> impl IntoView {
    let area: NodeRef<html::Textarea> = NodeRef::new();
    let size = crate::charts::use_element_size(pane);
    let width = Memo::new(move |_| size.get().0);
    Effect::new(move |_| {
        text.track();
        // Read, not `track()`: a memo never read never subscribes.
        let _ = width.get();
        // A frame later, so the value binding has written the new text.
        request_animation_frame(move || {
            // `None` once the tab is gone: a frame can outlive it.
            if let Some(el) = area.try_get_untracked().flatten() {
                fit(&el);
            }
        });
    });
    view! {
        <textarea
            class="input ta mono manifest-editor"
            spellcheck="false"
            // The height until the first fit a frame later: a row per line,
            // which wrapping only adds to.
            rows=move || text.with(|t| t.lines().count() + 1)
            node_ref=area
            prop:value=move || text.get()
            on:input=move |ev| text.set(event_target_value(&ev))
        ></textarea>
    }
}
