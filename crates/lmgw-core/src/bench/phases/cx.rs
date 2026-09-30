//! What every phase shares: the client, the suite, the live facts, the
//! tokenized corpus, the sampler and the progress counter.

use std::sync::atomic::{AtomicU64, Ordering};

use lmgw_api_types::bench::{PointPlan, Sampling, ServerFacts, SuiteParams};
use serde_json::{json, Value};

use crate::bench::client::{BenchError, LlamaClient};
use crate::bench::corpus::{self, Offsets};
use crate::bench::probes::Caps;
use crate::bench::run::{BenchSink, Progress};
use crate::bench::sampler::Sampler;

/// The run's progress in steps: one step per point repetition (a probe, a
/// prefill request, a decode request, a concurrent release, a mixed
/// repetition), counted up front from the plan so `done/total` means the
/// same thing all run long.
pub struct Steps<'a> {
    done: AtomicU64,
    total: u64,
    sink: &'a dyn BenchSink,
}

impl<'a> Steps<'a> {
    pub fn new(total: u64, sink: &'a dyn BenchSink) -> Self {
        Self {
            done: AtomicU64::new(0),
            total,
            sink,
        }
    }

    /// Report what is starting now.
    pub async fn stage(&self, stage: String) {
        self.sink
            .progress(Progress {
                stage,
                done: self.done.load(Ordering::Relaxed),
                total: self.total,
            })
            .await;
    }

    /// One step finished.
    pub fn advance(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total
    }
}

pub struct PhaseCx<'a> {
    pub client: &'a LlamaClient,
    pub params: &'a SuiteParams,
    pub plan: &'a PointPlan,
    pub server: &'a ServerFacts,
    /// The corpus as this server tokenizes it, and the prefix (BOS) every
    /// prompt starts with.
    pub corpus: &'a corpus::Corpus,
    pub caps: Caps,
    pub sampler: &'a Sampler,
    pub steps: &'a Steps<'a>,
    /// Slot resets sent so far, so each draws its own corpus offsets.
    pub resets: AtomicU64,
}

impl PhaseCx<'_> {
    pub fn reps(&self) -> u32 {
        self.params.repetitions.max(1)
    }

    /// Before a measured repetition (decision 41): one unmeasured request per
    /// slot, released together so each lands on a slot of its own, and waited
    /// for. llama-server resets a slot's previous context before it starts a
    /// new task on it, outside the prompt timer — measured live, 0.86 s after
    /// a 250k-token prompt on Qwen3.5-0.8B — so without this a repetition
    /// that follows a long one (the needle probe, the previous prefill
    /// repetition, the deepest decode) pays that reset in its TTFT, or starts
    /// one concurrent stream late. A decode depth's first repetition gets one
    /// too: on ik_llama.cpp another slot's deep context slows it (decision 61).
    pub async fn reset_slots(&self) -> Result<(), BenchError> {
        let n = self.server.n_slots.max(1) as u64;
        let first = self.resets.fetch_add(n, Ordering::Relaxed);
        let mut offsets = Offsets::new(self.corpus.len(), salt::RESET);
        // `nth(k)` walks k steps: a few hundred at most in a run.
        let start = offsets.nth(first as usize).unwrap_or(0);
        let bodies: Vec<Value> = std::iter::once(start)
            .chain(offsets.take(n as usize - 1))
            .filter_map(|off| {
                unmeasured_body(self.params, self.plan.per_slot_ctx, self.corpus, off)
            })
            .collect();
        unmeasured(self.client, &bodies, "slot reset").await
    }
}

/// The unmeasured request of the warm-up (decision 39) and of a slot reset
/// (decision 41): `params.warmup_prompt_tokens` from `offset` (fewer on a
/// context too small for them), `warmup_generate_tokens` generated, no
/// cache. `None` when the context has no room for a prompt at all.
pub fn unmeasured_body(
    params: &SuiteParams,
    per_slot_ctx: u64,
    corpus: &corpus::Corpus,
    offset: usize,
) -> Option<Value> {
    let generate = u64::from(params.warmup_generate_tokens.max(1));
    let room = per_slot_ctx.saturating_sub(generate + 1);
    let len = params.warmup_prompt_tokens.min(room) as usize;
    (len > 0).then(|| {
        let prompt = corpus.prompt(offset, len);
        completion_body(&prompt, generate, false, &params.sampling)
    })
}

/// Send `bodies` together and wait for all of them. Only a cancel is an
/// error: a request that fails is logged, and the measured requests after
/// it report their own failures.
pub async fn unmeasured(
    client: &LlamaClient,
    bodies: &[Value],
    what: &str,
) -> Result<(), BenchError> {
    let answers = futures::future::join_all(bodies.iter().map(|b| client.completion(b))).await;
    for a in answers {
        match a {
            Ok(_) => {}
            Err(BenchError::Canceled) => return Err(BenchError::Canceled),
            Err(BenchError::Failed(e)) => {
                tracing::warn!("benchmark {what} request failed (the run goes on): {e}")
            }
        }
    }
    Ok(())
}

/// A streamed `/completion` over token ids with the suite's pinned sampling
/// (§4.2). `ignore_eos` everywhere, so every request generates exactly
/// `n_predict` tokens.
pub fn completion_body(prompt: &[u32], n_predict: u64, cache_prompt: bool, s: &Sampling) -> Value {
    json!({
        "prompt": prompt,
        "n_predict": n_predict,
        "stream": true,
        "cache_prompt": cache_prompt,
        "ignore_eos": true,
        "temperature": s.temperature,
        "top_p": s.top_p,
        "top_k": s.top_k,
        "seed": s.seed,
    })
}

/// Per-phase salts for [`crate::bench::corpus::Offsets`], so phases draw
/// different sequences.
pub mod salt {
    pub const PREFILL: u64 = 1;
    pub const DECODE: u64 = 2;
    pub const CONCURRENT: u64 = 3;
    pub const MIXED: u64 = 4;
    pub const NEEDLE: u64 = 5;
    pub const WARMUP: u64 = 6;
    pub const RESET: u64 = 7;
    pub const MIXED_RATES: u64 = 8;
}
