//! A side panel beside a main pane (UX plan §2 #17): Chat's thread list and
//! thread settings, the labs' help panels.
//!
//! The panel is a share of the split's width held between a floor and a
//! ceiling (`clamp(220px, 24%, 360px)` unless the caller says otherwise), so
//! it grows with the window without ever taking the room the main pane needs;
//! there is no drag handle. Folded, it is a 28px rail that still says what it
//! holds ("Voice library 2") and opens it again. Whether it is open is kept as
//! `lmgw.ui.side.<persist>`.
//!
//! When the main pane beside the open panel would be narrower than
//! `auto_collapse_below`, the panel folds to its rail whatever was stored,
//! and the rail then lays the panel *over* the main pane for a look instead
//! of squeezing it further. A click back in the main pane, Esc, or picking
//! something marked `data-dock-pick` inside the panel puts it away. The
//! threshold is measured (a ResizeObserver on the split, see
//! [`crate::charts::use_element_size`]) rather than written as a container
//! query: it is the caller's number, and a container query cannot take one.
//!
//! Each pane scrolls itself: the panel's body is its own scroller, and the
//! main pane is the caller's to fill (a transcript, a form column). The panel
//! is mounted only while it shows — a thread's settings list its attached MCP
//! servers' tools, which connects them, and a folded panel must not. So what
//! is typed in it must not live in it: a draft belongs to the caller (Chat
//! holds its thread settings), and a field that saves on `change` is blurred
//! before the panel goes, however it goes — a click back in the main pane,
//! Esc, the fold button, a pick, or the window narrowing — so its value is
//! committed, not dropped with the element (review code:A2, par:PAR-1/2).

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::charts::use_element_size;

/// Which edge of the split the panel sits on.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// `(floor px, share of the split in %, ceiling px)`.
pub const DEFAULT_WIDTH: (u32, u32, u32) = (220, 24, 360);
/// The main pane's floor beside an open panel, unless the caller sets one.
pub const DEFAULT_COLLAPSE_BELOW: u32 = 560;

/// The open panel's width in a split `w` px wide.
pub fn side_width(w: f64, (min, pct, max): (u32, u32, u32)) -> f64 {
    (w * f64::from(pct) / 100.0)
        .min(f64::from(max))
        .max(f64::from(min))
}

/// Whether an open panel would leave the main pane narrower than `below`.
/// An unmeasured split (width 0, the first frame) is not narrow.
pub fn too_narrow(w: f64, width: (u32, u32, u32), below: u32) -> bool {
    w > 0.0 && w - side_width(w, width) < f64::from(below)
}

