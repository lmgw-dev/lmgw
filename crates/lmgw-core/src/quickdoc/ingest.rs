//! The `ingest` job (quickdoc §8): fetch → extract → validate → chunk → embed.
//!
//! ```text
//!  code   fetch (fence + robots + delay)  ─→ sniff kind ─→ document text
//!  code   content_hash gate               ─→ unchanged pages never reach a model
//!  code   window by the ingest model's REAL context length
//!  model  emit_extraction: spans + metadata, schema-constrained
//!  code   slice the original text, check the anchors, reject what fails
//!  code   chunk + embed with the corpus's pinned embedder
//! ```
//!
//! The engine for the one model-driven step is the server-side tool loop
//! (`agent.rs`, §21): the model is given exactly one tool, and a rejected span
//! comes back to it as that tool's result, so a retry is an ordinary next turn
//! under the loop's visible budgets rather than bespoke retry machinery.
//!
//! **What the model cannot do here:** write a payload (it emits line ranges,
//! code slices), reach a URL (there is no `fetch` tool — §8's contract point 1
//! is "code fetches"), or have a bad span quietly accepted (validation discards,
//! never repairs).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use quickdoc_core::embed::Embedder;
use quickdoc_core::ingest::{
    self, extract, prompt, AcceptedSection, Extraction, Fence, LineIndex, SourceKind,
};
use quickdoc_core::store as qstore;
use quickdoc_core::store::{Corpus, NewChunk};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::embed::InProcessEmbedder;
use super::fetch::Fetcher;
use crate::agent::{
    self, Budget, CollectSink, DeltaSink, ResolvedTool, RunConfig, StopReason, ToolExecutor,
    ToolOutcome, TurnRunner,
};
use crate::config::{Route, ROUTER_UPSTREAM_ID};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Completion, Message, Params, Role, ToolDef, ToolResultBlock};
use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::proxy;
use crate::state::SharedState;
use crate::telemetry::INGEST_PROTO;

/// `server_label` of the extraction tool in the loop's events. The loop needs
/// every tool to be owned by *someone*; this one is owned by the job.
pub const TOOL_LABEL: &str = "quickdoc";

/// Request payload of an `ingest` job.
///
/// Just the corpus: everything the run needs — which model extracts, under which
/// prompt version, into which embedding space, from which sources — is pinned on
/// the corpus row (§4). A job that could override any of those could produce a
/// corpus that does not match its own metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub corpus_id: i64,
}

/// Kind-specific progress detail. `done`/`total` are **documents**.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub corpus: String,
    /// What the job is working on right now.
    pub url: String,
    pub fetched: u64,
    /// Skipped because `content_hash` was unchanged — the model never ran.
    pub unchanged: u64,
    pub extracted: u64,
    pub failed: u64,
    pub chunks: u64,
    pub embedded: u64,
    /// Spans the verbatim check refused. Non-zero is normal (the model corrects
    /// and re-emits); it is surfaced because a corpus built with many rejections
    /// says something about the ingest model.
    pub rejected_spans: u64,
}

/// One live ingest per corpus.
pub fn job_key(corpus_id: i64) -> String {
    format!("corpus:{corpus_id}")
}

/// Start ingesting a corpus, or report the job already doing it.
pub async fn start(state: &SharedState, corpus: &Corpus) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::Ingest,
        Some(job_key(corpus.id)),
        format!("ingest {}", corpus.corpus_id()),
        json!({ "corpus_id": corpus.id }),
    )
    .await
}

pub struct IngestExecutor;

#[async_trait]
impl JobExecutor for IngestExecutor {
    fn kind(&self) -> JobKind {
        JobKind::Ingest
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("ingest input: {e}"))?;
        let corpus = qstore::get_corpus(&ctx.state.corpus, input.corpus_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("corpus {} no longer exists", input.corpus_id))?;
        match run_ingest(&ctx, &corpus).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                let _ = qstore::set_corpus_status(&ctx.state.corpus, corpus.id, "failed").await;
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// A document the planning pass decided to fetch.
struct Planned {
    source_id: i64,
    url: String,
    fence: Fence,
    /// Already fetched during planning (source roots); `None` means fetch it.
    body: Option<String>,
    content_type: Option<String>,
}

async fn run_ingest(ctx: &JobCtx, corpus: &Corpus) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let snap = state.snapshot();

