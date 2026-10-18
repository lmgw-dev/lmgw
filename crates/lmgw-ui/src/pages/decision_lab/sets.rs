//! Question sets (labs-store design §3.2, §3.5): the Decision lab's presets
//! of kind `question_set`, read from and written to
//! `/labs/api/decision/presets`. "Save as set" keeps the form's questions and
//! the head model under a name; "Update set" rewrites the loaded set whole,
//! carrying the `updated_at` it read, so a set that moved since (a threshold
//! written from a set run, another window) is the 409 `set_changed`, which
//! the page says and offers to reload; loading a set replaces the questions
//! and picks its model. A threshold the update keeps stays on its question
//! (the server drops a changed question's measurement, not its value), and
//! one whose question left the form goes with it: a set's thresholds name
//! its questions.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{Measured, QuestionDraft, QuestionSet, Threshold};
use lmgw_api_types::lab_store::{
    LabPresetBody, LabPresetList, LabPresetUpdate, LabPresetView, LabPresetWrite,
};
use lmgw_api_types::systemone::Ordered;

use super::page::Lab;
use super::threshold::{cut, measured_text};
use crate::api;
use crate::scope::Latest;

/// The presets route of the Decision lab.
const BASE: &str = "/labs/api/decision/presets";

/// Where the list of sets stands.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SetsLoad {
    Loading,
    Ready,
    Failed(String),
}

/// The lab's question sets. `Copy`, a part of `Lab`.
#[derive(Clone, Copy)]
pub(super) struct Sets {
    /// By name, as the server lists them.
    pub list: RwSignal<Vec<LabPresetView>>,
    pub load: RwSignal<SetsLoad>,
    /// The set the form's questions came from, as last read or written: its
    /// `updated_at` is what an update sends.
    pub loaded: RwSignal<Option<LabPresetView>>,
    /// The loaded set moved on the server since it was read: the 409's
    /// message, shown with Reload until a reload or a dismissal.
    pub conflict: RwSignal<Option<String>>,
    /// Which list read is the newest.
    reads: Latest,
}

impl Sets {
    pub fn new() -> Self {
        Self {
            list: RwSignal::new(Vec::new()),
            load: RwSignal::new(SetsLoad::Loading),
            loaded: RwSignal::new(None),
            conflict: RwSignal::new(None),
            reads: Latest::new(),
        }
    }
}

/// A preset's question set. The lab reads only presets of kind
/// `question_set` (the server keeps each kind to its lab); another kind
/// reads as an empty set.
pub(super) fn set_of(p: &LabPresetView) -> &QuestionSet {
    static NONE: std::sync::LazyLock<QuestionSet> = std::sync::LazyLock::new(QuestionSet::default);
    match &p.preset {
        LabPresetBody::QuestionSet(s) => s,
        LabPresetBody::ImagePrompt(_) | LabPresetBody::AudioSettings(_) => &NONE,
    }
}

/// `kept`'s thresholds of the questions in `questions`, in `kept`'s order:
/// a set's thresholds name its questions, so one whose question left goes.
pub(super) fn thresholds_for(
    questions: &[QuestionDraft],
    kept: &Ordered<Threshold>,
) -> Ordered<Threshold> {
    Ordered(
        kept.0
            .iter()
            .filter(|(id, _)| questions.iter().any(|q| q.id == *id))
            .cloned()
            .collect(),
    )
}

/// How the form differs from the loaded set: its questions, its model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Edits {
    pub questions: bool,
    pub model: bool,
}

impl Edits {
    pub fn any(self) -> bool {
        self.questions || self.model
    }

    /// The card's word for it: "" when the form is the set.
    pub fn text(self) -> &'static str {
        match (self.questions, self.model) {
            (false, false) => "",
            (true, false) => "questions edited",
            (false, true) => "another model",
            (true, true) => "questions edited · another model",
        }
    }
}

/// How `questions` asked on `model` differ from `set`. A set that names no
/// model is not differed from by one.
pub(super) fn edits(set: &QuestionSet, questions: &[QuestionDraft], model: &str) -> Edits {
    Edits {
        questions: set.questions != questions,
        model: !set.default_model.trim().is_empty() && set.default_model != model,
    }
}

