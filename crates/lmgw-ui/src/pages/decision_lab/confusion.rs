//! A question's confusion matrix (labs-store design §3.4): the labels as
//! rows, the predictions as columns, both in the server's class order (the
//! options, the levels by index, `yes` then `no`). Each cell is shaded with
//! `--accent` at the share of its row it holds, so a label's spread reads
//! across its row whatever the class sizes (past half of it the count is
//! written in `--accent-ink`, which reads on the fill); the counts are in the
//! cells, the diagonal bold, each row's and column's total at its end, and
//! a cell's tooltip says the count and the share.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{QuestionDraft, QuestionKind, QuestionMetrics};

use crate::fmt::grouped;

/// The classes as the matrix names them: a score's level by its index and
/// text ("2 · today"), the others as they are.
pub(super) fn class_names(q: Option<&QuestionDraft>, classes: &[String]) -> Vec<String> {
    classes
        .iter()
        .map(|c| match q.map(|q| &q.kind) {
            Some(QuestionKind::Score { levels }) => c
                .parse::<usize>()
                .ok()
                .and_then(|i| levels.get(i).map(|t| format!("{i} · {t}")))
                .unwrap_or_else(|| c.clone()),
            _ => c.clone(),
        })
        .collect()
}

/// A cell's shade: its share of its row, 0 for an empty row.
pub(super) fn share(n: u64, row: u64) -> f64 {
    if row == 0 {
        0.0
    } else {
        n as f64 / row as f64
    }
}

#[component]
pub(super) fn Confusion(m: QuestionMetrics, q: Option<QuestionDraft>) -> impl IntoView {
    let names = class_names(q.as_ref(), &m.classes);
    if m.confusion.iter().all(|r| r.iter().all(|n| *n == 0)) {
        return view! {
            <p class="mini-note dim">"No item is both answered and labelled yet."</p>
        }
        .into_any();
    }
    let cols = m.classes.len();
    let col_totals: Vec<u64> = (0..cols)
        .map(|j| {
            m.confusion
                .iter()
                .map(|r| r.get(j).copied().unwrap_or(0))
                .sum()
        })
        .collect();
    let rows = m
        .confusion
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let total: u64 = row.iter().sum();
            let label = names.get(i).cloned().unwrap_or_default();
            let cells = row
                .iter()
                .enumerate()
                .map(|(j, n)| {
                    let s = share(*n, total);
                    let hi = s > 0.55;
                    let pred = names.get(j).cloned().unwrap_or_default();
                    let title = format!(
                        "label {label}, predicted {pred}: {} ({:.0}% of the label's {})",
                        grouped(*n),
                        s * 100.0,
                        grouped(total)
                    );
                    view! {
                        <td
                            class="dl-cm-cell"
                            class:diag=i == j
                            class:zero=*n == 0
                            class:hi=hi
                            title=title
                        >
                            <i style=format!("opacity:{s:.3}")></i>
                            <span>{grouped(*n)}</span>
                        </td>
                    }
                })
                .collect_view();
            view! {
                <tr>
                    <th class="dl-cm-label" title=label.clone()>{label.clone()}</th>
                    {cells}
                    <td class="dl-cm-total">{grouped(total)}</td>
                </tr>
            }
        })
        .collect_view();
    // A score's columns are headed by the level's index alone, its text in
    // the tooltip and the row heads: four level texts side by side do not
    // fit a matrix's column.
    let heads = names
        .iter()
        .zip(&m.classes)
        .map(|(n, c)| {
            let short = if m.kind == "score" {
                c.clone()
            } else {
                n.clone()
            };
            view! { <th class="dl-cm-pred" title=n.clone()>{short}</th> }
        })
        .collect_view();
    let foot = col_totals
        .iter()
        .map(|n| view! { <td class="dl-cm-total">{grouped(*n)}</td> })
        .collect_view();
    view! {
        <p class="mini-note dim dl-cm-axes">"rows: the label · columns: the prediction"</p>
        <div class="dl-cm-wrap">
            <table class="dl-cm">
                <thead>
                    <tr>
                        <th class="dl-cm-corner"></th>
                        {heads}
                        <th class="dl-cm-total">"all"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
                <tfoot>
                    <tr>
                        <th class="dl-cm-label">"all"</th>
                        {foot}
                        <td class="dl-cm-total">
                            {grouped(col_totals.iter().sum())}
                        </td>
                    </tr>
                </tfoot>
            </table>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::examples;

    #[test]
    fn a_score_names_its_levels_and_a_cell_its_share_of_the_row() {
        let qs = examples().into_iter().next().unwrap().form.questions;
        let classes: Vec<String> = ["0", "1", "2", "3"].map(String::from).to_vec();
        assert_eq!(
            class_names(Some(&qs[1]), &classes)[2],
            "2 · today",
            "urgency's third level"
        );
        assert_eq!(class_names(Some(&qs[0]), &["billing".into()]), ["billing"]);
        assert_eq!(share(3, 12), 0.25);
        assert_eq!(share(0, 0), 0.0);
    }
}
