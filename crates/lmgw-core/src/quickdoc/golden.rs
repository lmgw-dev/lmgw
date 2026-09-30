//! The `golden_gen` job (quickdoc §10, §11): synthetic golden-query candidates.
//!
//! ```text
//!  code   sample N chunks of the corpus
//!  model  emit_queries: the questions that chunk answers, schema-constrained
//!  code   check them, attach the chunk's own id, file them as candidates
//!  owner  the §11 curation queue: accept → golden query · reject → discarded
//! ```
//!
//! The engine is the same server-side tool loop the extraction pass runs on
//! (`agent.rs`, §21), through the corpus's **pinned ingest model** — a corpus is
//! a function of its two models (§4), and bootstrapping its measurement with a
//! third one would make the score a statement about a model the corpus does not
//! name. A refused query comes back as that tool's result, so a correction is an
//! ordinary next turn rather than bespoke retry machinery.
//!
//! **What the model cannot do here:** name a chunk id (code attaches the id of
//! the chunk it was shown), create a golden query (this job writes only
//! candidates — §11's queue is the only path from one to the other), or have a
//! copied sentence accepted as a question
//! ([`quickdoc_core::golden::validate`] refuses it).
//!
//! **What is not capped:** how many chunks a run samples and how many questions
//! it asks for per chunk are both request parameters, visible in the eval
//! dashboard's own fields, and the sample defaults to *every chunk in the
//! corpus* — a generation run that quietly looked at the first twenty would
//! produce golden queries about the first page of the documentation.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use quickdoc_core::golden;
use quickdoc_core::store::{self as qstore, Chunk, Corpus, NewGoldenCandidate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{
    self, Budget, CollectSink, DeltaSink, ResolvedTool, RunConfig, StopReason, ToolExecutor,
    ToolOutcome, TurnRunner,
};
use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Completion, Message, Params, Role, ToolDef, ToolResultBlock};
use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::proxy;
use crate::state::SharedState;
use crate::telemetry::GOLDEN_PROTO;

/// `server_label` of the generation tool in the loop's events, as
/// [`ingest::TOOL_LABEL`](super::ingest::TOOL_LABEL) is for extraction.
pub const TOOL_LABEL: &str = "quickdoc";

/// Request payload of a `golden_gen` job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Input {
    pub corpus_id: i64,
    /// How many chunks to sample. Absent or `0` means **every chunk** — the
    /// corpus's own size is the only honest default, and a smaller run is the
    /// owner asking for one.
    #[serde(default)]
    pub sample: Option<usize>,
    /// Questions to ask for per sampled chunk. Absent means one.
    #[serde(default)]
    pub per_chunk: Option<usize>,
}

/// `done`/`total` are sampled chunks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub corpus: String,
    /// The corpus's pinned ingest model, which is what writes the questions.
    pub model: String,
    /// Heading path of the chunk being worked on.
    pub chunk: String,
    pub per_chunk: usize,
    /// Questions the model emitted, accepted or not.
    pub proposed: u64,
    /// Candidates filed. Every one of them is `pending`: nothing here becomes a
    /// golden query without the owner (§11).
    pub candidates: u64,
    /// Proposals a check refused — a copied sentence, an empty query, or one of
    /// the `duplicates` below. Non-zero is ordinary; the model is told and
    /// corrects.
    pub rejected: u64,
    /// Rejections that were questions this corpus already asks.
    pub duplicates: u64,
    /// Chunks whose turn failed outright (the model errored, the loop hit its
    /// budget). Counted rather than hidden: a run that failed half its chunks
    /// produced half a bootstrap.
    pub failed: u64,
}

/// One live generation per corpus — the same key the other corpus jobs use, so
/// the Docs tab's job line finds it.
pub fn job_key(corpus_id: i64) -> String {
    format!("corpus:{corpus_id}")
}

pub async fn start(
    state: &SharedState,
    corpus: &Corpus,
    sample: Option<usize>,
    per_chunk: Option<usize>,
) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::GoldenGen,
        Some(job_key(corpus.id)),
        format!("golden queries for {}", corpus.corpus_id()),
        json!({ "corpus_id": corpus.id, "sample": sample, "per_chunk": per_chunk }),
    )
    .await
}

pub struct GoldenGenExecutor;

