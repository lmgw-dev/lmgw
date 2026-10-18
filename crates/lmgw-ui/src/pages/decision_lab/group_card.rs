//! A kept group's card (labs-store design §2.6, §2.7). One cell: its answers
//! stacked. Several: a matrix, a row per question and a column per cell
//! ("decide/laya · order reversed"), each cell the compact answer, a click
//! opening its full views below; and for each perturbed model a Δ line per
//! question. A group with fewer runs than cells says so and offers the rest.
//! The card's menu: Open in form, Export, Delete.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::metrics::is_2xx;
use lmgw_api_types::decision_lab::{DecisionForm, Variant};
use lmgw_api_types::lab_store::{LabExportFormat, LabGroupView, LabRunView};
use lmgw_api_types::systemone::{Answer, SystemOneResponse};

use super::super::lab_frame::LabView;
use super::super::lab_history::ExportScope;
use super::answers::{compact, delta};
use super::ask::{open_cells, run_rest};
use super::page::{note_for, CellNote, Lab};
use super::run_view::{answer_of, RunView};
use crate::fmt::{grouped, local_datetime};
use crate::pages::docs::download;
use crate::widgets::{MenuItem, RowMenu};

/// A plan cell's column head: "decide/laya · order reversed".
pub(super) fn cell_label(model: &str, variant: &str) -> String {
    match Variant::parse(variant) {
        Some(v) => format!("{model} · {}", v.label()),
        None => format!("{model} · {variant}"),
    }
}

/// The models of a plan, each once, in order.
pub(super) fn plan_models(g: &LabGroupView) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &g.plan {
        if !out.contains(&c.model) {
            out.push(c.model.clone());
        }
    }
    out
}

/// The card's second line: what was asked.
pub(super) fn plan_summary(g: &LabGroupView) -> String {
    let models = plan_models(g);
    let mut variants: Vec<&str> = Vec::new();
    for c in &g.plan {
        let l = Variant::parse(&c.variant)
            .map(Variant::label)
            .unwrap_or(&c.variant);
        if !variants.contains(&l) {
            variants.push(l);
        }
    }
    let mut s = if g.plan.len() == 1 {
        g.plan.first().map(|c| c.model.clone()).unwrap_or_default()
    } else {
        format!(
            "{} cells · {} × {}",
            g.plan.len(),
            models.join(", "),
            variants.join(", ")
        )
    };
    if g.override_text.is_some() {
        s.push_str(" · override");
    }
    s
}

/// The answer of a run, per question id.
fn answers(run: &LabRunView) -> Vec<(String, Answer)> {
    answer_of(run).map(|r| r.answers.0).unwrap_or_default()
}

/// The Δ lines: for each model asked with perturbations, each perturbed
/// cell's change against the same model as written, per question.
pub(super) fn deltas(g: &LabGroupView) -> Vec<String> {
    let models = plan_models(g);
    let mut out = Vec::new();
    for m in &models {
        let cell_of = |variant: &str| {
            g.plan
                .iter()
                .position(|c| c.model == *m && c.variant == variant)
                .and_then(|i| g.runs.iter().find(|r| r.cell as usize == i))
        };
        let Some(base) = cell_of(Variant::AsWritten.as_str()) else {
            continue;
        };
        let base = answers(base);
        for v in [Variant::NoDescriptions, Variant::Reversed] {
            let Some(run) = cell_of(v.as_str()) else {
                continue;
            };
            let other = answers(run);
            for (qid, a) in &base {
                let Some((_, b)) = other.iter().find(|(k, _)| k == qid) else {
                    continue;
                };
                if let Some(line) = delta(a, b, v) {
                    if models.len() > 1 {
                        out.push(format!("{m} · {qid}: {line}"));
                    } else {
                        out.push(format!("{qid}: {line}"));
                    }
                }
            }
        }
    }
    out
}

