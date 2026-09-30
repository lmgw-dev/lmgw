//! Runs side by side (benchmark design §6): which runs are comparable, the
//! previous comparable run, the headline numbers, and the regression rule.
//! Pure — every function takes stored runs and returns a verdict — so the
//! whole rule is unit-tested here.
//!
//! In `lmgw-api-types` rather than the engine, like [`crate::image_lab`]'s
//! builder: both sides have to reach the same verdict. The ops answer with it
//! (`bench_runs`' badges, `bench_run`'s comparison with the previous
//! comparable run), and the Benchmarks page's Compare view judges runs the
//! owner picked against each other with the very same rule, not a copy of it.
//!
//! **The rule, per headline metric:** *delta* = (new − old) / old; *noise* =
//! the larger relative spread, (max − min) / median, of the two points; a
//! **regression** when *delta* < −max(threshold, noise), an **improvement**
//! when *delta* > +max(threshold, noise). "Lower is better" metrics (TTFT,
//! load time, VRAM, stall) flip the sign. The threshold is the caller's
//! (`threshold_pct`, default 5 %) and is echoed in every answer.
//!
//! **Only the same point is judged** (§13 decision 53). A headline reads the
//! nearest prompt length, the deepest depth and the largest stream count a
//! run measured, so a run that stopped early reads another point than a
//! complete one — decode at 16k against decode at 130k. Such a metric keeps
//! its delta as information, with a note naming both points, and its verdict
//! is [`Verdict::NotSamePoint`]: no regression, no improvement, not counted.

use crate::bench::{ProbeKind, ProbeOutcome, RunResults, Stat};
use crate::bench_ops::{
    BenchComparison, BenchRun, BenchStatus, Headline, MetricDelta, ProbeChange, Verdict,
};

/// The prefill length the headline reads (§6): 2048, or the nearest measured.
pub const HEADLINE_PREFILL_TOKENS: u64 = 2048;

/// The decode depth the headline reads: 64, the suite's first depth.
pub const HEADLINE_DECODE_DEPTH: u64 = 64;

/// Why `a` and `b` are not comparable (§6: same model file — path and size —,
/// settings hash, suite version and GPU name; the build may differ). Empty
/// when they are.
pub fn not_comparable(a: &BenchRun, b: &BenchRun) -> Vec<String> {
    let mut why = Vec::new();
    for r in [a, b] {
        if !r.unreadable.is_empty() {
            why.push(format!(
                "run {}'s stored data could not be read ({})",
                r.id,
                r.unreadable.join("; ")
            ));
        }
    }
    if a.model.gguf_path != b.model.gguf_path || a.model.gguf_size != b.model.gguf_size {
        why.push(format!(
            "different model files ({} of {} bytes vs {} of {} bytes)",
            a.model.gguf_path, a.model.gguf_size, b.model.gguf_path, b.model.gguf_size
        ));
    }
    if a.settings_hash != b.settings_hash {
        why.push("different settings (the settings hash differs)".into());
    }
    if a.suite_version != b.suite_version {
        why.push(format!(
            "different suite versions ({} vs {})",
            a.suite_version, b.suite_version
        ));
    }
    if a.gpu.name != b.gpu.name {
        why.push(format!(
            "different GPUs ({} vs {})",
            a.gpu.name.as_deref().unwrap_or("unknown"),
            b.gpu.name.as_deref().unwrap_or("unknown")
        ));
    }
    why
}

/// The previous comparable run (§6): the latest `done` run before `run` (by
/// id, which is creation order) that [`not_comparable`] has nothing against.
pub fn previous_comparable<'a>(run: &BenchRun, candidates: &'a [BenchRun]) -> Option<&'a BenchRun> {
    candidates
        .iter()
        .filter(|c| c.id < run.id && c.status == BenchStatus::Done)
        .filter(|c| not_comparable(run, c).is_empty())
        .max_by_key(|c| c.id)
}

/// One headline point: a median with its spread, and where it was measured.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    median: f64,
    min: f64,
    max: f64,
    /// The prompt length, depth or stream count it was measured at.
    at: Option<u64>,
}

impl Point {
    fn of(s: &Stat, at: Option<u64>) -> Option<Self> {
        (!s.values.is_empty() || s.median != 0.0).then_some(Self {
            median: s.median,
            min: s.min,
            max: s.max,
            at,
        })
    }

