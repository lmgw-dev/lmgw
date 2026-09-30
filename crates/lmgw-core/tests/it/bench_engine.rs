//! The benchmark engine against a fake llama-server (benchmark design §9,
//! WP1): every phase and probe, both engines' `/props` shapes, `/apply-template`
//! as 200 and as ik's 500, energy from a scripted counter and from integrated
//! power, a cancel that drops the in-flight request, and a failing phase
//! that keeps the run going. The fake and the scripted GPU are
//! `support/llama_fake.rs`.
//!
//! `live_engine_against_a_running_server` is the one ignored test: it drives
//! a real llama-server (and the real NVML) when `LMGW_BENCH_LIVE_URL` points
//! at one, which is how WP1's numbers were sanity-checked on the 4090.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lmgw_api_types::bench::{EnergySource, MixedPlan, Phase, ProbeKind, ProbeOutcome, SuiteParams};
use lmgw_core::agent::Cancel;
use lmgw_core::bench::{
    run_suite, Bench, BenchSink, BenchTarget, Progress, Record, Sampler, SuiteEnd,
};
use lmgw_core::vram::nvml::NoTelemetry;
use lmgw_core::vram::GpuProbe;

use crate::support::llama_fake::{self, Config, FakeLlama, ScriptedGpu, Shape};

#[derive(Default)]
struct Sink {
    progress: Mutex<Vec<Progress>>,
    phases: Mutex<Vec<(Phase, Record)>>,
}

#[async_trait]
impl BenchSink for Sink {
    async fn progress(&self, p: Progress) {
        self.progress.lock().unwrap().push(p);
    }

    async fn phase_done(&self, phase: Phase, record: &Record) {
        self.phases.lock().unwrap().push((phase, record.clone()));
    }
}

/// Suite v1 with its sizes shrunk so a run takes seconds: *G* = 32, 96-token
/// streams (a length no other point has), a 300 ms steady window.
fn fast(reps: u32, phases: &[Phase]) -> SuiteParams {
    let mut p = SuiteParams::v1(reps, phases.to_vec());
    p.generate_tokens = 32;
    p.stream_prompt_tokens = 96;
    p.mixed_steady_ms = 300;
    p
}

struct Ran {
    end: SuiteEnd,
    record: Record,
    sink: Arc<Sink>,
    fake: FakeLlama,
    took: Duration,
}

/// The sampler's tick in these tests: energy windows shorter than two of
/// them (40 ms) are too short to measure (decision 56).
const TICK: Duration = Duration::from_millis(20);

async fn run(
    cfg: Config,
    target: BenchTarget,
    params: SuiteParams,
    probe: Arc<dyn GpuProbe>,
    cancel: Cancel,
) -> Ran {
    run_ticked(cfg, target, params, probe, cancel, TICK).await
}

/// [`run`] with the sampler ticking every `tick`.
async fn run_ticked(
    cfg: Config,
    target: BenchTarget,
    params: SuiteParams,
    probe: Arc<dyn GpuProbe>,
    cancel: Cancel,
    tick: Duration,
) -> Ran {
    let fake = llama_fake::start(cfg).await;
    let sampler = Sampler::with_interval(probe, tick);
    sampler.mark(Phase::Load);
    let bench = Bench {
        http: reqwest::Client::new(),
        target: BenchTarget {
            base_url: format!("{}/", fake.url),
            ..target
        },
        params,
        sampler: &sampler,
        cancel,
    };
    let sink = Arc::new(Sink::default());
    let mut record = Record::default();
    let t0 = Instant::now();
    let end = run_suite(&bench, &mut record, sink.as_ref()).await;
    Ran {
        end,
        record,
        sink,
        fake,
        took: t0.elapsed(),
    }
}

fn outcome(r: &Record, kind: ProbeKind) -> ProbeOutcome {
    r.probes
        .probes
        .iter()
        .find(|p| p.probe == kind)
        .unwrap_or_else(|| panic!("no {kind:?} result"))
        .outcome
}

/// Wait (up to two seconds) until the fake has seen `n` clients leave
/// before their stream's end.
async fn closed_early(fake: &FakeLlama, n: usize) {
    for _ in 0..200 {
        if fake.seen.closed_early.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the fake saw {} early closes, expected {n}",
        fake.seen.closed_early.load(Ordering::SeqCst)
    );
}

fn near(value: f64, expected: f64, tolerance: f64) -> bool {
    (value - expected).abs() <= tolerance
}

