//! Do a base's stored chunks fit an embedding model (review R2 finding 1, R3
//! finding 3)?
//!
//! A chunk is sized for the tokenizer and the per-input limit of the model it
//! was cut for. Moving a base to another model — or resuming a re-embed after
//! the model's limit changed — embeds the *stored* chunks, and a chunk the new
//! model refuses fails the whole job with no way forward: re-embedding runs the
//! same chunks again. So the stored chunks are measured first, against the new
//! model's limit ([`limit::input_limit`]). The files with chunks over it are
//! re-chunked for the new model instead ([`super::ops`], [`super::reembed`]);
//! the rest are only re-embedded.
//!
//! **Method** (a sample per base, never a call per file or chunk). Every chunk
//! stores its tiktoken count. The model's tokenizer is measured **once per
//! base**: [`SAMPLE_CHUNKS`] chunks, evenly spread over the base in reading
//! order, are counted by the model ([`limit::calibrate`], at most
//! `WINDOWS` = 16 tokenizer calls in all) and by tiktoken; the worst window's
//! ratio wins, so a dense stretch is not averaged away. A stored chunk is then
//! over the limit when `ceil(tokens × ratio) + special tokens` exceeds it
//! ([`store::file_fits`]: one SQL query for the whole base, no chunk text
//! loaded). This is an estimate — a chunk denser than every sampled window can
//! still be missed — and what it misses is caught by the model's own refusal
//! at embed time, which splits the chunk ([`super::ingest_split`]) rather than
//! failing the file. The sentence naming the sample and the ratio is part of
//! what is reported ([`Measure::method`]).
//!
//! The measurement runs inside the job that acts on it ([`super::reembed`]),
//! never in a request handler.

use crate::state::SharedState;

use super::limit;
use super::store::{self, FileFit, Kb};

/// How many chunks of a base the tokenizer is measured on. Visible in
/// [`Measure::method`]; the tokenizer calls it costs are bounded by
/// `limit::WINDOWS` however large the base is.
pub const SAMPLE_CHUNKS: usize = 16;

/// What measuring a base's stored chunks against a model found.
#[derive(Debug, Clone, Default)]
pub struct Measure {
    /// The model's per-input limit in tokens; `None` when it is not known, in
    /// which case nothing can be said (the re-embed's own error names it).
    pub max: Option<u64>,
    /// Model tokens per tiktoken token, as sampled.
    pub ratio: f64,
    /// One sentence: what was sampled, with what, and what limit it was held
    /// against.
    pub method: String,
    /// Every file that has chunks over the limit, however it stands.
    pub over: Vec<FileFit>,
    pub chunks_over: usize,
    pub chunks_total: usize,
}

impl Measure {
    /// The files to re-chunk: over the limit and not `failed`. A failed file
    /// has said why it cannot be read; re-reading it on every Resume would
    /// cost the same failure each time, so it is reported instead
    /// ([`Self::failed_over`]).
    pub fn rechunk(&self) -> Vec<&FileFit> {
        self.over.iter().filter(|f| f.status != "failed").collect()
    }

    /// Failed files whose stale chunks are over the limit.
    pub fn failed_over(&self) -> Vec<&FileFit> {
        self.over.iter().filter(|f| f.status == "failed").collect()
    }

    /// One sentence for the owner, when anything is over.
    pub fn why(&self, alias: &str) -> Option<String> {
        let max = self.max?;
        (self.chunks_over > 0).then(|| {
            format!(
                "{} of {} stored chunks are larger than what '{alias}' takes per input \
                 ({max} tokens)",
                self.chunks_over, self.chunks_total
            )
        })
    }
}

/// Measure `kb`'s stored chunks against `alias`'s model. See the module doc
/// for the method.
pub async fn measure(state: &SharedState, kb: &Kb, alias: &str) -> Result<Measure, String> {
    let input = limit::input_limit(state, alias).await;
    let Some(max) = input.tokens else {
        return Ok(Measure {
            method: input.source,
            ratio: 1.0,
            ..Default::default()
        });
    };
    let pool = &state.knowledge.pool;
    let total = store::chunk_total(pool, kb.id)
        .await
        .map_err(|e| e.to_string())?;
    if total == 0 {
        return Ok(Measure {
            max: Some(max),
            ratio: 1.0,
            method: "the base holds no chunks".into(),
            ..Default::default()
        });
    }
    let (counter, how, n) = sampled_counter(state, kb, alias, total).await?;
    limit::remember_ratio(state, alias, &counter);
    let ratio = counter.ratio();
    let ceiling = max as i64 - limit::SPECIAL_TOKENS as i64;
    let fits = store::file_fits(pool, kb.id, ratio, ceiling)
        .await
        .map_err(|e| e.to_string())?;
    let chunks_total = fits.iter().map(|f| f.chunks as usize).sum();
    let over: Vec<FileFit> = fits.into_iter().filter(|f| f.over > 0).collect();
    let chunks_over = over.iter().map(|f| f.over as usize).sum();
    Ok(Measure {
        max: Some(max),
        ratio,
        method: format!(
            "estimated from the token counts stored with the chunks, scaled by {how}, measured \
             on {n} of {total} chunks spread over the base; held against {max} tokens ({})",
            input.source
        ),
        over,
        chunks_over,
        chunks_total,
    })
}

/// The model's tokenizer ratio, measured on [`SAMPLE_CHUNKS`] chunks spread
/// over `kb` (of `total`). Returns the counter, the sentence naming what
/// counted, and how many chunks were sampled.
pub async fn sampled_counter(
    state: &SharedState,
    kb: &Kb,
    alias: &str,
    total: i64,
) -> Result<(std::sync::Arc<limit::ScaledCounter>, String, i64), String> {
    let pool = &state.knowledge.pool;
    let n = (SAMPLE_CHUNKS as i64).min(total).max(0);
    let mut sample = String::new();
    for i in 0..n {
        let offset = i * total / n;
        if let Some((heading, payload)) = store::nth_chunk(pool, kb.id, offset)
            .await
            .map_err(|e| e.to_string())?
        {
            if !sample.is_empty() {
                sample.push('\n');
            }
            sample.push_str(&super::chunk::embed_text(&heading, &payload));
        }
    }
    let (counter, how) = limit::calibrate(state, alias, &sample).await;
    Ok((counter, how, n))
}

/// Make sure `alias`'s tokenizer ratio is remembered, sampling `kb` once per
/// model identity per process ([`limit::cached_counter`] then answers every
/// later search without a tokenizer call). A base with no chunks, a model with
/// no reachable tokenizer, or a ratio already remembered costs nothing.
pub async fn prime_ratio(state: &SharedState, kb: &Kb, alias: &str) {
    if limit::ratio_known(state, alias)
        || limit::input_limit(state, alias).await.tokenizer == "tiktoken_guess"
    {
        return;
    }
    let total = store::chunk_total(&state.knowledge.pool, kb.id)
        .await
        .unwrap_or(0);
    if total == 0 {
        return;
    }
    if let Ok((counter, _, _)) = sampled_counter(state, kb, alias, total).await {
        limit::remember_ratio(state, alias, &counter);
    }
}
