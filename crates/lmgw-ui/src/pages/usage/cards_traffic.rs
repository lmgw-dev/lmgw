use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::{SeriesMeta, UsageCell, UsageSeriesResponse, UsageTopResponse};

use crate::charts::{
    self, band_path, bot_round, chart_height, crosshair, hit_bands, legend, line_path, path_len,
    top_round, x_labels, y_grid, LegendItem, Plot, Tip, TipRow, Tips,
};
use crate::fmt::{compact, grouped};
use crate::widgets::ShowMore;

use super::*;

/// A chart point `(x, y)`, and the pair of points a latency band is drawn between.
type Band = ((f64, f64), (f64, f64));

// ---------------------------------------------------------------------------
// 4 · tokens in / out
// ---------------------------------------------------------------------------

#[component]
pub(super) fn TokensCard(
    series: Src<UsageSeriesResponse>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    view! {
        <div class="card chart-card u-tokens">
            <ChartHead title="Tokens in / out" note="diverging from one zero rule · the ratio is the shape of the workload" tv=tv/>
            <CardErr err=series.err/>
            <div class="chart-box" node_ref=node class:stale=move || series.stale()>
                {move || {
                    let w = width.get();
                    let Some(r) = series.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    let g = Grid::build(&r);
                    let n = g.nb();
                    if n == 0 {
                        return view! { <div class="chart-empty">"No buckets."</div> }.into_any();
                    }
                    let anim = entry.get_untracked();
                    let p = Plot::new(w, chart_height(w), 56.0, 16.0, 12.0, 30.0);
                    let in_max = g.totals.iter().map(|t| t.tokens_in as f64).fold(0.0, f64::max);
                    let out_max = g
                        .totals
                        .iter()
                        .map(|t| t.tokens_out as f64)
                        .fold(0.0, f64::max);
                    let span = (in_max + out_max).max(1.0);
                    let up_h = p.ih() * (in_max / span);
                    let dn_h = p.ih() - up_h;
                    let mid = p.t + up_h;
                    let bw = (p.band_w(n) * 0.6).clamp(1.0, 20.0);
                    let mut axis: Vec<AnyView> = Vec::new();
                    for f in [0.5_f64, 1.0] {
                        let y = mid - up_h * f;
                        axis.push(
                            view! {
                                <line x1=p.l x2=p.right() y1=y y2=y class="gridline"></line>
                                <text x=p.l - 8.0 y=y + 3.5 class="ax" text-anchor="end">
                                    {compact(in_max * f)}
                                </text>
                            }
                                .into_any(),
                        );
                    }
                    // The output half is shorter: one tick, or the labels collide.
                    axis.push(
                        view! {
                            <line
                                x1=p.l
                                x2=p.right()
                                y1=mid + dn_h
                                y2=mid + dn_h
                                class="gridline"
                            ></line>
                            <text x=p.l - 8.0 y=mid + dn_h + 3.5 class="ax" text-anchor="end">
                                {compact(out_max)}
                            </text>
                            <line
                                x1=p.l
                                x2=p.right()
                                y1=mid
                                y2=mid
                                stroke="var(--border-strong)"
                                stroke-width="1"
                            ></line>
                        }
                            .into_any(),
                    );
                    let cols: Vec<AnyView> = (0..n)
                        .map(|b| {
                            let x = p.band_x(b, n) + (p.band_w(n) - bw) / 2.0;
                            let hi = if in_max > 0.0 {
                                (g.totals[b].tokens_in as f64 / in_max) * up_h
                            } else {
                                0.0
                            };
                            let ho = if out_max > 0.0 {
                                (g.totals[b].tokens_out as f64 / out_max) * dn_h
                            } else {
                                0.0
                            };
                            let cls = if anim { "anim-fade" } else { "" };
                            let style = if anim {
                                format!("animation-delay:{}ms", b * 8)
                            } else {
                                String::new()
                            };
                            view! {
                                <g class=cls style=style>
                                    <path
                                        d=top_round(x, mid - hi, bw, (hi - 1.0).max(1.0), 4.0)
                                        fill="var(--c1)"
                                    ></path>
                                    <path
                                        d=bot_round(x, mid + 1.0, bw, (ho - 1.0).max(1.0), 4.0)
                                        fill="var(--c2)"
                                    ></path>
                                </g>
                            }
                                .into_any()
                        })
                        .collect();
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|b| {
                            let t = &g.totals[b];
                            let ratio = if t.tokens_out > 0 {
                                format!("{:.1} : 1", t.tokens_in as f64 / t.tokens_out as f64)
                            } else {
                                "—".into()
                            };
                            Tip::new(
                                    bucket_full(&g.buckets[b]),
                                    vec![
                                        TipRow::new(
                                            "var(--c1)",
                                            "input",
                                            compact(t.tokens_in as f64),
                                        ),
                                        TipRow::new(
                                            "var(--c2)",
                                            "output",
                                            compact(t.tokens_out as f64),
                                        ),
                                        TipRow::new(
                                            "var(--text-3)",
                                            "cached in",
                                            compact(t.tokens_cached as f64),
                                        ),
                                        TipRow::new(
                                            "var(--text-3)",
                                            "cache write",
                                            compact(t.tokens_cache_write as f64),
                                        ),
                                    ],
                                )
                                .with_total("ratio", ratio)
                        })
                        .collect();
                    let rows: Vec<Vec<String>> = (0..n)
                        .map(|b| {
                            let t = &g.totals[b];
                            vec![
                                bucket_full(&g.buckets[b]),
                                compact(t.tokens_in as f64),
                                compact(t.tokens_out as f64),
                                compact(t.tokens_cached as f64),
                                compact(t.tokens_cache_write as f64),
                                compact(t.tokens_reasoning as f64),
                            ]
                        })
                        .collect();
                    let chart = || {
                        view! {
                        {legend(
                            vec![
                                LegendItem::fill("var(--c1)", "input (prompt)"),
                                LegendItem::fill("var(--c2)", "output (completion)"),
                            ],
                        )}
                        <svg width=p.w height=p.h viewBox=p.view_box()>
                            {axis}
                            {cols}
                            {x_labels(&p, &g.labels(), g.every(p.iw()))}
                            {crosshair(&p, hover, n)}
                            {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, heads(
                                &[
                                    "Bucket",
                                    "Input",
                                    "Output",
                                    "Cached in",
                                    "Cache write",
                                    "Reasoning",
                                ],
                            ),
                            rows,)}
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// 5 · latency
// ---------------------------------------------------------------------------

#[component]
pub(super) fn LatencyCard(
    series: Src<UsageSeriesResponse>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    view! {
        <div class="card chart-card u-latency">
            <ChartHead title="Latency" note="p50 with the p95 band, per bucket, from the stored histograms — a mean would be dragged four times over by one 30-second outlier" tv=tv/>
            <CardErr err=series.err/>
            <div class="chart-box" node_ref=node class:stale=move || series.stale()>
                {move || {
                    let w = width.get();
                    let Some(r) = series.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    let g = Grid::build(&r);
                    let n = g.nb();
                    if n == 0 {
                        return view! { <div class="chart-empty">"No buckets."</div> }.into_any();
                    }
                    // `latency` is aligned with `buckets`; index defensively all
                    // the same, because a shorter list must gap rather than shift.
                    let at = |i: usize| r.latency.get(i);
                    let has = r.latency.iter().any(|p| p.p50_total_ms.is_some());
                    if !has {
                        return view! {
                            <div class="chart-empty">
                                "No successful request in this window, so there is no percentile to draw."
                            </div>
                        }
                            .into_any();
                    }
                    let anim = entry.get_untracked();
                    let p = Plot::new(w, chart_height(w), 56.0, 64.0, 12.0, 30.0);
                    let max = r
                        .latency
                        .iter()
                        .filter_map(|q| q.p95_total_ms)
                        .fold(0.0, f64::max)
                        .max(1.0) * 1.15;
                    let grid = y_grid(&p, max, 4, &|v| format!("{} ms", v.round()));
                    // An empty bucket has no percentile: a gap, never a zero that
                    // would draw a gateway which got infinitely fast.
                    let pts = |f: fn(&lmgw_api_types::LatencyPoint) -> Option<f64>| {
                        (0..n)
                            .map(|i| {
                                at(i).and_then(f).map(|v| (p.band_mid(i, n), p.y(v, max)))
                            })
                            .collect::<Vec<_>>()
                    };
                    let hi = pts(|q| q.p95_total_ms);
                    let lo = pts(|q| q.p50_total_ms);
                    // The band only exists where both ends were measured.
                    let bands: Vec<AnyView> = {
                        let paired: Vec<Option<Band>> = (0..n)
                            .map(|i| hi[i].zip(lo[i]))
                            .collect();
                        let mut out = Vec::new();
                        let mut run: Vec<Band> = Vec::new();
                        let mut flush = |run: &mut Vec<Band>| {
                            if run.len() > 1 {
                                let up: Vec<(f64, f64)> = run.iter().map(|(a, _)| *a).collect();
                                let dn: Vec<(f64, f64)> = run.iter().map(|(_, b)| *b).collect();
                                out.push(
                                    view! {
                                        <path
                                            d=band_path(&up, &dn)
                                            fill="var(--c2)"
                                            fill-opacity="0.10"
                                        ></path>
                                    }
                                        .into_any(),
                                );
                            }
                            run.clear();
                        };
                        for cell in &paired {
                            match cell {
                                Some(pair) => run.push(*pair),
                                None => flush(&mut run),
                            }
                        }
                        flush(&mut run);
                        out
                    };
                    let draw_cls = if anim { "anim-draw" } else { "" };
                    let line = |runs: Vec<Vec<(f64, f64)>>, color: &'static str| -> Vec<AnyView> {
                        runs.into_iter()
                            .map(|run| {
                                let len = path_len(&run);
                                // A lone point has no line to draw: give it a dot,
                                // or an isolated bucket would vanish entirely.
                                if run.len() == 1 {
                                    let (x, y) = run[0];
                                    return view! {
                                        <circle cx=x cy=y r="2.5" fill=color></circle>
                                    }
                                        .into_any();
                                }
                                view! {
                                    <path
                                        d=line_path(&run)
                                        class=draw_cls
                                        style=format!("--len:{len:.0}")
                                        fill="none"
                                        stroke=color
                                        stroke-width="2"
                                        stroke-linejoin="round"
                                        stroke-linecap="round"
                                    ></path>
                                }
                                    .into_any()
                            })
                            .collect()
                    };
                    let hi_lines = line(charts::line_runs(&hi), "var(--c2)");
                    let lo_lines = line(charts::line_runs(&lo), "var(--c1)");
                    let last = |v: &[Option<(f64, f64)>]| {
                        (0..n).rev().find_map(|i| v[i].map(|pt| (i, pt)))
                    };
                    let end_marks: Vec<AnyView> = [
                        (last(&hi), "var(--c2)", 0usize),
                        (last(&lo), "var(--c1)", 1usize),
                    ]
                        .into_iter()
                        .filter_map(|(found, c, which)| {
                            let (i, (x, y)) = found?;
                            let q = at(i)?;
                            let v = if which == 0 { q.p95_total_ms } else { q.p50_total_ms }?;
                            Some(
                                view! {
                                    <circle
                                        cx=x
                                        cy=y
                                        r="4"
                                        fill=c
                                        stroke="var(--surface)"
                                        stroke-width="2"
                                    ></circle>
                                    <text x=x + 9.0 y=y + 4.0 class="mark-label">
                                        {format!("{} ms", v.round())}
                                    </text>
                                }
                                    .into_any(),
                            )
                        })
                        .collect();
                    let ms = |v: Option<f64>| {
                        v.map(|v| format!("{} ms", v.round()))
                            .unwrap_or_else(|| "—".to_string())
                    };
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|i| {
                            let q = at(i);
                            if q.map(|q| q.p50_total_ms.is_none()).unwrap_or(true) {
                                return Tip::new(
                                    bucket_full(&g.buckets[i]),
                                    vec![
                                        TipRow::new(
                                            "var(--text-3)",
                                            "no successful request",
                                            "—",
                                        ),
                                    ],
                                );
                            }
                            let q = q.unwrap();
                            Tip::new(
                                    bucket_full(&g.buckets[i]),
                                    vec![
                                        TipRow::new("var(--c2)", "p95 total", ms(q.p95_total_ms)),
                                        TipRow::new("var(--c1)", "p50 total", ms(q.p50_total_ms)),
                                        TipRow::new(
                                            "var(--text-3)",
                                            "p95 TTFB",
                                            ms(q.p95_ttfb_ms),
                                        ),
                                        TipRow::new(
                                            "var(--text-3)",
                                            "p50 TTFB",
                                            ms(q.p50_ttfb_ms),
                                        ),
                                    ],
                                )
                                .with_total("requests", grouped(g.totals[i].requests.max(0) as u64))
                        })
                        .collect();
                    let rows: Vec<Vec<String>> = (0..n)
                        .map(|i| {
                            let q = at(i);
                            vec![
                                bucket_full(&g.buckets[i]),
                                ms(q.and_then(|q| q.p50_total_ms)),
                                ms(q.and_then(|q| q.p95_total_ms)),
                                ms(q.and_then(|q| q.p50_ttfb_ms)),
                                ms(q.and_then(|q| q.p95_ttfb_ms)),
                                grouped(g.totals[i].requests.max(0) as u64),
                            ]
                        })
                        .collect();
                    // The window's own percentiles, from the merged
                    // histograms: one line under the chart.
                    let stat = |l: &'static str, v: Option<f64>| {
                        view! {
                            <span class="fact-inline">
                                {l}
                                " "
                                <b>
                                    {v
                                        .map(|v| format!("{} ms", v.round()))
                                        .unwrap_or_else(|| "—".into())}
                                </b>
                            </span>
                        }
                            .into_any()
                    };
                    let chart = || {
                        view! {
                        {legend(
                            vec![
                                LegendItem::rule("var(--c1)", "p50 total"),
                                LegendItem::rule("var(--c2)", "p95 total"),
                            ],
                        )}
                        <svg width=p.w height=p.h viewBox=p.view_box()>
                            {grid} {bands} {hi_lines} {lo_lines} {end_marks}
                            {x_labels(&p, &g.labels(), g.every(p.iw()))}
                            {crosshair(&p, hover, n)} {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, heads(
                                &["Bucket", "p50 total", "p95 total", "p50 TTFB", "p95 TTFB", "Requests"],
                            ),
                            rows,)}
                        <div class="chart-foot facts">
                            {stat("p50", r.p50_total_ms)} {stat("p95", r.p95_total_ms)}
                            {stat("TTFB p50", r.p50_ttfb_ms)} {stat("p95", r.p95_ttfb_ms)}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// 6 · where it went
// ---------------------------------------------------------------------------

/// Where it went, as a table: each alias one row, its cost and its tokens as
/// bars inside their cells. Text in HTML cells ellipsizes and carries its
/// title — SVG text drawn over a bar track could only be clipped by guesswork.
#[component]
pub(super) fn ShareCard(
    top: Src<UsageTopResponse>,
    class: RwSignal<String>,
    key_id: RwSignal<String>,
    /// The aliases' page slots: the rows the other charts colour wear the
    /// same colour here; the rest are what those charts fold into Other.
    slots: Memo<HashMap<String, u8>>,
    group_by: Signal<String>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    // How many rank rows the table draws (ShowMore owns it).
    let shown = RwSignal::new(10usize);
    let total = Signal::derive(move || top.data.with(|r| r.as_ref().map_or(0, |r| r.rows.len())));
    let note = Signal::derive(move || {
        let by = if group_by.get() == "alias" {
            "coloured as in the charts; grey rows are what they fold into Other"
        } else {
            "coloured as the per-alias charts; grey rows are past their six colours"
        };
        format!("cost and tokens per alias, in the server's rank order · {by}")
    });
    view! {
        <div class="card chart-card u-share">
            <ChartHead title="Where it went" note=move || note.get()/>
            <CardErr err=top.err/>
            <div class="chart-box" class:stale=move || top.stale()>
                {move || {
                    let Some(r) = top.data.get() else { return ().into_any() };
                    if r.rows.is_empty() {
                        return view! { <div class="chart-empty">"Nothing in this window."</div> }
                            .into_any();
                    }
                    let meta: HashMap<&str, &SeriesMeta> = r
                        .series
                        .iter()
                        .map(|s| (s.key.as_str(), s))
                        .collect();
                    let cur = r.currency.clone();
                    let anim = entry.get_untracked();
                    let sl = slots.get();
                    let toks = |c: &UsageCell| (c.tokens_in + c.tokens_out) as f64;
                    let max_cost = r.rows.iter().map(|c| c.cost_micro as f64).fold(0.0, f64::max).max(1.0);
                    let max_tok = r.rows.iter().map(toks).fold(0.0, f64::max).max(1.0);
                    let body: Vec<AnyView> = r
                        .rows
                        .iter()
                        .take(shown.get())
                        .enumerate()
                        .map(|(i, c)| {
                            let m = meta.get(c.series.as_str()).copied();
                            let label = m.map(|m| m.label.clone()).unwrap_or_else(|| c.series.clone());
                            let is_local = m.is_some_and(|m| m.local);
                            let color = charts::slot_of(&sl, &c.series);
                            let t = toks(c);
                            let cost_text = if c.cost_micro > 0 {
                                money(c.cost_micro, &cur)
                            } else if is_local {
                                "free · local".to_string()
                            } else if c.cost_unknown_requests > 0 {
                                "unpriced".to_string()
                            } else {
                                money(0, &cur)
                            };
                            let per_mtok = if c.cost_micro > 0 && t > 0.0 {
                                money((c.cost_micro as f64 / (t / 1e6)) as i64, &cur)
                            } else {
                                "—".into()
                            };
                            let bar = |v: f64, max: f64, text: String| {
                                let w = if v > 0.0 { (v / max * 100.0).max(1.5) } else { 0.0 };
                                let style = if anim {
                                    format!("width:{w:.1}%;background:{color};animation-delay:{}ms", i * 35)
                                } else {
                                    format!("width:{w:.1}%;background:{color}")
                                };
                                view! {
                                    <td class="bar-cell">
                                        <span class="cell-bar">
                                            <i class:anim-row=anim style=style></i>
                                        </span>
                                        <span class="bar-val">{text}</span>
                                    </td>
                                }
                            };
                            let tip = Tip::new(
                                label.clone(),
                                vec![
                                    TipRow::new(color, "cost", cost_text.clone()),
                                    TipRow::new(color, "tokens", compact(t)),
                                    TipRow::new(color, "requests", grouped(c.requests.max(0) as u64)),
                                    TipRow::new(color, "per Mtok", per_mtok.clone()),
                                ],
                            );
                            let href = TrafficLink::scoped(&class.get_untracked(), &key_id.get_untracked())
                                .alias(c.series.clone())
                                .href();
                            let ctl = tips;
                            view! {
                                <tr
                                    on:mousemove=move |ev: web_sys::MouseEvent| ctl.show(&ev, tip.clone())
                                    on:mouseleave=move |_| ctl.hide()
                                >
                                    <td class="clip share-name" title=label.clone()>
                                        <i class="slot-swatch" style=format!("background:{color}")></i>
                                        <a class="mono-sm row-link" href=href title=format!("Open {label} in Traffic")>
                                            {label.clone()}
                                        </a>
                                    </td>
                                    {bar(c.cost_micro as f64, max_cost, cost_text)}
                                    {bar(t, max_tok, compact(t))}
                                    <td class="num col-p3">{grouped(c.requests.max(0) as u64)}</td>
                                    <td class="num col-p2">{per_mtok}</td>
                                </tr>
                            }
                                .into_any()
                        })
                        .collect();
                    view! {
                        <div class="table-scroll share-wrap">
                            <table class="data share-table">
                                <thead>
                                    <tr>
                                        <th>"Alias"</th>
                                        <th>"Cost"</th>
                                        <th>"Tokens"</th>
                                        <th class="num-h col-p3">"Requests"</th>
                                        <th class="num-h col-p2">"Per Mtok"</th>
                                    </tr>
                                </thead>
                                <tbody>{body}</tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }}
                // Outside the table's closure: that one re-renders on every
                // fold and unfold, and this owns the choice.
                <ShowMore total=total shown=shown noun="aliases" persist="usage.share"/>
                {move || {
                    let Some(r) = top.data.get() else { return ().into_any() };
                    if r.rows.is_empty() {
                        return ().into_any();
                    }
                    let cur = r.currency.clone();
                    // Over every row, shown or not: the foot is the window's
                    // total, whatever the list is folded to.
                    let unpriced = r
                        .rows
                        .iter()
                        .fold((0i64, 0i64), |a, c| {
                            (a.0 + c.cost_unknown_requests, a.1 + c.cost_unknown_tokens)
                        });
                    let (cost, toks, reqs) = r.rows.iter().fold((0i64, 0i64, 0i64), |a, c| {
                        (a.0 + c.cost_micro, a.1 + c.tokens_in + c.tokens_out, a.2 + c.requests)
                    });
                    let n = r.rows.len();
                    view! {
                        <div class="chart-foot">
                            {format!(
                                "{} {} in this window · {} · {} tokens · {} requests",
                                grouped(n as u64),
                                if n == 1 { "alias" } else { "aliases" },
                                money(cost, &cur),
                                compact(toks as f64),
                                grouped(reqs.max(0) as u64),
                            )}
                            {unpriced_note(unpriced.0, unpriced.1).map(|n| format!(" · {n}"))}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}
