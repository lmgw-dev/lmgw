//! The one filter row a long list gets (UX plan §2 #11): a search box, facet
//! chips with their counts, removable chips for filters set elsewhere, and a
//! count that always says how much is showing.

// The area phases adopt these page by page; until then only the design
// sample shows them.
#![allow(dead_code)]

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::fmt::{count_of, grouped};

/// One facet chip: "Offered 62".
#[derive(Clone, PartialEq, Debug)]
pub struct Facet {
    pub id: String,
    pub label: String,
    pub count: usize,
}

/// The facet chips and which is on. `active == ""` is "All", which the bar
/// renders itself (with the total), so `items` holds only the real facets.
#[derive(Clone, Copy)]
pub struct FacetSet {
    pub items: Signal<Vec<Facet>>,
    pub active: RwSignal<String>,
}

/// `.filter-bar`: search · facets · chips · extra · count.
///
/// The count reads "395 models" when nothing narrows the list, otherwise
/// "Showing 17 of 395 models · Clear". Clear empties the search, resets the
/// facet to All and calls `on_clear`, which is where a caller resets the
/// filters it owns (the ones behind `chips` and `extra`).
///
/// `/` focuses the search box when no field has focus and no modal is open
/// (the first search box on the page, if there are several — see
/// [`use_slash_focus`]); Esc in the box clears it, and a second Esc leaves
/// it.
#[component]
pub fn FilterBar(
    query: RwSignal<String>,
    #[prop(into)] placeholder: String,
    /// Rows left after every filter.
    #[prop(into)]
    shown: Signal<usize>,
    /// Rows before any filter.
    #[prop(into)]
    total: Signal<usize>,
    /// Plural, e.g. "models".
    #[prop(into)]
    noun: String,
    #[prop(optional)] facets: Option<FacetSet>,
    /// Filters set elsewhere (a clicked row, a URL), each with its remover.
    #[prop(optional, into)]
    chips: Option<Signal<Vec<(String, Callback<()>)>>>,
    /// Controls of the caller's own (a Select, a toggle) before the count.
    #[prop(optional, into)]
    extra: Option<ViewFn>,
    #[prop(optional, into)] on_clear: Option<Callback<()>>,
    /// Controls that belong to the search box itself, right after it (a
    /// "known values" button).
    #[prop(optional, into)]
    query_addon: Option<ViewFn>,
    /// Filters of the caller's own (in `extra`) are narrowing the list: the
    /// count offers Clear for them too.
    #[prop(optional, into)]
    active: Option<Signal<bool>>,
    /// Replaces "Showing 17 of 395 models" for a list with no total to count
    /// against — a paged log says how many rows it holds and what feeds it —
    /// or while there is nothing to count yet ("Loading…"). Empty: the count.
    #[prop(optional, into)]
    status: Option<Signal<String>>,
) -> impl IntoView {
    let input: NodeRef<html::Input> = NodeRef::new();
    let noun = StoredValue::new(noun);

    let narrowed = move || {
        !query.with(|q| q.trim().is_empty())
            || facets.is_some_and(|f| !f.active.with(String::is_empty))
            || chips.is_some_and(|c| c.with(|c| !c.is_empty()))
            || active.is_some_and(|a| a.get())
            || shown.get() != total.get()
    };
    let clear = move || {
        query.set(String::new());
        if let Some(f) = facets {
            f.active.set(String::new());
        }
        if let Some(cb) = on_clear {
            cb.run(());
        }
    };

    use_slash_focus(input);

    let q_input = view! {
            <input
                class="input filter-q"
                type="search"
                node_ref=input
                data-slash
                placeholder=placeholder
                autocomplete="off"
                spellcheck="false"
                title="Filter · / focuses, Esc clears"
                data-untracked
                prop:value=move || query.get()
                on:input=move |ev| query.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() != "Escape" {
                        return;
                    }
                    ev.prevent_default();
                    if query.with_untracked(|q| q.is_empty()) {
                        if let Some(el) = input.get_untracked() {
                            let _ = el.blur();
                        }
                    } else {
                        query.set(String::new());
                    }
                }
            />
    };
    // A button of the box's own (Traffic's known values ▾) sits inside its
    // right edge, like a Select's arrow, rather than beside it as a stray
    // glyph.
    let q_box = match query_addon {
        Some(a) => view! { <span class="filter-q-box">{q_input} {a.run()}</span> }.into_any(),
        None => q_input.into_any(),
    };

    view! {
        <div class="filter-bar">
            {q_box}
            {facets
                .map(|f| {
                    let chip = move |id: String, label: String, n: Signal<usize>| {
                        let on = {
                            let id = id.clone();
                            move || f.active.with(|a| *a == id)
                        };
                        view! {
                            <button
                                type="button"
                                class="facet"
                                class:on=on.clone()
                                aria-pressed=move || on().to_string()
                                on:click=move |_| f.active.set(id.clone())
                            >
                                {label}
                                <span class="count">{move || grouped(n.get() as u64)}</span>
                            </button>
                        }
                    };
                    view! {
                        <div class="facets" role="group">
                            {chip(String::new(), "All".to_string(), total)}
                            <For
                                each=move || f.items.get()
                                key=|x| (x.id.clone(), x.label.clone(), x.count)
                                let:x
                            >
                                {chip(x.id, x.label, Signal::stored(x.count))}
                            </For>
                        </div>
                    }
                })}
            {chips
                .map(|c| {
                    view! {
                        <For each=move || c.get() key=|(l, _)| l.clone() let:item>
                            <span class="chip filter-chip">
                                {item.0.clone()}
                                <button
                                    type="button"
                                    class="chip-x"
                                    title="Remove this filter"
                                    on:click=move |_| item.1.run(())
                                >
                                    "✕"
                                </button>
                            </span>
                        </For>
                    }
                })}
            {extra.map(|e| e.run())}
            <span class="filter-count">
                {move || {
                    if let Some(st) = status.map(|s| s.get()).filter(|s| !s.is_empty()) {
                        return st;
                    }
                    let (s, t) = (shown.get(), total.get());
                    let all = noun.with_value(|n| count_of(t, n));
                    if narrowed() {
                        format!("Showing {} of {all}", grouped(s as u64))
                    } else {
                        all
                    }
                }}
                <Show when=narrowed>
                    " · "
                    <button type="button" class="link-btn" on:click=move |_| clear()>
                        "Clear"
                    </button>
                </Show>
            </span>
        </div>
    }
}

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
