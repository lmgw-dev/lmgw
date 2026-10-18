//! Set runs (labs-store design §3.3): a question set asked of every row of
//! an uploaded CSV or JSONL file, optionally with the expected answers, as a
//! durable job. Each row is an **item**, a lab group of the set run with one
//! cell; its run comes when the job answers it.

use serde::{Deserialize, Serialize};

use super::form::StateMode;
use super::sets::QuestionSet;
use crate::lab_store::{LabGroupView, LabKind};
use crate::systemone::Ordered;

/// How many rows a preview shows when the request names no number.
pub const PREVIEW_ROWS: u32 = 20;

/// The jobs feed's `kind` of a set run's job.
pub const JOB_KIND: &str = "lab_batch";

/// The jobs feed's `key` of set run `id`'s job: one live job per run.
pub fn job_key(id: i64) -> String {
    format!("lab-batch:{id}")
}

/// A set run's job's `detail` in the jobs feed (and its stored result):
/// where the run is while it runs, between the batch reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchJobDetail {
    pub batch_id: i64,
    /// Items in all.
    pub total: u64,
    /// Items with a run.
    pub done: u64,
    /// Of those, the runs that are no 2xx.
    pub failed: u64,
    /// Of those, the 2xx runs that do not read as an answer to every
    /// question of the set.
    pub unreadable: u64,
    /// Items this job answered whose run the store could not keep.
    pub not_kept: u64,
    /// The job asks the items whose run failed again too (Resume's
    /// `retry_failed`): a failed run is not final while it runs.
    pub retry_failed: bool,
    /// A cancel is asked of the job (from any caller: this page, another
    /// tab, the jobs route, MCP): the next Cancel aborts the calls in
    /// flight.
    #[serde(default)]
    pub cancel_requested: bool,
}

/// The words a `noul` label may be, any case: a yes, then a no.
pub const NOUL_YES: [&str; 3] = ["yes", "true", "1"];
pub const NOUL_NO: [&str; 3] = ["no", "false", "0"];

/// The uploaded file's format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BatchFormat {
    /// RFC 4180 with a header row.
    #[default]
    Csv,
    /// One JSON object per line; its columns are its top-level keys.
    Jsonl,
}

/// How a `score` question's label column reads: every value one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ScoreLabels {
    /// Each value is a level's exact text.
    Text,
    /// Each value is a level's index, from 0.
    Index,
}

/// Which columns a set run reads. A column is named by its header; a
/// column whose header is empty cannot be mapped (give it a name in the
/// file).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchMapping {
    /// The column holding each item's state. Required.
    pub state: String,
    /// How that column's values are sent: as text, or as JSON (each value
    /// must parse).
    pub state_mode: StateMode,
    /// The column holding each item's own key (a ticket number, a row id):
    /// kept per item and carried into both exports, so an answer can be
    /// matched back to its source row. Absent, none; an empty value is no
    /// key.
    pub key: Option<String>,
    /// Per question id, the column holding its expected answer: an option's
    /// name (`choice`); yes, no, true, false, 1 or 0 (`noul`); a level's
    /// exact text or its index, as the whole column reads
    /// (`score_labels`). An empty value is no label.
    pub labels: Ordered<String>,
    /// Per `score` question id, how its label column reads. A question not
    /// named here reads as its column does: as level texts when every
    /// non-empty value is a level's exact text, as indices when every one is
    /// an index in range. When both fit and disagree (a scale written "1" to
    /// "5" and values "1" to "4"), or the column mixes the two, the input is
    /// refused until this says which.
    pub score_labels: Ordered<ScoreLabels>,
}

/// `POST /decision-lab/api/batches/preview`: read a file, show its top, and
/// with `mapping` check every row as a start would.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchPreview {
    pub format: BatchFormat,
    /// The file's text, as read by the page.
    pub text: String,
    /// How many rows to answer from the top; absent, [`PREVIEW_ROWS`].
    pub sample: Option<u32>,
    /// The set's questions, which `mapping`'s labels are checked against.
    pub questions: Vec<super::form::QuestionDraft>,
    /// When given, every row is checked against it and `problems` lists
    /// each refusal.
    pub mapping: Option<BatchMapping>,
}

/// One column of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchColumn {
    pub name: String,
    /// Every non-empty value is a JSON object or array (in JSONL, written as
    /// one or as a string holding one): the state's JSON mode fits it.
    pub json: bool,
}

