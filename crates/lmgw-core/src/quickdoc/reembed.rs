//! The `re_embed` job: ingestion's embedding stage on its own.
//!
//! Two things bring a corpus here. Its vectors are incomplete — an ingest was
//! cancelled part-way, so some chunks have none. Or its embedding model is being
//! *changed*, which §4 makes a deliberate, visible act: the pin moves first, the
//! old vectors are dropped (they belong to a different space, and half a corpus
//! in each is worse than none), and the corpus sits at `re_embed_required` until
//! the last chunk is done.
//!
//! Payloads are never touched, so chunk ids — and with them citations and golden
//! queries — survive a model change untouched.

use quickdoc_core::embed::Embedder;
use quickdoc_core::ingest;
use quickdoc_core::store::{self as qstore, Corpus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::embed::InProcessEmbedder;
use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::state::SharedState;

/// Request payload of a `re_embed` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub corpus_id: i64,
    /// Model alias to re-embed *onto*. Omitted keeps the corpus's current pin
    /// and only fills in the chunks that have no vector.
    #[serde(default)]
    pub embed_model: Option<String>,
}

/// `done`/`total` are chunks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub corpus: String,
    pub embed_model: String,
    /// True when the pin moved and every existing vector was dropped.
    pub repinned: bool,
}

pub fn job_key(corpus_id: i64) -> String {
    format!("corpus:{corpus_id}")
}

pub async fn start(
    state: &SharedState,
    corpus: &Corpus,
    embed_model: Option<String>,
) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::ReEmbed,
        Some(job_key(corpus.id)),
        format!("re-embed {}", corpus.corpus_id()),
        json!({ "corpus_id": corpus.id, "embed_model": embed_model }),
    )
    .await
}

pub struct ReEmbedExecutor;

#[async_trait::async_trait]
impl JobExecutor for ReEmbedExecutor {
    fn kind(&self) -> JobKind {
        JobKind::ReEmbed
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("re_embed input: {e}"))?;
        let corpus = qstore::get_corpus(&ctx.state.corpus, input.corpus_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("corpus {} no longer exists", input.corpus_id))?;
        run_re_embed(&ctx, corpus, input.embed_model).await
    }
}

async fn run_re_embed(
    ctx: &JobCtx,
    corpus: Corpus,
    embed_model: Option<String>,
) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let snap = state.snapshot();
    let batch = snap.settings.docs_embed_batch.max(1) as i64;

    let (embedder, corpus, repinned) = match embed_model {
        Some(alias) => {
            let e = InProcessEmbedder::probe(state.clone(), &alias)
                .await
                .map_err(|err| err.to_string())?;
            let identity = e.identity();
            if identity == corpus.embed_identity() {
                (e, corpus, false)
            } else {
                // Order matters: re-pin *before* dropping the vectors, so a
                // query landing mid-run sees a corpus that is honestly empty of
                // vectors for its declared model rather than full of vectors
                // from a different one.
                qstore::set_corpus_embed(&state.corpus, corpus.id, &identity)
                    .await
                    .map_err(|err| err.to_string())?;
                qstore::clear_corpus_embeddings(&state.corpus, corpus.id)
                    .await
                    .map_err(|err| err.to_string())?;
                let refreshed = qstore::get_corpus(&state.corpus, corpus.id)
                    .await
                    .map_err(|err| err.to_string())?
                    .ok_or("corpus vanished mid-re-embed")?;
                (e, refreshed, true)
            }
        }
        None => (
            InProcessEmbedder::for_corpus(state.clone(), &corpus)
                .await
                .map_err(|e| e.to_string())?,
            corpus,
            false,
        ),
    };

    let detail = Detail {
        corpus: corpus.corpus_id(),
        embed_model: embedder.identity().to_string(),
        repinned,
    };
    let total = qstore::count_unembedded(&state.corpus, corpus.id)
        .await
        .map_err(|e| e.to_string())? as u64;
    let report = |done: u64, stage: &str| JobProgress {
        done,
        total: Some(total),
        stage: stage.to_string(),
        detail: serde_json::to_value(&detail).unwrap_or(Value::Null),
    };
    ctx.progress(report(0, "embedding")).await;
    qstore::set_corpus_status(&state.corpus, corpus.id, "re_embed_required")
        .await
        .map_err(|e| e.to_string())?;

    let dims = corpus.dims();
    let mut done = 0u64;
    loop {
        // Cancellation lands on a batch boundary; every chunk already written is
        // a chunk that stays written, and `count_unembedded` says what is left.
        if ctx.canceled() {
            ctx.progress(report(done, "canceled")).await;
            return Ok(JobOutcome::Canceled);
        }
        let pending = qstore::list_unembedded(&state.corpus, corpus.id, batch)
            .await
            .map_err(|e| e.to_string())?;
        if pending.is_empty() {
            break;
        }
        let texts: Vec<String> = pending
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
        let vectors = embedder
            .embed(&texts)
            .await
            .map_err(|e| format!("re-embedding {}: {e}", corpus.corpus_id()))?;
        for (chunk, v) in pending.iter().zip(vectors) {
            qstore::set_chunk_embedding(&state.corpus, &chunk.id, dims, &v)
                .await
                .map_err(|e| e.to_string())?;
            done += 1;
        }
        ctx.progress(report(done, "embedding")).await;
    }

    qstore::set_corpus_status(&state.corpus, corpus.id, "ready")
        .await
        .map_err(|e| e.to_string())?;
    ctx.progress(report(done, "ready")).await;
    Ok(JobOutcome::Done(json!({
        "corpus_id": corpus.id,
        "embedded": done,
        "embed_model": embedder.identity().to_string(),
        "repinned": repinned,
    })))
}
