//! The `kb_reembed` job (chat-complete design §9.2): a base's embedding stage
//! on its own — the quickdoc `re_embed` pattern.
//!
//! Two things bring a base here. Its embedding model is being **changed**:
//! the pin moves first, every old vector is dropped (they belong to another
//! space, and half a base in each is worse than none), and the base sits at
//! `re_embed_required` until the last chunk has a vector again. Or its
//! vectors are **incomplete** (a re-embed that was cancelled or held), and
//! this fills in what is missing with the current pin.
//!
//! Payloads and chunk ids are never touched, so the citations stored with
//! Chat messages keep resolving across a model change. When it finishes and
//! files are waiting, it hands on to their ingest ([`ingest::hand_on`]) — the
//! two jobs never run on one base at once, so the queue is handed on rather
//! than raced.
//!
//! **First stage: measure.** A model change (and a Resume that asks for it)
//! first measures the stored chunks against the pinned model's per-input limit
//! ([`fit`]: one sampled tokenizer calibration per base, stored token counts
//! scaled by it — never a request per file). Files with chunks over the limit
//! go back to `pending` with the reason on them; only *they* are re-chunked,
//! by the ingest this job hands on to. Failed files are not re-read (they say
//! why they failed) and their stale chunks neither queue here nor keep the
//! base `re_embed_required`. The decision is in the job's stage (`measuring`)
//! and detail, and on the base.
//!
//! Chunks of files that are waiting to be re-ingested are not embedded here:
//! that ingest replaces them, so a vector for the old text would be paid for
//! and thrown away. A model change that also changes the chunk sizes never
//! gets here at all: [`ops`](super::ops) sends it straight to one ingest job,
//! which re-chunks and embeds in a single pass.

use quickdoc_core::embed::Embedder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::quickdoc::embed::refuse_if_held;
use crate::quickdoc::InProcessEmbedder;
use crate::state::SharedState;

use super::store::{self, Kb, Unembedded};
use super::{chunk, fit, job_key};

/// Request payload of a `kb_reembed` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub kb_id: i64,
    /// The alias to re-embed *onto*. Omitted keeps the pin and fills in the
    /// chunks that have no vector.
    #[serde(default)]
    pub embed_alias: Option<String>,
    /// Measure the stored chunks against the pinned model first, and send the
    /// files that overrun it back to ingest ([`super::fit`]). A model change
    /// always measures; a Resume asks for it here, so no request handler does
    /// the work.
    #[serde(default)]
    pub measure: bool,
}

/// `done`/`total` are chunks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub kb_id: i64,
    pub kb: String,
    pub embed_model: String,
    /// The pin moved and every old vector was dropped.
    pub repinned: bool,
    /// How the stored chunks were measured against the model, when they were
    /// ([`super::fit::Measure::method`]).
    #[serde(default)]
    pub measure: String,
    /// The files sent back to ingest because their chunks overrun the model —
    /// the decision "re-chunk these, re-embed the rest".
    #[serde(default)]
    pub rechunk_files: Vec<String>,
    #[serde(default)]
    pub rechunk_why: Option<String>,
    /// Failed files whose stale chunks overrun the model. They are not
    /// re-chunked again (they failed, and say why); the owner re-ingests or
    /// deletes them.
    #[serde(default)]
    pub failed_over: Vec<String>,
}

pub(crate) async fn spawn(
    state: &SharedState,
    kb: &Kb,
    embed_alias: Option<String>,
    measure: bool,
) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::KbReembed,
        Some(job_key(kb.id)),
        format!("re-embed {}", kb.name),
        json!({ "kb_id": kb.id, "embed_alias": embed_alias, "measure": measure }),
    )
    .await
}

pub struct KbReembedExecutor;

#[async_trait::async_trait]
impl JobExecutor for KbReembedExecutor {
    fn kind(&self) -> JobKind {
        JobKind::KbReembed
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("kb_reembed input: {e}"))?;
        let outcome = run(&ctx, input.clone()).await?;
        if matches!(outcome, JobOutcome::Done(_)) {
            super::ingest::hand_on(ctx.state.clone(), input.kb_id, ctx.id);
        }
        Ok(outcome)
    }
}

