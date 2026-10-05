use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::{KeysResponse, UsageSeriesResponse};

use crate::charts::{
    area_path, chart_height, crosshair, hit_bands, legend, line_path, path_len, rect_path,
    top_round, x_labels, y_grid, LegendItem, Plot, Tip, TipRow, Tips,
};
use crate::fmt::{compact, grouped, pct};

use super::*;

// ---------------------------------------------------------------------------
// 2 · spend over time
// ---------------------------------------------------------------------------

#[component]
pub(super) fn SpendCard(
    series: Src<UsageSeriesResponse>,
    group_by: Signal<String>,
    slots: Memo<HashMap<String, u8>>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    // The owner asked for the plot although nothing in it has a price.
    let draw_anyway = RwSignal::new(false);
    let note = Signal::derive(move || {
        let g = group_by.get();
        let how = if g == "none" {
            "not stacked".to_string()
        } else {
            format!("stacked by {g}")
        };
        format!("{how} · local models are free_local and never appear here")
    });
    view! {
        <div class="card chart-card u-spend">
            <ChartHead title="Spend over time" note=move || note.get() tv=tv/>
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
                        return view! {
                            <div class="chart-empty">"No buckets in this window."</div>
                        }
                            .into_any();
                    }
                    // Nothing in the window has a price: a full-size plot of
                    // $0 bars would be the page's biggest area saying nothing.
                    // It says why, and where that is fixed; the plot is one
                    // click away and the Table view keeps every bucket.
                    if r.totals.cost_micro == 0 && !tv.get() && !draw_anyway.get() {
                        let t = &r.totals;
                        let why = if t.cost_unknown_requests > 0 {
                            view! {
                                "Nothing in this window has a price: "
                                <b>
                                    {format!(
                                        "{} requests",
                                        grouped(t.cost_unknown_requests.max(0) as u64),
                                    )}
                                </b>
                                {format!(
                                    " ({} tokens) unpriced, local ones are free. ",
                                    compact(t.cost_unknown_tokens.max(0) as f64),
                                )}
                                <a href="/usage/prices">"Price them on Prices →"</a>
                            }
                                .into_any()
                        } else if t.requests > 0 {
                            "Nothing was spent in this window: every request was free (local, or priced at 0)."
                                .into_any()
                        } else {
                            "No requests in this window.".into_any()
                        };
                        return view! {
                            <div class="chart-zero">
                                <span class="chart-zero-sum">
                                    {money(0, &r.currency)} " in this window"
                                </span>
                                <span class="chart-zero-why">{why}</span>
                                <button
                                    class="link-btn"
                                    title="Draw the chart anyway: every bar $0, the unpriced requests as the hatched rail"
                                    on:click=move |_| draw_anyway.set(true)
                                >
                                    "Draw it anyway"
                                </button>
                            </div>
                        }
                            .into_any();
                    }
                    let cur = r.currency.clone();
                    let anim = entry.get_untracked();
                    let sl = slots.get();
                    let gb = group_by.get_untracked();
                    let color = |si: usize| series_color(&sl, &g.series[si]);
                    // A local model's money cost is a real zero: it cannot be a
                    // segment here, only a missing one.
                    let vis: Vec<usize> = (0..g.series.len())
                        .filter(|i| !g.series[*i].local)
                        .collect();
                    let p = Plot::new(w, chart_height(w), 56.0, 16.0, 12.0, 34.0);
                    let max = g
                        .totals
                        .iter()
                        .map(|t| t.cost_micro as f64)
                        .fold(0.0, f64::max)
                        .max(MIN_MONEY_SCALE) * 1.12;
                    let bw = p.bar_w(n, 0.62);
                    let peak = g
                        .totals
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, t)| t.cost_micro)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let grid = y_grid(&p, max, 4, &money_ticks(max, &cur));
                    let mut cols: Vec<AnyView> = Vec::with_capacity(n);
                    let mut rails: Vec<AnyView> = Vec::new();
                    for b in 0..n {
                        let x = p.band_x(b, n) + (p.band_w(n) - bw) / 2.0;
                        let mut acc = 0.0_f64;
                        let mut segs: Vec<AnyView> = Vec::new();
                        let mut placed = 0usize;
                        for (pos, &si) in vis.iter().enumerate() {
                            let v = g.cells[b][si].cost_micro as f64;
                            if v <= 0.0 {
                                continue;
                            }
                            let h_full = if max > 0.0 { (v / max) * p.ih() } else { 0.0 };
                            let y = p.y(acc + v, max);
                            let is_top = vis[pos + 1..]
                                .iter()
                                .all(|&j| g.cells[b][j].cost_micro <= 0);
                            // 2px surface gap between touching fills
                            let h = if placed > 0 { (h_full - 2.0).max(1.0) } else { h_full.max(1.0) };
                            let d = if is_top {
                                top_round(x, y, bw, h, 4.0)
                            } else {
                                rect_path(x, y, bw, h)
                            };
                            segs.push(
                                view! { <path d=d fill=color(si)></path> }
                                    .into_any(),
                            );
                            acc += v;
                            placed += 1;
                        }
                        // The honesty rail: an unpriced remainder is drawn as a
                        // neutral hatch off the axis, never as a zero-height
                        // segment of the stack (design §2.3).
                        if g.totals[b].cost_unknown_requests > 0 {
                            rails.push(
                                view! {
                                    <rect
                                        x=x
                                        y=p.t + p.ih() + 5.0
                                        width=bw
                                        height="5"
                                        rx="2"
                                        fill="url(#usage-hatch)"
                                    ></rect>
                                }
                                    .into_any(),
                            );
                        }
                        if b == peak && g.totals[b].cost_micro > 0 {
                            segs.push(
                                view! {
                                    <text
                                        x=x + bw / 2.0
                                        y=p.y(g.totals[b].cost_micro as f64, max) - 7.0
                                        class="mark-label"
                                        text-anchor="middle"
                                    >
                                        {money(g.totals[b].cost_micro, &cur)}
                                    </text>
                                }
                                    .into_any(),
                            );
                        }
                        let style = if anim {
                            format!("animation-delay:{}ms", b * 9)
                        } else {
                            String::new()
                        };
                        // The entry grow and the live tick are two different
                        // statements; never both on the same column.
                        let acls = if anim {
                            "anim-col"
                        } else if b + 1 == n {
                            "tick"
                        } else {
                            ""
                        };
                        cols.push(view! { <g class=acls style=style>{segs}</g> }.into_any());
                    }
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|b| {
                            let mut rows: Vec<TipRow> = vis
                                .iter()
                                .filter(|&&si| g.cells[b][si].cost_micro > 0)
                                .map(|&si| {
                                    TipRow::new(
                                        color(si),
                                        series_label(&g.series[si], &gb),
                                        money(g.cells[b][si].cost_micro, &cur),
                                    )
                                })
                                .collect();
                            if g.totals[b].cost_unknown_requests > 0 {
                                rows.push(
                                    TipRow::new(
                                        "var(--text-3)",
                                        "unpriced",
                                        format!("{} req", g.totals[b].cost_unknown_requests),
                                    ),
                                );
                            }
                            Tip::new(bucket_full(&g.buckets[b]), rows)
                                .with_total("total", money(g.totals[b].cost_micro, &cur))
                        })
                        .collect();
                    let mut leg: Vec<LegendItem> = vis
                        .iter()
                        .map(|&si| {
                            LegendItem::fill(color(si), series_label(&g.series[si], &gb))
                        })
                        .collect();
                    leg.push(LegendItem::fill("var(--text-3)", "unpriced (no price known)"));
                    let mut headers = vec!["Bucket".to_string()];
                    headers.extend(vis.iter().map(|&si| series_label(&g.series[si], &gb)));
                    headers.push("Unpriced req".into());
                    headers.push("Total".into());
                    let rows: Vec<Vec<String>> = (0..n)
                        .map(|b| {
                            let mut row = vec![bucket_full(&g.buckets[b])];
                            row.extend(
                                vis.iter().map(|&si| money(g.cells[b][si].cost_micro, &cur)),
                            );
                            row.push(g.totals[b].cost_unknown_requests.to_string());
                            row.push(money(g.totals[b].cost_micro, &cur));
                            row
                        })
                        .collect();
                    let total_unpriced = unpriced_note(
                        r.totals.cost_unknown_requests,
                        r.totals.cost_unknown_tokens,
                    );
                    let chart = || {
                        view! {
                        {legend(leg)}
                        <svg width=p.w height=p.h viewBox=p.view_box()>
                            <defs>
                                <pattern
                                    id="usage-hatch"
                                    width="6"
                                    height="6"
                                    patternUnits="userSpaceOnUse"
                                    patternTransform="rotate(45)"
                                >
                                    <rect width="6" height="6" fill="var(--surface)"></rect>
                                    <line
                                        x1="0"
                                        y1="0"
                                        x2="0"
                                        y2="6"
                                        stroke="var(--text-3)"
                                        stroke-width="2.4"
                                    ></line>
                                </pattern>
                            </defs>
                            {grid}
                            {cols}
                            {rails}
                            {x_labels(&p, &g.labels(), g.every(p.iw()))}
                            {crosshair(&p, hover, n)}
                            {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, headers, rows)}
                        <div class="chart-foot">
                            {money(r.totals.cost_micro, &cur)}
                            " in this window"
                            {total_unpriced.map(|n| format!(" · {n}"))}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// 3 · cumulative vs budget
// ---------------------------------------------------------------------------

#[component]
pub(super) fn BudgetCard(
    series: Src<UsageSeriesResponse>,
    keys: Src<KeysResponse>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    let draw_anyway = RwSignal::new(false);
    view! {
        <div class="card chart-card u-budget">
            <ChartHead title="This period vs budget" note="dashed = linear projection" tv=tv/>
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
                    let cur = r.currency.clone();
                    let anim = entry.get_untracked();
                    let budget = keys.data.get().map(|k| k.global_budget_micro).unwrap_or(0);
                    let mut acc = 0i64;
                    let cum: Vec<i64> = g
                        .totals
                        .iter()
                        .map(|t| {
                            acc += t.cost_micro;
                            acc
                        })
                        .collect();
                    let last = *cum.last().unwrap_or(&0);
                    // Project the observed rate to the end of the current
                    // calendar month; dashing means projection here, which is
                    // why gridlines are solid everywhere else.
                    let period = keys
                        .data
                        .get()
                        .map(|k| k.global_budget_period)
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "month".into());
                    let (proj, proj_to) = project(last, n, &r.buckets, &period);
                    let max = (budget as f64)
                        .max(proj as f64)
                        .max(last as f64)
                        .max(MIN_MONEY_SCALE) * 1.1;
                    let p = Plot::new(w, chart_height(w), 56.0, 46.0, 12.0, 34.0);
                    let grid = y_grid(&p, max, 4, &money_ticks(max, &cur));
                    // The cumulative line stops short of the right edge so the
                    // projection has room to run without leaving the frame.
                    let lp = Plot::new(p.w, p.h, p.l, p.r + p.iw() * 0.22, p.t, p.b);
                    let pts: Vec<(f64, f64)> = cum
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (lp.lin_x(i, n), p.y(*v as f64, max)))
                        .collect();
                    let len = path_len(&pts);
                    let draw_cls = if anim { "anim-draw" } else { "" };
                    let budget_rule = (budget > 0)
                        .then(|| {
                            let y = p.y(budget as f64, max);
                            view! {
                                <line
                                    x1=p.l
                                    x2=p.right()
                                    y1=y
                                    y2=y
                                    stroke="var(--amber)"
                                    stroke-width="1.5"
                                ></line>
                                <text x=p.l + 4.0 y=y - 6.0 class="mark-label">
                                    {format!("budget {}", money(budget, &cur))}
                                </text>
                            }
                                .into_any()
                        });
                    let proj_view = (proj > last)
                        .then(|| {
                            let x0 = lp.lin_x(n.saturating_sub(1), n);
                            view! {
                                <path
                                    d=format!(
                                        "M{:.2},{:.2}L{:.2},{:.2}",
                                        x0,
                                        p.y(last as f64, max),
                                        p.right(),
                                        p.y(proj as f64, max),
                                    )
                                    fill="none"
                                    stroke="var(--c1)"
                                    stroke-width="2"
                                    stroke-dasharray="5 5"
                                    stroke-opacity="0.55"
                                    stroke-linecap="round"
                                ></path>
                                <text
                                    x=p.right()
                                    y=(p.y(proj as f64, max) - 8.0).max(p.t + 10.0)
                                    class="ax"
                                    text-anchor="end"
                                >
                                    {format!("proj. {}", money(proj, &cur))}
                                </text>
                            }
                                .into_any()
                        });
                    let (lx, ly) = *pts.last().unwrap_or(&(p.l, p.y(0.0, max)));
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|i| {
                            let mut rows = vec![
                                TipRow::new("var(--c1)", "cumulative", money(cum[i], &cur)),
                            ];
                            if budget > 0 {
                                rows.push(
                                    TipRow::new(
                                        "var(--amber)",
                                        "of budget",
                                        pct(cum[i] as f64 / budget as f64),
                                    ),
                                );
                            }
                            Tip::new(bucket_full(&g.buckets[i]), rows)
                        })
                        .collect();
                    let global = keys.data.get();
                    let chart = || {
                        view! {
                        {legend(
                            vec![
                                LegendItem::rule("var(--c1)", "cumulative spend"),
                                LegendItem::rule("var(--amber)", "budget"),
                            ],
                        )}
                        <svg width=p.w height=p.h viewBox=p.view_box()>
                            {grid}
                            {budget_rule}
                            <path
                                d=area_path(&pts, p.bottom())
                                fill="var(--c1)"
                                fill-opacity="0.10"
                            ></path>
                            <path
                                d=line_path(&pts)
                                class=draw_cls
                                style=format!("--len:{len:.0}")
                                fill="none"
                                stroke="var(--c1)"
                                stroke-width="2"
                                stroke-linejoin="round"
                                stroke-linecap="round"
                            ></path>
                            {proj_view}
                            <circle
                                cx=lx
                                cy=ly
                                r="4"
                                fill="var(--c1)"
                                stroke="var(--surface)"
                                stroke-width="2"
                            ></circle>
                            <text x=lx - 6.0 y=ly - 9.0 class="mark-label" text-anchor="end">
                                {money(last, &cur)}
                            </text>
                            {x_labels(&p, &g.labels(), (n / 3).max(1))}
                            {crosshair(&p, hover, n)}
                            {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    // Nothing spent in the window: a flat line along $0 under
                    // the budget rule says less than the foot below it.
                    let flat = last == 0 && !tv.get() && !draw_anyway.get();
                    let body = if flat {
                        view! {
                            <div class="chart-zero">
                                <span class="chart-zero-why">
                                    "Nothing spent in this window, so there is no curve to draw."
                                </span>
                                <button class="link-btn" on:click=move |_| draw_anyway.set(true)>
                                    "Draw it anyway"
                                </button>
                            </div>
                        }
                            .into_any()
                    } else {
                        chart_or_table(tv, chart, heads(&["Bucket", "Spent", "Cumulative", "Of budget"]),
                            (0..n)
                                .map(|i| {
                                    vec![
                                        bucket_full(&g.buckets[i]),
                                        money(g.totals[i].cost_micro, &cur),
                                        money(cum[i], &cur),
                                        if budget > 0 {
                                            pct(cum[i] as f64 / budget as f64)
                                        } else {
                                            "—".into()
                                        },
                                    ]
                                })
                                .collect(),)
                    };
                    view! {
                        {body}
                        <div class="chart-foot">
                            // The line above is the *window's* cumulative spend;
                            // this is the budget period's own, which is the figure
                            // that actually answers "am I over".
                            {match global {
                                Some(k) if k.global_budget_micro > 0 => {
                                    let over = k.global_spent_micro - k.global_budget_micro;
                                    let line = if over > 0 {
                                        format!(
                                            "{} of {} this {} — over by {}",
                                            money(k.global_spent_micro, &k.currency),
                                            money(k.global_budget_micro, &k.currency),
                                            period_word(&k.global_budget_period),
                                            money(over, &k.currency),
                                        )
                                    } else {
                                        format!(
                                            "{} of {} this {}",
                                            money(k.global_spent_micro, &k.currency),
                                            money(k.global_budget_micro, &k.currency),
                                            period_word(&k.global_budget_period),
                                        )
                                    };
                                    let rem = (k.global_spent_unknown_requests > 0)
                                        .then(|| {
                                            format!(
                                                " ({} requests against it unpriced)",
                                                grouped(
                                                    k.global_spent_unknown_requests.max(0) as u64,
                                                ),
                                            )
                                        });
                                    view! { "Global budget: " {line} {rem} "." }.into_any()
                                }
                                _ => {
                                    view! {
                                        "No global budget set — "
                                        <a href=crate::pages::settings_href("global_budget_micro")>
                                            "Settings → Usage & cost"
                                        </a>
                                        "."
                                    }
                                        .into_any()
                                }
                            }}
                            {(!proj_to.is_empty())
                                .then(|| {
                                    let mut t = proj_to.clone();
                                    if let Some(c) = t.get_mut(0..1) {
                                        c.make_ascii_uppercase();
                                    }
                                    format!(" {t}.")
                                })}
                            {unpriced_note(
                                    r.totals.cost_unknown_requests,
                                    r.totals.cost_unknown_tokens,
                                )
                                .map(|n| format!(" This window: {n}."))}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

/// Linear projection of the window's cumulative spend to the end of the
/// **budget's** period, at the rate the window observed. Returns the projected
/// total (equal to `cum` when there is nothing to project to) and a phrase
/// naming the horizon — or naming why there isn't one.
///
/// `total` has no period end, and a projection to an imaginary one would be
/// the page inventing a number; it says so instead. A `day` budget against a
/// multi-day window is the same problem one step down: the cumulative line
/// covers more than one allowance, so the rule below is a reference and the
/// period's own spend (stated in the card's foot) is the real comparison.
fn project(cum: i64, n: usize, buckets: &[String], period: &str) -> (i64, String) {
    if n == 0 || cum <= 0 {
        return (cum, String::new());
    }
    let last = buckets.last().map(|b| b.len()).unwrap_or(10);
    let (hourly, monthly) = (last == 13, last == 7);
    if period == "total" {
        return (
            cum,
            "no period end — this budget is a lifetime total".into(),
        );
    }
    if monthly {
        return (cum, String::new());
    }
    if period == "day" && !hourly {
        return (
            cum,
            "the budget is per day and this window spans several — the figure below is the day's own spend".into(),
        );
    }
    let d = js_sys::Date::new_0();
    let rate = cum as f64 / n as f64;
    if hourly {
        // Hour buckets: the horizon is the end of today either way, because a
        // month-long projection off a 24-hour window is not a projection.
        let remaining = (24 - d.get_hours() as i64).max(0) as f64;
        return (
            (cum as f64 + rate * remaining).round() as i64,
            "projected to midnight".into(),
        );
    }
    let month_days = days_in_month(d.get_full_year() as i32, d.get_month());
    let remaining = (month_days as i64 - d.get_date() as i64).max(0) as f64;
    (
        (cum as f64 + rate * remaining).round() as i64,
        format!(
            "projected to {} {}",
            month_days,
            MONTHS[(d.get_month() as usize).min(11)]
        ),
    )
}

/// `day` | `month` | `total` as it reads in a sentence.
pub(super) fn period_word(period: &str) -> &str {
    match period {
        "day" => "day",
        "total" => "budget, in total",
        _ => "month",
    }
}

fn days_in_month(y: i32, m0: u32) -> u32 {
    match m0 {
        0 | 2 | 4 | 6 | 7 | 9 | 11 => 31,
        3 | 5 | 8 | 10 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}
