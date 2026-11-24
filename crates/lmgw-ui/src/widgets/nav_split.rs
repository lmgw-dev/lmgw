//! A list beside its detail (modern-sample.html), libadwaita's navigation
//! split view: in a pane at least `narrow_below` px wide the two sit side by
//! side, parted by a handle that resizes the list (dragged, or with its arrow
//! keys; a double-click or Enter forgets the width), and the width is kept as
//! `lmgw.ui.<width_key>`. Narrower, one pane shows at a time: the list, or,
//! once something is opened (`has_sel`), its detail with a back button the
//! caller draws.
//!
//! The list's automatic width is a share of the pane between a floor and a
//! ceiling; a chosen one is held so the detail keeps `detail_min` px.
//!
//! Page keys, while no modal is open: Ctrl+F goes to the caller's search,
//! Ctrl+S runs the save the shown detail installed ([`SaveKey`]).
//!
//! The Models page and Settings are built on it; the pane classes keep the
//! names the Models page gave them (`mlist-pane`, `mdetail-pane`,
//! `mhandle`), which the drives select by.

use leptos::html;
use leptos::prelude::*;
use lmgw_ui_kit::element_size::use_element_size;
use wasm_bindgen::JsCast;

use super::split::{docked_width, drag_width, key_move, max_width, KeyMove, Side};

/// What one split view is: its page class, where its width is kept, and
/// its bounds.
#[derive(Clone, Copy)]
pub struct SplitConf {
    /// The page's own class beside `nav-split` (`models-split`).
    pub class: &'static str,
    /// Kept as `lmgw.ui.<width_key>`.
    pub width_key: &'static str,
    /// The list: (floor px, share of the pane in %, ceiling px).
    pub list_width: (u32, u32, u32),
    /// The detail's floor beside the list, which a chosen width keeps.
    pub detail_min: u32,
    /// Below this pane width the view is one pane at a time.
    pub narrow_below: f64,
    /// The handle's name for assistive technology ("Resize the list").
    pub handle_label: &'static str,
}

/// The measured pane, made before the panes are, since both read `narrow`.
#[derive(Clone, Copy)]
pub struct SplitState {
    conf: SplitConf,
    root: NodeRef<html::Div>,
    width_now: RwSignal<f64>,
    /// One pane at a time.
    pub narrow: Memo<bool>,
}

impl SplitState {
    pub fn new(conf: SplitConf) -> Self {
        let root: NodeRef<html::Div> = NodeRef::new();
        let observed = use_element_size(root);
        // The pane's width: what the ResizeObserver says, and what the box
        // measures on a window resize too — WebKitGTK can hold an
        // observation back until its next rendering update, and the layout
        // must not wait for that with the wrong number of panes.
        //
        // Until the box itself has been measured it starts from the content
        // area it is about to fill, else from the window's width, which the
        // pane never exceeds: a narrow window is one pane from its first
        // frame, and a wide one is not held blank meanwhile (a ResizeObserver
        // WebKitGTK delays, as under Broadway, left a narrow window showing
        // both panes until it answered).
        let width_now = RwSignal::new(first_guess());
        Effect::new(move |_| {
            let w = observed.get().0;
            if w > 0.0 && width_now.get_untracked() != w {
                width_now.set(w);
            }
        });
        let measure = move || {
            if let Some(el) = root.get_untracked() {
                let w = el.get_bounding_client_rect().width();
                if w > 0.0 && width_now.get_untracked() != w {
                    width_now.set(w);
                }
            }
        };
        Effect::new(move |_| {
            if root.get().is_some() {
                measure();
            }
        });
        let on_resize = window_event_listener(leptos::ev::resize, move |_| measure());
        on_cleanup(move || on_resize.remove());
        let narrow = Memo::new(move |_| {
            let w = width_now.get();
            w > 0.0 && w < conf.narrow_below
        });
        Self {
            conf,
            root,
            width_now,
            narrow,
        }
    }
}

/// The pane's width before its box is measured: the shell's content area
/// while a page replaces another there, else the window's (on a cold load the
/// shell is being built with the page), never more than the pane will have.
fn first_guess() -> f64 {
    let content = document()
        .query_selector("main.content")
        .ok()
        .flatten()
        .map_or(0.0, |el| el.get_bounding_client_rect().width());
    if content > 0.0 {
        return content;
    }
    window()
        .inner_width()
        .ok()
        .and_then(|w| w.as_f64())
        .unwrap_or(0.0)
}

/// What Ctrl+S does on the page: the shown detail's save, while it has one,
/// with the id of the detail that installed it.
#[derive(Clone, Copy)]
pub struct SaveKey(pub RwSignal<Option<(u64, Callback<()>)>>);

