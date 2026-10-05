//! Delivery cues (realtime design §5.5, §8.1, §8.2; WP9b): a `[laughing]`
//! that opens a spoken clause, sent to a TTS that renders no tags as how to
//! say the clause rather than as a sound.
//!
//! - **One grammar** (C2): a cue is an inline tag of [`super::tags`] in a new
//!   place — every tag at the very start of a clause. Several join with
//!   ", " (`[excited] [laughing]` → "excited, laughing"), `_` and `-` are
//!   spaces, and what punctuated them goes with them, as in the transcript
//!   (`clauses::Spoken`): "[laughing], ja." is sent "ja.". A tag inside the
//!   clause is no cue, nor is a bracket the grammar does not take (`[1]`,
//!   `[S1]`, a link's text): those are shaped as ever.
//! - **Which routes** ([`takes`], C1): a `style` or `passthrough` route that
//!   renders no tags — [`lmgw_api_types::realtime::takes_cues`], the one
//!   rule the dashboard goes by too. A cloud alias nobody described is
//!   passthrough by assumption only ([`Expressive::assumed`]) and takes
//!   none. On any other route the clause goes as it came, and shaping maps
//!   or strips its tags.
//! - **Combined with the style** ([`combine`], C4): appended, never a
//!   replacement — `"{style}, but {cue} right now"`. The style is the one in
//!   effect: the clause's instructions, else the answering row's own
//!   description ([`row_description`]), which a sent text would otherwise
//!   replace under both keys (`super::shape`, R2).
//! - **Best effort.** Qwen3 CustomVoice takes a cue as a nudge: the words
//!   themselves dominate, so a funny line laughs easily and a neutral one
//!   rarely, whatever the cue says.
//!
//! The responder works out each clause's cue without knowing the route
//! (`realtime::responder::speech`); [`fold`] applies it on the route that
//! answers (`crate::proxy::synthesize`, C5).

use super::shape::Expressive;
use super::tags;
use crate::config::AudioModel;

/// The cue that opens `input`, normalised (module doc), and where its words
/// begin — past the cue and what punctuated it. `None` when `input` does
/// not open with a tag.
pub(crate) fn lead(input: &str) -> Option<(String, usize)> {
    let blank = |from: usize| input.len() - input[from..].trim_start().len();
    let mut at = blank(0);
    let mut names = Vec::new();
    let mut end = None;
    for span in tags::spans(input) {
        // A link's text is no tag: `[see](url)`.
        if span.start != at || input[span.end..].starts_with('(') {
            break;
        }
        let name = span
            .name
            .replace(['_', '-'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() {
            names.push(name);
        }
        end = Some(span.end);
        at = blank(span.end);
    }
    let end = end.filter(|_| !names.is_empty())?;
    let rest = &input[end..];
    let words =
        rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '.'));
    Some((names.join(", "), input.len() - words.len()))
}

/// A clause's instructions with `cue` (C4): `"{base}, but {cue} right
/// now"`, or the cue alone with no base — a blank one is none. The
/// contrast is the point: in the listening tests of 2026-10-02 Qwen3
/// CustomVoice heard `"{base}; {cue}"` as one more adjective of a calm
/// style and muted the laugh (1 of 8 renders laughed), while "…, but
/// laughing right now" laughed in 4 of 8. The base's own closing
/// punctuation goes first: a style written as prose ("Speak calmly.")
/// would otherwise go out as "Speak calmly., but laughing right now" on
/// every cued clause. The full-width marks are there because Qwen3 reads
/// Chinese instructions too.
pub(crate) fn combine(base: Option<&str>, cue: &str) -> String {
    let closing = |c: char| {
        c.is_whitespace()
            || matches!(c, '.' | '!' | '?' | ';' | ',' | ':' | '…')
            || matches!(c, '。' | '！' | '？' | '；' | '，' | '：')
    };
    match base
        .map(|b| b.trim_start().trim_end_matches(closing))
        .filter(|b| !b.is_empty())
    {
        Some(base) => format!("{base}, but {cue} right now"),
        None => cue.to_string(),
    }
}

/// A row's own description of its voice or style: its default request
/// options' `instruction`, else `instruct` — the text its engine merges into
/// every request that sends none.
pub(crate) fn row_description(row: &AudioModel) -> Option<String> {
    ["instruction", "instruct"].iter().find_map(|k| {
        row.default_request_options
            .get(*k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    })
}

/// Whether a route with `rules` takes cues (C1).
pub(crate) fn takes(rules: &Expressive) -> bool {
    lmgw_api_types::realtime::takes_cues(
        rules.declared_instructions(),
        rules.tags.as_str(),
        &rules.vocab,
    )
}

/// A clause with its cue folded in (C5): what it is sent instead of the
/// caller's text and instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Folded<'a> {
    /// The text from its words on: the leading tags are the cue now.
    pub input: &'a str,
    /// The style in effect with the cue after it ([`combine`]).
    pub instructions: String,
}

/// `cue` folded into a clause sent on a route whose `rules` take cues:
/// `input`'s leading tags left out, `instructions` — else `row`'s own
/// description, `row` being the lmgw audio row that answers — with the cue
/// after them. `None` with no cue or on a route that takes none: the
/// clause goes as it came, and shaping handles its tags.
pub(crate) fn fold<'a>(
    rules: &Expressive,
    row: Option<&AudioModel>,
    input: &'a str,
    instructions: Option<&str>,
    cue: Option<&str>,
) -> Option<Folded<'a>> {
    let cue = cue.filter(|c| !c.trim().is_empty() && takes(rules))?;
    let input = lead(input).map_or(input, |(_, at)| &input[at..]);
    let own = instructions.map(str::trim).filter(|t| !t.is_empty());
    let base = own
        .map(str::to_string)
        .or_else(|| row.and_then(row_description));
    Some(Folded {
        input,
        instructions: combine(base.as_deref(), cue),
    })
}

#[cfg(test)]
mod tests;
