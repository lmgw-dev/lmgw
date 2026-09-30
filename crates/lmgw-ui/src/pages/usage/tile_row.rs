use leptos::prelude::*;
use lmgw_api_types::{UsageCell, UsageLocalResponse, UsageSeriesResponse};

use crate::charts::{self, sparkline};
use crate::fmt::{compact, grouped, pct};

use super::*;

// ---------------------------------------------------------------------------
// 1 · tiles
// ---------------------------------------------------------------------------

/// A delta's colour is direction **times** whether up is good: a rising
/// request count is not a red number. `good_up = None` means neutral.
fn delta_span(d: Option<f64>, good_up: Option<bool>, note: &str) -> AnyView {
    let Some(d) = d.filter(|d| d.is_finite()) else {
        return view! { <span class="dim">"no previous window"</span> }.into_any();
    };
    let moved = d.abs() > 0.005;
    let cls = match good_up {
        Some(up) if moved => {
            if (d > 0.0) == up {
                "delta good"
            } else {
                "delta bad"
            }
        }
        _ => "delta flat",
    };
    let sign = if d > 0.0 { "+" } else { "" };
    let note = note.to_string();
    view! {
        <span class=cls>{format!("{sign}{:.0}%", d * 100.0)}</span>
        " "
        <span class="dim">{note}</span>
    }
    .into_any()
}

fn ratio(now: f64, prev: f64) -> Option<f64> {
    (prev.abs() > f64::EPSILON).then(|| (now - prev) / prev)
}