/// What kind of problem a [`BatchProblem`] is, for a client that acts on one
/// without reading its message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BatchProblemKind {
    /// The file itself: a line that does not read, a header, no rows.
    #[default]
    File,
    /// The mapping: a column it names that is empty, missing or named twice,
    /// or a `labels` or `score_labels` entry naming no question it can take.
    Mapping,
    /// A row's state: empty, or no JSON in JSON mode.
    State,
    /// A row's label that is no answer of its question.
    Label,
    /// A `score` question's label column reads both as level texts and as
    /// indices with different meanings, or mixes the two: the mapping's
    /// `score_labels` for `question` has to say which.
    ScoreLabelsNeeded,
}

/// What stops a file from being run: a line that does not read, a column
/// that is not there, a state or a label that is no answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchProblem {
    pub kind: BatchProblemKind,
    /// The question it is about (a label, a `labels` or `score_labels`
    /// entry); `null` for the file, the state and the state or key column.
    pub question: Option<String>,
    /// The item's number, from 1; `null` for the file or the mapping.
    pub row: Option<u64>,
    /// The line the row starts on, from 1 (the header is line 1 of a CSV);
    /// `null` for the mapping.
    pub line: Option<u64>,
    /// The column at fault.
    pub column: Option<String>,
    /// The value at fault, as read.
    pub value: Option<String>,
    /// The whole problem in a sentence, row and value included.
    pub message: String,
}

/// What a preview read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchPreviewed {
    /// In the file's order (a CSV's header; the keys of a JSONL file in the
    /// order first met).
    pub columns: Vec<BatchColumn>,
    /// Every row the file holds.
    pub rows: u64,
    /// The first rows, each a value per column in `columns`' order; a JSONL
    /// row without a key, or with `null`, has an empty value there.
    pub sample: Vec<Vec<String>>,
    /// Each problem: the file's own, and with a mapping every row's.
    pub problems: Vec<BatchProblem>,
}

/// `POST /decision-lab/api/batches`: run a set over a file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchStart {
    /// A saved set to run (its body is the run's snapshot, and "Use as
    /// threshold" writes into it).
    pub preset_id: Option<i64>,
    /// An unsaved set to run instead: one of the two.
    pub set: Option<QuestionSet>,
    /// The model to ask; blank asks the set's default model.
    pub model: String,
    pub format: BatchFormat,
    /// The file's text.
    pub text: String,
    /// The file's name, kept as the run's source.
    pub source_name: String,
    pub mapping: BatchMapping,
    /// How many items are asked at once; absent, 1. Admission and the
    /// model's slots are the only other bound.
    pub concurrency: Option<u32>,
}

/// `POST /decision-lab/api/batches/{id}/resume`: a new job for the items
/// without a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchResume {
    /// A new concurrency for the run; absent, the one it had.
    pub concurrency: Option<u32>,
    /// Ask the items whose run failed (no 2xx) again too: each failed run is
    /// replaced by the new one. False when absent.
    pub retry_failed: bool,
}

/// Where a set run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BatchStatus {
    /// Its job is answering items now.
    Running,
    /// Every item has a run.
    Done,
    /// Cancelled with items left; Resume runs the rest.
    Canceled,
    /// Its job ended with items left: a restart of the gateway, a failure
    /// (`job_error` says which: lmgw refusing before the model was asked,
    /// such as the GPU hold, ends the job too), or a job row since pruned.
    /// Resume runs the rest.
    Interrupted,
    /// No job ran it yet.
    NotStarted,
}

/// A set run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchView {
    pub id: i64,
    pub lab: LabKind,
    /// The saved set it ran; `null` for an unsaved set, or one deleted
    /// since.
    pub preset_id: Option<i64>,
    /// The saved set's name, when it is still there.
    pub preset_name: Option<String>,
    /// The set as run: later edits of the saved set do not change it.
    pub set: QuestionSet,
    pub model: String,
    /// The uploaded file's name.
    pub source_name: String,
    pub mapping: BatchMapping,
    pub concurrency: u32,
    pub status: BatchStatus,
    /// Items in all.
    pub total: u64,
    /// Items with a run, whatever it answered: the progress.
    pub done: u64,
    /// Of those, the runs the route did not answer with a 2xx.
    pub failed: u64,
    /// Of those, the 2xx runs that do not read as an answer to every
    /// question of the set (the metrics count them per question as
    /// `unreadable`).
    pub unreadable: u64,
    /// Items with at least one expected answer.
    pub labelled: u64,
    /// The last job that ran it (no longer listed once pruned): the live
    /// one while it runs.
    pub job_id: Option<i64>,
    /// That job's error when it failed (`interrupted by shutdown` after a
    /// restart).
    pub job_error: Option<String>,
    /// A Cancel already went to the job that runs it now: the next Cancel
    /// aborts the calls in flight (their answers are not kept). False when
    /// no job runs it.
    pub cancel_requested: bool,
    pub created_at: String,
}

/// `GET /decision-lab/api/batches`: every set run, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchList {
    pub batches: Vec<BatchView>,
}

