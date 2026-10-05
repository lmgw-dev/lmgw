use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use leptos_router::NavigateOptions;
use lmgw_api_types::{ErrorKindSeries, UsageErrorsResponse};

use crate::charts::{
    self, chart_height, crosshair, hit_bands, legend, rect_path, top_round, x_labels, y_grid_ticks,
    LegendItem, Plot, Tip, TipRow, Tips,
};

use super::*;

// ---------------------------------------------------------------------------
// 11 · refusals & errors
// ---------------------------------------------------------------------------

#[component]
pub(super) fn ErrorsCard(
    errors: Src<UsageErrorsResponse>,
    class: RwSignal<String>,
    key_id: RwSignal<String>,
    entry: RwSignal<bool>,
    tips: Tips,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    let width = chart_width(node);
    let hover = RwSignal::new(None::<usize>);
    let tv = RwSignal::new(false);
    let navigate = use_navigate();
    view! {
        <div class="card chart-card u-errors">
            <ChartHead
                title="Refusals & errors"
                note="by kind — lmgw refusing is amber, something failing is red"
                tv=tv
                extra=move || {
                    view! {
                        <a
                            class="head-link"
                            href=move || {
                                TrafficLink::scoped(&class.get(), &key_id.get()).failures().href()
                            }
                        >
                            "open these in Traffic"
                        </a>
                    }
                }
            />
            <CardErr err=errors.err/>
            <div class="chart-box" node_ref=node class:stale=move || errors.stale()>
                {move || {
                    let w = width.get();
                    let Some(r) = errors.data.get() else { return ().into_any() };
                    if w < 120.0 {
                        return ().into_any();
                    }
                    // This card reads the RAW rows, so it can only see as far back
                    // as log retention — every other card on the page covers the
                    // whole rollup history. Drawing empty columns for pruned days
                    // would read as "the errors stopped", so the axis is clipped
                    // to the window that can actually hold data, and the note says
                    // which window that is.
                    let keep = retention_cutoff(r.retention_days);
                    // Compare on the common prefix: the cutoff is a day key, and
                    // a month bucket ("2026-08") would lose a plain string
                    // compare against "2026-08-19" despite containing it.
                    let within = |b: &str| match keep.as_deref() {
                        None => true,
                        Some(k) => {
                            let n = b.len().min(k.len());
                            b[..n] >= k[..n]
                        }
                    };
                    let first = r
                        .buckets
                        .iter()
                        .position(|b| within(b))
                        .unwrap_or(0);
                    let buckets: Vec<String> = r.buckets[first..].to_vec();
                    let n = buckets.len();
                    // `counts` is documented as aligned with `buckets`; pad to
                    // exactly `n` anyway, because an off-by-one from the server
                    // would otherwise be an index panic that blanks the page.
                    let cut = |c: &[i64]| -> Vec<i64> {
                        let mut v: Vec<i64> = c.iter().skip(first).copied().collect();
                        v.resize(n, 0);
                        v
                    };
                    // Failures at the bottom, refusals on top: the two groups stay
                    // contiguous, so the colour reads as a group and the opacity
                    // step separates the kinds inside it.
                    let mut order: Vec<&ErrorKindSeries> =
                        r.kinds.iter().filter(|k| !k.refusal).collect();
                    order.extend(r.kinds.iter().filter(|k| k.refusal));
                    let counts: Vec<Vec<i64>> = order.iter().map(|k| cut(&k.counts)).collect();
                    let shade = |k: &ErrorKindSeries, rank: usize| -> (String, f64) {
                        // Status colours are reserved for status (design §6.3),
                        // and here they *mean* status: lmgw refusing is amber,
                        // something failing is red. Kinds inside a group separate
                        // by an opacity step rather than by borrowing a series
                        // hue, which would say "this refusal is a different kind
                        // of thing" when it is not.
                        let base = if k.refusal { "var(--amber)" } else { "var(--err)" };
                        let op = match rank {
                            0 => 1.0,
                            1 => 0.72,
                            2 => 0.5,
                            _ => 0.34,
                        };
                        (base.to_string(), op)
                    };
                    let mut rank_f = 0usize;
                    let mut rank_r = 0usize;
                    let shades: Vec<(String, f64)> = order
                        .iter()
                        .map(|k| {
                            let rank = if k.refusal {
                                rank_r += 1;
                                rank_r - 1
                            } else {
                                rank_f += 1;
                                rank_f - 1
                            };
                            shade(k, rank)
                        })
                        .collect();
                    let totals: Vec<i64> = (0..n)
                        .map(|b| counts.iter().map(|c| c[b]).sum())
                        .collect();
                    let grand: i64 = totals.iter().sum();
                    // Say the range that is actually drawn, not the one that was
                    // asked for — and only claim the retention bound when the
                    // clip above is what decided the left edge.
                    let unit = buckets.first().map(|b| bucket_word(b)).unwrap_or("day");
                    let range_note = match (keep.is_some(), first > 0) {
                        (true, true) => {
                            format!(
                                "Clipped to the last {} days: this card reads the raw request rows, which is as far back as log retention goes, while every other card here covers the whole rollup history.",
                                r.retention_days,
                            )
                        }
                        (true, false) => {
                            format!(
                                "By {unit}, from the raw request rows — which are kept {} days, so this card cannot look back as far as the rest of the page.",
                                r.retention_days,
                            )
                        }
                        (false, _) => {
                            format!(
                                "By {unit}, from the raw request rows, which are kept indefinitely.",
                            )
                        }
                    };
                    if grand == 0 || n == 0 {
                        return view! {
                            <div class="chart-empty">
                                "Nothing failed and nothing was refused in this window."
                            </div>
                            <div class="chart-foot">{range_note}</div>
                        }
                            .into_any();
                    }
                    let anim = entry.get_untracked();
                    let nav = navigate.clone();
                    let scope = TrafficLink::scoped(&class.get(), &key_id.get());
                    let p = Plot::new(w, chart_height(w), 38.0, 12.0, 10.0, 28.0);
                    let max = totals.iter().map(|v| *v as f64).fold(0.0, f64::max).max(1.0)
                        * 1.15;
                    let grid = y_grid_ticks(
                        &p,
                        max,
                        charts::nice_ticks_int(max, 3),
                        &|v| format!("{}", v.round()),
                    );
                    let bw = (p.band_w(n) * 0.6).clamp(1.0, 16.0);
                    let cols: Vec<AnyView> = (0..n)
                        .map(|b| {
                            let x = p.band_x(b, n) + (p.band_w(n) - bw) / 2.0;
                            let mut segs: Vec<AnyView> = Vec::new();
                            let mut acc = 0.0_f64;
                            let mut placed = 0usize;
                            for (si, c) in counts.iter().enumerate() {
                                let v = c[b] as f64;
                                if v <= 0.0 {
                                    continue;
                                }
                                let h_full = (v / max) * p.ih();
                                let y = p.y(acc + v, max);
                                let is_top = counts[si + 1..].iter().all(|cc| cc[b] <= 0);
                                // 2px surface gap between touching fills
                                let h = if placed > 0 {
                                    (h_full - 2.0).max(1.0)
                                } else {
                                    h_full.max(1.0)
                                };
                                let d = if is_top {
                                    top_round(x, y, bw, h, 3.0)
                                } else {
                                    rect_path(x, y, bw, h)
                                };
                                let (fill, op) = shades[si].clone();
                                // Each segment already knows its kind, so it can
                                // land on exactly those rows. `stop_propagation`
                                // keeps it from also firing the whole-plot link
                                // underneath.
                                let href = scope
                                    .clone()
                                    .failures()
                                    .kind(order[si].kind.clone())
                                    .href();
                                let nav = nav.clone();
                                let kind = order[si].kind.clone();
                                segs.push(
                                    view! {
                                        <path
                                            d=d
                                            fill=fill
                                            fill-opacity=op.to_string()
                                            style="cursor:pointer"
                                            on:click=move |ev: web_sys::MouseEvent| {
                                                ev.stop_propagation();
                                                nav(&href, NavigateOptions::default());
                                            }
                                        >
                                            <title>
                                                {format!("Open {kind} in Traffic")}
                                            </title>
                                        </path>
                                    }
                                        .into_any(),
                                );
                                acc += v;
                                placed += 1;
                            }
                            let cls = if anim { "anim-col" } else { "" };
                            let style = if anim {
                                format!("animation-delay:{}ms", b * 9)
                            } else {
                                String::new()
                            };
                            view! { <g class=cls style=style>{segs}</g> }.into_any()
                        })
                        .collect();
                    // The whole plot is one click-through: `/api/logs` filters on
                    // status, not on kind or on a bucket, so a per-kind or
                    // per-column link would land on the unfiltered head while
                    // looking like it had filtered. One honest link instead.
                    // The gaps between segments still mean "these failures", so
                    // the plot keeps a whole-chart link under the per-kind ones.
                    let go = {
                        let nav = nav.clone();
                        let href = scope.clone().failures().href();
                        move || nav(&href, NavigateOptions::default())
                    };
                    let tip_data: Vec<Tip> = (0..n)
                        .map(|b| {
                            Tip::new(
                                    bucket_full(&buckets[b]),
                                    order
                                        .iter()
                                        .enumerate()
                                        .filter(|(si, _)| counts[*si][b] > 0)
                                        .map(|(si, k)| {
                                            TipRow::new(
                                                shades[si].0.clone(),
                                                if k.refusal {
                                                    format!("{} (refused)", k.kind)
                                                } else {
                                                    k.kind.clone()
                                                },
                                                counts[si][b].to_string(),
                                            )
                                        })
                                        .collect(),
                                )
                                .with_total("total", totals[b].to_string())
                        })
                        .collect();
                    let leg: Vec<LegendItem> = order
                        .iter()
                        .enumerate()
                        .map(|(si, k)| {
                            LegendItem::fill(
                                    shades[si].0.clone(),
                                    if k.refusal {
                                        format!("{} (refused)", k.kind)
                                    } else {
                                        k.kind.clone()
                                    },
                                )
                                .at(shades[si].1)
                                .linking(
                                    scope.clone().failures().kind(k.kind.clone()).href(),
                                )
                        })
                        .collect();
                    let mut headers = vec!["Bucket".to_string()];
                    headers.extend(order.iter().map(|k| k.kind.clone()));
                    headers.push("Total".into());
                    let rows: Vec<Vec<String>> = (0..n)
                        .map(|b| {
                            let mut row = vec![bucket_full(&buckets[b])];
                            row.extend(counts.iter().map(|c| c[b].to_string()));
                            row.push(totals[b].to_string());
                            row
                        })
                        .collect();
                    let refused: i64 =
                        r.kinds.iter().filter(|k| k.refusal).map(|k| k.total).sum();
                    let chart = || {
                        view! {
                        {legend(leg)}
                        <svg
                            width=p.w
                            height=p.h
                            viewBox=p.view_box()
                            style="cursor:pointer"
                            role="link"
                            tabindex="0"
                            aria-label="Open the failing and refused requests in Traffic"
                            on:click={
                                let go = go.clone();
                                move |_| go()
                            }
                            on:keydown={
                                let go = go.clone();
                                move |ev: web_sys::KeyboardEvent| {
                                    if ev.key() == "Enter" || ev.key() == " " {
                                        ev.prevent_default();
                                        go();
                                    }
                                }
                            }
                        >
                            {grid} {cols} {x_labels(&p, &buckets.iter().map(|b| bucket_label(b)).collect::<Vec<_>>(), (n / 3).max(1))}
                            {crosshair(&p, hover, n)} {hit_bands(&p, tip_data, tips, hover)}
                        </svg>
                        }
                        .into_any()
                    };
                    view! {
                        {chart_or_table(tv, chart, headers, rows)}
                        <div class="chart-foot">
                            {range_note}
                            " "
                            {(refused > 0)
                                .then(|| {
                                    view! {
                                        "A refusal names the setting it hit; "
                                        <span class="mono-sm">"key_budget"</span>
                                        " answers 403, deliberately not a 429, because a monthly budget will not clear inside any retry window."
                                    }
                                })}
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

/// The earliest bucket the raw rows can still cover, as a `YYYY-MM-DD` key.
/// `None` when retention is unlimited, which is a real setting (`0`).
fn retention_cutoff(retention_days: i64) -> Option<String> {
    if retention_days <= 0 {
        return None;
    }
    let ms = now_ms() - (retention_days as f64) * 86_400_000.0;
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(ms));
    Some(format!(
        "{:04}-{:02}-{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date()
    ))
}
