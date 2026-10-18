//! The Decision lab's page (labs-store design §2.1, §3.2): the `Lab` state
//! every child reads, and its wiring onto the labs' shared layout — the head
//! is the model and its facts, the form column the examples, the question
//! set, state, images, questions, compare and perturb, the gauge and the
//! request; the results column the kept history; the reference panel the
//! saved sets, the set runs, the model's limits and notes and the question
//! types' rules. "New set run…" opens its modal over the page.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{DecisionLabModel, DecisionLabModels, Variant};
use lmgw_api_types::lab_store::{LabGroupView, LabKind};

use super::super::lab_frame::{LabFrame, LabView};
use super::super::lab_history::{ExportMenu, HistoryList, LabHistory};
use super::ask::{self, CheckIn};
use super::batch_list::{self, Runs};
use super::batch_new::{NewRun, NewRunModal};
use super::compare::CompareCard;
use super::examples_menu::ExamplesMenu;
use super::form_state::FormState;
use super::gauge::{self, Gauge, GaugeCard};
use super::group_card::GroupCard;
use super::head::{first_pick, Head};
use super::images::ImagesCard;
use super::questions::QuestionsCard;
use super::reference::Reference;
use super::request::RequestCard;
use super::set_card::SetCard;
use super::sets::{self, Sets};
use super::state_editor::StateCard;
use crate::api;
use crate::scope::Scope;
use crate::widgets::model_picker::ListStatus;
use crate::widgets::{use_toasts, Toasts};

/// A cell's own note beside its run, or in place of one: "not kept: …" when
/// the store failed, the lab's refusal when it was not dispatched.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CellNote {
    pub group: i64,
    pub cell: u32,
    pub text: String,
}

/// The note `g`'s card shows for `cell`: the cell's newest, unless a kept
/// run answers the cell now (it landed, or a reload brought it), which says
/// more than a note from before it.
pub(super) fn note_for(notes: &[CellNote], g: &LabGroupView, cell: u32) -> Option<String> {
    if g.runs.iter().any(|r| r.cell == cell && r.id.is_some()) {
        return None;
    }
    notes
        .iter()
        .rev()
        .find(|n| n.group == g.id && n.cell == cell)
        .map(|n| n.text.clone())
}

/// The page's state. `Copy`: every child takes it whole.
#[derive(Clone, Copy)]
pub(super) struct Lab {
    pub models: RwSignal<Vec<DecisionLabModel>>,
    pub list: RwSignal<ListStatus>,
    /// The GPU hold is on.
    pub hold: RwSignal<bool>,
    /// The head model: the first asked, and the one the preview and the
    /// gauge are for.
    pub alias: RwSignal<String>,
    /// "Also ask on".
    pub compare: RwSignal<Vec<String>>,
    pub no_descriptions: RwSignal<bool>,
    pub reversed: RwSignal<bool>,
    pub form: FormState,
    /// The request override: sent instead of the form while it is set.
    pub override_text: RwSignal<Option<String>>,
    /// Is the override a JSON object? `None` without one. Read once per
    /// edit, by the action bar and the override's own line.
    pub override_ok: Memo<Option<bool>>,
    pub gauge: RwSignal<Gauge>,
    /// The last answer's measured `input_tokens`, and the model asked.
    pub measured: RwSignal<Option<(String, u64)>>,
    pub history: LabHistory,
    pub running: RwSignal<bool>,
    /// The cell being asked: (group, cell).
    pub in_flight: RwSignal<Option<(i64, u32)>>,
    pub notes: RwSignal<Vec<CellNote>>,
    /// Stop was pressed: no further cell is posted.
    pub stop: StoredValue<bool>,
    pub aborter: StoredValue<Option<web_sys::AbortController>>,
    /// Which count is the newest: an older one that lands late is dropped.
    pub counts: StoredValue<u64>,
    pub view: RwSignal<LabView>,
    /// The saved question sets and the one loaded.
    pub sets: Sets,
    /// The set runs, newest first.
    pub runs: Runs,
    /// The New set run modal's inputs.
    pub new_run: NewRun,
    pub scope: Scope,
    pub toasts: Toasts,
}

impl Lab {
    pub fn model_of(self, name: &str) -> Option<DecisionLabModel> {
        self.models
            .with(|v| v.iter().find(|m| m.name == name).cloned())
    }

    /// The head model's entry.
    pub fn model(self) -> Option<DecisionLabModel> {
        self.model_of(&self.alias.get())
    }

