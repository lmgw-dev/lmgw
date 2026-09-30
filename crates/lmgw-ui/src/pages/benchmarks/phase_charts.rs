//! A run's phases as chart series (benchmark design §4.3, §8.2), for one run
//! (the detail) or several overlaid (Compare). One builder per chart, over
//! the stored points only — nothing here computes a number the engine did
//! not measure.
//!
//! One run: each chart's series are its readings (aggregate and per-stream,
//! before and during). Several: one colour per run ([`crate::charts::slot_color`],
//! in the order they were picked), the same reading in each.

use leptos::prelude::*;
use lmgw_api_types::bench::{Energy, Stat};
use lmgw_api_types::bench_ops::BenchRun;

use super::charts::{Axis, BarPanel, BarSeries, BarsCard, Pt, Series, XyCard};
use super::{fmt_ms, fmt_ms_axis, fmt_rate, fmt_tokj, tokens_short};

/// One run as the charts draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunLine {
    pub run: BenchRun,
    /// "#8 official-master b11226" — the legend's name for it.
    pub label: String,
    pub color: String,
}

const TOKENS: Axis = Axis {
    log: true,
    fmt: |v| tokens_short(v as u64),
    name: "tokens",
};
const DEPTH: Axis = Axis {
    log: true,
    fmt: |v| tokens_short(v as u64),
    name: "tokens of depth",
};
const STREAMS: Axis = Axis {
    log: true,
    fmt: |v| format!("{v:.0}"),
    name: "streams",
};
const RATE: Axis = Axis {
    log: false,
    fmt: fmt_rate,
    name: "tok/s",
};
const MS_LOG: Axis = Axis {
    log: true,
    fmt: fmt_ms_axis,
    name: "ms",
};

fn pt(x: f64, s: &Stat) -> Option<Pt> {
    (!s.values.is_empty() || s.median != 0.0).then(|| Pt::new(x, s.median, s.min, s.max))
}

fn one(lines: &[RunLine]) -> bool {
    lines.len() == 1
}

/// A series per run, from the points `read` finds in it.
fn per_run(lines: &[RunLine], single: &str, read: impl Fn(&BenchRun) -> Vec<Pt>) -> Vec<Series> {
    lines
        .iter()
        .map(|l| Series {
            label: if one(lines) {
                single.to_string()
            } else {
                l.label.clone()
            },
            color: l.color.clone(),
            pts: read(&l.run),
            faint: false,
        })
        .collect()
}

pub fn prefill_rate(lines: &[RunLine]) -> Vec<Series> {
    per_run(lines, "prefill", |r| {
        r.results
            .prefill
            .iter()
            .filter_map(|p| pt(p.prompt_tokens as f64, &p.prompt_tok_s))
            .collect()
    })
}

pub fn ttft(lines: &[RunLine]) -> Vec<Series> {
    per_run(lines, "time to first token", |r| {
        r.results
            .prefill
            .iter()
            .filter_map(|p| pt(p.prompt_tokens as f64, &p.ttft_ms))
            .collect()
    })
}

pub fn decode(lines: &[RunLine]) -> Vec<Series> {
    per_run(lines, "decode", |r| {
        r.results
            .decode
            .iter()
            .filter_map(|p| {
                let acc = p
                    .draft
                    .as_ref()
                    .and_then(|d| d.acceptance.map(|a| (a, d.accepted, d.drafted)))
                    .map(|(a, acc, dr)| {
                        format!("draft acceptance {:.0} % ({acc} of {dr})", a * 100.0)
                    });
                // A low share of distinct tokens is a degenerate loop, which
                // a drafter predicts perfectly.
                let distinct = p
                    .distinct_token_ratio
                    .as_ref()
                    .map(|d| format!("distinct tokens {:.0} %", d.median * 100.0));
                let note = [acc, distinct].into_iter().flatten().collect::<Vec<_>>();
                let note = (!note.is_empty()).then(|| note.join(" · "));
                pt(p.depth as f64, &p.tok_s).map(|q| q.with(note))
            })
            .collect()
    })
}

/// The aggregate, and each stream's own rate under it (faint).
pub fn concurrent(lines: &[RunLine]) -> Vec<Series> {
    let single = one(lines);
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let c = &l.run.results.concurrent;
        let agg: Vec<Pt> = c
            .iter()
            .filter_map(|p| pt(f64::from(p.streams), &p.aggregate_tok_s))
            .collect();
        let per: Vec<Pt> = c
            .iter()
            .filter_map(|p| pt(f64::from(p.streams), &p.per_stream_tok_s))
            .collect();
        out.push(Series {
            label: if single {
                "aggregate".into()
            } else {
                format!("{} aggregate", l.label)
            },
            color: l.color.clone(),
            pts: agg,
            faint: false,
        });
        out.push(Series {
            label: if single {
                "per stream".into()
            } else {
                format!("{} per stream", l.label)
            },
            // One run: a second colour, so the two readings are told apart
            // by more than weight. Several: the run's own colour, faint.
            color: if single {
                "var(--c3)".into()
            } else {
                lines[i].color.clone()
            },
            pts: per,
            faint: !single,
        });
    }
    out
}

