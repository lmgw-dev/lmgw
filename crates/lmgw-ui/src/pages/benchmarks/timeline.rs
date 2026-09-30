//! The run's timeline (benchmark design §4.3, §8.2): what the sampler stored
//! every 500 ms — power, device VRAM and the hottest temperature — as three
//! panels on one time axis, under the phase bands, with the moments the card
//! was held back (a [`THROTTLE_MASK`] clock event) marked in red.
//!
//! The table twin is per phase (how long, mean and peak power, peak VRAM and
//! temperature) rather than hundreds of samples: that is what the timeline is
//! read for.

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::bench::{Phase, Timeline, TimelineSample, CLOCK_EVENT_REASONS, THROTTLE_MASK};
use wasm_bindgen::JsCast;

use crate::charts::{
    line_path, line_runs, nice_ticks, use_element_size, use_tips, Plot, Tip, TipRow,
};
use crate::fmt::human_bytes;

use super::super::usage::{chart_or_table, ChartHead};
use super::phase_label;

/// One panel: a reading of every sample, its colour, its unit.
struct Lane {
    name: &'static str,
    color: &'static str,
    read: fn(&TimelineSample) -> Option<f64>,
    fmt: fn(f64) -> String,
}

const LANES: [Lane; 3] = [
    Lane {
        name: "Power",
        color: "var(--c2)",
        read: |s| s.power_w,
        fmt: |v| format!("{v:.0} W"),
    },
    Lane {
        name: "VRAM",
        color: "var(--c1)",
        read: |s| s.vram_used_bytes.map(|b| b as f64),
        fmt: |v| human_bytes(v.max(0.0) as u64),
    },
    Lane {
        name: "Temperature",
        color: "var(--c6)",
        read: |s| s.temp_c.map(f64::from),
        fmt: |v| format!("{v:.0} °C"),
    },
];

const LANE_H: f64 = 74.0;
const LANE_GAP: f64 = 14.0;

/// The throttling reasons a sample's clock events name.
pub fn throttle_reasons(bits: u64) -> Vec<&'static str> {
    CLOCK_EVENT_REASONS
        .iter()
        .filter(|(b, _, throttles)| *throttles && bits & b != 0)
        .map(|(_, n, _)| *n)
        .collect()
}

/// The phase a moment falls in.
fn phase_at(t: &Timeline, ms: u64) -> Option<Phase> {
    t.phases
        .iter()
        .find(|p| ms >= p.start_ms && p.end_ms.is_none_or(|e| ms < e))
        .map(|p| p.phase)
}

