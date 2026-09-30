//! One run of the suite against a running server (benchmark design §4.4):
//! read the server's real slot count and context, derive the points,
//! tokenize the corpus, send one unmeasured warm-up request, then probes →
//! prefill → decode → concurrent → mixed, whichever of them the run selected.
//!
//! **What the integration provides.** The container, its load phase and the
//! baseline are WP2's: it starts a [`Sampler`] before `podman run`, marks
//! [`Phase::Load`] on it, times the start, fills `results.load` and the
//! baseline half of `results.energy`, and then calls [`run_suite`] with the
//! same sampler. The cancel is the job's own flag
//! (`Cancel::flag(ctx.cancel_signal())`).
//!
//! **Hooks.** [`BenchSink::progress`] gets a stage line and `done/total` in
//! steps (one per probe, per point repetition, per mixed repetition) —
//! what `JobCtx::progress` wants. [`BenchSink::phase_done`] gets the whole
//! [`Record`] after every phase, finished or not, so the store can write the
//! row each time and a crash keeps what was measured (§7).
//!
//! **Errors.** A phase that fails keeps the points it finished, is listed in
//! `results.phase_errors`, and the run goes on to the next phase — unless the
//! server stopped answering `/health`, which ends the run there. Either way
//! the run ends [`SuiteEnd::Failed`] with the first error, and
//! `results.complete` stays false. A request whose connection broke before
//! any response is sent once more by the client and listed in
//! `results.retried` (decision 62); only a second failure is the phase's.

use async_trait::async_trait;
use lmgw_api_types::bench::{
    GpuIdentity, Phase, PhaseError, PointPlan, ProbeKind, ProbeReport, RunResults, ServerFacts,
    SuiteParams, Timeline,
};
use serde::Serialize;

use super::client::{BenchError, LlamaClient};
use super::phases::{self, PhaseCx, Steps};
use super::probes::Caps;
use super::sampler::Sampler;
use super::{corpus, facts, points};
use crate::agent::Cancel;

/// The server under test, and what the row says it can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BenchTarget {
    /// The server's root, e.g. `http://127.0.0.1:41234`.
    pub base_url: String,
    /// The row's derived capabilities (§5 "Capabilities"). `Some(false)`
    /// skips the thinking probes; `None` (unknown) runs them.
    pub reasoning: Option<bool>,
    /// Same for `tool_call`. The live `/props` `chat_template_caps` wins
    /// where the engine reports it.
    pub tools: Option<bool>,
    /// The row starts the server with a projector: the vision probe runs.
    pub projector: bool,
    /// Whether the row's slots share one KV pool
    /// (`LlamaParams::effective_kv_unified`: `kv_unified` on, or `parallel`
    /// left on auto). Official llama.cpp's `/props` and `/slots` do not say,
    /// so this is what the engine goes by there (§13 decision 57); `None`
    /// reads as split.
    pub kv_unified: Option<bool>,
}

/// One progress report: the stage line and the step counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
}

/// Where a run reports to. The integration's implementation forwards
/// progress to the job and writes the row on every `phase_done`.
#[async_trait]
pub trait BenchSink: Send + Sync {
    async fn progress(&self, p: Progress);
    /// `phase` just ended (done, failed or canceled); `record` is everything
    /// measured so far.
    async fn phase_done(&self, phase: Phase, record: &Record);
}

/// What a run measured: the `results`, `probes`, `timeline` and `gpu`
/// columns (§7). The integration may pre-fill `results.load` and the
/// baseline fields of `results.energy`; the engine keeps them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Record {
    pub results: RunResults,
    pub probes: ProbeReport,
    pub timeline: Timeline,
    pub gpu: GpuIdentity,
}

/// How [`run_suite`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuiteEnd {
    /// Every selected phase ran without an error.
    Done,
    Canceled,
    /// The first error, prefixed with its phase.
    Failed(String),
}

/// Everything [`run_suite`] needs besides the record and the sink.
pub struct Bench<'a> {
    /// No overall timeout on it (see [`super::client`]); a connect timeout
    /// is fine. `state.http` qualifies.
    pub http: reqwest::Client,
    pub target: BenchTarget,
    pub params: SuiteParams,
    /// Started by the caller before the load phase; the engine marks its own
    /// phases on it and reads energy from it.
    pub sampler: &'a Sampler,
    pub cancel: Cancel,
}

/// Run the selected measured phases against `bench.target`, filling
/// `record` and reporting to `sink` as it goes.
pub async fn run_suite(bench: &Bench<'_>, record: &mut Record, sink: &dyn BenchSink) -> SuiteEnd {
    let client = LlamaClient::new(
        bench.http.clone(),
        &bench.target.base_url,
        bench.cancel.clone(),
    );
    let staged = Staged {
        sink,
        client: &client,
    };
    let end = drive(bench, &client, record, &staged).await;
    record.results.retried.extend(client.take_retried());
    bench.sampler.end_phase();
    refresh(bench.sampler, record);
    record.results.complete = end == SuiteEnd::Done;
    end
}