/// The whole suite, official shape: every phase's points, probes, energy
/// and timeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_whole_suite_against_the_official_shape() {
    let gpu = ScriptedGpu::new(100, true, 0x1);
    let target = BenchTarget {
        reasoning: Some(true),
        tools: Some(true),
        projector: true,
        ..Default::default()
    };
    let ran = run(
        Config::default(),
        target,
        fast(2, &Phase::ALL),
        gpu,
        Cancel::none(),
    )
    .await;
    let r = &ran.record;
    assert_eq!(ran.end, SuiteEnd::Done, "{:?}", r.results.phase_errors);
    assert!(r.results.complete);

    // The live facts and the points derived from them.
    let facts = r.results.server.as_ref().unwrap();
    assert_eq!((facts.n_slots, facts.per_slot_ctx), (2, 4096));
    assert_eq!(facts.build_info.as_deref(), Some("b11226-0c6a6a7"));
    let plan = r.results.points.as_ref().unwrap();
    assert_eq!(plan.prefill, vec![512, 2048, 4094]);
    assert_eq!(plan.decode, vec![64, 1024, 4063]);
    assert_eq!(plan.concurrent, vec![1, 2]);
    assert_eq!(
        plan.mixed,
        Some(MixedPlan {
            streams: 1,
            stream_prompt_tokens: 96,
            stream_predict: 3999,
            inject_tokens: 4094
        })
    );
    assert_eq!(r.results.corpus_tokens, Some(20_000));
    assert_eq!(
        r.results.phases_done,
        vec![
            Phase::Probes,
            Phase::Prefill,
            Phase::Decode,
            Phase::Concurrent,
            Phase::Mixed
        ]
    );

    // Probes: all pass against the fake, the history is information.
    let kinds: Vec<ProbeKind> = r.probes.probes.iter().map(|p| p.probe).collect();
    assert_eq!(kinds, ProbeKind::ALL.to_vec());
    for k in ProbeKind::ALL {
        let expected = if k == ProbeKind::ReasoningHistory {
            ProbeOutcome::Info
        } else {
            ProbeOutcome::Pass
        };
        assert_eq!(outcome(r, k), expected, "{k:?}: {:?}", r.probes.probes);
    }
    let history = &r.probes.probes[7];
    assert!(history.detail.starts_with("dropped"), "{}", history.detail);

    // Prefill: exact prompt lengths, no cache, TTFT at least the fake's
    // 50 µs per prompt token, energy from the counter at 100 W — where the
    // TTFT windows are long enough to measure (4094 tokens: ≈ 205 ms; 512
    // tokens' window sits near the 40 ms two ticks need, either way).
    assert_eq!(r.results.prefill.len(), 3);
    for pt in &r.results.prefill {
        assert_eq!(pt.prompt_tok_s.values.len(), 2);
        assert_eq!(pt.evaluated_tokens.median, pt.prompt_tokens as f64);
        assert!(pt.ttft_ms.min >= pt.prompt_tokens as f64 * 0.05, "{pt:?}");
        assert!(pt.prompt_tok_s.median > 0.0);
        let Some(e) = pt.energy.as_ref() else {
            let why = pt.energy_unmeasured.as_deref().unwrap_or_default();
            assert!(why.starts_with("too short to measure"), "{pt:?}");
            assert!(pt.ttft_ms.min < 2.0 * TICK.as_millis() as f64, "{pt:?}");
            continue;
        };
        assert_eq!(pt.energy_unmeasured, None);
        assert_eq!(e.source, EnergySource::Counter);
        assert!(near(e.avg_w, 100.0, 5.0), "{e:?}");
        assert_eq!(e.tokens, 2 * pt.prompt_tokens);
        assert!(e.tokens_per_joule.unwrap() > 0.0);
    }
    assert!(
        r.results.prefill[2].energy.is_some(),
        "{:?}",
        r.results.prefill[2]
    );

    // Decode: the first repetition primes the cache, the second reuses it.
    assert_eq!(r.results.decode.len(), 3);
    for pt in &r.results.decode {
        assert_eq!(
            pt.evaluated_tokens.values,
            vec![pt.depth as f64, 1.0],
            "{pt:?}"
        );
        assert_eq!(pt.generated.median, 32.0);
        assert!(pt.tok_s.median > 0.0 && pt.tok_s.median < 600.0, "{pt:?}");
        assert!(pt.draft.is_none());
        // The official shape streams every id; the fake's never repeat.
        let distinct = pt.distinct_token_ratio.as_ref().expect("ids streamed");
        assert_eq!((distinct.median, distinct.values.len()), (1.0, 2), "{pt:?}");
        let e = pt.energy.as_ref().expect("decode energy");
        assert_eq!(e.tokens, 2 * 31, "tokens after the first, per repetition");
        assert!(near(e.avg_w, 100.0, 5.0), "{e:?}");
        // One figure per repetition's window, around the point's own.
        let each = &e.tokens_per_joule_each;
        assert_eq!(each.values.len(), 2, "{e:?}");
        let tpj = e.tokens_per_joule.unwrap();
        assert!(each.min <= tpj * 1.001 && each.max >= tpj * 0.999, "{e:?}");
    }

    // Concurrent: two streams really ran together, and decode faster in
    // aggregate than one.
    let c = &r.results.concurrent;
    assert_eq!(c.iter().map(|p| p.streams).collect::<Vec<_>>(), vec![1, 2]);
    assert!(ran.fake.seen.max_inflight.load(Ordering::SeqCst) >= 2);
    assert!(
        c[1].aggregate_tok_s.median > 1.3 * c[0].aggregate_tok_s.median,
        "{c:?}"
    );
    assert!(c[1].energy.is_some());

    // Mixed: the injected 4094-token prefill (≈ 205 ms under the fake's
    // compute lock) stalls the decoding stream.
    let m = r.results.mixed.as_ref().expect("mixed result");
    assert_eq!((m.streams, m.inject_tokens), (1, 4094));
    assert_eq!(m.stall_ms.values.len(), 2);
    assert!(m.stall_ms.min >= 150.0, "{m:?}");
    assert!(m.inject_ttft_ms.min >= 150.0, "{m:?}");
    assert!(m.before_tok_s.median > m.during_tok_s.median, "{m:?}");
    assert!(m.solo_ttft_ms.is_some());
    assert!(m.notes.is_empty(), "{:?}", m.notes);
    // Each mixed repetition closed its decoding stream early (the fake
    // notices a little later, at its next write).
    closed_early(&ran.fake, 2).await;

    // Every request: token arrays, pinned sampling, the right cache mode.
    let bodies = ran.fake.seen.completions.lock().unwrap().clone();
    for b in &bodies {
        assert_eq!(b["seed"], 1234);
        assert_eq!(b["top_k"], 40);
        assert_eq!(b["ignore_eos"], true);
        let n = b["n_predict"].as_u64().unwrap();
        let cache = b["cache_prompt"].as_bool().unwrap();
        assert_eq!(
            cache,
            n == 32 && b["prompt"].as_array().unwrap().len() != 96
        );
    }
    // Two repetitions of a prefill point start at different offsets, after
    // the vocabulary's BOS.
    let p512: Vec<_> = bodies
        .iter()
        .filter(|b| b["n_predict"] == 1 && b["prompt"].as_array().unwrap().len() == 512)
        .collect();
    assert_eq!(p512.len(), 2);
    assert_ne!(p512[0]["prompt"][1], p512[1]["prompt"][1]);
    // Review finding 6: every prompt starts with the BOS (llama-server adds
    // none to token ids), counted inside its length, and holds it only there.
    assert_eq!(r.results.prompt_prefix, vec![llama_fake::BOS as u32]);
    for b in &bodies {
        let prompt = b["prompt"].as_array().unwrap();
        assert_eq!(prompt[0], llama_fake::BOS, "{b}");
        assert!(!prompt[1..].contains(&serde_json::json!(llama_fake::BOS)));
    }
    // The corpus went as pieces, special strings read as text, and no piece
    // holds a chat-template marker the specs quote.
    let tokenizes = ran.fake.seen.tokenizes.lock().unwrap().clone();
    assert!(
        tokenizes.iter().all(|t| t["parse_special"] == false),
        "{tokenizes:?}"
    );
    let pieces: Vec<&str> = tokenizes
        .iter()
        .find_map(|t| t["content"].as_array())
        .expect("the corpus as an array")
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(pieces.concat(), lmgw_core::bench::corpus::CORPUS_V1);
    assert!(pieces
        .iter()
        .all(|p| !p.contains("<think>") && !p.contains("<|channel|>")));

    // Timeline, GPU identity, energy summary.
    assert_eq!(r.timeline.interval_ms, 500);
    assert!(!r.timeline.samples.is_empty());
    let bands: Vec<Phase> = r.timeline.phases.iter().map(|s| s.phase).collect();
    assert_eq!(bands, Phase::ALL.to_vec());
    assert!(r.timeline.phases.iter().all(|s| s.end_ms.is_some()));
    assert_eq!(r.gpu.name.as_deref(), Some("FakeGPU 4090"));
    assert_eq!(r.gpu.driver.as_deref(), Some("615.71.09"));
    assert_eq!(r.gpu.power_limit_w, Some(450.0));
    assert!(!r.gpu.throttled);
    assert_eq!(r.results.energy.source, Some(EnergySource::Counter));
    assert_eq!(r.results.energy.peak_power_w, Some(100.0));
    let total = r.results.energy.total_joules.unwrap();
    assert!(total > 0.0);

    // Hooks: one phase_done per phase, progress up to the total.
    let phases: Vec<Phase> = ran
        .sink
        .phases
        .lock()
        .unwrap()
        .iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(phases, r.results.phases_done);
    let progress = ran.sink.progress.lock().unwrap();
    let last = progress.last().unwrap();
    assert_eq!(last.total, 9 + 3 * 2 + 3 * 2 + 2 * 2 + 2);
    assert_eq!(
        last.done,
        last.total - 1,
        "the last stage is reported before it runs"
    );
    assert!(progress.iter().any(|p| p.stage.starts_with("mixed:")));
}

