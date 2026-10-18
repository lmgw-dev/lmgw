//! A set run's input (labs-store design §3.3): a CSV (RFC 4180 with a header
//! row, read with the `csv` crate) or JSONL (one object per line, its
//! columns its top-level keys) file, the columns it maps to the state and to
//! each question's expected answer, and the check every row goes through.
//!
//! Nothing is dropped: a line that does not read, a state that is empty or no
//! JSON in JSON mode, and a label that is no answer of its question each
//! refuse the whole input, every one of them named with its row, line,
//! column and value. An empty label is no label: the item runs unlabelled.
//! A `score` question's label column is read as a whole, as level texts or
//! as level indices, never value by value: a column both readings fit with
//! different meanings, or one that mixes the two, refuses the input until
//! the mapping's `score_labels` says which.

use lmgw_api_types::decision_lab::{
    read_label, score_column, BatchColumn, BatchFormat, BatchMapping, BatchPreview, BatchPreviewed,
    BatchProblem, BatchProblemKind, QuestionDraft, QuestionKind, ScoreColumn, ScoreLabels,
    StateMode, PREVIEW_ROWS,
};
use lmgw_api_types::systemone::Ordered;
use serde::de::IgnoredAny;
use serde_json::value::RawValue;

/// One row of the file: the line it starts on and a value per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: u64,
    pub cells: Vec<String>,
}

/// The file as read: its columns, its rows, and what did not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub problems: Vec<BatchProblem>,
}

/// One item a set run asks: its state as the mapping sends it, its labels
/// in the store's spelling per question id, and its key.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub state: String,
    pub expected: Ordered<String>,
    /// The key column's value; `None` without a key column or for an empty
    /// value.
    pub key: Option<String>,
}

/// How one label column is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Every value one way (the only way, for a `choice` or a `noul`).
    As(ScoreLabels),
    /// A `score` column with no one reading: the column's problem is named,
    /// and a value is only checked to be a level at all.
    Unsettled,
}

fn file_problem(line: Option<u64>, message: String) -> BatchProblem {
    BatchProblem {
        kind: BatchProblemKind::File,
        question: None,
        row: None,
        line,
        column: None,
        value: None,
        message,
    }
}

/// A problem of the mapping, about `question` when it names one.
fn mapping_problem(question: Option<&str>, message: String) -> BatchProblem {
    BatchProblem {
        kind: BatchProblemKind::Mapping,
        question: question.map(String::from),
        ..file_problem(None, message)
    }
}

/// Read `text` as `format`.
pub fn parse(format: BatchFormat, text: &str) -> Table {
    // A UTF-8 byte order mark is the file's, not its first column's.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format {
        BatchFormat::Csv => parse_csv(text),
        BatchFormat::Jsonl => parse_jsonl(text),
    }
}

/// The line a record starts on, from its byte offset. The `csv` crate's own
/// line is one short after a CRLF (it takes the record's position before the
/// previous record's `\n`), so the line is counted here: the newlines before
/// the record's first byte that is no line break. Records come in order, so
/// the count runs on from the last one.
struct Lines<'a> {
    text: &'a [u8],
    at: usize,
    newlines: u64,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text: text.as_bytes(),
            at: 0,
            newlines: 0,
        }
    }

    fn of(&mut self, pos: &::csv::Position) -> u64 {
        let mut s = usize::try_from(pos.byte())
            .unwrap_or(usize::MAX)
            .min(self.text.len());
        while s < self.text.len() && matches!(self.text[s], b'\r' | b'\n') {
            s += 1;
        }
        if s < self.at {
            self.at = 0;
            self.newlines = 0;
        }
        let count = |b: &[u8]| b.iter().filter(|c| **c == b'\n').count() as u64;
        self.newlines += count(&self.text[self.at..s]);
        self.at = s;
        self.newlines + 1
    }
}