/// A question card's threshold line, from the loaded set.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ThresholdText {
    pub text: String,
    /// The set run it was measured on, for the link.
    pub batch: Option<i64>,
}

/// What a question card says of its threshold in the loaded `set`: the
/// value and where it was measured ("threshold 0.72 · 81% covered, 96.4%
/// correct (n = 812) on set run #5"), or "not measured"; a question the set
/// does not hold yet, or holds otherwise, says what an update does to it.
pub(super) fn threshold_text(set: &QuestionSet, q: &QuestionDraft) -> ThresholdText {
    let Some(kept) = set.question(&q.id) else {
        return ThresholdText {
            text: "threshold: not in the set yet (Update set adds it, not measured)".into(),
            batch: None,
        };
    };
    let t = set.thresholds.get(&q.id).cloned().unwrap_or_default();
    let edited = kept != q;
    let mut text = match (&t.measured, t.value) {
        (Some(Measured { batch_id, .. }), v) => format!(
            "threshold {} · {} on set run #{batch_id}",
            cut(v),
            measured_text(
                t.measured
                    .as_ref()
                    .map(|m| (m.coverage, m.accuracy, m.labelled))
            )
        ),
        (None, v) if v > 0.0 => format!("threshold {} · not measured", cut(v)),
        (None, _) => "threshold: not measured".into(),
    };
    if edited {
        text.push_str(if t.measured.is_some() {
            " · edited: Update set keeps the value, not the measurement"
        } else {
            " · edited"
        });
    }
    ThresholdText {
        text,
        batch: t.measured.map(|m| m.batch_id),
    }
}

/// Do the form's questions differ from the ones last loaded (tracked)? A
/// load replaces them, so it asks first then: the set card's Load menu and
/// the reference panel's Load both arm on it.
pub(super) fn questions_dirty(lab: Lab) -> bool {
    let now = lab.form.question_drafts();
    lab.form.loaded.with(|l| l.questions != now)
}

/// Read the lab's sets again.
pub(super) fn reload(lab: Lab) {
    let sets = lab.sets;
    let ticket = sets.reads.next();
    lab.scope.spawn(async move {
        let got = api::get::<LabPresetList>(format!("{BASE}?kind=question_set")).await;
        if !ticket.is_some_and(|t| sets.reads.is(t)) {
            return;
        }
        match got {
            Ok(l) => {
                // The loaded set as the list has it now (a rename elsewhere,
                // a threshold written): its name and stamp, not its questions,
                // which only a load puts in the form.
                if let Some(cur) = sets.loaded.get_untracked() {
                    match l.presets.iter().find(|p| p.id == cur.id) {
                        Some(p) if p.updated_at != cur.updated_at => {
                            if set_of(p).questions == set_of(&cur).questions {
                                sets.loaded.set(Some(p.clone()));
                            }
                        }
                        Some(_) => {}
                        None => sets.loaded.set(None),
                    }
                }
                sets.list.set(l.presets);
                sets.load.set(SetsLoad::Ready);
            }
            Err(e) => sets.load.set(SetsLoad::Failed(e.to_string())),
        }
    });
}

/// The form's questions on the head model as a set, keeping `kept`'s
/// thresholds of the questions it still has.
pub(super) fn form_set(lab: Lab, kept: Option<&QuestionSet>) -> QuestionSet {
    let questions = untrack(|| lab.form.question_drafts());
    let thresholds = kept
        .map(|k| thresholds_for(&questions, &k.thresholds))
        .unwrap_or_default();
    QuestionSet {
        default_model: lab.alias.get_untracked(),
        questions,
        thresholds,
    }
}

/// Put `p` in the list in place of its old self (or add it), and make it the
/// loaded set when it is that one (or `load` says so). A list read already
/// on its way is from before this write: its ticket goes, so it cannot put
/// the old list back.
fn put(lab: Lab, p: LabPresetView, load: bool) {
    lab.sets.reads.next();
    lab.sets.list.update(|v| {
        v.retain(|x| x.id != p.id);
        let at = v.partition_point(|x| x.name < p.name);
        v.insert(at, p.clone());
    });
    let is_loaded = lab
        .sets
        .loaded
        .with_untracked(|l| l.as_ref().is_some_and(|l| l.id == p.id));
    if load || is_loaded {
        lab.sets.loaded.set(Some(p));
    }
}