/// ik's shape: no `build_info`, a top-level `n_ctx`, no `tokens` arrays, no
/// `cache_n`, and `/apply-template` refused with a 500. The GPU has no
/// energy counter (integrated instead) and throttles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ik_shape_template_500_and_integrated_energy() {
    let cfg = Config {
        shape: Shape::Ik,
        template_status: 500,
        // A vocabulary without a BOS: prompts are the corpus alone.
        bos: false,
        ..Config::default()
    };
    let gpu = ScriptedGpu::new(100, false, 0x1 | 0x4);
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Probes, Phase::Prefill]),
        gpu,
        Cancel::none(),
    )
    .await;
    let r = &ran.record;
    assert_eq!(ran.end, SuiteEnd::Done, "{:?}", r.results.phase_errors);
    let facts = r.results.server.as_ref().unwrap();
    assert_eq!(facts.build_info, None);
    assert_eq!(facts.model_ftype, None);
    assert_eq!(facts.total_ctx, Some(8192));
    assert_eq!(facts.per_slot_ctx_source, "slots");

    let history = r
        .probes
        .probes
        .iter()
        .find(|p| p.probe == ProbeKind::ReasoningHistory)
        .unwrap();
    assert_eq!(history.outcome, ProbeOutcome::Error);
    assert_eq!(history.evidence[0].status, Some(500));
    assert!(history.evidence[0]
        .body
        .as_deref()
        .unwrap()
        .contains("template failed"));
    assert_eq!(outcome(r, ProbeKind::Vision), ProbeOutcome::Skipped);
    // ik reports no template caps and the row says nothing: tools run.
    assert_eq!(outcome(r, ProbeKind::ToolCall), ProbeOutcome::Pass);
    assert_eq!(outcome(r, ProbeKind::ThinkingOn), ProbeOutcome::Pass);

    assert_eq!(r.results.prefill.len(), 3);
    for pt in &r.results.prefill {
        assert_eq!(pt.evaluated_tokens.median, pt.prompt_tokens as f64);
        // A TTFT window shorter than two ticks has a reason, not a figure.
        let Some(e) = pt.energy.as_ref() else {
            assert!(pt.energy_unmeasured.is_some(), "{pt:?}");
            continue;
        };
        assert_eq!(e.source, EnergySource::Integrated);
        assert!(near(e.avg_w, 100.0, 1.0), "{e:?}");
    }
    assert!(r.results.prefill[2].energy.is_some());
    assert!(r.results.decode.is_empty() && r.results.mixed.is_none());
    assert_eq!(r.results.energy.source, Some(EnergySource::Integrated));
    assert!(r.results.prompt_prefix.is_empty());
    let bodies = ran.fake.seen.completions.lock().unwrap().clone();
    assert!(bodies
        .iter()
        .all(|b| b["prompt"][0] != serde_json::json!(llama_fake::BOS)));
    assert!(r.gpu.throttled);
    assert_eq!(r.gpu.clock_events, 0x5);
}

