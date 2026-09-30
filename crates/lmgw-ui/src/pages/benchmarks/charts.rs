//! The Benchmarks page's chart cards (benchmark design §8.2), drawn with the
//! shared SVG toolkit in [`crate::charts`] and the Usage page's card
//! furniture (a head with its note, and the Chart | Table twin, so no value is
//! only reachable by pointer).
//!
//! Three shapes cover every chart the page has:
//!
//! * [`XyCard`] — lines over a measured x (prompt length, depth, streams),
//!   log2 when the points are powers of two apart, with a min–max band per
//!   series. Tick labels are the real values the points were measured at.
//! * [`BarsCard`] — grouped bars: categories along x, one bar per series,
//!   with a min–max whisker.
//! * [`TimelineCard`] (in `timeline.rs`) — the sampler's power, VRAM and
//!   temperature over time, under the phase bands.

use leptos::html;
use leptos::prelude::*;

use crate::charts::{
    legend, line_path, nice_ticks, use_element_size, use_tips, LegendItem, Plot, Tip, TipRow, Tips,
};

use super::super::usage::{chart_or_table, ChartHead};

/// One measured point: the median and its spread over the repetitions.
#[derive(Clone, Debug, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
    pub lo: f64,
    pub hi: f64,
    /// More about the point for its tooltip ("draft acceptance 85 %").
    pub extra: Option<String>,
}

impl Pt {
    pub fn new(x: f64, y: f64, lo: f64, hi: f64) -> Self {
        Self {
            x,
            y,
            lo,
            hi,
            extra: None,
        }
    }

    pub fn with(mut self, extra: Option<String>) -> Self {
        self.extra = extra;
        self
    }
}

/// One line on an [`XyCard`].
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub label: String,
    pub color: String,
    pub pts: Vec<Pt>,
    /// Drawn thinner and lighter: a secondary reading of the same run (the
    /// per-stream rate under the aggregate).
    pub faint: bool,
}

/// How an axis maps and labels its values.
#[derive(Clone, Copy)]
pub struct Axis {
    /// log2 for x, log10 for y.
    pub log: bool,
    pub fmt: fn(f64) -> String,
    /// What the values count, for the tooltip's head ("tokens").
    pub name: &'static str,
}

/// Where a value lands, and the tick values an axis shows.
#[derive(Clone, Copy)]
struct Scale {
    log: bool,
    lo: f64,
    hi: f64,
    from: f64,
    to: f64,
}

impl Scale {
    fn t(&self, v: f64) -> f64 {
        if self.log {
            v.max(1e-12).log10()
        } else {
            v
        }
    }

    /// `from` is the pixel of `lo`, `to` the pixel of `hi`.
    fn px(&self, v: f64) -> f64 {
        let (a, b) = (self.t(self.lo), self.t(self.hi));
        if (b - a).abs() < 1e-12 {
            return (self.from + self.to) / 2.0;
        }
        self.from + (self.t(v) - a) / (b - a) * (self.to - self.from)
    }
}

/// Log-y ticks: the axis runs from the 1-2-5 value at or below the lowest
/// point to the one at or above the highest, with every 1-2-5 value between
/// as a tick — only the powers of ten (and the two ends) once there are more
/// than six.
fn log_ticks(lo: f64, hi: f64) -> (f64, f64, Vec<f64>) {
    let lo = lo.max(1e-9);
    let floor125 = |v: f64| {
        let b = 10f64.powf(v.log10().floor());
        let m = v / b;
        b * if m >= 5.0 - 1e-9 {
            5.0
        } else if m >= 2.0 - 1e-9 {
            2.0
        } else {
            1.0
        }
    };
    let ceil125 = |v: f64| {
        let b = 10f64.powf(v.log10().floor());
        let m = v / b;
        b * if m <= 1.0 + 1e-9 {
            1.0
        } else if m <= 2.0 + 1e-9 {
            2.0
        } else if m <= 5.0 + 1e-9 {
            5.0
        } else {
            10.0
        }
    };
    let a = floor125(lo);
    let z = ceil125(hi.max(a * 1.5));
    let mut ticks = Vec::new();
    let mut e = a.log10().floor() as i32;
    while 10f64.powi(e) <= z * 1.001 {
        for m in [1.0, 2.0, 5.0] {
            let v = m * 10f64.powi(e);
            if v >= a * 0.999 && v <= z * 1.001 {
                ticks.push(v);
            }
        }
        e += 1;
    }
    if ticks.len() > 6 {
        let decade = |v: f64| (v.log10() - v.log10().round()).abs() < 1e-9;
        ticks.retain(|v| decade(*v) || (*v - a).abs() < 1e-9 * a || (*v - z).abs() < 1e-9 * z);
    }
    (a, z, ticks)
}

