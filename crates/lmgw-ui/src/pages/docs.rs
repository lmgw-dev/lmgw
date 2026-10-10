//! Docs — the quickdoc corpora plane (quickdoc §11).
//!
//! Five surfaces behind one page: the **corpus list** (with the same badges
//! `docs__resolve` reports to a model, so owner and agent read the same
//! status), the agent **request queue**, the **ingest wizard**, the **search
//! playground** over the debug endpoint, and the **eval dashboard**. The corpus
//! browser opens per corpus from the list.
//!
//! Job progress for `ingest` / `re_embed` / `eval_run` / `golden_gen` comes off
//! the shared jobs feed exactly the way Downloads consumes `hf_download` (§9c): the
//! durable rows are re-fetched when a job starts or ends, and every tick in
//! between arrives on the bus, so this page never polls.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::{
    ChunksResponse, CorpusView, DocRequestsResponse, DocsJobStarted, DocsOverview, DocumentRow,
    DocumentsResponse, ExportManifest, ImportReport, JobRow, MessageAck,
};
use serde_json::{json, Value};
use wasm_bindgen::JsCast;

use crate::fmt::{grouped, hue_for, human_bytes};
use crate::widgets::{
    use_toasts, Facet, FacetSet, FilterBar, MenuItem, Modal, ModalFooter, ModalSize, ModelPicker,
    NavTab, PageFrame, PageMode, RowMenu, Side, SplitPane, SubNav, Tone,
};

/// Every job kind that reports against a corpus. All four share the
/// `corpus:<id>` dedup key (it is scoped to the kind), so a lookup has to match
/// on both — otherwise an eval would show up as the corpus's ingest.
pub const CORPUS_JOB_KINDS: [&str; 4] = ["ingest", "re_embed", "eval_run", "golden_gen"];

// ---------------------------------------------------------------------------
// Pending-request badge (sidebar)
// ---------------------------------------------------------------------------

/// Number of pending `docs__request` filings, shown on the sidebar's Docs
/// entry. §11 wants the count visible without opening the tab: an agent that
/// asked for a corpus is waiting on the owner, and a queue nobody sees is a
/// queue nobody works.
#[derive(Clone, Copy)]
pub struct DocsPending(pub RwSignal<u64>);

pub fn use_docs_pending() -> DocsPending {
    expect_context::<DocsPending>()
}

/// Re-count the queue. Called on mount, after every queue mutation, and
/// whenever a corpus job finishes — a completed ingest auto-fulfils the
/// requests it satisfies, so the badge would otherwise sit one ingest stale.
pub fn refresh_pending(pending: DocsPending) {
    spawn_local(async move {
        if let Ok(r) =
            crate::api::get::<DocRequestsResponse>("/api/docs/requests?status=pending").await
        {
            pending.0.set(r.requests.len() as u64);
        }
    });
}

/// Install the badge signal and keep it current. Called once, in `App`, after
/// the live bus exists.
pub fn provide_docs_pending() {
    let pending = DocsPending(RwSignal::new(0));
    provide_context(pending);
    refresh_pending(pending);

    let live = crate::live::use_live();
    let ingest_jobs = Memo::new(move |_| {
        let mut ids: Vec<i64> = live
            .jobs
            .get()
            .unwrap_or_default()
            .iter()
            .filter(|j| j.kind == "ingest")
            .map(|j| j.id)
            .collect();
        ids.sort_unstable();
        ids
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = ingest_jobs.get();
        if prev.is_some_and(|p| p != now) {
            refresh_pending(pending);
        }
        now
    });
}

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

/// What the ingest wizard opens with. A queue entry fills `library`/`version`
/// and carries its own text along, so the form says which request it is
/// answering rather than silently arriving pre-typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WizardSeed {
    pub library: String,
    pub version: String,
    pub reason: Option<String>,
    pub requested_by: Option<String>,
}

#[derive(Clone, Copy)]
pub struct DocsState {
    /// Bumped after every mutation; the corpora resource tracks it.
    pub reload: RwSignal<u32>,
    pub corpora: RwSignal<Vec<CorpusView>>,
    /// The corpus list has been read at least once.
    pub loaded: RwSignal<bool>,
    /// The last read of the corpus list failed, with the server's words.
    pub error: RwSignal<Option<String>>,
    /// Which model the rerank stage would call, or `None` with the reason
    /// showing up in every trace.
    pub rerank_model: RwSignal<Option<String>>,
    /// Corpus row id the playground / eval / browser act on, as a string so it
    /// drops straight into a `Select`. Empty = none picked.
    pub selected: RwSignal<String>,
    pub wizard: RwSignal<Option<WizardSeed>>,
    pub browse: RwSignal<Option<CorpusView>>,
}

impl DocsState {
    pub fn bump(&self) {
        self.reload.update(|n| *n += 1);
    }

    /// Options for a corpus picker, newest-listed first as the API returns them.
    pub fn options(&self) -> Vec<(String, String)> {
        self.corpora
            .get()
            .into_iter()
            .map(|c| (c.id.to_string(), c.corpus_id))
            .collect()
    }

    pub fn selected_corpus(&self) -> Option<CorpusView> {
        let id = self.selected.get();
        self.corpora
            .get()
            .into_iter()
            .find(|c| c.id.to_string() == id)
    }
}

pub fn use_docs() -> DocsState {
    expect_context::<DocsState>()
}

/// The job currently running against one corpus, if any.
pub fn corpus_job(id: i64) -> Option<JobRow> {
    let key = format!("corpus:{id}");
    crate::live::use_live()
        .jobs
        .get()
        .unwrap_or_default()
        .into_iter()
        .find(|j| CORPUS_JOB_KINDS.contains(&j.kind.as_str()) && j.key.as_deref() == Some(&key))
}