fn parse_csv(text: &str) -> Table {
    let mut t = Table::default();
    let mut lines = Lines::new(text);
    let mut rdr = ::csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_reader(text.as_bytes());
    match rdr.headers() {
        Ok(h) if !h.is_empty() => t.columns = h.iter().map(String::from).collect(),
        Ok(_) => {
            t.problems.push(file_problem(
                Some(1),
                "the file has no header row: a CSV names its columns in its first line".into(),
            ));
            return t;
        }
        Err(e) => {
            t.problems.push(file_problem(
                Some(1),
                format!("line 1: the header does not read ({e})"),
            ));
            return t;
        }
    }
    for rec in rdr.records() {
        match rec {
            Ok(r) => t.rows.push(Row {
                line: r.position().map_or(0, |p| lines.of(p)),
                cells: r.iter().map(String::from).collect(),
            }),
            Err(e) => {
                let line = e.position().map(|p| lines.of(p));
                let why = match e.kind() {
                    ::csv::ErrorKind::UnequalLengths {
                        expected_len, len, ..
                    } => format!("{len} fields where the header has {expected_len}"),
                    _ => e.to_string(),
                };
                let at = line.map_or_else(String::new, |l| format!("line {l}: "));
                t.problems.push(file_problem(line, format!("{at}{why}")));
            }
        }
    }
    t
}

/// A JSONL value as a cell: a string's text, `null` empty, any other value
/// its JSON as written.
fn cell_of(raw: &RawValue) -> String {
    let text = raw.get().trim();
    if text == "null" {
        return String::new();
    }
    serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_string())
}

fn parse_jsonl(text: &str) -> Table {
    let mut t = Table::default();
    let mut objects: Vec<(u64, Ordered<Box<RawValue>>)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Ordered<Box<RawValue>>>(line) {
            Ok(o) => {
                for k in o.keys() {
                    if !t.columns.iter().any(|c| c == k) {
                        t.columns.push(k.to_string());
                    }
                }
                objects.push((n, o));
            }
            Err(e) => t.problems.push(file_problem(
                Some(n),
                format!("line {n}: not a JSON object ({e})"),
            )),
        }
    }
    for (line, o) in objects {
        let cells = t
            .columns
            .iter()
            .map(|c| o.get(c).map_or_else(String::new, |raw| cell_of(raw)))
            .collect();
        t.rows.push(Row { line, cells });
    }
    t
}

/// Whether `s` is a JSON object or array.
fn is_json_document(s: &str) -> bool {
    let t = s.trim_start();
    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<IgnoredAny>(s).is_ok()
}

