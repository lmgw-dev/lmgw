//! A run against another (benchmark design §6): the per-metric delta table
//! with its noise and verdict, the probe outcomes side by side, the headline
//! tiles, and the threshold control every verdict on the page is judged at.
//!
//! The verdicts are the ops' own (`bench_run`'s comparison) on the detail,
//! and `lmgw_api_types::bench_compare::compare` on Compare — one rule.

use leptos::prelude::*;
use lmgw_api_types::bench::ProbeOutcome;
use lmgw_api_types::bench_ops::{BenchComparison, Headline, MetricDelta, Verdict};

use super::probes::{outcome_chip, probe_label};
use super::{fmt_delta, fmt_unit, parse_threshold, use_bn};

/// The threshold every verdict on the page is judged at, in percent. Kept
/// across visits; a value that is not a percentage is marked and the
/// default is used meanwhile.
#[component]
pub fn ThresholdControl() -> impl IntoView {
    let bn = use_bn();
    let bad = move || parse_threshold(&bn.threshold_text.get()).is_none();
    view! {
        <label
            class="bn-thr"
            title="A metric is a regression or an improvement when it moves by more than this, or by more than its own measured noise (the spread of its repetitions), whichever is larger"
        >
            "Threshold"
            <input
                class="input w-num"
                class:bn-bad=bad
                type="number"
                min="0"
                step="0.5"
                data-untracked
                prop:value=move || bn.threshold_text.get()
                on:input=move |ev| bn.threshold_text.set(event_target_value(&ev))
            />
            "%"
            <Show when=bad>
                <span class="bn-thr-bad">{move || format!("not a percentage; judging at {} %", bn.threshold.get())}</span>
            </Show>
        </label>
    }
}

/// `(class, words)` of a verdict.
pub fn verdict_look(v: Verdict) -> (&'static str, &'static str) {
    match v {
        Verdict::Regression => ("chip err", "regression"),
        Verdict::Improvement => ("chip ok", "improvement"),
        Verdict::Same => ("chip off", "same"),
        Verdict::Missing => ("dim", "—"),
        Verdict::NotSamePoint => ("chip off", "other point"),
    }
}

pub fn verdict_chip(v: Verdict, title: String) -> AnyView {
    let (class, words) = verdict_look(v);
    if v == Verdict::Missing {
        return view! { <span class="dim" title=title>"—"</span> }.into_any();
    }
    view! {
        <span class=format!("{class} bn-verdict") title=title>
            <span class="dot"></span>
            {words}
        </span>
    }
    .into_any()
}

fn delta_class(v: Verdict) -> &'static str {
    match v {
        Verdict::Regression => "num bn-worse",
        Verdict::Improvement => "num bn-better",
        _ => "num",
    }
}

fn value(unit: &str, v: Option<f64>) -> String {
    v.map(|v| fmt_unit(unit, v)).unwrap_or_else(|| "—".into())
}

/// One metric's line.
#[component]
fn MetricRow(m: MetricDelta) -> impl IntoView {
    let tip = format!(
        "{}: {} is better. The band is ±{:.1} %, the larger of the threshold and the noise (the wider relative spread of the two points' repetitions).",
        m.label,
        if m.lower_is_better { "lower" } else { "higher" },
        m.band_pct
    );
    let missing_tip = match m.verdict {
        Verdict::Missing => "one of the two runs did not measure it".to_string(),
        Verdict::NotSamePoint => format!(
            "{}. The two runs measured this at different points (one stopped early, say), so the change is shown but judged neither way and not counted.",
            m.note.as_deref().unwrap_or("measured at different points")
        ),
        _ => tip.clone(),
    };
    view! {
        <tr>
            <td class="wrap">
                <span title=tip>{m.label.clone()}</span>
                {m.note.clone().map(|n| view! { <span class="dim bn-note">{n}</span> })}
            </td>
            <td class="num">{value(&m.unit, m.old)}</td>
            <td class="num">{value(&m.unit, m.new)}</td>
            <td class=delta_class(m.verdict)>
                {m.delta_pct.map(fmt_delta).unwrap_or_else(|| "—".into())}
            </td>
            <td class="num bn-a" title=format!("band ±{:.1} %", m.band_pct)>
                {if m.verdict == Verdict::Missing { "—".to_string() } else { format!("{:.1} %", m.noise_pct) }}
            </td>
            <td>{verdict_chip(m.verdict, missing_tip)}</td>
        </tr>
    }
}

