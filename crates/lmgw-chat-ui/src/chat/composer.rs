//! The composer's text box grows with its text, up to the bubble's
//! `max-height` (app.css), and scrolls past it. Measured in Rust rather than
//! with CSS `field-sizing: content`, which the app's WebKitGTK webview cannot
//! be relied on for.
//!
//! An empty box shows a short hint, one of [`HINTS`] in turn. It is an
//! element over the box rather than the box's `placeholder`: a placeholder
//! cannot fade, and WebKitGTK lays a zoomed page's placeholder out wider
//! than its box (by the zoom factor, so at the app's 125% a textarea with
//! any placeholder is 25% wider inside than it is), which a classic
//! scrollbar shows as a horizontal bar under an empty box.

use leptos::html;
use leptos::prelude::*;

/// What an empty box says, one at a time, first to last and around again.
/// One short line each: a slash command or another way in gets a line of
/// its own here rather than a longer sentence. Enter sends, as everywhere,
/// so no line says it; the box's tooltip ([`KEYS`]) has every key.
pub(super) const HINTS: &[&str] = &[
    "Message…",
    "Shift+Enter for a new line",
    "Type # to add knowledge",
];

/// The box's tooltip while it holds no dictation (which has its own): the
/// keys, where they stay findable while the hints come and go.
pub(super) const KEYS: &str = "Enter sends · Shift+Enter for a new line · # adds knowledge";

/// Fit the box to its text: collapse it so `scrollHeight` is the text's own
/// height, then set that. The CSS `max-height` clamps it, and only past that
/// does the box get a vertical scrollbar: below the cap a classic bar's
/// gutter would narrow the text, and a rounding pixel (a 110.4 px text
/// reads as a 111 `scrollHeight` at 125%) would show a bar for nothing. The
/// box's scroll offset survives the collapse, which would otherwise move a
/// long draft's view while typing in its middle.
pub(super) fn fit(ta: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(ta);
    let top = ta.scroll_top();
    let _ = style.set_property("overflow-y", "hidden");
    let _ = style.set_property("height", "auto");
    let height = ta.scroll_height();
    // No border on the composer's box: the content height is the box's.
    let _ = style.set_property("height", &format!("{height}px"));
    if ta.scroll_height() > ta.client_height() + 1 {
        let _ = style.set_property("overflow-y", "auto");
    }
    ta.set_scroll_top(top);
}

/// Refit whenever the text changes from anywhere (typing, Send clearing it,
/// a refused send putting it back, dictation, a `#` pick, a thread switch)
/// and whenever the bubble changes width, which changes where the text
/// wraps — including coming back from voice mode, whose `display: none`
/// measured nothing. A frame later, so the value binding has written the
/// new text first.
pub(super) fn follow(
    text: RwSignal<String>,
    ta: NodeRef<html::Textarea>,
    bubble: NodeRef<html::Div>,
) {
    let size = lmgw_ui_kit::element_size::use_element_size(bubble);
    let width = Memo::new(move |_| size.get().0);
    Effect::new(move |_| {
        text.track();
        // Read, not `track()`: a memo computes lazily, and one never read
        // never subscribes to its source, so it would never change.
        let _ = width.get();
        request_animation_frame(move || {
            // `None` once the page is gone: a frame can outlive it.
            if let Some(el) = ta.try_get_untracked().flatten() {
                fit(&el);
            }
        });
    });
}

/// The empty box's hint, over the box's first line: shown while `text` is
/// empty, the next of [`HINTS`] every 5 s with a short fade (a plain swap
/// under reduced motion). The cycle is one infinite CSS animation (app.css,
/// `.composer-hint`), and the text moves on at each of its iterations, where
/// the animation holds the hint at opacity 0: no timer of its own that could
/// drift against the fade, and no fade that ends (WebKitGTK blanks an element
/// for a frame when an opacity transition finishes). A `display: none` stops
/// the animation, so it holds still while voice mode hides the composer, and
/// a hint shown again starts its animation afresh; after the box had text it
/// starts again at the first hint. Hidden from screen readers: the box is
/// named "Message", and a name that changed every few seconds would be read
/// out each time.
#[component]
pub(super) fn ComposerHint(text: RwSignal<String>) -> impl IntoView {
    let at = RwSignal::new(0_usize);
    let empty = Memo::new(move |_| text.with(String::is_empty));
    Effect::new(move |_| {
        if !empty.get() {
            at.set(0);
        }
    });
    view! {
        <div
            class="composer-hint"
            aria-hidden="true"
            style:display=move || if empty.get() { "" } else { "none" }
            on:animationiteration=move |_| at.update(|i| *i = (*i + 1) % HINTS.len())
        >
            {move || HINTS[at.get() % HINTS.len()]}
        </div>
    }
}
