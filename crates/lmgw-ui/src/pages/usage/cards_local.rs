use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::{UsageHeatResponse, UsageLocalResponse, UsageSeriesResponse};

use crate::charts::{
    self, chart_height, crosshair, hit_bands, legend, line_path, path_len, x_labels, y_grid,
    LegendItem, Plot, Tip, TipRow, Tips,
};
use crate::fmt::{compact, grouped, pct};

use super::*;

// ---------------------------------------------------------------------------
// 7 · local vs cloud + the counterfactual
// ---------------------------------------------------------------------------

#[component]
pub(super) fn LocalCard(local: Src<UsageLocalResponse>) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let tv = RwSignal::new(false);
    view! {
        <div class="card chart-card u-local">
            <ChartHead title="Local vs cloud" note="tokens in this window" tv=tv/>
            <CardErr err=local.err/>
            <div class="chart-box" node_ref=node class:stale=move || local.stale()>
                {move || {
                    let w = width.get();
                    let Some(l) = local.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    let tot = (l.local_tokens + l.cloud_tokens) as f64;
                    if tot <= 0.0 {
                        return view! { <div class="chart-empty">"No tokens in this window."</div> }
                            .into_any();
                    }
                    let share = l.local_tokens as f64 / tot;
                    let lw = (share * w).clamp(0.0, w);
                    let chart = || {
                        view! {
                        <svg width=w height="74" viewBox=format!("0 0 {w:.0} 74")>
                            <rect
                                x="0"
                                y="14"
                                width=(lw - 1.0).max(0.0)
                                height="26"
                                rx="4"
                                fill="var(--c3)"
                            ></rect>
                            <rect
                                x=lw + 1.0
                                y="14"
                                width=(w - lw - 1.0).max(0.0)
                                height="26"
                                rx="4"
                                fill="var(--c1)"
                            ></rect>
                            <text x="10" y="31" fill="var(--bg0)" font-size="12" font-weight="600">
                                {format!("{} local", pct(share))}
                            </text>
                            <text
                                x=w - 10.0
                                y="31"
                                fill="var(--bg0)"
                                font-size="12"
                                font-weight="600"
                                text-anchor="end"
                            >
                                {pct(1.0 - share)}
                            </text>
                            <text x="0" y="58" class="ax">
                                {format!(
                                    "{} local tokens · {} cloud",
                                    compact(l.local_tokens as f64),
                                    compact(l.cloud_tokens as f64),
                                )}
                            </text>
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, heads(&["", "Tokens", "Share"]),
                            vec![
                                vec![
                                    "local".to_string(),
                                    compact(l.local_tokens as f64),
                                    pct(share),
                                ],
                                vec![
                                    "cloud".to_string(),
                                    compact(l.cloud_tokens as f64),
                                    pct(1.0 - share),
                                ],
                                vec![
                                    "counterfactual".to_string(),
                                    l
                                        .counterfactual_micro
                                        .map(|v| money(v, &l.currency))
                                        .unwrap_or_else(|| "not configured".into()),
                                    l.reference_alias.clone().unwrap_or_else(|| "—".into()),
                                ],
                            ],)}
                        <div style="margin-top:6px;padding-top:12px;border-top:1px solid var(--border)">
                            <div class="tile-label">"Counterfactual"</div>
                            {match (l.counterfactual_micro, l.reference_alias.clone()) {
                                (Some(v), Some(alias)) => {
                                    view! {
                                        <div class="tile-value" style="font-size:34px">
                                            {money(v, &l.currency)}
                                        </div>
                                        <div class="tile-sub">
                                            "what those " <b>{compact(l.local_tokens as f64)}</b>
                                            " local tokens " <b>"would"</b> " have cost at "
                                            <b>{alias}</b>
                                            "'s price. Not a saving — nobody would have run all of it there. "
                                            <span class="dim">
                                                "("
                                                <a href=crate::pages::settings_href("local_reference_alias")>
                                                    "Settings → Usage & cost"
                                                </a>
                                                ")"
                                            </span>
                                        </div>
                                    }
                                        .into_any()
                                }
                                _ => {
                                    view! {
                                        <div class="tile-value" style="font-size:20px">"not configured"</div>
                                        <div class="tile-sub">
                                            "No reference alias is set, so there is no honest price to compare local tokens against. "
                                            <span class="dim">
                                                "Set one under "
                                                <a href=crate::pages::settings_href("local_reference_alias")>
                                                    "Settings → Usage & cost"
                                                </a>
                                                "."
                                            </span>
                                        </div>
                                    }
                                        .into_any()
                                }
                            }}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// 8 · local decode throughput
// ---------------------------------------------------------------------------

#[component]
pub(super) fn PerfCard(
    by_alias: Src<UsageSeriesResponse>,
    local: Src<UsageLocalResponse>,
    slots: Memo<HashMap<String, u8>>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    view! {
        <div class="card chart-card u-perf">
            <ChartHead title="Local decode throughput" note="tok/s per model per bucket, from the stored llama.cpp timings · Σtokens / Σms, never a mean of rates" tv=tv/>
            <CardErr err=by_alias.err/>
            <div class="chart-box" node_ref=node class:stale=move || by_alias.stale()>
                {move || {
                    let w = width.get();
                    let Some(r) = by_alias.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    let g = Grid::build(&r);
                    let n = g.nb();
                    let locals: Vec<usize> = (0..g.series.len())
                        .filter(|i| {
                            g.series[*i].local
                                && (0..n).any(|b| g.cells[b][*i].decode_ms > 0.0)
                        })
                        .collect();
                    if n == 0 || locals.is_empty() {
                        return view! {
                            <div class="chart-empty">
                                "No local decode timings in this window — either nothing ran locally, or it ran before timings were stored."
                            </div>
                        }
                            .into_any();
                    }
                    let anim = entry.get_untracked();
                    let sl = slots.get();
                    let color = |si: usize| series_color(&sl, &g.series[si]);
                    let p = Plot::new(w, chart_height(w), 48.0, 66.0, 12.0, 30.0);
                    let tok_s = |b: usize, si: usize| -> Option<f64> {
                        let c = &g.cells[b][si];
                        (c.decode_ms > 0.0 && c.decode_tokens > 0)
                            .then(|| c.decode_tokens as f64 / (c.decode_ms / 1000.0))
                    };
                    let max = locals
                        .iter()
                        .flat_map(|&si| (0..n).filter_map(move |b| tok_s(b, si)))
                        .fold(0.0, f64::max)
                        .max(1.0) * 1.2;
                    let grid = y_grid(&p, max, 4, &|v| format!("{}", v.round()));
                    let mut lines: Vec<AnyView> = Vec::new();
                    for (k, &si) in locals.iter().enumerate() {
                        let pts: Vec<(f64, f64)> = (0..n)
                            .filter_map(|b| tok_s(b, si).map(|v| (p.band_mid(b, n), p.y(v, max))))
                            .collect();
                        if pts.is_empty() {
                            continue;
                        }
                        let color = color(si);
                        let len = path_len(&pts);
                        let cls = if anim { "anim-draw" } else { "" };
                        let style = format!("--len:{len:.0};animation-delay:{}ms", k * 40);
                        let (lx, ly) = pts[pts.len() - 1];
                        let lv = (0..n).rev().find_map(|b| tok_s(b, si)).unwrap_or(0.0);
                        lines
                            .push(
                                view! {
                                    <path
                                        d=line_path(&pts)
                                        class=cls
                                        style=style
                                        fill="none"
                                        stroke=color
                                        stroke-width="2"
                                        stroke-linejoin="round"
                                        stroke-linecap="round"
                                    ></path>
                                    <circle
                                        cx=lx
                                        cy=ly
                                        r="4"
                                        fill=color
                                        stroke="var(--surface)"
                                        stroke-width="2"
                                    ></circle>
                                    <text x=lx + 9.0 y=ly + 4.0 class="mark-label">
                                        {format!("{:.0} tok/s", lv)}
                                    </text>
                                }
                                    .into_any(),
                            );
                    }
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|b| {
                            Tip::new(
                                bucket_full(&g.buckets[b]),
                                locals
                                    .iter()
                                    .map(|&si| {
                                        TipRow::new(
                                            color(si),
                                            g.series[si].label.clone(),
                                            tok_s(b, si)
                                                .map(|v| format!("{v:.1} tok/s"))
                                                .unwrap_or_else(|| "—".into()),
                                        )
                                    })
                                    .collect(),
                            )
                        })
                        .collect();
                    let leg: Vec<LegendItem> = locals
                        .iter()
                        .map(|&si| {
                            LegendItem::rule(color(si), g.series[si].label.clone())
                        })
                        .collect();
                    let mut headers = vec!["Bucket".to_string()];
                    headers.extend(locals.iter().map(|&si| g.series[si].label.clone()));
                    let rows: Vec<Vec<String>> = (0..n)
                        .map(|b| {
                            let mut row = vec![bucket_full(&g.buckets[b])];
                            row.extend(
                                locals
                                    .iter()
                                    .map(|&si| {
                                        tok_s(b, si)
                                            .map(|v| format!("{v:.1}"))
                                            .unwrap_or_else(|| "—".into())
                                    }),
                            );
                            row
                        })
                        .collect();
                    let l = local.data.get();
                    // One line, not a strip of three tiles: the numbers are
                    // the window's local totals, the tooltip says what each is.
                    let fact = |label: &'static str, v: Option<String>, tip: &'static str| {
                        view! {
                            <span class="fact-inline" title=tip>
                                {label}
                                " "
                                <b>{v.unwrap_or_else(|| "—".into())}</b>
                            </span>
                        }
                            .into_any()
                    };
                    let chart = || {
                        view! {
                        {legend(leg)}
                        <svg width=p.w height=p.h viewBox=p.view_box()>
                            {grid} {lines} {x_labels(&p, &g.labels(), g.every(p.iw()))}
                            {crosshair(&p, hover, n)} {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, headers, rows)}
                        <div class="chart-foot facts">
                            {fact(
                                "KV cache reuse",
                                l.as_ref().and_then(|l| l.kv_reuse).map(pct),
                                "of prompt tokens never recomputed",
                            )}
                            {fact(
                                "draft acceptance",
                                l.as_ref().and_then(|l| l.draft_acceptance).map(pct),
                                "speculative-decoding tokens kept",
                            )}
                            {fact(
                                "decode",
                                l
                                    .as_ref()
                                    .and_then(|l| l.decode_tok_s)
                                    .map(|v| format!("{v:.1} tok/s")),
                                "all local models together, Σtokens / Σms",
                            )}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// 9 · when (weekday × hour heatmap)
// ---------------------------------------------------------------------------

#[component]
pub(super) fn HeatCard(heat: Src<UsageHeatResponse>, tips: Tips) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let tv = RwSignal::new(false);
    view! {
        <div class="card chart-card u-heat">
            <ChartHead
                title="When"
                note="requests summed over the window · weekday × hour, local time — a cell is every such hour added up, not a rate"
                tv=tv
            />
            <CardErr err=heat.err/>
            <div class="chart-box" node_ref=node class:stale=move || heat.stale()>
                {move || {
                    let w = width.get();
                    let Some(r) = heat.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    // dow 0 = Sunday on the wire; the grid reads Mon..Sun.
                    let mut cells = [[0i64; 24]; 7];
                    for c in &r.cells {
                        let row = (((c.dow + 6) % 7).max(0) as usize).min(6);
                        let h = (c.hour.max(0) as usize).min(23);
                        cells[row][h] = c.requests;
                    }
                    let max = r.max.max(1) as f64;
                    let p = Plot::new(w, chart_height(w), 34.0, 6.0, 10.0, 28.0);
                    let cw = p.iw() / 24.0;
                    let ch = p.ih() / 7.0;
                    let mut grid_cells: Vec<AnyView> = Vec::new();
                    for (row, name) in DOWS.iter().enumerate() {
                        grid_cells
                            .push(
                                view! {
                                    <text
                                        x=p.l - 7.0
                                        y=p.t + row as f64 * ch + ch / 2.0 + 3.5
                                        class="ax"
                                        text-anchor="end"
                                    >
                                        {*name}
                                    </text>
                                }
                                    .into_any(),
                            );
                        for h in 0..24usize {
                            let v = cells[row][h];
                            let step = charts::seq_step(v as f64, max);
                            let fill = charts::SEQ[step];
                            let tip = Tip::new(
                                format!("{name} {h:02}:00"),
                                vec![
                                    TipRow::new(fill, "requests (sum over window)", grouped(v.max(0) as u64)),
                                ],
                            );
                            let ctl = tips;
                            grid_cells
                                .push(
                                    view! {
                                        <rect
                                            x=p.l + h as f64 * cw + 1.0
                                            y=p.t + row as f64 * ch + 1.0
                                            width=(cw - 2.0).max(1.0)
                                            height=(ch - 2.0).max(1.0)
                                            rx="2.5"
                                            fill=fill
                                            class="cell"
                                            on:mousemove=move |ev: web_sys::MouseEvent| {
                                                ctl.show(&ev, tip.clone())
                                            }
                                            on:mouseleave=move |_| ctl.hide()
                                        ></rect>
                                    }
                                        .into_any(),
                                );
                        }
                    }
                    let hour_labels: Vec<AnyView> = [0usize, 6, 12, 18]
                        .into_iter()
                        .map(|h| {
                            view! {
                                <text
                                    x=p.l + h as f64 * cw + cw / 2.0
                                    y=p.h - 14.0
                                    class="ax"
                                    text-anchor="middle"
                                >
                                    {format!("{h:02}")}
                                </text>
                            }
                                .into_any()
                        })
                        .collect();
                    let swatches: Vec<AnyView> = charts::SEQ[1..]
                        .iter()
                        .map(|c| view! { <i style=format!("background:{c}")></i> }.into_any())
                        .collect();
                    let all: i64 = cells.iter().flatten().sum();
                    let weekend: i64 = cells[5].iter().chain(cells[6].iter()).sum();
                    let night: i64 = (0..7)
                        .flat_map(|r| (0..6).map(move |h| (r, h)))
                        .map(|(r, h)| cells[r][h])
                        .sum();
                    let best = (0..7)
                        .flat_map(|r| (0..24).map(move |h| (r, h)))
                        .max_by_key(|(r, h)| cells[*r][*h])
                        .unwrap_or((0, 0));
                    // Hours down, days across: 24 short rows read better in a
                    // card than 24 columns do.
                    let rows: Vec<Vec<String>> = (0..24)
                        .map(|h| {
                            let mut row = vec![format!("{h:02}:00")];
                            row.extend((0..7).map(|d| cells[d][h].to_string()));
                            row
                        })
                        .collect();
                    let mut headers = vec!["Hour".to_string()];
                    headers.extend(DOWS.iter().map(|d| d.to_string()));
                    let chart = || {
                        view! {
                            <svg width=p.w height=p.h viewBox=p.view_box()>
                                {grid_cells}
                                {hour_labels}
                            </svg>
                            <div class="heat-scale">
                                "0" {swatches}
                                {format!("{} requests (sum over window)", grouped(r.max.max(0) as u64))}
                            </div>
                        }
                            .into_any()
                    };
                    let share = |part: i64| pct(if all > 0 { part as f64 / all as f64 } else { 0.0 });
                    view! {
                        {chart_or_table(tv, chart, headers, rows)}
                        <div class="chart-foot facts">
                            <span class="fact-inline" title="the weekday hour with the most requests, summed over the window">
                                "busiest "
                                <b>{format!("{} {:02}:00", DOWS[best.0], best.1)}</b>
                                {format!(" ({})", grouped(cells[best.0][best.1].max(0) as u64))}
                            </span>
                            <span class="fact-inline" title="share of all requests between 00:00 and 06:00">
                                "overnight " <b>{share(night)}</b>
                            </span>
                            <span class="fact-inline" title="share of all requests on Saturday and Sunday">
                                "weekend " <b>{share(weekend)}</b>
                            </span>
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}
