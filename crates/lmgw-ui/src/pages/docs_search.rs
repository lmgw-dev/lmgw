//! The search playground — the debug endpoint's face (quickdoc §6, §10, §11).
//!
//! Every stage parameter is a per-request knob here, seeded from the owner's
//! configured defaults, because that is exactly what the endpoint offers: an
//! external agent may *experiment* per request, only the owner *commits* — so
//! "Save as defaults" is a separate, explicit button rather than a side effect
//! of searching.
//!
//! Two renderings of one answer are shown side by side: the markdown a
//! `docs__query` caller actually receives, and the per-stage trace behind it —
//! BM25 candidates, KNN candidates and the kernel that found them, the RRF
//! fusion table, rerank scores *or the named reason the stage was skipped*, and
//! what the token budget trimmed.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    DocsSearchResponse, SearchHit, SearchParamsView, SearchTraceView, SettingsFull, StageHitView,
};
use serde_json::{json, Value};

use crate::fmt::human_bytes;
use crate::widgets::{use_toasts, Explain, Select};

use super::docs::use_docs;

/// Chunk ids are content hashes; the head is enough to match rows across
/// stages by eye, and the full id is one hover away.
fn short_id(id: &str) -> String {
    id.chars().take(10).collect()
}

fn parse_num<T: std::str::FromStr>(sig: RwSignal<String>, label: &str) -> Result<T, String> {
    sig.get_untracked()
        .trim()
        .parse::<T>()
        .map_err(|_| format!("{label} must be a number"))
}