/// Start a corpus job and report what happened. `already_running` is not an
/// error — it is the honest answer to "ingest this again" while one is running.
pub fn start_job(path: String, body: Value, then: impl Fn() + 'static) {
    let toasts = use_toasts();
    spawn_local(async move {
        match crate::api::post::<DocsJobStarted, _>(path, &body).await {
            Ok(r) if r.already_running => toasts.ok("a job is already running for this corpus"),
            Ok(_) => toasts.ok("job queued"),
            Err(e) => toasts.err(e.to_string()),
        }
        then();
    });
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// The page's tabs, as the path segment after `/docs`. Corpora is `/docs`.
pub const DOCS_TABS: &[&str] = &["corpora", "requests", "playground", "eval"];

/// Where a tab lives.
pub fn docs_href(tab: &str) -> String {
    match tab {
        "requests" | "playground" | "eval" => format!("/docs/{tab}"),
        _ => "/docs".to_string(),
    }
}

#[component]
pub fn Docs() -> impl IntoView {
    let state = DocsState {
        reload: RwSignal::new(0),
        corpora: RwSignal::new(Vec::new()),
        loaded: RwSignal::new(false),
        error: RwSignal::new(None),
        rerank_model: RwSignal::new(None),
        selected: RwSignal::new(String::new()),
        wizard: RwSignal::new(None),
        browse: RwSignal::new(None),
    };
    provide_context(state);
    let pending = use_docs_pending();
    let tab = crate::url_state::use_view("tab", DOCS_TABS, "corpora");

    let overview = LocalResource::new(move || {
        state.reload.get();
        crate::api::get::<DocsOverview>("/api/docs/corpora")
    });
    Effect::new(move |_| match overview.get() {
        Some(Ok(v)) => {
            state.corpora.set(v.corpora.clone());
            state.rerank_model.set(v.rerank_model.clone());
            pending.0.set(v.pending_requests);
            state.error.set(None);
            state.loaded.set(true);
            // Pick something the moment there is something to pick, so the
            // playground and eval views are never an empty dropdown.
            if state.selected.get_untracked().is_empty() {
                if let Some(first) = v.corpora.first() {
                    state.selected.set(first.id.to_string());
                }
            }
        }
        // A failed re-read keeps the list it had, and says why.
        Some(Err(e)) => state.error.set(Some(e.to_string())),
        None => {}
    });

    // Same trigger Downloads uses: the durable list is re-fetched when a job
    // starts or ends, never on a timer.
    let live = crate::live::use_live();
    let job_ids = Memo::new(move |_| {
        let mut ids: Vec<i64> = live
            .jobs
            .get()
            .unwrap_or_default()
            .iter()
            .filter(|j| CORPUS_JOB_KINDS.contains(&j.kind.as_str()))
            .map(|j| j.id)
            .collect();
        ids.sort_unstable();
        ids
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = job_ids.get();
        if prev.is_some_and(|p| p != now) {
            state.bump();
        }
        now
    });

    // The tabs carry their counts; the queue's is amber because it is agents
    // waiting on the owner (§11).
    let tabs = Signal::derive(move || {
        let mut corpora = NavTab::new("Corpora", docs_href("corpora")).exact();
        if state.loaded.get() {
            corpora = corpora.count(state.corpora.with(Vec::len));
        }
        let mut requests = NavTab::new("Requests", docs_href("requests"));
        let waiting = pending.0.get();
        if waiting > 0 {
            requests = requests.count(waiting).tone(Tone::Attn);
        }
        vec![
            corpora,
            requests,
            NavTab::new("Playground", docs_href("playground")),
            NavTab::new("Eval", docs_href("eval")),
        ]
    });

    let export_open = RwSignal::new(false);
    let import_open = RwSignal::new(false);

    view! {
        <PageFrame
            title="Docs"
            mode=PageMode::Fill
            class="docs"
            head_extra=move || view! { <SubNav tabs=tabs/> }
            actions=move || {
                view! {
                    <button class="btn ghost" on:click=move |_| import_open.set(true)>
                        "Import…"
                    </button>
                    <button class="btn ghost" on:click=move |_| export_open.set(true)>
                        "Export…"
                    </button>
                    <button
                        class="btn primary"
                        on:click=move |_| state.wizard.set(Some(WizardSeed::default()))
                    >
                        "Ingest a library"
                    </button>
                }
            }
        >
            {move || match tab.get() {
                "requests" => view! { <super::docs_requests::RequestQueue/> }.into_any(),
                "playground" => view! { <super::docs_search::Playground/> }.into_any(),
                "eval" => view! { <super::docs_eval::EvalDashboard/> }.into_any(),
                _ => view! { <CorpusList/> }.into_any(),
            }}

            <super::docs_wizard::IngestWizard/>
            <CorpusBrowser/>
            <ExportModal open=export_open/>
            <ImportModal open=import_open/>
        </PageFrame>
    }
}

// ---------------------------------------------------------------------------
// Corpus list
// ---------------------------------------------------------------------------

/// The facets a corpus can be filed under: the badges an agent sees on
/// `docs__resolve`, so "which corpora need me" is one click.
fn corpus_facets(c: &CorpusView) -> Vec<&'static str> {
    let mut out = Vec::new();
    if c.embed_status == "re_embed_required" {
        out.push("re-embed");
    }
    if c.eval_status == "regression" {
        out.push("regression");
    }
    if c.eval_status == "unmeasured" {
        out.push("unmeasured");
    }
    if c.chunk_count == 0 {
        out.push("empty");
    }
    out
}

/// Does this corpus match the words typed? Everything the row shows is
/// searchable: the library, both models, the sources.
fn corpus_matches(c: &CorpusView, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {}",
        c.corpus_id,
        c.embed_identity,
        c.ingest_model,
        c.sources
            .iter()
            .map(|s| format!("{} {}", s.root, s.kind))
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w))
}

