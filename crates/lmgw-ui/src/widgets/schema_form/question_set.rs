//! A question-set field (`integer` + `format: lmgw-question-set`, labs-store
//! design §4): a select of the owner's Decision lab question sets, read
//! from `/labs/api/decision/presets?kind=question_set`, its value the set's
//! id; the empty choice classifies with the model call. Beside it the
//! manifest's own `decide` view: the categories field is not read while a
//! set is chosen, and a stored set that cannot classify says why.

use leptos::prelude::*;
use lmgw_api_types::lab_store::{LabPresetBody, LabPresetList, LabPresetView};
use lmgw_api_types::AgentDecide;

use super::{Draft, DraftSelect};

/// The presets route the select reads.
const SETS: &str = "/labs/api/decision/presets?kind=question_set";

/// The select's choices: none, then every set (`name · model`); a held id
/// that is in no list stays listed, so the select shows what the form
/// holds: "not there" once the list has loaded (`sets` is `Some`), else
/// only its id, while the sets load or when their read failed.
pub fn set_options(sets: Option<&[LabPresetView]>, held: &str) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "none · classify with the model".to_string())];
    for p in sets.unwrap_or_default() {
        // The route reads question sets only; another lab's kind is none.
        let LabPresetBody::QuestionSet(s) = &p.preset else {
            continue;
        };
        let model = match s.default_model.trim() {
            "" => "no model",
            m => m,
        };
        out.push((p.id.to_string(), format!("{} · {model}", p.name)));
    }
    let held = held.trim();
    if !held.is_empty() && !out.iter().any(|(v, _)| v == held) {
        let label = match sets {
            Some(_) => format!("set {held} · not there"),
            None => format!("set {held}"),
        };
        out.push((held.to_string(), label));
    }
    out
}

/// What the categories field (`decide.enum_from`) says while the form holds
/// a set: it is not read, the set's question gives the categories.
pub fn categories_note(decide: &AgentDecide, held: &str) -> Option<String> {
    if held.trim().is_empty() {
        return None;
    }
    Some(format!(
        "Not read while a question set is chosen: the set's choice question '{}' gives the \
         categories.",
        decide.question
    ))
}

/// What the set field says under its control about the stored set: why it
/// cannot classify, or where attention starts. Only while the form holds
/// the stored set: another choice is not judged until it is saved.
pub fn set_note(decide: &AgentDecide, held: &str) -> Option<(bool, String)> {
    let stored = decide.set.map(|s| s.to_string())?;
    if held.trim() != stored {
        return None;
    }
    if let Some(p) = &decide.problem {
        return Some((true, p.clone()));
    }
    let t = decide.threshold?;
    let q = &decide.question;
    Some((
        false,
        if decide.measured && t > 0.0 {
            format!(
                "A row needs attention below the set's measured threshold of {t} for '{q}', on \
                 the fallback, or when its call failed."
            )
        } else if t > 0.0 {
            format!(
                "The set's threshold for '{q}' is {t}, not measured: not applied. A row needs \
                 attention on the fallback or when its call failed."
            )
        } else {
            format!(
                "The set has no measured threshold for '{q}': a row needs attention on the \
                 fallback or when its call failed."
            )
        },
    ))
}

#[component]
pub(super) fn QuestionSetSelect(name: String, draft: Draft) -> impl IntoView {
    let key = StoredValue::new(name.clone());
    let sets = LocalResource::new(|| crate::api::get::<LabPresetList>(SETS));
    let options = Signal::derive(move || {
        let held = draft.text(&key.get_value());
        let list = sets.get().and_then(|r| r.ok()).map(|l| l.presets);
        set_options(list.as_deref(), &held)
    });
    let failed = move || {
        sets.get()
            .and_then(|r| r.err())
            .map(|e| format!("the question sets could not be read: {e}"))
    };
    // A fragment, so the select is the control cell's own child and takes
    // its width as every other select does.
    view! {
        <DraftSelect name=name draft=draft options=options/>
        {move || {
            failed()
                .map(|e| {
                    view! {
                        <div class="notice err row">
                            {e}
                            <button class="btn ghost sm" on:click=move |_| sets.refetch()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::decision_lab::QuestionSet;
    use lmgw_api_types::lab_store::LabKind;

    use super::*;

    fn preset(id: i64, name: &str, model: &str) -> LabPresetView {
        LabPresetView {
            id,
            lab: LabKind::Decision,
            name: name.into(),
            preset: LabPresetBody::QuestionSet(QuestionSet {
                default_model: model.into(),
                ..Default::default()
            }),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn decide() -> AgentDecide {
        AgentDecide {
            field: "decision_set".into(),
            question: "category".into(),
            enum_from: Some("categories".into()),
            set: Some(3),
            set_name: Some("mail".into()),
            threshold: Some(0.72),
            measured: true,
            model_field: Some("model".into()),
            problem: None,
        }
    }

    #[test]
    fn the_select_lists_none_then_each_set_and_keeps_a_gone_one_it_holds() {
        let sets = [preset(3, "mail", "decide/laya"), preset(5, "draft", "")];
        assert_eq!(
            set_options(Some(&sets[..]), "3"),
            [
                (String::new(), "none · classify with the model".to_string()),
                ("3".into(), "mail · decide/laya".into()),
                ("5".into(), "draft · no model".into()),
            ]
        );
        assert_eq!(
            set_options(Some(&sets[..]), "9").last().unwrap(),
            &("9".to_string(), "set 9 · not there".to_string())
        );
    }

    #[test]
    fn a_held_set_is_not_there_only_once_the_list_has_loaded() {
        // Loading, or a read that failed: the id alone, no verdict.
        assert_eq!(
            set_options(None, "3"),
            [
                (String::new(), "none · classify with the model".to_string()),
                ("3".into(), "set 3".into()),
            ]
        );
        assert_eq!(set_options(None, "").len(), 1);
        assert_eq!(
            set_options(Some(&[][..]), "3").last().unwrap().1,
            "set 3 · not there"
        );
    }

    #[test]
    fn the_notes_follow_what_the_form_holds() {
        let d = decide();
        assert_eq!(
            categories_note(&d, ""),
            None,
            "no set: the categories are read"
        );
        assert!(categories_note(&d, "5").unwrap().contains("'category'"));
        // The stored set's measured threshold, only while the form still
        // holds it.
        let (problem, note) = set_note(&d, "3").unwrap();
        assert!(
            !problem && note.contains("measured threshold of 0.72") && note.contains("fallback"),
            "{note}"
        );
        assert_eq!(set_note(&d, "5"), None);
        let mut gone = d.clone();
        gone.problem = Some("question set 3 is not there".into());
        assert_eq!(
            set_note(&gone, "3"),
            Some((true, "question set 3 is not there".to_string()))
        );
        // Kept but not measured: shown, not applied (Decision 10).
        let mut unmeasured = d;
        unmeasured.measured = false;
        let (_, note) = set_note(&unmeasured, "3").unwrap();
        assert!(
            note.contains("is 0.72, not measured: not applied"),
            "{note}"
        );
        unmeasured.threshold = Some(0.0);
        let (_, note) = set_note(&unmeasured, "3").unwrap();
        assert!(note.contains("no measured threshold"), "{note}");
    }
}
