//! Settings — the category pane: the title with the version and the data
//! directory under it, the search, and the categories as list rows with
//! their glyphs. A category with unsaved edits says how many; during a
//! search each says how many of its settings match.
//!
//! In a narrow window, where the list and the detail are one pane at a
//! time, a search lists the matching settings themselves instead
//! (libadwaita's preferences search): a click opens the setting in its
//! category, scrolled to and flashed.
//!
//! The header, search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]); Enter in the search stays there.

use std::collections::BTreeMap;

use leptos::prelude::*;

use super::render::cat_icon;
use super::{anchor, fields, Ctl, Page, CATS, GROUPS};
use crate::widgets::list_pane::{ListBox, ListHead, ListNav, ListRefs, ListSearch};
use crate::widgets::ListRow;

#[component]
pub(super) fn CatList(
    page: Page,
    query: RwSignal<String>,
    /// The category the address names, `None` at `/settings`.
    picked: Memo<Option<&'static str>>,
    narrow: Memo<bool>,
    refs: ListRefs<&'static str>,
    dirty_by_cat: Memo<BTreeMap<&'static str, usize>>,
    matches_by_cat: Memo<BTreeMap<&'static str, usize>>,
    #[prop(into)] sub: Signal<String>,
    /// Open a category (a click, Enter, or the selection moving in a wide
    /// window): its slug, and whether the move replaces the history entry.
    open_cat: Callback<(&'static str, bool)>,
) -> impl IntoView {
    let order: Vec<&'static str> = CATS.iter().map(|(s, ..)| *s).collect();
    // The cursor follows the category the address names; a wide window
    // shows (and moves from) the page's category, Network at a bare
    // `/settings`. A narrow window's search lists the settings it finds,
    // which the keys do not move in.
    let nav = ListNav::new(
        refs,
        Signal::stored(order),
        picked,
        narrow,
        open_cat,
        |s: &&str| s.to_string(),
    )
    .wide_from(Signal::derive(move || Some(page.active.get())))
    .held(Signal::derive(move || page.searching()));
    // None is selected while a search shows every category's hits.
    let selected = move |slug: &'static str| !page.searching() && nav.lit(&slug);
    // Enter in the search stays there: the hits are in the detail.
    let on_search_key = Callback::new(move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Enter" {
            nav.on_search_key(query, ev);
        }
    });

    let cats = move || {
        CATS.iter()
            .map(|(slug, label, blurb)| {
                let slug: &'static str = slug;
                let dirty = move || dirty_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                let hits = move || matches_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                let state_text = move || {
                    if page.searching() {
                        hits().to_string()
                    } else if dirty() > 0 {
                        format!("{} unsaved", dirty())
                    } else {
                        String::new()
                    }
                };
                // A link, so a click is the router's (and the guard's) and a
                // middle-click opens a tab; the list keeps the one tab stop.
                view! {
                    <a
                        class="lrow-link"
                        href=format!("/settings/{slug}")
                        tabindex="-1"
                        title=super::prow::sentence(blurb)
                        on:click=move |_| nav.cursor.set(Some(slug))
                    >
                        <ListRow
                            key=slug.to_string()
                            name=*label
                            icon=cat_icon(slug)
                            state=state_text
                            selected=Signal::derive(move || selected(slug))
                            off=Signal::derive(move || page.searching() && hits() == 0)
                        />
                    </a>
                }
            })
            .collect_view()
    };
    // The narrow window's search: the matching settings, each a link into
    // its category.
    let results = move || {
        let hits: Vec<_> = fields().filter(|d| page.matches(d)).collect();
        if hits.is_empty() {
            return view! {
                <div class="mlist-empty">
                    {format!("No setting matches \u{201c}{}\u{201d}", query.get().trim())}
                </div>
            }
            .into_any();
        }
        hits.into_iter()
            .map(|d| {
                let cat = CATS
                    .iter()
                    .find(|(s, ..)| *s == d.cat)
                    .map_or("", |(_, l, _)| l);
                let group = GROUPS
                    .iter()
                    .find(|g| g.id == d.group)
                    .map_or("", |g| g.title);
                let href = if matches!(d.ctl, Ctl::MachineLink) {
                    format!("/settings/gpu#{}", super::MACHINE_ANCHOR)
                } else {
                    format!("/settings/{}#{}", d.cat, anchor(d.key))
                };
                // The address has no `?q=`: the search ends, and the
                // setting is scrolled to and flashed in its category.
                view! {
                    <a class="lrow-link" href=href tabindex="-1">
                        <ListRow
                            key=d.key.to_string()
                            name=d.label
                            icon=cat_icon(d.cat)
                            meta=format!("{cat} › {group}")
                        />
                    </a>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="Settings" sub=sub/>
        <ListSearch
            nav=nav
            query=query
            placeholder="Search settings"
            title="Search"
            untracked=true
            on_key=on_search_key
        />
        <ListBox nav=nav label="Settings categories" class="set-cats">
            {move || {
                if narrow.get() && page.searching() { results().into_any() } else { cats().into_any() }
            }}
        </ListBox>
    }
}