/// Label positions that do not collide: keeps the last one, then walks back
/// dropping any closer than `gap` pixels to the one kept after it.
pub fn spaced(px: &[f64], gap: f64) -> Vec<bool> {
    let mut keep = vec![false; px.len()];
    let mut last: Option<f64> = None;
    for i in (0..px.len()).rev() {
        if last.is_none_or(|l| (l - px[i]).abs() >= gap) {
            keep[i] = true;
            last = Some(px[i]);
        }
    }
    keep
}

/// The chart height for a card's width: shorter than Usage's, since these
/// cards sit two or three to a row.
pub fn card_height(w: f64) -> f64 {
    (w * 0.42).clamp(170.0, 280.0)
}

/// The shared tooltip and a hover index, one per chart.
fn hit_columns(
    p: &Plot,
    xs: &[f64],
    tips_data: Vec<Tip>,
    ctl: Tips,
    hover: RwSignal<Option<usize>>,
) -> Vec<AnyView> {
    let n = xs.len();
    tips_data
        .into_iter()
        .enumerate()
        .map(|(i, tip)| {
            let left = if i == 0 {
                p.l
            } else {
                (xs[i - 1] + xs[i]) / 2.0
            };
            let right = if i + 1 == n {
                p.right()
            } else {
                (xs[i] + xs[i + 1]) / 2.0
            };
            view! {
                <rect
                    x=left
                    y=p.t
                    width=(right - left).max(1.0)
                    height=p.ih()
                    fill="transparent"
                    on:mousemove=move |ev: web_sys::MouseEvent| {
                        hover.set(Some(i));
                        ctl.show(&ev, tip.clone());
                    }
                    on:mouseleave=move |_| {
                        hover.set(None);
                        ctl.hide();
                    }
                ></rect>
            }
            .into_any()
        })
        .collect()
}

fn range_text(fmt: fn(f64) -> String, pt: &Pt) -> String {
    if (pt.hi - pt.lo).abs() > f64::EPSILON * pt.y.abs().max(1.0) {
        format!("{} ({}–{})", fmt(pt.y), fmt(pt.lo), fmt(pt.hi))
    } else {
        fmt(pt.y)
    }
}

/// Every distinct x any series has, ascending.
fn all_xs(series: &[Series]) -> Vec<f64> {
    let mut xs: Vec<f64> = series
        .iter()
        .flat_map(|s| s.pts.iter().map(|p| p.x))
        .collect();
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    xs
}

