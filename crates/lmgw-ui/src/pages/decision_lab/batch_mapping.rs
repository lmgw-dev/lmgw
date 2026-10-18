//! A new set run's mapping (labs-store design §3.3, §3.5 R1, R5, R7): the
//! column that holds each item's state, sent as text or as JSON; an
//! optional key column (a ticket number), carried into both exports; per
//! question an optional label column, and for a `score` question whose
//! column the preview says reads both as level texts and as indices (or
//! mixes the two), how it reads.
//!
//! A field the file leaves open, or one that names a column the file does
//! not have (another file was read), is guessed from the column names: a
//! state column called `state`, `text`, `message`, `body` or `content`; a
//! key called `key`, `id` or ending in `_id`; a question's label column
//! called as its id. A guess is a pick like any other, there to be changed;
//! a field set to "none" stays none, and a guess that finds no column leaves
//! the field open, to be guessed from the next file. A guess counts as a
//! move only when the mapping it sends changes, so the preview that follows
//! a move is always asked for.
//!
//! A score's reading appears where the preview names the question with a
//! problem of kind `score_labels_needed`, and stays while it is set.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{
    BatchColumn, BatchPreviewed, BatchProblemKind, QuestionKind, StateMode,
};

use super::batch_new::NewRun;
use super::page::Lab;
use crate::widgets::Select;

const STATE_NAMES: [&str; 5] = ["state", "text", "message", "body", "content"];

/// The state column a file's columns suggest.
pub(super) fn guess_state(cols: &[BatchColumn]) -> Option<&BatchColumn> {
    STATE_NAMES
        .iter()
        .find_map(|n| cols.iter().find(|c| c.name.eq_ignore_ascii_case(n)))
}

/// The key column a file's columns suggest.
pub(super) fn guess_key(cols: &[BatchColumn]) -> Option<String> {
    cols.iter()
        .find(|c| {
            let l = c.name.to_ascii_lowercase();
            l == "key" || l == "id" || l.ends_with("_id")
        })
        .map(|c| c.name.clone())
}

/// A question's label column a file's columns suggest: one called as its id.
pub(super) fn guess_label(cols: &[BatchColumn], qid: &str) -> Option<String> {
    cols.iter()
        .find(|c| c.name.eq_ignore_ascii_case(qid))
        .map(|c| c.name.clone())
}

/// Fill the fields the preview's columns leave open or stale; `true` when
/// the mapping they send moved (a new preview follows).
pub(super) fn guess(nr: NewRun, p: &BatchPreviewed) -> bool {
    let before = untrack(|| nr.mapping());
    fill(nr, p);
    untrack(|| nr.mapping()) != before
}

fn fill(nr: NewRun, p: &BatchPreviewed) {
    let cols = &p.columns;
    let has = |name: &str| cols.iter().any(|c| c.name == name);
    let state = nr.state_col.get_untracked();
    let fresh_file = state.is_empty() || !has(&state);
    if fresh_file {
        let (name, json) = guess_state(cols)
            .map(|c| (c.name.clone(), c.json))
            .unwrap_or_default();
        if name != state {
            nr.state_col.set(name);
            nr.state_mode.set(if json {
                StateMode::Json
            } else {
                StateMode::Text
            });
        }
        let key = nr.key_col.get_untracked();
        let k = guess_key(cols).unwrap_or_default();
        if k != key {
            nr.key_col.set(k);
        }
    } else {
        let key = nr.key_col.get_untracked();
        if !key.is_empty() && !has(&key) {
            nr.key_col.set(String::new());
        }
    }
    // A question's label column: open (no entry) or stale is guessed; a
    // guess that finds nothing leaves it open, so the next file is guessed
    // again, where an entry of "" is a pick of none and stays.
    let questions = untrack(|| nr.questions());
    let mut labels = nr.labels.get_untracked();
    let mut changed = false;
    for q in &questions {
        let at = labels.iter().position(|(id, _)| *id == q.id);
        let stale = at.is_some_and(|i| !labels[i].1.is_empty() && !has(&labels[i].1));
        if at.is_some() && !stale {
            continue;
        }
        match (at, guess_label(cols, &q.id)) {
            (Some(i), Some(g)) => labels[i].1 = g,
            (Some(i), None) => {
                labels.remove(i);
            }
            (None, Some(g)) => labels.push((q.id.clone(), g)),
            (None, None) => continue,
        }
        changed = true;
    }
    if changed {
        nr.labels.set(labels);
    }
}

/// Does the preview ask how question `qid`'s score column reads?
pub(super) fn asks_score_reading(p: &BatchPreviewed, qid: &str) -> bool {
    p.problems.iter().any(|x| {
        x.kind == BatchProblemKind::ScoreLabelsNeeded && x.question.as_deref() == Some(qid)
    })
}

