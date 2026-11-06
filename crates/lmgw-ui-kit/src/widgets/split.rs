//! A side panel beside a main pane (UX plan §2 #17): Chat's thread list and
//! thread settings, the labs' help panels.
//!
//! Until the user picks a width, the panel is a share of the split's width
//! held between a floor and a ceiling (`clamp(220px, 24%, 360px)` unless the
//! caller says otherwise), so it grows with the window without ever taking
//! the room the main pane needs. Folded, it is a 28px rail that still says
//! what it holds ("Voice library 2") and opens it again. Whether it is open
//! is kept as `lmgw.ui.side.<persist>`.
//!
//! The open panel's inner edge is a drag handle (a `separator`, focusable):
//! dragging it, or its arrow keys (16 px, 64 with Shift; Home and End go to
//! the bounds), set the width, kept in px as `lmgw.ui.side.<persist>.width`.
//! A double-click or Enter forgets it, and the share above applies again.
//! The bounds are the panel's floor and whatever leaves the main pane its
//! `auto_collapse_below`; a drag stops there rather than jumping. The ceiling
//! only bounds the automatic width. A window that later narrows past a chosen
//! width shows the panel narrower, down to its floor, without rewriting what
//! was chosen — the window widening again brings it back.
//!
//! When the main pane beside the open panel would be narrower than
//! `auto_collapse_below`, the panel folds to its rail whatever was stored,
//! and the rail then lays the panel *over* the main pane for a look instead
//! of squeezing it further. A click back in the main pane, Esc, or picking
//! something marked `data-dock-pick` inside the panel puts it away. The look
//! has no handle: it is there because the window has no room for the panel,
//! and a width set over the page would not be one the split could keep. The
//! threshold is measured (a ResizeObserver on the split, see
//! [`crate::element_size::use_element_size`]) rather than written as a container
//! query: it is the caller's number, and a container query cannot take one.
//!
//! A caller that has a column too narrow for any rail (the Chat below 520 px,
//! a Firefox sidebar) hands a [`Drawer`]: while it is active the panel has no
//! rail and is never docked; it lies over the main pane while the caller's
//! `open` is set (from a button of its own), from the panel's edge, as wide as
//! its automatic share held between its floor and the split's width, and goes
//! away as the look does.
//!
//! Each pane scrolls itself, and the panel's body only vertically: its body is
//! its own scroller, and the main pane is the caller's to fill (a transcript,
//! a form column). A row too long for the panel ellipsizes rather than
//! widening it — with the app's classic scrollbars a pixel of overflow is a
//! bar. The panel
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

use crate::element_size::use_element_size;

/// The panel as a drawer in place of its rail (module doc): `active` while
/// the caller's column has no room for a rail, `open` while the drawer is
/// out. The caller shows the button that sets `open`; Esc, a click back in
/// the main pane, a pick and the fold button clear it, and so does the
/// drawer ending.
#[derive(Clone, Copy)]
pub struct Drawer {
    pub active: Signal<bool>,
    pub open: RwSignal<bool>,
}

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

/// The folded rail's width in px (kit.css `.dock-rail`).
const RAIL: f64 = 28.0;
/// One arrow-key step of the handle in px, and one with Shift.
pub const STEP: f64 = 16.0;
pub const STEP_LARGE: f64 = 64.0;

/// The open panel's automatic width in a split `w` px wide.
pub fn side_width(w: f64, (min, pct, max): (u32, u32, u32)) -> f64 {
    (w * f64::from(pct) / 100.0)
        .min(f64::from(max))
        .max(f64::from(min))
}

/// The widest the open panel may be in a split `w` px wide: what leaves the
/// main pane `below`, never under the panel's own floor. Whole pixels, down:
/// a split measured at 827.2 px leaves 267 and a main pane of 560.2, where
/// 267.2 would leave a main pane a rounding error under its floor (folded).
pub fn max_width(w: f64, (min, _, _): (u32, u32, u32), below: u32) -> f64 {
    (w - f64::from(below)).floor().max(f64::from(min))
}

/// The open panel's width: the chosen one held between the floor and
/// [`max_width`] (the choice itself is kept), else the automatic one.
pub fn docked_width(w: f64, width: (u32, u32, u32), below: u32, chosen: Option<f64>) -> f64 {
    match chosen {
        Some(c) => c.min(max_width(w, width, below)).max(f64::from(width.0)),
        None => side_width(w, width),
    }
}

