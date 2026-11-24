//! The selected corpus as a preferences page: the hero (its
//! `library@version`, sources, and chips for its state, the badges
//! `docs__resolve` reports, chunks, resident size and score), its warnings,
//! then boxed groups — the job running against it and Re-ingest, the two
//! models it is a function of and Re-embed, its eval (score, best, when,
//! Run eval, the eval tab and the playground for it), its documents
//! (Browse, Export) and Delete. Every action that starts a job waits while
//! one runs. With nothing selected it is a status page with the ways to add
//! a corpus.
//!
//! The corpus comes from the page's list, so a selection that moves never
//! waits for, or shows, an answer about the corpus before it.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::CorpusView;
use serde_json::json;

use super::{
    corpus_state, docs_href, download, score_words, start_job, use_docs, CorpusJobRow, WizardSeed,
};
use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::nav_split::track_scroll;
use crate::widgets::{ActionRow, Dot, Ico, Icon, InfoRow, PrefGroup, StateChip};

#[component]
pub(super) fn DetailPane(
    raw: Memo<Option<String>>,
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let docs = use_docs();
    let corpus = Memo::new(move |_| {
        let id = sel.get()?;
        docs.corpora
            .with(|cs| cs.iter().find(|c| c.id == id).cloned())
    });
    // Whether it is there, apart: a re-read that changes the corpus must not
    // build its detail anew.
    let present = Memo::new(move |_| corpus.with(Option::is_some));
    let scrolled = RwSignal::new(false);
    view! {
        <Show when=move || narrow.get()>
            <div class="docs-back" class:scrolled=move || scrolled.get()>
                <button type="button" class="btn flat" on:click=move |_| back.run(())>
                    <Icon ico=Ico::Back/>
                    "Corpora"
                </button>
            </div>
        </Show>
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            {move || {
                match (raw.with(Option::is_some), sel.get()) {
                    (false, _) => view! { <StatusPage/> }.into_any(),
                    (true, Some(id)) => {
                        if present.get() {
                            view! { <CorpusDetail id=id corpus=corpus/> }.into_any()
                        } else if docs.loaded.get() {
                            view! { <Gone back=back/> }.into_any()
                        } else {
                            view! { <div class="mdetail-empty">"Loading…"</div> }.into_any()
                        }
                    }
                    (true, None) => view! { <Gone back=back/> }.into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn CorpusDetail(id: i64, corpus: Memo<Option<CorpusView>>) -> impl IntoView {
    let docs = use_docs();
    let navigate = StoredValue::new(use_navigate());
    let running = Memo::new(move |_| docs.has_job(id));
    let c = move || corpus.get().unwrap_or_default();
    let field = move |f: fn(&CorpusView) -> String| {
        Signal::derive(move || corpus.with(|c| c.as_ref().map(f).unwrap_or_default()))
    };
    let has_chunks =
        Signal::derive(move || corpus.with(|c| c.as_ref().is_some_and(|c| c.chunk_count > 0)));
    let go = move |tab: &'static str| {
        docs.selected.set(id.to_string());
        navigate.with_value(|n| n(&docs_href(tab), Default::default()));
    };
    let ingest = move || {
        start_job(
            format!("/api/docs/corpora/{id}/ingest"),
            json!({}),
            move || docs.bump(),
        )
    };
    let eval = move || {
        start_job(
            "/api/docs/eval".to_string(),
            json!({ "corpus_id": id }),
            move || docs.bump(),
        )
    };
    let busy_sub = move |idle: &'static str| {
        move || {
            if running.get() {
                "A job is running against it; this waits until it ends".to_string()
            } else {
                idle.to_string()
            }
        }
    };

    view! {
        <div class="detail-flow docs-detail">
            <Hero corpus=corpus running=running/>
            {move || {
                let w = corpus.with(|c| c.as_ref().map(|c| (c.warnings.clone(), c.unembedded_chunks)).unwrap_or_default());
                (!w.0.is_empty() || w.1 > 0)
                    .then(|| {
                        view! {
                            <div class="kb-hero-notes">
                                {w.0.into_iter().map(|n| view! { <div class="notice warn">{n}</div> }).collect_view()}
                                {(w.1 > 0)
                                    .then(|| {
                                        view! {
                                            <div class="notice">
                                                {grouped(w.1 as u64)} " chunks have no vector yet: the vector stage sees only part of the corpus."
                                            </div>
                                        }
                                    })}
                            </div>
                        }
                    })
            }}
            <PrefGroup title="Ingest">
                <CorpusJobRow id=id/>
                <InfoRow title="Status" value=field(|c| c.status.clone())/>
                <InfoRow
                    title="Crawled"
                    value=field(|c| if c.crawl_date.is_empty() { "never".into() } else { c.crawl_date.clone() })
                />
                {move || {
                    corpus
                        .with(|c| c.as_ref().map(|c| c.sources.clone()).unwrap_or_default())
                        .into_iter()
                        .map(|s| {
                            let root = s.root.clone();
                            view! {
                                <div class="prow docs-source">
                                    <div class="prow-text">
                                        <span class="prow-title">"Source"</span>
                                        <span class="prow-sub mono-sub" title=root.clone()>{root.clone()}</span>
                                    </div>
                                    <span class="chip">{s.kind.clone()}</span>
                                </div>
                            }
                        })
                        .collect_view()
                }}
                <ActionRow
                    title=move || if has_chunks.get() { "Re-ingest" } else { "Ingest" }
                    sub=busy_sub("Fetch its sources again; unchanged pages never re-run the model")
                >
                    <button
                        type="button"
                        class="btn"
                        disabled=move || running.get()
                        on:click=move |_| ingest()
                    >
                        {move || if has_chunks.get() { "Re-ingest" } else { "Ingest" }}
                    </button>
                </ActionRow>
            </PrefGroup>
            <PrefGroup title="Models">
                <InfoRow title="Embedding" value=field(|c| c.embed_identity.clone()) mono=true/>
                <InfoRow
                    title="Extraction"
                    value=field(|c| {
                        if c.ingest_prompt_version.is_empty() {
                            c.ingest_model.clone()
                        } else {
                            format!("{} · prompt {}", c.ingest_model, c.ingest_prompt_version)
                        }
                    })
                    mono=true
                />
                <ActionRow
                    title="Re-embed…"
                    sub=busy_sub("Fill in the chunks that have no vector, or move it onto another embedding model")
                >
                    <button
                        type="button"
                        class="btn"
                        disabled=move || running.get()
                        on:click=move |_| docs.reembed.set(corpus.get_untracked())
                    >
                        "Re-embed…"
                    </button>
                </ActionRow>
            </PrefGroup>
            <PrefGroup title="Eval">
                <InfoRow title="Latest" value=Signal::derive(move || score_words(&c(), 3))/>
                <InfoRow
                    title="Best"
                    value=field(|c| c.eval_best.map(|b| format!("{b:.3}")).unwrap_or_else(|| "no best yet".into()))
                />
                <InfoRow title="Measured" value=field(|c| c.eval_at.clone().unwrap_or_else(|| "never".into()))/>
                <ActionRow title="Run eval" sub=busy_sub("Score its golden queries now")>
                    <button
                        type="button"
                        class="btn"
                        disabled=move || running.get()
                        on:click=move |_| eval()
                    >
                        "Run eval"
                    </button>
                </ActionRow>
                <ActionRow
                    title="Golden queries and eval"
                    sub="What it is asked, what is proposed, how it went"
                    chevron=true
                    on_click=Callback::new(move |()| go("eval"))
                />
                <ActionRow
                    title="Search it in the playground"
                    sub="Every stage's candidates, and what a docs__query caller receives"
                    chevron=true
                    on_click=Callback::new(move |()| go("playground"))
                />
            </PrefGroup>
            <PrefGroup title="Documents">
                <ActionRow
                    title="Browse documents and chunks"
                    sub=move || {
                        if has_chunks.get() {
                            "Its documents and the chunks cut from each, verbatim".to_string()
                        } else {
                            "Nothing to browse: it has no chunks yet".to_string()
                        }
                    }
                    chevron=true
                    on_click=Callback::new(move |()| {
                        if has_chunks.get_untracked() {
                            docs.browse.set(corpus.get_untracked());
                        }
                    })
                />
                <ActionRow
                    title="Export corpus file"
                    sub="A SQLite file holding this corpus alone"
                    on_click=Callback::new(move |()| {
                        let label = corpus.with_untracked(|c| c.as_ref().map(|c| c.corpus_id.clone()).unwrap_or_default());
                        let file = format!("{}.db", label.replace(['/', '@'], "-"));
                        download(&format!("/api/docs/export?corpus_id={id}"), &file)
                    })
                >
                    <Icon ico=Ico::Download class="prow-chevron"/>
                </ActionRow>
            </PrefGroup>
            <PrefGroup>
                <ActionRow
                    title="Delete corpus…"
                    sub=busy_sub("Its documents, chunks, vectors, golden queries and eval history")
                    danger=true
                    on_click=Callback::new(move |()| {
                        if !running.get_untracked() {
                            docs.deleting.set(corpus.get_untracked());
                        }
                    })
                />
            </PrefGroup>
        </div>
    }
}

/// The corpus's name, sources and chips.
#[component]
fn Hero(corpus: Memo<Option<CorpusView>>, running: Memo<bool>) -> impl IntoView {
    view! {
        {move || {
            corpus
                .get()
                .map(|c| {
                    let (dot, word) = corpus_state(&c, running.get());
                    let warn = c.warnings.join("\n");
                    let sources = if c.sources.is_empty() {
                        if c.source_kind.is_empty() {
                            "No sources".to_string()
                        } else {
                            format!("Source kind {}", c.source_kind)
                        }
                    } else {
                        c.sources
                            .iter()
                            .map(|s| s.root.clone())
                            .collect::<Vec<_>>()
                            .join(" · ")
                    };
                    let score = score_words(&c, 2);
                    view! {
                        <div class="detail-hero">
                            <div class="glyph">
                                <Icon ico=Ico::Book/>
                            </div>
                            <div class="hero-text">
                                <h1>{c.corpus_id.clone()}</h1>
                                <div class="sub mono-sub" title=sources.clone()>{sources.clone()}</div>
                                <div class="chips">
                                    <StateChip dot=dot word=word title=warn.clone()/>
                                    // The two badges an agent sees on
                                    // docs__resolve, beside the state.
                                    {(c.embed_status == "re_embed_required" && dot != Dot::Error)
                                        .then(|| view! { <span class="chip err" title=warn.clone()>"re-embed required"</span> })}
                                    {(c.eval_status == "regression")
                                        .then(|| view! { <span class="chip warn" title=warn.clone()>"eval regression"</span> })}
                                    {(c.eval_status == "unmeasured")
                                        .then(|| view! { <span class="chip off" title="no golden queries have been run against it">"unmeasured"</span> })}
                                    <span class="chip">{format!("{} chunks", grouped(c.chunk_count.max(0) as u64))}</span>
                                    <span class="chip" title="what serving it costs once resident">
                                        {format!("{} resident", fmt_bytes(c.resident_bytes.max(0) as u64))}
                                    </span>
                                    <span class="chip">{score}</span>
                                </div>
                            </div>
                        </div>
                    }
                })
        }}
    }
}

/// Nothing selected: what the corpora hold, and the ways to add one.
#[component]
fn StatusPage() -> impl IntoView {
    let docs = use_docs();
    let pending = super::use_docs_pending();
    let navigate = StoredValue::new(use_navigate());
    let sum = move |f: fn(&CorpusView) -> i64| {
        docs.corpora
            .with(|cs| cs.iter().map(f).sum::<i64>().max(0) as u64)
    };
    view! {
        <div class="detail-flow docs-status">
            <div class="detail-hero">
                <div class="glyph">
                    <Icon ico=Ico::Book/>
                </div>
                <div class="hero-text">
                    <h1>"Docs"</h1>
                    <div class="sub">
                        "Library documentation agents look up through docs__query. Pick a corpus in the list, or add one."
                    </div>
                </div>
            </div>
            <PrefGroup title="In the list">
                <InfoRow title="Corpora" value=move || grouped(docs.corpora.with(Vec::len) as u64)/>
                <InfoRow title="Chunks" value=move || grouped(sum(|c| c.chunk_count))/>
                <InfoRow title="Resident" value=move || fmt_bytes(sum(|c| c.resident_bytes))/>
                {move || match docs.rerank_model.get() {
                    Some(model) => {
                        view! { <InfoRow title="Rerank stage" value=model mono=true/> }.into_any()
                    }
                    None => view! { <InfoRow title="Rerank stage" value="None"/> }.into_any(),
                }}
                <Show when=move || { pending.0.get() > 0 }>
                    <ActionRow
                        title="Requests waiting"
                        sub=move || format!("{} filed by agents through docs__request", pending.0.get())
                        chevron=true
                        on_click=Callback::new(move |()| navigate.with_value(|n| n(&docs_href("requests"), Default::default())))
                    />
                </Show>
            </PrefGroup>
            <PrefGroup title="Add">
                <ActionRow
                    title="Ingest a library"
                    sub="One library at one version, its sources and the two models it is built with"
                    chevron=true
                    on_click=Callback::new(move |()| docs.wizard.set(Some(WizardSeed::default())))
                />
                <ActionRow
                    title="Import a corpus file"
                    sub="Exported from this or another gateway; validated before anything is written"
                    chevron=true
                    on_click=Callback::new(move |()| docs.import_open.set(true))
                />
            </PrefGroup>
        </div>
    }
}

/// A selection that names a corpus the gateway does not have.
#[component]
fn Gone(back: Callback<()>) -> impl IntoView {
    view! {
        <div class="mdetail-empty">
            <p>"This corpus is not here: it was deleted, or the link names one that never was."</p>
            <button type="button" class="btn" on:click=move |_| back.run(())>
                "Back to the list"
            </button>
        </div>
    }
}