    fn single(v: f64) -> Self {
        Self {
            median: v,
            min: v,
            max: v,
            at: None,
        }
    }

    /// (max − min) / median; 0 for a single value or a zero median.
    fn spread(&self) -> f64 {
        if self.median.abs() > f64::EPSILON {
            ((self.max - self.min) / self.median).abs()
        } else {
            0.0
        }
    }
}

/// One headline metric: its key, label, unit, direction, and how to read it.
struct Metric {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    lower_is_better: bool,
    read: fn(&RunResults) -> Option<Point>,
    /// What `at` counts, for the note when the two runs differ.
    at_unit: &'static str,
}

fn nearest<T>(items: &[T], key: impl Fn(&T) -> u64, target: u64) -> Option<&T> {
    items.iter().min_by_key(|i| key(i).abs_diff(target))
}

fn prefill_point(r: &RunResults) -> Option<&crate::bench::PrefillPoint> {
    nearest(&r.prefill, |p| p.prompt_tokens, HEADLINE_PREFILL_TOKENS)
}

fn decode_point(r: &RunResults) -> Option<&crate::bench::DecodePoint> {
    nearest(&r.decode, |p| p.depth, HEADLINE_DECODE_DEPTH)
}

const METRICS: &[Metric] = &[
    Metric {
        key: "prefill_tok_s",
        label: "Prefill at 2048 tokens",
        unit: "tok/s",
        lower_is_better: false,
        read: |r| prefill_point(r).and_then(|p| Point::of(&p.prompt_tok_s, Some(p.prompt_tokens))),
        at_unit: "tokens",
    },
    Metric {
        key: "ttft_ms",
        label: "Time to first token at 2048 tokens",
        unit: "ms",
        lower_is_better: true,
        read: |r| prefill_point(r).and_then(|p| Point::of(&p.ttft_ms, Some(p.prompt_tokens))),
        at_unit: "tokens",
    },
    Metric {
        key: "decode_tok_s",
        label: "Decode at depth 64",
        unit: "tok/s",
        lower_is_better: false,
        read: |r| decode_point(r).and_then(|p| Point::of(&p.tok_s, Some(p.depth))),
        at_unit: "tokens of depth",
    },
    Metric {
        key: "decode_deep_tok_s",
        label: "Decode at the deepest point",
        unit: "tok/s",
        lower_is_better: false,
        read: |r| {
            r.decode
                .last()
                .and_then(|p| Point::of(&p.tok_s, Some(p.depth)))
        },
        at_unit: "tokens of depth",
    },
    Metric {
        key: "aggregate_tok_s",
        label: "Aggregate decode at all slots",
        unit: "tok/s",
        lower_is_better: false,
        read: |r| {
            r.concurrent
                .last()
                .and_then(|p| Point::of(&p.aggregate_tok_s, Some(u64::from(p.streams))))
        },
        at_unit: "streams",
    },
    Metric {
        key: "decode_tokens_per_joule",
        label: "Decode tokens per joule at depth 64",
        unit: "tok/J",
        lower_is_better: false,
        read: |r| {
            decode_point(r).and_then(|p| {
                let e = p.energy.as_ref()?;
                let tpj = e.tokens_per_joule?;
                // The figure is the whole point's; its noise is how far the
                // windows' own figures spread (a run stored before they were
                // kept has none: no noise, as before).
                let each = &e.tokens_per_joule_each;
                let (min, max) = if each.values.is_empty() {
                    (tpj, tpj)
                } else {
                    (each.min, each.max)
                };
                Some(Point {
                    median: tpj,
                    min,
                    max,
                    at: Some(p.depth),
                })
            })
        },
        at_unit: "tokens of depth",
    },
    Metric {
        key: "load_ms",
        label: "Load time",
        unit: "ms",
        lower_is_better: true,
        read: |r| r.load.as_ref().map(|l| Point::single(l.ms as f64)),
        at_unit: "",
    },
    Metric {
        key: "load_vram_bytes",
        label: "VRAM after load",
        unit: "bytes",
        lower_is_better: true,
        read: |r| {
            r.load
                .as_ref()
                .and_then(|l| l.vram_bytes)
                .map(|b| Point::single(b as f64))
        },
        at_unit: "",
    },
    Metric {
        key: "stall_ms",
        label: "Mixed-phase stall",
        unit: "ms",
        lower_is_better: true,
        // At its injection's size: on a shared KV pool that is sized from
        // the rates measured in the run (decision 57), so two runs may
        // inject different lengths.
        read: |r| {
            r.mixed
                .as_ref()
                .and_then(|m| Point::of(&m.stall_ms, Some(m.inject_tokens)))
        },
        at_unit: "injected tokens",
    },
];

