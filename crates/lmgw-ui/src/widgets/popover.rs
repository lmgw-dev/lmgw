//! A floating panel anchored to a button (UX plan §2 #5): the base of Select,
//! ModelPicker and RowMenu.
//!
//! It lives in the browser's top layer (`popover="auto"`), not in the page:
//! the content pane is a size container, which makes it the containing block
//! of every `position: fixed` descendant, and cards and modals clip their
//! overflow. The top layer is above all of that, so a pop opened inside a
//! scrolling table card or a `<dialog>` is never cut off. It is placed from
//! the anchor's rectangle (CSS anchor positioning is not in WebKitGTK), below
//! it, or above it when there is not room below and there is more above.
//!
//! Conventions the pops built on it share:
//! - `.pop-list` is the scrolling part; on open, its `[aria-selected="true"]`
//!   row is scrolled to the middle.
//! - the element marked `data-autofocus` takes focus on open.
//! - the pop closes when the window resizes, and when something that moves
//!   the anchor scrolls (a chat transcript streaming beside the anchor does
//!   not close it; the page body under it does).

use leptos::html::{self, ElementType};
use leptos::prelude::*;
use leptos::wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Space kept between the anchor and the pop, and between the pop and the
/// window edge.
const GAP: f64 = 4.0;
const EDGE: f64 = 8.0;
/// Below the anchor unless less than this (or less than the pop needs) fits
/// there and there is more room above.
const FLIP_BELOW: f64 = 320.0;

#[component]
pub fn Popover<E>(
    /// Two-way: set it to open or close; light dismiss (a click outside, Esc)
    /// writes `false` back.
    open: RwSignal<bool>,
    /// What it is placed against: the button that opens it, or — for a
    /// combobox, whose control is a text field — the box around the field.
    anchor: NodeRef<E>,
    /// Extra classes on the `.pop` element.
    #[prop(optional)]
    class: &'static str,
    /// A floor for the width in px; the anchor's own width is always one.
    #[prop(optional)]
    min_width: Option<u32>,
    /// Rendered only while open.
    children: ChildrenFn,
) -> impl IntoView
where
    E: ElementType + 'static,
    E::Output: JsCast + Clone + 'static,
{
    let pop: NodeRef<html::Div> = NodeRef::new();

    let place = move || {
        let (Some(a), Some(p)) = (anchor.get_untracked(), pop.get_untracked()) else {
            return;
        };
        place_at(a.unchecked_ref(), &p, min_width.unwrap_or(0) as f64);
    };

    Effect::new(move |_| {
        let want = open.get();
        let Some(p) = pop.get() else { return };
        let showing = p.matches(":popover-open").unwrap_or(false);
        if !want {
            if showing {
                let _ = p.hide_popover();
            }
            return;
        }
        if !showing && p.show_popover().is_err() {
            open.set(false);
            return;
        }
        place();
        // The children render in this same tick; by the next frame they are
        // laid out, and that is the height the placement has to fit.
        request_animation_frame(move || {
            if !open.get_untracked() {
                return;
            }
            place();
            if let Some(p) = pop.get_untracked() {
                settle_focus(&p);
            }
        });

        // A scroll that moves the anchor would leave the pop floating over
        // the wrong thing: close. Scrolls inside the pop (its own list), or
        // in a pane beside the anchor, are none of its business.
        let on_scroll = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
            let Some(target) = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()) else {
                return;
            };
            let inside = pop
                .get_untracked()
                .is_some_and(|p| p.contains(Some(&target)));
            let moves_anchor = anchor
                .get_untracked()
                .is_some_and(|a| target.contains(Some(a.unchecked_ref())));
            if !inside && moves_anchor {
                open.set(false);
            }
        });
        let on_resize = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| open.set(false));
        let w = window();
        let _ = w.add_event_listener_with_callback_and_bool(
            "scroll",
            on_scroll.as_ref().unchecked_ref(),
            true,
        );
        let _ = w.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
        // Local arena slot: the cleanup must be `Send`, the JS closures are not.
        let held = StoredValue::new_local((on_scroll, on_resize));
        on_cleanup(move || {
            held.with_value(|(s, r)| {
                let w = window();
                let _ = w.remove_event_listener_with_callback_and_bool(
                    "scroll",
                    s.as_ref().unchecked_ref(),
                    true,
                );
                let _ = w.remove_event_listener_with_callback("resize", r.as_ref().unchecked_ref());
            });
        });
    });

    let class = if class.is_empty() {
        "pop".to_string()
    } else {
        format!("pop {class}")
    };
    view! {
        <div
            node_ref=pop
            class=class
            popover="auto"
            // Light dismiss (outside click, Esc) closes it behind our back;
            // the signal follows.
            on:toggle=move |_| {
                let showing = pop
                    .get_untracked()
                    .is_some_and(|p| p.matches(":popover-open").unwrap_or(false));
                if open.get_untracked() != showing {
                    open.set(showing);
                }
            }
        >
            <Show when=move || open.get()>{children()}</Show>
        </div>
    }
}