/// A select's options: "none" first when `none`, then each column (a JSON
/// column marked).
fn column_options(cols: &[BatchColumn], none: Option<&str>) -> Vec<(String, String)> {
    none.map(|n| (String::new(), n.to_string()))
        .into_iter()
        .chain(cols.iter().map(|c| {
            let label = if c.json {
                format!("{} · JSON", c.name)
            } else {
                c.name.clone()
            };
            (c.name.clone(), label)
        }))
        .collect()
}

#[component]
pub(super) fn MappingPart(lab: Lab) -> impl IntoView {
    let nr = lab.new_run;
    let cols = Memo::new(move |_| {
        nr.preview
            .with(|p| p.as_ref().map(|p| p.columns.clone()))
            .unwrap_or_default()
    });
    let state_opts = Signal::derive(move || cols.with(|c| column_options(c, None)));
    let key_opts = Signal::derive(move || cols.with(|c| column_options(c, Some("none"))));
    let state_json = Memo::new(move |_| {
        let s = nr.state_col.get();
        cols.with(|c| c.iter().any(|c| c.name == s && c.json))
    });
    view! {
        <Show when=move || cols.with(|c| !c.is_empty())>
            <section class="dl-newrun-part">
                <div class="dl-card-head">
                    <h3 class="lab-h3">"Mapping"</h3>
                </div>
                <div class="dl-map">
                    <span class="lbl">"state"</span>
                    <span class="dl-map-sel">
                        <Select value=nr.state_col options=state_opts placeholder="pick a column"/>
                    </span>
                    <span class="seg dl-state-mode" title="Text sends each value as a JSON string; JSON sends it as written, and every value must parse">
                        <button
                            type="button"
                            class="seg-btn"
                            class:active=move || nr.state_mode.get() == StateMode::Text
                            on:click=move |_| nr.state_mode.set(StateMode::Text)
                        >
                            "Text"
                        </button>
                        <button
                            type="button"
                            class="seg-btn"
                            class:active=move || nr.state_mode.get() == StateMode::Json
                            on:click=move |_| nr.state_mode.set(StateMode::Json)
                        >
                            "JSON"
                        </button>
                    </span>
                    <span class="mini-note dim">
                        {move || if state_json.get() && nr.state_mode.get() == StateMode::Text {
                            "every value of this column is JSON: JSON mode sends it as written"
                        } else {
                            "each item's state"
                        }}
                    </span>

                    <span class="lbl">"key"</span>
                    <span class="dl-map-sel">
                        <Select value=nr.key_col options=key_opts placeholder="none"/>
                    </span>
                    <span></span>
                    <span class="mini-note dim">"each item's own key (a ticket number), carried into both exports"</span>

                    <For each=move || nr.questions() key=|q| q.id.clone() let:q>
                        <LabelRow nr=nr qid=q.id.clone() score=matches!(q.kind, QuestionKind::Score { .. }) key_opts=key_opts/>
                    </For>
                </div>
            </section>
        </Show>
    }
}

