//! The Search tab: a playground over this base alone — query in, excerpts out
//! with score, file, page, heading path and tokens, and the per-stage trace.
//! It is the way to judge retrieval before a Chat relies on it. Clicking an
//! excerpt opens the file's text scrolled to that chunk.

use leptos::prelude::*;
use lmgw_api_types::SearchTraceView;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::widgets::{use_toasts, Section};

use super::knowledge::use_kb;
use super::knowledge_source::{SourceModal, SourceRef};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Excerpt {
    kb: String,
    file_id: i64,
    file: String,
    page: Option<i64>,
    chunk_id: String,
    heading_path: String,
    text: String,
    score: f64,
    tokens: u64,
    span_start: i64,
    span_end: i64,
    file_sha: String,
    /// The reranker did not score it: `score` is its fused score.
    rerank_skipped: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Retrieval {
    excerpts: Vec<Excerpt>,
    tokens: u64,
    dropped: u64,
    notes: Vec<String>,
    ms: f64,
    traces: Vec<SearchTraceView>,
}

#[component]
pub fn SearchTab() -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let query = RwSignal::new(String::new());
    let budget = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Retrieval>);
    let source = RwSignal::new(None::<SourceRef>);

    let run = move || {
        let q = query.get_untracked();
        if q.trim().is_empty() || busy.get_untracked() {
            return;
        }
        let mut body = Map::new();
        body.insert("query".into(), json!(q.trim()));
        body.insert("kb_ids".into(), json!([ctx.id]));
        let b = budget.get_untracked();
        if !b.trim().is_empty() {
            match b.trim().parse::<u64>() {
                Ok(n) => {
                    body.insert("budget_tokens".into(), json!(n));
                }
                Err(_) => {
                    toasts.err(format!(
                        "Budget must be a whole number of tokens (got '{}')",
                        b.trim()
                    ));
                    return;
                }
            }
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let res =
                crate::api::post::<Retrieval, _>("/api/knowledge/search", &Value::Object(body))
                    .await;
            if !ctx.scope.alive() {
                if let Err(e) = res {
                    toasts.err(e.to_string());
                }
                return;
            }
            busy.set(false);
            match res {
                Ok(r) => result.set(Some(r)),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
        <div class="card kb-search">
            <div class="kb-search-row">
                <div class="field">
                    <label>"Query"</label>
                    <input
                        class="input"
                        placeholder="what did the refund come to?"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                run();
                            }
                        }
                    />
                </div>
                <div class="field">
                    <label>"Token budget · optional"</label>
                    <input
                        class="input mono"
                        inputmode="numeric"
                        placeholder="none"
                        prop:value=move || budget.get()
                        on:input=move |ev| budget.set(event_target_value(&ev))
                    />
                </div>
                <button
                    class="btn primary"
                    disabled=move || busy.get() || query.with(|q| q.trim().is_empty())
                    on:click=move |_| run()
                >
                    {move || if busy.get() { "Searching…" } else { "Search" }}
                </button>
            </div>
        </div>
        {move || {
            result
                .get()
                .map(|r| {
                    let n = r.excerpts.len();
                    let traces = StoredValue::new(r.traces.clone().into_iter().enumerate().collect::<Vec<(usize, SearchTraceView)>>());
                    view! {
                        <div class="dim mono-sm kb-search-meta">
                            {n} " excerpts · " {r.tokens} " tokens · " {format!("{:.0}", r.ms)} " ms"
                            {(r.dropped > 0)
                                .then(|| format!(" · {} more did not fit the budget", r.dropped))}
                        </div>
                        <For each=move || r.notes.clone() key=|n| n.clone() let:n>
                            <div class="notice warn">{n}</div>
                        </For>
                        {(n == 0)
                            .then(|| view! { <div class="empty">"Nothing found."</div> })}
                        <For
                            each=move || r.excerpts.clone()
                            key=|e| e.chunk_id.clone()
                            let:e
                        >
                            <ExcerptCard e=e source=source/>
                        </For>
                        <Section title="Trace" default_open=false>
                            <For
                                each=move || traces.get_value()
                                key=|(i, _)| *i
                                let:t
                            >
                                <super::docs_search::TracePane t=t.1/>
                            </For>
                        </Section>
                    }
                })
        }}
        <SourceModal target=source/>
    }
}

#[component]
fn ExcerptCard(e: Excerpt, source: RwSignal<Option<SourceRef>>) -> impl IntoView {
    let (file_id, chunk) = (e.file_id, e.chunk_id.clone());
    let (span, sha) = (
        Some((e.span_start, e.span_end)),
        Some(e.file_sha.clone()).filter(|s| !s.is_empty()),
    );
    let open = move || {
        source.set(Some(SourceRef {
            file_id,
            chunk: Some(chunk.clone()),
            span,
            sha: sha.clone(),
        }))
    };
    let open2 = open.clone();
    view! {
        <div class="card kb-excerpt">
            <div class="row">
                <button class="link-btn mono-sm" title="Open the file at this chunk" on:click=move |_| open()>
                    {e.file.clone()}
                    {e.page.map(|p| format!(" · page {p}"))}
                </button>
                {(!e.heading_path.is_empty())
                    .then(|| view! { <span class="dim mono-sm">{e.heading_path.clone()}</span> })}
                <span class="spacer" style="flex:1"></span>
                <span class="dim mono-sm">
                    "score " {format!("{:.4}", e.score)}
                    {e.rerank_skipped.then_some(" (not reranked)")} " · " {e.tokens} " tok"
                </span>
            </div>
            <pre
                class="kb-excerpt-text"
                title="Click to open the file at this chunk"
                on:click=move |_| open2()
            >
                {e.text.clone()}
            </pre>
        </div>
    }
}