/// The query of `GET /decision-lab/api/batches/{id}/items`: one page of the
/// items in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchItemsQuery {
    /// Only items after this index (the next page).
    pub after: Option<u64>,
    /// How many items the page holds. Absent, every item left: the server
    /// sets no maximum.
    pub limit: Option<u32>,
}

/// One page of a set run's items, in item order: each a group with its
/// `item` and its run once answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchItemPage {
    pub items: Vec<LabGroupView>,
    /// Items in all.
    pub total: u64,
    /// The next page's cursor: this page's last `item_ix`, to send as
    /// `after`; `null` when no item follows.
    pub next_after: Option<u64>,
}

/// `POST /decision-lab/api/batches/{id}/threshold`: "Use as threshold".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThresholdWrite {
    /// The question's id.
    pub question: String,
    /// The cut, from 0 to 1.
    pub value: f64,
}

/// A label as typed, read for its question: the canonical spelling the
/// store keeps (the option's name, the level's index, `yes` or `no`), `None`
/// for an empty value (no label), `Err` naming why it is none. `score` says
/// how a `score` question's value reads, the reading of its whole column
/// ([`score_column`], or the mapping's `score_labels`); the other types
/// ignore it.
pub fn read_label(
    q: &super::form::QuestionDraft,
    raw: &str,
    score: ScoreLabels,
) -> Result<Option<String>, String> {
    use super::form::QuestionKind;
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    match &q.kind {
        QuestionKind::Choice { options } => {
            if options.iter().any(|o| o.name == v) {
                return Ok(Some(v.to_string()));
            }
            let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
            Err(format!(
                "'{v}' is not an option of '{}' ({})",
                q.id,
                names.join(", ")
            ))
        }
        QuestionKind::Noul { .. } => {
            let lower = v.to_ascii_lowercase();
            if NOUL_YES.contains(&lower.as_str()) {
                Ok(Some("yes".into()))
            } else if NOUL_NO.contains(&lower.as_str()) {
                Ok(Some("no".into()))
            } else {
                Err(format!(
                    "'{v}' is no yes or no for '{}' (yes, no, true, false, 1 or 0)",
                    q.id
                ))
            }
        }
        QuestionKind::Score { levels } => match score {
            ScoreLabels::Text => match level_of_text(levels, v) {
                Some(i) => Ok(Some(i.to_string())),
                None => Err(format!(
                    "'{v}' is not the text of a level of '{}' ({}); the column reads as level \
                     texts",
                    q.id,
                    levels.join(", ")
                )),
            },
            ScoreLabels::Index => match level_of_index(levels, v) {
                Some(i) => Ok(Some(i.to_string())),
                None => Err(format!(
                    "'{v}' is no level index of '{}' (0 to {}); the column reads as level \
                     indices",
                    q.id,
                    levels.len().saturating_sub(1)
                )),
            },
        },
    }
}

/// The level whose exact text `v` is.
fn level_of_text(levels: &[String], v: &str) -> Option<usize> {
    levels.iter().position(|l| l == v)
}

/// The level whose index `v` is, in range.
fn level_of_index(levels: &[String], v: &str) -> Option<usize> {
    v.parse::<usize>().ok().filter(|i| *i < levels.len())
}

/// How a `score` question's label column reads, decided over the column as
/// a whole, never per value: never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreColumn {
    /// Every value that is a level at all reads this way (a column of no
    /// values reads as texts). A value that is neither is refused by
    /// [`read_label`].
    Reads(ScoreLabels),
    /// Every value reads both as a level's text and as an index, and the two
    /// readings disagree: the value at place `at` (among those given) is the
    /// text of level `text_level` but the index of another. The mapping's
    /// `score_labels` has to say which.
    Ambiguous {
        at: usize,
        value: String,
        text_level: usize,
    },
    /// Some values are only a level's text, others only an index: their
    /// places among those given.
    Mixed {
        texts: Vec<usize>,
        indices: Vec<usize>,
    },
}

