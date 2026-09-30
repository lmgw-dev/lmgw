//! Concurrent decode (benchmark design §4.3): *N* streams released together,
//! each a 256-token prompt from its own corpus offset, `n_predict: G`,
//! `ignore_eos`. Aggregate = Σ `predicted_n` / (last stream's end − first
//! stream's first token); per-stream = the median of the streams'
//! `predicted_per_second`.

use lmgw_api_types::bench::ConcurrentPoint;

use super::cx::{completion_body, salt, PhaseCx};
use crate::bench::client::BenchError;
use crate::bench::corpus::Offsets;
use crate::bench::sampler::EnergyWindow;
use crate::bench::stats::{aggregate_tok_s, draft_stats, median, stat, StreamSpan};

pub async fn run(cx: &PhaseCx<'_>, out: &mut Vec<ConcurrentPoint>) -> Result<(), BenchError> {
    let reps = cx.reps();
    let g = cx.params.generate_tokens as u64;
    let prompt_len = cx.params.stream_prompt_tokens as usize;
    let mut offsets = Offsets::new(cx.corpus.len(), salt::CONCURRENT);
    for &n in &cx.plan.concurrent {
        let mut aggregate = Vec::new();
        let mut per_stream = Vec::new();
        let mut timings = Vec::new();
        let mut windows = Vec::new();
        for r in 1..=reps {
            cx.steps
                .stage(format!("concurrent: {n} streams, repetition {r}/{reps}"))
                .await;
            cx.reset_slots().await?;
            let bodies: Vec<_> = (0..n)
                .map(|_| {
                    let prompt = cx.corpus.prompt(offsets.next().unwrap_or(0), prompt_len);
                    completion_body(&prompt, g, false, &cx.params.sampling)
                })
                .collect();
            // Built first, then released together: every stream's `sent` is
            // taken as its request goes out, within the same poll.
            let records = futures::future::join_all(bodies.iter().map(|b| cx.client.completion(b)))
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
            let mut spans = Vec::with_capacity(records.len());
            let mut rates = Vec::with_capacity(records.len());
            for rec in &records {
                let t = *rec
                    .timings()
                    .map_err(|e| BenchError::Failed(format!("concurrent {n}: {e}")))?;
                let (Some(first_token), Some(end)) = (rec.first_token(), rec.end) else {
                    return Err(BenchError::Failed(format!(
                        "concurrent {n}: a stream produced no token"
                    )));
                };
                spans.push(StreamSpan {
                    first_token,
                    end,
                    predicted_n: t.predicted_n,
                });
                rates.push(t.predicted_per_second);
                timings.push(t);
            }
            let agg = aggregate_tok_s(&spans).ok_or_else(|| {
                BenchError::Failed(format!("concurrent {n}: an empty decode window"))
            })?;
            aggregate.push(agg);
            per_stream.push(median(&rates).unwrap_or_default());
            let start = spans.iter().map(|s| s.first_token).min();
            let end = spans.iter().map(|s| s.end).max();
            if let (Some(start), Some(end)) = (start, end) {
                windows.push(EnergyWindow {
                    start,
                    end,
                    tokens: spans.iter().map(|s| s.predicted_n).sum(),
                });
            }
            cx.steps.advance();
        }
        let (energy, energy_unmeasured) = cx.sampler.energy(&windows).await.into_parts();
        out.push(ConcurrentPoint {
            streams: n,
            aggregate_tok_s: stat(&aggregate),
            per_stream_tok_s: stat(&per_stream),
            draft: draft_stats(&timings),
            energy,
            energy_unmeasured,
        });
    }
    Ok(())
}
