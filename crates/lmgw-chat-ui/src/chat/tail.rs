//! Keeping the tail of the conversation in view — the port of the old app's
//! `scrollDown` (chat-app.js:742-747), which the Leptos rewrite dropped along
//! with its `.messages` pane.
//!
//! The reader is **pinned** to the end while they are within [`AT_END_PX`] of
//! it, and stays pinned until they scroll up past that: whatever then makes
//! the transcript taller or its pane shorter re-pins them, through one
//! `ResizeObserver` on the pane and its column ([`install`]). That covers the
//! growth no stream event announces: a picture that lands as a card hundreds
//! of pixels tall, a re-roll's strip, a spoken result's player, an `<img>`
//! decoded after layout, a composer growing under the pane.
//!
//! Pinned is kept as state, not measured after the growth: measured
//! afterwards, a picture taller than [`AT_END_PX`] reads as the reader having
//! scrolled up and the tail stops following (the gap left under a generated
//! picture). And a scroll up away from the end unpins only when the reader
//! made it: one they asked for (the wheel, a touch drag, a press on the pane's
//! scrollbar, a scrolling key) since they were last at the very end, or one
//! with no change of size beside it. A scroll the page made while the
//! transcript or its pane changed size is not one, and goes back to the end:
//! WebKitGTK was seen to drop the pane to the top while a picture's live pane
//! mounted (`scrollTop` 211 → 0 as the transcript went 531 → 698 px), which
//! read as the reader leaving the end.

use std::cell::Cell;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

thread_local! {
    /// At most one queued frame: a local model lands dozens of tokens between
    /// repaints, and each would otherwise register its own callback for the
    /// same single scroll. `Some(true)` means a forced scroll won the frame.
    static SCROLL_PENDING: Cell<Option<bool>> = const { Cell::new(None) };
    /// Whether the reader is at the end of the transcript (see the module).
    static PINNED: Cell<bool> = const { Cell::new(true) };
    /// The reader asked for a scroll since they were last at the very end.
    static ASKED: Cell<bool> = const { Cell::new(false) };
    /// The pane's `scrollTop` at its last scroll event: which way a scroll
    /// went.
    static LAST_TOP: Cell<i32> = const { Cell::new(0) };
    /// The pane's `scrollHeight` and `clientHeight` when last seen (a scroll
    /// event, a resize): whether a scroll came with a change of size.
    static LAST_SIZE: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
}

/// How close to the bottom of the transcript a reader counts as at its end,
/// in CSS pixels: the tail follows them.
const AT_END_PX: i32 = 120;

/// The keys that scroll a page up (and Shift+Space), or move the focus to
/// what may be above.
const UP_KEYS: [&str; 4] = ["PageUp", "ArrowUp", "Home", "Tab"];

/// A listener kept for the page's lifetime: removed again when it goes.
type Listener = (
    web_sys::EventTarget,
    &'static str,
    Closure<dyn FnMut(web_sys::Event)>,
);

fn pane() -> Option<web_sys::Element> {
    document().get_element_by_id("chat-scroll")
}

/// The pane's distance from its end, in CSS pixels.
fn gap(pane: &web_sys::Element) -> i32 {
    pane.scroll_height() - pane.scroll_top() - pane.client_height()
}

/// The transcript's height and the pane's.
fn size(pane: &web_sys::Element) -> (i32, i32) {
    (pane.scroll_height(), pane.client_height())
}

/// Bring the end of the transcript into view.
fn to_end(pane: &web_sys::Element) {
    pane.set_scroll_top(pane.scroll_height());
}

/// Follow the tail after a change: `force` (opening a thread, sending) always
/// lands at the bottom and pins the reader; otherwise only a pinned reader
/// follows — one who scrolled up to read something is not yanked back down
/// every token.
///
/// Done inside a `requestAnimationFrame`, not at the call site: Leptos flushes
/// the DOM on the microtask queue, so the element a token produced does not
/// exist yet when the event handler returns, and `scrollHeight` read there is
/// the previous frame's.
pub(crate) fn scroll_down(force: bool) {
    let already_queued = SCROLL_PENDING.with(|p| {
        let prev = p.get();
        p.set(Some(prev.unwrap_or(false) || force));
        prev.is_some()
    });
    if already_queued {
        return;
    }
    request_animation_frame(move || {
        let force = SCROLL_PENDING.with(|p| p.take()).unwrap_or(false);
        let Some(pane) = pane() else { return };
        if force {
            ASKED.set(false);
        }
        if force || PINNED.get() || gap(&pane) < AT_END_PX {
            to_end(&pane);
            PINNED.set(true);
        }
    });
}