#[component]
fn CorpusList() -> impl IntoView {
    let state = use_docs();
    let rerank = state.rerank_model;
    let query = crate::url_state::use_query_signal("q");
    let facet = crate::url_state::use_query_signal("show");
    let reembed = RwSignal::new(None::<CorpusView>);
    let deleting = RwSignal::new(None::<CorpusView>);

    let facets = Signal::derive(move || {
        let labels = [
            ("re-embed", "Needs re-embed"),
            ("regression", "Eval regression"),
            ("unmeasured", "Unmeasured"),
            ("empty", "Empty"),
        ];
        state.corpora.with(|cs| {
            labels
                .iter()
                .map(|(id, label)| Facet {
                    id: id.to_string(),
                    label: label.to_string(),
                    count: cs.iter().filter(|c| corpus_facets(c).contains(id)).count(),
                })
                // A facet with nothing in it is noise — unless it is the one
                // the URL asked for.
                .filter(|f| f.count > 0 || facet.with(|a| a == &f.id))
                .collect::<Vec<_>>()
        })
    });
    let shown = Memo::new(move |_| {
        let words: Vec<String> = query
            .get()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let on = facet.get();
        state.corpora.with(|cs| {
            cs.iter()
                .filter(|c| on.is_empty() || corpus_facets(c).contains(&on.as_str()))
                .filter(|c| corpus_matches(c, &words))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let total = Signal::derive(move || state.corpora.with(Vec::len));
    let count = Signal::derive(move || shown.with(Vec::len));
    let resident = move || {
        human_bytes(
            state
                .corpora
                .get()
                .iter()
                .map(|c| c.resident_bytes.max(0) as u64)
                .sum::<u64>(),
        )
    };

    view! {
        <div class="docs-meta dim">
            <span>
                "Rerank stage: "
                {move || match rerank.get() {
                    Some(m) => view! { <span class="mono-sm">{m}</span> }.into_any(),
                    None => {
                        view! {
                            <span class="mono-sm">
                                "none — every trace names why it was skipped"
                            </span>
                        }
                            .into_any()
                    }
                }}
            </span>
            <span>"Resident total " <span class="mono-sm">{resident}</span></span>
        </div>
        <FilterBar
            query=query
            placeholder="Filter by library, model or source"
            shown=count
            total=total
            noun="corpora"
            facets=FacetSet {
                items: facets,
                active: facet,
            }
        />
        {move || {
            state
                .error
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err">
                            "Failed to load corpora: " {e} " "
                            <button class="link-btn" on:click=move |_| state.bump()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        <div class="fill-pane hug card pad0">
            // Not `many-cols`: that folded Resident, Ingest model and Eval
            // under a 1920 window, where the table measured 1,153px at 1440
            // (par:PAR-3). They fold below the width the table fits whole in
            // (app.css), and what they hold is in the corpus's and the chunk
            // count's tooltips then.
            <table class="data corpus-table">
                <thead>
                    <tr>
                        <th>"Corpus"</th>
                        <th>"Status"</th>
                        <th class="num-h">"Chunks"</th>
                        <th class="num-h col-p2">"Resident"</th>
                        <th>"Embedding"</th>
                        <th class="col-p2">"Ingest model"</th>
                        <th class="col-p2">"Eval"</th>
                        <th class="col-p3">"Crawled"</th>
                        <th class="actions"></th>
                    </tr>
                </thead>
                <tbody>
                    <For each=move || shown.get() key=|c| (c.id, c.updated_at.clone()) let:c>
                        <CorpusRow c=c reembed=reembed deleting=deleting/>
                    </For>
                </tbody>
            </table>
            <Show when=move || !state.loaded.get() && state.error.with(Option::is_none)>
                <div class="empty">"Loading…"</div>
            </Show>
            <Show when=move || state.loaded.get() && total.get() == 0>
                <div class="empty">
                    "No corpora yet — ingest a library, or import a corpus file from another machine."
                </div>
            </Show>
            <Show when=move || total.get() != 0 && count.get() == 0>
                <div class="empty">"No corpus matches the filter."</div>
            </Show>
        </div>
        <ReembedModal target=reembed/>
        <DeleteCorpusModal target=deleting/>
    }
}

/// The two badges an agent sees on `docs__resolve`, rendered the same way here
/// so "what does the model think of this corpus" needs no translation.
#[component]
fn StatusBadges(c: CorpusView) -> impl IntoView {
    let warn = c.warnings.join("\n");
    let re_embed = c.embed_status == "re_embed_required";
    let eval = c.eval_status.clone();
    view! {
        {re_embed
            .then(|| {
                view! {
                    <span class="chip err" title=warn.clone()>
                        <span class="dot"></span>
                        "re-embed required"
                    </span>
                }
            })}
        {(eval == "regression")
            .then(|| {
                view! {
                    <span class="chip warn" title=warn.clone()>
                        <span class="dot"></span>
                        "eval regression"
                    </span>
                }
            })}
        {(eval == "unmeasured")
            .then(|| {
                view! {
                    <span class="chip off" title="no golden queries have been run against it">
                        <span class="dot"></span>
                        "unmeasured"
                    </span>
                }
            })}
        {(c.chunk_count == 0)
            .then(|| {
                view! {
                    <span class="chip off">
                        <span class="dot"></span>
                        "empty"
                    </span>
                }
            })}
    }
}

/// Live progress for whatever job owns this corpus right now — the ticks are
/// the feed's, the unit is the kind's own (documents, chunks, queries).
#[component]
pub fn CorpusJobLine(id: i64) -> impl IntoView {
    let toasts = use_toasts();
    let job = Memo::new(move |_| corpus_job(id));
    let cancel = move |_| {
        let Some(j) = job.get_untracked() else { return };
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/job_cancel", &json!({ "id": j.id })).await {
                Ok(_) => toasts.ok("cancel requested"),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        {move || {
            job.get()
                .map(|j| {
                    let unit = match j.kind.as_str() {
                        "ingest" => "documents",
                        "re_embed" | "golden_gen" => "chunks",
                        _ => "queries",
                    };
                    let counts = match j.total {
                        Some(t) => format!("{} / {t} {unit}", j.done),
                        None => format!("{} {unit}", j.done),
                    };
                    let url = j.detail.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                    view! {
                        <div class="dl-line">
                            <span class="mono-sm">
                                {j.kind.clone()} " · " {j.stage.clone()}
                                {(!url.is_empty()).then(|| view! { <span class="dim">" · " {url}</span> })}
                            </span>
                            <span class="row" style="flex-wrap:nowrap; justify-self:end">
                                <span class="dim mono-sm">{counts}</span>
                                <button class="btn ghost sm" on:click=cancel>
                                    "Cancel"
                                </button>
                            </span>
                            <div class="progress">
                                <i style=format!("width:{}%", j.percent.unwrap_or(3))></i>
                            </div>
                            <IngestCounters detail=j.detail.clone()/>
                        </div>
                    }
                })
        }}
    }
}

/// The counters a corpus job reports, in the kind's own words. The order is
/// the pipeline's, and a zero is left out rather than shown — except that
/// nothing is *rounded* or hidden: rejected spans in particular are surfaced
/// because a corpus built with many of them says something about the ingest
/// model (§8).
fn job_counters(detail: &Value) -> Vec<String> {
    let n = |k: &str| detail.get(k).and_then(Value::as_u64);
    [
        ("fetched", n("fetched")),
        ("unchanged", n("unchanged")),
        ("extracted", n("extracted")),
        ("failed", n("failed")),
        ("chunks", n("chunks")),
        ("embedded", n("embedded")),
        ("rejected spans", n("rejected_spans")),
        // …and a golden-query run's, for the same reason: a run that had most
        // of its questions refused says something about the ingest model.
        ("proposed", n("proposed")),
        ("candidates", n("candidates")),
        ("rejected", n("rejected")),
        ("already asked", n("duplicates")),
    ]
    .into_iter()
    .filter_map(|(label, v)| v.filter(|v| *v > 0).map(|v| format!("{v} {label}")))
    .collect()
}

#[component]
fn IngestCounters(detail: Value) -> impl IntoView {
    let parts = job_counters(&detail);
    view! {
        {(!parts.is_empty())
            .then(|| {
                view! {
                    <span class="dim mono-sm" style="grid-column:1 / -1">{parts.join(" · ")}</span>
                }
            })}
    }
}

/// One corpus: a row, and under it — only while there is something to say —
/// the job running against it, its warnings and the chunks still without a
/// vector.
#[component]
fn CorpusRow(
    c: CorpusView,
    reembed: RwSignal<Option<CorpusView>>,
    deleting: RwSignal<Option<CorpusView>>,
) -> impl IntoView {
    let state = use_docs();
    let navigate = StoredValue::new(use_navigate());
    let id = c.id;
    let label = c.corpus_id.clone();
    let hue = hue_for(&label);
    let running = Memo::new(move |_| corpus_job(id).is_some());
    let has_chunks = c.chunk_count > 0;
    let row = StoredValue::new(c.clone());

    let ingest = move || {
        start_job(
            format!("/api/docs/corpora/{id}/ingest"),
            json!({}),
            move || state.bump(),
        )
    };
    let eval = move || {
        start_job(
            "/api/docs/eval".to_string(),
            json!({ "corpus_id": id }),
            move || state.bump(),
        )
    };
    let go = move |tab: &'static str| {
        state.selected.set(id.to_string());
        navigate.get_value()(&docs_href(tab), Default::default());
    };
    let file = format!("{}.db", label.replace(['/', '@'], "-"));
    let menu = Signal::derive(move || {
        let busy = running.get();
        let file = file.clone();
        vec![
            MenuItem::new("Search it in the playground", move || go("playground")),
            MenuItem::new("Golden queries & eval", move || go("eval")),
            MenuItem::new("Export corpus file", move || {
                download(&format!("/api/docs/export?corpus_id={id}"), &file)
            })
            .title("a SQLite file holding this corpus alone"),
            MenuItem::new("Run eval", eval)
                .disabled(busy)
                .title("score its golden queries now"),
            MenuItem::new("Re-embed…", move || reembed.set(Some(row.get_value())))
                .disabled(busy)
                .title(
                    "fill in the chunks that have no vector, or move this corpus onto another \
                     embedding model",
                ),
            MenuItem::new(if has_chunks { "Re-ingest" } else { "Ingest" }, ingest)
                .disabled(busy)
                .title("fetch its sources again; unchanged pages never re-run the model"),
            MenuItem::new("Delete…", move || deleting.set(Some(row.get_value())))
                .disabled(busy)
                .title("its documents, chunks, vectors, golden queries and eval history"),
        ]
    });

    let score = match c.eval_score {
        Some(s) => format!("hit@{} {:.2}", c.eval_k.max(1), s),
        None => "never measured".into(),
    };
    let best = c
        .eval_best
        .map(|b| format!(" · best {b:.2}"))
        .unwrap_or_default();
    let sources = c
        .sources
        .iter()
        .map(|s| format!("{} ({})", s.root, s.kind))
        .collect::<Vec<_>>()
        .join(" · ");
    let ingest_title = if c.ingest_prompt_version.is_empty() {
        c.ingest_model.clone()
    } else {
        format!("{} · prompt {}", c.ingest_model, c.ingest_prompt_version)
    };
    let corpus_title = {
        let mut lines = vec![label.clone()];
        match (sources.is_empty(), c.source_kind.is_empty()) {
            (false, _) => lines.push(format!("sources: {sources}")),
            (true, false) => lines.push(format!("source kind: {}", c.source_kind)),
            (true, true) => {}
        }
        // The folding columns' values, so a narrow table hides no value.
        if !ingest_title.is_empty() {
            lines.push(format!("ingest model: {ingest_title}"));
        }
        lines.push(format!("eval: {score}{best}"));
        if !c.crawl_date.is_empty() {
            lines.push(format!("crawled: {}", c.crawl_date));
        }
        lines.join("\n")
    };
    let resident = human_bytes(c.resident_bytes.max(0) as u64);
    let unembedded = c.unembedded_chunks;
    let warnings = StoredValue::new(c.warnings.clone());
    let has_detail =
        move || running.get() || unembedded > 0 || warnings.with_value(|w| !w.is_empty());
    let status = c.status.clone();

    view! {
        <tr>
            <td title=corpus_title>
                // The source kinds are in the tooltip, with the roots they
                // belong to: a badge here cost the embedding its width.
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {label.clone()}
                </span>
            </td>
            <td>
                <span class="row-acts">
                    <span class=move || if running.get() { "chip live" } else { "chip ok" }>
                        <span class="dot"></span>
                        {move || if running.get() { "working".to_string() } else { status.clone() }}
                    </span>
                    <StatusBadges c=c.clone()/>
                </span>
            </td>
            <td class="num" title=format!("{resident} resident")>
                {grouped(c.chunk_count.max(0) as u64)}
            </td>
            <td class="num col-p2">{resident.clone()}</td>
            <td class="mono-sm clip" title=c.embed_identity.clone()>{c.embed_identity.clone()}</td>
            <td class="mono-sm dim col-p2" title=ingest_title>{c.ingest_model.clone()}</td>
            <td class="mono-sm dim col-p2">{score}{best}</td>
            // The day; the stamp is in the tooltip.
            <td class="mono-sm dim col-p3" title=c.crawl_date.clone()>
                {if c.crawl_date.is_empty() {
                    "—".to_string()
                } else {
                    c.crawl_date.get(..10).unwrap_or(&c.crawl_date).to_string()
                }}
            </td>
            <td class="actions">
                <span class="row-acts">
                    <button
                        class="btn ghost sm"
                        disabled=!has_chunks
                        title="its documents and the chunks cut from each"
                        on:click=move |_| state.browse.set(Some(row.get_value()))
                    >
                        "Browse"
                    </button>
                    <RowMenu items=menu/>
                </span>
            </td>
        </tr>
        <Show when=has_detail>
            <tr class="detail-row">
                <td colspan="9">
                    <For each=move || warnings.get_value() key=|w| w.clone() let:w>
                        <div class="status-warn">{w}</div>
                    </For>
                    {(unembedded > 0)
                        .then(|| {
                            view! {
                                <div class="dim mono-sm">
                                    {grouped(unembedded as u64)} " chunks have no vector yet"
                                </div>
                            }
                        })}
                    <CorpusJobLine id=id/>
                </td>
            </tr>
        </Show>
    }
}

/// Start a download the way a click on `<a download>` does; the router leaves
/// a `download` link alone.
pub fn download(href: &str, filename: &str) {
    let doc = document();
    let Ok(el) = doc.create_element("a") else {
        return;
    };
    let _ = el.set_attribute("href", href);
    let _ = el.set_attribute("download", filename);
    let Ok(a) = el.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    if let Some(body) = doc.body() {
        let _ = body.append_child(&a);
        a.click();
        let _ = body.remove_child(&a);
    }
}

/// Two things behind one dialog, because they are the same job with and
/// without a new pin. Filling in missing vectors keeps the corpus where it
/// is; naming a model re-pins it and drops every old vector first (half a
/// corpus in each embedding space is worse than none).
#[component]
fn ReembedModal(target: RwSignal<Option<CorpusView>>) -> impl IntoView {
    let state = use_docs();
    let open = RwSignal::new(false);
    let model = RwSignal::new(String::new());
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
        if want {
            model.set(String::new());
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    let go = move |body: Value| {
        let Some(c) = target.get_untracked() else {
            return;
        };
        open.set(false);
        start_job(
            format!("/api/docs/corpora/{}/re-embed", c.id),
            body,
            move || state.bump(),
        )
    };
    view! {
        <Modal open=open title="Re-embed corpus">
            {move || {
                target
                    .get()
                    .map(|c| {
                        view! {
                            <p>
                                <span class="mono-sm">{c.corpus_id.clone()}</span>
                                " is pinned to " <span class="mono-sm">{c.embed_identity.clone()}</span> "."
                                {(c.unembedded_chunks > 0)
                                    .then(|| {
                                        format!(
                                            " {} of its chunks have no vector yet.",
                                            grouped(c.unembedded_chunks as u64),
                                        )
                                    })}
                            </p>
                        }
                    })
            }}
            <div class="field">
                <label>"Move it to another model · optional"</label>
                // Every model the gateway can embed with — local, aux and
                // cloud — from the one catalog the other pickers use.
                <ModelPicker
                    value=model
                    tasks=&["embedding"]
                    empty_label="keep the pinned model"
                    recent_key="docs.embed"
                />
            </div>
            <p class="dim mini-note">
                "Re-pinning drops every existing vector before it starts: half a corpus in each "
                "embedding space is worse than none. Leaving the picker alone only embeds what is "
                "missing."
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn ghost" on:click=move |_| go(json!({}))>
                    "Fill in missing vectors"
                </button>
                <button
                    class="btn primary"
                    disabled=move || model.get().trim().is_empty()
                    on:click=move |_| go(json!({ "embed_model": model.get_untracked() }))
                >
                    "Re-pin and re-embed"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[component]
fn DeleteCorpusModal(target: RwSignal<Option<CorpusView>>) -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    let delete = move |_| {
        let Some(c) = target.get_untracked() else {
            return;
        };
        open.set(false);
        let id = c.id;
        spawn_local(async move {
            match crate::api::post::<MessageAck, _>(
                format!("/api/docs/corpora/{id}/delete"),
                &json!({}),
            )
            .await
            {
                Ok(v) => {
                    toasts.ok(v.message);
                    state.bump();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <Modal open=open title="Delete corpus">
            <p>
                "Delete "
                <span class="mono-sm">
                    {move || target.with(|t| t.as_ref().map(|c| c.corpus_id.clone()).unwrap_or_default())}
                </span>
                " — its documents, chunks, vectors, golden queries and eval history?"
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Keep it"
                </button>
                <button class="btn danger" on:click=delete>
                    "Delete corpus"
                </button>
            </ModalFooter>
        </Modal>
    }
}

// ---------------------------------------------------------------------------
// Corpus browser — documents → chunks
// ---------------------------------------------------------------------------

/// The part every document URL shares, cut back to a `/` so it is a
/// directory: shown once over the list, which then reads as the paths under
/// it. One document shares its own directory; none share nothing.
pub fn common_prefix<'a>(urls: impl IntoIterator<Item = &'a str>) -> String {
    let mut it = urls.into_iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut len = first.len();
    for u in it {
        let same = first
            .bytes()
            .zip(u.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        len = len.min(same);
    }
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    let shared = &first[..len];
    match shared.rfind('/') {
        Some(i) => shared[..=i].to_string(),
        None => String::new(),
    }
}

/// A document's name in the browser: its path under the folder. A URL that
/// ends in `/` — a crawl's root, a folder's index page — has no such name
/// and was drawn as an empty entry (par:PAR-12); it is named for its last
/// segment, with the slash (`axum/`, `extract/`).
pub fn doc_label(name: &str, url: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    match url.trim_end_matches('/').rsplit('/').next() {
        Some(seg) if !seg.is_empty() => format!("{seg}/"),
        _ => url.to_string(),
    }
}

/// The documents under their folders, relative to `prefix`, filtered by the
/// words typed: `("", top-level files)` first, then each folder in order.
pub fn doc_groups(
    docs: &[DocumentRow],
    prefix: &str,
    words: &[String],
) -> Vec<(String, Vec<(String, DocumentRow)>)> {
    let mut groups: Vec<(String, Vec<(String, DocumentRow)>)> = Vec::new();
    for d in docs {
        let rel = d.url.strip_prefix(prefix).unwrap_or(&d.url).to_string();
        let low = rel.to_lowercase();
        if !words.iter().all(|w| low.contains(w)) {
            continue;
        }
        let folder = rel
            .rsplit_once('/')
            .map(|(f, _)| f.to_string())
            .unwrap_or_default();
        match groups.iter_mut().find(|(f, _)| *f == folder) {
            Some((_, v)) => v.push((rel, d.clone())),
            None => groups.push((folder, vec![(rel, d.clone())])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
}

#[component]
fn CorpusBrowser() -> impl IntoView {
    let state = use_docs();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = state.browse.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && state.browse.get_untracked().is_some() {
            state.browse.set(None);
        }
    });
    view! {
        <Modal open=open title="Corpus browser" size=ModalSize::Full fill=true>
            {move || {
                state
                    .browse
                    .get()
                    .map(|c| view! { <BrowserBody c=c/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn BrowserBody(c: CorpusView) -> impl IntoView {
    let id = c.id;
    let docs = LocalResource::new(move || {
        crate::api::get::<DocumentsResponse>(format!("/api/docs/corpora/{id}/documents"))
    });
    let list = Memo::new(move |_| {
        docs.get()
            .and_then(|r| r.ok())
            .map(|d| d.documents)
            .unwrap_or_default()
    });
    let failed = Memo::new(move |_| docs.get().and_then(|r| r.err()).map(|e| e.to_string()));
    let prefix = Memo::new(move |_| list.with(|l| common_prefix(l.iter().map(|d| d.url.as_str()))));
    let query = RwSignal::new(String::new());
    let groups = Memo::new(move |_| {
        let words: Vec<String> = query
            .get()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        list.with(|l| prefix.with(|p| doc_groups(l, p, &words)))
    });
    let shown =
        Signal::derive(move || groups.with(|g| g.iter().map(|(_, v)| v.len()).sum::<usize>()));
    let total = Signal::derive(move || list.with(Vec::len));
    let open_doc = RwSignal::new(None::<DocumentRow>);
    // The first document opens with the browser: a chunk pane that starts
    // empty is a click nobody needed to make.
    Effect::new(move |_| {
        if open_doc.with_untracked(Option::is_some) {
            return;
        }
        let first = groups.with(|g| {
            g.first()
                .and_then(|(_, v)| v.first().map(|(_, d)| d.clone()))
        });
        if first.is_some() {
            open_doc.set(first);
        }
    });

    view! {
        <div class="cb">
            <div class="cb-head">
                <span class="mono-sm">{c.corpus_id.clone()}</span>
                <span class="dim">{grouped(c.chunk_count.max(0) as u64)} " chunks"</span>
                <span class="mono-sm dim cb-prefix" title=move || prefix.get()>{move || prefix.get()}</span>
                <span class="spacer"></span>
                <span class="dim mini-note">
                    "payloads are verbatim slices of the source — shown as-is, never re-rendered"
                </span>
            </div>
            <SplitPane
                side=Side::Left
                persist="docs.browser"
                label="Documents"
                badge=Signal::derive(move || crate::fmt::of(shown.get(), total.get()))
                width=(240, 30, 480)
                auto_collapse_below=420
                side_view=move || {
                    view! {
                        <div class="cb-list">
                            <FilterBar
                                query=query
                                placeholder="Filter documents"
                                shown=shown
                                total=total
                                noun="docs"
                            />
                            {move || match (docs.get().is_none(), failed.get()) {
                                (true, _) => view! { <div class="dim">"Loading…"</div> }.into_any(),
                                (_, Some(e)) => view! { <div class="notice err">{e}</div> }.into_any(),
                                _ if total.get() == 0 => {
                                    view! { <div class="empty">"No documents yet."</div> }.into_any()
                                }
                                _ => {
                                    view! {
                                        <For
                                            each=move || groups.get()
                                            key=|(f, v)| (f.clone(), v.len())
                                            let:g
                                        >
                                            <DocGroup folder=g.0 docs=g.1 open_doc=open_doc/>
                                        </For>
                                    }
                                        .into_any()
                                }
                            }}
                        </div>
                    }
                }
            >
                <div class="cb-chunks">
                    {move || match open_doc.get() {
                        None => {
                            view! { <div class="empty">"Pick a document to see its chunks."</div> }
                                .into_any()
                        }
                        Some(doc) => view! { <ChunkList doc=doc/> }.into_any(),
                    }}
                </div>
            </SplitPane>
        </div>
    }
}

/// One folder's documents under a heading with its count; the top level has
/// no heading.
#[component]
fn DocGroup(
    folder: String,
    docs: Vec<(String, DocumentRow)>,
    open_doc: RwSignal<Option<DocumentRow>>,
) -> impl IntoView {
    let n = docs.len();
    let cut = if folder.is_empty() {
        0
    } else {
        folder.len() + 1
    };
    view! {
        {(!folder.is_empty())
            .then(|| {
                view! {
                    <div class="cb-folder" title=folder.clone()>
                        <span class="cb-folder-name">{format!("{folder}/")}</span>
                        <span class="count">{n}</span>
                    </div>
                }
            })}
        {docs
            .into_iter()
            .map(|(rel, doc)| {
                let id = doc.id;
                let url = doc.url.clone();
                let name = doc_label(rel.get(cut..).unwrap_or(&rel), &url);
                let nested = cut != 0;
                let is_open = move || open_doc.with(|d| d.as_ref().map(|d| d.id) == Some(id));
                view! {
                    <button
                        class="cb-doc"
                        class:nested=nested
                        aria-current=move || is_open().to_string()
                        title=url
                        data-dock-pick
                        on:click=move |_| open_doc.set(Some(doc.clone()))
                    >
                        {name}
                    </button>
                }
            })
            .collect_view()}
    }
}

#[component]
fn ChunkList(doc: DocumentRow) -> impl IntoView {
    let id = doc.id;
    let chunks = LocalResource::new(move || {
        crate::api::get::<ChunksResponse>(format!("/api/docs/chunks?document_id={id}"))
    });
    view! {
        <div class="mini-head">
            <a href=doc.url.clone() target="_blank" rel="noreferrer">{doc.url.clone()}</a>
        </div>
        <div class="dim mono-sm cb-meta">
            "fetched " {doc.fetched_at.clone()} " · hash "
            <span title=doc.content_hash.clone()>
                {doc.content_hash.chars().take(12).collect::<String>()}
            </span>
            {move || {
                chunks
                    .get()
                    .and_then(|r| r.ok())
                    .map(|c| format!(" · {} chunks", c.chunks.len()))
            }}
        </div>
        {move || match chunks.get() {
            None => view! { <div class="dim">"Loading…"</div> }.into_any(),
            Some(Err(e)) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
            Some(Ok(c)) if c.chunks.is_empty() => {
                view! { <div class="empty">"This document produced no chunks."</div> }.into_any()
            }
            Some(Ok(c)) => {
                view! {
                    <For each=move || c.chunks.clone() key=|c| c.id.clone() let:chunk>
                        <div class="card cb-chunk">
                            <div class="row">
                                <span class="type-badge">
                                    {format!("{}–{}", chunk.span_start, chunk.span_end)}
                                </span>
                                <span class="mono-sm">{chunk.heading_path.clone()}</span>
                                <span class="spacer"></span>
                                <crate::widgets::CopyBtn
                                    text=chunk.id.clone()
                                    title="Copy the chunk id (what a golden query expects)"
                                />
                            </div>
                            <DerivedLabels title=chunk.derived_title.clone() summary=chunk.derived_summary.clone()/>
                            <pre class="preset" style="white-space:pre-wrap; word-break:break-word; margin-top:8px">
                                {chunk.payload.clone()}
                            </pre>
                        </div>
                    </For>
                }
                    .into_any()
            }
        }}
    }
}

/// Derived title/summary are LLM output, so they are labelled as such and kept
/// visually apart from the verbatim payload below them (§8).
#[component]
fn DerivedLabels(title: String, summary: String) -> impl IntoView {
    let empty = title.trim().is_empty() && summary.trim().is_empty();
    view! {
        {(!empty)
            .then(|| {
                view! {
                    <div class="dim mini-note" style="margin-top:4px">
                        <span class="type-badge">"derived"</span>
                        " " {title} {(!summary.is_empty()).then(|| format!(" — {summary}"))}
                    </div>
                }
            })}
    }
}

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

/// What the whole-DB download contains, before committing to it. The manifest
/// travels beside the file rather than inside it (§10) — the artifact stays a
/// plain SQLite database so `rsync`-ing it is still a complete backup.
#[component]
fn ExportModal(open: RwSignal<bool>) -> impl IntoView {
    let manifest = LocalResource::new(move || {
        open.get();
        crate::api::get::<ExportManifest>("/api/docs/export/manifest")
    });
    view! {
        <Modal open=open title="Export corpora">
            <div class="wiz">
                <p class="dim mini-note">
                    "The export is the corpus database itself, checkpointed first — one file, "
                    "restorable by copying it back or importing it on another gateway."
                </p>
                {move || match manifest.get() {
                    None => view! { <div class="dim">"Reading the manifest…"</div> }.into_any(),
                    Some(Err(e)) => {
                        view! { <div class="notice err">{e.to_string()}</div> }.into_any()
                    }
                    Some(Ok(m)) => {
                        view! {
                            <div class="detail-grid" style="margin-top:10px">
                                <span class="dim">"Schema"</span>
                                <span class="mono-sm">{format!("v{}", m.schema_version)}</span>
                                <span class="dim">"Size"</span>
                                <span class="mono-sm">{human_bytes(m.byte_size)}</span>
                            </div>
                            <table class="data" style="margin-top:8px">
                                <thead>
                                    <tr>
                                        <th>"Corpus"</th>
                                        <th>"Embedding"</th>
                                        <th style="text-align:right">"Chunks"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For
                                        each=move || m.corpora.clone()
                                        key=|c| c.corpus_id.clone()
                                        let:c
                                    >
                                        <tr>
                                            <td class="mono-sm">{c.corpus_id.clone()}</td>
                                            <td class="dim mono-sm">
                                                {format!(
                                                    "{}/{} @{}",
                                                    c.embed_upstream,
                                                    c.embed_model,
                                                    c.embed_dims,
                                                )}
                                            </td>
                                            <td class="num">{grouped(c.chunk_count.max(0) as u64)}</td>
                                        </tr>
                                    </For>
                                </tbody>
                            </table>
                        }
                            .into_any()
                    }
                }}
                <div class="row" style="justify-content:flex-end; margin-top:12px">
                    <a class="btn primary" href="/api/docs/export" download>
                        "Download quickdoc.db"
                    </a>
                </div>
            </div>
        </Modal>
    }
}

/// Import is two deliberate steps: a **dry run** that runs every check and
/// writes nothing, then the commit. `replace` is never implied — overwriting a
/// `library@version` that already exists here is a choice, so the checkbox says
/// so and the dry run reports it first.
#[component]
fn ImportModal(open: RwSignal<bool>) -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    let file = RwSignal::new(None::<web_sys::File>);
    let name = RwSignal::new(String::new());
    let replace = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let report = RwSignal::new(None::<ImportReport>);
    let error = RwSignal::new(None::<String>);

    let send = move |validate_only: bool| {
        let Some(f) = file.get_untracked() else {
            toasts.err("pick a corpus file first");
            return;
        };
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        report.set(None);
        error.set(None);
        let url = format!(
            "/api/docs/import?replace={}&validate_only={validate_only}",
            replace.get_untracked()
        );
        spawn_local(async move {
            let res = crate::api::post_raw::<ImportReport>(url, f).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    let committed = !r.dry_run;
                    report.set(Some(r));
                    if committed {
                        toasts.ok("corpus file imported");
                        state.bump();
                    }
                }
                // Verbatim: these messages name the corpus and the model it
                // wants, which is the whole point of validating first.
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let pick = move |ev: web_sys::Event| {
        let el: web_sys::HtmlInputElement = event_target(&ev);
        let picked = el.files().and_then(|l| l.get(0));
        name.set(picked.as_ref().map(|f| f.name()).unwrap_or_default());
        file.set(picked);
        report.set(None);
        error.set(None);
        if file.get_untracked().is_some() {
            send(true);
        }
    };

    view! {
        <Modal open=open title="Import a corpus file">
            <div class="wiz">
                <p class="dim mini-note">
                    "A corpus file is a plain SQLite database exported from this or another "
                    "gateway. It is validated first — schema version and whether the models it "
                    "is pinned to resolve here — and nothing is written until you commit."
                </p>
                <div class="field" style="margin-top:10px">
                    <label>"Corpus file (.db)"</label>
                    <input class="input file" type="file" accept=".db,.sqlite,.sqlite3" on:change=pick/>
                </div>
                <div class="row" style="margin-top:8px">
                    <label class="row dim" style="gap:5px">
                        <input
                            type="checkbox"
                            prop:checked=move || replace.get()
                            on:change=move |ev| {
                                replace.set(event_target_checked(&ev));
                                if file.get_untracked().is_some() {
                                    send(true);
                                }
                            }
                        />
                        "overwrite a library@version that already exists here"
                    </label>
                </div>

                {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                {move || {
                    report
                        .get()
                        .map(|r| {
                            let head = if r.dry_run {
                                "Validated — nothing written yet".to_string()
                            } else {
                                "Imported".to_string()
                            };
                            view! {
                                <div class="wiz-ok" style="margin-top:12px">
                                    {head} " · file schema v" {r.file_schema_version}
                                    ", this gateway v" {r.schema_version}
                                </div>
                                <table class="data">
                                    <thead>
                                        <tr>
                                            <th>"Corpus"</th>
                                            <th>"Pinned embedding"</th>
                                            <th>"Resolves as"</th>
                                            <th style="text-align:right">"Chunks"</th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        <For
                                            each=move || r.imported.clone()
                                            key=|c| c.corpus_id.clone()
                                            let:c
                                        >
                                            <tr>
                                                <td class="mono-sm">
                                                    {c.corpus_id.clone()}
                                                    {c
                                                        .replaced
                                                        .then(|| {
                                                            view! { <span class="type-badge">"replaced"</span> }
                                                        })}
                                                </td>
                                                <td class="dim mono-sm">{c.embed_model.clone()}</td>
                                                <td class="mono-sm">
                                                    {c.embed_alias.clone().unwrap_or_else(|| "—".into())}
                                                </td>
                                                <td class="num">{grouped(c.chunks.max(0) as u64)}</td>
                                            </tr>
                                        </For>
                                    </tbody>
                                </table>
                            }
                        })
                }}

                <div class="row" style="justify-content:flex-end; margin-top:12px">
                    <button
                        class="btn ghost"
                        disabled=move || busy.get() || file.get().is_none()
                        on:click=move |_| send(true)
                    >
                        {move || if busy.get() { "Checking…" } else { "Re-validate" }}
                    </button>
                    <button
                        class="btn primary"
                        disabled=move || {
                            busy.get() || report.get().is_none_or(|r| !r.dry_run)
                        }
                        on:click=move |_| send(false)
                    >
                        "Import"
                    </button>
                </div>
            </div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The job line reads the executor's typed detail, in pipeline order, and
    /// leaves out what has not happened yet — without leaving out a count that
    /// has. A rejected span is the one nobody would think to look for, so it is
    /// there as soon as it is non-zero.
    #[test]
    fn job_counters_read_the_pipeline_in_order() {
        let detail = json!({
            "corpus": "axum@0.8", "url": "https://docs.rs/axum",
            "fetched": 12, "unchanged": 3, "extracted": 9, "failed": 0,
            "chunks": 140, "embedded": 0, "rejected_spans": 2,
        });
        assert_eq!(
            job_counters(&detail),
            [
                "12 fetched",
                "3 unchanged",
                "9 extracted",
                "140 chunks",
                "2 rejected spans"
            ],
        );
    }

    /// The shared prefix is a directory, shown once; one document shares its
    /// own directory, and URLs that share nothing share nothing.
    #[test]
    fn the_shared_prefix_is_cut_back_to_a_directory() {
        let urls = [
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/01_introduction.md",
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/async/10_resources.md",
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/SUMMARY.md",
        ];
        assert_eq!(
            common_prefix(urls),
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/"
        );
        assert_eq!(
            common_prefix(["https://docs.rs/axum/latest/axum/index.html"]),
            "https://docs.rs/axum/latest/axum/"
        );
        assert_eq!(common_prefix(["a.md", "b.md"]), "");
        assert_eq!(common_prefix(std::iter::empty()), "");
    }

    /// Top-level files first, then each folder; the words filter the paths
    /// under the prefix, not the prefix itself.
    #[test]
    fn documents_group_by_folder_under_the_prefix() {
        let doc = |id: i64, url: &str| DocumentRow {
            id,
            url: url.to_string(),
            ..Default::default()
        };
        let p = "https://x.dev/src/";
        let docs = vec![
            doc(1, "https://x.dev/src/intro.md"),
            doc(2, "https://x.dev/src/router/16_routes.md"),
            doc(3, "https://x.dev/src/async/10_resources.md"),
            doc(4, "https://x.dev/src/zeta.md"),
        ];
        let g = doc_groups(&docs, p, &[]);
        let shape: Vec<(String, Vec<String>)> = g
            .iter()
            .map(|(f, v)| (f.clone(), v.iter().map(|(r, _)| r.clone()).collect()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    "".to_string(),
                    vec!["intro.md".to_string(), "zeta.md".to_string()]
                ),
                (
                    "async".to_string(),
                    vec!["async/10_resources.md".to_string()]
                ),
                (
                    "router".to_string(),
                    vec!["router/16_routes.md".to_string()]
                ),
            ]
        );
        let only = doc_groups(&docs, p, &["rout".to_string()]);
        assert_eq!(only.len(), 1);
        assert!(doc_groups(&docs, p, &["x.dev".to_string()]).is_empty());

        // A crawl's root and a folder's index page end in "/": each keeps a
        // place in its group and gets a name, never an empty entry.
        let docs = vec![
            doc(1, "https://docs.rs/axum/0.8.0/axum/"),
            doc(2, "https://docs.rs/axum/0.8.0/axum/extract/"),
            doc(
                3,
                "https://docs.rs/axum/0.8.0/axum/extract/struct.Path.html",
            ),
        ];
        let p = common_prefix(docs.iter().map(|d| d.url.as_str()));
        assert_eq!(p, "https://docs.rs/axum/0.8.0/axum/");
        let g = doc_groups(&docs, &p, &[]);
        let names: Vec<(String, Vec<String>)> = g
            .iter()
            .map(|(f, v)| {
                let cut = if f.is_empty() { 0 } else { f.len() + 1 };
                let names = v
                    .iter()
                    .map(|(r, d)| doc_label(r.get(cut..).unwrap_or(r), &d.url))
                    .collect();
                (f.clone(), names)
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("".to_string(), vec!["axum/".to_string()]),
                (
                    "extract".to_string(),
                    vec!["extract/".to_string(), "struct.Path.html".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn a_docs_tab_is_a_path() {
        assert_eq!(docs_href("corpora"), "/docs");
        assert_eq!(docs_href("requests"), "/docs/requests");
        assert_eq!(docs_href("nope"), "/docs");
    }

    /// A re-embed or an eval reports a different shape entirely, and a job that
    /// has only just started reports nothing — neither is an error, and neither
    /// invents a number.
    #[test]
    fn a_detail_without_ingest_counters_says_nothing() {
        assert!(job_counters(&json!({ "corpus": "axum@0.8", "k": 10, "mrr": 0.5 })).is_empty());
        assert!(job_counters(&Value::Null).is_empty());
    }
}