async fn run(ctx: &JobCtx, input: Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let pool = &state.knowledge.pool;
    let kb = store::get_kb(pool, input.kb_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("knowledge base {} no longer exists", input.kb_id))?;

    let (embedder, kb, repinned) = match input.embed_alias {
        Some(alias) => {
            let e = InProcessEmbedder::probe(state.clone(), &alias)
                .await
                .map_err(|e| e.to_string())?;
            let identity = e.identity();
            if identity == kb.embed_identity() {
                store::set_kb_embed(pool, kb.id, &alias, &identity)
                    .await
                    .map_err(|e| e.to_string())?;
                (e, kb, false)
            } else {
                // Re-pin *before* dropping the vectors: a search landing
                // mid-run sees a base honestly empty of vectors for its
                // declared model, never full of another model's.
                store::set_kb_embed(pool, kb.id, &alias, &identity)
                    .await
                    .map_err(|e| e.to_string())?;
                store::clear_embeddings(pool, kb.id)
                    .await
                    .map_err(|e| e.to_string())?;
                let kb = store::get_kb(pool, kb.id)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("the knowledge base vanished mid-re-embed")?;
                (e, kb, true)
            }
        }
        None => (
            InProcessEmbedder::for_identity(
                state.clone(),
                &kb.embed_identity(),
                &kb.label(),
                "change its embedding model",
            )
            .await
            .map_err(|e| e.to_string())?,
            kb,
            false,
        ),
    };

    let mut detail = Detail {
        kb_id: kb.id,
        kb: kb.name.clone(),
        embed_model: embedder.identity().to_string(),
        repinned,
        ..Default::default()
    };
    // First stage: do the stored chunks fit the pinned model? Those files
    // that do not go back to ingest (which re-chunks and embeds them for it,
    // and this job hands on to when it ends); the rest are only re-embedded.
    if repinned || input.measure {
        if let Err(e) = refuse_if_held(state, embedder.alias(), super::ingest::HOLD_WHY) {
            return Ok(JobOutcome::FailedWith {
                error: super::ingest::hold_reason(state, &e.to_string())
                    .replace("The files stay pending", "The chunks without a vector wait"),
                value: json!(detail),
            });
        }
        ctx.progress(JobProgress {
            done: 0,
            total: None,
            stage: "measuring".into(),
            detail: serde_json::to_value(&detail).unwrap_or(Value::Null),
        })
        .await;
        let m = fit::measure(state, &kb, embedder.alias()).await?;
        detail.measure = m.method.clone();
        detail.failed_over = m.failed_over().iter().map(|f| f.name.clone()).collect();
        let rechunk = m.rechunk();
        if let (false, Some(why)) = (rechunk.is_empty(), m.why(embedder.alias())) {
            let ids: Vec<i64> = rechunk.iter().map(|f| f.id).collect();
            let reason = format!("re-chunking for '{}': {why}", embedder.alias());
            store::mark_files_pending(pool, &ids, &reason)
                .await
                .map_err(|e| e.to_string())?;
            detail.rechunk_files = rechunk.iter().map(|f| f.name.clone()).collect();
            detail.rechunk_why = Some(why);
        }
    }
    let total = store::count_unembedded(pool, kb.id, Unembedded::Embeddable)
        .await
        .map_err(|e| e.to_string())? as u64;
    let report = |done: u64, stage: &str| JobProgress {
        done,
        total: Some(total),
        stage: stage.to_string(),
        detail: serde_json::to_value(&detail).unwrap_or(Value::Null),
    };
    ctx.progress(report(0, "embedding")).await;
    if total > 0 {
        store::set_kb_status(pool, kb.id, "re_embed_required")
            .await
            .map_err(|e| e.to_string())?;
    }

    let batch = state.snapshot().settings.docs_embed_batch.max(1) as i64;
    let mut done = 0u64;
    loop {
        // Every chunk written stays written; `count_unembedded` is what is
        // left, and a Resume picks up exactly there.
        if ctx.canceled() {
            ctx.progress(report(done, "canceled")).await;
            return Ok(JobOutcome::CanceledWith(json!(detail)));
        }
        if let Err(e) = refuse_if_held(state, embedder.alias(), super::ingest::HOLD_WHY) {
            return Ok(JobOutcome::FailedWith {
                error: super::ingest::hold_reason(state, &e.to_string())
                    .replace("The files stay pending", "The chunks without a vector wait"),
                value: json!(detail),
            });
        }
        let pending = store::list_unembedded(pool, kb.id, batch, Unembedded::Embeddable)
            .await
            .map_err(|e| e.to_string())?;
        if pending.is_empty() {
            break;
        }
        let texts: Vec<String> = pending
            .iter()
            .map(|c| chunk::embed_text(&c.heading_path, &c.payload))
            .collect();
        let vectors = embedder.embed(&texts).await.map_err(|e| {
            let e = e.to_string();
            if super::ingest_split::is_oversize(&e) {
                format!(
                    "re-embedding {}: {e}. The stored chunks are larger than '{}' takes per \
                     input — they were sized for another model or limit. Resume re-chunks the \
                     files that hold them for this model; or lower chunk_tokens, or raise the \
                     model's -ub/-b.",
                    kb.label(),
                    embedder.alias()
                )
            } else {
                format!("re-embedding {}: {e}", kb.label())
            }
        })?;
        // One transaction and one revision bump per batch.
        let batch_out: Vec<(String, Vec<f32>)> = pending
            .iter()
            .zip(vectors)
            .map(|(c, v)| (c.id.clone(), v))
            .collect();
        store::set_chunk_embeddings(pool, kb.id, &batch_out)
            .await
            .map_err(|e| e.to_string())?;
        done += batch_out.len() as u64;
        ctx.progress(report(done, "embedding")).await;
    }

    // `ready` once every chunk has a vector; chunks of waiting files get
    // theirs from the ingest that replaces them, which then closes the status.
    let missing = store::count_unembedded(pool, kb.id, Unembedded::Live)
        .await
        .map_err(|e| e.to_string())?;
    if missing == 0 {
        store::set_kb_status(pool, kb.id, "ready")
            .await
            .map_err(|e| e.to_string())?;
    }
    ctx.progress(report(done, "ready")).await;

    Ok(JobOutcome::Done(json!({
        "kb_id": kb.id,
        "embedded": done,
        "embed_model": embedder.identity().to_string(),
        "repinned": repinned,
        "rechunk_files": detail.rechunk_files,
    })))
}