/// The headline metrics of `cmp`, base against this, and the probes that
/// gave a verdict in either.
#[component]
pub fn DeltaTable(
    cmp: BenchComparison,
    /// "run 8" — the base's column head.
    base: String,
    /// "run 9".
    this: String,
) -> impl IntoView {
    // The probes that changed (or that one of the two did not run); the
    // unchanged ones are counted, not listed.
    let probes: Vec<_> = cmp
        .probes
        .iter()
        .filter(|p| p.verdict != Verdict::Same)
        .cloned()
        .collect();
    let unchanged = cmp.probes.len() - probes.len();
    let rows = cmp
        .metrics
        .clone()
        .into_iter()
        .map(|m| view! { <MetricRow m=m/> })
        .collect_view();
    let probe_rows = probes
        .into_iter()
        .map(|p| {
            let o = |o: Option<ProbeOutcome>| match o {
                Some(o) => outcome_chip(o, String::new()),
                None => view! { <span class="dim">"not run"</span> }.into_any(),
            };
            view! {
                <tr>
                    <td>{probe_label(p.probe)}</td>
                    <td class="bn-r">{o(p.old)}</td>
                    <td class="bn-r">{o(p.new)}</td>
                    <td class="bn-a"></td>
                    <td class="bn-a"></td>
                    <td>{verdict_chip(p.verdict, "a pass that became a fail or an error is a regression".into())}</td>
                </tr>
            }
        })
        .collect_view();
    let has_probes = !cmp.probes.is_empty();
    let probes_head = match unchanged {
        0 => "Probes".to_string(),
        n if n == cmp.probes.len() => format!("Probes: all {n} gave the same outcome in both"),
        n => format!("Probes: {n} more gave the same outcome in both"),
    };
    view! {
        <div class="table-scroll bn-delta-wrap">
            <table class="data bk-sub bn-delta">
                <thead>
                    <tr>
                        <th>"Metric"</th>
                        <th class="num-h">{base.clone()}</th>
                        <th class="num-h">{this.clone()}</th>
                        <th class="num-h">"Change"</th>
                        <th class="num-h bn-a" title="The wider relative spread (max − min) / median of the two points">
                            "Noise"
                        </th>
                        <th>"Verdict"</th>
                    </tr>
                </thead>
                <tbody>
                    {rows}
                    {has_probes
                        .then(|| {
                            view! {
                                <tr class="group">
                                    <td colspan="6">{probes_head.clone()}</td>
                                </tr>
                            }
                        })}
                    {probe_rows}
                </tbody>
            </table>
        </div>
    }
}

/// "3 regressions · 1 improvement", or that nothing moved — and how many
/// metrics were measured at other points, which count for neither.
pub fn tally(cmp: &BenchComparison) -> String {
    let mut v = match (cmp.regressions, cmp.improvements) {
        (0, 0) => vec!["nothing moved beyond the band".to_string()],
        (r, i) => {
            let mut v = Vec::new();
            if r > 0 {
                v.push(crate::fmt::count_of(r as usize, "regressions"));
            }
            if i > 0 {
                v.push(crate::fmt::count_of(i as usize, "improvements"));
            }
            v
        }
    };
    let other = cmp
        .metrics
        .iter()
        .filter(|m| m.verdict == Verdict::NotSamePoint)
        .count();
    if other > 0 {
        v.push(format!("{other} not at the same point"));
    }
    v.join(" · ")
}

/// "12 202 tok/s" → ("12 202", "tok/s"): the figure large, its unit small.
fn split_unit(s: &str) -> (String, String) {
    match s.rsplit_once(' ') {
        Some((n, u)) if !u.chars().any(|c| c.is_ascii_digit()) => (n.to_string(), u.to_string()),
        _ => (s.to_string(), String::new()),
    }
}

