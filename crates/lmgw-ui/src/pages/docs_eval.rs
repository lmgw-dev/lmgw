//! The eval dashboard (quickdoc §11): golden queries, the run trigger, the
//! history that makes the regression badge mean something, and the
//! synthetic-candidate curation queue.
//!
//! Every run is shown with the §6 parameters it was measured under, because two
//! runs under different parameters are not comparable and a score without them
//! is a number pretending to be a measurement. `orphaned` is called out for the
//! same reason: those queries score zero because their expected chunks are gone
//! (§4), which is not "retrieval got worse".
//!
//! The queue below the table is the other half of that honesty: a generated
//! query is a *proposal* and is shown next to the section it was written from,
//! because deciding whether a question is a fair test means reading what it is
//! supposed to find. Nothing there counts until it is accepted — a benchmark
//! that wrote and accepted its own questions would be marking its own homework.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    ChunkRow, EvalHistory, EvalRunRow, GoldenCandidateRow, GoldenCandidatesResponse,
    GoldenQueryRow, GoldenResponse,
};
use serde_json::{json, Value};

use crate::widgets::{use_toasts, ConfirmButton, Modal, ModalFooter, Select};

use super::docs::{start_job, use_docs, CorpusJobLine};

#[component]
pub fn EvalDashboard() -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    let reload = RwSignal::new(0u32);
    let bump = move || reload.update(|n| *n += 1);
    let k = RwSignal::new(String::new());
    let editing = RwSignal::new(None::<GoldenQueryRow>);
    let sample = RwSignal::new(String::new());
    let per_chunk = RwSignal::new("1".to_string());

    let corpus_opts = Signal::derive(move || state.options());
    let corpus_id = Memo::new(move |_| state.selected_corpus().map(|c| c.id));

    let golden = LocalResource::new(move || {
        reload.get();
        let id = corpus_id.get();
        async move {
            match id {
                None => Ok(GoldenResponse::default()),
                Some(id) => {
                    crate::api::get::<GoldenResponse>(format!("/api/docs/golden?corpus_id={id}"))
                        .await
                }
            }
        }
    });
    let history = LocalResource::new(move || {
        reload.get();
        state.reload.get();
        let id = corpus_id.get();
        async move {
            match id {
                None => Ok(EvalHistory::default()),
                Some(id) => {
                    crate::api::get::<EvalHistory>(format!("/api/docs/eval?corpus_id={id}")).await
                }
            }
        }
    });
    let candidates = LocalResource::new(move || {
        reload.get();
        state.reload.get();
        let id = corpus_id.get();
        async move {
            match id {
                None => Ok(GoldenCandidatesResponse::default()),
                Some(id) => {
                    crate::api::get::<GoldenCandidatesResponse>(format!(
                        "/api/docs/golden/candidates?corpus_id={id}&status=pending"
                    ))
                    .await
                }
            }
        }
    });

    // The sample field starts at the corpus's own chunk count: a generation run
    // that quietly looked at the first twenty chunks would produce golden
    // queries about the first page of the documentation. Lowering it is the
    // owner asking for a smaller run, visibly.
    Effect::new(move |_| {
        if let Some(c) = state.selected_corpus() {
            sample.set(c.chunk_count.to_string());
        }
    });

    let generate = move |_| {
        let Some(id) = corpus_id.get_untracked() else {
            toasts.err("pick a corpus first");
            return;
        };
        let mut body = json!({ "corpus_id": id });
        for (field, raw) in [
            ("sample", sample.get_untracked()),
            ("per_chunk", per_chunk.get_untracked()),
        ] {
            let raw = raw.trim().to_string();
            if raw.is_empty() {
                continue;
            }
            match raw.parse::<u32>() {
                Ok(v) => body[field] = json!(v),
                Err(_) => {
                    toasts.err(format!("{field} must be a number"));
                    return;
                }
            }
        }
        start_job("/api/docs/golden/generate".to_string(), body, move || {
            state.bump();
            bump();
        });
    };

    let run = move |_| {
        let Some(id) = corpus_id.get_untracked() else {
            toasts.err("pick a corpus first");
            return;
        };
        let raw = k.get_untracked();
        let depth = raw.trim();
        let mut body = json!({ "corpus_id": id });
        if !depth.is_empty() {
            match depth.parse::<u32>() {
                Ok(v) => body["k"] = json!(v),
                Err(_) => {
                    toasts.err("k must be a number");
                    return;
                }
            }
        }
        start_job("/api/docs/eval".to_string(), body, move || {
            state.bump();
            bump();
        });
    };

    // One table at a time, the whole body high: the three are different
    // questions (what is asked, what is proposed, how it went), and a page of
    // three unbounded tables put the last one out of reach.
    let view_ = crate::prefs::persisted_string("docs.eval.view", "golden");
    let golden_n = move || {
        golden.with(|g| {
            g.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|g| g.golden_queries.len())
        })
    };
    let cand_n = move || {
        candidates.with(|c| {
            c.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|c| c.candidates.len())
        })
    };
    let runs_n = move || {
        history.with(|h| {
            h.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|h| h.eval_runs.len())
        })
    };
    let seg = move |which: &'static str| {
        move || {
            if view_.get() == which {
                "seg-btn active"
            } else {
                "seg-btn"
            }
        }
    };
    let n_pill = |n: Option<usize>| n.map(|n| view! { <span class="count">{n}</span> });

    view! {
        <div class="card eval-head">
            <div class="row">
                <div class="field">
                    <label>"Corpus"</label>
                    <Select value=state.selected options=corpus_opts placeholder="pick a corpus"/>
                </div>
                <div class="field">
                    <label title="empty = the configured default">"hit@k depth"</label>
                    <input
                        class="input mono w-num"
                        placeholder="default"
                        prop:value=move || k.get()
                        on:input=move |ev| k.set(event_target_value(&ev))
                    />
                </div>
                <span class="spacer"></span>
                <button class="btn primary" on:click=run>
                    "Run eval"
                </button>
            </div>
            {move || {
                state
                    .selected_corpus()
                    .map(|c| {
                        let score = match c.eval_score {
                            Some(s) => format!("hit@{} {s:.3}", c.eval_k.max(1)),
                            None => "never measured".into(),
                        };
                        let best = match c.eval_best {
                            Some(b) => format!("best ever {b:.3}"),
                            None => "no best yet".into(),
                        };
                        view! {
                            <div class="row eval-latest">
                                <span class="dim">"Latest"</span>
                                <span class="mono-sm">{score}</span>
                                <span class="dim">"·"</span>
                                <span class="mono-sm">{best}</span>
                                {(!c.eval_at.is_empty())
                                    .then(|| {
                                        view! { <span class="dim mono-sm">{c.eval_at.clone()}</span> }
                                    })}
                                {c
                                    .eval_regression
                                    .then(|| {
                                        view! {
                                            <span class="chip warn">
                                                <span class="dot"></span>
                                                "regression"
                                            </span>
                                        }
                                    })}
                            </div>
                            <CorpusJobLine id=c.id/>
                        }
                    })
            }}
        </div>

        <div class="docs-strip">
            <div class="seg">
                <button class=seg("golden") on:click=move |_| view_.set("golden".into())>
                    "Golden queries"
                    {move || n_pill(golden_n())}
                </button>
                <button class=seg("candidates") on:click=move |_| view_.set("candidates".into())>
                    "Synthetic candidates"
                    {move || n_pill(cand_n())}
                </button>
                <button class=seg("history") on:click=move |_| view_.set("history".into())>
                    "Run history"
                    {move || n_pill(runs_n())}
                </button>
            </div>
            <span class="spacer"></span>
            <Show when=move || view_.get() == "golden">
                <button
                    class="btn ghost"
                    disabled=move || corpus_id.get().is_none()
                    on:click=move |_| {
                        editing
                            .set(
                                Some(GoldenQueryRow {
                                    corpus_id: corpus_id.get_untracked().unwrap_or_default(),
                                    origin: "manual".into(),
                                    ..Default::default()
                                }),
                            )
                    }
                >
                    "New golden query"
                </button>
            </Show>
            <Show when=move || view_.get() == "candidates">
                <label class="lab-label" for="eval-sample">"Chunks to sample"</label>
                <input
                    id="eval-sample"
                    class="input mono w-num"
                    prop:value=move || sample.get()
                    on:input=move |ev| sample.set(event_target_value(&ev))
                />
                <label class="lab-label" for="eval-per-chunk">"Questions each"</label>
                <input
                    id="eval-per-chunk"
                    class="input mono w-num"
                    prop:value=move || per_chunk.get()
                    on:input=move |ev| per_chunk.set(event_target_value(&ev))
                />
                <button
                    class="btn ghost"
                    disabled=move || corpus_id.get().is_none()
                    on:click=generate
                >
                    "Generate"
                </button>
            </Show>
        </div>

        <Show when=move || view_.get() == "golden">
            <div class="fill-pane hug card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Query"</th>
                            <th>"Expected chunks"</th>
                            <th>"Origin"</th>
                            <th class="actions"></th>
                        </tr>
                    </thead>
                    <tbody>
                        {move || match golden.get() {
                            Some(Ok(g)) => {
                                let rows = g.golden_queries;
                                view! {
                                    <For each=move || rows.clone() key=|g| g.id let:row>
                                        <GoldenRow g=row editing=editing on_change=bump/>
                                    </For>
                                }
                                    .into_any()
                            }
                            Some(Err(e)) => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="4">{e.to_string()}</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                            None => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="4">"Loading…"</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                        }}
                    </tbody>
                </table>
                <Show when=move || {
                    golden.get().is_some_and(|g| g.is_ok_and(|g| g.golden_queries.is_empty()))
                }>
                    <div class="empty">
                        "No golden queries yet — copy a chunk id from the corpus browser and "
                        "write the question it should answer."
                    </div>
                </Show>
            </div>
        </Show>

        <Show when=move || view_.get() == "candidates">
            <div class="fill-pane hug card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Proposed query"</th>
                            <th>"From this section"</th>
                            <th class="col-p2">"Model"</th>
                            <th class="actions"></th>
                        </tr>
                    </thead>
                    <tbody>
                        {move || match candidates.get() {
                            Some(Ok(c)) => {
                                let chunks = c.chunks.clone();
                                let rows = c.candidates;
                                view! {
                                    <For each=move || rows.clone() key=|c| c.id let:row>
                                        {
                                            let chunk = row
                                                .expected_chunk_ids
                                                .first()
                                                .and_then(|id| chunks.get(id).cloned());
                                            view! {
                                                <CandidateRow c=row chunk=chunk on_change=bump/>
                                            }
                                        }
                                    </For>
                                }
                                    .into_any()
                            }
                            Some(Err(e)) => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="4">{e.to_string()}</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                            None => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="4">"Loading…"</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                        }}
                    </tbody>
                </table>
                <Show when=move || {
                    candidates.get().is_some_and(|c| c.is_ok_and(|c| c.candidates.is_empty()))
                }>
                    <div class="empty">
                        "Nothing waiting. A generation run reads the corpus's own chunks with "
                        "its pinned ingest model and proposes questions here — none of them "
                        "counts until you accept it."
                    </div>
                </Show>
            </div>
        </Show>

        <Show when=move || view_.get() == "history">
            <div class="fill-pane hug card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"When"</th>
                            <th class="num-h">"k"</th>
                            <th class="num-h">"Queries"</th>
                            <th class="num-h">"hit@k"</th>
                            <th class="num-h">"MRR"</th>
                            <th>"Notes"</th>
                        </tr>
                    </thead>
                    <tbody>
                        {move || match history.get() {
                            Some(Ok(h)) => {
                                let runs = h.eval_runs;
                                view! {
                                    <For each=move || runs.clone() key=|r| r.id let:run>
                                        <RunRow r=run/>
                                    </For>
                                }
                                    .into_any()
                            }
                            Some(Err(e)) => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="6">{e.to_string()}</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                            None => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="6">"Loading…"</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                        }}
                    </tbody>
                </table>
                <Show when=move || {
                    history.get().is_some_and(|h| h.is_ok_and(|h| h.eval_runs.is_empty()))
                }>
                    <div class="empty">"Never evaluated — which is not the same as scoring zero."</div>
                </Show>
            </div>
        </Show>

        <GoldenEditor editing=editing on_saved=bump/>
    }
}