/// The headline numbers of one run (§6), for the runs table.
pub fn headline(r: &RunResults) -> Headline {
    let prefill = prefill_point(r);
    let decode = decode_point(r);
    let deep = r.decode.last();
    let agg = r.concurrent.last();
    let has = |s: &Stat| Point::of(s, None).map(|p| p.median);
    Headline {
        prefill_tok_s: prefill.and_then(|p| has(&p.prompt_tok_s)),
        prefill_tokens: prefill.map(|p| p.prompt_tokens),
        ttft_ms: prefill.and_then(|p| has(&p.ttft_ms)),
        decode_tok_s: decode.and_then(|p| has(&p.tok_s)),
        decode_deep_tok_s: deep.and_then(|p| has(&p.tok_s)),
        decode_deep_depth: deep.map(|p| p.depth),
        aggregate_tok_s: agg.and_then(|p| has(&p.aggregate_tok_s)),
        aggregate_streams: agg.map(|p| p.streams),
        decode_tokens_per_joule: decode
            .and_then(|p| p.energy.as_ref())
            .and_then(|e| e.tokens_per_joule),
        load_ms: r.load.as_ref().map(|l| l.ms),
        load_vram_bytes: r.load.as_ref().and_then(|l| l.vram_bytes),
        stall_ms: r.mixed.as_ref().and_then(|m| has(&m.stall_ms)),
    }
}

/// The regression rule on two medians (module doc). Returns the delta in
/// percent (raw, not flipped), the band in percent, and the verdict.
pub fn judge(
    old: f64,
    new: f64,
    noise_pct: f64,
    threshold_pct: f64,
    lower_is_better: bool,
) -> (Option<f64>, f64, Verdict) {
    let band = threshold_pct.max(noise_pct);
    if old.abs() <= f64::EPSILON {
        return (None, band, Verdict::Missing);
    }
    let delta = (new - old) / old * 100.0;
    let better = if lower_is_better { -delta } else { delta };
    let verdict = if better < -band {
        Verdict::Regression
    } else if better > band {
        Verdict::Improvement
    } else {
        Verdict::Same
    };
    (Some(delta), band, verdict)
}

fn metric_delta(m: &Metric, old: &RunResults, new: &RunResults, threshold_pct: f64) -> MetricDelta {
    let (o, n) = ((m.read)(old), (m.read)(new));
    let mut d = MetricDelta {
        metric: m.key.into(),
        label: m.label.into(),
        unit: m.unit.into(),
        lower_is_better: m.lower_is_better,
        old: o.map(|p| p.median),
        new: n.map(|p| p.median),
        band_pct: threshold_pct,
        ..Default::default()
    };
    let (Some(o), Some(n)) = (o, n) else {
        d.verdict = Verdict::Missing;
        return d;
    };
    d.noise_pct = o.spread().max(n.spread()) * 100.0;
    let (delta, band, verdict) = judge(
        o.median,
        n.median,
        d.noise_pct,
        threshold_pct,
        m.lower_is_better,
    );
    d.delta_pct = delta;
    d.band_pct = band;
    d.verdict = verdict;
    if o.median.abs() <= f64::EPSILON {
        d.note = Some("the earlier run measured 0, so there is no relative change".into());
    } else if o.at != n.at {
        // Two different points: the change is information, not a verdict
        // (decision 53) — a run that stopped at 2 of 4 streams is not 45 %
        // slower than one that reached 4.
        d.verdict = Verdict::NotSamePoint;
        d.note = Some(format!(
            "measured at {} vs {} {} — not the same point, so no verdict",
            o.at.unwrap_or(0),
            n.at.unwrap_or(0),
            m.at_unit
        ));
    }
    d
}