#[component]
pub fn SplitPane(
    #[prop(optional)] side: Side,
    /// Kept as `lmgw.ui.side.<persist>` (e.g. `chat.threads`).
    persist: &'static str,
    /// What the panel holds: its head, and the folded rail's words.
    #[prop(into)]
    label: TextProp,
    /// The pill beside the label ("23", "5 of 23").
    #[prop(optional, into)]
    badge: Option<Signal<String>>,
    /// The main pane's floor in px beside the open panel (default 560).
    #[prop(optional)]
    auto_collapse_below: Option<u32>,
    /// Open while nothing is stored.
    #[prop(default = true)]
    default_open: bool,
    /// `(floor px, share %, ceiling px)` of the panel (default 220, 24, 360).
    #[prop(optional)]
    width: Option<(u32, u32, u32)>,
    /// Extra classes on the `.dock` element.
    #[prop(optional)]
    class: &'static str,
    /// The panel's body.
    #[prop(into)]
    side_view: ViewFn,
    /// The main pane.
    children: Children,
) -> impl IntoView {
    let width = width.unwrap_or(DEFAULT_WIDTH);
    let below = auto_collapse_below.unwrap_or(DEFAULT_COLLAPSE_BELOW);
    let stored = crate::prefs::persisted_bool(&format!("side.{persist}"), default_open);
    let peek = RwSignal::new(false);
    let root: NodeRef<html::Div> = NodeRef::new();
    let rail: NodeRef<html::Button> = NodeRef::new();
    let aside: NodeRef<html::Aside> = NodeRef::new();
    let size = use_element_size(root);
    let narrow = Memo::new(move |_| too_narrow(size.get().0, width, below));
    let docked = Memo::new(move |_| stored.get() && !narrow.get());
    let peeking = Memo::new(move |_| narrow.get() && peek.get());
    // A look that outlived the narrow window would reappear, unasked, the
    // next time the window narrows.
    Effect::new(move |_| {
        if !narrow.get() {
            peek.set(false);
        }
    });
    // Whether the panel is mounted. Follows `docked || peeking`, but through
    // here, so the field with focus inside is blurred — and fires its
    // `change` — before the panel it sits in is taken out.
    let shown = RwSignal::new(docked.get_untracked() || peeking.get_untracked());
    Effect::new(move |_| {
        let want = docked.get() || peeking.get();
        if !want && shown.get_untracked() {
            release_focus(aside);
        }
        if shown.get_untracked() != want {
            shown.set(want);
        }
    });

    let open = move || {
        if narrow.get_untracked() {
            peek.update(|p| *p = !*p);
        } else {
            stored.set(true);
        }
    };
    let fold = move || {
        if narrow.get_untracked() {
            peek.set(false);
            if let Some(r) = rail.get_untracked() {
                let _ = r.focus();
            }
        } else {
            stored.set(false);
        }
    };

    let (fold_glyph, open_glyph, edge) = match side {
        Side::Left => ("«", "»", "left"),
        Side::Right => ("»", "«", "right"),
    };
    let class = move || {
        let mut c = format!("dock {edge}");
        if peeking.get() {
            c.push_str(" peek");
        }
        if !class.is_empty() {
            c.push(' ');
            c.push_str(class);
        }
        c
    };
    let style = move || {
        let w = size.get().0;
        if w > 0.0 {
            format!("--side-w:{:.0}px", side_width(w, width))
        } else {
            String::new()
        }
    };

    let (l1, l2, l3) = (label.clone(), label.clone(), label.clone());
    let pill = move || badge.map(|b| b.get()).filter(|b| !b.is_empty());
    let panel = move || {
        let (l1, l2) = (l1.clone(), l2.clone());
        let side_view = side_view.clone();
        view! {
            <Show when=move || shown.get()>
                <aside
                    class="dock-side"
                    node_ref=aside
                    on:keydown=move |ev| {
                        if ev.key() == "Escape" && peeking.get_untracked() {
                            ev.stop_propagation();
                            fold();
                        }
                    }
                    on:click=move |ev| {
                        // A pick inside a look is the end of the look.
                        let picked = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .and_then(|el| el.closest("[data-dock-pick]").ok().flatten())
                            .is_some();
                        if picked && peeking.get_untracked() {
                            peek.set(false);
                        }
                    }
                >
                    <div class="dock-head">
                        <span class="dock-title">{let l = l1.clone(); move || l.get()}</span>
                        {move || pill().map(|b| view! { <span class="count">{b}</span> })}
                        <button
                            type="button"
                            class="dock-fold"
                            title={
                                let l = l2.clone();
                                move || format!("Fold {}", l.get())
                            }
                            on:click=move |_| fold()
                        >
                            {fold_glyph}
                        </button>
                    </div>
                    <div class="dock-body">{side_view.run()}</div>
                </aside>
            </Show>
        }
    };
    let rail_view = move || {
        let l3 = l3.clone();
        view! {
            <Show when=move || !docked.get()>
                <button
                    type="button"
                    class="dock-rail"
                    node_ref=rail
                    aria-expanded=move || peeking.get().to_string()
                    title={
                        let l = l3.clone();
                        move || {
                            if peeking.get() {
                                format!("Put {} away", l.get())
                            } else if narrow.get() {
                                format!("Show {} over the page (the window is too narrow to keep it open)", l.get())
                            } else {
                                format!("Show {}", l.get())
                            }
                        }
                    }
                    on:click=move |_| open()
                >
                    <span class="dock-glyph" aria-hidden="true">{open_glyph}</span>
                    <span class="dock-rail-label">{let l = l3.clone(); move || l.get()}</span>
                    {move || pill().map(|b| view! { <span class="count">{b}</span> })}
                </button>
            </Show>
        }
    };
    let main = view! {
        <div
            class="dock-main"
            on:pointerdown=move |_| {
                if peeking.get_untracked() {
                    peek.set(false);
                }
            }
        >
            {children()}
        </div>
    };

    match side {
        Side::Left => view! {
            <div class=class style=style node_ref=root>
                {panel()}
                {rail_view()}
                {main}
            </div>
        }
        .into_any(),
        Side::Right => view! {
            <div class=class style=style node_ref=root>
                {main}
                {rail_view()}
                {panel()}
            </div>
        }
        .into_any(),
    }
}

/// Blur the focused element if it sits inside the panel: an input's pending
/// `change` fires now, while it is still in the document.
fn release_focus(aside: NodeRef<html::Aside>) {
    let (Some(panel), Some(active)) = (aside.get_untracked(), document().active_element()) else {
        return;
    };
    if panel.contains(Some(&active)) {
        if let Some(el) = active.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.blur();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_is_a_share_of_the_split_between_its_floor_and_ceiling() {
        assert_eq!(side_width(1224.0, DEFAULT_WIDTH), 293.76);
        assert_eq!(side_width(700.0, DEFAULT_WIDTH), 220.0);
        assert_eq!(side_width(2344.0, DEFAULT_WIDTH), 360.0);
        assert_eq!(side_width(1000.0, (280, 30, 420)), 300.0);
    }

    #[test]
    fn a_main_pane_under_its_floor_folds_the_panel() {
        // 1224 − 294 = 930 of main: room enough.
        assert!(!too_narrow(1224.0, DEFAULT_WIDTH, 560));
        // 760 − 220 = 540 < 560.
        assert!(too_narrow(760.0, DEFAULT_WIDTH, 560));
        // A lab's help panel folds below a 1600 px pane (1600 − 360 = 1240).
        assert!(too_narrow(1599.0, DEFAULT_WIDTH, 1240));
        assert!(!too_narrow(1600.0, DEFAULT_WIDTH, 1240));
        // Not measured yet: nothing to decide on.
        assert!(!too_narrow(0.0, DEFAULT_WIDTH, 560));
    }
}