    // Both models are resolved before anything is fetched: a corpus pinned to a
    // model that is gone, or to a reranker, must fail in the first second rather
    // than after an hour of crawling.
    let embedder = InProcessEmbedder::for_corpus(state.clone(), corpus)
        .await
        .map_err(|e| e.to_string())?;
    let plan = ExtractionPlan::build(state, corpus).await?;

    let mut detail = Detail {
        corpus: corpus.corpus_id(),
        ..Default::default()
    };
    qstore::set_corpus_status(&state.corpus, corpus.id, "ingesting")
        .await
        .map_err(|e| e.to_string())?;
    ctx.progress(progress(0, None, "planning", &detail)).await;

    let mut fetcher = Fetcher::new(state.http.clone(), snap.settings.docs_fetch_delay_ms);
    let sources = qstore::list_sources(&state.corpus, corpus.id)
        .await
        .map_err(|e| e.to_string())?;
    if sources.is_empty() {
        return Err(format!(
            "corpus {} has no sources to ingest",
            corpus.corpus_id()
        ));
    }
    let (documents, root_kind) = plan_documents(&mut fetcher, &sources, &mut detail).await?;
    let total = documents.len() as u64;
    ctx.progress(progress(0, Some(total), "fetching", &detail))
        .await;

    let batch = snap.settings.docs_embed_batch.max(1) as usize;
    let mut done = 0u64;
    let mut canceled = false;
    for planned in documents {
        // Cancellation lands on a document boundary: a half-extracted document
        // has nothing worth keeping, and the boundary is where the corpus is
        // consistent.
        if ctx.canceled() {
            canceled = true;
            break;
        }
        detail.url = planned.url.clone();
        ctx.progress(progress(done, Some(total), "fetching", &detail))
            .await;
        match one_document(ctx, corpus, &plan, &embedder, batch, &mut fetcher, planned).await {
            Ok(d) => detail.merge(d),
            Err(e) => {
                detail.failed += 1;
                tracing::warn!("ingest {}: {e}", corpus.corpus_id());
            }
        }
        done += 1;
        ctx.progress(progress(done, Some(total), "extracting", &detail))
            .await;
    }

    let chunks = qstore::refresh_chunk_count(&state.corpus, corpus.id)
        .await
        .map_err(|e| e.to_string())?;
    qstore::set_corpus_crawl(
        &state.corpus,
        corpus.id,
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        root_kind.as_str(),
    )
    .await
    .map_err(|e| e.to_string())?;

    // A cancelled or partly failed run still leaves a queryable corpus when it
    // wrote any chunks, and the chunk count says how much of one. Claiming
    // `ready` with nothing in it would be the lie.
    //
    // So would claiming it while chunks still have no vector. The `content_hash`
    // gate skips unchanged documents without re-embedding them, so a corpus left
    // mid-re-embed keeps its NULL vectors right through a re-ingest and the KNN
    // half of the search would silently cover only part of it. That is exactly
    // what `re_embed_required` names, and `query::corpus_status` cannot notice it
    // on its own — it is sync and reads no DB, so the badge has to be written
    // here (`store::count_unembedded` documents itself as this guard).
    let unembedded = qstore::count_unembedded(&state.corpus, corpus.id)
        .await
        .map_err(|e| e.to_string())?;
    let status = match (chunks, unembedded) {
        (0, _) => "failed",
        (_, 0) => "ready",
        _ => "re_embed_required",
    };
    qstore::set_corpus_status(&state.corpus, corpus.id, status)
        .await
        .map_err(|e| e.to_string())?;
    // Fulfilment follows "the ingest produced a corpus", not "the corpus is
    // pristine" (§10): a degraded-but-queryable corpus still answers the agent
    // that asked for it, and the badge tells it what it is getting.
    if chunks > 0 {
        let _ = qstore::fulfill_doc_requests(&state.corpus, &corpus.library, &corpus.version).await;
    }
    detail.url.clear();
    ctx.progress(progress(done, Some(total), status, &detail))
        .await;