#[async_trait]
impl JobExecutor for GoldenGenExecutor {
    fn kind(&self) -> JobKind {
        JobKind::GoldenGen
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("golden_gen input: {e}"))?;
        let corpus = qstore::get_corpus(&ctx.state.corpus, input.corpus_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("corpus {} no longer exists", input.corpus_id))?;
        run_generation(&ctx, &corpus, input).await
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

async fn run_generation(ctx: &JobCtx, corpus: &Corpus, input: Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let plan = GenerationPlan::build(state, corpus, input.per_chunk)?;

    let chunks = qstore::sample_chunks(&state.corpus, corpus.id, input.sample.unwrap_or(0) as i64)
        .await
        .map_err(|e| e.to_string())?;
    if chunks.is_empty() {
        // Writing questions about nothing would file candidates that can never
        // hit. An empty corpus is a missing input, exactly as an eval with no
        // golden queries is.
        return Err(format!(
            "corpus {} has no chunks to write questions about — ingest it first",
            corpus.corpus_id()
        ));
    }

    // Everything this corpus already asks — its golden queries and every earlier
    // candidate, accepted or rejected — so a run spends its turns on questions
    // the corpus does not have yet, and never re-proposes one the owner
    // already turned down.
    let taken: Mutex<HashSet<String>> = Mutex::new(
        qstore::existing_query_texts(&state.corpus, corpus.id)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|q| golden::normalize(q))
            .collect(),
    );

    let mut detail = Detail {
        corpus: corpus.corpus_id(),
        model: plan.alias.clone(),
        per_chunk: plan.per_chunk,
        ..Default::default()
    };
    let total = chunks.len() as u64;
    ctx.progress(progress(0, total, "generating", &detail))
        .await;

    let mut done = 0u64;
    let mut canceled = false;
    for chunk in &chunks {
        // Cancel lands on a chunk boundary: everything filed before it is kept,
        // because a candidate is complete the moment it is written.
        if ctx.canceled() {
            canceled = true;
            break;
        }
        detail.chunk = chunk_label(chunk);
        ctx.progress(progress(done, total, "generating", &detail))
            .await;
        match one_chunk(ctx, corpus, &plan, chunk, &taken).await {
            Ok(o) => {
                detail.proposed += o.proposed;
                detail.candidates += o.candidates;
                detail.rejected += o.rejected;
                detail.duplicates += o.duplicates;
            }
            Err(e) => {
                detail.failed += 1;
                tracing::warn!("golden_gen {}: chunk {}: {e}", corpus.corpus_id(), chunk.id);
            }
        }
        done += 1;
        ctx.progress(progress(done, total, "generating", &detail))
            .await;
    }

    detail.chunk.clear();
    let stage = if canceled { "canceled" } else { "done" };
    ctx.progress(progress(done, total, stage, &detail)).await;
    if canceled {
        return Ok(JobOutcome::Canceled);
    }
    if detail.candidates == 0 && detail.failed == total {
        return Err(format!(
            "every one of the {total} sampled chunks failed — nothing was proposed"
        ));
    }
    Ok(JobOutcome::Done(json!({
        "corpus_id": corpus.id,
        "sampled": total,
        "proposed": detail.proposed,
        "candidates": detail.candidates,
        "rejected": detail.rejected,
        "duplicates": detail.duplicates,
        "failed": detail.failed,
        // Said in the outcome as well as in the UI: the run produced proposals,
        // and a proposal is not a measurement until the owner accepts it.
        "status": "pending curation",
    })))
}

fn progress(done: u64, total: u64, stage: &str, detail: &Detail) -> JobProgress {
    JobProgress {
        done,
        total: Some(total),
        stage: stage.to_string(),
        detail: serde_json::to_value(detail).unwrap_or(Value::Null),
    }
}

/// What the job line shows while a chunk is being worked on: its heading path,
/// or its derived title when it has no headings, or its id when it has neither.
fn chunk_label(chunk: &Chunk) -> String {
    for candidate in [&chunk.heading_path, &chunk.derived_title] {
        if !candidate.trim().is_empty() {
            return candidate.trim().to_string();
        }
    }
    chunk.id.chars().take(12).collect()
}

#[derive(Default)]
struct ChunkOutcome {
    proposed: u64,
    candidates: u64,
    rejected: u64,
    duplicates: u64,
}