static NEXT_SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl SaveKey {
    /// Ctrl+S runs `save` while the calling component is mounted.
    pub fn install(save: Callback<()>) {
        let Some(key) = use_context::<SaveKey>() else {
            return;
        };
        let id = NEXT_SAVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        key.0.set(Some((id, save)));
        on_cleanup(move || {
            key.0.try_update(|k| {
                if k.is_some_and(|(i, _)| i == id) {
                    *k = None;
                }
            });
        });
    }
}

/// Provide the page's [`SaveKey`] before the details that install into it
/// are built.
pub fn provide_save_key() -> SaveKey {
    let key = SaveKey(RwSignal::new(None));
    provide_context(key);
    key
}

/// Ctrl+F (to `search`, unless `search_off`) and Ctrl+S (the installed
/// save), while the page is up and no modal is.
fn page_keys(
    save_key: SaveKey,
    search: Option<NodeRef<html::Input>>,
    search_off: Option<Signal<bool>>,
) {
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !(ev.ctrl_key() || ev.meta_key()) || ev.alt_key() || ev.shift_key() {
            return;
        }
        if matches!(document().query_selector("dialog[open]"), Ok(Some(_))) {
            return;
        }
        match ev.key().as_str() {
            "f" | "F" => {
                let Some(input) = search.and_then(|s| s.get_untracked()) else {
                    return;
                };
                if search_off.is_some_and(|o| o.get_untracked()) {
                    return;
                }
                ev.prevent_default();
                let _ = input.focus();
                input.select();
            }
            "s" | "S" => {
                let Some((_, save)) = save_key.0.get_untracked() else {
                    return;
                };
                ev.prevent_default();
                // The field being typed in commits first (its `change`).
                let active = document()
                    .active_element()
                    .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
                if let Some(a) = &active {
                    let _ = a.blur();
                }
                save.run(());
                if let Some(a) = active {
                    let _ = a.focus();
                }
            }
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());
}