/// Cancel while a request is in flight: the run ends promptly, the
/// connection is dropped (the fake sees its client leave), and the phase is
/// reported so the store can write what it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_drops_the_request_in_flight() {
    // Raised by the fake once the request hangs: a timer could fire before
    // the request is even sent on a slow machine, and then nothing is in
    // flight to drop.
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cfg = Config {
        hang_completion: true,
        raise_when_hanging: Some(flag.clone()),
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Prefill, Phase::Decode]),
        Arc::new(NoTelemetry("test".into())),
        Cancel::flag(flag),
    )
    .await;
    assert_eq!(ran.end, SuiteEnd::Canceled);
    assert!(ran.took < Duration::from_secs(3), "{:?}", ran.took);
    assert!(!ran.record.results.complete);
    assert!(ran.record.results.prefill.is_empty());
    let phases: Vec<Phase> = ran
        .sink
        .phases
        .lock()
        .unwrap()
        .iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(phases, vec![Phase::Prefill]);
    closed_early(&ran.fake, 1).await;
}

/// A phase that fails keeps its finished points, is recorded, and the run
/// goes on (the server is still healthy) — then ends failed. Without
/// telemetry there is no energy anywhere, and the summary says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_phase_is_recorded_and_the_run_goes_on() {
    let cfg = Config {
        fail_prompt_len: Some((2048, 500)),
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Prefill, Phase::Decode]),
        Arc::new(NoTelemetry("no GPU here".into())),
        Cancel::none(),
    )
    .await;
    let r = &ran.record.results;
    let SuiteEnd::Failed(msg) = &ran.end else {
        panic!("{:?}", ran.end)
    };
    assert!(
        msg.starts_with("prefill: ") && msg.contains("HTTP 500"),
        "{msg}"
    );
    assert!(!r.complete);
    assert_eq!(
        r.prefill.len(),
        1,
        "the 512 point was finished before 2048 failed"
    );
    assert_eq!(r.decode.len(), 3);
    assert_eq!(r.phases_done, vec![Phase::Decode]);
    assert_eq!(r.phase_errors.len(), 1);
    assert_eq!(r.phase_errors[0].phase, Phase::Prefill);
    assert!(r.prefill[0].energy.is_none());
    assert!(r.decode.iter().all(|p| p.energy.is_none()));
    // No telemetry is not "too short": nothing was there to measure.
    assert!(r.prefill[0].energy_unmeasured.is_none());
    assert!(r.decode.iter().all(|p| p.energy_unmeasured.is_none()));
    assert_eq!(r.energy.source, None);
    assert!(r
        .energy
        .unavailable
        .as_deref()
        .unwrap()
        .contains("no GPU here"));
    assert!(ran.record.timeline.samples.is_empty());
}

