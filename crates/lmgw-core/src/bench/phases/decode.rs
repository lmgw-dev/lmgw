//! Decode (benchmark design §4.3): at each context depth *D*, `n_predict: G`,
//! `ignore_eos`, `cache_prompt: true`, `timings.predicted_per_second`.
//!
//! All repetitions at a depth send the **same** prompt: the first one primes
//! the KV cache and the later ones reuse it, which saves re-prefilling a deep
//! context for every repetition (energy's sake). The prefill therefore stays
//! out of the decode figures twice over — the server splits its timings, and
//! the energy window runs from the first generated token to the last.
//!
//! The first repetition at a depth is preceded by the slot reset the other
//! phases use (decision 61). ik_llama.cpp keeps every slot's cells in one KV
//! buffer, so a decode beside the deep prompt another slot kept from the
//! previous phase ran at about that depth's speed: depth 64 read 383 tok/s
//! right after the prefill phase (depth 16127: 367), 448 without one.
//! Official llama.cpp gives each slot its own stream, or with unified KV
//! clears idle slots when a task starts, so it read the same either way.
//! The later repetitions keep the primed cache, so they get no reset.

use lmgw_api_types::bench::DecodePoint;

use super::cx::{completion_body, salt, PhaseCx};
use crate::bench::client::BenchError;
use crate::bench::corpus::Offsets;
use crate::bench::sampler::EnergyWindow;
use crate::bench::stats::{draft_stats, stat};

pub async fn run(cx: &PhaseCx<'_>, out: &mut Vec<DecodePoint>) -> Result<(), BenchError> {
    let reps = cx.reps();
    let g = cx.params.generate_tokens as u64;
    let mut offsets = Offsets::new(cx.corpus.len(), salt::DECODE);
    for &d in &cx.plan.decode {
        let prompt = cx.corpus.prompt(offsets.next().unwrap_or(0), d as usize);
        let body = completion_body(&prompt, g, true, &cx.params.sampling);
        let mut tok_s = Vec::new();
        let mut generated = Vec::new();
        let mut evaluated = Vec::new();
        let mut timings = Vec::new();
        let mut windows = Vec::new();
        let mut distinct = Vec::new();
        for r in 1..=reps {
            cx.steps
                .stage(format!(
                    "decode: depth {d}, {g} tokens, repetition {r}/{reps}"
                ))
                .await;
            if r == 1 {
                cx.reset_slots().await?;
            }
            let rec = cx.client.completion(&body).await?;
            let t = *rec
                .timings()
                .map_err(|e| BenchError::Failed(format!("decode at depth {d}: {e}")))?;
            tok_s.push(t.predicted_per_second);
            generated.push(t.predicted_n as f64);
            evaluated.push(t.prompt_n as f64);
            distinct.extend(rec.distinct_ratio(t.predicted_n));
            timings.push(t);
            if let Some((start, end, tokens)) = rec.decode_window() {
                windows.push(EnergyWindow { start, end, tokens });
            }
            cx.steps.advance();
        }
        let (energy, energy_unmeasured) = if windows.len() == reps as usize {
            cx.sampler.energy(&windows).await.into_parts()
        } else {
            (None, None)
        };
        out.push(DecodePoint {
            depth: d,
            tok_s: stat(&tok_s),
            generated: stat(&generated),
            evaluated_tokens: stat(&evaluated),
            draft: draft_stats(&timings),
            // Every repetition's, or none: a ratio over some would read as
            // the point's.
            distinct_token_ratio: (distinct.len() == reps as usize).then(|| stat(&distinct)),
            energy,
            energy_unmeasured,
        });
    }
    Ok(())
}
