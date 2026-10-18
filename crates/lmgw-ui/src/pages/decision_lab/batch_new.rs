//! A new set run (labs-store design §3.3, §3.5): the form's questions asked
//! of every row of a CSV or JSONL file, pasted or uploaded (the page reads
//! the file and posts its text). `POST /decision-lab/api/batches/preview`
//! answers the columns, the row count, the first rows and, once the state
//! column is mapped, every problem Start would refuse; the mapping names the
//! state column (and its JSON mode), a label column per question, a key
//! column, and a `score` question's `score_labels` when its column reads
//! both ways. Concurrency is a visible field, 1 by default, with no maximum.
//! Before a run on a priced model its price row shows. Start posts the run
//! and opens its page.
//!
//! The run takes the loaded saved set when the form's questions are that
//! set's (so "Use as threshold" can write into it), else the questions as an
//! unsaved set, which the modal says.

use leptos::prelude::TimeoutHandle;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::decision_lab::{
    BatchFormat, BatchMapping, BatchPreview, BatchPreviewed, BatchStart, BatchView, QuestionDraft,
    QuestionSet, ScoreLabels, StateMode,
};
use lmgw_api_types::systemone::Ordered;

use super::batch_input::InputPart;
use super::batch_list;
use super::batch_mapping::{guess, MappingPart};
use super::batch_preview::PreviewPart;
use super::page::Lab;
use super::price_line::PriceLine;
use super::sets::{form_set, set_of};
use crate::api;
use crate::fmt::grouped;
use crate::scope::Latest;
use crate::widgets::{Modal, ModalSize};

/// How long the inputs stay still before a preview goes out.
const QUIET_MS: u64 = 350;

/// The set a new run asks.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RunSet {
    /// The loaded set, unedited: the run names it, and its thresholds can be
    /// written from the run.
    Saved {
        id: i64,
        name: String,
        set: QuestionSet,
    },
    /// The form's questions as they are.
    Unsaved(QuestionSet),
}

impl RunSet {
    pub fn set(&self) -> &QuestionSet {
        match self {
            Self::Saved { set, .. } | Self::Unsaved(set) => set,
        }
    }
}

/// The new run's inputs. `Copy`, a part of `Lab`.
#[derive(Clone, Copy)]
pub(super) struct NewRun {
    pub open: RwSignal<bool>,
    pub set: RwSignal<Option<RunSet>>,
    pub model: RwSignal<String>,
    pub format: RwSignal<BatchFormat>,
    /// The format was picked by hand: a new text does not move it.
    pub format_picked: RwSignal<bool>,
    /// The file's text, pasted or read.
    pub text: RwSignal<String>,
    /// Bumped on every change of `text`, which is too big to compare.
    pub version: RwSignal<u64>,
    /// An uploaded file's name and size; `None` for pasted text.
    pub file: RwSignal<Option<(String, u64)>>,
    pub state_col: RwSignal<String>,
    pub state_mode: RwSignal<StateMode>,
    /// "" for none.
    pub key_col: RwSignal<String>,
    /// Per question id, its label column ("" for none).
    pub labels: RwSignal<Vec<(String, String)>>,
    /// Per score question id, "" (as the column reads), "text" or "index".
    pub score_labels: RwSignal<Vec<(String, String)>>,
    pub concurrency: RwSignal<String>,
    pub preview: RwSignal<Option<BatchPreviewed>>,
    pub preview_err: RwSignal<Option<String>>,
    /// The last preview is for the inputs as they are.
    pub fresh: RwSignal<bool>,
    pub starting: RwSignal<bool>,
    pub start_err: RwSignal<Option<String>>,
    reads: Latest,
    timer: StoredValue<Option<TimeoutHandle>>,
}