fn is_failure(o: ProbeOutcome) -> bool {
    matches!(o, ProbeOutcome::Fail | ProbeOutcome::Error)
}

/// Probe outcomes side by side (§6): a pass that became a fail or an error
/// is a regression, the reverse an improvement; `info` and `skipped` never
/// judge.
fn probe_changes(old: &BenchRun, new: &BenchRun) -> Vec<ProbeChange> {
    let find = |r: &BenchRun, k: ProbeKind| {
        r.probes
            .probes
            .iter()
            .find(|p| p.probe == k)
            .map(|p| p.outcome)
    };
    ProbeKind::ALL
        .into_iter()
        .filter_map(|k| {
            let (o, n) = (find(old, k), find(new, k));
            if o.is_none() && n.is_none() {
                return None;
            }
            let verdict = match (o, n) {
                (Some(o), Some(n)) if o == ProbeOutcome::Pass && is_failure(n) => {
                    Verdict::Regression
                }
                (Some(o), Some(n)) if is_failure(o) && n == ProbeOutcome::Pass => {
                    Verdict::Improvement
                }
                (Some(_), Some(_)) => Verdict::Same,
                _ => Verdict::Missing,
            };
            Some(ProbeChange {
                probe: k,
                old: o,
                new: n,
                verdict,
            })
        })
        .collect()
}

/// `new` against `old` (§6).
pub fn compare(new: &BenchRun, old: &BenchRun, threshold_pct: f64) -> BenchComparison {
    let why = not_comparable(new, old);
    let metrics: Vec<MetricDelta> = METRICS
        .iter()
        .map(|m| metric_delta(m, &old.results, &new.results, threshold_pct))
        .collect();
    let probes = probe_changes(old, new);
    let count = |v: Verdict| {
        (metrics.iter().filter(|m| m.verdict == v).count()
            + probes.iter().filter(|p| p.verdict == v).count()) as u32
    };
    BenchComparison {
        base_run_id: old.id,
        base_created_at: old.created_at.clone(),
        comparable: why.is_empty(),
        not_comparable: why,
        threshold_pct,
        regressions: count(Verdict::Regression),
        improvements: count(Verdict::Improvement),
        metrics,
        probes,
    }
}