/// Review finding 5: one of the mixed phase's decoding streams fails at
/// once while the other decodes on. The phase used to wait for every
/// stream to end — here 65k tokens, over two minutes; at 262k twenty — before
/// it saw the failure. Now the first failure ends the repetition, and the
/// stream still decoding is dropped (its connection closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_mixed_stream_ends_the_repetition_at_once() {
    let params = fast(1, &[Phase::Mixed]);
    let cfg = Config {
        n_slots: 3,
        per_slot_ctx: 65_536,
        // The first request of a stream's length is the phase's unmeasured
        // rate measurement; the second is the first decoding stream.
        fail_prompt_len_nth: Some((params.stream_prompt_tokens as usize, 2, 500)),
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        params,
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let plan = ran.record.results.points.clone().unwrap();
    assert_eq!(plan.mixed.as_ref().map(|m| m.streams), Some(2), "{plan:?}");
    let SuiteEnd::Failed(msg) = &ran.end else {
        panic!("{:?}", ran.end)
    };
    assert!(
        msg.starts_with("mixed: ") && msg.contains("HTTP 500"),
        "{msg}"
    );
    assert!(ran.took < Duration::from_secs(10), "{:?}", ran.took);
    let errors = &ran.record.results.phase_errors;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].phase, Phase::Mixed);
    // The stream that was still decoding was dropped, not waited for.
    closed_early(&ran.fake, 1).await;
}

/// Decision 62, the first production run's failure: the measured request
/// right after a slot reset went out on a connection llama-server had
/// already closed, and failed without reaching a slot. The fake hangs up on
/// that request the same way — the 4th `/completion` is prefill 512's first
/// measured one, after the warm-up and the reset's two. It is sent again,
/// the point is measured all the same, and the resend is on the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_loses_its_connection_is_sent_again_and_recorded() {
    let cfg = Config {
        hang_up_completions: vec![4],
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(2, &[Phase::Prefill]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record.results;
    assert_eq!(ran.end, SuiteEnd::Done, "{:?}", r.phase_errors);
    assert!(r.complete);
    assert_eq!(ran.fake.seen.hung_up.load(Ordering::SeqCst), 1);
    assert_eq!(r.prefill.len(), 3);
    assert!(
        r.prefill.iter().all(|p| p.prompt_tok_s.values.len() == 2),
        "every repetition measured: {:?}",
        r.prefill
    );
    assert_eq!(r.retried.len(), 1, "{:?}", r.retried);
    let retried = &r.retried[0];
    assert_eq!(retried.stage, "prefill: 512 tokens, repetition 1/2");
    assert_eq!(retried.request, "POST /completion");
    assert!(
        retried.error.starts_with("error sending request for url (")
            && (retried
                .error
                .contains("connection closed before message completed")
                || retried.error.contains("reset")),
        "the whole cause chain: {}",
        retried.error
    );
    // Every stream said `Connection: close`: llama-server closes it anyway.
    let seen = &ran.fake.seen;
    let completions = seen.completions.lock().unwrap().len();
    assert_eq!(
        completions,
        1 + 3 * 2 * 3,
        "warm-up, then reset ×2 + measured"
    );
    assert_eq!(seen.completions_closing.load(Ordering::SeqCst), completions);
    // And the stored record says so: every phase's save carries it.
    let phases = ran.sink.phases.lock().unwrap();
    assert_eq!(phases.last().unwrap().1.results.retried, r.retried);
}

/// A request that loses its connection a second time is the phase's error,
/// naming both attempts; the first resend is still on the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_loses_its_connection_twice_fails_naming_both() {
    let cfg = Config {
        hang_up_completions: vec![4, 5],
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Prefill]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record.results;
    let SuiteEnd::Failed(msg) = &ran.end else {
        panic!("{:?}", ran.end)
    };
    assert!(
        msg.starts_with("prefill: POST /completion: error sending request")
            && msg.contains("(attempt 2 of 2; before it: error sending request"),
        "{msg}"
    );
    assert_eq!(r.phase_errors.len(), 1);
    assert!(r.prefill.is_empty());
    assert_eq!(ran.fake.seen.hung_up.load(Ordering::SeqCst), 2);
    assert_eq!(r.retried.len(), 1, "{:?}", r.retried);
    assert_eq!(r.retried[0].stage, "prefill: 512 tokens, repetition 1/1");
}

/// Review finding 3: a TTFT window of tens of milliseconds lies inside one
/// sampler tick, and its "energy" was the ticks' average power times its
/// length. A window shorter than two ticks (200 ms at the real 100 ms tick)
/// now gets no figure but a reason; one long enough is measured as before.
/// (The fake's TTFT carries some 30 ms of loopback overhead on top of its
/// prefill, hence the real tick here rather than the suite's 20 ms.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_the_sampler_cannot_resolve_has_a_reason_not_a_figure() {
    let cfg = Config {
        // 512 tokens: ≈ 51 ms (+ overhead); 4094: ≈ 409 ms.
        prefill_per_token: Duration::from_micros(100),
        // Decode windows: 31 × 10 ms ≈ 310 ms.
        token_delay: Duration::from_millis(10),
        ..Config::default()
    };
    let ran = run_ticked(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Prefill, Phase::Decode]),
        ScriptedGpu::new(100, true, 0),
        Cancel::none(),
        Duration::from_millis(100),
    )
    .await;
    assert_eq!(ran.end, SuiteEnd::Done);
    let r = &ran.record.results;
    let short = &r.prefill[0];
    assert_eq!(short.prompt_tokens, 512);
    assert_eq!(short.energy, None, "{short:?}");
    let why = short.energy_unmeasured.as_deref().unwrap();
    assert!(
        why.starts_with("too short to measure") && why.contains("200 ms or more"),
        "{why}"
    );
    let long = r.prefill.last().unwrap();
    assert_eq!(long.prompt_tokens, 4094);
    assert!(long.energy.is_some(), "{long:?}");
    assert_eq!(long.energy_unmeasured, None);
    for d in &r.decode {
        assert!(d.energy.is_some(), "{d:?}");
        assert_eq!(d.energy_unmeasured, None);
    }
}