fn stat3(s: &Stat) -> Option<(f64, f64, f64)> {
    (!s.values.is_empty() || s.median != 0.0).then_some((s.median, s.min, s.max))
}

/// The mixed phase (§4.3) as three panels: the decoding streams' rate before
/// and during the injected prefill, the injected prompt's TTFT alone and
/// amid decoding, and the stall.
pub fn mixed(lines: &[RunLine]) -> Vec<BarPanel> {
    type Read<'a> = &'a dyn Fn(&BenchRun) -> Vec<Option<(f64, f64, f64)>>;
    let series = |read: Read| {
        lines
            .iter()
            .map(|l| BarSeries {
                label: l.label.clone(),
                color: l.color.clone(),
                vals: read(&l.run),
                gaps: Vec::new(),
            })
            .collect::<Vec<_>>()
    };
    vec![
        BarPanel {
            title: "decoding streams, tok/s".into(),
            cats: vec!["before".into(), "during".into()],
            series: series(&|r| match &r.results.mixed {
                Some(m) => vec![stat3(&m.before_tok_s), stat3(&m.during_tok_s)],
                None => vec![None, None],
            }),
            fmt: fmt_rate,
        },
        BarPanel {
            title: "injected prompt, TTFT".into(),
            cats: vec!["alone".into(), "amid decoding".into()],
            series: series(&|r| match &r.results.mixed {
                Some(m) => vec![m.solo_ttft_ms.map(|v| (v, v, v)), stat3(&m.inject_ttft_ms)],
                None => vec![None, None],
            }),
            fmt: fmt_ms,
        },
        BarPanel {
            title: "stall".into(),
            cats: vec!["longest gap".into()],
            series: series(&|r| vec![r.results.mixed.as_ref().and_then(|m| stat3(&m.stall_ms))]),
            fmt: fmt_ms,
        },
    ]
}

fn tpj(e: Option<&Energy>) -> Option<(f64, f64, f64)> {
    let e = e?;
    let v = e.tokens_per_joule?;
    let each = &e.tokens_per_joule_each;
    Some(if each.values.is_empty() {
        (v, v, v)
    } else {
        (v, each.min.min(v), each.max.max(v))
    })
}

/// One point's energy as a bar: its figure, or why it has none.
type EnergyCell = (u64, Option<(f64, f64, f64)>, Option<String>);

