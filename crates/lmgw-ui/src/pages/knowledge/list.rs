//! The list pane: a header bar with the title, what the list holds and New
//! knowledge base; the search; a two-line row per base (its name, its state
//! as a dot and a word, and its files, size, chunks and embedding model).
//! The header, search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::prelude::*;

use super::{status_of, use_kb_page, Kb};
use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::list_pane::{
    ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{filter_words, Ico, Icon, ListRow};

/// Does the base match the words typed? Its name, description and model.
fn matches(kb: &Kb, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {}",
        kb.name, kb.description, kb.embed_alias, kb.embed_identity
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// A row's second line.
pub(super) fn meta_of(kb: &Kb) -> String {
    let c = &kb.counts;
    let mut parts = vec![
        format!(
            "{} file{}",
            grouped(c.files.max(0) as u64),
            if c.files == 1 { "" } else { "s" }
        ),
        fmt_bytes(c.bytes.max(0) as u64),
        format!("{} chunks", grouped(c.chunks.max(0) as u64)),
    ];
    if !kb.embed_alias.is_empty() {
        parts.push(kb.embed_alias.clone());
    }
    if !kb.embed_resolvable {
        parts.push("model gone".into());
    }
    parts.join(" · ")
}

/// A row's tooltip: its name, its state and why, and its facts in full,
/// as both lines ellipsize in a narrow list.
fn tip_of(kb: &Kb, word: &str, why: &str) -> String {
    let state = if why.is_empty() {
        word.to_string()
    } else {
        format!("{word}: {why}")
    };
    [kb.name.as_str(), state.as_str(), meta_of(kb).as_str()].join("\n")
}

#[component]
pub(super) fn ListPane(
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    refs: ListRefs<i64>,
    open_row: Callback<(i64, bool)>,
) -> impl IntoView {
    let page = use_kb_page();
    let query = crate::url_state::use_query_signal("q");
    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        page.bases.with(|bs| {
            bs.iter()
                .filter(|k| matches(k, &words))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let order = Memo::new(move |_| shown.with(|s| s.iter().map(|k| k.id).collect::<Vec<_>>()));
    let sub = move || {
        if !page.loaded.get() {
            return if page.error.with(Option::is_some) {
                "not loaded".to_string()
            } else {
                "Loading…".to_string()
            };
        }
        page.bases.with(|bs| {
            let files: i64 = bs.iter().map(|k| k.counts.files).sum();
            let n = bs.len();
            format!(
                "{n} base{} · {} file{}",
                if n == 1 { "" } else { "s" },
                grouped(files.max(0) as u64),
                if files == 1 { "" } else { "s" }
            )
        })
    };

    let nav = ListNav::new(refs, order, sel, narrow, open_row, |id: &i64| {
        format!("kb-{id}")
    });
    let new_base = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="icon-btn"
                title="New knowledge base"
                aria-label="New knowledge base"
                on:click=move |_| page.new_open.set(true)
            >
                <Icon ico=Ico::Plus/>
            </button>
        }
    });

    view! {
        <ListHead
            scrolled=nav.scrolled
            narrow=narrow
            title="Knowledge"
            sub=Signal::derive(sub)
            sub_title="Your documents, in collections: searched from Chat and served on /mcp"
            actions=new_base
        />
        <ListSearch nav=nav query=query placeholder="Search knowledge bases"/>
        <ListNotice
            error=page.error
            stale=page.loaded
            failed="Loading knowledge bases failed: "
            retry=Callback::new(move |()| page.bump())
        />
        <ListBox nav=nav label="Knowledge bases" class="kb-list">
            <ListEmpty
                loaded=page.loaded
                error=page.error
                empty=Signal::derive(move || shown.with(Vec::is_empty))
                query=query
                none="No knowledge bases yet: make one with +, then drop your files into it."
                no_match="No knowledge base matches"
            />
            <For each=move || shown.get() key=|k| k.id let:k>
                {
                    let id = k.id;
                    let first = StoredValue::new(k);
                    // The row reads its base from the list as it stands, so
                    // a re-read that changes it (its counts, its model, its
                    // last job) shows without the row being rebuilt; the
                    // base it was made from covers the moment it leaves.
                    let kb = Memo::new(move |_| {
                        page.bases
                            .with(|bs| bs.iter().find(|b| b.id == id).cloned())
                            .unwrap_or_else(|| first.get_value())
                    });
                    let st = Memo::new(move |_| {
                        let job = page.job(id);
                        kb.with(|k| status_of(k, job.as_ref()))
                    });
                    let name = Memo::new(move |_| kb.with(|k| k.name.clone()));
                    let meta = Memo::new(move |_| kb.with(meta_of));
                    let tip = move || st.with(|s| kb.with(|k| tip_of(k, &s.1, &s.2)));
                    view! {
                        <ListRow
                            key=format!("kb-{id}")
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
    fn a_row_says_its_size_and_model() {
        let mut k = Kb {
            embed_alias: "embed-demo".into(),
            embed_resolvable: true,
            ..Default::default()
        };
        k.counts.files = 1;
        k.counts.bytes = 2048;
        k.counts.chunks = 12;
        assert_eq!(meta_of(&k), "1 file · 2.0 KiB · 12 chunks · embed-demo");
        k.embed_resolvable = false;
        assert!(meta_of(&k).ends_with("model gone"));
        assert!(matches(&k, &["embed".into()]));
        assert!(!matches(&k, &["nope".into()]));
    }

    #[test]
    fn a_tooltip_says_the_row_in_full() {
        let k = Kb {
            name: "Taxes".into(),
            embed_alias: "embed-demo".into(),
            embed_resolvable: true,
            ..Default::default()
        };
        assert_eq!(
            tip_of(&k, "Re-embed required", "some chunks have no vector"),
            format!(
                "Taxes\nRe-embed required: some chunks have no vector\n{}",
                meta_of(&k)
            )
        );
        assert!(tip_of(&k, "Empty", "").contains("\nEmpty\n"));
    }
}