fn secs(ms: f64) -> String {
    let s = ms / 1000.0;
    if s < 120.0 {
        format!("{s:.0} s")
    } else {
        format!("{}m {:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    }
}

fn draw(w: f64, t: &Timeline, hover: RwSignal<Option<usize>>) -> AnyView {
    let tips = use_tips();
    let lanes_h = LANE_H * 3.0 + LANE_GAP * 2.0;
    let p = Plot::new(w, lanes_h + 18.0 + 26.0, 58.0, 16.0, 18.0, 26.0);
    let end = t
        .samples
        .last()
        .map(|s| s.t_ms)
        .into_iter()
        .chain(t.phases.iter().filter_map(|p| p.end_ms))
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let px = move |ms: f64| p.l + ms / end * p.iw();
    let bands: Vec<AnyView> = t
        .phases
        .iter()
        .enumerate()
        .map(|(i, ph)| {
            let x0 = px(ph.start_ms as f64);
            let x1 = px(ph.end_ms.map_or(end, |e| e as f64));
            let label = phase_label(ph.phase);
            view! {
                <rect
                    x=x0
                    y=p.t
                    width=(x1 - x0).max(1.0)
                    height=lanes_h
                    class=if i % 2 == 0 { "bn-band" } else { "bn-band alt" }
                ></rect>
                <text x=x0 + 4.0 y=p.t - 5.0 class="ax bn-band-l">
                    {((x1 - x0) > 34.0).then_some(label)}
                </text>
            }
            .into_any()
        })
        .collect();
    let mut lanes: Vec<AnyView> = Vec::new();
    for (li, lane) in LANES.iter().enumerate() {
        let top = p.t + li as f64 * (LANE_H + LANE_GAP);
        let vals: Vec<Option<f64>> = t.samples.iter().map(lane.read).collect();
        let max = vals.iter().flatten().fold(0.0_f64, |a, b| a.max(*b));
        let ticks = nice_ticks(max.max(1e-9) * 1.05, 2);
        let tmax = ticks.last().copied().unwrap_or(max).max(max).max(1e-9);
        let yy = move |v: f64| top + LANE_H - v / tmax * LANE_H;
        let grid: Vec<AnyView> = ticks
            .iter()
            .map(|&v| {
                let y = yy(v);
                view! {
                    <line x1=p.l x2=p.right() y1=y y2=y class="gridline"></line>
                    <text x=p.l - 8.0 y=y + 3.5 class="ax" text-anchor="end">{(lane.fmt)(v)}</text>
                }
                .into_any()
            })
            .collect();
        let pts: Vec<Option<(f64, f64)>> = t
            .samples
            .iter()
            .zip(&vals)
            .map(|(s, v)| v.map(|v| (px(s.t_ms as f64), yy(v))))
            .collect();
        let paths: Vec<AnyView> = line_runs(&pts)
            .into_iter()
            .map(|run| {
                view! {
                    <path
                        d=line_path(&run)
                        fill="none"
                        stroke=lane.color
                        stroke-width="1.5"
                        stroke-linejoin="round"
                    ></path>
                }
                .into_any()
            })
            .collect();
        lanes.push(
            view! {
                <text x=p.l + 4.0 y=top + 11.0 class="axl bn-lane-l">{lane.name}</text>
                {grid}
                {paths}
            }
            .into_any(),
        );
    }
    // Held back: a red tick under the power lane for every sample whose
    // clock events name a throttling reason.
    let throttled: Vec<AnyView> = t
        .samples
        .iter()
        .filter(|s| s.clock_events.is_some_and(|b| b & THROTTLE_MASK != 0))
        .map(|s| {
            let x = px(s.t_ms as f64);
            view! {
                <line x1=x x2=x y1=p.t + LANE_H - 5.0 y2=p.t + LANE_H class="bn-throttle"></line>
            }
            .into_any()
        })
        .collect();
    let ticks = nice_ticks(end / 1000.0, 6);
    let xlabels: Vec<AnyView> = ticks
        .iter()
        .map(|&s| {
            let x = px(s * 1000.0);
            view! { <text x=x y=p.h - 8.0 class="ax" text-anchor="middle">{secs(s * 1000.0)}</text> }
                .into_any()
        })
        .collect();
    let samples = StoredValue::new(t.samples.clone());
    let tl = StoredValue::new(t.clone());
    let cross = move || {
        hover
            .get()
            .and_then(|i| samples.with_value(|s| s.get(i).map(|s| s.t_ms)))
            .map(|ms| {
                let x = px(ms as f64);
                view! { <line x1=x x2=x y1=p.t y2=p.t + lanes_h class="crosshair"></line> }
            })
    };
    let on_move = move |ev: web_sys::MouseEvent| {
        let Some(el) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let r = el.get_bounding_client_rect();
        // The rect covers the plot only, and the svg is drawn at 1:1.
        let ms = ((ev.client_x() as f64 - r.left()) / r.width().max(1.0)) * end;
        let i = samples.with_value(|s| {
            s.iter()
                .enumerate()
                .min_by_key(|(_, s)| (s.t_ms as f64 - ms).abs() as u64)
                .map(|(i, _)| i)
        });
        let Some(i) = i else { return };
        hover.set(Some(i));
        let tip = samples.with_value(|all| {
            let s = &all[i];
            let mut rows: Vec<TipRow> = LANES
                .iter()
                .map(|l| {
                    TipRow::new(
                        l.color,
                        l.name,
                        (l.read)(s).map(l.fmt).unwrap_or_else(|| "—".into()),
                    )
                })
                .collect();
            let why = throttle_reasons(s.clock_events.unwrap_or(0));
            if !why.is_empty() {
                rows.push(TipRow::new("var(--err)", "held back", why.join(", ")));
            }
            let phase = tl
                .with_value(|t| phase_at(t, s.t_ms))
                .map(phase_label)
                .unwrap_or("between phases");
            Tip::new(format!("{} · {phase}", secs(s.t_ms as f64)), rows)
        });
        tips.show(&ev, tip);
    };
    view! {
        <svg width=p.w height=p.h viewBox=p.view_box()>
            {bands}
            {lanes}
            {throttled}
            {xlabels}
            {cross}
            <rect
                x=p.l
                y=p.t
                width=p.iw()
                height=lanes_h
                fill="transparent"
                on:mousemove=on_move
                on:mouseleave=move |_| {
                    hover.set(None);
                    tips.hide();
                }
            ></rect>
        </svg>
    }
    .into_any()
}

/// Per phase: how long, the mean and peak power, the peak VRAM and
/// temperature, and whether the card was held back in it.
fn per_phase(t: &Timeline) -> (Vec<String>, Vec<Vec<String>>) {
    let heads = [
        "Phase",
        "Duration",
        "Mean power",
        "Peak power",
        "Peak VRAM",
        "Peak temperature",
        "Held back",
    ]
    .map(String::from)
    .to_vec();
    let rows = t
        .phases
        .iter()
        .map(|ph| {
            let end = ph
                .end_ms
                .unwrap_or_else(|| t.samples.last().map_or(ph.start_ms, |s| s.t_ms));
            let inside: Vec<&TimelineSample> = t
                .samples
                .iter()
                .filter(|s| s.t_ms >= ph.start_ms && s.t_ms < end.max(ph.start_ms + 1))
                .collect();
            let power: Vec<f64> = inside.iter().filter_map(|s| s.power_w).collect();
            let mean = (!power.is_empty()).then(|| power.iter().sum::<f64>() / power.len() as f64);
            let peak = power.iter().copied().reduce(f64::max);
            let vram = inside.iter().filter_map(|s| s.vram_used_bytes).max();
            let temp = inside.iter().filter_map(|s| s.temp_c).max();
            let bits = inside
                .iter()
                .filter_map(|s| s.clock_events)
                .fold(0, |a, b| a | b);
            let why = throttle_reasons(bits);
            vec![
                phase_label(ph.phase).to_string(),
                secs(end.saturating_sub(ph.start_ms) as f64),
                mean.map_or("—".into(), |v| format!("{v:.0} W")),
                peak.map_or("—".into(), |v| format!("{v:.0} W")),
                vram.map_or("—".into(), human_bytes),
                temp.map_or("—".into(), |v| format!("{v} °C")),
                if why.is_empty() {
                    "no".into()
                } else {
                    why.join(", ")
                },
            ]
        })
        .collect();
    (heads, rows)
}

#[component]
pub fn TimelineCard(#[prop(into)] timeline: Signal<Option<Timeline>>) -> impl IntoView {
    let node: NodeRef<html::Div> = NodeRef::new();
    let size = use_element_size(node);
    let width = Memo::new(move |_| size.get().0);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    let empty = move || timeline.with(|t| t.as_ref().is_none_or(|t| t.samples.is_empty()));
    view! {
        <div class="card chart-card bn-chart bn-wide" class:bn-none=empty>
            <ChartHead
                title="Timeline"
                note="power, device VRAM and the hottest temperature every 500 ms, under the phases · red ticks: the card was held back (power cap, thermal or hardware slowdown), so the numbers there are throttled numbers"
                tv=tv
            />
            <div class="chart-box" node_ref=node>
                {move || {
                    let w = width.get();
                    let Some(t) = timeline.get() else { return ().into_any() };
                    if w < 160.0 || t.samples.is_empty() {
                        return ().into_any();
                    }
                    let (heads, rows) = per_phase(&t);
                    chart_or_table(tv, move || draw(w, &t, hover), heads, rows)
                }}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::bench::PhaseSpan;

    #[test]
    fn only_limiting_reasons_count_as_held_back() {
        assert_eq!(throttle_reasons(0x1 | 0x4), ["sw_power_cap"]);
        assert!(throttle_reasons(0x1 | 0x2 | 0x10 | 0x100).is_empty());
        assert_eq!(throttle_reasons(0x20 | 0x40).len(), 2);
    }

    #[test]
    fn a_moment_falls_in_its_phase_and_an_open_phase_runs_on() {
        let t = Timeline {
            interval_ms: 500,
            samples: vec![],
            phases: vec![
                PhaseSpan {
                    phase: Phase::Load,
                    start_ms: 100,
                    end_ms: Some(900),
                },
                PhaseSpan {
                    phase: Phase::Prefill,
                    start_ms: 900,
                    end_ms: None,
                },
            ],
        };
        assert_eq!(phase_at(&t, 50), None);
        assert_eq!(phase_at(&t, 100), Some(Phase::Load));
        assert_eq!(phase_at(&t, 900), Some(Phase::Prefill));
        assert_eq!(phase_at(&t, 99_999), Some(Phase::Prefill));
    }
}
