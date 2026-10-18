//! A question's reliability diagram (labs-store design §3.4, §3.5 R3, R8,
//! R16): `bins` equal-width bins over the predicted probability (the
//! winner's for `choice`, the nearest level's for `score`, P(yes) for
//! `noul`), each a column as tall as the observed rate in it (the winner
//! right, or for `noul` the share labelled yes), against the diagonal a
//! calibrated model's columns would reach. The server answers only the bins
//! that hold an answer; the empty ones are drawn from `bins`, as empty
//! tracks while a bin is at least 3 px wide, else as one track under the
//! columns (the count of empty bins is in the caption either way).
//!
//! The charts' mark specs hold (`charts.rs`): columns of at most 24 px with
//! a 4 px rounded data end on a square root, a solid hairline grid, a 2 px
//! gap between touching fills. Each bin's whole band is its hover target,
//! its tooltip the range, count, mean prediction and observed rate, and a
//! table of the bins is the diagram's text twin.

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::decision_lab::{QuestionMetrics, ReliabilityBin};

use crate::charts::{top_round, use_element_size, Plot};
use crate::fmt::grouped;

/// Where bin `index` of `bins` lies across the plot: its band's left edge
/// and width.
pub(super) fn band(p: &Plot, index: u32, bins: u32) -> (f64, f64) {
    let w = p.iw() / f64::from(bins.max(1));
    (p.l + f64::from(index) * w, w)
}

/// A column's x and width inside its band: at most 24 px, a 2 px gap to
/// each neighbour, centred.
pub(super) fn column(x: f64, w: f64) -> (f64, f64) {
    let cw = (w - 2.0).clamp(1.0, 24.0);
    (x + (w - cw) / 2.0, cw)
}

/// Are the bins wide enough to draw each empty one's track?
pub(super) fn tracks_each(p: &Plot, bins: u32) -> bool {
    p.iw() / f64::from(bins.max(1)) >= 3.0
}

/// A bin's tooltip.
pub(super) fn bin_title(b: &ReliabilityBin, noul: bool) -> String {
    format!(
        "{:.3}–{:.3}: {} {} · mean prediction {:.3} · {} {:.1}%",
        b.lower,
        b.upper,
        grouped(b.count),
        if b.count == 1 { "answer" } else { "answers" },
        b.mean_prediction,
        if noul { "labelled yes" } else { "right" },
        b.observed * 100.0
    )
}