/// One chunk: one tool loop, then the candidates it earned.
async fn one_chunk(
    ctx: &JobCtx,
    corpus: &Corpus,
    plan: &GenerationPlan,
    chunk: &Chunk,
    taken: &Mutex<HashSet<String>>,
) -> Result<ChunkOutcome, String> {
    let state = &ctx.state;
    let exec = EmitExecutor {
        payload: &chunk.payload,
        taken,
        accepted: Mutex::new(Vec::new()),
        proposed: AtomicU64::new(0),
        rejected: AtomicU64::new(0),
        duplicates: AtomicU64::new(0),
    };
    let ir = ChatRequest {
        model_alias: plan.alias.clone(),
        messages: vec![
            Message::text(Role::System, golden::SYSTEM),
            Message::text(
                Role::User,
                golden::chunk_turn(
                    &corpus.corpus_id(),
                    &chunk.heading_path,
                    &chunk.payload,
                    plan.per_chunk,
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
    // One admission for this chunk's whole tool loop (§9b). Per chunk rather
    // than per job: a generation run is background work, and a release point at
    // every chunk boundary is what lets interactive traffic reclaim the GPU
    // without waiting for the run to end.
    let hold = crate::vram::admit(state, &plan.route, &plan.alias)
        .await
        .map_err(|e| format!("admitting generator model '{}': {e}", plan.alias))?;
    let mut route = plan.route.clone();
    // A local model answers on the port `acquire` just started, not on the
    // class-wide router port the route was resolved against (§5). The runner
    // carries the swapped route, so it holds for every turn of the chunk's
    // loop.
    if let Some(hold) = &hold {
        route.upstream.base_url = hold.endpoint();
    }
    let runner = GoldenRunner {
        state: state.clone(),
        route,
        _admission: hold,
    };
    let result = agent::run(
        ir,
        // Serial, like extraction: the executor accumulates one chunk's
        // questions and the model is meant to see each verdict before the next
        // call.
        RunConfig::new(vec![plan.tool()], plan.budget, false),
        &runner,
        &exec,
        &mut CollectSink::default(),
    )
    .await
    .map_err(|e| format!("asking about chunk {}: {e}", chunk.id))?;
    if let StopReason::Incomplete(why) = result.reason {
        tracing::warn!("generation for chunk {} stopped early: {why}", chunk.id);
    }

    let mut out = ChunkOutcome {
        proposed: exec.proposed.load(Ordering::Relaxed),
        rejected: exec.rejected.load(Ordering::Relaxed),
        duplicates: exec.duplicates.load(Ordering::Relaxed),
        candidates: 0,
    };
    for q in exec.accepted.into_inner().unwrap_or_default() {
        // The expectation is the chunk the model was shown, attached here: the
        // model never gets to name one, so a candidate cannot expect a chunk
        // that does not exist.
        let filed = qstore::insert_golden_candidate(
            &state.corpus,
            &NewGoldenCandidate {
                corpus_id: corpus.id,
                query: q.query,
                expected_chunk_ids: vec![chunk.id.clone()],
                rationale: q.rationale,
                model: plan.alias.clone(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        if filed.is_some() {
            out.candidates += 1;
        } else {
            // Raced with an identical row (the same question about another
            // chunk of this corpus). Counted where every other "already asked"
            // is counted rather than silently dropped.
            out.duplicates += 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The model side
// ---------------------------------------------------------------------------

/// Everything one corpus's generation calls need, resolved once.
struct GenerationPlan {
    alias: String,
    route: Route,
    per_chunk: usize,
    budget: Budget,
    reply_tokens: u32,
}

impl GenerationPlan {
    fn build(
        state: &SharedState,
        corpus: &Corpus,
        per_chunk: Option<usize>,
    ) -> Result<Self, String> {
        let alias = corpus.ingest_model.trim().to_string();
        if alias.is_empty() {
            return Err(format!(
                "corpus {} pins no ingest model, so there is nothing to write its \
                 questions with",
                corpus.corpus_id()
            ));
        }
        let snap = state.snapshot();
        // Plain `resolve` and an up-front hold check, for the same reason
        // extraction does it (gpu-hold design §2): golden-query generation is
        // unattended background work, so a hold **refuses** it — with the
        // `gpu_hold` sentence on the job row — instead of quietly spending a
        // cloud model's tokens on it. Failing here means the run stops before
        // its first chunk rather than at `admit_local` per chunk.
        let route = snap.resolve(&alias).map_err(|e| e.to_string())?;
        if let Some(target) = crate::vram::classify(&route) {
            // A benchmark's lease refuses it the same way (benchmark design
            // §3.2): `gpu_benchmark` on the job row.
            if let Some(block) = snap.gpu_block() {
                return Err(block.refusal(target.model_id, "").to_string());
            }
        }
        Ok(Self {
            alias,
            route,
            per_chunk: per_chunk.filter(|n| *n > 0).unwrap_or(1),
            // The `/v1/responses` budgets, for the same reason extraction uses
            // them: this is a tool loop, they are visible on the Settings page,
            // and two knobs for one behaviour is how they drift apart.
            budget: Budget {
                max_tool_calls: snap.settings.responses_max_tool_calls,
                wall_clock: Duration::from_secs(snap.settings.responses_timeout_seconds.max(1)),
            },
            // One reply's worth of room, the same visible setting extraction
            // sizes its own reply with. Unlike extraction there is no window to
            // size from it: a chunk is a section, and one section is the unit.
            reply_tokens: snap.settings.docs_ingest_reply_tokens,
        })
    }

    fn tool(&self) -> ResolvedTool {
        ResolvedTool::server_side(
            TOOL_LABEL,
            ToolDef {
                name: golden::EMIT_TOOL.to_string(),
                description: Some(
                    "Report the questions this section answers. They are checked against \
                     the section text and against the questions this corpus already asks, \
                     and every one that passes is filed for the owner to review."
                        .into(),
                ),
                parameters: golden::emit_schema(self.per_chunk),
            },
        )
    }
}

/// One model turn, logged like any other in-process call.
struct GoldenRunner {
    state: SharedState,
    route: Route,
    /// GPU admission for this chunk's loop (§9b) — see
    /// [`crate::vram::LocalHold`] for why the guard is per loop, not per
    /// turn. Released when the runner is dropped at the end of the chunk.
    _admission: Option<crate::vram::LocalHold>,
}

#[async_trait]
impl TurnRunner for GoldenRunner {
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
            GOLDEN_PROTO,
            None,
            deadline,
        )
        .await
    }
}

/// The only tool a generation run has. Checks every proposed question against
/// the chunk it was written from and against what the corpus already asks, and
/// answers with the verdict — which is what turns a refusal into a retry the
/// model can act on.
struct EmitExecutor<'a> {
    payload: &'a str,
    taken: &'a Mutex<HashSet<String>>,
    accepted: Mutex<Vec<golden::AcceptedQuery>>,
    proposed: AtomicU64,
    rejected: AtomicU64,
    duplicates: AtomicU64,
}

#[async_trait]
impl ToolExecutor for EmitExecutor<'_> {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        if name != golden::EMIT_TOOL {
            return ToolOutcome::error(format!(
                "there is no tool named '{name}' here — only {}",
                golden::EMIT_TOOL
            ));
        }
        let proposal: golden::Proposal = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutcome::error(format!(
                    "those arguments do not match the schema: {e}. Every entry needs a \
                     'query'."
                ))
            }
        };
        self.proposed
            .fetch_add(proposal.queries.len() as u64, Ordering::Relaxed);

        // Validated and claimed under one lock: two chunks running in sequence
        // must not both file the same question, and the dedup set is the only
        // thing that knows.
        let verdict = {
            let mut taken = match self.taken.lock() {
                Ok(t) => t,
                Err(e) => return ToolOutcome::error(format!("the dedup set is poisoned: {e}")),
            };
            let verdict = golden::validate(self.payload, &taken, &proposal);
            for a in &verdict.accepted {
                taken.insert(golden::normalize(&a.query));
            }
            verdict
        };
        self.rejected
            .fetch_add(verdict.rejected.len() as u64, Ordering::Relaxed);
        self.duplicates
            .fetch_add(verdict.duplicates as u64, Ordering::Relaxed);
        let report = golden::verdict_report(&verdict);
        if let Ok(mut acc) = self.accepted.lock() {
            acc.extend(verdict.accepted);
        }
        // A refusal is the model's to fix: it comes back as a failed tool result
        // so the loop keeps going and the next turn can replace that question.
        if verdict.rejected.is_empty() {
            ToolOutcome::ok(ToolResultBlock::one(report))
        } else {
            ToolOutcome::error(report)
        }
    }
}
