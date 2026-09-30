//! Table helpers that sit on the table contract (UX plan §1.5, §2 #10):
//! group rows, the "⋯" row menu, and the "Showing 10 of 55" line.

// The area phases adopt these page by page; until then only the design
// sample shows them.
#![allow(dead_code)]

use leptos::html;
use leptos::prelude::*;

use super::confirm::is_second_press;
use super::popover::{self, Popover};
use crate::fmt::grouped;

/// The line under a list that is only partly shown: "Showing 10 of 55
/// aliases · Show all 55", and "All 55 aliases · Show fewer" once expanded.
///
/// `shown` is what the list renders — `step`, or `usize::MAX` for all of it —
/// so a caller writes `.take(shown.get())` and a list that grows while
/// expanded stays expanded. Nothing is rendered while the whole list fits in
/// `step`: nothing is held back then, so there is nothing to count.
///
/// `persist` keeps the choice across reloads as `lmgw.ui.showall.<persist>`.
#[component]
pub fn ShowMore(
    #[prop(into)] total: Signal<usize>,
    shown: RwSignal<usize>,
    #[prop(default = 10)] step: usize,
    /// Plural, e.g. "aliases".
    #[prop(into)]
    noun: String,
    #[prop(optional)] persist: Option<&'static str>,
) -> impl IntoView {
    let all = match persist {
        Some(p) => crate::prefs::persisted_bool(&format!("showall.{p}"), false),
        None => RwSignal::new(false),
    };
    let cap = move |all: bool| if all { usize::MAX } else { step };
    // Set before the first paint, so a stored "all" never flashes the short
    // list; the effect keeps it in step afterwards.
    shown.set(cap(all.get_untracked()));
    Effect::new(move |_| shown.set(cap(all.get())));
    let noun = StoredValue::new(noun);
    let held_back = move || total.get() > step;
    view! {
        <Show when=held_back>
            <div class="show-more">
                <span>
                    {move || {
                        let n = grouped(total.get() as u64);
                        let noun = noun.get_value();
                        if all.get() {
                            format!("All {n} {noun}")
                        } else {
                            format!("Showing {} of {n} {noun}", grouped(step as u64))
                        }
                    }}
                </span>
                <button type="button" class="link-btn" on:click=move |_| all.update(|a| *a = !*a)>
                    {move || {
                        if all.get() {
                            "Show fewer".to_string()
                        } else {
                            format!("Show all {}", grouped(total.get() as u64))
                        }
                    }}
                </button>
            </div>
        </Show>
    }
}

/// A group header row: `tr.group` with a caret, the label, its count and
/// optional meta/actions. The caller renders the group's own rows under
/// `<Show when=open>` — the count stays in the header either way, so a folded
/// group still says how much it holds.
///
/// For a remembered state pass `open` from
/// `prefs::persisted_bool("open.<page>.<group>", true)`.
#[component]
pub fn GroupRow(
    /// The table's column count.
    colspan: u32,
    #[prop(into)] label: TextProp,
    #[prop(optional, into)] count: Option<Signal<String>>,
    open: RwSignal<bool>,
    /// Dim text after the count ("57 tools · 11 on · 46 off").
    #[prop(optional, into)]
    meta: Option<ViewFn>,
    /// Right-aligned controls ("Enable all").
    #[prop(optional, into)]
    actions: Option<ViewFn>,
) -> impl IntoView {
    view! {
        <tr class="group" class:closed=move || !open.get()>
            <td colspan=colspan>
                <div class="group-cell">
                    <button
                        type="button"
                        class="group-toggle"
                        aria-expanded=move || open.get().to_string()
                        on:click=move |_| open.update(|o| *o = !*o)
                    >
                        <span class="caret-icon" aria-hidden="true">"▸"</span>
                        <span class="group-label">{move || label.get()}</span>
                    </button>
                    {count.map(|c| view! { <span class="count">{move || c.get()}</span> })}
                    {meta.map(|m| view! { <span class="group-meta">{m.run()}</span> })}
                    {actions.map(|a| view! { <span class="group-actions">{a.run()}</span> })}
                </div>
            </td>
        </tr>
    }
}

/// One entry of a [`RowMenu`].
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub on_select: Callback<()>,
    /// Destructive: the first pick only arms it ("Delete? ✓"), the second
    /// runs it (UX plan §4: nothing destructive is one click).
    pub danger: bool,
    pub disabled: bool,
    /// Why it is disabled, or what it does.
    pub title: Option<String>,
}

