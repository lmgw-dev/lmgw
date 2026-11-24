//! The corpus list pane: the search and the filter (the badges an agent
//! sees on `docs__resolve`, as `?show=`), how many it shows, and a two-line
//! row per corpus (its `library@version`, its state, and its chunks,
//! resident size and embedding model).
//! The search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::CorpusView;

use super::{corpus_facets, corpus_matches, corpus_state, use_docs};
use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::list_pane::{ListBox, ListNav, ListNotice, ListRefs, ListSearch};
use crate::widgets::{filter_words, Ico, Icon, ListRow, Popover};

/// The filter's choices: (`?show=` value, label).
const FACETS: [(&str, &str); 5] = [
    ("", "All corpora"),
    ("re-embed", "Needs re-embed"),
    ("regression", "Eval regression"),
    ("unmeasured", "Unmeasured"),
    ("empty", "Empty"),
];

/// A row's second line.
pub(super) fn meta_of(c: &CorpusView) -> String {
    let mut parts = vec![
        format!("{} chunks", grouped(c.chunk_count.max(0) as u64)),
        fmt_bytes(c.resident_bytes.max(0) as u64),
    ];
    if !c.embed_identity.is_empty() {
        parts.push(c.embed_identity.clone());
    }
    parts.join(" · ")
}

#[component]
pub(super) fn ListPane(
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    refs: ListRefs<i64>,
    open_row: Callback<(i64, bool)>,
) -> impl IntoView {
    let docs = use_docs();
    let query = crate::url_state::use_query_signal("q");
    let facet = crate::url_state::use_query_signal("show");
    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let on = facet.get();
        docs.corpora.with(|cs| {
            cs.iter()
                .filter(|c| on.is_empty() || corpus_facets(c).contains(&on.as_str()))
                .filter(|c| corpus_matches(c, &words))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let order = Memo::new(move |_| shown.with(|s| s.iter().map(|c| c.id).collect::<Vec<_>>()));
    let counts = Memo::new(move |_| {
        docs.corpora.with(|cs| {
            FACETS
                .iter()
                .map(|(id, _)| {
                    if id.is_empty() {
                        cs.len()
                    } else {
                        cs.iter().filter(|c| corpus_facets(c).contains(id)).count()
                    }
                })
                .collect::<Vec<_>>()
        })
    });
    let summary = move || {
        if !docs.loaded.get() {
            return if docs.error.with(Option::is_some) {
                "not loaded".to_string()
            } else {
                "Loading…".to_string()
            };
        }
        let f = facet.get();
        let label = FACETS
            .iter()
            .find(|(id, _)| *id == f)
            .map_or("All corpora", |(_, l)| l);
        let n = shown.with(Vec::len);
        let total = docs.corpora.with(Vec::len);
        if n == total {
            format!("{label} · {n}")
        } else {
            format!("{label} · {n} of {total}")
        }
    };

    let nav = ListNav::new(refs, order, sel, narrow, open_row, |id: &i64| {
        format!("corpus-{id}")
    });
    let fopen = RwSignal::new(false);
    let fbtn: NodeRef<html::Button> = NodeRef::new();

    view! {
        <div class="docs-list-tools" class:scrolled=move || nav.scrolled.get()>
            <ListSearch
                nav=nav
                query=query
                placeholder="Search corpora"
                label="Search corpora by library, model or source"
            />
            <button
                type="button"
                class="filter-btn docs-filter"
                node_ref=fbtn
                aria-haspopup="menu"
                aria-expanded=move || fopen.get().to_string()
                title="Show the corpora with one badge"
                on:click=move |_| fopen.update(|o| *o = !*o)
            >
                <span>{summary}</span>
                <Icon ico=Ico::Down/>
            </button>
        </div>
        <Popover open=fopen anchor=fbtn class="menu-pop">
            <div class="pop-list menu-list" role="menu" tabindex="-1" data-autofocus>
                {FACETS
                    .into_iter()
                    .enumerate()
                    .map(|(i, (id, label))| {
                        view! {
                            {(i == 1).then(|| view! { <hr class="menu-sep"/> })}
                            <button
                                type="button"
                                class="menu-item"
                                class:checked=move || facet.get() == id
                                role="menuitemradio"
                                aria-checked=move || (facet.get() == id).to_string()
                                on:click=move |_| {
                                    facet.set(id.to_string());
                                    fopen.set(false);
                                }
                            >
                                <span class="menu-check"></span>
                                <span class="menu-label">{label}</span>
                                <span class="menu-count">
                                    {move || counts.with(|c| c.get(i).copied().unwrap_or(0))}
                                </span>
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </Popover>
        <ListNotice
            error=docs.error
            stale=docs.loaded
            failed="Loading the corpora failed: "
            refresh_failed="Refreshing the corpora failed: "
            retry=Callback::new(move |()| docs.bump())
        />
        <ListBox nav=nav label="Corpora" class="docs-list">
            <Show when=move || !docs.loaded.get() && docs.error.with(Option::is_none)>
                <div class="mlist-empty">"Loading…"</div>
            </Show>
            <Show when=move || docs.loaded.get() && shown.with(Vec::is_empty)>
                <div class="mlist-empty">
                    {move || {
                        if docs.corpora.with(Vec::is_empty) {
                            "No corpora yet: ingest a library with +, or import a corpus file from another machine.".to_string()
                        } else {
                            "No corpus matches the search or the filter.".to_string()
                        }
                    }}
                </div>
            </Show>
            <For each=move || shown.get() key=|c| c.id let:c>
                {
                    let id = c.id;
                    let first = StoredValue::new(c);
                    // The row reads its corpus from the list as it stands,
                    // so a re-read that changes it (its chunks, its size, its
                    // pin) shows without the row being rebuilt.
                    let row = Memo::new(move |_| {
                        docs.corpora
                            .with(|cs| cs.iter().find(|c| c.id == id).cloned())
                            .unwrap_or_else(|| first.get_value())
                    });
                    let st = Memo::new(move |_| {
                        let running = docs.has_job(id);
                        row.with(|c| corpus_state(c, running))
                    });
                    let name = Memo::new(move |_| row.with(|c| c.corpus_id.clone()));
                    let meta = Memo::new(move |_| row.with(meta_of));
                    let tip = move || format!("{}\n{}\n{}", name.get(), st.get().1, meta.get());
                    view! {
                        <ListRow
                            key=format!("corpus-{id}")
                            name=move || name.get()
                            meta=move || meta.get()
                            title=tip
                            dot=Signal::derive(move || st.get().0)
                            state=move || st.get().1
                            selected=Signal::derive(move || nav.lit(&id))
                            on_click=Callback::new(move |()| nav.click(id))
                        />
                    }
                }
            </For>
        </ListBox>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_says_its_size_and_pin() {
        let c = CorpusView {
            chunk_count: 1200,
            resident_bytes: 2048,
            embed_identity: "demo/embed@64".into(),
            ..Default::default()
        };
        assert_eq!(meta_of(&c), "1\u{202F}200 chunks · 2.0 KiB · demo/embed@64");
    }
}