impl NewRun {
    pub fn new() -> Self {
        Self {
            open: RwSignal::new(false),
            set: RwSignal::new(None),
            model: RwSignal::new(String::new()),
            format: RwSignal::new(BatchFormat::Csv),
            format_picked: RwSignal::new(false),
            text: RwSignal::new(String::new()),
            version: RwSignal::new(0),
            file: RwSignal::new(None),
            state_col: RwSignal::new(String::new()),
            state_mode: RwSignal::new(StateMode::Text),
            key_col: RwSignal::new(String::new()),
            labels: RwSignal::new(Vec::new()),
            score_labels: RwSignal::new(Vec::new()),
            concurrency: RwSignal::new("1".into()),
            preview: RwSignal::new(None),
            preview_err: RwSignal::new(None),
            fresh: RwSignal::new(false),
            starting: RwSignal::new(false),
            start_err: RwSignal::new(None),
            reads: Latest::new(),
            timer: StoredValue::new(None),
        }
    }

    /// A new text (pasted, or a file read): its format follows it unless
    /// it was picked by hand.
    pub fn put_text(self, text: String, file: Option<(String, u64)>) {
        if !self.format_picked.get_untracked() {
            self.format
                .set(format_of(file.as_ref().map(|f| f.0.as_str()), &text));
        }
        self.text.set(text);
        self.file.set(file);
        self.version.update(|v| *v += 1);
    }

    /// The questions the run asks (tracked).
    pub fn questions(self) -> Vec<QuestionDraft> {
        self.set
            .with(|s| s.as_ref().map(|s| s.set().questions.clone()))
            .unwrap_or_default()
    }

    /// The mapping as the fields say (tracked); `None` until the state
    /// column is picked.
    pub fn mapping(self) -> Option<BatchMapping> {
        let state = self.state_col.get();
        if state.is_empty() {
            return None;
        }
        Some(mapping_of(
            state,
            self.state_mode.get(),
            &self.key_col.get(),
            &self.labels.get(),
            &self.score_labels.get(),
        ))
    }
}

/// The mapping the fields describe: a blank key or label column is none,
/// a blank `score_labels` reads as the column does.
pub(super) fn mapping_of(
    state: String,
    state_mode: StateMode,
    key: &str,
    labels: &[(String, String)],
    score_labels: &[(String, String)],
) -> BatchMapping {
    BatchMapping {
        state,
        state_mode,
        key: (!key.is_empty()).then(|| key.to_string()),
        labels: Ordered(
            labels
                .iter()
                .filter(|(_, c)| !c.is_empty())
                .cloned()
                .collect(),
        ),
        score_labels: Ordered(
            score_labels
                .iter()
                .filter_map(|(q, r)| {
                    let r = match r.as_str() {
                        "text" => ScoreLabels::Text,
                        "index" => ScoreLabels::Index,
                        _ => return None,
                    };
                    Some((q.clone(), r))
                })
                .collect(),
        ),
    }
}

/// The format a text is in: a `.jsonl`, `.ndjson` or `.json` file, or text
/// whose first non-blank character opens an object, is JSONL; else CSV.
pub(super) fn format_of(file: Option<&str>, text: &str) -> BatchFormat {
    if let Some(name) = file {
        let lower = name.to_ascii_lowercase();
        return if [".jsonl", ".ndjson", ".json"]
            .iter()
            .any(|e| lower.ends_with(e))
        {
            BatchFormat::Jsonl
        } else {
            BatchFormat::Csv
        };
    }
    if text.trim_start().starts_with('{') {
        BatchFormat::Jsonl
    } else {
        BatchFormat::Csv
    }
}

/// The concurrency field's value: a whole number, at least 1, no maximum.
pub(super) fn concurrency_of(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(0) => Err("concurrency: at least 1".into()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("concurrency: '{}' is no whole number", text.trim())),
    }
}