/// Read the live server's facts (§3.4) — also what `bench_plan`'s caller
/// could use against a running server.
pub async fn read_facts(client: &LlamaClient) -> Result<ServerFacts, BenchError> {
    let props = client.props().await?;
    let slots = client.slots().await?;
    facts::server_facts(&props, slots.as_ref()).map_err(BenchError::Failed)
}

/// Steps a plan takes, for `done/total`.
pub fn total_steps(plan: &PointPlan, params: &SuiteParams) -> u64 {
    let reps = params.repetitions.max(1) as u64;
    params
        .phases
        .iter()
        .map(|p| match p {
            Phase::Load => 0,
            Phase::Probes => ProbeKind::ALL.len() as u64,
            Phase::Prefill => plan.prefill.len() as u64 * reps,
            Phase::Decode => plan.decode.len() as u64 * reps,
            Phase::Concurrent => plan.concurrent.len() as u64 * reps,
            Phase::Mixed => u64::from(plan.mixed.is_some()) * reps,
        })
        .sum()
}

/// One unmeasured request before the first measured phase (decision 39): a
/// fresh server's first request pays for CUDA module loading and buffer
/// growth, which would otherwise land on whichever point happens to run
/// first — prefill at 512 when the probes are deselected.
async fn warm_up(
    client: &LlamaClient,
    params: &SuiteParams,
    plan: &PointPlan,
    corpus_tokens: &corpus::Corpus,
) -> Result<(), BenchError> {
    let offset = corpus::Offsets::new(corpus_tokens.len(), phases::salt::WARMUP)
        .next()
        .unwrap_or(0);
    let bodies: Vec<_> = phases::unmeasured_body(params, plan.per_slot_ctx, corpus_tokens, offset)
        .into_iter()
        .collect();
    phases::unmeasured(client, &bodies, "warm-up").await
}

/// The sink the suite reports to, which also tells the client which step
/// its requests belong to, so a resend's record names it (decision 62).
struct Staged<'a> {
    sink: &'a dyn BenchSink,
    client: &'a LlamaClient,
}

#[async_trait]
impl BenchSink for Staged<'_> {
    async fn progress(&self, p: Progress) {
        self.client.set_stage(&p.stage);
        self.sink.progress(p).await;
    }

    async fn phase_done(&self, phase: Phase, record: &Record) {
        self.sink.phase_done(phase, record).await;
    }
}

fn refresh(sampler: &Sampler, record: &mut Record) {
    record.timeline = sampler.timeline();
    record.gpu = sampler.gpu_identity();
    sampler.fill_energy_summary(&mut record.results.energy);
}

async fn setup_stage(sink: &dyn BenchSink, stage: &str) {
    sink.progress(Progress {
        stage: stage.into(),
        done: 0,
        total: 0,
    })
    .await;
}

fn ended(e: BenchError, what: &str) -> SuiteEnd {
    match e {
        BenchError::Canceled => SuiteEnd::Canceled,
        BenchError::Failed(m) => SuiteEnd::Failed(format!("{what}: {m}")),
    }
}