/// One question's label column, and a score's reading when it is asked.
#[component]
fn LabelRow(
    nr: NewRun,
    qid: String,
    score: bool,
    key_opts: Signal<Vec<(String, String)>>,
) -> impl IntoView {
    let id = StoredValue::new(qid.clone());
    let current = move || {
        nr.labels.with(|v| {
            id.with_value(|q| v.iter().find(|(x, _)| x == q).map(|(_, c)| c.clone()))
                .unwrap_or_default()
        })
    };
    let pick = RwSignal::new(untrack(current));
    // The field follows a guess; a pick writes the mapping.
    Effect::new(move |_| {
        let c = current();
        if pick.get_untracked() != c {
            pick.set(c);
        }
    });
    Effect::new(move |_| {
        let v = pick.get();
        if untrack(current) != v {
            let q = id.get_value();
            nr.labels
                .update(|l| match l.iter_mut().find(|(x, _)| *x == q) {
                    Some(slot) => slot.1 = v.clone(),
                    None => l.push((q, v.clone())),
                });
        }
    });
    let reading_now = move || {
        nr.score_labels.with(|v| {
            id.with_value(|q| v.iter().find(|(x, _)| x == q).map(|(_, r)| r.clone()))
                .unwrap_or_default()
        })
    };
    let reading = RwSignal::new(untrack(reading_now));
    Effect::new(move |_| {
        let v = reading.get();
        if untrack(reading_now) != v {
            let q = id.get_value();
            nr.score_labels
                .update(|l| match l.iter_mut().find(|(x, _)| *x == q) {
                    Some(slot) => slot.1 = v.clone(),
                    None => l.push((q, v.clone())),
                });
        }
    });
    let asked = move || {
        score
            && (!reading.get().is_empty()
                || nr.preview.with(|p| {
                    p.as_ref()
                        .is_some_and(|p| id.with_value(|q| asks_score_reading(p, q)))
                }))
    };
    let reading_opts = Signal::derive(|| {
        vec![
            (String::new(), "as the column reads".to_string()),
            ("text".to_string(), "level texts".to_string()),
            ("index".to_string(), "level indices (from 0)".to_string()),
        ]
    });
    view! {
        <span class="lbl mono">{qid.clone()}</span>
        <span class="dl-map-sel">
            <Select value=pick options=key_opts placeholder="none"/>
        </span>
        <span class="dl-map-reading">
            <Show when=asked>
                <span title="How this score's label column reads: the column is one or the other as a whole">
                    <Select value=reading options=reading_opts/>
                </span>
            </Show>
        </span>
        <span class="mini-note dim">
            {if score {
                "its expected level: a level's exact text or its index, the column one way"
            } else {
                "its expected answer"
            }}
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[(&str, bool)]) -> Vec<BatchColumn> {
        names
            .iter()
            .map(|(n, j)| BatchColumn {
                name: n.to_string(),
                json: *j,
            })
            .collect()
    }

    #[test]
    fn the_column_names_suggest_the_mapping() {
        let c = cols(&[("ticket_id", false), ("Text", false), ("team", false)]);
        assert_eq!(guess_state(&c).map(|c| c.name.as_str()), Some("Text"));
        assert_eq!(guess_key(&c).as_deref(), Some("ticket_id"));
        assert_eq!(guess_label(&c, "team").as_deref(), Some("team"));
        assert_eq!(guess_label(&c, "urgency"), None);
        let none = cols(&[("a", false)]);
        assert!(guess_state(&none).is_none());
        assert!(guess_key(&none).is_none());
    }

    #[test]
    fn a_guess_that_sends_the_same_mapping_is_no_move() {
        use super::super::batch_new::RunSet;
        use lmgw_api_types::decision_lab::{examples, QuestionSet};
        let owner = Owner::new();
        owner.with(|| {
            let nr = NewRun::new();
            let questions = examples().into_iter().next().unwrap().form.questions;
            nr.set.set(Some(RunSet::Unsaved(QuestionSet {
                questions,
                ..Default::default()
            })));
            // No column reads as a state, only one is called as a question:
            // the state stays open, so the mapping sent is none as before
            // and no preview follows; the other questions stay open too.
            let p = BatchPreviewed {
                columns: cols(&[("msg", false), ("team", false)]),
                rows: 2,
                ..Default::default()
            };
            assert!(!guess(nr, &p));
            assert_eq!(
                nr.labels.get_untracked(),
                [("team".to_string(), "team".to_string())]
            );
            assert_eq!(untrack(|| nr.mapping()), None);
            // The next file: a state column, and a question left open is
            // guessed from it.
            let p = BatchPreviewed {
                columns: cols(&[("text", false), ("team", false), ("urgency", false)]),
                rows: 2,
                ..Default::default()
            };
            assert!(guess(nr, &p));
            assert_eq!(nr.state_col.get_untracked(), "text");
            assert_eq!(nr.labels.get_untracked().len(), 2);
            assert!(!guess(nr, &p), "the same columns again");
            // A pick of none stays none.
            nr.labels
                .update(|l| l.push(("needs_human".into(), String::new())));
            let p = BatchPreviewed {
                columns: cols(&[("text", false), ("needs_human", false)]),
                rows: 2,
                ..Default::default()
            };
            guess(nr, &p);
            assert!(nr
                .labels
                .get_untracked()
                .contains(&("needs_human".to_string(), String::new())));
        });
    }

    #[test]
    fn a_score_reading_is_asked_when_the_preview_names_the_question() {
        let problem = |kind, q: &str, message: &str| lmgw_api_types::decision_lab::BatchProblem {
            kind,
            question: Some(q.into()),
            message: message.into(),
            ..Default::default()
        };
        let p = BatchPreviewed {
            problems: vec![
                problem(BatchProblemKind::ScoreLabelsNeeded, "urgency", "row 2: …"),
                // The message names score_labels, but the problem is a label's.
                problem(
                    BatchProblemKind::Label,
                    "team",
                    "'x' is no option; set score_labels for 'team'",
                ),
            ],
            ..Default::default()
        };
        assert!(asks_score_reading(&p, "urgency"));
        assert!(!asks_score_reading(&p, "team"));
    }
}
