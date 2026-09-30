//! Prefill (benchmark design §4.3): at each prompt length *P*, `n_predict: 1`,
//! `cache_prompt: false`, streamed, every repetition from a different corpus
//! offset. Server-side `prompt_ms` and `prompt_per_second`, client-side TTFT
//! on the same request.

use lmgw_api_types::bench::PrefillPoint;

use super::cx::{completion_body, salt, PhaseCx};
use crate::bench::client::BenchError;
use crate::bench::corpus::Offsets;
use crate::bench::sampler::EnergyWindow;
use crate::bench::stats::{ms, stat};

pub async fn run(cx: &PhaseCx<'_>, out: &mut Vec<PrefillPoint>) -> Result<(), BenchError> {
    let reps = cx.reps();
    let mut offsets = Offsets::new(cx.corpus.len(), salt::PREFILL);
    for &p in &cx.plan.prefill {
        let mut tok_s = Vec::new();
        let mut prompt_ms = Vec::new();
        let mut ttft = Vec::new();
        let mut evaluated = Vec::new();
        let mut windows = Vec::new();
        for r in 1..=reps {
            cx.steps
                .stage(format!("prefill: {p} tokens, repetition {r}/{reps}"))
                .await;
            cx.reset_slots().await?;
            let prompt = cx.corpus.prompt(offsets.next().unwrap_or(0), p as usize);
            let body = completion_body(&prompt, 1, false, &cx.params.sampling);
            let rec = cx.client.completion(&body).await?;
            let t = rec
                .timings()
                .map_err(|e| BenchError::Failed(format!("prefill {p}: {e}")))?;
            let first = rec
                .first_token()
                .ok_or_else(|| BenchError::Failed(format!("prefill {p}: no token arrived")))?;
            tok_s.push(t.prompt_per_second);
            prompt_ms.push(t.prompt_ms);
            evaluated.push(t.prompt_n as f64);
            ttft.push(rec.ttft().map(ms).unwrap_or_default());
            windows.push(EnergyWindow {
                start: rec.sent,
                end: first,
                tokens: t.prompt_n,
            });
            cx.steps.advance();
        }
        // TTFT windows at the short lengths are tens of milliseconds:
        // shorter than the sampler resolves, so they get a reason, not a
        // figure (decision 56).
        let (energy, energy_unmeasured) = cx.sampler.energy(&windows).await.into_parts();
        out.push(PrefillPoint {
            prompt_tokens: p,
            prompt_tok_s: stat(&tok_s),
            prompt_ms: stat(&prompt_ms),
            ttft_ms: stat(&ttft),
            evaluated_tokens: stat(&evaluated),
            energy,
            energy_unmeasured,
        });
    }
    Ok(())
}
