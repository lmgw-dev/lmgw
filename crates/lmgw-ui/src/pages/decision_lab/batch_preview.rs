//! A new set run's preview (labs-store design §3.3): the file's columns, each
//! marked with what the mapping reads it as, the first rows ("first 20 of
//! 1 234"), and every problem the server finds, each with its row, line,
//! column and value. Nothing is dropped: a problem refuses the file until
//! the file or the mapping changes. A long list shows its first problems and
//! a "Show all" that shows every one. A preview that did not load says so
//! with Retry, and Start's line says it too.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{BatchPreviewed, BatchProblem};

use super::batch_new::preview_again;
use super::page::Lab;
use crate::fmt::grouped;
use crate::widgets::ShowMore;

/// "first 20 of 1 234 rows", or "all 12 rows" when the sample is the file.
pub(super) fn sample_line(p: &BatchPreviewed) -> String {
    let shown = p.sample.len() as u64;
    if shown >= p.rows {
        format!(
            "all {} {}",
            grouped(p.rows),
            if p.rows == 1 { "row" } else { "rows" }
        )
    } else {
        format!("first {} of {} rows", grouped(shown), grouped(p.rows))
    }
}

/// A problem's message without the "row 3 (line 4), column 'team': " its
/// columns already say.
pub(super) fn problem_text(p: &BatchProblem) -> String {
    let lead = match (p.row, p.line, &p.column) {
        (Some(r), Some(l), Some(c)) => format!("row {r} (line {l}), column '{c}': "),
        (Some(r), Some(l), None) => format!("row {r} (line {l}): "),
        _ => String::new(),
    };
    p.message
        .strip_prefix(&lead)
        .filter(|_| !lead.is_empty())
        .unwrap_or(&p.message)
        .to_string()
}

/// What the mapping reads a column as: "state", "key", "label: team".
fn roles(lab: Lab, col: &str) -> Vec<String> {
    let nr = lab.new_run;
    let mut out = Vec::new();
    if nr.state_col.with(|s| s == col) {
        out.push("state".to_string());
    }
    if nr.key_col.with(|k| k == col) {
        out.push("key".to_string());
    }
    nr.labels.with(|l| {
        for (q, c) in l {
            if c == col {
                out.push(format!("label: {q}"));
            }
        }
    });
    out
}

#[component]
pub(super) fn PreviewPart(lab: Lab) -> impl IntoView {
    let nr = lab.new_run;
    let shown = RwSignal::new(20usize);
    let problems_n = Signal::derive(move || {
        nr.preview
            .with(|p| p.as_ref().map(|p| p.problems.len()))
            .unwrap_or(0)
    });
    view! {
        {move || nr.preview_err.get().map(|e| view! {
            <div class="notice err dl-preview-failed">
                {format!("⚠ The preview did not load: {e}")}
                " "
                <button class="btn ghost sm" on:click=move |_| preview_again(lab)>"Retry"</button>
            </div>
        })}
        <Show when=move || { problems_n.get() > 0 }>
            <section class="dl-newrun-part dl-problems">
                <div class="dl-card-head">
                    <h3 class="lab-h3">"Problems"</h3>
                    <span class="count bad">{move || grouped(problems_n.get() as u64)}</span>
                    <span class="mini-note dim">"the run is refused until each is gone; nothing is dropped"</span>
                </div>
                <div class="dl-problems-wrap">
                    <table class="data dl-problem-table">
                        <thead>
                            <tr>
                                <th class="num-h">"row"</th>
                                <th class="num-h">"line"</th>
                                <th>"column"</th>
                                <th>"value"</th>
                                <th>"problem"</th>
                            </tr>
                        </thead>
                        <tbody>
                            {move || nr.preview.with(|p| {
                                p.as_ref().map(|p| {
                                    p.problems
                                        .iter()
                                        .take(shown.get())
                                        .map(|x| {
                                            let opt = |v: Option<u64>| v.map(grouped).unwrap_or_default();
                                            view! {
                                                <tr>
                                                    <td class="num">{opt(x.row)}</td>
                                                    <td class="num">{opt(x.line)}</td>
                                                    <td class="mono-sm">{x.column.clone().unwrap_or_default()}</td>
                                                    <td class="mono-sm dl-cellv dl-prob-value" title=x.value.clone().unwrap_or_default()>
                                                        {x.value.clone().map(|v| format!("'{v}'")).unwrap_or_default()}
                                                    </td>
                                                    <td class="wrap">{problem_text(x)}</td>
                                                </tr>
                                            }
                                        })
                                        .collect_view()
                                })
                            })}
                        </tbody>
                    </table>
                </div>
                <ShowMore total=problems_n shown=shown step=20 noun="problems"/>
            </section>
        </Show>
        {move || nr.preview.get().map(|p| {
            let heads = p.columns.iter().map(|c| {
                let name = c.name.clone();
                let json = c.json;
                let col = c.name.clone();
                view! {
                    <th title=if json { format!("{name}: every value is JSON") } else { name.clone() }>
                        <span class="dl-col-name">{name.clone()}</span>
                        {json.then(|| view! { <span class="chip off sm">"JSON"</span> })}
                        {move || roles(lab, &col).into_iter().map(|r| view! {
                            <span class="chip info sm">{r}</span>
                        }).collect_view()}
                    </th>
                }
            }).collect_view();
            let rows = p.sample.iter().enumerate().map(|(i, r)| {
                let cells = r.iter().map(|v| {
                    view! { <td class="dl-cellv" title=v.clone()>{v.clone()}</td> }
                }).collect_view();
                view! {
                    <tr>
                        <td class="num">{(i + 1).to_string()}</td>
                        {cells}
                    </tr>
                }
            }).collect_view();
            view! {
                <section class="dl-newrun-part">
                    <div class="dl-card-head">
                        <h3 class="lab-h3">"Preview"</h3>
                        <span class="mini-note dim">{sample_line(&p)}</span>
                        <span class="spacer"></span>
                        <Show when=move || !nr.fresh.get()>
                            <span class="mini-note dim">"reading…"</span>
                        </Show>
                    </div>
                    <div class="dl-preview-wrap">
                        <table class="data dl-preview">
                            <thead>
                                <tr>
                                    <th class="num-h">"row"</th>
                                    {heads}
                                </tr>
                            </thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                </section>
            }
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_line_says_how_much_of_the_file_it_is() {
        let mut p = BatchPreviewed {
            rows: 1234,
            sample: vec![vec![]; 20],
            ..Default::default()
        };
        assert_eq!(sample_line(&p), "first 20 of 1\u{202F}234 rows");
        p.rows = 12;
        p.sample.truncate(12);
        assert_eq!(sample_line(&p), "all 12 rows");
    }

    #[test]
    fn a_problem_drops_the_lead_its_columns_say() {
        let p = BatchProblem {
            kind: lmgw_api_types::decision_lab::BatchProblemKind::Label,
            question: Some("team".into()),
            row: Some(3),
            line: Some(4),
            column: Some("team".into()),
            value: Some("Billing".into()),
            message: "row 3 (line 4), column 'team': 'Billing' is not an option of 'team' (billing, technical)".into(),
        };
        assert_eq!(
            problem_text(&p),
            "'Billing' is not an option of 'team' (billing, technical)"
        );
        let file = BatchProblem {
            message: "the mapping names no state column".into(),
            ..Default::default()
        };
        assert_eq!(problem_text(&file), "the mapping names no state column");
    }
}
