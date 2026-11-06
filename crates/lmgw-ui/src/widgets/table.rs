//! Table helpers that sit on the table contract (UX plan §1.5, §2 #10):
//! group rows and the "Showing 10 of 55" line. The "⋯" row menu is the
//! kit's (`lmgw_ui_kit::widgets::RowMenu`), re-exported here.

// The area phases adopt these page by page; until then only the design
// sample shows them.
#![allow(dead_code)]

use leptos::prelude::*;

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

pub use lmgw_ui_kit::widgets::row_menu::{MenuItem, RowMenu};