impl Table {
    /// The columns, each with whether every non-empty value is a JSON
    /// object or array.
    pub fn column_views(&self) -> Vec<BatchColumn> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut values = self
                    .rows
                    .iter()
                    .filter_map(|r| r.cells.get(i))
                    .filter(|v| !v.trim().is_empty())
                    .peekable();
                let json = values.peek().is_some() && values.all(|v| is_json_document(v));
                BatchColumn {
                    name: name.clone(),
                    json,
                }
            })
            .collect()
    }

    /// The column named `name`: its index, or the problem with it (not
    /// there, or named twice).
    fn column(&self, what: &str, name: &str) -> Result<usize, BatchProblem> {
        let at: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| *c == name)
            .map(|(i, _)| i)
            .collect();
        match at.as_slice() {
            [i] => Ok(*i),
            [] => Err(BatchProblem {
                column: Some(name.into()),
                ..mapping_problem(
                    None,
                    format!(
                        "{what}: the column '{name}' is not in the file (its columns: {})",
                        self.columns.join(", ")
                    ),
                )
            }),
            _ => Err(BatchProblem {
                column: Some(name.into()),
                ..mapping_problem(
                    None,
                    format!(
                        "{what}: the file has {} columns named '{name}'; give each a name of \
                         its own",
                        at.len()
                    ),
                )
            }),
        }
    }

    /// Every row as an item of `questions` under `mapping`, or every problem:
    /// the file's own, the mapping's, and each row's state and labels.
    pub fn items(
        &self,
        questions: &[QuestionDraft],
        mapping: &BatchMapping,
    ) -> Result<Vec<Item>, Vec<BatchProblem>> {
        let mut problems = self.problems.clone();
        let state = match self.mapped("state", &mapping.state) {
            Ok(i) => Some(i),
            Err(p) => {
                problems.push(p);
                None
            }
        };
        let key = match mapping.key.as_deref() {
            None => None,
            Some(name) => match self.mapped("key", name) {
                Ok(i) => Some(i),
                Err(p) => {
                    problems.push(p);
                    None
                }
            },
        };
        for (id, _) in mapping.score_labels.iter() {
            match questions.iter().find(|q| q.id == id) {
                Some(q) if matches!(q.kind, QuestionKind::Score { .. }) => {}
                Some(q) => problems.push(mapping_problem(
                    Some(id),
                    format!(
                        "score_labels: '{id}' is a {} question; only a score's labels are \
                         read as texts or indices",
                        q.kind.type_name()
                    ),
                )),
                None => problems.push(mapping_problem(
                    Some(id),
                    format!("score_labels: '{id}' is not a question of the set"),
                )),
            }
        }
        let mut labels: Vec<(&QuestionDraft, &str, usize, Reading)> = Vec::new();
        for (id, col) in mapping.labels.iter() {
            let Some(q) = questions.iter().find(|q| q.id == id) else {
                problems.push(mapping_problem(
                    Some(id),
                    format!("labels: '{id}' is not a question of the set"),
                ));
                continue;
            };
            match self.mapped(&format!("labels of '{id}'"), col) {
                Ok(i) => {
                    let reading = self.reading(q, col, i, mapping, &mut problems);
                    labels.push((q, col, i, reading));
                }
                Err(p) => problems.push(BatchProblem {
                    question: Some(id.to_string()),
                    ..p
                }),
            }
        }
        if self.rows.is_empty() && problems.is_empty() {
            problems.push(file_problem(None, "the file has no rows".into()));
        }
        let mut items = Vec::with_capacity(self.rows.len());
        for (ix, row) in self.rows.iter().enumerate() {
            let n = ix as u64 + 1;
            let state_at = |column: &str, value: &str, message: String| BatchProblem {
                kind: BatchProblemKind::State,
                ..row_problem(row, n, column, value, message)
            };
            let label_at =
                |q: &QuestionDraft, column: &str, value: &str, message: String| BatchProblem {
                    kind: BatchProblemKind::Label,
                    question: Some(q.id.clone()),
                    ..row_problem(row, n, column, value, message)
                };
            let cell = |i: usize| row.cells.get(i).map_or("", String::as_str);
            let mut text = String::new();
            if let Some(i) = state {
                text = cell(i).to_string();
                if text.trim().is_empty() {
                    problems.push(state_at(&mapping.state, &text, "the state is empty".into()));
                } else if mapping.state_mode == StateMode::Json {
                    if let Err(e) = serde_json::from_str::<IgnoredAny>(&text) {
                        problems.push(state_at(
                            &mapping.state,
                            &text,
                            format!("the state is not JSON ({e}); map it as text to send it"),
                        ));
                    }
                }
            }
            let mut expected = Ordered::default();
            for (q, col, i, reading) in &labels {
                let raw = cell(*i);
                match reading {
                    Reading::As(r) => match read_label(q, raw, *r) {
                        Ok(Some(label)) => expected.0.push((q.id.clone(), label)),
                        Ok(None) => {}
                        Err(m) => problems.push(label_at(q, col, raw, m)),
                    },
                    Reading::Unsettled => {
                        let either = [ScoreLabels::Text, ScoreLabels::Index]
                            .iter()
                            .any(|r| read_label(q, raw, *r).is_ok());
                        if !either {
                            problems.push(label_at(q, col, raw, no_level(q, raw)));
                        }
                    }
                }
            }
            let key = key
                .map(|i| cell(i).to_string())
                .filter(|k| !k.trim().is_empty());
            items.push(Item {
                state: text,
                expected,
                key,
            });
        }
        if problems.is_empty() {
            Ok(items)
        } else {
            Err(problems)
        }
    }

    /// The column the mapping names for `what`, refusing an empty name: a
    /// column whose header is empty cannot be mapped (a pandas index is
    /// one), so a mapping that names none does not take it by accident.
    fn mapped(&self, what: &str, name: &str) -> Result<usize, BatchProblem> {
        if name.trim().is_empty() {
            let message = if what == "state" {
                "state: the mapping names no state column; name the column that holds each \
                 item's state (a column whose header is empty cannot be mapped: give it a name \
                 in the file)"
                    .to_string()
            } else {
                format!(
                    "{what}: the mapping names a column with an empty name; a column whose \
                     header is empty cannot be mapped: give it a name in the file"
                )
            };
            return Err(BatchProblem {
                column: Some(String::new()),
                ..mapping_problem(None, message)
            });
        }
        self.column(what, name)
    }

    /// How question `q`'s label column `col` (at `i`) reads: a `score`'s as
    /// the mapping says, else as its values say ([`score_column`]), naming
    /// the column's problem when they say no one thing.
    fn reading(
        &self,
        q: &QuestionDraft,
        col: &str,
        i: usize,
        mapping: &BatchMapping,
        problems: &mut Vec<BatchProblem>,
    ) -> Reading {
        let QuestionKind::Score { levels } = &q.kind else {
            return Reading::As(ScoreLabels::Text);
        };
        if let Some(r) = mapping.score_labels.get(&q.id) {
            return Reading::As(*r);
        }
        let values = self
            .rows
            .iter()
            .map(|r| r.cells.get(i).map_or("", String::as_str));
        let fix = format!(
            "write the column one way, or set score_labels for '{}' to text or index",
            q.id
        );
        match score_column(levels, values) {
            ScoreColumn::Reads(r) => Reading::As(r),
            ScoreColumn::Ambiguous {
                at,
                value,
                text_level,
            } => {
                let row = &self.rows[at];
                problems.push(score_problem(
                    q,
                    row,
                    at as u64 + 1,
                    col,
                    &value,
                    format!(
                        "every value of the column reads both as a level's text and as a \
                         level's index of '{}', and the two disagree ('{value}' is the text of \
                         level {text_level} and the index of level {value}): set score_labels \
                         for '{}' to text or index",
                        q.id, q.id
                    ),
                ));
                Reading::Unsettled
            }
            ScoreColumn::Mixed { texts, indices } => {
                // The rows of the smaller part are named; each message names
                // a row of the other.
                let (odd, other, odd_is, other_are) = if indices.len() <= texts.len() {
                    (indices, texts, "a level index", "level texts")
                } else {
                    (texts, indices, "a level's text", "level indices")
                };
                let as_at = other[0] as u64 + 1;
                for at in odd {
                    let row = &self.rows[at];
                    let value = row.cells.get(i).map_or("", String::as_str).trim();
                    problems.push(score_problem(
                        q,
                        row,
                        at as u64 + 1,
                        col,
                        value,
                        format!(
                            "'{value}' is {odd_is} of '{}', but the column's other values are \
                             {other_are} (as at row {as_at}): {fix}",
                            q.id
                        ),
                    ));
                }
                Reading::Unsettled
            }
        }
    }
}

