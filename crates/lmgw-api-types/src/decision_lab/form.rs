//! The Decision lab's form and the one function that turns it into the body
//! `/v1/systemone` receives (labs-store design §2.3, §2.4).
//!
//! The builder writes the JSON text itself rather than serializing a
//! `SystemOneRequest`: a JSON-mode state goes out as typed, byte for byte,
//! and a `Value` on the way would sort an object state's members. Ids,
//! options, levels and questions go out in the order entered. The page's
//! Request panel shows what this writes and the server dispatches what this
//! writes, so the two cannot drift.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The route a cell is dispatched to.
pub const ENDPOINT: &str = "/v1/systemone";

/// How many options the wire allows a `choice` question (TypeSafe's limit).
/// Some decision types allow fewer, and llama.cpp's 400 names the limit.
pub const WIRE_MAX_OPTIONS: usize = 255;

/// How many levels a `score` question has: 2 to 10.
pub const SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

/// How long a title derived from the state may be, in characters, before it
/// ends in an ellipsis. A label for the history list: the form keeps the
/// whole state.
pub const TITLE_CHARS: usize = 120;

/// How the state is written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum StateMode {
    /// Plain text, sent as a JSON string.
    #[default]
    Text,
    /// JSON text (an object, an array, `null` for the types that take no
    /// state), checked to parse and sent as typed.
    Json,
}

/// One image the form sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "DecisionImageInput"))]
pub enum ImageInput {
    /// A new image, as the page read it.
    DataUrl { name: String, url: String },
    /// An image the store keeps already (a re-opened form): its artifact id.
    /// Nothing is uploaded again.
    Artifact { id: i64, name: String },
}

impl ImageInput {
    pub fn name(&self) -> &str {
        match self {
            Self::DataUrl { name, .. } | Self::Artifact { name, .. } => name,
        }
    }
}

/// One option of a `choice` question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OptionDraft {
    pub name: String,
    /// What the option means; blank sends `null`.
    pub description: String,
}

/// A question's type and its type's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "DecisionQuestionKind"))]
pub enum QuestionKind {
    /// Pick one of the options, in the order offered.
    Choice { options: Vec<OptionDraft> },
    /// Rate on 2 to 10 levels, lowest first.
    Score { levels: Vec<String> },
    /// Yes or no; `yes` and `no` say what each means, both blank sends no
    /// `criteria`.
    Noul { yes: String, no: String },
}

impl Default for QuestionKind {
    fn default() -> Self {
        Self::Choice {
            options: Vec::new(),
        }
    }
}

impl QuestionKind {
    /// The wire's name for the type.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Noul { .. } => "noul",
        }
    }
}

/// One question as the form holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QuestionDraft {
    /// The key the answer comes back under.
    pub id: String,
    pub instructions: String,
    pub kind: QuestionKind,
}

/// What a cell sends: the form as written, or one of the two perturbations
/// (labs-store design §2.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Variant {
    #[default]
    AsWritten,
    /// Every `choice` description `null`, and no `noul` criteria.
    NoDescriptions,
    /// Each `choice` question's options in reverse.
    Reversed,
}

impl Variant {
    pub const ALL: [Self; 3] = [Self::AsWritten, Self::NoDescriptions, Self::Reversed];

    /// The stored spelling (a plan cell's `variant`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AsWritten => "as_written",
            Self::NoDescriptions => "no_descriptions",
            Self::Reversed => "reversed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// How a card names it ("decide/laya · reversed").
    pub fn label(self) -> &'static str {
        match self {
            Self::AsWritten => "as written",
            Self::NoDescriptions => "descriptions dropped",
            Self::Reversed => "order reversed",
        }
    }
}

/// The Decision lab's whole form, as the page holds it and the store keeps
/// it (`lab_groups.form`). Every list is a `Vec`, so storing it keeps its
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionForm {
    pub state_mode: StateMode,
    /// As typed; JSON mode keeps its text verbatim.
    pub state: String,
    pub images: Vec<ImageInput>,
    pub questions: Vec<QuestionDraft>,
}

/// What the gauge counts, piece by piece (labs-store design §2.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CountPieces {
    /// The state: Text mode's string, JSON mode's text as typed.
    pub state: String,
    /// Each question: its id, its text (the instructions, then each option's
    /// name and description, each level, or the `noul` criteria, joined by
    /// newlines), and for a `choice` question its options alone (the option
    /// span).
    pub questions: Vec<(String, String, Option<String>)>,
}

