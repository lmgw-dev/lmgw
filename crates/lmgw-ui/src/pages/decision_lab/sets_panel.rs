//! The reference panel's question sets (labs-store design §3.2): each saved
//! set with its questions, its model and how many of its thresholds are
//! measured; Load, Rename and Delete (with a confirm; the set runs that ran
//! it keep their snapshot). While the form's questions have edits, Load
//! arms first and loads on the second press, as the set card's Load menu
//! does.

use leptos::prelude::*;
use lmgw_api_types::lab_store::LabPresetView;

use super::page::Lab;
use super::sets::{self, set_of, SetsLoad};
use crate::widgets::ConfirmButton;

/// A set's line under its name: "3 questions · decide/laya · 1 of 3
/// thresholds measured".
pub(super) fn set_summary(p: &LabPresetView) -> String {
    let s = set_of(p);
    let n = s.questions.len();
    let measured = s
        .questions
        .iter()
        .filter(|q| {
            s.thresholds
                .get(&q.id)
                .is_some_and(|t| t.measured.is_some())
        })
        .count();
    let model = if s.default_model.trim().is_empty() {
        "no model"
    } else {
        s.default_model.as_str()
    };
    format!(
        "{n} {} · {model} · {measured} of {n} thresholds measured",
        if n == 1 { "question" } else { "questions" }
    )
}

#[component]
pub(super) fn SetsPanel(lab: Lab) -> impl IntoView {
    let dirty = Memo::new(move |_| sets::questions_dirty(lab));
    view! {
        <h3 class="lab-h3">"Question sets"</h3>
        {move || match lab.sets.load.get() {
            SetsLoad::Loading => view! { <p class="mini-note dim">"Reading the sets…"</p> }.into_any(),
            SetsLoad::Failed(e) => view! {
                <p class="mini-note bad">
                    {format!("The sets did not load: {e} ")}
                    <button class="btn ghost sm" on:click=move |_| sets::reload(lab)>"Retry"</button>
                </p>
            }
            .into_any(),
            SetsLoad::Ready => ().into_any(),
        }}
        <Show when=move || lab.sets.load.get() == SetsLoad::Ready && lab.sets.list.with(Vec::is_empty)>
            <p class="mini-note dim">
                "No saved set: Save as set keeps the form's questions and the head model under a name."
            </p>
        </Show>
        <For each=move || lab.sets.list.get() key=|p| (p.id, p.updated_at.clone(), p.name.clone()) let:p>
            <SetRow lab=lab p=p dirty=dirty/>
        </For>
    }
}

#[component]
fn SetRow(lab: Lab, p: LabPresetView, dirty: Memo<bool>) -> impl IntoView {
    let id = p.id;
    let renaming = RwSignal::new(false);
    let name = RwSignal::new(p.name.clone());
    let err = RwSignal::new(None::<String>);
    let on = move || {
        lab.sets
            .loaded
            .with(|l| l.as_ref().is_some_and(|l| l.id == id))
    };
    let pv = StoredValue::new(p.clone());
    let rename = move || {
        sets::rename(
            lab,
            pv.get_value(),
            name.get_untracked(),
            err,
            Callback::new(move |()| renaming.set(false)),
        )
    };
    let confirm = format!("Delete the set '{}'?", p.name);
    view! {
        <div class="dl-setrow" class:on=on>
            <Show
                when=move || renaming.get()
                fallback=move || {
                    view! {
                        <span class="dl-setrow-head">
                            <b class="n" title=pv.with_value(|p| p.name.clone())>
                                {pv.with_value(|p| p.name.clone())}
                            </b>
                            <Show when=on>
                                <span class="chip info sm">"loaded"</span>
                            </Show>
                        </span>
                    }
                }
            >
                <span class="dl-setname-row">
                    <input
                        class="input"
                        prop:value=move || name.get()
                        on:input=move |ev| name.set(event_target_value(&ev))
                        on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                            "Enter" => rename(),
                            "Escape" => renaming.set(false),
                            _ => {}
                        }
                    />
                    <button class="btn primary sm" on:click=move |_| rename()>"Save"</button>
                    <button class="btn ghost sm" on:click=move |_| renaming.set(false)>"Cancel"</button>
                </span>
            </Show>
            <span class="mini-note dim">{set_summary(&p)}</span>
            {move || err.get().map(|e| view! { <span class="mini-note bad">{e}</span> })}
            <span class="dl-setrow-acts">
                <Show
                    when=move || dirty.get()
                    fallback=move || view! {
                        <button
                            class="btn ghost sm"
                            title="Its questions replace the form's; its model becomes the head model"
                            on:click=move |_| sets::load_set(lab, pv.get_value())
                        >
                            "Load"
                        </button>
                    }
                >
                    <span class="dl-setrow-load">
                        <ConfirmButton
                            label="Load"
                            confirm="Replace the edited questions?"
                            title="Its questions replace the form's, which have edits: press again to replace them"
                            on_confirm=Callback::new(move |()| sets::load_set(lab, pv.get_value()))
                        />
                    </span>
                </Show>
                <button
                    class="btn ghost sm"
                    on:click=move |_| {
                        err.set(None);
                        name.set(pv.with_value(|p| p.name.clone()));
                        renaming.set(true);
                    }
                >
                    "Rename"
                </button>
                <ConfirmButton
                    label="Delete"
                    confirm=confirm
                    title="Delete this set; the set runs that ran it keep what they ran"
                    on_confirm=Callback::new(move |()| sets::delete(lab, id))
                />
            </span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::{examples, Measured, QuestionSet, Threshold};
    use lmgw_api_types::lab_store::{LabKind, LabPresetBody};

    #[test]
    fn a_set_says_its_questions_model_and_measured_thresholds() {
        let set = QuestionSet {
            questions: examples().into_iter().next().unwrap().form.questions,
            default_model: "decide/laya".into(),
            thresholds: [(
                "team",
                Threshold {
                    value: 0.7,
                    measured: Some(Measured {
                        batch_id: 1,
                        coverage: 0.8,
                        accuracy: 0.9,
                        labelled: 10,
                    }),
                },
            )]
            .into_iter()
            .collect(),
        };
        let p = LabPresetView {
            id: 1,
            lab: LabKind::Decision,
            name: "triage".into(),
            preset: LabPresetBody::QuestionSet(set),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(
            set_summary(&p),
            "3 questions · decide/laya · 1 of 3 thresholds measured"
        );
    }
}