/// Why Start cannot go yet; `None` when it can. The state column is asked
/// for once any preview named the columns, whether or not the last one is
/// for the inputs as they are; a failed preview says so (Retry is beside
/// it).
pub(super) fn start_blocked(
    has_text: bool,
    preview: Option<&BatchPreviewed>,
    preview_failed: bool,
    fresh: bool,
    mapped: bool,
    concurrency: &str,
) -> Option<String> {
    if !has_text {
        return Some("paste the file's text or pick a file".into());
    }
    if preview_failed {
        return Some("the file did not read: Retry the preview".into());
    }
    let Some(p) = preview else {
        return Some("reading the file…".into());
    };
    if fresh && p.rows == 0 {
        return Some("the file holds no row".into());
    }
    if !mapped {
        return Some("pick the column that holds each item's state".into());
    }
    if !fresh {
        return Some("reading the file…".into());
    }
    if !p.problems.is_empty() {
        let n = p.problems.len();
        return Some(format!(
            "{} {}: fix the file or the mapping",
            grouped(n as u64),
            if n == 1 { "problem" } else { "problems" }
        ));
    }
    concurrency_of(concurrency).err()
}

/// Open the modal on the form as it is now: the loaded set when the
/// questions are its, else the questions unsaved; the head model.
pub(super) fn open_new_run(lab: Lab) {
    let nr = lab.new_run;
    let questions = untrack(|| lab.form.question_drafts());
    let loaded = lab.sets.loaded.get_untracked();
    let src = match &loaded {
        Some(p) if set_of(p).questions == questions => RunSet::Saved {
            id: p.id,
            name: p.name.clone(),
            set: set_of(p).clone(),
        },
        _ => RunSet::Unsaved(form_set(lab, loaded.as_ref().map(set_of))),
    };
    // Labels of questions the run no longer asks go.
    nr.labels
        .update(|v| v.retain(|(q, _)| questions.iter().any(|x| x.id == *q)));
    nr.score_labels
        .update(|v| v.retain(|(q, _)| questions.iter().any(|x| x.id == *q)));
    nr.set.set(Some(src));
    nr.model.set(lab.alias.get_untracked());
    nr.start_err.set(None);
    nr.open.set(true);
}

/// Preview the inputs again now: Retry after a failed preview.
pub(super) fn preview_again(lab: Lab) {
    lab.new_run.preview_err.set(None);
    schedule(lab);
}

/// Preview the inputs as they are, after they stay still for a moment.
fn schedule(lab: Lab) {
    let nr = lab.new_run;
    if let Some(h) = nr.timer.get_value() {
        h.clear();
    }
    let ticket = nr.reads.next();
    nr.fresh.set(false);
    if nr.text.with_untracked(|t| t.trim().is_empty()) {
        nr.preview.set(None);
        nr.preview_err.set(None);
        return;
    }
    let h = set_timeout_with_handle(
        move || {
            if !lab.scope.alive() {
                return;
            }
            let body = BatchPreview {
                format: nr.format.get_untracked(),
                text: nr.text.get_untracked(),
                sample: None,
                questions: untrack(|| nr.questions()),
                mapping: untrack(|| nr.mapping()),
            };
            lab.scope.spawn(async move {
                let got =
                    api::post::<BatchPreviewed, _>("/decision-lab/api/batches/preview", &body)
                        .await;
                if !ticket.is_some_and(|t| nr.reads.is(t)) {
                    return;
                }
                match got {
                    Ok(p) => {
                        // A field the file leaves empty or names no column
                        // of is guessed; a guess moves the mapping, which
                        // previews again, so this answer is not the last.
                        let moved = guess(nr, &p);
                        nr.preview.set(Some(p));
                        nr.preview_err.set(None);
                        nr.fresh.set(!moved);
                    }
                    Err(e) => {
                        nr.preview_err.set(Some(e.to_string()));
                        nr.fresh.set(false);
                    }
                }
            });
        },
        std::time::Duration::from_millis(QUIET_MS),
    )
    .ok();
    nr.timer.set_value(h);
}