#[component]
fn GoldenRow(
    g: GoldenQueryRow,
    editing: RwSignal<Option<GoldenQueryRow>>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let id = g.id;
    let expected = g.expected_chunk_ids.clone();
    let edit_row = g.clone();
    let delete = move || {
        spawn_local(async move {
            match crate::api::post::<Value, _>(format!("/api/docs/golden/{id}/delete"), &json!({}))
                .await
            {
                Ok(_) => {
                    toasts.ok("golden query removed");
                    on_change();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <tr>
            <td class="clip" title=g.query.clone()>{g.query.clone()}</td>
            <td class="dim mono-sm" title=expected.join(", ")>
                {if expected.is_empty() {
                    "— (any hit counts as a miss)".to_string()
                } else {
                    expected
                        .iter()
                        .map(|c| c.chars().take(10).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(", ")
                }}
            </td>
            <td><span class="type-badge">{g.origin.clone()}</span></td>
            <td class="actions">
                <span class="row-acts">
                    <ConfirmButton
                        label="Delete"
                        confirm="Delete this query?"
                        class="btn ghost sm"
                        on_confirm=Callback::new(move |()| delete())
                    />
                    <button
                        class="btn ghost sm"
                        on:click=move |_| editing.set(Some(edit_row.clone()))
                    >
                        "Edit"
                    </button>
                </span>
            </td>
        </tr>
    }
}

/// One proposal, with the section it was written from one click away. Accept
/// writes a golden query with `synthetic` as its origin — a curated query never
/// stops saying where it came from — and reject keeps the row so the next
/// generation run does not propose the same question again.
#[component]
fn CandidateRow(
    c: GoldenCandidateRow,
    chunk: Option<ChunkRow>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let id = c.id;
    let open = RwSignal::new(false);
    let deciding = RwSignal::new(false);
    let heading = chunk
        .as_ref()
        .map(|k| {
            if k.heading_path.trim().is_empty() {
                k.derived_title.clone()
            } else {
                k.heading_path.clone()
            }
        })
        .unwrap_or_else(|| "— the chunk is gone".to_string());
    let payload = chunk
        .as_ref()
        .map(|k| k.payload.clone())
        .unwrap_or_default();
    let rationale = c.rationale.clone();

    let decide = move |what: &'static str| {
        if deciding.get_untracked() {
            return;
        }
        deciding.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                format!("/api/docs/golden/candidates/{id}/{what}"),
                &json!({}),
            )
            .await;
            deciding.set(false);
            match res {
                Ok(_) => {
                    toasts.ok(if what == "accept" {
                        "accepted as a golden query"
                    } else {
                        "candidate rejected"
                    });
                    on_change();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
        <tr>
            // The rationale is the question's own second line — the one row
            // kind here that reads better with it than without.
            // Which model proposed it folds away on a narrow table; it stays
            // readable here (par:PAR-6).
            <td class="wrap" title=format!("proposed by {}", c.model)>
                {c.query.clone()}
                {(!rationale.is_empty())
                    .then(|| view! { <div class="dim mono-sm clip-line" title=rationale.clone()>{rationale.clone()}</div> })}
            </td>
            <td class="clickable clip" title=heading.clone() on:click=move |_| open.update(|o| *o = !*o)>
                <span class="dim mono-sm">{heading.clone()}</span>
            </td>
            <td class="dim mono-sm col-p2">{c.model.clone()}</td>
            <td class="actions">
                <span class="row-acts">
                    <button
                        class="btn ghost sm"
                        disabled=move || deciding.get()
                        on:click=move |_| decide("reject")
                    >
                        "Reject"
                    </button>
                    <button
                        class="btn sm"
                        disabled=move || deciding.get()
                        on:click=move |_| decide("accept")
                    >
                        "Accept"
                    </button>
                </span>
            </td>
        </tr>
        <Show when=move || open.get()>
            <tr class="detail-row">
                <td colspan="4">
                    <div class="mini-head">"The section this question should find"</div>
                    <pre class="preset">{payload.clone()}</pre>
                </td>
            </tr>
        </Show>
    }
}

#[component]
fn RunRow(r: EvalRunRow) -> impl IntoView {
    let open = RwSignal::new(false);
    let params = serde_json::to_string_pretty(&r.params).unwrap_or_default();
    let report = serde_json::to_string_pretty(&r.report).unwrap_or_default();
    let notes = {
        let mut n = Vec::new();
        if r.regression {
            n.push("below this corpus's best".to_string());
        }
        if r.orphaned_queries > 0 {
            n.push(format!(
                "{} orphaned (expected chunks gone)",
                r.orphaned_queries
            ));
        }
        n.join(" · ")
    };
    view! {
        <tr class="clickable" on:click=move |_| open.update(|o| *o = !*o)>
            <td class="dim mono-sm">{r.created_at.clone()}</td>
            <td class="num">{r.k}</td>
            <td class="num">{r.queries}</td>
            <td class="num">{format!("{:.3}", r.hit_at_k)}</td>
            <td class="num">{format!("{:.3}", r.mrr)}</td>
            <td class=if r.regression { "status-err" } else { "dim" }>{notes}</td>
        </tr>
        <Show when=move || open.get()>
            <tr class="detail-row">
                <td colspan="6">
                    <div class="mini-head">"Measured under"</div>
                    <pre class="preset">{params.clone()}</pre>
                    <div class="mini-head" style="margin-top:8px">"Per-query breakdown"</div>
                    <pre class="preset">{report.clone()}</pre>
                </td>
            </tr>
        </Show>
    }
}

/// Create/edit a golden query. Expected chunk ids are validated server-side
/// against this corpus — an id that is not in it would score as a permanent
/// miss and read as a retrieval regression, so it is refused on the way in.
#[component]
fn GoldenEditor(
    editing: RwSignal<Option<GoldenQueryRow>>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = editing.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && editing.get_untracked().is_some() {
            editing.set(None);
        }
    });
    view! {
        <Modal open=open title="Golden query" guard=true>
            {move || {
                editing
                    .get()
                    .map(|g| {
                        view! { <GoldenForm g=g open=open on_saved=on_saved/> }.into_any()
                    })
            }}
        </Modal>
    }
}

#[component]
fn GoldenForm(
    g: GoldenQueryRow,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = g.id == 0;
    let id = g.id;
    let corpus_id = g.corpus_id;
    let query = RwSignal::new(g.query.clone());
    let expected = RwSignal::new(g.expected_chunk_ids.join("\n"));
    let origin = RwSignal::new(if g.origin.is_empty() {
        "manual".to_string()
    } else {
        g.origin.clone()
    });
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let origin_opts = Signal::derive(|| {
        [("manual", "manual"), ("synthetic", "synthetic")]
            .into_iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let ids: Vec<String> = expected
            .get_untracked()
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let mut body = json!({
            "query": query.get_untracked().trim(),
            "expected_chunk_ids": ids,
            "origin": origin.get_untracked(),
        });
        if create {
            body["corpus_id"] = json!(corpus_id);
        } else {
            body["id"] = json!(id);
        }
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/docs/golden", &body).await;
            saving.set(false);
            match res {
                Ok(_) => {
                    toasts.ok(if create {
                        "golden query added"
                    } else {
                        "golden query saved"
                    });
                    on_saved();
                    open.set(false);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <div class="wiz">
            <div class="field">
                <label>"Query"</label>
                <input
                    class="input"
                    style="width:100%"
                    placeholder="how do I add a middleware layer?"
                    prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                />
            </div>
            <div class="field">
                <label>"Expected chunk ids · one per line, copied from the corpus browser"</label>
                <textarea
                    class="input mono ta"
                    prop:value=move || expected.get()
                    on:input=move |ev| expected.set(event_target_value(&ev))
                ></textarea>
            </div>
            <div class="field" style="max-width:200px">
                <label>"Origin"</label>
                <Select value=origin options=origin_opts/>
            </div>
            {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn primary" disabled=move || saving.get() on:click=save>
                    {move || if saving.get() { "Saving…" } else { "Save" }}
                </button>
            </ModalFooter>
        </div>
    }
}