#[component]
pub fn Playground() -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    // Settings' "Search stage defaults" lands here as `#params`: open the
    // folded parameters, where Save as defaults lives. Read from the router,
    // which already holds the new URL while this page renders.
    if leptos_router::hooks::use_location()
        .hash
        .get_untracked()
        .trim_start_matches('#')
        == "params"
    {
        crate::prefs::store_bool("open.docs.explain.params", true);
    }

    let query = RwSignal::new(String::new());
    let k_fts = RwSignal::new(String::new());
    let k_vec = RwSignal::new(String::new());
    let rrf_k = RwSignal::new(String::new());
    let rerank = RwSignal::new(true);
    let k_rerank = RwSignal::new(String::new());
    let limit = RwSignal::new(String::new());
    let budget = RwSignal::new(String::new());
    // Not a search knob — the depth an eval run measures hit@k at. It lives
    // here because this is where the stage defaults are saved, and a default
    // the page can show but not save is the gap this closes.
    let eval_k = RwSignal::new(String::new());
    let w_payload = RwSignal::new("1".to_string());
    let w_heading = RwSignal::new("1.5".to_string());
    let w_title = RwSignal::new("1.5".to_string());
    let w_summary = RwSignal::new("1".to_string());
    let searching = RwSignal::new(false);
    let result = RwSignal::new(None::<DocsSearchResponse>);
    let error = RwSignal::new(None::<String>);
    let show_raw = RwSignal::new(false);

    // The owner's defaults are where a request that overrides nothing starts,
    // so that is where the knobs start too.
    let settings = LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let seed = move |d: &lmgw_api_types::DocsSearchDefaults| {
        let p = SearchParamsView::from_defaults(d);
        k_fts.set(p.k_fts.to_string());
        k_vec.set(p.k_vec.to_string());
        rrf_k.set(p.rrf_k.to_string());
        rerank.set(p.rerank);
        k_rerank.set(if p.k_rerank > 0 { p.k_rerank } else { 20 }.to_string());
        limit.set(p.limit.to_string());
        budget.set(p.budget_tokens.map(|b| b.to_string()).unwrap_or_default());
        eval_k.set(d.eval_k.to_string());
        w_payload.set(p.fts_weights.payload.to_string());
        w_heading.set(p.fts_weights.heading_path.to_string());
        w_title.set(p.fts_weights.derived_title.to_string());
        w_summary.set(p.fts_weights.derived_summary.to_string());
    };
    Effect::new(move |seeded: Option<bool>| {
        if seeded == Some(true) {
            return true;
        }
        match settings.get().and_then(|r| r.ok()) {
            Some(s) => {
                seed(&s.docs_search);
                true
            }
            None => false,
        }
    });

    let params = move || -> Result<SearchParamsView, String> {
        Ok(SearchParamsView {
            k_fts: parse_num(k_fts, "k_fts")?,
            k_vec: parse_num(k_vec, "k_vec")?,
            rrf_k: parse_num(rrf_k, "rrf_k")?,
            fts_weights: lmgw_api_types::FtsWeightsView {
                payload: parse_num(w_payload, "payload weight")?,
                heading_path: parse_num(w_heading, "heading weight")?,
                derived_title: parse_num(w_title, "derived-title weight")?,
                derived_summary: parse_num(w_summary, "derived-summary weight")?,
            },
            rerank: rerank.get_untracked(),
            k_rerank: parse_num(k_rerank, "k_rerank")?,
            limit: parse_num(limit, "limit")?,
            budget_tokens: {
                let raw = budget.get_untracked();
                if raw.trim().is_empty() {
                    None
                } else {
                    Some(parse_num::<u64>(budget, "budget_tokens")?).filter(|b| *b > 0)
                }
            },
        })
    };

    let search = move |_| {
        let Some(corpus) = state.selected_corpus() else {
            toasts.err("pick a corpus first");
            return;
        };
        let q = query.get_untracked().trim().to_string();
        if q.is_empty() {
            toasts.err("type a query");
            return;
        }
        let p = match params() {
            Ok(p) => p,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        if searching.get_untracked() {
            return;
        }
        searching.set(true);
        error.set(None);
        let body = json!({ "corpus_id": corpus.id, "query": q, "params": p });
        spawn_local(async move {
            let res = crate::api::post::<DocsSearchResponse, _>("/api/docs/search", &body).await;
            searching.set(false);
            match res {
                Ok(r) => result.set(Some(r)),
                Err(e) => {
                    result.set(None);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };

    let save_defaults = move |_| {
        let p = match params() {
            Ok(p) => p,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let k = match parse_num::<u32>(eval_k, "eval_k") {
            Ok(k) => k,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        // Everything the page exposes goes in the patch: a knob somebody tuned
        // and then saved has to come back next time, or the button is lying.
        // `k_rerank: 0` is how Settings spells "no rerank stage", so the
        // checkbox and the stored default cannot disagree.
        let patch = json!({
            "docs_search": {
                "k_fts": p.k_fts,
                "k_vec": p.k_vec,
                "rrf_k": p.rrf_k,
                "fts_weights": {
                    "payload": p.fts_weights.payload,
                    "heading_path": p.fts_weights.heading_path,
                    "derived_title": p.fts_weights.derived_title,
                    "derived_summary": p.fts_weights.derived_summary,
                },
                "k_rerank": if p.rerank { p.k_rerank } else { 0 },
                "limit": p.limit,
                "budget_tokens": p.budget_tokens.unwrap_or(0),
                "eval_k": k,
            }
        });
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/settings_set_full", &patch).await {
                Ok(_) => toasts.ok("these are the new stage defaults"),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let corpus_opts = Signal::derive(move || state.options());
    // The knobs, folded to the values they hold: the results are what the
    // page is for, and a dozen fields ahead of them pushed them off screen.
    let params_line = move || {
        let budget = budget.get();
        format!(
            "Stage parameters · k_fts {} · k_vec {} · rrf {} · rerank {} · limit {} · budget {} · \
             weights {} / {} / {} / {}",
            k_fts.get(),
            k_vec.get(),
            rrf_k.get(),
            if rerank.get() {
                k_rerank.get()
            } else {
                "off".to_string()
            },
            limit.get(),
            if budget.trim().is_empty() {
                "none".to_string()
            } else {
                budget
            },
            w_payload.get(),
            w_heading.get(),
            w_title.get(),
            w_summary.get(),
        )
    };

    view! {
        <div class="fill-pane playground">
        <div class="card">
            <div class="row pg-query">
                <div class="field">
                    <label>"Corpus"</label>
                    <Select value=state.selected options=corpus_opts placeholder="pick a corpus"/>
                </div>
                <div class="field pg-q">
                    <label>"Query"</label>
                    <input
                        class="input"
                        placeholder="how do I add a middleware layer?"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                search(());
                            }
                        }
                    />
                </div>
                <button
                    class="btn primary"
                    disabled=move || searching.get()
                    on:click=move |_| search(())
                >
                    {move || if searching.get() { "Searching…" } else { "Search" }}
                </button>
            </div>

            <div class="pg-params">
                <Explain summary=params_line persist="docs.explain.params">
                    <div class="field-grid">
                        <NumField label="k_fts · BM25 candidates" sig=k_fts/>
                        <NumField label="k_vec · KNN candidates" sig=k_vec/>
                        <NumField label="rrf_k · rank damping" sig=rrf_k/>
                        <NumField label="k_rerank · cross-encoder depth" sig=k_rerank/>
                        <NumField label="limit · chunks returned" sig=limit/>
                        <NumField label="budget_tokens · empty = none" sig=budget/>
                    </div>
                    <div class="mini-head">"BM25 column weights"</div>
                    <div class="field-grid">
                        <NumField label="payload" sig=w_payload/>
                        <NumField label="heading_path" sig=w_heading/>
                        <NumField label="derived_title" sig=w_title/>
                        <NumField label="derived_summary" sig=w_summary/>
                    </div>
                    <div class="mini-head">"Eval default"</div>
                    <div class="field-grid">
                        <NumField label="eval_k · hit@k depth a run measures at" sig=eval_k/>
                    </div>
                    <div class="row">
                        <label class="check">
                            <input
                                type="checkbox"
                                prop:checked=move || rerank.get()
                                on:change=move |ev| rerank.set(event_target_checked(&ev))
                            />
                            "run the cross-encoder stage when a reranker is attached"
                        </label>
                        <span class="spacer"></span>
                        <button
                            class="btn ghost"
                            on:click=move |_| {
                                if let Some(s) = settings.get().and_then(|r| r.ok()) {
                                    seed(&s.docs_search);
                                }
                            }
                        >
                            "Reset to defaults"
                        </button>
                        <button
                            class="btn ghost"
                            title="Write these values back as the gateway's defaults"
                            on:click=save_defaults
                        >
                            "Save as defaults"
                        </button>
                    </div>
                </Explain>
            </div>
        </div>

        {move || error.get().map(|e| view! { <div class="notice err">{e}</div> })}

        {move || {
            result
                .get()
                .map(|r| {
                    // Each `move ||` below captures its own piece: one `r`
                    // cannot be moved into several closures.
                    let DocsSearchResponse { hits, trace, urls, status, markdown, .. } = r;
                    let warnings = status.warnings;
                    let hit_count = hits.len();
                    view! {
                        <For each=move || warnings.clone() key=|w| w.clone() let:w>
                            <div class="notice warn">{w}</div>
                        </For>
                        <section class="ov-section">
                            <div class="row" style="justify-content:space-between; align-items:baseline">
                                <h2>"What a docs__query caller receives"</h2>
                                <button
                                    class="btn ghost"
                                    on:click=move |_| show_raw.update(|s| *s = !*s)
                                >
                                    {move || {
                                        if show_raw.get() {
                                            "Show rendered"
                                        } else {
                                            "Show raw markdown"
                                        }
                                    }}
                                </button>
                            </div>
                            <div class="card">
                                <MarkdownPane markdown=markdown raw=show_raw/>
                            </div>
                        </section>
                        <section class="ov-section">
                            <h2>{format!("Ranked chunks · {hit_count}")}</h2>
                            <For each=move || hits.clone() key=|h| h.chunk.id.clone() let:hit>
                                {
                                    let url = urls.get(&hit.chunk.id).cloned();
                                    view! { <HitCard hit=hit url=url/> }
                                }
                            </For>
                        </section>
                        <section class="ov-section">
                            <h2>"Trace"</h2>
                            <TracePane t=trace/>
                        </section>
                    }
                })
        }}
        </div>
    }
}

#[component]
fn NumField(label: &'static str, sig: RwSignal<String>) -> impl IntoView {
    view! {
        <div class="field">
            <label>{label}</label>
            <input
                class="input mono"
                prop:value=move || sig.get()
                on:input=move |ev| sig.set(event_target_value(&ev))
            />
        </div>
    }
}

/// The response is markdown, so it is shown as markdown — with a raw toggle,
/// because the payloads inside it carry their own fences and "what exactly did
/// the model get" is sometimes the only question worth asking.
#[component]
fn MarkdownPane(markdown: String, raw: RwSignal<bool>) -> impl IntoView {
    let md_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    let src = markdown.clone();
    Effect::new(move |_| {
        let Some(el) = md_ref.get() else { return };
        el.set_inner_html(&super::chat::md_to_html(&src));
    });
    view! {
        <Show
            when=move || raw.get()
            fallback=move || view! { <div class="md" node_ref=md_ref></div> }
        >
            <pre class="preset" style="white-space:pre-wrap; word-break:break-word">
                {markdown.clone()}
            </pre>
        </Show>
    }
}

#[component]
fn HitCard(hit: SearchHit, url: Option<String>) -> impl IntoView {
    let c = hit.chunk.clone();
    let ranks = [
        hit.fts_rank.map(|r| format!("bm25 #{r}")),
        hit.knn_rank.map(|r| format!("knn #{r}")),
        hit.knn_score.map(|s| format!("cos {s:.3}")),
        Some(format!("rrf {:.4}", hit.rrf_score)),
        hit.rerank_score.map(|s| format!("rerank {s:.3}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    view! {
        <div class="card" style="margin-bottom:8px">
            <div class="row">
                <span class="type-badge" title=c.id.clone()>{short_id(&c.id)}</span>
                <span class="mono-sm">{c.heading_path.clone()}</span>
                <span class="spacer" style="flex:1"></span>
                <span class="dim mono-sm">{ranks} " · " {hit.tokens} " tok"</span>
            </div>
            {url
                .map(|u| {
                    view! {
                        <div class="dim mini-note">
                            <a href=u.clone() target="_blank" rel="noreferrer">{u.clone()}</a>
                        </div>
                    }
                })}
            <pre class="preset" style="white-space:pre-wrap; word-break:break-word; margin-top:8px">
                {c.payload.clone()}
            </pre>
        </div>
    }
}

#[component]
pub(super) fn TracePane(t: SearchTraceView) -> impl IntoView {
    let ti = t.timings;
    let timings = format!(
        "embed {:.1} · fts {:.1} · knn {:.1} · fuse {:.1} · fetch {:.1} · rerank {:.1} · total {:.1} ms",
        ti.embed_ms, ti.fts_ms, ti.knn_ms, ti.fuse_ms, ti.fetch_ms, ti.rerank_ms, ti.total_ms,
    );
    let dropped = t.budget_dropped.clone();
    let dropped_any = !dropped.is_empty();
    view! {
        <div class="card">
            <div class="detail-grid">
                <span class="dim">"Embedding"</span>
                <span class="mono-sm">{t.embed_model.clone()}</span>
                <span class="dim">"KNN kernel"</span>
                <span class="mono-sm">
                    {t.knn_kernel.clone()} " · " {t.resident_vectors} " vectors · "
                    {human_bytes(t.resident_bytes)}
                </span>
                <span class="dim">"FTS query"</span>
                <span class="mono-sm">{t.fts_query.clone()}</span>
                <span class="dim">"Token counter"</span>
                <span class="mono-sm">
                    {t.token_counter.clone()} " · " {t.budget_used_tokens} " tokens used"
                </span>
                <span class="dim">"Timings"</span>
                <span class="mono-sm" style="grid-column:2 / -1">{timings}</span>
            </div>

            <div class="tablewrap" style="margin-top:12px">
                <div class="docs-stages">
                    <StageTable title="BM25 (negative, lower is better)" hits=t.fts.clone()/>
                    <StageTable title="Exact KNN (cosine)" hits=t.knn.clone()/>
                    <FusedTable t=t.clone()/>
                    <RerankStage t=t.clone()/>
                </div>
            </div>

            <Show when=move || dropped_any>
                <div class="notice warn" style="margin-top:10px">
                    <b>{format!("{} chunk(s) trimmed by the token budget", dropped.len())}</b>
                    <span class="detail mono-sm" title=dropped.join(", ")>
                        {dropped.iter().map(|d| short_id(d)).collect::<Vec<_>>().join(", ")}
                    </span>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn StageTable(title: &'static str, hits: Vec<StageHitView>) -> impl IntoView {
    let empty = hits.is_empty();
    view! {
        <div>
            <div class="mini-head">{title} " · " {hits.len()}</div>
            <table class="data">
                <tbody>
                    <For each=move || hits.clone() key=|h| (h.rank, h.chunk_id.clone()) let:h>
                        <tr>
                            <td class="dim num">{h.rank}</td>
                            <td class="mono-sm" title=h.chunk_id.clone()>{short_id(&h.chunk_id)}</td>
                            <td class="num">{format!("{:.4}", h.score)}</td>
                        </tr>
                    </For>
                </tbody>
            </table>
            <Show when=move || empty>
                <div class="empty">"no candidates"</div>
            </Show>
        </div>
    }
}

#[component]
fn FusedTable(t: SearchTraceView) -> impl IntoView {
    let fused = t.fused.clone();
    view! {
        <div>
            <div class="mini-head">
                "RRF fusion · k=" {format!("{:.0}", t.params.rrf_k)} " · " {fused.len()}
            </div>
            <table class="data">
                <tbody>
                    <For each=move || fused.clone() key=|f| f.chunk_id.clone() let:f>
                        <tr>
                            <td class="mono-sm" title=f.chunk_id.clone()>{short_id(&f.chunk_id)}</td>
                            <td class="num">{format!("{:.4}", f.rrf_score)}</td>
                            <td class="dim mono-sm">
                                {format!(
                                    "bm25 {} · knn {}",
                                    f.fts_rank.map(|r| r.to_string()).unwrap_or_else(|| "—".into()),
                                    f.knn_rank.map(|r| r.to_string()).unwrap_or_else(|| "—".into()),
                                )}
                            </td>
                        </tr>
                    </For>
                </tbody>
            </table>
        </div>
    }
}

/// The rerank stage either ran, or says why it did not — never silently
/// weaker ranking (§6).
#[component]
fn RerankStage(t: SearchTraceView) -> impl IntoView {
    view! {
        <div>
            {match (t.rerank_skipped.clone(), t.rerank_model.clone()) {
                (Some(reason), _) => {
                    view! {
                        <div class="mini-head">"Rerank · skipped"</div>
                        <div class="notice warn">{reason}</div>
                    }
                        .into_any()
                }
                (None, model) => {
                    let hits = t.rerank.clone();
                    let label = model.unwrap_or_else(|| "cross-encoder".into());
                    view! {
                        <div class="mini-head">{label} " · " {hits.len()}</div>
                        <table class="data">
                            <tbody>
                                <For
                                    each=move || hits.clone()
                                    key=|h| (h.rank, h.chunk_id.clone())
                                    let:h
                                >
                                    <tr>
                                        <td class="dim num">{h.rank}</td>
                                        <td class="mono-sm" title=h.chunk_id.clone()>
                                            {short_id(&h.chunk_id)}
                                        </td>
                                        <td class="num">{format!("{:.4}", h.score)}</td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
