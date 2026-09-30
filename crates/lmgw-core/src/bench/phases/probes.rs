//! The behaviour probes' runner (benchmark design §5), in
//! [`ProbeKind::ALL`] order with the needle last. A probe's own failure —
//! a 500, a wrong answer, a refused connection — is its outcome, never the
//! phase's error; only a cancel ends the phase early.

use lmgw_api_types::bench::{ProbeKind, ProbeOutcome, ProbeReport, ProbeResult};

use super::cx::{salt, PhaseCx};
use crate::bench::client::{BenchError, HttpAnswer};
use crate::bench::corpus::{self, Offsets};
use crate::bench::png;
use crate::bench::probes::{self as p, transport_error};

pub async fn run(cx: &PhaseCx<'_>, out: &mut ProbeReport) -> Result<(), BenchError> {
    let total = ProbeKind::ALL.len();
    for (i, kind) in ProbeKind::ALL.into_iter().enumerate() {
        cx.steps
            .stage(format!("probes: {} ({}/{total})", kind.as_str(), i + 1))
            .await;
        let result = match p::skip_reason(kind, cx.caps, cx.server, cx.plan.needle_tokens) {
            Some(reason) => ProbeResult {
                probe: kind,
                outcome: ProbeOutcome::Skipped,
                detail: reason,
                evidence: Vec::new(),
            },
            None => probe(cx, kind).await?,
        };
        out.probes.push(result);
        cx.steps.advance();
    }
    Ok(())
}

/// A request's answer, or the probe's `error` result when there was none.
async fn answer(
    kind: ProbeKind,
    fut: impl std::future::Future<Output = Result<HttpAnswer, BenchError>>,
) -> Result<Result<HttpAnswer, ProbeResult>, BenchError> {
    match fut.await {
        Ok(a) => Ok(Ok(a)),
        Err(BenchError::Canceled) => Err(BenchError::Canceled),
        Err(BenchError::Failed(m)) => Ok(Err(transport_error(kind, &m))),
    }
}

async fn probe(cx: &PhaseCx<'_>, kind: ProbeKind) -> Result<ProbeResult, BenchError> {
    let seed = cx.params.sampling.seed;
    let c = cx.client;
    let one = |body: serde_json::Value, judge: fn(&HttpAnswer) -> ProbeResult| async move {
        Ok::<_, BenchError>(match answer(kind, c.chat(&body)).await? {
            Ok(a) => judge(&a),
            Err(e) => e,
        })
    };
    match kind {
        ProbeKind::Chat => one(p::chat_request(seed), p::judge_chat).await,
        ProbeKind::ThinkingOff => {
            one(p::thinking_request(false, seed), p::judge_thinking_off).await
        }
        ProbeKind::ThinkingOn => one(p::thinking_request(true, seed), p::judge_thinking_on).await,
        ProbeKind::ToolCall => one(p::tool_request(seed), p::judge_tool_call).await,
        ProbeKind::JsonSchema => one(p::json_schema_request(seed), p::judge_json_schema).await,
        ProbeKind::Vision => {
            one(
                p::vision_request(&png::solid_red_data_url(), seed),
                p::judge_vision,
            )
            .await
        }
        ProbeKind::Deterministic => {
            // Two pairs: cache reuse allowed, as a client would send it (the
            // current request), then again with `cache_prompt: false` —
            // isolating whether prompt-cache reuse is the source of any
            // non-determinism (§2.1, §5).
            let cached_body = p::deterministic_request(seed);
            let no_cache_body = p::deterministic_request_no_cache(seed);
            let ca = match answer(kind, c.chat(&cached_body)).await? {
                Ok(a) => a,
                Err(e) => return Ok(e),
            };
            let cb = match answer(kind, c.chat(&cached_body)).await? {
                Ok(b) => b,
                Err(e) => return Ok(e),
            };
            let na = match answer(kind, c.chat(&no_cache_body)).await? {
                Ok(a) => a,
                Err(e) => return Ok(e),
            };
            let nb = match answer(kind, c.chat(&no_cache_body)).await? {
                Ok(b) => b,
                Err(e) => return Ok(e),
            };
            Ok(p::judge_deterministic((&ca, &cb), (&na, &nb)))
        }
        ProbeKind::ReasoningHistory => Ok(
            match answer(kind, c.apply_template(&p::history_template_request())).await? {
                Ok(a) => p::judge_reasoning_history(&a),
                Err(e) => e,
            },
        ),
        ProbeKind::Needle => {
            let Some(len) = cx.plan.needle_tokens else {
                // `skip_reason` already skipped it; kept total all the same.
                return Ok(ProbeResult {
                    probe: kind,
                    outcome: ProbeOutcome::Skipped,
                    detail: "no needle length".into(),
                    evidence: Vec::new(),
                });
            };
            let offset = Offsets::new(cx.corpus.len(), salt::NEEDLE)
                .next()
                .unwrap_or(0);
            // Plain corpus text: the chat template adds its own BOS.
            let haystack = corpus::slice(&cx.corpus.tokens, offset, len as usize);
            let text = match c.detokenize(&haystack).await {
                Ok(t) => t,
                Err(BenchError::Canceled) => return Err(BenchError::Canceled),
                Err(BenchError::Failed(m)) => return Ok(transport_error(kind, &m)),
            };
            one(p::needle_request(&text, seed), p::judge_needle).await
        }
    }
}