/// The look's width (the panel laid over the main pane): the chosen or
/// automatic width, short of the rail beside it, never under the floor.
pub fn peek_width(w: f64, width: (u32, u32, u32), chosen: Option<f64>) -> f64 {
    chosen
        .unwrap_or_else(|| side_width(w, width))
        .min(w - RAIL)
        .max(f64::from(width.0))
}

/// The drawer's width: the chosen or automatic width, never under the
/// panel's floor, never wider than the split.
pub fn drawer_width(w: f64, width: (u32, u32, u32), chosen: Option<f64>) -> f64 {
    chosen
        .unwrap_or_else(|| side_width(w, width))
        .max(f64::from(width.0))
        .min(w)
}

/// Whether an open panel would leave the main pane narrower than `below`.
/// An unmeasured split (width 0, the first frame) is not narrow.
pub fn too_narrow(w: f64, width: (u32, u32, u32), below: u32, chosen: Option<f64>) -> bool {
    w > 0.0 && w - docked_width(w, width, below, chosen) < f64::from(below)
}

/// The width a drag that started at `start` px has reached after the
/// pointer moved `dx` px, within `(lo, hi)`. The handle is on the panel's
/// inner edge, so a left panel follows the pointer and a right one mirrors it.
pub fn drag_width(side: Side, start: f64, dx: f64, (lo, hi): (f64, f64)) -> f64 {
    let w = match side {
        Side::Left => start + dx,
        Side::Right => start - dx,
    };
    w.min(hi).max(lo).round()
}

/// What a key on the focused handle does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum KeyMove {
    /// Set this width.
    To(f64),
    /// Forget the chosen width: the automatic one applies.
    Reset,
}

