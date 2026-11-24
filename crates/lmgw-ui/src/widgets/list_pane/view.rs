//! The list pane's markup: the header bar, the search entry, the listbox,
//! a group's sticky heading, and what the list says while it loads, failed
//! or holds nothing. The classes are the ones the Models page gave them
//! (`mlist-head`, `msearch`, `mlist`), which the drives select by.

use leptos::prelude::*;

use super::nav::{ListNav, RowId};
use crate::frame::{End, WindowButtons};
use crate::widgets::nav_split::track_scroll;
use crate::widgets::{Ico, Icon};

/// The list's header bar: the title, a line under it, the actions; the
/// window buttons where the shell's frame wants them. `lead` goes before
/// the title (a back button).
#[component]
pub fn ListHead(
    /// The list scrolled: the bar draws its line.
    scrolled: RwSignal<bool>,
    narrow: Memo<bool>,
    title: &'static str,
    /// The line under the title (how many, how they are).
    #[prop(optional, into)]
    sub: Option<Signal<String>>,
    /// The line's tooltip, when it is not the line itself.
    #[prop(optional)]
    sub_title: Option<&'static str>,
    /// Under the title in place of a line (the Models filter's button).
    #[prop(optional, into)]
    under: Option<ViewFn>,
    #[prop(optional, into)] actions: Option<ViewFn>,
    /// Before the title, at the bar's start (a back button).
    #[prop(optional, into)]
    lead: Option<ViewFn>,
) -> impl IntoView {
    let rail = expect_context::<crate::shell::SidebarState>().rail;
    let sub = sub.map(|sub| {
        let tip = move || sub_title.map_or_else(|| sub.get(), str::to_string);
        view! { <span class="sub" title=tip>{move || sub.get()}</span> }
    });
    view! {
        <header class="page-head mlist-head" class:scrolled=move || scrolled.get() data-window-header>
            <Show when=move || rail.get()>
                <WindowButtons end=End::Start/>
            </Show>
            <div class="head-main">
                {lead.map(|l| l.run())}
                <div class="head-text">
                    <h1>{title}</h1>
                    {sub}
                    {under.map(|u| u.run())}
                </div>
                {actions.map(|a| view! { <div class="actions">{a.run()}</div> })}
            </div>
            <Show when=move || narrow.get()>
                <WindowButtons end=End::End/>
            </Show>
        </header>
    }
}

/// The search entry under the list's header bar.
#[component]
pub fn ListSearch<K: RowId>(
    nav: ListNav<K>,
    /// What the box holds (a URL query, or a box over one).
    query: RwSignal<String>,
    placeholder: &'static str,
    /// The tooltip: what it matches; the keys are said after it.
    #[prop(optional)]
    title: Option<&'static str>,
    /// Its name for assistive technology, when not the placeholder.
    #[prop(optional)]
    label: Option<&'static str>,
    /// Typing in it is no edit the unsaved-edits guard counts.
    #[prop(optional)]
    untracked: bool,
    /// Its keydown, when the page's differs from [`ListNav::on_search_key`].
    #[prop(optional)]
    on_key: Option<Callback<web_sys::KeyboardEvent>>,
) -> impl IntoView {
    let title = title.map(|t| format!("{t} · / or Ctrl+F focuses, Esc clears"));
    view! {
        <label class="msearch" class:has=move || !query.with(String::is_empty)>
            <Icon ico=Ico::Search/>
            <input
                node_ref=nav.search
                data-slash
                data-untracked=untracked.then_some("")
                type="search"
                placeholder=placeholder
                spellcheck="false"
                autocomplete="off"
                aria-label=label.unwrap_or(placeholder)
                title=title
                prop:value=move || query.get()
                on:input=move |ev| query.set(event_target_value(&ev))
                on:keydown=move |ev| match on_key {
                    Some(k) => k.run(ev),
                    None => nav.on_search_key(query, ev),
                }
            />
            <button
                type="button"
                class="msearch-clear"
                title="Clear"
                on:click=move |ev| {
                    ev.prevent_default();
                    query.set(String::new());
                    if let Some(i) = nav.search.get_untracked() {
                        let _ = i.focus();
                    }
                }
            >
                <Icon ico=Ico::Close/>
            </button>
        </label>
    }
}

/// The scrolling list itself: a listbox the keys work in.
#[component]
pub fn ListBox<K: RowId>(
    nav: ListNav<K>,
    label: &'static str,
    /// The page's own class beside `mlist` (`kb-list`).
    #[prop(optional)]
    class: &'static str,
    children: Children,
) -> impl IntoView {
    let class = if class.is_empty() {
        "mlist".to_string()
    } else {
        format!("mlist {class}")
    };
    view! {
        <div
            class=class
            node_ref=nav.list
            tabindex="0"
            role="listbox"
            aria-label=label
            on:keydown=move |ev| nav.on_list_key(ev)
            on:scroll=move |ev| track_scroll(&ev, nav.scrolled)
        >
            {children()}
        </div>
    }
}

/// A group's sticky heading, with how many rows it shows.
#[component]
pub fn GroupHead(
    #[prop(into)] label: String,
    #[prop(optional, into)] n: Option<Signal<usize>>,
) -> impl IntoView {
    view! {
        <div class="list-head" role="presentation">
            {label}
            {n.map(|n| view! { <span class="n">{move || n.get()}</span> })}
        </div>
    }
}

/// Why the list could not be read, with Retry: the list it had stays.
#[component]
pub fn ListNotice(
    #[prop(into)] error: Signal<Option<String>>,
    /// A list was read before (what is shown is the old one).
    #[prop(into)]
    stale: Signal<bool>,
    /// The words before the error on a first read ("Loading agents failed: ").
    failed: &'static str,
    /// And on a re-read.
    #[prop(optional)]
    refresh_failed: Option<&'static str>,
    retry: Callback<()>,
) -> impl IntoView {
    let refresh_failed = refresh_failed.unwrap_or("Refreshing the list failed: ");
    move || {
        error.get().map(|e| {
            view! {
                <div class="notice err mlist-notice">
                    {if stale.get() { refresh_failed } else { failed }}
                    {e}
                    <button class="btn sm" on:click=move |_| retry.run(())>
                        "Retry"
                    </button>
                </div>
            }
        })
    }
}

/// What the list says without rows: Loading… until it is read, then that
/// there is nothing yet (`none`), or what the search found nothing for.
#[component]
pub fn ListEmpty(
    /// Read (rows or none).
    #[prop(into)]
    loaded: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    /// Nothing is shown.
    #[prop(into)]
    empty: Signal<bool>,
    #[prop(into)] query: Signal<String>,
    /// Without a search ("No agents yet: add one with +").
    none: &'static str,
    /// Before the search's text in quotes ("No agents match").
    no_match: &'static str,
) -> impl IntoView {
    view! {
        <Show when=move || !loaded.get() && error.with(Option::is_none)>
            <div class="mlist-empty">"Loading…"</div>
        </Show>
        <Show when=move || loaded.get() && empty.get()>
            <div class="mlist-empty">
                {move || {
                    let q = query.get();
                    if q.trim().is_empty() {
                        none.to_string()
                    } else {
                        format!("{no_match} \u{201c}{}\u{201d}", q.trim())
                    }
                }}
            </div>
        </Show>
    }
}
