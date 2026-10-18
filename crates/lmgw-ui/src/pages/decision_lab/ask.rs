//! Asking (labs-store design §2.6): Ask stores the group and its plan, then
//! the page posts its cells one at a time, so each latency is the cell's
//! own and each card fills in as its cells land. Stop stops posting and
//! aborts the cell in flight; a group with fewer runs than cells says so and
//! offers the rest.
//!
//! Also the action bar's line: what Ask will do, or why it cannot yet.

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::decision_lab::metrics::is_2xx;
use lmgw_api_types::decision_lab::{DecisionAsk, DecisionForm, PERTURB_WITH_OVERRIDE};
use lmgw_api_types::lab_store::{LabCellResult, LabGroupView};
use lmgw_api_types::systemone::Ordered;
use serde::de::IgnoredAny;

use super::super::lab_frame::{Check, LabView};
use super::page::Lab;
use crate::api;
use crate::widgets::model_picker::ListStatus;

fn n_of(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What Ask will do: "Ask decide/laya · 3 questions", or for several cells
/// "Ask 6: 2 models × 3 variants".
pub(super) fn plan_line(
    models: &[String],
    variants: usize,
    questions: usize,
    with_override: bool,
) -> String {
    let per_model = 1 + variants;
    let cells = models.len() * per_model;
    if cells == 1 {
        let what = if with_override {
            "the override".to_string()
        } else {
            n_of(questions, "question", "questions")
        };
        return format!("Ask {} · {what}", models[0]);
    }
    format!(
        "Ask {cells}: {} × {}",
        n_of(models.len(), "model", "models"),
        n_of(per_model, "variant", "variants")
    )
}

/// What the action bar's line reads from.
pub(super) struct CheckIn<'a> {
    pub list: &'a ListStatus,
    pub listed: usize,
    /// The head model first, then the compared ones.
    pub models: &'a [String],
    pub form: &'a DecisionForm,
    /// Whether the override is a JSON object; `None` without one.
    pub override_ok: Option<bool>,
    /// The checked perturbations.
    pub variants: usize,
    /// What to add to a sendable line (a hold, a cold start).
    pub note: Option<String>,
}

/// Is `text` a JSON object? The override's only check (§2.4): a lab exists
/// to see what the route says, so anything else goes out as typed.
pub(super) fn is_object(text: &str) -> bool {
    serde_json::from_str::<Ordered<IgnoredAny>>(text).is_ok()
}

/// The action bar's line.
pub(super) fn check(c: CheckIn<'_>) -> Check {
    if c.listed == 0 {
        return Check::Needs(match c.list {
            ListStatus::Loading => "loading the models that decide…".into(),
            ListStatus::Failed(_) => "the model list did not load".into(),
            ListStatus::Ready => "no model here answers /v1/systemone".into(),
        });
    }
    if c.models.is_empty() || c.models[0].trim().is_empty() {
        return Check::Needs("pick a model".into());
    }
    if let Some(ok) = c.override_ok {
        if c.variants > 0 {
            return Check::Invalid(PERTURB_WITH_OVERRIDE.into());
        }
        if !ok {
            return Check::Invalid("override: not a JSON object".into());
        }
    } else if let Err(e) = c.form.check() {
        let missing = c.form.state.trim().is_empty() || c.form.questions.is_empty();
        return if missing {
            Check::Needs(e)
        } else {
            Check::Invalid(e)
        };
    }
    let mut line = plan_line(
        c.models,
        c.variants,
        c.form.questions.len(),
        c.override_ok.is_some(),
    );
    if let Some(n) = c.note {
        line.push_str(" · ");
        line.push_str(&n);
    }
    Check::Ready(line)
}

/// The cells of `g` that have no run yet, in plan order.
pub(super) fn open_cells(g: &LabGroupView) -> Vec<u32> {
    (0..g.plan.len() as u32)
        .filter(|c| !g.runs.iter().any(|r| r.cell == *c))
        .collect()
}