/// Position `pop` against `anchor`'s rectangle, in viewport coordinates (the
/// top layer's containing block is the viewport).
fn place_at(anchor: &web_sys::HtmlElement, pop: &web_sys::HtmlElement, min_width: f64) {
    let r = anchor.get_bounding_client_rect();
    let w = window();
    let vw = w
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    let vh = w
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(768.0);
    let s = pop.style();
    let room_w = (vw - 2.0 * EDGE).max(0.0);
    let _ = s.set_property("max-width", &format!("{room_w}px"));
    let _ = s.set_property(
        "min-width",
        &format!("{}px", r.width().max(min_width).min(room_w)),
    );
    // The height it would take with no cap is what "fits" is measured against.
    let _ = s.remove_property("max-height");
    let needed = pop.scroll_height() as f64;
    let below = vh - r.bottom() - GAP - EDGE;
    let above = r.top() - GAP - EDGE;
    let up = below < needed.min(FLIP_BELOW) && above > below;
    if up {
        // Pinned by its bottom edge, so a list that shrinks while filtering
        // stays attached to the anchor instead of floating up and away.
        let _ = s.set_property("top", "auto");
        let _ = s.set_property("bottom", &format!("{}px", vh - r.top() + GAP));
        let _ = s.set_property("max-height", &format!("{}px", above.max(80.0)));
    } else {
        let _ = s.set_property("bottom", "auto");
        let _ = s.set_property("top", &format!("{}px", r.bottom() + GAP));
        let _ = s.set_property("max-height", &format!("{}px", below.max(80.0)));
    }
    // Left-aligned with the anchor; one that would run off the right edge
    // (a row's "⋯" at the end of a table) aligns its right edge instead.
    let pw = pop.offset_width() as f64;
    let left = if r.left() + pw > vw - EDGE {
        r.right() - pw
    } else {
        r.left()
    };
    let left = left.min(vw - EDGE - pw).max(EDGE);
    let _ = s.set_property("left", &format!("{left}px"));
}

/// Focus the `data-autofocus` element and bring the selected row of the
/// `.pop-list` to the middle of it.
fn settle_focus(pop: &web_sys::HtmlElement) {
    if let Ok(Some(el)) = pop.query_selector("[data-autofocus]") {
        if let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() {
            // Focusing would scroll the page to "reveal" it — and a scroll
            // under the anchor closes the pop it is in.
            let opts = web_sys::FocusOptions::new();
            opts.set_prevent_scroll(true);
            let _ = el.focus_with_options(&opts);
        }
    }
    let (Ok(Some(list)), Ok(Some(sel))) = (
        pop.query_selector(".pop-list"),
        pop.query_selector(".pop-list [aria-selected=\"true\"]"),
    ) else {
        return;
    };
    let (Ok(list), Ok(sel)) = (
        list.dyn_into::<web_sys::HtmlElement>(),
        sel.dyn_into::<web_sys::HtmlElement>(),
    ) else {
        return;
    };
    let top = offset_in(&sel, &list);
    let mid = top - (list.client_height() as f64 - sel.offset_height() as f64) / 2.0;
    list.set_scroll_top(mid.max(0.0) as i32);
}

/// `el`'s top relative to `list`'s scrolled content.
fn offset_in(el: &web_sys::HtmlElement, list: &web_sys::HtmlElement) -> f64 {
    let (e, l) = (
        el.get_bounding_client_rect(),
        list.get_bounding_client_rect(),
    );
    e.top() - l.top() + list.scroll_top() as f64
}

/// Scroll `list` the least that shows its `index`th child element whole: the
/// keyboard's active row. Done by hand, not `scrollIntoView`, which would
/// also scroll every scroller around the pop's position in the page.
pub(crate) fn reveal_child(list: &web_sys::HtmlElement, index: usize) {
    let Some(el) = list
        .children()
        .item(index as u32)
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let top = offset_in(&el, list);
    let bottom = top + el.offset_height() as f64;
    let view_top = list.scroll_top() as f64;
    let view_bottom = view_top + list.client_height() as f64;
    if top < view_top {
        list.set_scroll_top(top as i32);
    } else if bottom > view_bottom {
        list.set_scroll_top((bottom - list.client_height() as f64).ceil() as i32);
    }
}

/// How many rows of `list` one PgUp/PgDn moves: a screenful less one, so the
/// row the eye was on stays in view.
pub(crate) fn page_rows(list: &web_sys::HtmlElement) -> usize {
    let row = list
        .first_element_child()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        .map(|e| e.offset_height())
        .filter(|h| *h > 0)
        .unwrap_or(28);
    ((list.client_height() / row) as usize)
        .saturating_sub(1)
        .max(1)
}

/// Tell whoever watches the form around `el` that a custom control changed:
/// the bubbling `change` a native control would have fired, so a guarded
/// modal or `use_touched` sees a Select pick like any typed character.
pub(crate) fn fire_change(el: &web_sys::HtmlElement) {
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    if let Ok(ev) = web_sys::Event::new_with_event_init_dict("change", &init) {
        let _ = el.dispatch_event(&ev);
    }
}