fn draw_xy(
    w: f64,
    series: &[Series],
    x: Axis,
    y: Axis,
    hover: RwSignal<Option<usize>>,
    tips: Tips,
) -> AnyView {
    let h = card_height(w);
    let p = Plot::new(w, h, 54.0, 16.0, 10.0, 26.0);
    let xs = all_xs(series);
    let pad = 14.0;
    let sx = Scale {
        log: false,
        lo: xs.first().copied().unwrap_or(0.0),
        hi: xs.last().copied().unwrap_or(1.0),
        from: p.l + pad,
        to: p.right() - pad,
    };
    let tx = move |v: f64| {
        if x.log {
            // log2 and log10 differ by a constant factor: one scale serves both.
            Scale { log: true, ..sx }.px(v)
        } else {
            sx.px(v)
        }
    };
    let his = series
        .iter()
        .flat_map(|s| s.pts.iter().map(|p| p.hi.max(p.y)));
    let ymax = his.fold(0.0_f64, f64::max);
    let los = series
        .iter()
        .flat_map(|s| s.pts.iter().map(|p| p.lo.min(p.y)));
    let ymin = los.fold(f64::INFINITY, f64::min);
    let (sy, ticks) = if y.log && ymin > 0.0 && ymin.is_finite() {
        let (a, b, t) = log_ticks(ymin, ymax);
        (
            Scale {
                log: true,
                lo: a,
                hi: b,
                from: p.bottom(),
                to: p.t,
            },
            t,
        )
    } else {
        let top = (ymax * 1.1).max(1e-9);
        let t = nice_ticks(top, 4);
        let top = t.last().copied().unwrap_or(top).max(top);
        (
            Scale {
                log: false,
                lo: 0.0,
                hi: top,
                from: p.bottom(),
                to: p.t,
            },
            t,
        )
    };
    let grid: Vec<AnyView> = ticks
        .iter()
        .map(|&v| {
            let yy = sy.px(v);
            view! {
                <line x1=p.l x2=p.right() y1=yy y2=yy class="gridline"></line>
                <text x=p.l - 8.0 y=yy + 3.5 class="ax" text-anchor="end">
                    {(y.fmt)(v)}
                </text>
            }
            .into_any()
        })
        .collect();
    let xs_px: Vec<f64> = xs.iter().map(|&v| tx(v)).collect();
    let keep = spaced(&xs_px, 40.0);
    let xlabels: Vec<AnyView> = xs
        .iter()
        .zip(&xs_px)
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|((v, px), _)| {
            view! {
                <text x=*px y=p.h - 8.0 class="ax" text-anchor="middle">
                    {(x.fmt)(*v)}
                </text>
            }
            .into_any()
        })
        .collect();
    let marks: Vec<AnyView> = series
        .iter()
        .map(|s| {
            let pts: Vec<(f64, f64)> = s.pts.iter().map(|q| (tx(q.x), sy.px(q.y))).collect();
            let has_band = s.pts.iter().any(|q| (q.hi - q.lo).abs() > 1e-9);
            let band = (has_band && s.pts.len() > 1).then(|| {
                let up: Vec<(f64, f64)> = s.pts.iter().map(|q| (tx(q.x), sy.px(q.hi))).collect();
                let dn: Vec<(f64, f64)> = s.pts.iter().map(|q| (tx(q.x), sy.px(q.lo))).collect();
                view! {
                    <path
                        d=crate::charts::band_path(&up, &dn)
                        fill=s.color.clone()
                        fill-opacity=if s.faint { "0.07" } else { "0.13" }
                    ></path>
                }
            });
            // A lone point's spread is a whisker: a band needs two ends.
            let whiskers = (has_band && s.pts.len() == 1).then(|| {
                let q = &s.pts[0];
                let xx = tx(q.x);
                view! {
                    <line
                        x1=xx
                        x2=xx
                        y1=sy.px(q.lo)
                        y2=sy.px(q.hi)
                        stroke=s.color.clone()
                        stroke-width="2"
                        stroke-opacity="0.5"
                    ></line>
                }
            });
            let dots: Vec<AnyView> = pts
                .iter()
                .map(|(cx, cy)| {
                    view! {
                        <circle
                            cx=*cx
                            cy=*cy
                            r=if s.faint { "2.5" } else { "3.5" }
                            fill=s.color.clone()
                            stroke="var(--surface)"
                            stroke-width="1.5"
                        ></circle>
                    }
                    .into_any()
                })
                .collect();
            view! {
                <g opacity=if s.faint { "0.75" } else { "1" }>
                    {band}
                    {whiskers}
                    <path
                        d=line_path(&pts)
                        fill="none"
                        stroke=s.color.clone()
                        stroke-width=if s.faint { "1.5" } else { "2" }
                        stroke-linejoin="round"
                        stroke-linecap="round"
                    ></path>
                    {dots}
                </g>
            }
            .into_any()
        })
        .collect();
    let tip_data: Vec<Tip> = xs
        .iter()
        .map(|&xv| {
            let mut rows = Vec::new();
            for s in series {
                if let Some(q) = s.pts.iter().find(|q| (q.x - xv).abs() < 1e-9) {
                    rows.push(TipRow::new(
                        s.color.clone(),
                        s.label.clone(),
                        range_text(y.fmt, q),
                    ));
                    if let Some(e) = &q.extra {
                        rows.push(TipRow::new("transparent", e.clone(), ""));
                    }
                }
            }
            Tip::new(format!("{} {}", (x.fmt)(xv), x.name), rows)
        })
        .collect();
    let cross = {
        let xs_px = xs_px.clone();
        move || {
            hover.get().and_then(|i| xs_px.get(i).copied()).map(|xx| {
                view! { <line x1=xx x2=xx y1=p.t y2=p.bottom() class="crosshair"></line> }
            })
        }
    };
    let items: Vec<LegendItem> = series
        .iter()
        .map(|s| {
            LegendItem::rule(s.color.clone(), s.label.clone()).at(if s.faint { 0.6 } else { 1.0 })
        })
        .collect();
    view! {
        {legend(items)}
        <svg width=p.w height=p.h viewBox=p.view_box()>
            {grid}
            {xlabels}
            {marks}
            {cross}
            {hit_columns(&p, &xs_px, tip_data, tips, hover)}
        </svg>
    }
    .into_any()
}