/// The question ids the matrix has rows for: the stored form's, else the
/// answers' (an override's questions are only in its text).
fn question_ids(g: &LabGroupView) -> Vec<String> {
    let from_form: Vec<String> = serde_json::from_value::<DecisionForm>(g.form.clone())
        .map(|f| f.questions.into_iter().map(|q| q.id).collect())
        .unwrap_or_default();
    if g.override_text.is_none() && !from_form.is_empty() {
        return from_form;
    }
    let mut out: Vec<String> = Vec::new();
    for r in &g.runs {
        for (k, _) in answers(r) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

fn when(created_at: &str) -> String {
    let ms = js_sys::Date::parse(created_at);
    if ms.is_nan() {
        created_at.to_string()
    } else {
        local_datetime(ms / 1000.0)
    }
}

/// Load a group's form, plan and override back into the form. A kept image
/// stays its artifact, in the form and as `lab-artifact:<id>` in the
/// override: the server sends the data URL it first came as.
fn open_in_form(lab: Lab, g: LabGroupView) {
    let form = match serde_json::from_value::<DecisionForm>(g.form.clone()) {
        Ok(f) => f,
        Err(e) => {
            lab.toasts
                .err(format!("this group's form does not read: {e}"));
            return;
        }
    };
    lab.form.load(form, &g.artifacts);
    let models = plan_models(&g);
    if let Some((head, rest)) = models.split_first() {
        lab.alias.set(head.clone());
        lab.compare.set(rest.to_vec());
    }
    lab.no_descriptions.set(
        g.plan
            .iter()
            .any(|c| c.variant == Variant::NoDescriptions.as_str()),
    );
    lab.reversed.set(
        g.plan
            .iter()
            .any(|c| c.variant == Variant::Reversed.as_str()),
    );
    lab.override_text.set(g.override_text.clone());
    lab.view.set(LabView::Form);
}

/// The notes of one group, so a card follows its own and no other.
fn notes_of(notes: &[CellNote], id: i64) -> Vec<CellNote> {
    notes.iter().filter(|n| n.group == id).cloned().collect()
}

#[component]
pub(super) fn GroupCard(lab: Lab, id: i64) -> impl IntoView {
    let group = Memo::new(move |_| {
        lab.history
            .groups
            .with(|v| v.iter().find(|g| g.id == id).cloned())
    });
    let selected = RwSignal::new(None::<u32>);
    // This group's cell in flight and its notes, and nothing else: a cell of
    // another group landing leaves this card as it is (an open Raw stays
    // open, no answer is read again).
    let flying = Memo::new(move |_| {
        lab.in_flight
            .get()
            .filter(|(gid, _)| *gid == id)
            .map(|(_, c)| c)
    });
    let notes = Memo::new(move |_| lab.notes.with(|v| notes_of(v, id)));
    let menu = Signal::derive(move || {
        let Some(g) = group.get() else {
            return Vec::new();
        };
        let dirty = lab.form.dirty();
        let busy = g.in_flight || flying.get().is_some();
        let jsonl = lab
            .history
            .export_href(LabExportFormat::Jsonl, &ExportScope::Group(id));
        let csv = lab
            .history
            .export_href(LabExportFormat::Csv, &ExportScope::Group(id));
        let open =
            MenuItem::new("Open in form", move || open_in_form(lab, g.clone())).title(if dirty {
                "Replaces the form, which has edits: pick again to replace it"
            } else {
                "The stored form, its models, its perturbations and its override"
            });
        vec![
            if dirty { open.danger() } else { open },
            MenuItem::new("Export as JSONL", move || download(&jsonl, "")),
            MenuItem::new("Export as CSV", move || download(&csv, "")),
            MenuItem::new("Delete", move || lab.history.delete(id))
                .danger()
                .disabled(busy)
                .title(if busy {
                    "In flight: stop it, or delete it once its answer is in"
                } else {
                    "Delete this group, its runs and the files nothing else names"
                }),
        ]
    });
    view! {
        {move || {
            group
                .get()
                .map(|g| {
                    let flying = flying.get();
                    let cell_notes: Vec<Option<String>> = notes
                        .with(|ns| (0..g.plan.len() as u32).map(|c| note_for(ns, &g, c)).collect());
                    let open = open_cells(&g);
                    let answered = g.plan.len() - open.len();
                    let failed = !g.runs.is_empty() && g.runs.iter().all(|r| !is_2xx(i64::from(r.http_status)));
                    let status = match flying {
                        Some(c) => {
                            Some(
                                view! {
                                    <span class="chip live">
                                        <i class="dot"></i>
                                        {format!("asking cell {} of {}…", c + 1, g.plan.len())}
                                    </span>
                                }
                                    .into_any(),
                            )
                        }
                        // In flight in another tab, or found so by a reload.
                        None if g.in_flight => {
                            Some(
                                view! {
                                    <span class="chip live">
                                        <i class="dot"></i>
                                        "in flight — its answer is on its way"
                                    </span>
                                }
                                    .into_any(),
                            )
                        }
                        None if !open.is_empty() => {
                            Some(
                                view! {
                                    <span class="chip warn">
                                        {format!("{answered} of {} answered — stopped", g.plan.len())}
                                    </span>
                                    <button
                                        type="button"
                                        class="btn ghost sm"
                                        disabled=move || lab.running.get()
                                        title="Ask the cells that have no run yet, one at a time"
                                        on:click=move |_| run_rest(lab, id)
                                    >
                                        "Run the rest"
                                    </button>
                                }
                                    .into_any(),
                            )
                        }
                        None => None,
                    };
                    let body = if g.plan.len() == 1 {
                        let note = cell_notes.first().cloned().flatten();
                        single(&g, flying.is_some() || g.in_flight, note).into_any()
                    } else {
                        matrix(&g, selected, flying, cell_notes).into_any()
                    };
                    view! {
                        <div class="card result-card dl-group" class:failed=failed>
                            <div class="run-head">
                                <span class="run-model dl-title" title=g.title.clone()>
                                    {g.title.clone()}
                                </span>
                                <span class="spacer"></span>
                                <RowMenu items=menu title="Group actions"/>
                            </div>
                            <div class="dl-group-sub mono-sm dim">
                                {format!("#{} · {} · {}", g.id, when(&g.created_at), plan_summary(&g))}
                            </div>
                            {status.map(|s| view! { <div class="dl-status">{s}</div> })}
                            {body}
                        </div>
                    }
                })
        }}
    }
}

/// A one-cell group: the cell in full, or why it has no run.
fn single(g: &LabGroupView, asking: bool, note: Option<String>) -> impl IntoView {
    match g.runs.first().cloned() {
        Some(run) => view! { <RunView run=run note=note/> }.into_any(),
        None => match (asking, note) {
            (true, _) => view! { <p class="mini-note dim">"asking…"</p> }.into_any(),
            (false, Some(n)) => view! { <p class="mini-note dl-bad-line">{n}</p> }.into_any(),
            (false, None) => view! { <p class="mini-note dim">"no answer kept"</p> }.into_any(),
        },
    }
}

/// A matrix cell's text and class for question `qid`, from its run's answer
/// as read (`None` without a run): the compact answer; a dash for no run or
/// no answer to `qid`, "…" for the cell in flight; for a failed cell its
/// status line, and for a 200 that is no answer `answer_of`'s why.
fn cell_text(
    read: Option<&Result<SystemOneResponse, String>>,
    qid: &str,
    flying: bool,
) -> (String, &'static str) {
    match read {
        None if flying => ("…".to_string(), "dim"),
        None => ("—".to_string(), "dim"),
        Some(Ok(a)) => match a.answers.get(qid) {
            Some(ans) => (compact(ans), ""),
            None => ("—".to_string(), "dim"),
        },
        Some(Err(why)) => (why.clone(), "bad"),
    }
}

/// A several-cell group: the matrix, the Δ lines and the picked cell.
fn matrix(
    g: &LabGroupView,
    selected: RwSignal<Option<u32>>,
    flying: Option<u32>,
    notes: Vec<Option<String>>,
) -> impl IntoView {
    let note_of = move |i: u32| notes.get(i as usize).cloned().flatten();
    let qids = question_ids(g);
    let runs: Vec<Option<LabRunView>> = (0..g.plan.len() as u32)
        .map(|c| g.runs.iter().find(|r| r.cell == c).cloned())
        .collect();
    // Each answer read once, not once per question row.
    let read: Vec<Option<Result<SystemOneResponse, String>>> =
        runs.iter().map(|r| r.as_ref().map(answer_of)).collect();
    let lines = deltas(g);
    let heads = g
        .plan
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let i = i as u32;
            let label = cell_label(&c.model, &c.variant);
            view! {
                <th>
                    <button
                        type="button"
                        class="dl-cellbtn"
                        class:on=move || selected.get() == Some(i)
                        title=format!("Open {label} below")
                        on:click=move |_| selected.update(|s| *s = if *s == Some(i) { None } else { Some(i) })
                    >
                        <span class="m">{c.model.clone()}</span>
                        <span class="v">
                            {Variant::parse(&c.variant).map(Variant::label).unwrap_or(&c.variant).to_string()}
                        </span>
                    </button>
                </th>
            }
        })
        .collect_view();
    let rows = qids
        .iter()
        .map(|qid| {
            let cells = read
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let i = i as u32;
                    let (text, cls) = cell_text(r.as_ref(), qid, flying == Some(i));
                    view! {
                        <td class=cls on:click=move |_| selected.set(Some(i))>
                            {text}
                        </td>
                    }
                })
                .collect_view();
            view! {
                <tr>
                    <th class="q mono-sm">{qid.clone()}</th>
                    {cells}
                </tr>
            }
        })
        .collect_view();
    let foot = runs
        .iter()
        .map(|r| {
            let t = r
                .as_ref()
                .map(|r| {
                    // A number keeps its unit on a narrow column.
                    let mut s = format!("{}\u{a0}ms", grouped(r.latency_ms));
                    if let Some(t) = r.input_tokens {
                        s.push_str(&format!(" · {}\u{a0}tok", grouped(t)));
                    }
                    s
                })
                .unwrap_or_default();
            view! { <td class="dim">{t}</td> }
        })
        .collect_view();
    let plan = g.plan.clone();
    view! {
        <div class="tablewrap dl-matrix-wrap">
            <table class="dl-matrix">
                <thead>
                    <tr>
                        <th></th>
                        {heads}
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
                <tfoot>
                    <tr>
                        <th></th>
                        {foot}
                    </tr>
                </tfoot>
            </table>
        </div>
        {(!lines.is_empty())
            .then(|| {
                view! {
                    <ul class="dl-deltas">
                        {lines.into_iter().map(|l| view! { <li>{l}</li> }).collect_view()}
                    </ul>
                }
            })}
        {move || {
            selected
                .get()
                .and_then(|i| {
                    let c = plan.get(i as usize)?;
                    let label = cell_label(&c.model, &c.variant);
                    let body = match runs.get(i as usize).cloned().flatten() {
                        Some(run) => view! { <RunView run=run note=note_of(i)/> }.into_any(),
                        None => {
                            view! {
                                <p class="mini-note dim">
                                    {note_of(i).unwrap_or_else(|| "no run yet".into())}
                                </p>
                            }
                                .into_any()
                        }
                    };
                    Some(
                        view! {
                            <div class="dl-cell-open">
                                <div class="dl-answer-head">
                                    <span class="mono-sm">{label}</span>
                                </div>
                                {body}
                            </div>
                        },
                    )
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(plan: &[(&str, &str)], runs: Vec<serde_json::Value>) -> LabGroupView {
        serde_json::from_value(serde_json::json!({
            "id": 4, "lab": "decision", "title": "t", "form": null,
            "plan": plan.iter().map(|(m, v)| serde_json::json!({"model": m, "variant": v})).collect::<Vec<_>>(),
            "override": null, "created_at": "", "runs": runs, "artifacts": []
        }))
        .unwrap()
    }

    fn run(cell: u32, answers: &str) -> serde_json::Value {
        serde_json::json!({
            "id": cell, "group_id": 4, "cell": cell, "variant": "x", "endpoint": "/v1/systemone",
            "model_asked": "m", "answered_by": "m", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null, "request": "{}",
            "response": format!(r#"{{"model":"m","answers":{answers},"usage":{{"input_tokens":1,"output_tokens":0}}}}"#),
            "response_type": null, "headers": {}, "input_tokens": 1, "output_tokens": 0,
            "latency_ms": 5, "created_at": "", "artifacts": []
        })
    }

    #[test]
    fn a_matrix_cell_says_why_a_200_is_no_answer() {
        let g = group(&[("m", "as_written"), ("m", "reversed")], vec![run(0, "7")]);
        let read = answer_of(&g.runs[0]);
        let (text, cls) = cell_text(Some(&read), "team", false);
        assert!(text.starts_with("not an answer: "), "{text}");
        assert_eq!(cls, "bad");
        let mut failed = g.runs[0].clone();
        failed.http_status = 503;
        failed.error_code = Some("gpu_hold".into());
        assert_eq!(
            cell_text(Some(&answer_of(&failed)), "team", false),
            ("HTTP 503 · gpu_hold".to_string(), "bad")
        );
        assert_eq!(cell_text(None, "team", true), ("…".to_string(), "dim"));
        assert_eq!(cell_text(None, "team", false), ("—".to_string(), "dim"));
    }

    #[test]
    fn a_card_follows_its_own_notes() {
        let n = |group, cell| CellNote {
            group,
            cell,
            text: format!("{group}/{cell}"),
        };
        let all = [n(1, 0), n(2, 0), n(1, 2)];
        let mine: Vec<String> = notes_of(&all, 1).into_iter().map(|n| n.text).collect();
        assert_eq!(mine, ["1/0", "1/2"]);
    }

    #[test]
    fn labels_and_summaries() {
        assert_eq!(
            cell_label("decide/laya", "reversed"),
            "decide/laya · order reversed"
        );
        let g = group(
            &[
                ("decide/laya", "as_written"),
                ("decide/laya", "reversed"),
                ("decide/kev-4b", "as_written"),
                ("decide/kev-4b", "reversed"),
            ],
            vec![],
        );
        assert_eq!(
            plan_summary(&g),
            "4 cells · decide/laya, decide/kev-4b × as written, order reversed"
        );
        let one = group(&[("decide/laya", "as_written")], vec![]);
        assert_eq!(plan_summary(&one), "decide/laya");
    }

    #[test]
    fn delta_lines_compare_each_variant_with_as_written() {
        let base = r#"{"team":{"type":"choice","choice":"billing","probabilities":{"billing":0.81,"other":0.19},"confidence":0.6},"urgency":{"type":"score","score":1.2,"legend":{"0":"a","1":"b"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.2}}"#;
        let nodesc = r#"{"team":{"type":"choice","choice":"billing","probabilities":{"billing":0.42,"other":0.58},"confidence":0.1},"urgency":{"type":"score","score":1.2,"legend":{"0":"a","1":"b"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.2}}"#;
        let rev = r#"{"team":{"type":"choice","choice":"other","probabilities":{"other":0.7,"billing":0.3},"confidence":0.4}}"#;
        let g = group(
            &[
                ("decide/laya", "as_written"),
                ("decide/laya", "no_descriptions"),
                ("decide/laya", "reversed"),
            ],
            vec![run(0, base), run(1, nodesc), run(2, rev)],
        );
        assert_eq!(
            deltas(&g),
            [
                "team: winner unchanged · billing 0.81 → 0.42 without descriptions",
                "team: winner changed under reversal: billing 0.81 → other 0.70",
            ]
        );
        assert_eq!(question_ids(&g), ["team", "urgency"]);
    }
}