/// The headline numbers as tiles, each with its change against the previous
/// comparable run when there is one.
#[component]
pub fn HeadlineTiles(h: Headline, cmp: Option<BenchComparison>) -> impl IntoView {
    let find = |key: &str| {
        cmp.as_ref()
            .and_then(|c| c.metrics.iter().find(|m| m.metric == key).cloned())
    };
    let prefill_at = h
        .prefill_tokens
        .map(|t| format!("prefill at {t}"))
        .unwrap_or_else(|| "prefill".into());
    let ttft_at = h
        .prefill_tokens
        .map(|t| format!("TTFT at {t}"))
        .unwrap_or_else(|| "TTFT".into());
    let deep_at = h
        .decode_deep_depth
        .map(|d| format!("decode at {}", super::tokens_short(d)))
        .unwrap_or_else(|| "decode, deepest".into());
    let agg_at = h
        .aggregate_streams
        .map(|n| format!("aggregate, {n} streams"))
        .unwrap_or_else(|| "aggregate".into());
    let tiles: Vec<(String, &str, Option<f64>, &str)> = vec![
        (prefill_at, "tok/s", h.prefill_tok_s, "prefill_tok_s"),
        (ttft_at, "ms", h.ttft_ms, "ttft_ms"),
        (
            "decode at 64".into(),
            "tok/s",
            h.decode_tok_s,
            "decode_tok_s",
        ),
        (deep_at, "tok/s", h.decode_deep_tok_s, "decode_deep_tok_s"),
        (agg_at, "tok/s", h.aggregate_tok_s, "aggregate_tok_s"),
        (
            "decode tok/J at 64".into(),
            "tok/J",
            h.decode_tokens_per_joule,
            "decode_tokens_per_joule",
        ),
        ("load".into(), "ms", h.load_ms.map(|v| v as f64), "load_ms"),
        (
            "VRAM after load".into(),
            "bytes",
            h.load_vram_bytes.map(|v| v as f64),
            "load_vram_bytes",
        ),
        ("mixed stall".into(), "ms", h.stall_ms, "stall_ms"),
    ];
    let tiles = tiles
        .into_iter()
        .filter(|(_, _, v, _)| v.is_some())
        .map(|(label, unit, v, key)| {
            let d = find(key);
            let (cls, delta) = match &d {
                Some(m) if m.delta_pct.is_some() => {
                    let c = match m.verdict {
                        Verdict::Regression => "bn-tile-d bn-worse",
                        Verdict::Improvement => "bn-tile-d bn-better",
                        // Another point than the previous run's: no verdict,
                        // so no colour, and marked as not like for like.
                        Verdict::NotSamePoint => "bn-tile-d dim",
                        _ => "bn-tile-d",
                    };
                    let words = m.delta_pct.map(fmt_delta).map(|d| {
                        if m.verdict == Verdict::NotSamePoint {
                            format!("{d} ≠")
                        } else {
                            d
                        }
                    });
                    (c, words)
                }
                _ => ("bn-tile-d", None),
            };
            let title = d
                .as_ref()
                .and_then(|m| {
                    let prev = m
                        .old
                        .map(|o| format!("previous comparable run: {}", fmt_unit(unit, o)))?;
                    Some(match (&m.note, m.verdict) {
                        (Some(n), Verdict::NotSamePoint) => format!("{prev}\n{n}"),
                        _ => prev,
                    })
                })
                .unwrap_or_default();
            let (num, unit_text) = v
                .map(|v| split_unit(&fmt_unit(unit, v)))
                .unwrap_or_default();
            view! {
                <div class="tile bn-tile" title=title>
                    <div class="tile-value">{num}<span class="bn-unit">{unit_text}</span></div>
                    <div class="tile-label">
                        {label}
                        {delta.map(|d| view! { <span class=cls>{d}</span> })}
                    </div>
                </div>
            }
        })
        .collect_view();
    view! { <div class="auto-grid fit tile-row bn-tiles">{tiles}</div> }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A metric measured at two different points (a run that stopped early)
    /// is named in the tally but counts for neither side (decision 53).
    #[test]
    fn the_tally_names_metrics_at_other_points_and_counts_them_for_neither() {
        let m = |verdict| MetricDelta {
            verdict,
            ..Default::default()
        };
        let mut c = BenchComparison {
            metrics: vec![m(Verdict::Same), m(Verdict::NotSamePoint)],
            ..Default::default()
        };
        assert_eq!(
            tally(&c),
            "nothing moved beyond the band · 1 not at the same point"
        );
        c.regressions = 1;
        c.metrics.push(m(Verdict::NotSamePoint));
        assert_eq!(tally(&c), "1 regression · 2 not at the same point");
        c.metrics.clear();
        c.regressions = 0;
        assert_eq!(tally(&c), "nothing moved beyond the band");
        assert_eq!(verdict_look(Verdict::NotSamePoint).1, "other point");
    }
}