/// Tokens per joule per phase (§4.3): one panel per phase, its categories
/// the union of the runs' points, each on its own scale — prefill's hundreds
/// of tokens per joule would flatten decode's handful. A point whose windows
/// were too short to measure (decision 56) keeps its category and shows the
/// reason instead of a bar.
pub fn energy(lines: &[RunLine]) -> Vec<BarPanel> {
    fn panel(
        lines: &[RunLine],
        title: &str,
        keys: impl Fn(&BenchRun) -> Vec<EnergyCell>,
        label: fn(u64) -> String,
    ) -> Option<BarPanel> {
        let mut xs: Vec<u64> = lines
            .iter()
            .flat_map(|l| {
                keys(&l.run)
                    .into_iter()
                    .filter(|(_, v, why)| v.is_some() || why.is_some())
                    .map(|(x, ..)| x)
            })
            .collect();
        xs.sort_unstable();
        xs.dedup();
        if xs.is_empty() {
            return None;
        }
        let series = lines
            .iter()
            .map(|l| {
                let got = keys(&l.run);
                let at = |x: &u64| got.iter().find(|(k, ..)| k == x);
                BarSeries {
                    label: l.label.clone(),
                    color: l.color.clone(),
                    vals: xs.iter().map(|x| at(x).and_then(|(_, v, _)| *v)).collect(),
                    gaps: xs
                        .iter()
                        .map(|x| at(x).and_then(|(_, _, why)| why.clone()))
                        .collect(),
                }
            })
            .collect();
        Some(BarPanel {
            title: title.to_string(),
            cats: xs.iter().map(|x| label(*x)).collect(),
            series,
            fmt: fmt_tokj,
        })
    }
    [
        panel(
            lines,
            "prefill, tok/J",
            |r| {
                r.results
                    .prefill
                    .iter()
                    .map(|p| {
                        (
                            p.prompt_tokens,
                            tpj(p.energy.as_ref()),
                            p.energy_unmeasured.clone(),
                        )
                    })
                    .collect()
            },
            tokens_short,
        ),
        panel(
            lines,
            "decode, tok/J",
            |r| {
                r.results
                    .decode
                    .iter()
                    .map(|p| (p.depth, tpj(p.energy.as_ref()), p.energy_unmeasured.clone()))
                    .collect()
            },
            tokens_short,
        ),
        panel(
            lines,
            "concurrent, tok/J",
            |r| {
                r.results
                    .concurrent
                    .iter()
                    .map(|p| {
                        (
                            u64::from(p.streams),
                            tpj(p.energy.as_ref()),
                            p.energy_unmeasured.clone(),
                        )
                    })
                    .collect()
            },
            |n| format!("{n}×"),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Every phase chart of the runs, in suite order; a phase no run measured
/// has no card.
#[component]
pub fn PhaseCharts(
    #[prop(into)] lines: Signal<Vec<RunLine>>,
    /// Figures under the mixed and energy cards (one run's detail only).
    #[prop(optional, into)]
    mixed_foot: Option<Signal<Vec<(String, String)>>>,
    #[prop(optional, into)] energy_foot: Option<Signal<Vec<(String, String)>>>,
) -> impl IntoView {
    let s = move |f: fn(&[RunLine]) -> Vec<Series>| Signal::derive(move || lines.with(|l| f(l)));
    let b = move |f: fn(&[RunLine]) -> Vec<BarPanel>| Signal::derive(move || lines.with(|l| f(l)));
    let mixed_foot = mixed_foot.unwrap_or_else(|| Signal::derive(Vec::new));
    let energy_foot = energy_foot.unwrap_or_else(|| Signal::derive(Vec::new));
    view! {
        <div class="bn-charts">
            <XyCard
                title="Prefill"
                note="prompt tokens per second by prompt length (server-side timing), median with the min–max band over the repetitions; the KV cache is kept out of it"
                series=s(prefill_rate)
                x=TOKENS
                y=RATE
            />
            <XyCard
                title="Time to first token"
                note="request sent → first streamed token, measured by lmgw on the same requests; both axes logarithmic"
                series=s(ttft)
                x=TOKENS
                y=MS_LOG
            />
            <XyCard
                title="Decode"
                note="generation speed after a prompt of that depth (256 tokens generated), median with the min–max band; the tooltip has the draft acceptance when the row speculates, and the share of distinct tokens generated where the engine streams token ids (a low one is a degenerate loop)"
                series=s(decode)
                x=DEPTH
                y=RATE
            />
            <XyCard
                title="Concurrent streams"
                note="N streams released together: the aggregate is every generated token over their shared window; per stream is the median of each stream's own rate"
                series=s(concurrent)
                x=STREAMS
                y=RATE
            />
            <BarsCard
                title="Mixed load"
                note="all slots but one decode steadily, then one long prompt arrives: what the decoding streams lose while it is prefilled, what the prompt pays, and the longest gap between two tokens of any stream"
                panels=b(mixed)
                foot=mixed_foot
            />
            <BarsCard
                title="Tokens per joule"
                note="tokens per joule of the whole card, idle draw included, per point; each phase on its own scale; whiskers: the spread of the measured windows. A point whose windows are shorter than the GPU sampler resolves (two of its ticks) says so instead of showing a bar"
                panels=b(energy)
                foot=energy_foot
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::bench::PrefillPoint;

    /// Decision 56: a prefill point whose TTFT windows were too short to
    /// measure keeps its category and carries the reason, instead of
    /// vanishing from the energy chart.
    #[test]
    fn a_point_too_short_to_measure_keeps_its_place_with_the_reason() {
        let mut run = BenchRun::default();
        run.results.prefill = vec![
            PrefillPoint {
                prompt_tokens: 512,
                energy_unmeasured: Some("too short to measure: 15 ms".into()),
                ..Default::default()
            },
            PrefillPoint {
                prompt_tokens: 32_768,
                energy: Some(Energy {
                    tokens_per_joule: Some(120.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        let lines = [RunLine {
            run,
            label: "run 1".into(),
            color: "red".into(),
        }];
        let panels = energy(&lines);
        assert_eq!(panels.len(), 1, "only prefill has anything");
        let p = &panels[0];
        assert_eq!(p.cats, vec!["512", "32k"]);
        let s = &p.series[0];
        assert_eq!(s.vals, vec![None, Some((120.0, 120.0, 120.0))]);
        assert_eq!(s.gaps[0].as_deref(), Some("too short to measure: 15 ms"));
        assert_eq!(s.gaps[1], None);
    }
}
