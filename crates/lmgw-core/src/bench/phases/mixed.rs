//! Prefill under decode (benchmark design §4.3): *N_slots* − 1 streams decode;
//! after 2 s of steady decode one prompt of *P_inj* tokens is injected; every
//! streamed token is timestamped on the client.
//!
//! One repetition:
//!
//! 1. release the decoding streams (256-token prompts, `n_predict` = what the
//!    slot holds, `ignore_eos`) and wait until each has produced a token;
//! 2. wait the steady window, then send the injected prompt (`n_predict: 1`)
//!    and wait for its answer;
//! 3. wait until every decoding stream has produced one more token after the
//!    injected first token (or has ended), so a stall that spans the end of
//!    the window is measured whole rather than cut off;
//! 4. close the decoding streams — dropping them closes their connections,
//!    and llama-server stops generating for a client that went away.
//!
//! Everything is driven inside this task (no spawned streams), so a cancel
//! that drops the repetition drops every connection with it.
//!
//! **Sizing** (§13 decision 57). Before the repetitions, one unmeasured
//! request ([`measure_rates`]) reads a stream's decode and prefill rate, and
//! [`crate::bench::points::size_mixed`] checks that a stream can decode
//! through the steady window and the injection — and, on a shared KV pool,
//! sizes the injection to what fits beside the decoding streams. A phase
//! that cannot fit is a planned skip, not an error.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lmgw_api_types::bench::{MixedPlan, MixedResult};

use super::cx::{completion_body, salt, PhaseCx};
use crate::bench::client::{lock, BenchError};
use crate::bench::corpus::Offsets;
use crate::bench::points::Rates;
use crate::bench::stats::{mixed_window, ms, stat, MixedWindow};
use crate::bench::stream::StreamRecord;

/// How often the waits below look at the streams' records. Only a reaction
/// time: every timestamp is taken when its chunk arrives, not here.
const POLL: Duration = Duration::from_millis(5);

/// One unmeasured request shaped like a decoding stream — its prompt, *G*
/// generated, no cache — for the rates the phase is sized from: one stream
/// on an otherwise idle server, the fastest a stream decodes here.
pub async fn measure_rates(cx: &PhaseCx<'_>, plan: &MixedPlan) -> Result<Rates, BenchError> {
    let offset = Offsets::new(cx.corpus.len(), salt::MIXED_RATES)
        .next()
        .unwrap_or(0);
    let prompt = cx.corpus.prompt(offset, plan.stream_prompt_tokens as usize);
    let g = u64::from(cx.params.generate_tokens);
    let body = completion_body(&prompt, g, false, &cx.params.sampling);
    let rec = cx.client.completion(&body).await?;
    let t = rec
        .timings()
        .map_err(|e| BenchError::Failed(format!("mixed, measuring a stream's rates: {e}")))?;
    let usable = |v: f64| v.is_finite() && v > 0.0;
    if !usable(t.predicted_per_second) || !usable(t.prompt_per_second) {
        return Err(BenchError::Failed(format!(
            "mixed, measuring a stream's rates: the server reported {} tok/s decode and {} tok/s \
             prefill",
            t.predicted_per_second, t.prompt_per_second
        )));
    }
    Ok(Rates {
        decode: t.predicted_per_second,
        prefill: t.prompt_per_second,
    })
}

/// Measure the phase as `plan` (already sized) says.
pub async fn run(
    cx: &PhaseCx<'_>,
    plan: &MixedPlan,
    solo_ttft_ms: Option<f64>,
    out: &mut Option<MixedResult>,
) -> Result<(), BenchError> {
    let plan = plan.clone();
    let reps = cx.reps();
    let steady = Duration::from_millis(cx.params.mixed_steady_ms);
    let mut offsets = Offsets::new(cx.corpus.len(), salt::MIXED);
    let mut result = MixedResult {
        streams: plan.streams,
        inject_tokens: plan.inject_tokens,
        solo_ttft_ms,
        ..Default::default()
    };
    let (mut before, mut during, mut stall, mut ttft) = (vec![], vec![], vec![], vec![]);
    for r in 1..=reps {
        cx.steps
            .stage(format!(
                "mixed: {} decoding streams, injecting {} tokens, repetition {r}/{reps}",
                plan.streams, plan.inject_tokens
            ))
            .await;
        cx.reset_slots().await?;
        let (w, inject_ttft) = cx
            .client
            .guarded(repetition(cx, &plan, steady, &mut offsets))
            .await
            .map_err(|e| match e {
                BenchError::Failed(m) => BenchError::Failed(format!("mixed, repetition {r}: {m}")),
                c => c,
            })?;
        if w.open_streams > 0 {
            result.notes.push(format!(
                "repetition {r}: {} stream(s) produced no token after the injected first token; \
                 their stall is counted up to it",
                w.open_streams
            ));
        }
        before.push(w.before_tok_s);
        during.push(w.during_tok_s);
        stall.push(w.stall_ms);
        ttft.push(inject_ttft);
        cx.steps.advance();
        // Persist per repetition: a later one failing keeps these.
        result.before_tok_s = stat(&before);
        result.during_tok_s = stat(&during);
        result.stall_ms = stat(&stall);
        result.inject_ttft_ms = stat(&ttft);
        *out = Some(result.clone());
    }
    Ok(())
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        tokio::time::sleep(POLL).await;
    }
}