#[component]
pub(super) fn Reliability(m: QuestionMetrics, bins: u32) -> impl IntoView {
    let noul = m.kind == "noul";
    let box_ref: NodeRef<html::Div> = NodeRef::new();
    let size = use_element_size(box_ref);
    let data = StoredValue::new(m.reliability.clone());
    let filled = m.reliability.len() as u64;
    let empty = u64::from(bins).saturating_sub(filled);
    let caption = format!(
        "{} bins · {} hold an answer · {} empty",
        grouped(u64::from(bins)),
        grouped(filled),
        grouped(empty)
    );
    let (x_label, y_label) = if noul {
        ("predicted P(yes)", "labelled yes")
    } else {
        ("predicted probability", "right")
    };
    let chart = move || {
        let w = size.get().0;
        if w < 40.0 {
            return ().into_any();
        }
        let h = (w * 0.62).clamp(150.0, 240.0);
        let p = Plot::new(w, h, 34.0, 8.0, 8.0, 26.0);
        let base = p.bottom();
        let y = |v: f64| p.y(v, 1.0);
        let grid = [0.0, 0.25, 0.5, 0.75, 1.0]
            .into_iter()
            .map(|v| {
                let yy = y(v);
                let label = format!("{v}");
                view! {
                    <line x1=p.l x2=p.right() y1=yy y2=yy class="gridline"></line>
                    <text x=p.l - 6.0 y=yy + 3.5 class="ax" text-anchor="end">{label.clone()}</text>
                    <text x=p.l + v * p.iw() y=p.h - 12.0 class="ax" text-anchor="middle">{label}</text>
                }
            })
            .collect_view();
        let tracks = if tracks_each(&p, bins) {
            (0..bins)
                .map(|i| {
                    let (x, bw) = band(&p, i, bins);
                    let (cx, cw) = column(x, bw);
                    view! { <rect class="dl-rel-track" x=cx y=p.t width=cw height=p.ih()></rect> }
                })
                .collect_view()
                .into_any()
        } else {
            view! { <rect class="dl-rel-track" x=p.l y=p.t width=p.iw() height=p.ih()></rect> }
                .into_any()
        };
        let cols = data.with_value(|d| {
            d.iter()
                .map(|b| {
                    let (x, bw) = band(&p, b.index, bins);
                    let (cx, cw) = column(x, bw);
                    let top = y(b.observed);
                    let hgt = (base - top).max(0.0);
                    let title = bin_title(b, noul);
                    view! {
                        <g class="dl-rel-bin">
                            <path class="dl-rel-col" d=top_round(cx, top, cw, hgt, 4.0)></path>
                            <rect class="dl-rel-hit" x=x y=p.t width=bw height=p.ih()>
                                <title>{title}</title>
                            </rect>
                        </g>
                    }
                })
                .collect_view()
        });
        view! {
            <svg
                class="dl-rel-svg"
                width=w
                height=h
                viewBox=p.view_box()
                role="img"
                aria-label=format!("reliability diagram: {y_label} against {x_label}")
            >
                {tracks}
                {grid}
                <line class="dl-rel-diag" x1=p.l y1=base x2=p.right() y2=p.t></line>
                {cols}
                <text x=p.right() y=p.h - 1.0 class="axl" text-anchor="end">{x_label}</text>
                <text x=p.l + 5.0 y=p.t + 12.0 class="axl" text-anchor="start">{format!("{y_label} ↑")}</text>
            </svg>
        }
        .into_any()
    };
    let rows = m
        .reliability
        .iter()
        .map(|b| {
            view! {
                <tr>
                    <td class="num">{format!("{:.3}–{:.3}", b.lower, b.upper)}</td>
                    <td class="num">{grouped(b.count)}</td>
                    <td class="num">{format!("{:.3}", b.mean_prediction)}</td>
                    <td class="num">{format!("{:.1}%", b.observed * 100.0)}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <div class="dl-rel">
            {(filled == 0).then(|| view! {
                <p class="mini-note dim">"No labelled answer yet: every bin is empty."</p>
            })}
            <div class="dl-rel-box" node_ref=box_ref>{chart}</div>
            <p class="mini-note dim dl-rel-cap">
                {format!("columns: share {y_label} per bin · diagonal: calibrated · {caption}")}
            </p>
            <details class="dl-rel-table">
                <summary>"the bins as a table"</summary>
                <table class="data">
                    <thead>
                        <tr>
                            <th class="num-h">"bin"</th>
                            <th class="num-h">"answers"</th>
                            <th class="num-h">"mean prediction"</th>
                            <th class="num-h">{y_label}</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
            </details>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bin_is_placed_by_its_index_and_its_column_keeps_the_mark_spec() {
        let p = Plot::new(434.0, 200.0, 34.0, 0.0, 0.0, 0.0);
        assert_eq!(band(&p, 0, 10), (34.0, 40.0));
        assert_eq!(band(&p, 9, 10), (394.0, 40.0));
        assert_eq!(column(34.0, 40.0), (42.0, 24.0), "capped at 24 px, centred");
        assert_eq!(column(0.0, 4.0), (1.0, 2.0), "a 2 px gap");
        assert_eq!(column(0.0, 1.0).1, 1.0, "never thinner than a pixel");
        assert!(tracks_each(&p, 100));
        assert!(!tracks_each(&p, 1000));
    }

    #[test]
    fn a_bins_tooltip_says_what_it_holds() {
        let b = ReliabilityBin {
            index: 7,
            lower: 0.7,
            upper: 0.8,
            count: 34,
            mean_prediction: 0.756,
            observed: 0.7059,
        };
        assert_eq!(
            bin_title(&b, false),
            "0.700–0.800: 34 answers · mean prediction 0.756 · right 70.6%"
        );
        assert!(bin_title(&b, true).ends_with("labelled yes 70.6%"));
    }
}