#[component]
pub(super) fn TileRow(
    series: Src<UsageSeriesResponse>,
    by_alias: Src<UsageSeriesResponse>,
    local: Src<UsageLocalResponse>,
) -> impl IntoView {
    let currency = Memo::new(move |_| {
        series
            .data
            .get()
            .map(|r| r.currency)
            .unwrap_or_else(|| "EUR".into())
    });
    let totals = Memo::new(move |_| series.data.get().map(|r| r.totals).unwrap_or_default());
    let prev = Memo::new(move |_| series.data.get().and_then(|r| r.previous));
    let sparks = Memo::new(move |_| {
        series
            .data
            .get()
            .map(|r| Grid::build(&r).totals)
            .unwrap_or_default()
    });

    let spend = count_of(move || totals.get().cost_micro as f64 / 1e6);
    let reqs = count_of(move || totals.get().requests as f64);
    let tok_in = count_of(move || totals.get().tokens_in as f64);
    let p95 = count_of(move || {
        series
            .data
            .get()
            .and_then(|r| r.p95_total_ms)
            .unwrap_or(0.0)
    });
    let errs = count_of(move || {
        let t = totals.get();
        if t.requests > 0 {
            (t.errors + t.refusals) as f64 / t.requests as f64
        } else {
            0.0
        }
    });
    let share = count_of(move || {
        local
            .data
            .get()
            .map(|l| {
                let tot = (l.local_tokens + l.cloud_tokens) as f64;
                if tot > 0.0 {
                    l.local_tokens as f64 / tot
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
    });

    let spark = move |f: fn(&UsageCell) -> f64| {
        Signal::derive(move || sparks.get().iter().map(f).collect::<Vec<f64>>())
    };
    let s_cost = spark(|c| c.cost_micro as f64);
    let s_req = spark(|c| c.requests as f64);
    let s_tok = spark(|c| (c.tokens_in + c.tokens_out) as f64);
    let s_lat = spark(|c| mean_ms(c.total_sum, c.total_count).unwrap_or(0.0));
    let s_err = spark(|c| (c.errors + c.refusals) as f64);
    // The local tile's sparkline needs the per-bucket local split, which only
    // the alias grouping carries — whatever the page is grouped by.
    let s_local = Memo::new(move |_| {
        let Some(r) = by_alias.data.get() else {
            return Vec::<f64>::new();
        };
        let g = Grid::build(&r);
        let li: Vec<usize> = (0..g.series.len()).filter(|i| g.series[*i].local).collect();
        (0..g.nb())
            .map(|b| {
                li.iter()
                    .map(|&si| (g.cells[b][si].tokens_in + g.cells[b][si].tokens_out) as f64)
                    .sum()
            })
            .collect()
    });

    view! {
        <div class="tiles">
            <div class="card tile hero anim-fade">
                <div class="tile-label">"Spend"</div>
                <div class="tile-value">{move || money((spend.get() * 1e6) as i64, &currency.get())}</div>
                // Unknown is not zero (§2.3), and at the weight of the figure
                // it qualifies: "$0.00" over a window that is 98 % unpriced
                // says nothing about what was spent.
                <div
                    class="hero-unpriced"
                    title=move || {
                        let t = totals.get();
                        unpriced_note(t.cost_unknown_requests, t.cost_unknown_tokens)
                            .map(|n| format!("{n} — their cost is unknown, not zero"))
                            .unwrap_or_else(|| "every request in this window was priced".into())
                    }
                >
                    {move || {
                        let t = totals.get();
                        if t.cost_unknown_requests <= 0 {
                            return view! { "every request priced" }.into_any();
                        }
                        let share = if t.requests > 0 {
                            t.cost_unknown_requests as f64 / t.requests as f64
                        } else {
                            1.0
                        };
                        view! { <b>{pct(share)}</b> " of requests unpriced" }.into_any()
                    }}
                </div>
                <div class="tile-sub">
                    {move || {
                        delta_span(
                            prev
                                .get()
                                .and_then(|p| {
                                    ratio(totals.get().cost_micro as f64, p.cost_micro as f64)
                                }),
                            Some(false),
                            "vs previous window",
                        )
                    }}
                </div>
                <Spark vals=s_cost color="var(--c1)"/>
            </div>

            <div class="card tile anim-fade">
                <div class="tile-label">"Requests"</div>
                <div class="tile-value">{move || compact(reqs.get())}</div>
                <div class="tile-sub" title="cloud + local, every class the filter row selects">
                    "cloud + local"
                </div>
                <div class="tile-sub">
                    {move || {
                        delta_span(
                            prev
                                .get()
                                .and_then(|p| ratio(totals.get().requests as f64, p.requests as f64)),
                            None,
                            "vs previous window",
                        )
                    }}
                </div>
                <Spark vals=s_req color="var(--c1)"/>
            </div>

            <div class="card tile anim-fade">
                <div class="tile-label">"Tokens in / out"</div>
                <div class="tile-value">{move || compact(tok_in.get())}</div>
                // One line; a narrow tile clips it, the tooltip has it whole.
                <div
                    class="tile-sub"
                    title=move || {
                        let t = totals.get();
                        format!(
                            "out {} · in:out {} · cache {} read, {} written",
                            compact(t.tokens_out as f64),
                            if t.tokens_out > 0 {
                                format!("{:.1} : 1", t.tokens_in as f64 / t.tokens_out as f64)
                            } else {
                                "—".into()
                            },
                            compact(t.tokens_cached as f64),
                            compact(t.tokens_cache_write as f64),
                        )
                    }
                >
                    {move || {
                        let t = totals.get();
                        let r = if t.tokens_out > 0 {
                            format!("{:.1} : 1", t.tokens_in as f64 / t.tokens_out as f64)
                        } else {
                            "—".into()
                        };
                        // The dearest input tier is the one worth naming: a
                        // window whose spend jumped because it re-wrote the
                        // cache reads as an unexplained jump without it.
                        let cache = (t.tokens_cached + t.tokens_cache_write > 0)
                            .then(|| {
                                view! {
                                    " · cache "
                                    <b>{compact(t.tokens_cached as f64)}</b>
                                    " read · "
                                    <b>{compact(t.tokens_cache_write as f64)}</b>
                                    " written"
                                }
                            });
                        view! {
                            "out " <b>{compact(t.tokens_out as f64)}</b> " · ratio " {r} {cache}
                        }
                    }}
                </div>
                <div class="tile-sub">
                    {move || {
                        delta_span(
                            prev
                                .get()
                                .and_then(|p| {
                                    ratio(totals.get().tokens_in as f64, p.tokens_in as f64)
                                }),
                            None,
                            "vs previous window",
                        )
                    }}
                </div>
                <Spark vals=s_tok color="var(--c1)"/>
            </div>

            <div class="card tile anim-fade">
                <div class="tile-label">"p95 total"</div>
                <div class="tile-value">
                    {move || {
                        let r = series.data.get();
                        match r.as_ref().and_then(|r| r.p95_total_ms) {
                            Some(_) => format!("{} ms", p95.get().round()),
                            None => "—".to_string(),
                        }
                    }}
                </div>
                <div
                    class="tile-sub"
                    title="p50 of every request's total time, and the p95 of time to first byte"
                >
                    {move || {
                        let r = series.data.get();
                        let f = |v: Option<f64>| {
                            v.map(|v| format!("{} ms", v.round())).unwrap_or_else(|| "—".into())
                        };
                        format!(
                            "p50 {} · TTFB p95 {}",
                            f(r.as_ref().and_then(|r| r.p50_total_ms)),
                            f(r.as_ref().and_then(|r| r.p95_ttfb_ms)),
                        )
                    }}
                </div>
                <div class="tile-sub">
                    {move || {
                        let now = mean_ms(totals.get().total_sum, totals.get().total_count);
                        let was = prev.get().and_then(|p| mean_ms(p.total_sum, p.total_count));
                        delta_span(
                            now.zip(was).and_then(|(a, b)| ratio(a, b)),
                            Some(false),
                            "mean vs previous",
                        )
                    }}
                </div>
                <Spark vals=s_lat color="var(--amber)"/>
            </div>

            <div class="card tile anim-fade">
                <div class="tile-label">"Error + refusal rate"</div>
                <div class="tile-value">{move || pct(errs.get())}</div>
                <div class="tile-sub">
                    {move || {
                        let t = totals.get();
                        format!(
                            "{} of {} requests",
                            grouped((t.errors + t.refusals).max(0) as u64),
                            compact(t.requests as f64),
                        )
                    }}
                </div>
                <div class="tile-sub">
                    {move || {
                        let t = totals.get();
                        let now = if t.requests > 0 {
                            Some((t.errors + t.refusals) as f64 / t.requests as f64)
                        } else {
                            None
                        };
                        let was = prev
                            .get()
                            .filter(|p| p.requests > 0)
                            .map(|p| (p.errors + p.refusals) as f64 / p.requests as f64);
                        delta_span(
                            now.zip(was).and_then(|(a, b)| ratio(a, b)),
                            Some(false),
                            "vs previous window",
                        )
                    }}
                </div>
                <Spark vals=s_err color="var(--err)"/>
            </div>

            <div class="card tile anim-fade">
                <div class="tile-label">"Served locally"</div>
                <div class="tile-value">
                    {move || if local.data.with(|d| d.is_some()) { pct(share.get()) } else { "—".into() }}
                </div>
                <div class="tile-sub">
                    {move || {
                        local
                            .data
                            .get()
                            .map(|l| {
                                view! {
                                    <b>{compact(l.local_tokens as f64)}</b>
                                    " tokens never left the house"
                                }
                                    .into_any()
                            })
                            .unwrap_or_else(|| view! { "no local split available" }.into_any())
                    }}
                </div>
                <div class="tile-sub">
                    <span class="dim">"share of tokens, this window"</span>
                </div>
                <Spark vals=s_local color="var(--c3)"/>
            </div>
        </div>
    }
}

/// A tile's sparkline, as wide as its tile.
#[component]
fn Spark(#[prop(into)] vals: Signal<Vec<f64>>, color: &'static str) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    view! {
        <div class="spark-box" node_ref=node>
            {move || {
                let w = width.get();
                (w >= 20.0).then(|| sparkline(&vals.get(), w, 26.0, color))
            }}
        </div>
    }
}

/// Tile figures interpolate to their new value over ~500 ms (and not at all
/// under reduced motion).
fn count_of(f: impl Fn() -> f64 + Send + Sync + 'static) -> Signal<f64> {
    charts::count_up(Signal::derive(f))
}