    /// The models Ask asks: the head model, then the compared ones.
    pub fn asked(self) -> Vec<String> {
        let mut out = Vec::new();
        let head = self.alias.get();
        if !head.trim().is_empty() {
            out.push(head);
        }
        for m in self.compare.get() {
            if !m.trim().is_empty() && !out.contains(&m) {
                out.push(m);
            }
        }
        out
    }

    /// The checked perturbations; none while an override is in effect.
    pub fn variants(self) -> Vec<Variant> {
        if self.override_text.with(Option::is_some) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.no_descriptions.get() {
            out.push(Variant::NoDescriptions);
        }
        if self.reversed.get() {
            out.push(Variant::Reversed);
        }
        out
    }

    /// A local model's container state, the live frame first: `None` for a
    /// cloud model and for a row with no container.
    pub fn state_of(self, m: &DecisionLabModel) -> Option<String> {
        let id = m.model_id.clone()?;
        if let Some(rows) = crate::live::use_live().runtime.get() {
            return rows
                .into_iter()
                .find(|r| r.class == "aux" && r.model_id == id)
                .map(|r| r.state);
        }
        m.state.clone()
    }

    /// A ticket for a count about to go out.
    pub fn count_ticket(self) -> u64 {
        self.counts
            .try_update_value(|n| {
                *n += 1;
                *n
            })
            .unwrap_or_default()
    }

    /// Is `ticket` the newest count's?
    pub fn count_is(self, ticket: u64) -> bool {
        self.counts.try_get_value() == Some(ticket)
    }

    /// The cell's note, in place of an earlier one.
    pub fn note_cell(self, group: i64, cell: u32, text: String) {
        self.notes.update(|v| {
            v.retain(|n| (n.group, n.cell) != (group, cell));
            v.push(CellNote { group, cell, text });
        });
    }

    /// The cell's run landed: a note from before it is stale.
    pub fn clear_note(self, group: i64, cell: u32) {
        let has = self
            .notes
            .with_untracked(|v| v.iter().any(|n| (n.group, n.cell) == (group, cell)));
        if has {
            self.notes
                .update(|v| v.retain(|n| (n.group, n.cell) != (group, cell)));
        }
    }

    pub fn reload_models(self) {
        self.scope.spawn(async move {
            match api::get::<DecisionLabModels>("/decision-lab/api/models").await {
                Ok(m) => {
                    self.hold.set(m.hold);
                    let first = first_pick(&m.models);
                    self.models.set(m.models);
                    self.list.set(ListStatus::Ready);
                    if self.alias.get_untracked().is_empty() {
                        if let Some(a) = first {
                            self.alias.set(a);
                        }
                    }
                }
                Err(e) => {
                    self.models.set(Vec::new());
                    self.list.set(ListStatus::Failed(e.to_string()));
                }
            }
        });
    }

    /// What a sendable line adds: a held head model, or a cold start.
    fn note(self) -> Option<String> {
        let m = self.model()?;
        if !m.local {
            return None;
        }
        if m.held {
            return Some(match m.hold_fallback {
                Some(f) => format!("held: {f} answers"),
                None => "held: no fallback, it answers 503".into(),
            });
        }
        (self.state_of(&m).as_deref() != Some("ready"))
            .then(|| format!("the first ask starts {}", m.name))
    }

    fn check(self) -> super::super::lab_frame::Check {
        // The check reads no image: the text alone, not a copy of each data
        // URL per keystroke.
        let form = self.form.text_draft();
        let models = self.asked();
        let list = self.list.get();
        let override_ok = self.override_ok.get();
        let perturb = usize::from(self.no_descriptions.get()) + usize::from(self.reversed.get());
        ask::check(CheckIn {
            list: &list,
            listed: self.models.with(Vec::len),
            models: &models,
            form: &form,
            override_ok,
            // Unchecked while the override is in effect: Perturb is off then.
            variants: if override_ok.is_some() { 0 } else { perturb },
            note: self.note(),
        })
    }
}