/// Start the run and open its page (`go` navigates).
fn start(lab: Lab, go: Callback<String>) {
    let nr = lab.new_run;
    let Some(src) = nr.set.get_untracked() else {
        return;
    };
    let Some(mapping) = untrack(|| nr.mapping()) else {
        return;
    };
    let Ok(concurrency) = concurrency_of(&nr.concurrency.get_untracked()) else {
        return;
    };
    let (preset_id, set) = match src {
        RunSet::Saved { id, .. } => (Some(id), None),
        RunSet::Unsaved(s) => (None, Some(s)),
    };
    let body = BatchStart {
        preset_id,
        set,
        model: nr.model.get_untracked(),
        format: nr.format.get_untracked(),
        text: nr.text.get_untracked(),
        source_name: nr
            .file
            .get_untracked()
            .map(|f| f.0)
            .unwrap_or_else(|| "pasted text".into()),
        mapping,
        concurrency: Some(concurrency),
    };
    nr.starting.set(true);
    nr.start_err.set(None);
    lab.scope.spawn(async move {
        let got = api::post::<BatchView, _>("/decision-lab/api/batches", &body).await;
        nr.starting.set(false);
        match got {
            Ok(v) => {
                nr.open.set(false);
                lab.toasts.ok(format!("set run #{} started", v.id));
                batch_list::reload(lab);
                lab.history.refresh_usage();
                go.run(format!("/decision-lab/batches/{}", v.id));
            }
            Err(e) => nr.start_err.set(Some(e.to_string())),
        }
    });
}