#[allow(dead_code)] // the area phases build their menus with these
impl MenuItem {
    pub fn new(label: impl Into<String>, on_select: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            label: label.into(),
            on_select: Callback::new(move |()| on_select()),
            danger: false,
            disabled: false,
            title: None,
        }
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// The "⋯" button that holds a row's secondary actions (UX plan §4: at most
/// one visible row button, the rest in here). The menu is a [`Popover`], so
/// it opens over the table card's edge instead of being cut off by it.
///
/// Keyboard: ↑↓ Home End move, Enter/Space picks, Esc and Tab close. A
/// danger item asks in place: its first pick turns it into "<label>? ✓", the
/// second runs it; closing the menu disarms it. The second press of a
/// double-click (or a held Enter) is not that second pick. Something that
/// deletes what a modal should explain first (a model, an upstream) is a
/// plain "Delete…" item that opens that modal instead.
#[component]
pub fn RowMenu(
    #[prop(into)] items: Signal<Vec<MenuItem>>,
    /// The button's tooltip.
    #[prop(default = "More actions")]
    title: &'static str,
    /// Words instead of "⋯", for a menu that is an action of its own ("+ add
    /// file role (9 more)") rather than a row's overflow.
    #[prop(optional, into)]
    label: Option<Signal<String>>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let active = RwSignal::new(None::<usize>);
    let armed = RwSignal::new(None::<usize>);
    let armed_at = StoredValue::new(0.0_f64);
    let was_open = StoredValue::new(false);

    Effect::new(move |_| {
        if !open.get() {
            armed.set(None);
            active.set(None);
        }
    });

    let enabled = move |i: usize| items.with_untracked(|v| v.get(i).is_some_and(|m| !m.disabled));
    let step = move |from: Option<usize>, down: bool| {
        let n = items.with_untracked(Vec::len);
        let mut i = from;
        for _ in 0..n {
            let next = match (i, down) {
                (None, true) => 0,
                (None, false) => n - 1,
                (Some(k), true) => (k + 1) % n,
                (Some(k), false) => (k + n - 1) % n,
            };
            if enabled(next) {
                return Some(next);
            }
            i = Some(next);
        }
        None
    };
    let focus_btn = move || {
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
        }
    };
    // `detail`: the click count of a mouse pick, 0 from the keyboard.
    let choose = move |i: usize, detail: i32| {
        let Some(item) = items.with_untracked(|v| v.get(i).cloned()) else {
            return;
        };
        if item.disabled {
            return;
        }
        if item.danger {
            if armed.get_untracked() != Some(i) {
                armed_at.set_value(js_sys::Date::now());
                armed.set(Some(i));
                active.set(Some(i));
                return;
            }
            if is_second_press(detail, js_sys::Date::now() - armed_at.get_value()) {
                return;
            }
        }
        open.set(false);
        focus_btn();
        item.on_select.run(());
    };
    let set_active = move |i: Option<usize>| {
        active.set(i);
        if let (Some(i), Some(l)) = (i, list.get_untracked()) {
            popover::reveal_child(&l, i);
        }
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let cur = active.get_untracked();
        match ev.key().as_str() {
            "ArrowDown" => set_active(step(cur, true)),
            "ArrowUp" => set_active(step(cur, false)),
            "Home" => set_active(step(None, true)),
            "End" => set_active(step(None, false)),
            "Enter" | " " => {
                if let Some(i) = cur {
                    if !ev.repeat() {
                        choose(i, 0);
                    }
                }
            }
            "Escape" => {
                ev.stop_propagation();
                open.set(false);
                focus_btn();
            }
            "Tab" => {
                open.set(false);
                focus_btn();
                return;
            }
            _ => return,
        }
        ev.prevent_default();
    };

    view! {
        <span class="row-menu">
            <button
                type="button"
                class=if label.is_some() { "btn ghost sm" } else { "btn ghost sm row-menu-btn" }
                node_ref=btn
                title=title
                aria-haspopup="menu"
                aria-expanded=move || open.get().to_string()
                on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    open.set(reopen && !open.get_untracked());
                }
                on:keydown=move |ev| {
                    if !open.get_untracked() && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            >
                {move || label.map(|l| l.get()).unwrap_or_else(|| "⋯".to_string())}
            </button>
            <Popover open=open anchor=btn class="menu-pop">
                <div
                    class="pop-list menu-list"
                    role="menu"
                    tabindex="-1"
                    node_ref=list
                    data-autofocus
                    on:keydown=on_key
                >
                    {move || {
                        items
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, m)| {
                                let is_armed = move || armed.get() == Some(i);
                                let label = m.label.clone();
                                view! {
                                    <div
                                        class="menu-item"
                                        class:danger=m.danger
                                        class:armed=is_armed
                                        class:active=move || active.get() == Some(i)
                                        class:disabled=m.disabled
                                        role="menuitem"
                                        aria-disabled=m.disabled.then_some("true")
                                        title=m.title.clone()
                                        on:pointermove=move |_| {
                                            if !m.disabled && active.get_untracked() != Some(i) {
                                                active.set(Some(i));
                                            }
                                        }
                                        on:click=move |ev| choose(i, ev.detail())
                                    >
                                        {move || {
                                            if is_armed() {
                                                format!("{label}? ✓")
                                            } else {
                                                label.clone()
                                            }
                                        }}
                                    </div>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Popover>
        </span>
    }
}