async fn drive(
    bench: &Bench<'_>,
    client: &LlamaClient,
    record: &mut Record,
    sink: &dyn BenchSink,
) -> SuiteEnd {
    let params = &bench.params;
    setup_stage(sink, "reading the server's slots and context").await;
    let mut facts = match read_facts(client).await {
        Ok(f) => f,
        Err(e) => return ended(e, "reading /props and /slots"),
    };
    if facts.kv_unified.is_none() {
        if let Some(u) = bench.target.kv_unified {
            facts.kv_unified = Some(u);
            facts.kv_unified_source = Some("settings".into());
        }
    }
    let pool = shared_pool(&facts);
    let plan = points::derive(facts.per_slot_ctx, facts.n_slots, pool, params);
    record.results.server = Some(facts.clone());
    record.results.points = Some(plan.clone());

    let measured: Vec<Phase> = params
        .phases
        .iter()
        .copied()
        .filter(|p| *p != Phase::Load)
        .collect();
    if measured.is_empty() {
        return SuiteEnd::Done;
    }

    setup_stage(sink, "tokenizing the corpus").await;
    let corpus_tokens = match client.tokenize_corpus(corpus::CORPUS_V1).await {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return SuiteEnd::Failed("tokenizing the corpus: no tokens".into()),
        Err(e) => return ended(e, "tokenizing the corpus"),
    };
    record.results.corpus_tokens = Some(corpus_tokens.len() as u64);
    record.results.prompt_prefix = corpus_tokens.prefix.clone();

    setup_stage(sink, "warming the server up").await;
    if let Err(BenchError::Canceled) = warm_up(client, params, &plan, &corpus_tokens).await {
        return SuiteEnd::Canceled;
    }

    let steps = Steps::new(total_steps(&plan, params), sink);
    let cx = PhaseCx {
        client,
        params,
        plan: &plan,
        server: &facts,
        corpus: &corpus_tokens,
        caps: Caps {
            reasoning: bench.target.reasoning,
            tools: bench.target.tools,
            projector: bench.target.projector,
        },
        sampler: bench.sampler,
        steps: &steps,
        resets: Default::default(),
    };

    let mut first_error: Option<String> = None;
    for phase in measured {
        bench.sampler.mark(phase);
        let r = &mut record.results;
        let res = match phase {
            Phase::Load => Ok(()),
            Phase::Probes => phases::probes::run(&cx, &mut record.probes).await,
            Phase::Prefill => phases::prefill::run(&cx, &mut r.prefill).await,
            Phase::Decode => phases::decode::run(&cx, &mut r.decode).await,
            Phase::Concurrent => phases::concurrent::run(&cx, &mut r.concurrent).await,
            Phase::Mixed => match &plan.mixed {
                None => Ok(()),
                Some(planned) => mixed_phase(&cx, planned, pool, r).await,
            },
        };
        let stop = match res {
            Ok(()) => {
                record.results.phases_done.push(phase);
                None
            }
            Err(BenchError::Canceled) => Some(SuiteEnd::Canceled),
            // Failed while the cancel is raised: the run was being ended
            // (the hold's abort removes the container at once), and the
            // failure is that, not the phase's (review finding 9).
            Err(BenchError::Failed(_)) if bench.cancel.is_raised() => Some(SuiteEnd::Canceled),
            Err(BenchError::Failed(e)) => {
                record.results.phase_errors.push(PhaseError {
                    phase,
                    error: e.clone(),
                });
                let msg = format!("{}: {e}", phase.as_str());
                first_error.get_or_insert_with(|| msg.clone());
                match client.health().await {
                    Ok(true) => None,
                    Ok(false) => Some(SuiteEnd::Failed(format!(
                        "{msg} (the server no longer answers /health, so the run stops here)"
                    ))),
                    Err(_) => Some(SuiteEnd::Canceled),
                }
            }
        };
        record.results.retried.extend(client.take_retried());
        refresh(bench.sampler, record);
        sink.phase_done(phase, record).await;
        if let Some(end) = stop {
            return end;
        }
    }
    match first_error {
        Some(e) => SuiteEnd::Failed(e),
        None => SuiteEnd::Done,
    }
}

/// The shared KV pool's size, when the slots share one: the whole context
/// where the server reports it, else *S* — every slot reports the whole
/// pool as its context (a per-slot cap can make that smaller, which only
/// errs small).
fn shared_pool(facts: &ServerFacts) -> Option<u64> {
    (facts.kv_unified == Some(true) && facts.n_slots > 1)
        .then(|| facts.total_ctx.unwrap_or(0).max(facts.per_slot_ctx))
}

/// The mixed phase (§4.3): measure a stream's rates, size the phase from
/// them (decision 57) — a skip is noted in the stored plan, not an error —
/// and run it as sized, the stored plan showing what ran.
async fn mixed_phase(
    cx: &PhaseCx<'_>,
    planned: &lmgw_api_types::bench::MixedPlan,
    pool: Option<u64>,
    results: &mut RunResults,
) -> Result<(), BenchError> {
    cx.steps
        .stage("mixed: measuring a stream's decode and prefill rate".into())
        .await;
    let rates = phases::mixed::measure_rates(cx, planned).await?;
    let sized = points::size_mixed(
        planned,
        cx.plan.per_slot_ctx,
        pool,
        cx.params.mixed_steady_ms,
        rates,
    );
    let stored = results.points.get_or_insert_with(|| cx.plan.clone());
    let plan = match sized {
        points::MixedSizing::Skip(why) => {
            stored.mixed = None;
            stored.notes.retain(|n| !n.starts_with("mixed:"));
            stored.notes.push(why);
            return Ok(());
        }
        points::MixedSizing::Run(plan, note) => {
            if let Some(note) = note {
                stored.notes.retain(|n| !n.starts_with("mixed:"));
                stored.notes.push(note);
            }
            stored.mixed = Some(plan.clone());
            plan
        }
    };
    let solo = results
        .prefill
        .iter()
        .find(|p| p.prompt_tokens == plan.inject_tokens)
        .map(|p| p.ttft_ms.median);
    phases::mixed::run(cx, &plan, solo, &mut results.mixed).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The integration runs a suite inside a spawned job, so its future must
    /// be `Send` — checked at compile time.
    #[allow(dead_code)]
    fn run_suite_is_send(bench: &Bench<'_>, record: &mut Record, sink: &dyn BenchSink) {
        fn send<T: Send>(_: T) {}
        send(run_suite(bench, record, sink));
    }

    #[test]
    fn steps_count_every_selected_repetition() {
        let params = SuiteParams::v1(3, vec![Phase::Probes, Phase::Decode, Phase::Mixed]);
        let plan = points::derive(8192, 2, None, &params);
        // 9 probes + 4 decode depths × 3 + one mixed point × 3.
        assert_eq!(plan.decode.len(), 4);
        assert_eq!(total_steps(&plan, &params), 9 + 4 * 3 + 3);
        let one_slot = points::derive(8192, 1, None, &params);
        assert_eq!(total_steps(&one_slot, &params), 9 + 4 * 3);
    }
}