/// "Save as set": the form's questions and the head model under `name`,
/// which becomes the loaded set. The refusal's message (a name taken, a
/// question that does not check) goes to `err`.
pub(super) fn save_as(lab: Lab, name: String, err: RwSignal<Option<String>>, done: Callback<()>) {
    let body = LabPresetWrite {
        name: name.trim().to_string(),
        preset: LabPresetBody::QuestionSet(form_set(lab, None)),
    };
    lab.scope.spawn(async move {
        match api::post::<LabPresetView, _>(BASE, &body).await {
            Ok(p) => {
                // The form is the set now: its questions are what was loaded.
                lab.form.load_questions(set_of(&p).questions.clone());
                lab.toasts.ok(format!("saved as the set '{}'", p.name));
                put(lab, p, true);
                lab.sets.conflict.set(None);
                err.set(None);
                done.run(());
            }
            Err(e) => err.set(Some(e.to_string())),
        }
    });
}

/// "Update set": the loaded set rewritten whole from the form, if it is
/// still the one read; else the 409 `set_changed` is said with Reload.
pub(super) fn update_loaded(lab: Lab) {
    let Some(cur) = lab.sets.loaded.get_untracked() else {
        return;
    };
    let body = LabPresetUpdate {
        name: cur.name.clone(),
        preset: LabPresetBody::QuestionSet(form_set(lab, Some(set_of(&cur)))),
        updated_at: cur.updated_at.clone(),
    };
    let href = format!("{BASE}/{}", cur.id);
    lab.scope.spawn(async move {
        match api::post::<LabPresetView, _>(href, &body).await {
            Ok(p) => {
                lab.form.load_questions(set_of(&p).questions.clone());
                lab.toasts.ok(format!("the set '{}' is updated", p.name));
                put(lab, p, true);
                lab.sets.conflict.set(None);
            }
            Err(api::Error::Api(e)) if e.code == "set_changed" => {
                lab.sets.conflict.set(Some(e.message));
            }
            Err(e) => lab.toasts.err(format!("Update set failed: {e}")),
        }
    });
}

/// Load `p`: its questions replace the form's (the state and images stay),
/// its model becomes the head model when the lab lists it, and an override
/// (the old form's request) is dropped.
pub(super) fn load_set(lab: Lab, p: LabPresetView) {
    let set = set_of(&p).clone();
    lab.form.load_questions(set.questions);
    lab.override_text.set(None);
    let model = set.default_model.trim().to_string();
    if !model.is_empty() {
        if lab.model_of(&model).is_some() {
            lab.alias.set(model);
        } else {
            lab.toasts.err(format!(
                "the set's model '{model}' is not listed here: the head model stays {}",
                lab.alias.get_untracked()
            ));
        }
    }
    lab.sets.conflict.set(None);
    lab.sets.loaded.set(Some(p));
}

/// Read the loaded set as the server has it now and load it: the answer to
/// a 409 `set_changed`.
pub(super) fn reload_loaded(lab: Lab) {
    let Some(id) = lab.sets.loaded.with_untracked(|l| l.as_ref().map(|l| l.id)) else {
        return;
    };
    lab.scope.spawn(async move {
        match api::get::<LabPresetView>(format!("{BASE}/{id}")).await {
            Ok(p) => {
                put(lab, p.clone(), false);
                load_set(lab, p);
            }
            Err(e) if e.is_not_found() => {
                lab.sets.loaded.set(None);
                lab.sets.conflict.set(None);
                lab.toasts
                    .err("the set was deleted: Save as set keeps the form's questions");
                reload(lab);
            }
            Err(e) => lab.toasts.err(format!("reading the set failed: {e}")),
        }
    });
}