/// Ask: store the group with its plan, then run its cells.
pub(super) fn ask(lab: Lab) {
    if lab.running.get_untracked() {
        return;
    }
    let body = untrack(|| DecisionAsk {
        form: lab.form.draft(),
        models: lab.asked(),
        variants: lab.variants(),
        override_text: lab.override_text.get(),
    });
    lab.view.set(LabView::Results);
    lab.running.set(true);
    lab.stop.set_value(false);
    lab.scope.spawn(async move {
        match api::post::<LabGroupView, _>("/decision-lab/api/groups", &body).await {
            Ok(g) => {
                let id = g.id;
                let cells = open_cells(&g);
                lab.history.prepend(g);
                run_cells(lab, id, cells).await;
            }
            Err(e) => {
                lab.toasts.err(format!("Ask failed: {e}"));
                lab.running.set(false);
            }
        }
    });
}

/// "Run the rest" of a stopped group.
pub(super) fn run_rest(lab: Lab, group_id: i64) {
    if lab.running.get_untracked() {
        return;
    }
    let Some(cells) = lab
        .history
        .groups
        .with_untracked(|v| v.iter().find(|g| g.id == group_id).map(open_cells))
    else {
        return;
    };
    lab.running.set(true);
    lab.stop.set_value(false);
    lab.scope.spawn(run_cells(lab, group_id, cells));
}

/// Stop posting, and abort the cell in flight (its handler's future is
/// dropped, which logs the request's 499 as any disconnect does).
pub(super) fn stop(lab: Lab) {
    lab.stop.set_value(true);
    if let Some(a) = lab.aborter.get_value() {
        a.abort();
    }
}