/// The threshold a caller asked for, or the default — refused when it is
/// not a usable percentage.
pub fn threshold(asked: Option<f64>) -> Result<f64, String> {
    match asked {
        None => Ok(crate::bench_ops::DEFAULT_THRESHOLD_PCT),
        Some(t) if t.is_finite() && t >= 0.0 => Ok(t),
        Some(t) => Err(format!(
            "threshold_pct must be a percentage of 0 or more, not {t}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{
        ConcurrentPoint, DecodePoint, Energy, LoadResult, MixedResult, PrefillPoint, ProbeReport,
        ProbeResult,
    };

    fn stat(median: f64, min: f64, max: f64) -> Stat {
        Stat {
            median,
            min,
            max,
            values: vec![min, median, max],
        }
    }

    fn results(prefill: f64, ttft: f64, decode: f64, spread: f64) -> RunResults {
        RunResults {
            prefill: vec![
                PrefillPoint {
                    prompt_tokens: 512,
                    prompt_tok_s: stat(1.0, 1.0, 1.0),
                    ..Default::default()
                },
                PrefillPoint {
                    prompt_tokens: 2048,
                    prompt_tok_s: stat(prefill, prefill, prefill),
                    ttft_ms: stat(ttft, ttft, ttft),
                    ..Default::default()
                },
            ],
            decode: vec![DecodePoint {
                depth: 64,
                tok_s: stat(decode, decode * (1.0 - spread), decode * (1.0 + spread)),
                energy: Some(Energy {
                    tokens_per_joule: Some(2.0),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            concurrent: vec![ConcurrentPoint {
                streams: 2,
                aggregate_tok_s: stat(800.0, 800.0, 800.0),
                ..Default::default()
            }],
            mixed: Some(MixedResult {
                stall_ms: stat(50.0, 50.0, 50.0),
                ..Default::default()
            }),
            load: Some(LoadResult {
                ms: 4000,
                vram_used_bytes: Some(3 << 30),
                vram_bytes: Some(1 << 30),
            }),
            ..Default::default()
        }
    }

    fn run(id: i64, r: RunResults) -> BenchRun {
        let mut run = BenchRun {
            id,
            status: BenchStatus::Done,
            settings_hash: "h".into(),
            suite_version: 1,
            results: r,
            ..Default::default()
        };
        run.model.gguf_path = "m.gguf".into();
        run.model.gguf_size = 10;
        run.gpu.name = Some("RTX 4090".into());
        run
    }

    fn metric<'a>(c: &'a BenchComparison, key: &str) -> &'a MetricDelta {
        c.metrics.iter().find(|m| m.metric == key).unwrap()
    }

    /// A run whose stored columns did not decode compares with nothing
    /// (review finding 7): its numbers read as empty.
    #[test]
    fn an_unreadable_run_is_comparable_with_nothing() {
        let a = run(1, RunResults::default());
        let mut b = run(2, RunResults::default());
        assert!(not_comparable(&a, &b).is_empty());
        b.unreadable = vec!["results: expected value".into()];
        let why = not_comparable(&a, &b);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(
            why[0].contains("run 2") && why[0].contains("results"),
            "{why:?}"
        );
        assert!(previous_comparable(&b, std::slice::from_ref(&a)).is_none());
        assert!(previous_comparable(&run(3, RunResults::default()), &[b]).is_none());
    }

    #[test]
    fn the_rule_uses_the_threshold_and_flips_lower_is_better() {
        // 10 % faster decode: an improvement at 5 %, the same at 15 %.
        assert_eq!(judge(100.0, 110.0, 0.0, 5.0, false).2, Verdict::Improvement);
        assert_eq!(judge(100.0, 110.0, 0.0, 15.0, false).2, Verdict::Same);
        assert_eq!(judge(100.0, 90.0, 0.0, 5.0, false).2, Verdict::Regression);
        // 10 % more TTFT is worse.
        let (delta, band, v) = judge(100.0, 110.0, 0.0, 5.0, true);
        assert_eq!(v, Verdict::Regression);
        assert!((delta.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(band, 5.0);
        assert_eq!(judge(100.0, 90.0, 0.0, 5.0, true).2, Verdict::Improvement);
        // Exactly at the band is not past it.
        assert_eq!(judge(100.0, 95.0, 0.0, 5.0, false).2, Verdict::Same);
        // A zero baseline has no relative change.
        assert_eq!(judge(0.0, 5.0, 0.0, 5.0, false).2, Verdict::Missing);
    }

    #[test]
    fn the_noise_band_widens_the_threshold() {
        // Decode 8 % slower, but either point spreads by 20 %: within noise.
        let old = run(1, results(1000.0, 100.0, 500.0, 0.10));
        let new = run(2, results(1000.0, 100.0, 460.0, 0.0));
        let c = compare(&new, &old, 5.0);
        let d = metric(&c, "decode_tok_s");
        assert!((d.noise_pct - 20.0).abs() < 1e-6, "{d:?}");
        assert!((d.band_pct - 20.0).abs() < 1e-6);
        assert_eq!(d.verdict, Verdict::Same);
        // Without the noise the same drop is a regression.
        let quiet = run(1, results(1000.0, 100.0, 500.0, 0.0));
        let c = compare(&new, &quiet, 5.0);
        assert_eq!(metric(&c, "decode_tok_s").verdict, Verdict::Regression);
        // The one decode point is both depth 64 and the deepest.
        assert_eq!(metric(&c, "decode_deep_tok_s").verdict, Verdict::Regression);
        assert_eq!(c.regressions, 2);
        assert!(c.comparable);
    }

    /// Live (2026-09-29): decode at depth 64 read 2.47 and 3.01 tok/J in two
    /// runs of identical settings — its half-second windows fall where the
    /// card's clocks ramp up after an idle moment. The windows' own spread is
    /// that figure's noise, as a throughput's repetitions are.
    #[test]
    fn tokens_per_joule_is_judged_against_its_windows_spread() {
        let with = |tpj: f64, each: Option<(f64, f64)>| {
            let mut r = results(1000.0, 100.0, 500.0, 0.0);
            let e = r.decode[0].energy.as_mut().unwrap();
            e.tokens_per_joule = Some(tpj);
            if let Some((lo, hi)) = each {
                e.tokens_per_joule_each = stat(tpj, lo, hi);
            }
            r
        };
        let old = run(1, with(2.47, Some((2.2, 2.7))));
        let new = run(2, with(3.01, Some((2.6, 3.4))));
        let d = metric(&compare(&new, &old, 5.0), "decode_tokens_per_joule").clone();
        assert!((d.noise_pct - 0.8 / 3.01 * 100.0).abs() < 1e-6, "{d:?}");
        assert_eq!(d.verdict, Verdict::Same, "{d:?}");
        // Runs stored without the windows' figures: no noise, as before.
        let (old, new) = (run(1, with(2.47, None)), run(2, with(3.01, None)));
        let d = metric(&compare(&new, &old, 5.0), "decode_tokens_per_joule").clone();
        assert_eq!(d.noise_pct, 0.0);
        assert_eq!(d.verdict, Verdict::Improvement);
    }

    #[test]
    fn the_headline_reads_2048_or_the_nearest_length() {
        let r = results(35_000.0, 60.0, 520.0, 0.0);
        let h = headline(&r);
        assert_eq!(h.prefill_tokens, Some(2048));
        assert_eq!(h.prefill_tok_s, Some(35_000.0));
        assert_eq!(h.decode_tok_s, Some(520.0));
        assert_eq!(h.aggregate_streams, Some(2));
        assert_eq!(h.decode_tokens_per_joule, Some(2.0));
        assert_eq!(h.load_vram_bytes, Some(1 << 30));
        let mut short = r.clone();
        short.prefill.retain(|p| p.prompt_tokens == 512);
        assert_eq!(headline(&short).prefill_tokens, Some(512));
        assert_eq!(headline(&RunResults::default()), Headline::default());
    }

    #[test]
    fn a_metric_one_run_lacks_is_missing_and_lengths_that_differ_are_noted() {
        let old = run(1, results(1000.0, 100.0, 500.0, 0.0));
        let mut r = results(1000.0, 100.0, 500.0, 0.0);
        r.mixed = None;
        r.prefill.retain(|p| p.prompt_tokens == 512);
        let new = run(2, r);
        let c = compare(&new, &old, 5.0);
        assert_eq!(metric(&c, "stall_ms").verdict, Verdict::Missing);
        let p = metric(&c, "prefill_tok_s");
        assert_eq!(
            p.note.as_deref(),
            Some("measured at 2048 vs 512 tokens — not the same point, so no verdict")
        );
        // 1000 → 1 tok/s is kept as information, not judged.
        assert_eq!(p.verdict, Verdict::NotSamePoint);
        assert!(p.delta_pct.unwrap() < -99.0, "{p:?}");
        assert_eq!(c.regressions, 0, "{c:?}");
        // A stall after another injection size is another point too.
        let mut r = results(1000.0, 100.0, 500.0, 0.0);
        r.mixed.as_mut().unwrap().inject_tokens = 4096;
        let mut base = results(1000.0, 100.0, 500.0, 0.0);
        base.mixed.as_mut().unwrap().inject_tokens = 8192;
        let c = compare(&run(2, r), &run(1, base), 5.0);
        let s = metric(&c, "stall_ms");
        assert_eq!(s.verdict, Verdict::NotSamePoint, "{s:?}");
        assert!(s
            .note
            .as_deref()
            .unwrap()
            .contains("8192 vs 4096 injected tokens"));
    }

    /// Review finding 1: a canceled run stopped at 2 of 4 streams and at
    /// depth 16k compared with a complete one. Before, "aggregate at all
    /// slots" read 2 against 4 streams as a regression, and decode at the
    /// deepest point 16k against 130k as a +100 % improvement — counted in
    /// the tally and coloured on the tiles.
    #[test]
    fn points_that_differ_are_information_not_verdicts() {
        let full = {
            let mut r = results(1000.0, 100.0, 500.0, 0.0);
            r.decode.push(DecodePoint {
                depth: 130_815,
                tok_s: stat(200.0, 200.0, 200.0),
                ..Default::default()
            });
            r.concurrent = [(1, 500.0), (2, 800.0), (4, 1450.0)]
                .map(|(n, v)| ConcurrentPoint {
                    streams: n,
                    aggregate_tok_s: stat(v, v, v),
                    ..Default::default()
                })
                .to_vec();
            r
        };
        let mut canceled = full.clone();
        canceled.decode[1] = DecodePoint {
            depth: 16_384,
            tok_s: stat(400.0, 400.0, 400.0),
            ..Default::default()
        };
        canceled.concurrent.truncate(2);
        let c = compare(&run(2, canceled), &run(1, full), 5.0);
        let agg = metric(&c, "aggregate_tok_s");
        assert_eq!(agg.verdict, Verdict::NotSamePoint, "{agg:?}");
        assert!((agg.delta_pct.unwrap() + 44.83).abs() < 0.01, "{agg:?}");
        assert!(agg.note.as_deref().unwrap().contains("4 vs 2 streams"));
        let deep = metric(&c, "decode_deep_tok_s");
        assert_eq!(deep.verdict, Verdict::NotSamePoint, "{deep:?}");
        assert!((deep.delta_pct.unwrap() - 100.0).abs() < 1e-9);
        // Every other headline is at the same point and unchanged.
        assert_eq!((c.regressions, c.improvements), (0, 0), "{c:?}");
        assert_eq!(metric(&c, "decode_tok_s").verdict, Verdict::Same);
        assert_eq!(
            serde_json::to_value(Verdict::NotSamePoint).unwrap(),
            "not_same_point"
        );
    }

    #[test]
    fn a_pass_that_turns_into_a_fail_is_a_regression() {
        let probe = |k, o| ProbeResult {
            probe: k,
            outcome: o,
            ..Default::default()
        };
        let mut old = run(1, RunResults::default());
        old.probes = ProbeReport {
            probes: vec![
                probe(ProbeKind::Chat, ProbeOutcome::Pass),
                probe(ProbeKind::ToolCall, ProbeOutcome::Fail),
                probe(ProbeKind::ReasoningHistory, ProbeOutcome::Info),
            ],
        };
        let mut new = run(2, RunResults::default());
        new.probes = ProbeReport {
            probes: vec![
                probe(ProbeKind::Chat, ProbeOutcome::Error),
                probe(ProbeKind::ToolCall, ProbeOutcome::Pass),
                probe(ProbeKind::ReasoningHistory, ProbeOutcome::Info),
            ],
        };
        let c = compare(&new, &old, 5.0);
        let v = |k| c.probes.iter().find(|p| p.probe == k).unwrap().verdict;
        assert_eq!(v(ProbeKind::Chat), Verdict::Regression);
        assert_eq!(v(ProbeKind::ToolCall), Verdict::Improvement);
        assert_eq!(v(ProbeKind::ReasoningHistory), Verdict::Same);
        assert_eq!(c.probes.len(), 3, "probes neither run ran are left out");
    }

    #[test]
    fn the_previous_comparable_run_is_the_latest_done_one_with_the_key() {
        let base = results(1.0, 1.0, 1.0, 0.0);
        let mut runs = vec![
            run(1, base.clone()),
            run(2, base.clone()),
            run(3, base.clone()),
            run(4, base.clone()),
            run(5, base.clone()),
        ];
        runs[1].status = BenchStatus::Failed;
        runs[2].settings_hash = "other".into();
        runs[3].build.image_ref = "another build".into();
        let this = run(6, base.clone());
        // 4 differs only in its build, which comparable runs may.
        assert_eq!(previous_comparable(&this, &runs).map(|r| r.id), Some(5));
        runs[4].gpu.name = Some("RTX 3090".into());
        assert_eq!(previous_comparable(&this, &runs).map(|r| r.id), Some(4));
        // Never a later run.
        assert_eq!(previous_comparable(&runs[0], &runs).map(|r| r.id), None);
        let why = not_comparable(&this, &runs[2]);
        assert_eq!(why, vec!["different settings (the settings hash differs)"]);
    }

    #[test]
    fn a_threshold_must_be_a_percentage() {
        assert_eq!(threshold(None), Ok(5.0));
        assert_eq!(threshold(Some(12.5)), Ok(12.5));
        assert!(threshold(Some(-1.0)).is_err());
        assert!(threshold(Some(f64::NAN)).is_err());
    }
}