#[component]
pub fn DecisionLab() -> impl IntoView {
    let scope = Scope::new();
    let override_text = RwSignal::new(None::<String>);
    let override_ok = Memo::new(move |_| override_text.with(|o| o.as_deref().map(ask::is_object)));
    let lab = Lab {
        models: RwSignal::new(Vec::new()),
        list: RwSignal::new(ListStatus::Loading),
        hold: RwSignal::new(false),
        alias: RwSignal::new(String::new()),
        compare: RwSignal::new(Vec::new()),
        no_descriptions: RwSignal::new(false),
        reversed: RwSignal::new(false),
        form: FormState::new(),
        override_text,
        override_ok,
        gauge: RwSignal::new(Gauge::Idle),
        measured: RwSignal::new(None),
        history: LabHistory::new(LabKind::Decision, scope),
        running: RwSignal::new(false),
        in_flight: RwSignal::new(None),
        notes: RwSignal::new(Vec::new()),
        stop: StoredValue::new(false),
        aborter: StoredValue::new(None),
        counts: StoredValue::new(0),
        view: RwSignal::new(LabView::Form),
        sets: Sets::new(),
        runs: Runs::new(),
        new_run: NewRun::new(),
        scope,
        toasts: use_toasts(),
    };
    lab.reload_models();
    sets::reload(lab);
    batch_list::reload(lab);
    batch_list::follow(lab);
    gauge::auto_count(lab);
    on_cleanup(move || {
        lab.aborter
            .try_with_value(|a| a.as_ref().map(web_sys::AbortController::abort));
    });

    let check = Memo::new(move |_| lab.check());
    let filter_models = Signal::derive(move || {
        lab.models
            .with(|v| v.iter().map(|m| m.name.clone()).collect::<Vec<_>>())
    });
    let usage = Signal::derive(move || lab.history.usage.get());
    let card = Callback::new(move |id: i64| view! { <GroupCard lab=lab id=id/> }.into_any());

    view! {
        <LabFrame
            persist="decision-lab.reference"
            side_label="Reference"
            side_badge=Signal::derive(move || match lab.runs.running() {
                0 => String::new(),
                n => format!("{n} running"),
            })
            side=move || view! { <Reference lab=lab/> }
            head=move || view! { <Head lab=lab/> }
            form=move || {
                view! {
                    <ExamplesMenu lab=lab/>
                    <SetCard lab=lab/>
                    <StateCard lab=lab/>
                    <ImagesCard lab=lab/>
                    <QuestionsCard lab=lab/>
                    <CompareCard lab=lab/>
                    <GaugeCard lab=lab/>
                    <RequestCard lab=lab/>
                    <NewRunModal lab=lab/>
                }
            }
            results=move || view! { <HistoryList history=lab.history card=card/> }
            results_n=Signal::derive(move || lab.history.total.get() as usize)
            check=check
            run_label=Signal::derive(|| "Ask".to_string())
            running=lab.running
            on_run=Callback::new(move |()| ask::ask(lab))
            on_stop=Callback::new(move |()| ask::stop(lab))
            on_clear=Callback::new(move |()| lab.history.clear())
            view=lab.view
            kept=usage
            results_tools=move || view! { <ExportMenu history=lab.history models=filter_models/> }
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(runs: &[(u32, Option<i64>)]) -> LabGroupView {
        let runs: Vec<serde_json::Value> = runs
            .iter()
            .map(|(cell, id)| {
                serde_json::json!({
                    "id": id, "group_id": 4, "cell": cell, "variant": "as_written",
                    "endpoint": "/v1/systemone", "model_asked": "m", "answered_by": "m",
                    "fallback_reason": null, "upstream_model": null, "http_status": 200,
                    "error_code": null, "error_message": null, "request": "{}",
                    "response": null, "response_type": null, "headers": {},
                    "input_tokens": null, "output_tokens": null, "latency_ms": 1,
                    "created_at": "", "artifacts": []
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": 4, "lab": "decision", "title": "t", "form": null,
            "plan": [{"model": "m", "variant": "as_written"}, {"model": "m", "variant": "reversed"}],
            "override": null, "created_at": "", "runs": runs, "artifacts": []
        }))
        .unwrap()
    }

    fn note(group: i64, cell: u32, text: &str) -> CellNote {
        CellNote {
            group,
            cell,
            text: text.into(),
        }
    }

    #[test]
    fn a_note_stands_until_a_kept_run_answers_its_cell() {
        let notes = [
            note(4, 1, "not sent: offline"),
            note(5, 1, "another group's"),
            note(4, 0, "not kept: disk full"),
        ];
        let none = group(&[]);
        assert_eq!(
            note_for(&notes, &none, 1).as_deref(),
            Some("not sent: offline")
        );
        // a run the store could not keep: its note says why, beside it
        let unkept = group(&[(0, None)]);
        assert_eq!(
            note_for(&notes, &unkept, 0).as_deref(),
            Some("not kept: disk full")
        );
        // landed, or brought by a reload: the note is gone
        let landed = group(&[(0, Some(10)), (1, Some(11))]);
        assert_eq!(note_for(&notes, &landed, 0), None);
        assert_eq!(note_for(&notes, &landed, 1), None);
    }
}