/// A JSON string literal of `s`.
fn quote(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// One member line, `"key": value`, at `pad`. The value goes in as given: a
/// JSON-mode state is inserted verbatim and not re-indented.
fn push_member(out: &mut String, pad: &str, key: &str, value: &str, last: bool) {
    out.push_str(pad);
    out.push_str(&quote(key));
    out.push_str(": ");
    out.push_str(value);
    if !last {
        out.push(',');
    }
    out.push('\n');
}

impl DecisionForm {
    /// What the wire documents, checked before a request is built (§2.3): a
    /// non-empty state that parses in JSON mode (`null` included); at least
    /// one question; non-empty, unique ids; a `choice` with at least one
    /// option, its names non-empty and unique; a `score` with 2 to 10
    /// levels. The message names the field. Anything else is the upstream's
    /// to refuse.
    pub fn check(&self) -> Result<(), String> {
        if self.state.trim().is_empty() {
            return Err("state: write what the questions are about".into());
        }
        if self.state_mode == StateMode::Json {
            serde_json::from_str::<IgnoredAny>(&self.state)
                .map_err(|e| format!("state: not JSON ({e}); switch to Text to send it as text"))?;
        }
        check_questions(&self.questions)
    }

    /// The state as JSON text: Text mode's string as a JSON string, JSON
    /// mode's text as typed, once it parses.
    pub fn state_json(&self) -> Result<String, String> {
        match self.state_mode {
            StateMode::Text => Ok(quote(&self.state)),
            StateMode::Json => {
                serde_json::from_str::<IgnoredAny>(&self.state)
                    .map_err(|e| format!("state: not JSON ({e})"))?;
                Ok(self.state.clone())
            }
        }
    }
}

/// The questions' part of [`DecisionForm::check`], shared with a question
/// set: at least one question; non-empty, unique ids; a `choice` with at
/// least one option, its names non-empty and unique; a `score` with 2 to 10
/// levels. The message names the field.
pub fn check_questions(questions: &[QuestionDraft]) -> Result<(), String> {
    if questions.is_empty() {
        return Err("questions: add at least one".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let at = format!("question {}", i + 1);
        if q.id.trim().is_empty() {
            return Err(format!("{at}: give it an id"));
        }
        if seen.contains(&q.id.as_str()) {
            return Err(format!("{at}: the id '{}' is used twice", q.id));
        }
        seen.push(&q.id);
        let at = format!("question '{}'", q.id);
        match &q.kind {
            QuestionKind::Choice { options } => {
                if options.is_empty() {
                    return Err(format!("{at}: a choice needs at least one option"));
                }
                let mut names: Vec<&str> = Vec::new();
                for (j, o) in options.iter().enumerate() {
                    if o.name.trim().is_empty() {
                        return Err(format!("{at}: option {} has no name", j + 1));
                    }
                    if names.contains(&o.name.as_str()) {
                        return Err(format!("{at}: the option '{}' is offered twice", o.name));
                    }
                    names.push(&o.name);
                }
            }
            QuestionKind::Score { levels } => {
                if !SCORE_LEVELS.contains(&levels.len()) {
                    return Err(format!(
                        "{at}: a score has {} to {} levels, not {}",
                        SCORE_LEVELS.start(),
                        SCORE_LEVELS.end(),
                        levels.len()
                    ));
                }
            }
            QuestionKind::Noul { .. } => {}
        }
    }
    Ok(())
}

impl DecisionForm {
    /// The `/v1/systemone` body for `model` in `variant`, with `images` (data
    /// URLs, or the stored request's `lab-artifact:<id>`) as its `images`.
    /// Indented JSON with the members `model`, `state`, `questions`, then
    /// `images` when there are any. The one builder: the page's preview and
    /// every cell the server dispatches are this text.
    pub fn body(&self, model: &str, variant: Variant, images: &[String]) -> Result<String, String> {
        let state = self.state_json()?;
        let mut out = String::from("{\n");
        push_member(&mut out, "  ", "model", &quote(model), false);
        push_member(&mut out, "  ", "state", &state, false);
        let questions = self.questions_json(variant);
        push_member(&mut out, "  ", "questions", &questions, images.is_empty());
        if !images.is_empty() {
            let mut list = String::from("[\n");
            for (i, url) in images.iter().enumerate() {
                list.push_str("    ");
                list.push_str(&quote(url));
                if i + 1 < images.len() {
                    list.push(',');
                }
                list.push('\n');
            }
            list.push_str("  ]");
            push_member(&mut out, "  ", "images", &list, true);
        }
        out.push('}');
        Ok(out)
    }

    /// The `questions` object, indented to sit at the body's first level.
    fn questions_json(&self, variant: Variant) -> String {
        if self.questions.is_empty() {
            return "{}".into();
        }
        let mut out = String::from("{\n");
        for (i, q) in self.questions.iter().enumerate() {
            let mut members: Vec<(&str, String)> = vec![
                ("type", quote(q.kind.type_name())),
                ("instructions", quote(&q.instructions)),
            ];
            match &q.kind {
                QuestionKind::Choice { options } => {
                    let mut options: Vec<&OptionDraft> = options.iter().collect();
                    if variant == Variant::Reversed {
                        options.reverse();
                    }
                    let entries: Vec<(String, String)> = options
                        .iter()
                        .map(|o| {
                            let d = o.description.trim();
                            let value = if d.is_empty() || variant == Variant::NoDescriptions {
                                "null".to_string()
                            } else {
                                quote(&o.description)
                            };
                            (quote(&o.name), value)
                        })
                        .collect();
                    members.push(("criteria", object(&entries, 4)));
                }
                QuestionKind::Score { levels } => {
                    members.push(("criteria", array(levels, 4)));
                }
                QuestionKind::Noul { yes, no } => {
                    let mut entries = Vec::new();
                    if !yes.trim().is_empty() {
                        entries.push((quote("true"), quote(yes)));
                    }
                    if !no.trim().is_empty() {
                        entries.push((quote("false"), quote(no)));
                    }
                    if !entries.is_empty() && variant != Variant::NoDescriptions {
                        members.push(("criteria", object(&entries, 4)));
                    }
                }
            }
            out.push_str("    ");
            out.push_str(&quote(&q.id));
            out.push_str(": {\n");
            for (j, (k, v)) in members.iter().enumerate() {
                push_member(&mut out, "      ", k, v, j + 1 == members.len());
            }
            out.push_str("    }");
            if i + 1 < self.questions.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  }");
        out
    }

    /// The history's title: the state's first non-empty line, trimmed, at
    /// most [`TITLE_CHARS`] characters (then an ellipsis).
    pub fn title(&self) -> String {
        let line = self
            .state
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default();
        if line.is_empty() {
            return match self.questions.first() {
                Some(q) if !q.id.trim().is_empty() => q.id.clone(),
                _ => "(no state)".into(),
            };
        }
        if line.chars().count() <= TITLE_CHARS {
            return line.to_string();
        }
        let mut t: String = line.chars().take(TITLE_CHARS).collect();
        t.push('…');
        t
    }

    /// What the gauge counts (labs-store design §2.5).
    pub fn count_pieces(&self) -> CountPieces {
        let questions = self
            .questions
            .iter()
            .map(|q| {
                let mut parts = vec![q.instructions.clone()];
                let mut span = None;
                match &q.kind {
                    QuestionKind::Choice { options } => {
                        let mut opts = Vec::new();
                        for o in options {
                            opts.push(o.name.clone());
                            if !o.description.trim().is_empty() {
                                opts.push(o.description.clone());
                            }
                        }
                        parts.extend(opts.iter().cloned());
                        span = Some(opts.join("\n"));
                    }
                    QuestionKind::Score { levels } => parts.extend(levels.iter().cloned()),
                    QuestionKind::Noul { yes, no } => {
                        parts.extend(
                            [yes, no]
                                .into_iter()
                                .filter(|s| !s.trim().is_empty())
                                .cloned(),
                        );
                    }
                }
                (q.id.clone(), parts.join("\n"), span)
            })
            .collect();
        CountPieces {
            state: self.state.clone(),
            questions,
        }
    }
}

/// `{k: v, ...}` with members at `level` (two spaces each), the closing
/// brace one level up.
fn object(entries: &[(String, String)], level: usize) -> String {
    if entries.is_empty() {
        return "{}".into();
    }
    let pad = "  ".repeat(level);
    let mut out = String::from("{\n");
    for (i, (k, v)) in entries.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        if i + 1 < entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&"  ".repeat(level - 1));
    out.push('}');
    out
}

/// `[s, ...]` of strings, items at `level`.
fn array(items: &[String], level: usize) -> String {
    if items.is_empty() {
        return "[]".into();
    }
    let pad = "  ".repeat(level);
    let mut out = String::from("[\n");
    for (i, s) in items.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&quote(s));
        if i + 1 < items.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&"  ".repeat(level - 1));
    out.push(']');
    out
}

/// A data URL as the Request panel shows it: the head kept, the payload
/// elided with its decoded size ("data:image/png;base64,… 1.4 MB"). Any
/// other string comes back as it is.
pub fn elide_data_url(url: &str) -> String {
    let Some((head, payload)) = url.split_once(',') else {
        return url.to_string();
    };
    if !head.starts_with("data:") {
        return url.to_string();
    }
    let bytes = if head.ends_with(";base64") {
        payload.trim_end_matches('=').len() * 3 / 4
    } else {
        payload.len()
    };
    format!("{head},… {}", human_bytes(bytes as u64))
}

/// `1.4 MB`, `812 kB`, `96 B`: decimal units, one decimal from MB on.
pub fn human_bytes(n: u64) -> String {
    const KB: f64 = 1000.0;
    let f = n as f64;
    if f < KB {
        format!("{n} B")
    } else if f < KB * KB {
        format!("{:.0} kB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.1} GB", f / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests;