async fn repetition(
    cx: &PhaseCx<'_>,
    plan: &MixedPlan,
    steady: Duration,
    offsets: &mut Offsets,
) -> Result<(MixedWindow, f64), BenchError> {
    let s = &cx.params.sampling;
    let bodies: Vec<_> = (0..plan.streams)
        .map(|_| {
            let prompt = cx.corpus.prompt(
                offsets.next().unwrap_or(0),
                plan.stream_prompt_tokens as usize,
            );
            completion_body(&prompt, plan.stream_predict, false, s)
        })
        .collect();
    let inject_prompt = cx
        .corpus
        .prompt(offsets.next().unwrap_or(0), plan.inject_tokens as usize);
    let inject_body = completion_body(&inject_prompt, 1, false, s);

    let now = Instant::now();
    let recs: Vec<Mutex<StreamRecord>> = bodies
        .iter()
        .map(|_| Mutex::new(StreamRecord::new(now)))
        .collect();
    // `try_join_all`: the first stream that fails ends the wait at once and
    // drops the others (their connections close). A plain `join_all` waited
    // for every other stream to decode to its end — up to *S* − 257 tokens,
    // twenty minutes at 262k — before the failure was seen (review
    // finding 5).
    let mut streams = std::pin::pin!(futures::future::try_join_all(
        bodies
            .iter()
            .zip(&recs)
            .map(|(b, rec)| cx.client.stream_into(b, rec))
    ));
    let mut streams_over = false;

    // 1. Every stream decoding.
    let started = || recs.iter().all(|r| !lock(r).tokens.is_empty());
    tokio::select! {
        res = &mut streams => {
            res?;
            return Err(BenchError::Failed(
                "the decoding streams ended before all of them produced a token".into(),
            ));
        }
        () = until(started) => {}
    }

    // 2. Steady decode, then the injection.
    let ready = recs
        .iter()
        .filter_map(|r| lock(r).tokens.first().map(|t| t.0))
        .max()
        .unwrap_or(now);
    tokio::select! {
        res = &mut streams => {
            res?;
            return Err(BenchError::Failed(
                "the decoding streams ended during the steady window; the slot context is too \
                 small for this phase".into(),
            ));
        }
        () = tokio::time::sleep_until((ready + steady).into()) => {}
    }
    let inject = Mutex::new(StreamRecord::new(Instant::now()));
    let mut injected = std::pin::pin!(cx.client.stream_into(&inject_body, &inject));
    loop {
        tokio::select! {
            res = &mut injected => { res?; break; }
            res = &mut streams, if !streams_over => {
                streams_over = true;
                res?;
            }
        }
    }
    let (inject_sent, inject_first, inject_ttft) = {
        let rec = lock(&inject);
        let first = rec
            .first_token()
            .ok_or_else(|| BenchError::Failed("the injected request produced no token".into()))?;
        (rec.sent, first, rec.ttft().map(ms).unwrap_or_default())
    };

    // 3. One token past the injected first token on every stream.
    let settled = || {
        recs.iter().all(|r| {
            let r = lock(r);
            r.end.is_some() || r.tokens.last().is_some_and(|t| t.0 > inject_first)
        })
    };
    if !streams_over {
        tokio::select! {
            res = &mut streams => { res?; }
            () = until(settled) => {}
        }
    }

    // 4. Close the streams (dropping `streams` at return) and measure.
    let tokens: Vec<Vec<(Instant, u32)>> = recs.iter().map(|r| lock(r).tokens.clone()).collect();
    let w = mixed_window(&tokens, inject_sent, inject_first, steady).ok_or_else(|| {
        BenchError::Failed("the injected first token arrived no later than it was sent".into())
    })?;
    Ok((w, inject_ttft))
}