/// Read a `score` question's label column, its values in order (an empty
/// value is no label and has no say): texts when every value is a level's
/// exact text, indices when every value is an index in range; both fitting
/// and disagreeing is [`ScoreColumn::Ambiguous`], a column of the two
/// [`ScoreColumn::Mixed`].
pub fn score_column<'a>(
    levels: &[String],
    values: impl IntoIterator<Item = &'a str>,
) -> ScoreColumn {
    let mut texts = Vec::new();
    let mut indices = Vec::new();
    let mut both_disagree: Option<(usize, String, usize)> = None;
    for (at, raw) in values.into_iter().enumerate() {
        let v = raw.trim();
        if v.is_empty() {
            continue;
        }
        match (level_of_text(levels, v), level_of_index(levels, v)) {
            (Some(_), None) => texts.push(at),
            (None, Some(_)) => indices.push(at),
            (Some(t), Some(i)) if t != i => {
                both_disagree.get_or_insert((at, v.to_string(), t));
            }
            _ => {}
        }
    }
    match (texts.is_empty(), indices.is_empty()) {
        (false, false) => ScoreColumn::Mixed { texts, indices },
        (true, false) => ScoreColumn::Reads(ScoreLabels::Index),
        (false, true) => ScoreColumn::Reads(ScoreLabels::Text),
        (true, true) => match both_disagree {
            Some((at, value, text_level)) => ScoreColumn::Ambiguous {
                at,
                value,
                text_level,
            },
            None => ScoreColumn::Reads(ScoreLabels::Text),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision_lab::{OptionDraft, QuestionDraft, QuestionKind};

    fn q(kind: QuestionKind) -> QuestionDraft {
        QuestionDraft {
            id: "q".into(),
            instructions: String::new(),
            kind,
        }
    }

    #[test]
    fn a_label_is_read_as_its_questions_type_says() {
        let choice = q(QuestionKind::Choice {
            options: ["billing", "technical"]
                .map(|n| OptionDraft {
                    name: n.into(),
                    description: String::new(),
                })
                .to_vec(),
        });
        let read = |q: &QuestionDraft, raw: &str| read_label(q, raw, ScoreLabels::Text);
        assert_eq!(read(&choice, " billing "), Ok(Some("billing".into())));
        assert_eq!(read(&choice, ""), Ok(None));
        let e = read(&choice, "Billing").unwrap_err();
        assert!(
            e.contains("'Billing' is not an option of 'q' (billing, technical)"),
            "{e}"
        );

        let noul = q(QuestionKind::Noul {
            yes: String::new(),
            no: String::new(),
        });
        for (raw, want) in [
            ("yes", "yes"),
            ("TRUE", "yes"),
            ("1", "yes"),
            ("No", "no"),
            ("false", "no"),
            ("0", "no"),
        ] {
            assert_eq!(read(&noul, raw), Ok(Some(want.into())), "{raw}");
        }
        assert!(read(&noul, "maybe").is_err());

        // A score reads as its column reads: on a 1-to-3 scale written as
        // numbers, "3" is the text of level 2 and no index at all.
        let score = q(QuestionKind::Score {
            levels: vec!["1".into(), "2".into(), "3".into()],
        });
        assert_eq!(read(&score, "3"), Ok(Some("2".into())));
        assert_eq!(
            read_label(&score, "1", ScoreLabels::Index),
            Ok(Some("1".into()))
        );
        let e = read_label(&score, "3", ScoreLabels::Index).unwrap_err();
        assert!(e.contains("0 to 2"), "{e}");
        let score = q(QuestionKind::Score {
            levels: vec!["later".into(), "today".into()],
        });
        assert_eq!(read(&score, "today"), Ok(Some("1".into())));
        let e = read(&score, "0").unwrap_err();
        assert!(e.contains("not the text of a level"), "{e}");
    }

    fn levels(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn a_score_column_reads_one_way_as_a_whole_or_is_refused() {
        let numbered = levels(&["1", "2", "3", "4", "5"]);
        // 0-based indices: "0" is no text, so the column is indices, and
        // "1" is level 1, not the text of level 0.
        assert_eq!(
            score_column(&numbered, ["0", "1", "", "4"]),
            ScoreColumn::Reads(ScoreLabels::Index)
        );
        // "5" is no index: texts.
        assert_eq!(
            score_column(&numbered, ["1", "5", "3"]),
            ScoreColumn::Reads(ScoreLabels::Text)
        );
        // Both fit and disagree: never a guess.
        assert_eq!(
            score_column(&numbered, ["", "1", "2"]),
            ScoreColumn::Ambiguous {
                at: 1,
                value: "1".into(),
                text_level: 0
            }
        );
        // Both fit and agree: a scale written "0" to "2" reads the same
        // either way.
        assert_eq!(
            score_column(&levels(&["0", "1", "2"]), ["0", "2"]),
            ScoreColumn::Reads(ScoreLabels::Text)
        );
        let words = levels(&["later", "today"]);
        assert_eq!(
            score_column(&words, ["today", "1", "later", "0"]),
            ScoreColumn::Mixed {
                texts: vec![0, 2],
                indices: vec![1, 3]
            }
        );
        // A value that is neither has no say; read_label refuses it.
        assert_eq!(
            score_column(&words, ["today", "soon"]),
            ScoreColumn::Reads(ScoreLabels::Text)
        );
        assert_eq!(
            score_column(&words, std::iter::empty()),
            ScoreColumn::Reads(ScoreLabels::Text)
        );
    }
}