#[component]
pub(super) fn NewRunModal(lab: Lab) -> impl IntoView {
    let nr = lab.new_run;
    let navigate = StoredValue::new(use_navigate());
    let go = Callback::new(move |href: String| {
        navigate.with_value(|n| n(&href, Default::default()));
    });
    // What a preview reads, so a change of anything else does not ask again.
    let inputs = Memo::new(move |_| {
        (
            nr.version.get(),
            nr.format.get(),
            nr.mapping(),
            nr.questions(),
            nr.open.get(),
        )
    });
    Effect::new(move |_| {
        let (_, _, _, _, open) = inputs.get();
        if open {
            schedule(lab);
        }
    });
    on_cleanup(move || {
        if let Some(Some(h)) = nr.timer.try_get_value() {
            h.clear();
        }
    });
    let blocked = Memo::new(move |_| {
        let has_text = nr.text.with(|t| !t.trim().is_empty());
        let mapped = nr.state_col.with(|s| !s.is_empty());
        let preview_failed = nr.preview_err.with(Option::is_some);
        nr.preview.with(|p| {
            start_blocked(
                has_text,
                p.as_ref(),
                preview_failed,
                nr.fresh.get(),
                mapped,
                &nr.concurrency.get(),
            )
        })
    });
    let what = move || {
        let model = nr.model.get();
        nr.set.with(|s| match s {
            Some(RunSet::Saved { name, set, .. }) => format!(
                "Runs the saved set '{name}' ({} {}) on {model}",
                set.questions.len(),
                if set.questions.len() == 1 {
                    "question"
                } else {
                    "questions"
                }
            ),
            Some(RunSet::Unsaved(set)) => format!(
                "Runs the form's {} {} on {model}, as an unsaved set",
                set.questions.len(),
                if set.questions.len() == 1 {
                    "question"
                } else {
                    "questions"
                }
            ),
            None => String::new(),
        })
    };
    let unsaved = move || nr.set.with(|s| matches!(s, Some(RunSet::Unsaved(_))));
    let footer = move || {
        view! {
            <span class="dl-start-note" class:needs=move || blocked.get().is_some()>
                {move || blocked.get().unwrap_or_else(|| {
                    let rows = nr.preview.with(|p| p.as_ref().map(|p| p.rows)).unwrap_or(0);
                    format!("Start: {} {} as one job", grouped(rows), if rows == 1 { "item" } else { "items" })
                })}
            </span>
            <button
                class="btn primary"
                disabled=move || blocked.get().is_some() || nr.starting.get()
                on:click=move |_| start(lab, go)
            >
                {move || if nr.starting.get() { "Starting…" } else { "Start set run" }}
            </button>
        }
    };
    view! {
        <Modal open=nr.open title="New set run" size=ModalSize::Wide guard=true footer=footer>
            <div class="dl-newrun">
                <p class="dl-newrun-what">{what}</p>
                <Show when=unsaved>
                    <p class="mini-note dl-warn-line">
                        "An unsaved set: its run's metrics show, but Use as threshold has no set to write into. Save as set (or Update set) first to measure the set's thresholds."
                    </p>
                </Show>
                <PriceLine lab=lab/>
                <InputPart lab=lab/>
                <MappingPart lab=lab/>
                <PreviewPart lab=lab/>
                <div class="dl-newrun-conc">
                    <label class="field">
                        <span class="lbl">"Concurrency"</span>
                        <input
                            class="input w-num"
                            type="number"
                            min="1"
                            step="1"
                            prop:value=move || nr.concurrency.get()
                            on:input=move |ev| nr.concurrency.set(event_target_value(&ev))
                        />
                    </label>
                    <span class="mini-note dim">
                        "How many items are asked at once. Admission and the model's slots are the only other bound; a llama-server with one slot answers one at a time whatever this says."
                    </span>
                </div>
                {move || nr.start_err.get().map(|e| view! {
                    <div class="notice err dl-start-err">{format!("⚠ Not started: {e}")}</div>
                })}
            </div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_or_its_first_character_says_its_format() {
        assert_eq!(format_of(Some("t.JSONL"), ""), BatchFormat::Jsonl);
        assert_eq!(format_of(Some("t.csv"), "{"), BatchFormat::Csv);
        assert_eq!(format_of(None, "  {\"a\":1}"), BatchFormat::Jsonl);
        assert_eq!(format_of(None, "state,team\n"), BatchFormat::Csv);
    }

    #[test]
    fn the_mapping_leaves_out_blank_columns() {
        let m = mapping_of(
            "text".into(),
            StateMode::Text,
            "",
            &[
                ("team".into(), "team".into()),
                ("urgency".into(), String::new()),
            ],
            &[
                ("urgency".into(), "index".into()),
                ("x".into(), String::new()),
            ],
        );
        assert_eq!(m.key, None);
        assert_eq!(m.labels.keys().collect::<Vec<_>>(), ["team"]);
        assert_eq!(m.score_labels.get("urgency"), Some(&ScoreLabels::Index));
        assert_eq!(m.score_labels.len(), 1);
    }

    #[test]
    fn start_says_what_is_missing_first() {
        let p = BatchPreviewed {
            columns: vec![],
            rows: 30,
            sample: vec![],
            problems: vec![],
        };
        let blocked = |preview: Option<&BatchPreviewed>, failed, fresh, mapped, conc| {
            start_blocked(true, preview, failed, fresh, mapped, conc)
        };
        assert!(start_blocked(false, None, false, false, false, "1")
            .unwrap()
            .contains("paste"));
        assert_eq!(
            blocked(None, false, false, false, "1").unwrap(),
            "reading the file…"
        );
        assert_eq!(
            blocked(Some(&p), false, false, true, "1").unwrap(),
            "reading the file…"
        );
        // Unmapped: asked for even when the last preview is not this one's
        // (a file no column of which is guessed never brings a fresh one).
        for fresh in [true, false] {
            assert!(blocked(Some(&p), false, fresh, false, "1")
                .unwrap()
                .contains("pick the column"));
        }
        assert!(blocked(Some(&p), true, true, true, "1")
            .unwrap()
            .contains("did not read"));
        assert!(blocked(None, true, false, false, "1")
            .unwrap()
            .contains("did not read"));
        assert_eq!(blocked(Some(&p), false, true, true, "1"), None);
        assert_eq!(
            blocked(Some(&p), false, true, true, "64"),
            None,
            "no maximum"
        );
        assert!(blocked(Some(&p), false, true, true, "0")
            .unwrap()
            .contains("at least 1"));
        let mut bad = p.clone();
        bad.problems.push(Default::default());
        bad.problems.push(Default::default());
        assert_eq!(
            blocked(Some(&bad), false, true, true, "1").unwrap(),
            "2 problems: fix the file or the mapping"
        );
        let empty = BatchPreviewed {
            rows: 0,
            ..p.clone()
        };
        assert_eq!(
            blocked(Some(&empty), false, true, false, "1").unwrap(),
            "the file holds no row"
        );
    }
}