    if canceled {
        return Ok(JobOutcome::Canceled);
    }
    if detail.failed > 0 && detail.extracted == 0 {
        return Err(format!(
            "every document failed ({} of {total}) — nothing was ingested",
            detail.failed
        ));
    }
    Ok(JobOutcome::Done(json!({
        "corpus_id": corpus.id,
        "documents": total,
        "unchanged": detail.unchanged,
        "extracted": detail.extracted,
        "failed": detail.failed,
        "chunks": chunks,
        "rejected_spans": detail.rejected_spans,
    })))
}

fn progress(done: u64, total: Option<u64>, stage: &str, detail: &Detail) -> JobProgress {
    JobProgress {
        done,
        total,
        stage: stage.to_string(),
        detail: serde_json::to_value(detail).unwrap_or(Value::Null),
    }
}

impl Detail {
    fn merge(&mut self, d: DocumentOutcome) {
        self.fetched += 1;
        self.unchanged += u64::from(d.unchanged);
        self.extracted += u64::from(d.extracted);
        self.chunks += d.chunks;
        self.embedded += d.embedded;
        self.rejected_spans += d.rejected_spans;
    }
}

/// Fetch every source root and expand the links it carries — the *code* follows
/// links, and only inside the source's fence (§8).
async fn plan_documents(
    fetcher: &mut Fetcher,
    sources: &[qstore::Source],
    detail: &mut Detail,
) -> Result<(Vec<Planned>, SourceKind), String> {
    let mut out: Vec<Planned> = Vec::new();
    let mut root_kind = SourceKind::Markdown;
    let mut errors: Vec<String> = Vec::new();
    for (i, source) in sources.iter().enumerate() {
        let fence = Fence::new(&source.root, &source.fence);
        let fetched = match fetcher.get(&source.root, &fence).await {
            Ok(f) => f,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let kind = SourceKind::parse(&source.kind).unwrap_or_else(|| {
            ingest::sniff(&source.root, fetched.content_type.as_deref(), &fetched.body)
        });
        if i == 0 {
            root_kind = kind;
        }
        detail.url = source.root.clone();
        let links = ingest::linked_urls(kind, &fetched.body, &source.root);
        // The root first, then what it points at, so progress reads in the
        // order the crawl actually happens.
        out.push(Planned {
            source_id: source.id,
            url: source.root.clone(),
            fence: fence.clone(),
            body: Some(fetched.body),
            content_type: fetched.content_type,
        });
        for link in links {
            if fence.allows(&link) && !out.iter().any(|p| p.url == link) {
                out.push(Planned {
                    source_id: source.id,
                    url: link,
                    fence: fence.clone(),
                    body: None,
                    content_type: None,
                });
            }
        }
    }
    if out.is_empty() {
        return Err(format!("no source could be fetched: {}", errors.join("; ")));
    }
    Ok((out, root_kind))
}

#[derive(Default)]
struct DocumentOutcome {
    unchanged: bool,
    extracted: bool,
    chunks: u64,
    embedded: u64,
    rejected_spans: u64,
}

async fn one_document(
    ctx: &JobCtx,
    corpus: &Corpus,
    plan: &ExtractionPlan,
    embedder: &InProcessEmbedder,
    batch: usize,
    fetcher: &mut Fetcher,
    planned: Planned,
) -> Result<DocumentOutcome, String> {
    let state = &ctx.state;
    let (body, content_type) = match planned.body {
        Some(b) => (b, planned.content_type),
        None => {
            let f = fetcher.get(&planned.url, &planned.fence).await?;
            (f.body, f.content_type)
        }
    };

    // The hash is over the bytes that were served, so a page whose rendering
    // changed but whose source did not still skips the model.
    let hash = qstore::content_hash(&body);
    let (document_id, changed) =
        qstore::upsert_document(&state.corpus, planned.source_id, &planned.url, &hash)
            .await
            .map_err(|e| e.to_string())?;
    if !changed {
        return Ok(DocumentOutcome {
            unchanged: true,
            ..Default::default()
        });
    }

    let outcome = extract_and_store(
        ctx,
        corpus,
        plan,
        embedder,
        batch,
        document_id,
        &planned.url,
        content_type.as_deref(),
        &body,
    )
    .await;
    if outcome.is_err() {
        // The row already carries the new hash, so without this the gate would
        // skip this document on every future run.
        let _ = qstore::mark_document_stale(&state.corpus, document_id).await;
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn extract_and_store(
    ctx: &JobCtx,
    corpus: &Corpus,
    plan: &ExtractionPlan,
    embedder: &InProcessEmbedder,
    batch: usize,
    document_id: i64,
    url: &str,
    content_type: Option<&str>,
    body: &str,
) -> Result<DocumentOutcome, String> {
    let state = &ctx.state;
    let kind = ingest::sniff(url, content_type, body);
    let text = ingest::to_document_text(kind, body).map_err(|e| e.to_string())?;
    let idx = LineIndex::new(&text);
    let counter = GatewayTokens {
        state: state.clone(),
        alias: plan.alias.clone(),
    };
    let windows = ingest::plan_windows(&text, &idx, plan.window_tokens, &counter)
        .await
        .map_err(|e| format!("planning extraction windows for {url}: {e}"))?;

    let mut sections: Vec<AcceptedSection> = Vec::new();
    let mut rejected_spans = 0u64;
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    // Admitted once for the whole document rather than per window or per turn.
    // Scoped so the guard is gone before the embedding batches below ask the
    // scheduler for the aux model.
    let hold = crate::vram::admit(state, &plan.route, &plan.alias)
        .await
        .map_err(|e| format!("admitting ingest model '{}': {e}", plan.alias))?;
    let mut route = plan.route.clone();
    // A local model answers on the port `acquire` just started, not on the
    // class-wide router port the route was resolved against (§5). The runner
    // carries the swapped route, so it holds for every window and every turn.
    if let Some(hold) = &hold {
        route.upstream.base_url = hold.endpoint();
    }
    let runner = IngestRunner {
        state: state.clone(),
        route,
        _admission: hold,
    };
    for (first, last) in windows {
        let exec = EmitExecutor {
            document: &text,
            idx: &idx,
            accepted: Mutex::new(Vec::new()),
            rejected: AtomicU64::new(0),
        };
        let ir = ChatRequest {
            model_alias: plan.alias.clone(),
            messages: vec![
                Message::text(Role::System, plan.system),
                Message::text(
                    Role::User,
                    prompt::document_turn(
                        url,
                        kind,
                        &idx.numbered(&text, first, last),
                        first,
                        last,
                        idx.len(),
                    ),
                ),
            ],
            params: Params {
                max_tokens: Some(plan.reply_tokens),
                ..Default::default()
            },
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
        };
        let result = agent::run(
            ir,
            // Serial: the executor accumulates into one document's section list
            // and the model is meant to see each verdict before the next call.
            RunConfig::new(vec![plan.tool()], plan.budget, false),
            &runner,
            &exec,
            &mut CollectSink::default(),
        )
        .await
        .map_err(|e| format!("extracting {url} (lines {first}–{last}): {e}"))?;
        if let StopReason::Incomplete(why) = result.reason {
            tracing::warn!("extraction of {url} lines {first}–{last} stopped early: {why}");
        }
        rejected_spans += exec.rejected.load(Ordering::Relaxed);
        for s in exec.accepted.into_inner().unwrap_or_default() {
            if seen.insert(s.span) {
                sections.push(s);
            }
        }
    }
    // Extraction is over: hand the GPU back before the embedder asks for it.
    drop(runner);

    if sections.is_empty() {
        // Nothing accepted *and* something rejected means the model tried and
        // failed the contract — a failure worth retrying later, not an empty
        // page. Nothing accepted with nothing rejected is simply a page with no
        // documentation on it.
        if rejected_spans > 0 {
            return Err(format!(
                "{url}: every span was rejected by the verbatim check ({rejected_spans})"
            ));
        }
        return Ok(DocumentOutcome {
            rejected_spans,
            ..Default::default()
        });
    }
    sections.sort_by_key(|s| s.span);

    let dims = corpus.dims();
    let mut chunks: Vec<NewChunk> = sections
        .iter()
        .map(|s| NewChunk {
            corpus_id: corpus.id,
            document_id,
            heading_path: s.heading_path.clone(),
            span_start: s.span.0 as i64,
            span_end: s.span.1 as i64,
            payload: s.payload.clone(),
            derived_title: s.derived_title.clone(),
            derived_summary: s.derived_summary.clone(),
            embedding: None,
        })
        .collect();

    let texts: Vec<String> = chunks
        .iter()
        .map(|c| {
            ingest::embed_text(
                &c.heading_path,
                &c.derived_title,
                &c.derived_summary,
                &c.payload,
            )
        })
        .collect();
    let mut embedded = 0u64;
    for (i, group) in texts.chunks(batch).enumerate() {
        let vectors = embedder
            .embed(group)
            .await
            .map_err(|e| format!("embedding {url}: {e}"))?;
        for (j, v) in vectors.into_iter().enumerate() {
            // The store refuses a vector of the wrong width too; this keeps the
            // message about the *model* rather than about a row.
            if v.len() != dims {
                return Err(format!(
                    "{} returned a {}-wide vector but corpus {} is pinned to {dims}",
                    embedder.alias(),
                    v.len(),
                    corpus.corpus_id()
                ));
            }
            chunks[i * batch + j].embedding = Some(v);
            embedded += 1;
        }
    }

    // Replace this document's chunks wholesale: a section that no longer exists
    // must not survive as an orphan. Ids are content-derived, so unchanged
    // sections keep theirs across the replacement (§4).
    qstore::delete_document_chunks(&state.corpus, document_id)
        .await
        .map_err(|e| e.to_string())?;
    let ids = qstore::insert_chunks(&state.corpus, url, dims, &chunks)
        .await
        .map_err(|e| e.to_string())?;
    Ok(DocumentOutcome {
        unchanged: false,
        extracted: true,
        chunks: ids.len() as u64,
        embedded,
        rejected_spans,
    })
}

// ---------------------------------------------------------------------------
// The model side
// ---------------------------------------------------------------------------

/// Everything one corpus's extraction calls need, resolved once.
struct ExtractionPlan {
    alias: String,
    route: Route,
    system: &'static str,
    budget: Budget,
    reply_tokens: u32,
    /// Room for document text in one call: the model's real context, minus the
    /// measured prompt scaffold, minus the reply budget.
    window_tokens: usize,
}

impl ExtractionPlan {
    async fn build(state: &SharedState, corpus: &Corpus) -> Result<Self, String> {
        let alias = corpus.ingest_model.trim().to_string();
        if alias.is_empty() {
            return Err(format!(
                "corpus {} pins no ingest model",
                corpus.corpus_id()
            ));
        }
        let snap = state.snapshot();
        // Plain `resolve`, deliberately: an unattended batch job is **refused
        // under a GPU hold, never re-routed** (gpu-hold design §2). Nobody is
        // waiting on an ingest, it can be re-run after the hold, and its
        // embedding step could not fall back anyway — aux rows never inherit
        // the global fallback, because a corpus embedded with a different
        // model is a silently corrupted vector index.
        let route = snap.resolve(&alias).map_err(|e| e.to_string())?;
        // Checked here rather than left to `admit_local`, and before anything
        // is measured or fetched: the very next step counts tokens through
        // `count_tokens_inner`, which *does* fall back, so a held run would
        // otherwise size its extraction windows with the fallback's tokenizer
        // and only fail once the first document was already being extracted.
        if let Some(target) = crate::vram::classify(&route) {
            // A benchmark's lease refuses it the same way (benchmark design
            // §3.2): `gpu_benchmark` on the job row.
            if let Some(block) = snap.gpu_block() {
                return Err(block.refusal(target.model_id, "").to_string());
            }
        }
        let system =
            prompt::for_version(&corpus.ingest_prompt_version).map_err(|e| e.to_string())?;

        let context = context_length(state, &alias, &route).await.ok_or_else(|| {
            format!(
                "the context length of ingest model '{alias}' is unknown to this gateway, \
                     so an extraction window cannot be sized from it — set the model's ctx-size \
                     (local models) or make sure its upstream reports context_length"
            )
        })? as usize;
        let reply_tokens = snap.settings.docs_ingest_reply_tokens;
        // Measured with the model's own tokenizer, not estimated: the scaffold
        // is the system prompt plus an empty document turn.
        let scaffold_text = format!(
            "{system}\n{}",
            prompt::document_turn("", SourceKind::Markdown, "", 1, 1, 1)
        );
        let scaffold = proxy::count_tokens_inner(state, &alias, &scaffold_text, true)
            .await
            .map_err(|(_, e)| format!("counting the ingest prompt against '{alias}': {e}"))?
            .0 as usize;
        let window_tokens = context
            .checked_sub(scaffold + reply_tokens as usize)
            .filter(|w| *w > 0)
            .ok_or_else(|| {
                format!(
                    "ingest model '{alias}' has a {context}-token context, which the \
                     {scaffold}-token prompt and the {reply_tokens}-token reply budget \
                     (docs_ingest_reply_tokens) already fill — no room is left for document text"
                )
            })?;

        Ok(Self {
            alias,
            route,
            system,
            // The `/v1/responses` budgets: this is the same thing they bound (a
            // tool loop, not one generation), they are visible on the Settings
            // page, and two knobs for one behaviour is how they drift apart.
            budget: Budget {
                max_tool_calls: snap.settings.responses_max_tool_calls,
                wall_clock: Duration::from_secs(snap.settings.responses_timeout_seconds.max(1)),
            },
            reply_tokens,
            window_tokens,
        })
    }

    fn tool(&self) -> ResolvedTool {
        ResolvedTool::server_side(
            TOOL_LABEL,
            ToolDef {
                name: extract::EMIT_TOOL.to_string(),
                description: Some(
                    "Report the sections of the document you were shown. Line ranges are \
                     sliced out of the original text; first_line and last_line are checked \
                     against it character for character."
                        .into(),
                ),
                parameters: extract::emit_schema(),
            },
        )
    }
}

/// The ingest model's real context window.
///
/// Two sources, both facts lmgw already holds: a local model's per-request
/// context (`LlamaParams::per_request_ctx` — split `ctx-size / parallel`, or
/// for a unified row the pool/per-slot-cap/trained-context minimum,
/// unified-KV design §3.2), or the upstream catalog's `context_length`.
/// Nothing is guessed — an unknown context is reported as unknown, because a
/// wrong window silently truncates documentation.
async fn context_length(state: &SharedState, alias: &str, route: &Route) -> Option<u64> {
    let snap = state.snapshot();
    if route.upstream.id == ROUTER_UPSTREAM_ID {
        let m = snap
            .local_models
            .iter()
            .find(|m| m.model_id == route.upstream_model)?;
        let dir = snap.settings.router.models_dir.clone();
        let trained =
            crate::capabilities::exposed::trained_context(state, &dir, &m.gguf_path).await;
        let ctx = m.params.per_request_ctx(trained)?;
        // A ladder starts at its base rung, whose slot llama-server caps at
        // the weights' trained context (`ladder::slot_ctx`).
        let ctx = if m.is_ladder() {
            crate::ladder::slot_ctx(ctx, trained)
        } else {
            ctx
        };
        return Some(ctx as u64);
    }
    match crate::catalog::upstream_models(state, &route.upstream).await {
        Ok(models) => models
            .into_iter()
            .find(|m| m.id == route.upstream_model)
            .and_then(|m| m.context_length),
        Err(e) => {
            tracing::warn!("listing models of '{alias}'s upstream: {e}");
            None
        }
    }
}

/// One model turn, logged like any other in-process call (§8).
struct IngestRunner {
    state: SharedState,
    route: Route,
    /// GPU admission for this document's extraction (§9b), taken once and held
    /// across every window and every turn inside them. Background ingestion is
    /// precisely the traffic §8 says the scheduler must arbitrate against
    /// interactive requests — and without a guard the extraction model is the
    /// *first* thing an eviction pass reaches for, because an unclaimed model
    /// with no LRU stamp sorts as the coldest resident there is.
    ///
    /// Released when the runner is dropped, which is before this document's
    /// chunks are embedded: the embedding batches admit the aux model on their
    /// own, and a chat model pinned for the whole job would starve them.
    _admission: Option<crate::vram::LocalHold>,
}

#[async_trait]
impl TurnRunner for IngestRunner {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        _sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        proxy::sample_once(
            &self.state,
            self._admission.as_ref(),
            &self.route,
            // A batch runner is admitted by plain `vram::admit`: never a
            // fallback (gpu-hold design §2).
            None,
            ir,
            INGEST_PROTO,
            None,
            deadline,
        )
        .await
    }
}

/// The only tool an extraction run has. Validates every proposed span against
/// the original document and answers with the verdict, which is what turns a
/// rejection into a retry the model can act on.
struct EmitExecutor<'a> {
    document: &'a str,
    idx: &'a LineIndex,
    accepted: Mutex<Vec<AcceptedSection>>,
    rejected: AtomicU64,
}

#[async_trait]
impl ToolExecutor for EmitExecutor<'_> {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        if name != extract::EMIT_TOOL {
            return ToolOutcome::error(format!(
                "there is no tool named '{name}' here — only {}",
                extract::EMIT_TOOL
            ));
        }
        let proposal: Extraction = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutcome::error(format!(
                    "those arguments do not match the schema: {e}. Every section needs \
                     start_line, end_line, first_line and last_line."
                ))
            }
        };
        let verdict = extract::validate(self.document, self.idx, &proposal);
        let report = extract::verdict_report(&verdict);
        self.rejected
            .fetch_add(verdict.rejected.len() as u64, Ordering::Relaxed);
        if let Ok(mut acc) = self.accepted.lock() {
            acc.extend(verdict.accepted);
        }
        // A rejection is the model's problem to fix, not the gateway's to hide:
        // it comes back as a failed tool result so the loop keeps going and the
        // next turn can correct it.
        if verdict.rejected.is_empty() {
            ToolOutcome::ok(ToolResultBlock::one(report))
        } else {
            ToolOutcome::error(report)
        }
    }
}

/// [`ingest::AsyncTokenCounter`] over the gateway's own token counting, so
/// window planning uses the ingest model's real tokenizer.
struct GatewayTokens {
    state: SharedState,
    alias: String,
}

#[async_trait]
impl ingest::AsyncTokenCounter for GatewayTokens {
    async fn count(&self, text: &str) -> quickdoc_core::Result<usize> {
        // Pinned: windows are sized with this model's own tokenizer, so a
        // count from a fallback would be a silently wrong window (§4.7).
        proxy::count_tokens_inner(&self.state, &self.alias, text, true)
            .await
            .map(|(n, _)| n as usize)
            .map_err(|(_, e)| {
                quickdoc_core::QuickdocError::Invalid(format!("counting tokens: {e}"))
            })
    }
}
