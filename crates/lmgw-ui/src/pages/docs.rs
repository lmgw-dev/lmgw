//! Docs — the quickdoc corpora plane (quickdoc §11).
//!
//! Four tabs under one header bar (the Docs title with the rerank stage and
//! what the corpora hold resident, the tabs, and Import, Export and Ingest a
//! library as icon buttons): the **corpora** as a list beside the selected
//! corpus on the shared split view ([`list`], [`detail`]; `/docs` and
//! `/docs/corpora/<id>`, with the same badges `docs__resolve` reports to a
//! model, so owner and agent read the same status), the agent **request
//! queue**, the **search playground** over the debug endpoint and the **eval
//! dashboard**; the **ingest wizard**, the **corpus browser** ([`browser`])
//! and the export and import dialogs ([`modals`]) open over them.
//!
//! Job progress for `ingest` / `re_embed` / `eval_run` / `golden_gen` comes off
//! the shared jobs feed exactly the way Downloads consumes `hf_download` (§9c): the
//! durable rows are re-fetched when a job starts or ends, and every tick in
//! between arrives on the bus, so this page never polls.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_params_map};
use lmgw_api_types::{
    CorpusView, DocRequestsResponse, DocsJobStarted, DocsOverview, JobRow, MessageAck,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::fmt::fmt_bytes;
use crate::widgets::list_pane::{ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Dot, Ico, Icon, NavTab, PageFrame, PageMode, SubNav, Tone};

/// The corpus browser: documents beside their chunks.
mod browser;
/// The selected corpus, and the status page.
mod detail;
/// The corpus list pane.
mod list;
/// Re-embed, Delete, Export and Import.
mod modals;

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
    /// The corpus whose Re-embed or Delete dialog is up.
    pub reembed: RwSignal<Option<CorpusView>>,
    pub deleting: RwSignal<Option<CorpusView>>,
    pub export_open: RwSignal<bool>,
    pub import_open: RwSignal<bool>,
    /// The jobs running against the corpora, by corpus id: one pass over
    /// the jobs feed per tick, read by every row and view.
    pub jobs: Memo<BTreeMap<i64, JobRow>>,
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

    /// The job running against corpus `id` right now, if any. Tracked.
    pub fn job(&self, id: i64) -> Option<JobRow> {
        self.jobs.with(|m| m.get(&id).cloned())
    }

    /// Whether a job runs against corpus `id`. Tracked.
    pub fn has_job(&self, id: i64) -> bool {
        self.jobs.with(|m| m.contains_key(&id))
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

/// The jobs running against the corpora right now, by corpus id.
fn corpus_jobs() -> Memo<BTreeMap<i64, JobRow>> {
    let live = crate::live::use_live();
    Memo::new(move |_| {
        live.jobs.with(|js| {
            js.as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|j| CORPUS_JOB_KINDS.contains(&j.kind.as_str()))
                .filter_map(|j| {
                    let id = j.key.as_deref()?.strip_prefix("corpus:")?.parse().ok()?;
                    Some((id, j.clone()))
                })
                .collect()
        })
    })
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

/// The page's tabs, as the path segment after `/docs`. Corpora is `/docs`, a
/// selected corpus `/docs/corpora/<id>`.
pub const DOCS_TABS: &[&str] = &["corpora", "requests", "playground", "eval"];

/// Where a tab lives.
pub fn docs_href(tab: &str) -> String {
    match tab {
        "requests" | "playground" | "eval" => format!("/docs/{tab}"),
        _ => "/docs".to_string(),
    }
}

/// Where a corpus is selected.
pub fn corpus_href(id: i64) -> String {
    format!("/docs/corpora/{id}")
}

/// The corpus list's own query keys (`q`, `show`), carried from one
/// selection to the next.
const LIST_KEYS: &[&str] = &["q", "show"];

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
        reembed: RwSignal::new(None),
        deleting: RwSignal::new(None),
        export_open: RwSignal::new(false),
        import_open: RwSignal::new(false),
        jobs: corpus_jobs(),
    };
    provide_context(state);
    let pending = use_docs_pending();
    let tab = crate::url_state::use_view("tab", DOCS_TABS, "corpora");
    let location = use_location();

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

    // The Corpora tab leads back to the corpus it showed last.
    let corpora_path = RwSignal::new(docs_href("corpora"));
    // Read off the path alone: on a move the path changes before the tab
    // parameter does, and the tab would file the new path under Corpora.
    Effect::new(move |_| {
        let p = location.pathname.get();
        let corpora = p.trim_end_matches('/') == "/docs" || p.starts_with("/docs/corpora");
        if corpora && corpora_path.get_untracked() != p {
            corpora_path.set(p);
        }
    });
    // The tabs carry their counts; the queue's is amber because it is agents
    // waiting on the owner (§11).
    let tabs = Signal::derive(move || {
        let mut corpora = NavTab::new("Corpora", corpora_path.get()).exact();
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
    // What the rerank stage calls and what the corpora cost resident, on
    // every tab.
    let sub = move || {
        let resident = fmt_bytes(state.corpora.with(|cs| {
            cs.iter()
                .map(|c| c.resident_bytes.max(0) as u64)
                .sum::<u64>()
        }));
        let rerank = state
            .rerank_model
            .get()
            .map_or_else(|| "No reranker".to_string(), |m| format!("Reranker {m}"));
        format!("{rerank} · {resident} in memory")
    };

    let sub_title = move || {
        let rerank = if state.rerank_model.with(Option::is_some) {
            "A reranker re-reads the best matches of a search and puts the most relevant first."
        } else {
            "A reranker re-reads the best matches of a search and puts the most relevant first; none is set, so results keep the order the search found them in."
        };
        format!(
            "{rerank}\nIn memory is the size of the search index the corpora hold in RAM while lmgw runs."
        )
    };

    view! {
        <PageFrame
            title="Docs"
            sub=sub
            sub_title=sub_title
            mode=PageMode::Fill
            class="docs"
            head_extra=move || view! { <SubNav tabs=tabs/> }
            actions=move || {
                view! {
                    <button
                        type="button"
                        class="icon-btn"
                        title="Import a corpus file"
                        aria-label="Import a corpus file"
                        on:click=move |_| state.import_open.set(true)
                    >
                        <Icon ico=Ico::Upload/>
                    </button>
                    <button
                        type="button"
                        class="icon-btn"
                        title="Export the corpora"
                        aria-label="Export the corpora"
                        on:click=move |_| state.export_open.set(true)
                    >
                        <Icon ico=Ico::Download/>
                    </button>
                    <button
                        type="button"
                        class="icon-btn"
                        title="Ingest a library"
                        aria-label="Ingest a library"
                        on:click=move |_| state.wizard.set(Some(WizardSeed::default()))
                    >
                        <Icon ico=Ico::Plus/>
                    </button>
                }
            }
        >
            {move || match tab.get() {
                "requests" => view! { <super::docs_requests::RequestQueue/> }.into_any(),
                "playground" => view! { <super::docs_search::Playground/> }.into_any(),
                "eval" => view! { <super::docs_eval::EvalDashboard/> }.into_any(),
                _ => view! { <CorporaSplit/> }.into_any(),
            }}

            <super::docs_wizard::IngestWizard/>
            <browser::CorpusBrowser/>
            <modals::ReembedModal target=state.reembed/>
            <modals::DeleteCorpusModal target=state.deleting/>
            <modals::ExportModal open=state.export_open/>
            <modals::ImportModal open=state.import_open/>
        </PageFrame>
    }
}

/// Below 800 px of pane the corpora show one pane at a time, as Models'.
const CONF: SplitConf = SplitConf {
    class: "docs-split",
    width_key: "docs.list.width",
    list_width: (240, 30, 380),
    detail_min: 440,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// The corpora: the list beside the selected corpus.
#[component]
fn CorporaSplit() -> impl IntoView {
    let params = use_params_map();
    let raw = Memo::new(move |_| params.with(|p| p.get_str("id").map(str::to_string)));
    let sel = Memo::new(move |_| raw.get().and_then(|s| s.parse::<i64>().ok()));
    // The corpus selected in the list is the one the playground, the eval
    // dashboard and the browser act on.
    let docs = use_docs();
    Effect::new(move |_| {
        let Some(id) = sel.get() else { return };
        let listed = docs.corpora.with(|cs| cs.iter().any(|c| c.id == id));
        let id = id.to_string();
        if listed && docs.selected.with_untracked(|s| *s != id) {
            docs.selected.set(id);
        }
    });
    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let nav = Nav::new();
    let refs = ListRefs::new();
    let has_sel = Signal::derive(move || raw.with(Option::is_some));
    let open_row = Callback::new(move |(id, replace): (i64, bool)| {
        nav.open(&corpus_href(id), LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| nav.back("/docs", LIST_KEYS, refs));
    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=refs.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || {
                view! { <list::ListPane sel=sel narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <detail::DetailPane raw=raw sel=sel narrow=narrow back=back/> }
            })
        />
    }
}

// ---------------------------------------------------------------------------
// A corpus's state
// ---------------------------------------------------------------------------

/// The facets a corpus can be filed under: the badges an agent sees on
/// `docs__resolve`, so "which corpora need me" is one click.
pub(crate) fn corpus_facets(c: &CorpusView) -> Vec<&'static str> {
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
pub(crate) fn corpus_matches(c: &CorpusView, words: &[String]) -> bool {
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

/// The corpus's state as a dot and a word: the job running against it, or
/// the most pressing badge, or its own status.
pub(crate) fn corpus_state(c: &CorpusView, running: bool) -> (Dot, String) {
    if running {
        return (Dot::Busy, "Working".into());
    }
    if c.embed_status == "re_embed_required" {
        return (Dot::Error, "Re-embed required".into());
    }
    if c.status == "failed" {
        return (Dot::Error, "Failed".into());
    }
    if c.eval_status == "regression" {
        return (Dot::Error, "Eval regression".into());
    }
    if c.chunk_count == 0 {
        return (Dot::Off, "Empty".into());
    }
    match c.status.as_str() {
        "" | "ready" | "ok" => (Dot::Running, "Ready".into()),
        other => {
            let mut w = other.replace('_', " ");
            if let Some(f) = w.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            (Dot::Busy, w)
        }
    }
}

/// The eval score in a few words.
pub(crate) fn score_words(c: &CorpusView, digits: usize) -> String {
    match c.eval_score {
        Some(s) => match c.eval_k {
            Some(k) => format!("hit@{k} {s:.digits$}"),
            None => format!("hit rate {s:.digits$}"),
        },
        None => "never measured".into(),
    }
}

// ---------------------------------------------------------------------------
// A corpus's job
// ---------------------------------------------------------------------------

/// Live progress for whatever job owns this corpus right now, as a row of a
/// boxed group — the ticks are the feed's, the unit is the kind's own
/// (documents, chunks, queries), the counters under it, and Cancel.
#[component]
pub fn CorpusJobRow(id: i64) -> impl IntoView {
    let toasts = use_toasts();
    let docs = use_docs();
    let job = Memo::new(move |_| docs.job(id));
    let cancel = move |_| {
        let Some(j) = job.get_untracked() else { return };
        spawn_local(async move {
            match crate::api::post::<MessageAck, _>("/api/op/job_cancel", &json!({ "id": j.id }))
                .await
            {
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
                    let parts = job_counters(&j.detail);
                    let kind = j.kind.replace('_', " ");
                    view! {
                        <div class="prow stack docs-job">
                            <div class="prow-text">
                                <span class="prow-title">{kind} " · " {j.stage.clone()}</span>
                                {(!url.is_empty())
                                    .then(|| {
                                        let tip = url.clone();
                                        view! { <span class="prow-sub docs-job-url" title=tip>{url}</span> }
                                    })}
                                {(!parts.is_empty())
                                    .then(|| view! { <span class="prow-sub">{parts.join(" · ")}</span> })}
                            </div>
                            <div class="prow-end">
                                <span class="prow-value">{counts}</span>
                                <button type="button" class="btn sm" on:click=cancel>
                                    "Cancel"
                                </button>
                            </div>
                            <div class="progress">
                                <i style=format!("width:{}%", j.percent.unwrap_or(3))></i>
                            </div>
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

/// Start a download the way a click on `<a download>` does: the kit's, which
/// the Chat's downloads use too (`lmgw_ui_kit::http::bytes::click_download`,
/// whose doc says what the desktop shell does with it).
pub use lmgw_ui_kit::http::bytes::click_download as download;

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

    #[test]
    fn a_docs_tab_is_a_path() {
        assert_eq!(docs_href("corpora"), "/docs");
        assert_eq!(docs_href("requests"), "/docs/requests");
        assert_eq!(docs_href("nope"), "/docs");
        assert_eq!(corpus_href(5), "/docs/corpora/5");
    }

    /// A re-embed or an eval reports a different shape entirely, and a job that
    /// has only just started reports nothing — neither is an error, and neither
    /// invents a number.
    #[test]
    fn a_detail_without_ingest_counters_says_nothing() {
        assert!(job_counters(&json!({ "corpus": "axum@0.8", "k": 10, "mrr": 0.5 })).is_empty());
        assert!(job_counters(&Value::Null).is_empty());
    }

    #[test]
    fn a_corpus_state_says_the_most_pressing_badge() {
        let c = CorpusView {
            chunk_count: 10,
            status: "ready".into(),
            ..Default::default()
        };
        assert_eq!(corpus_state(&c, false), (Dot::Running, "Ready".into()));
        assert_eq!(corpus_state(&c, true).0, Dot::Busy);
        let empty = CorpusView::default();
        assert_eq!(corpus_state(&empty, false).0, Dot::Off);
        let re = CorpusView {
            chunk_count: 3,
            embed_status: "re_embed_required".into(),
            ..Default::default()
        };
        assert_eq!(corpus_state(&re, false).1, "Re-embed required");
        let ing = CorpusView {
            chunk_count: 3,
            status: "ingesting".into(),
            ..Default::default()
        };
        assert_eq!(corpus_state(&ing, false).1, "Ingesting");
        assert_eq!(score_words(&c, 2), "never measured");
    }
}