/// The handle's keys: the arrows move the edge (16 px, 64 with Shift), Home
/// and End go to the narrowest and the widest, Enter resets. `None` for any
/// other key.
pub fn key_move(
    key: &str,
    shift: bool,
    side: Side,
    now: f64,
    (lo, hi): (f64, f64),
) -> Option<KeyMove> {
    let step = if shift { STEP_LARGE } else { STEP };
    // The arrows move the separator; a right panel grows when it moves left.
    let grow = match side {
        Side::Left => step,
        Side::Right => -step,
    };
    let to = |w: f64| KeyMove::To(w.min(hi).max(lo).round());
    match key {
        "ArrowRight" => Some(to(now + grow)),
        "ArrowLeft" => Some(to(now - grow)),
        "Home" => Some(to(lo)),
        "End" => Some(to(hi)),
        "Enter" => Some(KeyMove::Reset),
        _ => None,
    }
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
    /// A drawer in place of the rail while its `active` holds (module doc).
    #[prop(optional)]
    drawer: Option<Drawer>,
    /// The panel's body.
    #[prop(into)]
    side_view: ViewFn,
    /// The main pane.
    children: Children,
) -> impl IntoView {
    let width = width.unwrap_or(DEFAULT_WIDTH);
    let below = auto_collapse_below.unwrap_or(DEFAULT_COLLAPSE_BELOW);
    let stored = crate::prefs::persisted_bool(&format!("side.{persist}"), default_open);
    let width_key = format!("side.{persist}.width");
    // The width the user chose, in px; `None` is the automatic share.
    let chosen = RwSignal::new(crate::prefs::stored_number(&width_key).filter(|w| *w > 0.0));
    let width_key = StoredValue::new(width_key);
    // A drawer's look is the caller's `open`: its button sets it.
    let peek = drawer.map_or_else(|| RwSignal::new(false), |d| d.open);
    let drawn = Memo::new(move |_| drawer.is_some_and(|d| d.active.get()));
    let root: NodeRef<html::Div> = NodeRef::new();
    let rail: NodeRef<html::Button> = NodeRef::new();
    let aside: NodeRef<html::Aside> = NodeRef::new();
    let size = use_element_size(root);
    let narrow =
        Memo::new(move |_| drawn.get() || too_narrow(size.get().0, width, below, chosen.get()));
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
    // The handle: a drag in progress is (pointer, its start x, the width then);
    // `moved` keeps a plain click (or the two of a double-click) from turning
    // the automatic width into a chosen one.
    let drag = StoredValue::new(None::<(i32, f64, f64)>);
    let moved = StoredValue::new(false);
    let dragging = RwSignal::new(false);
    let bounds = move || {
        let w = size.get_untracked().0;
        (f64::from(width.0), max_width(w, width, below))
    };
    let now_width =
        move || docked_width(size.get_untracked().0, width, below, chosen.get_untracked());
    let keep = move |w: f64| {
        chosen.set(Some(w));
        width_key.with_value(|k| crate::prefs::store_number(k, w));
    };
    let reset = move || {
        chosen.set(None);
        width_key.with_value(|k| crate::prefs::forget(k));
        // The automatic width can be one the split has no room for (a lab
        // panel chosen at 300 docks where its automatic 360 folds): the handle
        // goes with the panel, and the focus it held would drop to <body>, so
        // it moves to the rail's open button once that is mounted.
        if too_narrow(size.get_untracked().0, width, below, None) {
            request_animation_frame(move || {
                if let Some(r) = rail.get_untracked() {
                    let _ = r.focus();
                }
            });
        }
    };
    let follow = move |x: i32| {
        if let Some((_, x0, w0)) = drag.get_value() {
            let dx = f64::from(x) - x0;
            if dx != 0.0 {
                moved.set_value(true);
            }
            if moved.get_value() {
                chosen.set(Some(drag_width(side, w0, dx, bounds())));
            }
        }
    };
    let end_drag = move || {
        if drag.get_value().is_some() {
            drag.set_value(None);
            dragging.set(false);
            if moved.get_value() {
                if let Some(w) = chosen.get_untracked() {
                    keep(w);
                }
            }
        }
    };

    // The handle can go while a drag is on (the window was resized or tiled
    // narrow, the panel folded): its `lostpointercapture` then fires at the
    // document, where nothing listens, and the drag would stay on, with
    // `.resizing` holding the cursor and the selection lock. The drag ends
    // here instead, storing the width reached like any other end.
    Effect::new(move |_| {
        if !docked.get() {
            end_drag();
        }
    });
    let panel_id = StoredValue::new(format!("dock-{}", persist.replace('.', "-")));

    let class = move || {
        let mut c = format!("dock {edge}");
        if drawn.get() {
            c.push_str(" drawer");
        }
        if peeking.get() {
            c.push_str(" peek");
        }
        if dragging.get() {
            c.push_str(" resizing");
        }
        if !class.is_empty() {
            c.push(' ');
            c.push_str(class);
        }
        c
    };
    let style = move || {
        let w = size.get().0;
        if w <= 0.0 {
            String::new()
        } else if peeking.get() && drawn.get() {
            format!("--side-w:{:.0}px", drawer_width(w, width, chosen.get()))
        } else if peeking.get() {
            format!("--side-w:{:.0}px", peek_width(w, width, chosen.get()))
        } else {
            format!(
                "--side-w:{:.0}px",
                docked_width(w, width, below, chosen.get())
            )
        }
    };

    let (l1, l2, l3, l4) = (label.clone(), label.clone(), label.clone(), label.clone());
    let measured = move || size.get().0 > 0.0;
    let handle_view = move || {
        let l4 = l4.clone();
        view! {
            <Show when=move || docked.get() && measured()>
                <div
                    class="dock-handle"
                    role="separator"
                    tabindex="0"
                    aria-orientation="vertical"
                    aria-controls=move || panel_id.get_value()
                    aria-label={
                        let l = l4.clone();
                        move || format!("Resize {}", l.get())
                    }
                    aria-valuemin=width.0.to_string()
                    aria-valuemax=move || format!("{:.0}", max_width(size.get().0, width, below))
                    aria-valuenow=move || {
                        format!("{:.0}", docked_width(size.get().0, width, below, chosen.get()))
                    }
                    aria-valuetext=move || {
                        let w = docked_width(size.get().0, width, below, chosen.get());
                        if chosen.get().is_some() {
                            format!("{w:.0} px")
                        } else {
                            format!("{w:.0} px, automatic")
                        }
                    }
                    title="Drag to resize · double-click for the automatic width"
                    on:pointerdown=move |ev| {
                        if ev.button() != 0 {
                            return;
                        }
                        // Not prevented: the press keeps its default, the focus
                        // (the arrows fine-tune what the drag left); kit.css
                        // keeps a text selection from starting at the handle.
                        if let Some(el) = ev
                            .current_target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                        {
                            let _ = el.set_pointer_capture(ev.pointer_id());
                        }
                        moved.set_value(false);
                        drag.set_value(Some((ev.pointer_id(), f64::from(ev.client_x()), now_width())));
                        dragging.set(true);
                    }
                    on:pointermove=move |ev| {
                        if drag.get_value().is_some_and(|(id, _, _)| id == ev.pointer_id()) {
                            follow(ev.client_x());
                        }
                    }
                    on:pointerup=move |ev| {
                        if drag.get_value().is_some_and(|(id, _, _)| id == ev.pointer_id()) {
                            follow(ev.client_x());
                            end_drag();
                        }
                    }
                    on:pointercancel=move |_| end_drag()
                    on:lostpointercapture=move |_| end_drag()
                    on:dblclick=move |_| reset()
                    on:keydown=move |ev| {
                        // Alt+Left is back, Ctrl+Home the top of the page:
                        // chords belong to the browser, not the edge.
                        if ev.ctrl_key() || ev.alt_key() || ev.meta_key() {
                            return;
                        }
                        let Some(m) = key_move(&ev.key(), ev.shift_key(), side, now_width(), bounds())
                        else {
                            return;
                        };
                        ev.prevent_default();
                        match m {
                            KeyMove::To(w) => keep(w),
                            KeyMove::Reset => reset(),
                        }
                    }
                ></div>
            </Show>
        }
    };
    let pill = move || badge.map(|b| b.get()).filter(|b| !b.is_empty());
    let panel = move || {
        let (l1, l2) = (l1.clone(), l2.clone());
        let side_view = side_view.clone();
        view! {
            <Show when=move || shown.get()>
                <aside
                    id=move || panel_id.get_value()
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
            <Show when=move || !docked.get() && !drawn.get()>
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
                {handle_view()}
                {rail_view()}
                {main}
            </div>
        }
        .into_any(),
        Side::Right => view! {
            <div class=class style=style node_ref=root>
                {main}
                {rail_view()}
                {handle_view()}
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
        assert!(!too_narrow(1224.0, DEFAULT_WIDTH, 560, None));
        // 760 − 220 = 540 < 560.
        assert!(too_narrow(760.0, DEFAULT_WIDTH, 560, None));
        // A lab's help panel folds below a 1600 px pane (1600 − 360 = 1240).
        assert!(too_narrow(1599.0, DEFAULT_WIDTH, 1240, None));
        assert!(!too_narrow(1600.0, DEFAULT_WIDTH, 1240, None));
        // Not measured yet: nothing to decide on.
        assert!(!too_narrow(0.0, DEFAULT_WIDTH, 560, None));
    }

    #[test]
    fn a_chosen_width_is_held_between_the_floor_and_the_main_panes_floor() {
        // Room for it: as chosen, past the automatic ceiling too.
        assert_eq!(docked_width(1400.0, DEFAULT_WIDTH, 560, Some(500.0)), 500.0);
        // The window narrowed: the main pane keeps its 560 (1000 − 560).
        assert_eq!(docked_width(1000.0, DEFAULT_WIDTH, 560, Some(500.0)), 440.0);
        // Never under the floor, whatever was stored.
        assert_eq!(docked_width(1400.0, DEFAULT_WIDTH, 560, Some(90.0)), 220.0);
        // Nothing chosen: the automatic share.
        assert_eq!(docked_width(1224.0, DEFAULT_WIDTH, 560, None), 293.76);
        assert_eq!(max_width(1400.0, DEFAULT_WIDTH, 560), 840.0);
        assert_eq!(max_width(700.0, DEFAULT_WIDTH, 560), 220.0);
        // A fractional split (125% zoom): the panel at its widest still docks.
        assert_eq!(max_width(827.2000122070312, DEFAULT_WIDTH, 560), 267.0);
        assert!(!too_narrow(
            827.2000122070312,
            DEFAULT_WIDTH,
            560,
            Some(375.0)
        ));
    }

    #[test]
    fn a_chosen_width_gives_way_down_to_the_floor_before_the_panel_folds() {
        // 800 − 240 = 560: the 500 chosen shrinks to 240 and still docks.
        assert!(!too_narrow(800.0, DEFAULT_WIDTH, 560, Some(500.0)));
        // 779 − 220 = 559: even the floor leaves the main pane short.
        assert!(too_narrow(779.0, DEFAULT_WIDTH, 560, Some(500.0)));
        // A lab panel chosen at 300 docks in a 1540 px split (1540 − 300 = 1240),
        // where the automatic 360 folds.
        assert!(!too_narrow(1540.0, DEFAULT_WIDTH, 1240, Some(300.0)));
        assert!(too_narrow(1540.0, DEFAULT_WIDTH, 1240, None));
    }

    #[test]
    fn a_drawer_is_its_share_held_between_its_floor_and_the_split() {
        // The Chat's thread list in a 338 px sidebar: its floor.
        assert_eq!(drawer_width(338.0, DEFAULT_WIDTH, None), 220.0);
        // The thread settings' floor fills a 280 px column.
        assert_eq!(drawer_width(280.0, (280, 30, 420), None), 280.0);
        // Never wider than the split, whatever was chosen on a wide window.
        assert_eq!(drawer_width(338.0, DEFAULT_WIDTH, Some(400.0)), 338.0);
        assert_eq!(drawer_width(500.0, DEFAULT_WIDTH, Some(300.0)), 300.0);
    }

    #[test]
    fn the_look_keeps_the_rail_beside_it() {
        assert_eq!(peek_width(700.0, DEFAULT_WIDTH, None), 220.0);
        assert_eq!(peek_width(700.0, DEFAULT_WIDTH, Some(500.0)), 500.0);
        assert_eq!(peek_width(400.0, DEFAULT_WIDTH, Some(500.0)), 372.0);
        assert_eq!(peek_width(200.0, DEFAULT_WIDTH, Some(500.0)), 220.0);
    }

    #[test]
    fn a_drag_follows_the_pointer_and_stops_at_the_bounds() {
        let b = (220.0, 640.0);
        assert_eq!(drag_width(Side::Left, 300.0, 40.0, b), 340.0);
        // A right panel grows as its edge moves left.
        assert_eq!(drag_width(Side::Right, 300.0, -40.0, b), 340.0);
        assert_eq!(drag_width(Side::Right, 300.0, 40.0, b), 260.0);
        // Past either bound it stays there.
        assert_eq!(drag_width(Side::Left, 300.0, -500.0, b), 220.0);
        assert_eq!(drag_width(Side::Left, 300.0, 900.0, b), 640.0);
        // Whole pixels, from an automatic width that was not one.
        assert_eq!(drag_width(Side::Left, 293.76, 10.4, b), 304.0);
    }

    #[test]
    fn the_handles_keys_move_the_edge() {
        let b = (220.0, 640.0);
        assert_eq!(
            key_move("ArrowRight", false, Side::Left, 300.0, b),
            Some(KeyMove::To(316.0))
        );
        assert_eq!(
            key_move("ArrowLeft", true, Side::Left, 300.0, b),
            Some(KeyMove::To(236.0))
        );
        // The edge of a right panel: left widens it.
        assert_eq!(
            key_move("ArrowLeft", false, Side::Right, 300.0, b),
            Some(KeyMove::To(316.0))
        );
        assert_eq!(
            key_move("ArrowRight", true, Side::Right, 300.0, b),
            Some(KeyMove::To(236.0))
        );
        assert_eq!(
            key_move("ArrowLeft", true, Side::Left, 230.0, b),
            Some(KeyMove::To(220.0))
        );
        assert_eq!(
            key_move("Home", false, Side::Right, 300.0, b),
            Some(KeyMove::To(220.0))
        );
        assert_eq!(
            key_move("End", false, Side::Left, 300.0, b),
            Some(KeyMove::To(640.0))
        );
        assert_eq!(
            key_move("Enter", false, Side::Left, 300.0, b),
            Some(KeyMove::Reset)
        );
        assert_eq!(key_move("a", false, Side::Left, 300.0, b), None);
    }
}
