//! The Search tab: a playground over this base alone, search first — the
//! query field with Search, the token budget under it — then the excerpts
//! as rows of a boxed group (file, page, heading path, score and tokens over
//! the text), and the per-stage trace folded under them. It is the way to
//! judge retrieval before a Chat relies on it. Clicking an excerpt opens the
//! file's text scrolled to that chunk.

use leptos::prelude::*;
use lmgw_api_types::SearchTraceView;
use serde_json::{json, Map, Value};

use crate::widgets::{use_toasts, Ico, Icon, NumberRow, PrefGroup};

use super::knowledge::use_kb;
use lmgw_chat_ui::knowledge_source::{SourceModal, SourceRef};

use lmgw_api_types::knowledge::{KnowledgeExcerpt as Excerpt, KnowledgeSearchResult as Retrieval};

/// The budget as typed: empty for none, else a whole number of tokens.
fn budget_of(raw: &str) -> Result<Option<u64>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<u64>()
        .map(Some)
        .map_err(|_| format!("Budget must be a whole number of tokens (got '{t}')"))
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
    let trace_open = RwSignal::new(false);
    let budget_err = Signal::derive(move || budget.with(|b| budget_of(b).err()));

    let run = move || {
        let q = query.get_untracked();
        if q.trim().is_empty() || busy.get_untracked() {
            return;
        }
        let mut body = Map::new();
        body.insert("query".into(), json!(q.trim()));
        body.insert("kb_ids".into(), json!([ctx.id]));
        match budget_of(&budget.get_untracked()) {
            Ok(Some(n)) => {
                body.insert("budget_tokens".into(), json!(n));
            }
            Ok(None) => {}
            Err(e) => {
                toasts.err(e);
                return;
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
        <PrefGroup>
            <div class="prow kb-query">
                <label class="msearch kb-search-field">
                    <Icon ico=Ico::Search/>
                    <input
                        type="search"
                        placeholder="what did the refund come to?"
                        aria-label="Search this knowledge base"
                        spellcheck="false"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                run();
                            }
                        }
                    />
                </label>
                <button
                    type="button"
                    class="btn suggested pill"
                    disabled=move || busy.get() || query.with(|q| q.trim().is_empty())
                    on:click=move |_| run()
                >
                    {move || if busy.get() { "Searching…" } else { "Search" }}
                </button>
            </div>
            <NumberRow
                title="Token budget"
                sub="Optional: the most the excerpts may hold together"
                value=budget
                placeholder="none"
                unit="tokens"
                error=budget_err
                wire="budget_tokens"
            />
        </PrefGroup>
        {move || {
            result
                .get()
                .map(|r| {
                    let n = r.excerpts.len();
                    let traces = StoredValue::new(
                        r.traces.clone().into_iter().enumerate().collect::<Vec<(usize, SearchTraceView)>>(),
                    );
                    let meta = format!(
                        "{n} excerpts · {} tokens · {:.0} ms{}",
                        r.tokens,
                        r.ms,
                        if r.dropped > 0 {
                            format!(" · {} more did not fit the budget", r.dropped)
                        } else {
                            String::new()
                        },
                    );
                    view! {
                        {r.notes
                            .iter()
                            .map(|n| view! { <div class="notice warn kb-note">{n.clone()}</div> })
                            .collect_view()}
                        <PrefGroup title="Excerpts" desc=meta>
                            {(n == 0).then(|| view! { <div class="prow kb-none">"Nothing found."</div> })}
                            <For each=move || r.excerpts.clone() key=|e| e.chunk_id.clone() let:e>
                                <ExcerptRow e=e source=source/>
                            </For>
                        </PrefGroup>
                        <section class="pref-group kb-trace">
                            <div class="pref-head">
                                <h2>"Trace"</h2>
                                <span class="pref-suffix">
                                    <button
                                        type="button"
                                        class="btn flat sm kb-trace-toggle"
                                        aria-expanded=move || trace_open.get().to_string()
                                        on:click=move |_| trace_open.update(|o| *o = !*o)
                                    >
                                        {move || if trace_open.get() { "Hide" } else { "Show" }}
                                    </button>
                                </span>
                            </div>
                            <Show when=move || trace_open.get()>
                                <For each=move || traces.get_value() key=|(i, _)| *i let:t>
                                    <super::docs_search::TracePane t=t.1/>
                                </For>
                            </Show>
                        </section>
                    }
                })
        }}
        <SourceModal target=source/>
    }
}

#[component]
fn ExcerptRow(e: Excerpt, source: RwSignal<Option<SourceRef>>) -> impl IntoView {
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
    let place = format!(
        "{}{}",
        e.file,
        e.page.map(|p| format!(" · page {p}")).unwrap_or_default()
    );
    view! {
        <div class="prow stack kb-excerpt">
            <div class="kb-excerpt-head">
                <button
                    type="button"
                    class="link-btn kb-excerpt-file"
                    title=format!("{place} · open the file at this chunk")
                    on:click=move |_| open()
                >
                    {place.clone()}
                </button>
                {(!e.heading_path.is_empty())
                    .then(|| {
                        view! {
                            <span class="kb-excerpt-path" title=e.heading_path.clone()>
                                {e.heading_path.clone()}
                            </span>
                        }
                    })}
                <span class="kb-excerpt-score">
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_budget_is_none_and_a_word_is_refused() {
        assert_eq!(budget_of(""), Ok(None));
        assert_eq!(budget_of(" 800 "), Ok(Some(800)));
        assert!(budget_of("lots").is_err());
    }
}