/// A problem with row `n` (`row`), in `column`, at `value`.
fn row_problem(row: &Row, n: u64, column: &str, value: &str, message: String) -> BatchProblem {
    BatchProblem {
        kind: BatchProblemKind::Label,
        question: None,
        row: Some(n),
        line: Some(row.line),
        column: Some(column.to_string()),
        value: Some(value.to_string()),
        message: format!("row {n} (line {}), column '{column}': {message}", row.line),
    }
}

/// A `score` question's label column that needs the mapping's
/// `score_labels`, named at row `n` (`row`).
fn score_problem(
    q: &QuestionDraft,
    row: &Row,
    n: u64,
    column: &str,
    value: &str,
    message: String,
) -> BatchProblem {
    BatchProblem {
        kind: BatchProblemKind::ScoreLabelsNeeded,
        question: Some(q.id.clone()),
        ..row_problem(row, n, column, value, message)
    }
}

/// Why `raw` is no level of `q` either way.
fn no_level(q: &QuestionDraft, raw: &str) -> String {
    let QuestionKind::Score { levels } = &q.kind else {
        return format!("'{}' is no label of '{}'", raw.trim(), q.id);
    };
    format!(
        "'{}' is no level of '{}' (a level's text: {}; or an index, 0 to {})",
        raw.trim(),
        q.id,
        levels.join(", "),
        levels.len().saturating_sub(1)
    )
}

/// What `POST /decision-lab/api/batches/preview` answers for `req`.
pub fn preview(req: &BatchPreview) -> BatchPreviewed {
    let t = parse(req.format, &req.text);
    let sample = req.sample.unwrap_or(PREVIEW_ROWS) as usize;
    let problems = match &req.mapping {
        Some(m) => t.items(&req.questions, m).err().unwrap_or_default(),
        None => t.problems.clone(),
    };
    BatchPreviewed {
        columns: t.column_views(),
        rows: t.rows.len() as u64,
        sample: t
            .rows
            .iter()
            .take(sample)
            .map(|r| r.cells.clone())
            .collect(),
        problems,
    }
}

/// The problems as one message, every one of them.
pub fn message(problems: &[BatchProblem]) -> String {
    let n = problems.len();
    let list: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
    format!(
        "the file cannot be run as mapped ({n} problem{}): {}",
        if n == 1 { "" } else { "s" },
        list.join("; ")
    )
}

#[cfg(test)]
mod tests;