/// Whether the reader is at the end of the transcript now, read before a
/// change lands: what another writer appends follows such a reader with
/// `scroll_down(true)`.
pub(crate) fn at_end() -> bool {
    PINNED.get() || pane().is_some_and(|p| gap(&p) < AT_END_PX)
}

/// The reader was taken somewhere in the transcript (a search hit): growth
/// must not pull them back to its end.
pub(crate) fn unpin() {
    PINNED.set(false);
}

/// Watch the pane (`scroller`) and its column (`content`) while the page is
/// mounted: the reader's scrolls set pinned, and any change of either's size
/// re-pins a pinned reader in the same frame, before it is painted.
pub(super) fn install(scroller: NodeRef<leptos::html::Div>, content: NodeRef<leptos::html::Div>) {
    Effect::new(move |_| {
        let (Some(pane), Some(col)) = (scroller.get(), content.get()) else {
            return;
        };
        PINNED.set(true);
        ASKED.set(false);
        LAST_TOP.set(pane.scroll_top());
        LAST_SIZE.set(size(&pane));
        let pane: web_sys::Element = pane.into();
        let mut held: Vec<Listener> = Vec::new();
        let mut listen = |target: web_sys::EventTarget,
                          name: &'static str,
                          f: Box<dyn FnMut(web_sys::Event)>| {
            let f = Closure::wrap(f);
            let opts = web_sys::AddEventListenerOptions::new();
            opts.set_passive(true);
            let _ = target.add_event_listener_with_callback_and_add_event_listener_options(
                name,
                f.as_ref().unchecked_ref(),
                &opts,
            );
            held.push((target, name, f));
        };

        // What the reader asks for: the wheel or a touch drag up, a press on
        // the pane's own box (its scrollbar, not a button in a card), a key
        // that scrolls up outside a text field.
        listen(
            pane.clone().into(),
            "wheel",
            Box::new(|ev| {
                if ev
                    .dyn_ref::<web_sys::WheelEvent>()
                    .is_some_and(|w| w.delta_y() < 0.0)
                {
                    ASKED.set(true);
                }
            }),
        );
        listen(
            pane.clone().into(),
            "touchmove",
            Box::new(|_| ASKED.set(true)),
        );
        listen(
            pane.clone().into(),
            "mousedown",
            Box::new(|ev| {
                if ev.target().map(JsValue::from) == ev.current_target().map(JsValue::from) {
                    ASKED.set(true);
                }
            }),
        );
        listen(
            window().into(),
            "keydown",
            Box::new(|ev| {
                let Some(k) = ev.dyn_ref::<web_sys::KeyboardEvent>() else {
                    return;
                };
                let editing = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                    .is_some_and(|el| {
                        el.is_content_editable()
                            || matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
                    });
                let key = k.key();
                if !editing && (UP_KEYS.contains(&key.as_str()) || key == " " && k.shift_key()) {
                    ASKED.set(true);
                }
            }),
        );

        {
            let p = pane.clone();
            listen(
                pane.clone().into(),
                "scroll",
                Box::new(move |_| {
                    let top = p.scroll_top();
                    let up = top < LAST_TOP.replace(top);
                    let now = size(&p);
                    let resized = LAST_SIZE.replace(now) != now;
                    let gap = gap(&p);
                    if gap < AT_END_PX {
                        PINNED.set(true);
                        if gap <= 1 {
                            ASKED.set(false);
                        }
                    } else if up && PINNED.get() {
                        if ASKED.get() || !resized {
                            // The reader went up to read something.
                            PINNED.set(false);
                        } else {
                            // The page moved it while resizing.
                            to_end(&p);
                        }
                    }
                }),
            );
        }

        let resized = {
            let pane = pane.clone();
            Closure::<dyn FnMut()>::new(move || {
                if PINNED.get() && gap(&pane) > 0 {
                    to_end(&pane);
                }
                LAST_SIZE.set(size(&pane));
            })
        };
        let Ok(ro) = web_sys::ResizeObserver::new(resized.as_ref().unchecked_ref()) else {
            return;
        };
        ro.observe(&pane);
        ro.observe(&col);

        let held = StoredValue::new_local(Some((held, ro, resized)));
        on_cleanup(move || {
            if let Some((held, ro, _resized)) = held.try_update_value(Option::take).flatten() {
                ro.disconnect();
                for (target, name, f) in held {
                    let _ = target
                        .remove_event_listener_with_callback(name, f.as_ref().unchecked_ref());
                }
            }
        });
    });
}