/// The table twin of an [`XyCard`]: one row per x, one column per series.
fn xy_table(series: &[Series], x: Axis, y: Axis) -> (Vec<String>, Vec<Vec<String>>) {
    let mut heads = vec![x.name.to_string()];
    heads.extend(series.iter().map(|s| s.label.clone()));
    let rows = all_xs(series)
        .into_iter()
        .map(|xv| {
            let mut r = vec![(x.fmt)(xv)];
            for s in series {
                r.push(
                    s.pts
                        .iter()
                        .find(|q| (q.x - xv).abs() < 1e-9)
                        .map(|q| range_text(y.fmt, q))
                        .unwrap_or_else(|| "—".to_string()),
                );
            }
            r
        })
        .collect();
    (heads, rows)
}

/// A card holding one [`Series`] chart. Renders nothing when no series has
/// a point: a phase the run did not measure has no card.
#[component]
pub fn XyCard(
    title: &'static str,
    #[prop(into)] note: TextProp,
    #[prop(into)] series: Signal<Vec<Series>>,
    x: Axis,
    y: Axis,
) -> impl IntoView {
    let node: NodeRef<html::Div> = NodeRef::new();
    let size = use_element_size(node);
    let width = Memo::new(move |_| size.get().0);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    let tips = use_tips();
    let empty = move || series.with(|s| s.iter().all(|s| s.pts.is_empty()));
    view! {
        <div class="card chart-card bn-chart" class:bn-none=empty>
            <ChartHead title=title note=note tv=tv/>
            <div class="chart-box" node_ref=node>
                {move || {
                    let w = width.get();
                    let s = series.get();
                    if w < 120.0 || s.iter().all(|s| s.pts.is_empty()) {
                        return ().into_any();
                    }
                    let (heads, rows) = xy_table(&s, x, y);
                    chart_or_table(tv, move || draw_xy(w, &s, x, y, hover, tips), heads, rows)
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Grouped bars
// ---------------------------------------------------------------------------

/// One series of a [`BarsCard`]: a value per category, `None` where the run
/// did not measure it. `(median, min, max)`.
#[derive(Clone, Debug, PartialEq)]
pub struct BarSeries {
    pub label: String,
    pub color: String,
    pub vals: Vec<Option<(f64, f64, f64)>>,
    /// Why a category has no value, where the engine said so (an energy
    /// window too short to measure): written in the bar's place, and in the
    /// tooltip and the table, instead of leaving a silent gap. Indexed like
    /// `vals`; shorter (or empty) means no reasons.
    pub gaps: Vec<Option<String>>,
}

impl BarSeries {
    /// The reason category `ci` has no value, when it has none.
    fn gap(&self, ci: usize) -> Option<&str> {
        if self.vals.get(ci).copied().flatten().is_some() {
            return None;
        }
        self.gaps.get(ci).and_then(|g| g.as_deref())
    }
}

/// The words written in a bar's place: the reason up to its colon ("too
/// short to measure: …" → "too short to measure").
pub fn gap_words(reason: &str) -> &str {
    reason.split(':').next().unwrap_or(reason).trim()
}

/// A cell of the tooltip or the table: the value, the gap's words, or "—".
fn cell_text(fmt: fn(f64) -> String, s: &BarSeries, ci: usize) -> String {
    match (s.vals.get(ci).copied().flatten(), s.gap(ci)) {
        (Some((m, lo, hi)), _) => range_text(fmt, &Pt::new(0.0, m, lo, hi)),
        (None, Some(g)) => gap_words(g).to_string(),
        (None, None) => "—".to_string(),
    }
}

/// One group of bars under its own y scale: a unit of its own ("tok/s",
/// "ms") gets a panel of its own rather than sharing a scale it would be
/// flattened on.
#[derive(Clone, Debug)]
pub struct BarPanel {
    pub title: String,
    pub cats: Vec<String>,
    pub series: Vec<BarSeries>,
    pub fmt: fn(f64) -> String,
}

fn draw_bars(w: f64, panel: &BarPanel, hover: RwSignal<Option<usize>>, tips: Tips) -> AnyView {
    let h = (card_height(w) * 0.85).max(150.0);
    let p = Plot::new(w, h, 50.0, 10.0, 18.0, 24.0);
    let n = panel.cats.len().max(1);
    let k = panel.series.len().max(1);
    let max = panel
        .series
        .iter()
        .flat_map(|s| s.vals.iter().flatten().map(|v| v.2.max(v.0)))
        .fold(0.0_f64, f64::max)
        * 1.12;
    let ticks = nice_ticks(max.max(1e-9), 3);
    let top = ticks.last().copied().unwrap_or(max).max(max).max(1e-9);
    let grid: Vec<AnyView> = ticks
        .iter()
        .map(|&v| {
            let yy = p.y(v, top);
            view! {
                <line x1=p.l x2=p.right() y1=yy y2=yy class="gridline"></line>
                <text x=p.l - 8.0 y=yy + 3.5 class="ax" text-anchor="end">{(panel.fmt)(v)}</text>
            }
            .into_any()
        })
        .collect();
    let band = p.band_w(n);
    let bw = ((band * 0.7) / k as f64).clamp(4.0, 24.0);
    let group_w = bw * k as f64 + 2.0 * (k as f64 - 1.0);
    let mut marks: Vec<AnyView> = Vec::new();
    for (ci, _) in panel.cats.iter().enumerate() {
        let x0 = p.band_mid(ci, n) - group_w / 2.0;
        for (si, s) in panel.series.iter().enumerate() {
            let x = x0 + si as f64 * (bw + 2.0);
            let Some((v, lo, hi)) = s.vals.get(ci).copied().flatten() else {
                // No bar: the reason, upright along the bar's place.
                if let Some(g) = s.gap(ci) {
                    let (tx, ty) = (x + bw / 2.0 + 3.5, p.bottom() - 4.0);
                    marks.push(
                        view! {
                            <text
                                x=tx
                                y=ty
                                class="ax bn-gap"
                                transform=format!("rotate(-90 {tx} {ty})")
                            >
                                {gap_words(g).to_string()}
                            </text>
                        }
                        .into_any(),
                    );
                }
                continue;
            };
            let yv = p.y(v, top);
            let hh = (p.bottom() - yv).max(1.0);
            marks.push(
                view! {
                    <path d=crate::charts::top_round(x, yv, bw, hh, 3.0) fill=s.color.clone()></path>
                }
                .into_any(),
            );
            if (hi - lo).abs() > 1e-9 {
                let cx = x + bw / 2.0;
                marks.push(
                    view! {
                        <line
                            x1=cx
                            x2=cx
                            y1=p.y(lo, top)
                            y2=p.y(hi, top)
                            stroke="var(--text)"
                            stroke-opacity="0.55"
                            stroke-width="1.5"
                        ></line>
                    }
                    .into_any(),
                );
            }
        }
    }
    // Labels that would run into each other give way, the last one kept.
    let mids: Vec<f64> = (0..n).map(|i| p.band_mid(i, n)).collect();
    let widest = panel
        .cats
        .iter()
        .map(|c| c.chars().count())
        .max()
        .unwrap_or(1) as f64;
    let keep = spaced(&mids, widest * 6.5 + 6.0);
    let labels: Vec<AnyView> = panel
        .cats
        .iter()
        .enumerate()
        .filter(|(i, _)| keep.get(*i).copied().unwrap_or(true))
        .map(|(i, c)| {
            view! {
                <text x=p.band_mid(i, n) y=p.h - 8.0 class="ax" text-anchor="middle">{c.clone()}</text>
            }
            .into_any()
        })
        .collect();
    let xs: Vec<f64> = (0..n).map(|i| p.band_mid(i, n)).collect();
    let tip_data: Vec<Tip> = panel
        .cats
        .iter()
        .enumerate()
        .map(|(ci, c)| {
            let rows = panel
                .series
                .iter()
                .map(|s| {
                    TipRow::new(
                        s.color.clone(),
                        s.label.clone(),
                        cell_text(panel.fmt, s, ci),
                    )
                })
                .collect();
            Tip::new(format!("{} · {c}", panel.title), rows)
        })
        .collect();
    view! {
        <svg width=p.w height=p.h viewBox=p.view_box()>
            <text x=p.l y=11.0 class="axl">{panel.title.clone()}</text>
            {grid}
            {marks}
            {labels}
            {hit_columns(&p, &xs, tip_data, tips, hover)}
        </svg>
    }
    .into_any()
}

fn bars_table(panels: &[BarPanel]) -> (Vec<String>, Vec<Vec<String>>) {
    let labels: Vec<String> = panels
        .first()
        .map(|p| p.series.iter().map(|s| s.label.clone()).collect())
        .unwrap_or_default();
    let mut heads = vec!["Measure".to_string()];
    heads.extend(labels);
    let mut rows = Vec::new();
    for p in panels {
        for (ci, c) in p.cats.iter().enumerate() {
            let mut r = vec![format!("{} · {c}", p.title)];
            for s in &p.series {
                r.push(cell_text(p.fmt, s, ci));
            }
            rows.push(r);
        }
    }
    (heads, rows)
}

/// A card of bar panels side by side, each on its own scale, under one
/// legend. Renders nothing when no panel has a value.
#[component]
pub fn BarsCard(
    title: &'static str,
    #[prop(into)] note: TextProp,
    #[prop(into)] panels: Signal<Vec<BarPanel>>,
    /// A line of figures under the bars ("stall 175 ms").
    #[prop(optional, into)]
    foot: Option<Signal<Vec<(String, String)>>>,
) -> impl IntoView {
    let tv = RwSignal::new(false);
    let tips = use_tips();
    let empty = move || {
        panels.with(|ps| {
            ps.iter().all(|p| {
                p.series.iter().all(|s| {
                    s.vals.iter().all(Option::is_none) && s.gaps.iter().all(Option::is_none)
                })
            })
        })
    };
    view! {
        <div class="card chart-card bn-chart" class:bn-none=empty>
            <ChartHead title=title note=note tv=tv/>
            {move || {
                let ps = panels.get();
                let items: Vec<LegendItem> = ps
                    .first()
                    .map(|p| {
                        p.series
                            .iter()
                            .map(|s| LegendItem::fill(s.color.clone(), s.label.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let (heads, rows) = bars_table(&ps);
                let chart = move || {
                    view! {
                        {legend(items)}
                        <div class="bn-panels">
                            {ps.into_iter().map(|p| view! { <BarPanelView panel=p tips=tips/> }).collect_view()}
                        </div>
                    }
                    .into_any()
                };
                chart_or_table(tv, chart, heads, rows)
            }}
            {foot.map(|f| {
                view! {
                    <Show when=move || f.with(|v| !v.is_empty())>
                        <div class="chart-foot facts">
                            {move || {
                                f.get()
                                    .into_iter()
                                    .map(|(k, v)| view! { <span class="fact-inline">{k} " " <b>{v}</b></span> })
                                    .collect_view()
                            }}
                        </div>
                    </Show>
                }
            })}
        </div>
    }
}

#[component]
fn BarPanelView(panel: BarPanel, tips: Tips) -> impl IntoView {
    let node: NodeRef<html::Div> = NodeRef::new();
    let size = use_element_size(node);
    let width = Memo::new(move |_| size.get().0);
    let hover = RwSignal::new(None::<usize>);
    let panel = StoredValue::new(panel);
    view! {
        <div class="bn-panel" node_ref=node>
            {move || {
                let w = width.get();
                (w >= 100.0).then(|| panel.with_value(|p| draw_bars(w, p, hover, tips)))
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A category without a value shows the engine's reason where it gave
    /// one (decision 56), "—" where it did not, and a value never shows one.
    #[test]
    fn a_gap_reads_as_its_reason() {
        let s = BarSeries {
            label: "run 1".into(),
            color: "red".into(),
            vals: vec![None, Some((2.0, 2.0, 2.0)), None],
            gaps: vec![
                Some("too short to measure: the shortest window is 12 ms".into()),
                Some("ignored: a value is there".into()),
            ],
        };
        let fmt: fn(f64) -> String = |v| format!("{v:.1}");
        assert_eq!(cell_text(fmt, &s, 0), "too short to measure");
        assert_eq!(cell_text(fmt, &s, 1), "2.0");
        assert_eq!(s.gap(1), None);
        assert_eq!(cell_text(fmt, &s, 2), "—");
    }

    #[test]
    fn labels_that_would_collide_give_way_to_the_later_one() {
        assert_eq!(
            spaced(&[0.0, 10.0, 60.0, 70.0], 40.0),
            [false, true, false, true]
        );
        assert_eq!(spaced(&[0.0, 50.0, 100.0], 40.0), [true, true, true]);
        assert!(spaced(&[], 40.0).is_empty());
    }

    #[test]
    fn log_ticks_end_on_one_two_five_values() {
        let (a, b, t) = log_ticks(40.0, 20_000.0);
        assert_eq!((a, b), (20.0, 20_000.0));
        assert_eq!(t.len(), 5, "{t:?}");
        for v in [20.0, 100.0, 1000.0, 10_000.0, 20_000.0] {
            assert!(t.iter().any(|x| (x - v).abs() < 1e-6), "{v} in {t:?}");
        }
        let (a, b, t) = log_ticks(40.0, 700.0);
        assert_eq!((a, b), (20.0, 1000.0));
        assert_eq!(t.len(), 6, "{t:?}");
    }

    #[test]
    fn a_scale_maps_its_ends_to_its_pixels() {
        let s = Scale {
            log: true,
            lo: 512.0,
            hi: 262_144.0,
            from: 10.0,
            to: 110.0,
        };
        assert!((s.px(512.0) - 10.0).abs() < 1e-9);
        assert!((s.px(262_144.0) - 110.0).abs() < 1e-9);
        // 8192 is 4 of 9 doublings up.
        assert!((s.px(8192.0) - (10.0 + 100.0 * 4.0 / 9.0)).abs() < 1e-6);
    }
}