#[component]
pub fn NavSplit(
    split: SplitState,
    /// Something is open: a narrow view shows the detail, not the list.
    #[prop(into)]
    has_sel: Signal<bool>,
    save_key: SaveKey,
    /// Where Ctrl+F goes.
    #[prop(optional)]
    search: Option<NodeRef<html::Input>>,
    /// Ctrl+F is the browser's while this holds (a narrow window showing a
    /// detail, whose list and search are hidden).
    #[prop(optional, into)]
    search_off: Option<Signal<bool>>,
    list: ViewFn,
    detail: ViewFn,
) -> impl IntoView {
    let SplitState {
        conf,
        root,
        width_now,
        narrow,
    } = split;
    let (lw, dmin) = (conf.list_width, conf.detail_min);
    page_keys(save_key, search, search_off);
    let size = move || width_now.get();

    // The list's width: a chosen one, else the automatic share.
    let chosen = RwSignal::new(crate::prefs::stored_number(conf.width_key).filter(|w| *w > 0.0));
    let width = move || docked_width(size(), lw, dmin, chosen.get());
    let bounds = move || {
        (
            f64::from(lw.0),
            max_width(width_now.get_untracked(), lw, dmin),
        )
    };
    let now_width =
        move || docked_width(width_now.get_untracked(), lw, dmin, chosen.get_untracked());
    let keep = move |w: f64| {
        chosen.set(Some(w));
        crate::prefs::store_number(conf.width_key, w);
    };
    let reset = move || {
        chosen.set(None);
        crate::prefs::forget(conf.width_key);
    };
    let drag = StoredValue::new(None::<(i32, f64, f64)>);
    let moved = StoredValue::new(false);
    let dragging = RwSignal::new(false);
    let follow = move |x: i32| {
        if let Some((_, x0, w0)) = drag.get_value() {
            let dx = f64::from(x) - x0;
            if dx != 0.0 {
                moved.set_value(true);
            }
            if moved.get_value() {
                chosen.set(Some(drag_width(Side::Left, w0, dx, bounds())));
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
    Effect::new(move |_| {
        if narrow.get() {
            end_drag();
        }
    });

    let handle = move || {
        view! {
            <div
                class="mhandle"
                role="separator"
                tabindex="0"
                aria-orientation="vertical"
                aria-label=conf.handle_label
                aria-valuemin=lw.0.to_string()
                aria-valuemax=move || format!("{:.0}", max_width(size(), lw, dmin))
                aria-valuenow=move || format!("{:.0}", width())
                aria-valuetext=move || {
                    if chosen.get().is_some() {
                        format!("{:.0} px", width())
                    } else {
                        format!("{:.0} px, automatic", width())
                    }
                }
                title="Drag to resize · double-click for the automatic width"
                on:pointerdown=move |ev| {
                    if ev.button() != 0 {
                        return;
                    }
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
                    if ev.ctrl_key() || ev.alt_key() || ev.meta_key() {
                        return;
                    }
                    let Some(m) = key_move(&ev.key(), ev.shift_key(), Side::Left, now_width(), bounds())
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
        }
    };

    view! {
        <div
            class=format!("page nav-split {}", conf.class)
            class:narrow=move || narrow.get()
            class:has-sel=move || has_sel.get()
            class:resizing=move || dragging.get()
            node_ref=root
            style=move || {
                if size() > 0.0 { format!("--list-w:{:.0}px", width()) } else { String::new() }
            }
        >
            <section class="mlist-pane">{list.run()}</section>
            <Show when=move || { !narrow.get() && size() > 0.0 }>{handle}</Show>
            <section class="mdetail-pane">{detail.run()}</section>
        </div>
    }
}

/// Note on `scrolled` whether the scroller under a header has moved: the
/// header then draws its line.
pub fn track_scroll(ev: &web_sys::Event, scrolled: RwSignal<bool>) {
    let top = ev
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .map_or(0, |el| el.scroll_top());
    if scrolled.get_untracked() != (top > 0) {
        scrolled.set(top > 0);
    }
}

/// The detail's header bar: in a narrow window the back button and the
/// title (the list is not beside it), the tabs if the detail has any, the
/// actions, the menu, and the window buttons at the end. In a wide window a
/// bar without tabs shows the title once the scroller has moved.
#[component]
pub fn DetailHead(
    narrow: Memo<bool>,
    back: Callback<()>,
    #[prop(into)] title: Signal<String>,
    scrolled: RwSignal<bool>,
    /// The back button's name ("Back to the list").
    #[prop(optional)]
    back_label: Option<&'static str>,
    /// Tabs after the title (the agent detail's Run, Runs, Definition).
    #[prop(optional, into)]
    tabs: Option<ViewFn>,
    #[prop(optional, into)] actions: Option<ViewFn>,
    #[prop(optional, into)] more: Option<Signal<Vec<super::MenuItem>>>,
) -> impl IntoView {
    use super::{Ico, Icon, RowMenu};
    use crate::frame::{End, WindowButtons};
    let rail = expect_context::<crate::shell::SidebarState>().rail;
    let back_label = back_label.unwrap_or("Back to the list");
    let has_tabs = tabs.is_some();
    // The scroller under the bar keeps its position when the detail changes
    // from one record to the next (the view is rebuilt over the same
    // elements), while `scrolled` starts again: read the position once the
    // bar is there, so the bar and the scroller agree from the start.
    let header = NodeRef::<html::Header>::new();
    Effect::new(move |_| {
        let Some(h) = header.get() else { return };
        request_animation_frame(move || {
            let top = h
                .parent_element()
                .and_then(|p| p.query_selector(".detail-scroll").ok().flatten())
                .map_or(0, |el| el.scroll_top());
            if scrolled.try_get_untracked() == Some(top <= 0) {
                scrolled.try_set(top > 0);
            }
        });
    });
    view! {
        <header
            node_ref=header
            class="page-head mdetail-head"
            class:scrolled=move || scrolled.get()
            data-window-header
        >
            <Show when=move || narrow.get() && rail.get()>
                <WindowButtons end=End::Start/>
            </Show>
            <div class="head-main">
                <Show when=move || narrow.get()>
                    <button
                        type="button"
                        class="icon-btn"
                        title=back_label
                        aria-label=back_label
                        on:click=move |_| back.run(())
                    >
                        <Icon ico=Ico::Back/>
                    </button>
                    <div class="head-text">
                        <h1 title=move || title.get()>{move || title.get()}</h1>
                    </div>
                </Show>
                // A wide window with no tabs: the name appears once the
                // hero has scrolled away (`scrolled` follows the live scroll
                // position). Hidden, it still takes its room, so the actions
                // stay at the end and the bar keeps its height.
                {(!has_tabs)
                    .then(|| {
                        view! {
                            <Show when=move || !narrow.get()>
                                <div class="head-text head-title-reveal" class:shown=move || scrolled.get()>
                                    <h1 title=move || title.get()>{move || title.get()}</h1>
                                </div>
                            </Show>
                        }
                    })}
                {tabs.as_ref().map(|t| view! { <div class="head-tabs">{t.run()}</div> })}
                <div class="actions">
                    {actions.as_ref().map(|a| a.run())}
                    {more.map(|m| view! { <RowMenu items=m icon=Ico::More title="More"/> })}
                </div>
            </div>
            <WindowButtons end=End::End/>
        </header>
    }
}