/// Review finding 4: a row whose four slots share one KV pool (an auto
/// row), the pool small. The three decoding streams and an injection of
/// min(8192, P_max) did not fit together: the pool overflowed and
/// llama-server aborted every running request. Now the phase measures a
/// stream's rates first and injects what fits beside the streams, whose
/// `n_predict` is bounded to what they were counted with.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_pool_sizes_the_mixed_injection_to_fit() {
    let pool = 4096;
    let cfg = || Config {
        n_slots: 4,
        // Every slot reports the whole pool as its context (§2.1 fact 2).
        per_slot_ctx: pool,
        unified_pool: Some(pool),
        ..Config::default()
    };
    let unified = BenchTarget {
        kv_unified: Some(true),
        ..Default::default()
    };
    let ran = run(
        cfg(),
        unified,
        fast(1, &[Phase::Mixed]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record.results;
    assert_eq!(ran.end, SuiteEnd::Done, "{:?}", r.phase_errors);
    assert_eq!(ran.fake.seen.pool_aborts.load(Ordering::SeqCst), 0);
    let facts = r.server.as_ref().unwrap();
    assert_eq!(facts.kv_unified, Some(true));
    assert_eq!(facts.kv_unified_source.as_deref(), Some("settings"));
    let plan = r.points.as_ref().unwrap();
    let sized = plan.mixed.clone().unwrap();
    assert_eq!(sized.streams, 3);
    assert!(
        sized.inject_tokens > 0 && sized.inject_tokens < pool - 2,
        "{sized:?}"
    );
    let held = 3 * (sized.stream_prompt_tokens + sized.stream_predict) + sized.inject_tokens + 1;
    assert!(held <= pool, "{sized:?}: {held} cells");
    assert!(
        plan.notes
            .iter()
            .any(|n| n.starts_with("mixed:") && n.contains("pool of 4096")),
        "{:?}",
        plan.notes
    );
    let m = r.mixed.as_ref().unwrap();
    assert_eq!(m.inject_tokens, sized.inject_tokens);
    assert_eq!(m.stall_ms.values.len(), 1);
    assert!(ran.fake.seen.pool_peak.load(Ordering::SeqCst) <= pool);

    // The same server taken for split KV (the old arithmetic): the full
    // injection overflows the pool, and every running request is aborted.
    let ran = run(
        cfg(),
        BenchTarget::default(),
        fast(1, &[Phase::Mixed]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let SuiteEnd::Failed(msg) = &ran.end else {
        panic!("{:?}", ran.end)
    };
    assert!(msg.contains("Context size has been exceeded"), "{msg}");
    assert!(ran.fake.seen.pool_aborts.load(Ordering::SeqCst) >= 1);
}

/// Finding 4's other half: a slot too small to keep a stream decoding
/// through the steady window failed every repetition "ended during the
/// steady window". Now it is a planned skip, noted, and not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slot_too_small_for_the_steady_window_skips_mixed() {
    let mut params = fast(1, &[Phase::Mixed]);
    // 303 tokens beyond a 96-token prompt last ≈ 0.6 s at the fake's rate.
    params.mixed_steady_ms = 1500;
    let cfg = Config {
        per_slot_ctx: 400,
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        params,
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record.results;
    assert_eq!(ran.end, SuiteEnd::Done, "{:?}", r.phase_errors);
    assert!(r.phase_errors.is_empty());
    assert_eq!(r.phases_done, vec![Phase::Mixed]);
    assert!(r.mixed.is_none());
    let plan = r.points.as_ref().unwrap();
    assert!(plan.mixed.is_none());
    let note = plan
        .notes
        .iter()
        .find(|n| n.starts_with("mixed:"))
        .expect("a note on the skip");
    assert!(
        note.contains("skipped") && note.contains("1500 ms"),
        "{note}"
    );
}

/// One slot: no mixed phase (and the plan says why). The row says no
/// reasoning and no projector, the live template says no tools: those
/// probes are skipped with their reasons.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_slot_and_capability_skips() {
    let cfg = Config {
        n_slots: 1,
        supports_tools: false,
        ..Config::default()
    };
    let target = BenchTarget {
        reasoning: Some(false),
        tools: Some(true),
        projector: false,
        ..Default::default()
    };
    let ran = run(
        cfg,
        target,
        fast(1, &[Phase::Probes, Phase::Mixed]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record;
    assert_eq!(ran.end, SuiteEnd::Done);
    let plan = r.results.points.as_ref().unwrap();
    assert!(plan.mixed.is_none());
    assert!(
        plan.notes.iter().any(|n| n.starts_with("mixed:")),
        "{:?}",
        plan.notes
    );
    assert!(r.results.mixed.is_none());
    assert_eq!(r.results.phases_done, vec![Phase::Probes, Phase::Mixed]);
    for k in [
        ProbeKind::ThinkingOff,
        ProbeKind::ThinkingOn,
        ProbeKind::ToolCall,
        ProbeKind::Vision,
    ] {
        assert_eq!(outcome(r, k), ProbeOutcome::Skipped, "{k:?}");
    }
    let tool = r
        .probes
        .probes
        .iter()
        .find(|p| p.probe == ProbeKind::ToolCall)
        .unwrap();
    assert!(tool.detail.contains("chat template"), "{}", tool.detail);
    assert_eq!(outcome(r, ProbeKind::Needle), ProbeOutcome::Pass);
    // No thinking request was ever sent, no vision request either.
    let chats = ran.fake.seen.chats.lock().unwrap();
    assert!(chats
        .iter()
        .all(|b| b["chat_template_kwargs"]["enable_thinking"] == false));
    assert!(!chats.iter().any(|b| b.to_string().contains("image_url")));
}

/// Live findings (2026-09-29). With the probes deselected, prefill at 512 was
/// the fresh server's first request and read 12k/18.7k tok/s against 26k
/// warm: one unmeasured warm-up request now precedes the first measured
/// phase. And llama-server resets a slot's previous context before a new
/// task, outside the prompt timer (0.86 s after 250k tokens), so every
/// measured prefill repetition is preceded by one unmeasured request per
/// slot. Neither is counted anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmeasured_requests_warm_up_and_reset_the_slots() {
    let params = fast(2, &[Phase::Prefill]);
    let (warm_prompt, warm_generate) = (
        params.warmup_prompt_tokens as usize,
        u64::from(params.warmup_generate_tokens),
    );
    let ran = run(
        Config::default(),
        BenchTarget::default(),
        params,
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    assert_eq!(ran.end, SuiteEnd::Done);
    let bodies = ran.fake.seen.completions.lock().unwrap().clone();
    let unmeasured = |b: &serde_json::Value| {
        b["n_predict"].as_u64() == Some(warm_generate)
            && b["cache_prompt"] == false
            && b["prompt"].as_array().unwrap().len() == warm_prompt
    };
    assert!(
        unmeasured(&bodies[0]),
        "the warm-up comes first: {}",
        bodies[0]
    );
    let r = &ran.record.results;
    let plan = r.points.as_ref().unwrap();
    let slots = plan.n_slots as usize;
    // Then, per measured repetition: one reset per slot, the measured request.
    let rest = &bodies[1..];
    assert_eq!(rest.len(), plan.prefill.len() * 2 * (slots + 1));
    for rep in rest.chunks(slots + 1) {
        assert!(rep[..slots].iter().all(unmeasured), "{rep:?}");
        assert_eq!(rep[slots]["n_predict"], 1);
    }
    // Each reset draws its own corpus offset (after the BOS).
    let firsts: std::collections::HashSet<_> = rest
        .iter()
        .filter(|b| unmeasured(b))
        .map(|b| b["prompt"][1].clone())
        .collect();
    assert!(firsts.len() > slots, "{firsts:?}");
    assert!(r.prefill.iter().all(|p| p.prompt_tok_s.values.len() == 2));
}

/// Live finding (2026-09-29, decision 61): on ik_llama.cpp, whose slots share
/// one KV buffer, decode at depth 64 right after the prefill phase read 383
/// tok/s — about the deepest point's speed — against 448 without the prefill
/// phase before it: another slot still held the last prefill's 16k tokens.
/// So each depth's first repetition is preceded by one reset per slot, and
/// the later repetitions, which reuse the primed cache, by none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_decode_depth_starts_after_a_slot_reset() {
    let params = fast(2, &[Phase::Decode]);
    let (warm_prompt, warm_generate) = (
        params.warmup_prompt_tokens as usize,
        u64::from(params.warmup_generate_tokens),
    );
    let ran = run(
        Config::default(),
        BenchTarget::default(),
        params,
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    assert_eq!(ran.end, SuiteEnd::Done);
    let bodies = ran.fake.seen.completions.lock().unwrap().clone();
    let unmeasured = |b: &serde_json::Value| {
        b["n_predict"].as_u64() == Some(warm_generate)
            && b["cache_prompt"] == false
            && b["prompt"].as_array().unwrap().len() == warm_prompt
    };
    assert!(unmeasured(&bodies[0]), "the warm-up: {}", bodies[0]);
    let r = &ran.record.results;
    let plan = r.points.as_ref().unwrap();
    let slots = plan.n_slots as usize;
    // Per depth: one reset per slot, then both repetitions back to back
    // with the same cached prompt.
    let rest = &bodies[1..];
    assert_eq!(rest.len(), plan.decode.len() * (slots + 2));
    for (depth, chunk) in plan.decode.iter().zip(rest.chunks(slots + 2)) {
        assert!(chunk[..slots].iter().all(unmeasured), "{chunk:?}");
        let (a, b) = (&chunk[slots], &chunk[slots + 1]);
        assert_eq!(a["cache_prompt"], true);
        assert_eq!(a["prompt"].as_array().unwrap().len() as u64, *depth);
        assert_eq!(a, b, "the second repetition reuses the first's prompt");
    }
    assert!(r.decode.iter().all(|p| p.tok_s.values.len() == 2));
}

/// Speculative decoding's draft statistics are summed per point.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn draft_statistics_are_kept() {
    let cfg = Config {
        drafting: true,
        per_slot_ctx: 1024,
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(2, &[Phase::Decode]),
        ScriptedGpu::new(100, true, 0),
        Cancel::none(),
    )
    .await;
    assert_eq!(ran.end, SuiteEnd::Done);
    let r = &ran.record.results;
    assert_eq!(r.server.as_ref().unwrap().speculative, Some(true));
    assert_eq!(
        r.decode.iter().map(|p| p.depth).collect::<Vec<_>>(),
        vec![64, 991]
    );
    let d = r.decode[0].draft.as_ref().unwrap();
    assert_eq!((d.drafted, d.accepted), (64, 32));
    assert_eq!(d.acceptance, Some(0.5));
}

/// The `deterministic` probe sends its pair twice: once as a client would
/// (cache reuse allowed), once with `cache_prompt: false`. Live testing
/// (§2.1) found some rows differ only in the cached pair — reproducible
/// without prompt-cache reuse — which is `info`, not `fail`, and names why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deterministic_is_info_when_only_cache_reuse_differs() {
    let cfg = Config {
        chat_cache_nondeterministic: true,
        ..Config::default()
    };
    let ran = run(
        cfg,
        BenchTarget::default(),
        fast(1, &[Phase::Probes]),
        ScriptedGpu::new(50, true, 0),
        Cancel::none(),
    )
    .await;
    let r = &ran.record;
    assert_eq!(ran.end, SuiteEnd::Done);
    assert_eq!(outcome(r, ProbeKind::Deterministic), ProbeOutcome::Info);
    let p = r
        .probes
        .probes
        .iter()
        .find(|p| p.probe == ProbeKind::Deterministic)
        .unwrap();
    assert!(p.detail.contains("prompt-cache reuse"), "{}", p.detail);
    // Four requests judged: the cached pair, then the no-cache pair.
    assert_eq!(p.evidence.len(), 4);
    assert_eq!(
        p.evidence[0].content.as_deref(),
        Some("The sea is vast and blue.")
    );
    assert_eq!(
        p.evidence[1].content.as_deref(),
        Some("The sea is deep and full of mystery.")
    );
    assert_eq!(
        p.evidence[2].content.as_deref(),
        Some("The sea is vast and blue.")
    );
    assert_eq!(
        p.evidence[3].content.as_deref(),
        Some("The sea is vast and blue.")
    );
    // The fake actually saw a cache_prompt: false pair.
    let chats = ran.fake.seen.chats.lock().unwrap();
    let det: Vec<_> = chats
        .iter()
        .filter(|b| {
            b["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("sea")
        })
        .collect();
    assert_eq!(det.len(), 4, "{det:?}");
    assert_eq!(det[2]["cache_prompt"], false, "{det:?}");
    assert_eq!(det[3]["cache_prompt"], false, "{det:?}");
}

/// The engine against a real llama-server, with the real NVML. Start one,
/// then e.g.:
///
/// ```sh
/// LMGW_BENCH_LIVE_URL=http://127.0.0.1:18433 LMGW_BENCH_LIVE_REPS=1 \
///   cargo test -p lmgw-core --test it bench_engine::live -- --ignored --nocapture
/// ```
///
/// `LMGW_BENCH_LIVE_PROJECTOR=1` when it runs with `--mmproj`,
/// `LMGW_BENCH_LIVE_UNIFIED=1` (or `0`) when its slots share one KV pool
/// (`parallel` unset, or `--kv-unified`). Prints the
/// whole record as JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running llama-server at LMGW_BENCH_LIVE_URL"]
async fn live_engine_against_a_running_server() {
    let url = std::env::var("LMGW_BENCH_LIVE_URL").expect("LMGW_BENCH_LIVE_URL");
    let reps: u32 = std::env::var("LMGW_BENCH_LIVE_REPS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(1);
    let phases = std::env::var("LMGW_BENCH_LIVE_PHASES").unwrap_or_default();
    let projector = std::env::var("LMGW_BENCH_LIVE_PROJECTOR").is_ok_and(|v| v == "1");
    let probe = lmgw_core::vram::detect_probe();
    let sampler = Sampler::start(probe);
    let base_from = Instant::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let baseline = sampler.baseline(base_from, Instant::now()).await;
    let bench = Bench {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
        target: BenchTarget {
            base_url: url,
            reasoning: None,
            tools: None,
            projector,
            kv_unified: std::env::var("LMGW_BENCH_LIVE_UNIFIED")
                .ok()
                .map(|v| v == "1"),
        },
        params: SuiteParams::v1(reps, Phase::parse_list(&phases).unwrap()),
        sampler: &sampler,
        cancel: Cancel::none(),
    };
    let sink = Sink::default();
    let mut record = Record::default();
    record.results.energy.baseline_vram_bytes = baseline.vram_used_bytes;
    record.results.energy.idle_power_w = baseline.idle_power_w;
    let end = run_suite(&bench, &mut record, &sink).await;
    println!("{}", serde_json::to_string_pretty(&record).unwrap());
    println!("end: {end:?}");
    assert_eq!(end, SuiteEnd::Done);
}