/// Rename `p` to `name`, its set as it is; the refusal goes to `err`.
pub(super) fn rename(
    lab: Lab,
    p: LabPresetView,
    name: String,
    err: RwSignal<Option<String>>,
    done: Callback<()>,
) {
    let body = LabPresetUpdate {
        name: name.trim().to_string(),
        preset: p.preset.clone(),
        updated_at: p.updated_at.clone(),
    };
    let href = format!("{BASE}/{}", p.id);
    lab.scope.spawn(async move {
        match api::post::<LabPresetView, _>(href, &body).await {
            Ok(p) => {
                put(lab, p, false);
                err.set(None);
                done.run(());
            }
            Err(e) => {
                err.set(Some(e.to_string()));
                // A set that moved: the list shows it as it is now.
                if matches!(&e, api::Error::Api(a) if a.code == "set_changed") {
                    reload(lab);
                }
            }
        }
    });
}

/// Delete set `id`; the set runs that ran it keep their snapshot.
pub(super) fn delete(lab: Lab, id: i64) {
    lab.scope.spawn(async move {
        match api::post::<LabPresetList, _>(format!("{BASE}/{id}/delete"), &()).await {
            Ok(l) => {
                lab.sets.list.set(l.presets);
                if lab
                    .sets
                    .loaded
                    .with_untracked(|p| p.as_ref().is_some_and(|p| p.id == id))
                {
                    lab.sets.loaded.set(None);
                    lab.sets.conflict.set(None);
                }
                lab.toasts.ok("set deleted");
            }
            Err(e) => lab.toasts.err(format!("delete failed: {e}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::{examples, QuestionKind};

    fn triage() -> QuestionSet {
        QuestionSet {
            questions: examples().into_iter().next().unwrap().form.questions,
            default_model: "decide/laya".into(),
            thresholds: Ordered::default(),
        }
    }

    fn measured(batch_id: i64) -> Threshold {
        Threshold {
            value: 0.72,
            measured: Some(Measured {
                batch_id,
                coverage: 0.81,
                accuracy: 0.964,
                labelled: 812,
            }),
        }
    }

    #[test]
    fn a_threshold_whose_question_left_goes() {
        let set = triage();
        let kept: Ordered<Threshold> = [("team", measured(1)), ("gone", measured(2))]
            .into_iter()
            .collect();
        let t = thresholds_for(&set.questions, &kept);
        assert_eq!(t.keys().collect::<Vec<_>>(), ["team"]);
    }

    #[test]
    fn the_form_differs_from_its_set_by_questions_or_model() {
        let set = triage();
        assert!(!edits(&set, &set.questions, "decide/laya").any());
        assert_eq!(
            edits(&set, &set.questions, "decide/kev-4b").text(),
            "another model"
        );
        let mut qs = set.questions.clone();
        qs[0].instructions.push('!');
        assert_eq!(
            edits(&set, &qs, "decide/kev-4b").text(),
            "questions edited · another model"
        );
        let mut none = set.clone();
        none.default_model.clear();
        assert!(!edits(&none, &none.questions, "decide/kev-4b").model);
    }

    #[test]
    fn a_question_card_says_its_threshold() {
        let mut set = triage();
        let team = set.questions[0].clone();
        assert_eq!(threshold_text(&set, &team).text, "threshold: not measured");
        set.thresholds = [("team", measured(5))].into_iter().collect();
        let t = threshold_text(&set, &team);
        assert_eq!(
            t.text,
            "threshold 0.72 · 81% covered, 96.4% correct (n = 812) on set run #5"
        );
        assert_eq!(t.batch, Some(5));
        let mut edited = team.clone();
        if let QuestionKind::Choice { options } = &mut edited.kind {
            options.pop();
        }
        assert!(threshold_text(&set, &edited)
            .text
            .ends_with("edited: Update set keeps the value, not the measurement"));
        set.thresholds.0[0].1.measured = None;
        assert_eq!(
            threshold_text(&set, &team).text,
            "threshold 0.72 · not measured"
        );
        let mut new = team;
        new.id = "region".into();
        assert!(threshold_text(&set, &new)
            .text
            .contains("not in the set yet"));
    }
}