async fn run_cells(lab: Lab, group_id: i64, cells: Vec<u32>) {
    for cell in cells {
        if lab.stop.get_value() {
            break;
        }
        lab.in_flight.set(Some((group_id, cell)));
        let ctrl = web_sys::AbortController::new().ok();
        let signal = ctrl.as_ref().map(|c| c.signal());
        lab.aborter.set_value(ctrl);
        let sent = Request::post(&format!("/decision-lab/api/groups/{group_id}/cells/{cell}"))
            .abort_signal(signal.as_ref())
            .send()
            .await;
        lab.aborter.set_value(None);
        let resp = match sent {
            Err(e) => {
                let e = e.to_string();
                if !e.to_lowercase().contains("abort") {
                    lab.note_cell(group_id, cell, format!("not sent: {e}"));
                }
                break;
            }
            // A refusal is read as every call reads it: a `401
            // session_required` raises the login gate.
            Ok(resp) => api::checked(resp).await,
        };
        match resp {
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                match serde_json::from_str::<LabCellResult>(&text) {
                    Ok(r) => {
                        match r.store_error {
                            Some(e) => lab.note_cell(group_id, cell, format!("not kept: {e}")),
                            None => lab.clear_note(group_id, cell),
                        }
                        if is_2xx(i64::from(r.run.http_status)) {
                            if let Some(t) = r.run.input_tokens {
                                lab.measured.set(Some((r.run.model_asked.clone(), t)));
                            }
                        }
                        lab.history.put_run(group_id, r.run);
                    }
                    Err(e) => lab.note_cell(group_id, cell, format!("bad answer: {e}")),
                }
            }
            // The lab's own refusal (a 409 for a cell answered meanwhile, a
            // 500 for a form that no longer builds): no run, the cell says
            // why. Without a session no further cell is let in either.
            Err(api::Error::Api(err)) => {
                lab.note_cell(group_id, cell, format!("{} ({})", err.message, err.code));
                if err.code == "session_required" {
                    break;
                }
            }
            Err(e) => lab.note_cell(group_id, cell, format!("bad answer: {e}")),
        }
    }
    lab.in_flight.set(None);
    lab.running.set(false);
    lab.history.refresh_usage();
    // A first ask starts the container; its /props fill the facts.
    lab.reload_models();
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::examples;

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_plan_line_counts_cells() {
        assert_eq!(
            plan_line(&models(&["decide/laya"]), 0, 3, false),
            "Ask decide/laya · 3 questions"
        );
        assert_eq!(
            plan_line(&models(&["decide/laya"]), 0, 1, true),
            "Ask decide/laya · the override"
        );
        assert_eq!(
            plan_line(&models(&["decide/laya", "decide/kev-4b"]), 2, 3, false),
            "Ask 6: 2 models × 3 variants"
        );
        assert_eq!(
            plan_line(&models(&["decide/laya"]), 1, 3, false),
            "Ask 2: 1 model × 2 variants"
        );
    }

    fn input<'a>(form: &'a DecisionForm, models: &'a [String]) -> CheckIn<'a> {
        CheckIn {
            list: &ListStatus::Ready,
            listed: 2,
            models,
            form,
            override_ok: None,
            variants: 0,
            note: None,
        }
    }

    #[test]
    fn check_states() {
        let triage = examples().into_iter().next().unwrap().form;
        let m = models(&["decide/laya"]);
        assert_eq!(
            check(input(&triage, &m)),
            Check::Ready("Ask decide/laya · 3 questions".into())
        );
        // the list first
        let mut c = input(&triage, &m);
        c.listed = 0;
        c.list = &ListStatus::Loading;
        assert!(matches!(check(c), Check::Needs(s) if s.contains("loading")));
        // no model picked
        assert!(matches!(check(input(&triage, &[])), Check::Needs(s) if s == "pick a model"));
        // nothing written yet is something to fill in, not an error
        let empty = DecisionForm::default();
        assert!(matches!(check(input(&empty, &m)), Check::Needs(_)));
        // a JSON state that does not parse is invalid
        let mut bad = triage.clone();
        bad.state_mode = lmgw_api_types::decision_lab::StateMode::Json;
        bad.state = "{\"a\":".into();
        assert!(matches!(check(input(&bad, &m)), Check::Invalid(s) if s.starts_with("state")));
        // an override is checked as an object only, and takes no perturbation
        assert!(is_object("{\"model\":\"x\",\"questions\":7}"));
        let mut c = input(&empty, &m);
        c.override_ok = Some(true);
        assert_eq!(
            check(c),
            Check::Ready("Ask decide/laya · the override".into())
        );
        assert!(!is_object("[1]"));
        let mut c = input(&empty, &m);
        c.override_ok = Some(false);
        assert!(matches!(check(c), Check::Invalid(s) if s.contains("JSON object")));
        let mut c = input(&triage, &m);
        c.override_ok = Some(true);
        c.variants = 1;
        assert_eq!(check(c), Check::Invalid(PERTURB_WITH_OVERRIDE.into()));
        // a note rides on a sendable line
        let mut c = input(&triage, &m);
        c.note = Some("the first ask starts decide/laya".into());
        assert_eq!(
            check(c),
            Check::Ready("Ask decide/laya · 3 questions · the first ask starts decide/laya".into())
        );
    }

    #[test]
    fn the_open_cells_are_those_without_a_run() {
        let v = serde_json::json!({
            "id": 1, "lab": "decision", "title": "t", "form": null,
            "plan": [
                {"model": "a", "variant": "as_written"},
                {"model": "a", "variant": "reversed"},
                {"model": "b", "variant": "as_written"}
            ],
            "override": null, "created_at": "", "runs": [], "artifacts": []
        });
        let mut g: LabGroupView = serde_json::from_value(v).unwrap();
        assert_eq!(open_cells(&g), [0, 1, 2]);
        let mut run: lmgw_api_types::lab_store::LabRunView = serde_json::from_value(serde_json::json!({
            "id": 3, "group_id": 1, "cell": 1, "variant": "reversed", "endpoint": "/v1/systemone",
            "model_asked": "a", "answered_by": "a", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null, "request": "{}",
            "response": null, "response_type": null, "headers": {}, "input_tokens": null,
            "output_tokens": null, "latency_ms": 1, "created_at": "", "artifacts": []
        }))
        .unwrap();
        g.runs.push(run.clone());
        run.cell = 0;
        g.runs.push(run);
        assert_eq!(open_cells(&g), [2]);
    }
}
