//! Inline tags in a speech request's `input` (audio-class gap 9b): the
//! non-verbal sounds and stage directions a client writes into the text
//! (`[laughs]`, `(sighs)`), kept for a family whose tokenizer renders them,
//! mapped into its vocabulary, or stripped — never spoken literally.
//!
//! What counts as a tag:
//! - the canonical client syntax `[tag]`: a lowercase letter, then 1 to 30
//!   of lowercase letters, space, `_`, `'` and `-` — a single letter (`[x]`,
//!   a checkbox; `[a]`, an enumeration) is text;
//! - `(…)`, `<…>`, `*…*` and a capitalised `[…]` only when the words inside
//!   are a stage direction of [`STAGE_DIRECTIONS`] (a closed list), so prose
//!   in parentheses or emphasis stays prose;
//! - never `<|…|>` (Fish's speaker markers), never a tag with digits or an
//!   uppercase speaker label (`[S1]`).
//!
//! Then, per family ([`TagMode`]): a **fixed** vocabulary keeps a tag it
//! names and maps a stage direction onto it (`laughs`, `laughing` →
//! `[laughter]`); a **free** family keeps every tag, in the bracket form; a
//! family with **none** has every tag stripped. Whitespace a stripped tag
//! leaves behind is collapsed. An input of nothing but tags has nothing to
//! say: [`only_tags`] is what the speech routes refuse as `empty_input`.

use serde::Serialize;

/// What a family does with inline tags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMode {
    /// Rendered as text if left in: every tag is stripped.
    #[default]
    None,
    /// A closed vocabulary the tokenizer renders (`tags`).
    Fixed,
    /// Any bracketed tag is rendered.
    Free,
}

impl TagMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fixed => "fixed",
            Self::Free => "free",
        }
    }

    /// The published word back into a mode (`capabilities.speech`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "fixed" => Some(Self::Fixed),
            "free" => Some(Self::Free),
            _ => None,
        }
    }
}

/// Stage directions lmgw recognises outside the canonical `[tag]` form, and
/// the canonical sound each means. A tag a family's vocabulary names wins;
/// this is how `(laughs)` finds `[laughter]`.
pub const STAGE_DIRECTIONS: &[(&str, &str)] = &[
    ("laugh", "laughter"),
    ("laughs", "laughter"),
    ("laughing", "laughter"),
    ("laughter", "laughter"),
    ("chuckle", "laughter"),
    ("chuckles", "laughter"),
    ("giggle", "laughter"),
    ("giggles", "laughter"),
    ("sigh", "sigh"),
    ("sighs", "sigh"),
    ("sighing", "sigh"),
    ("breath", "breath"),
    ("breathes", "breath"),
    ("inhales", "breath"),
    ("exhales", "breath"),
    ("gasp", "quick_breath"),
    ("gasps", "quick_breath"),
    ("cough", "cough"),
    ("coughs", "cough"),
    ("clears throat", "cough"),
    ("lip smack", "lipsmack"),
    ("pause", "pause"),
];

/// What [`shape_tags`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TagCounts {
    /// Left as the client wrote them (in the bracket form).
    pub kept: u32,
    /// Rewritten into the family's spelling (`[laughs]` → `[laughter]`).
    pub mapped: u32,
    /// Removed.
    pub stripped: u32,
}

impl TagCounts {
    pub fn total(&self) -> u32 {
        self.kept + self.mapped + self.stripped
    }
}

/// One tag found in the input: its byte span and the words inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
    /// Lowercase, whitespace collapsed.
    pub name: String,
    /// Written `[name]` in the canonical form (so a free family keeps it
    /// byte for byte).
    pub canonical: bool,
}

/// The canonical sound of a stage direction, if it is one.
fn stage_direction(name: &str) -> Option<&'static str> {
    STAGE_DIRECTIONS
        .iter()
        .find(|(word, _)| *word == name)
        .map(|(_, sound)| *sound)
}

fn normalise(inner: &str) -> String {
    inner
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// `[a-z][a-z _'-]{1,30}` — shared with the dashboard's cue chips.
fn is_canonical(inner: &str) -> bool {
    lmgw_api_types::realtime::is_tag_name(inner)
}

/// Every tag in `input`, in order — the one tag grammar lmgw has: the
/// speech routes' shaping and a realtime answer's clauses
/// (`realtime::clauses`) both read tags through it.
pub(crate) fn spans(input: &str) -> Vec<Span> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let close = match bytes[i] {
            b'[' => b']',
            b'(' => b')',
            b'<' => b'>',
            b'*' => b'*',
            _ => {
                i += 1;
                continue;
            }
        };
        let open = bytes[i];
        // The inside: up to 40 bytes, no line break, no nested opener.
        let rest = &input[i + 1..];
        let Some(len) = rest
            .bytes()
            .take(41)
            .position(|b| b == close || b == b'\n' || (b == open && open != b'*'))
            .filter(|&n| rest.as_bytes()[n] == close)
        else {
            i += 1;
            continue;
        };
        let inner = &rest[..len];
        let end = i + 1 + len + 1;
        // `<|speaker:0|>` and friends are the model's own markup.
        if inner.starts_with('|') || inner.trim().is_empty() {
            i += 1;
            continue;
        }
        let name = normalise(inner);
        let found = if open == b'[' && is_canonical(inner) {
            Some(Span {
                start: i,
                end,
                name,
                canonical: true,
            })
        } else if stage_direction(&name).is_some() {
            Some(Span {
                start: i,
                end,
                name,
                canonical: false,
            })
        } else {
            None
        };
        match found {
            Some(f) => {
                out.push(f);
                i = end;
            }
            None => i += 1,
        }
    }
    out
}

/// Whether `input` holds tags and nothing to say besides them — no letter
/// or digit outside every tag. Refused before anything is started: a model
/// given only `[laughter]`, or nothing once it is stripped, has no text to
/// speak.
pub fn only_tags(input: &str) -> bool {
    let tags = spans(input);
    if tags.is_empty() {
        return false;
    }
    let mut at = 0;
    let mut speakable = false;
    for t in &tags {
        speakable |= input[at..t.start].chars().any(char::is_alphanumeric);
        at = t.end;
    }
    speakable |= input[at..].chars().any(char::is_alphanumeric);
    !speakable
}

/// `input` with its tags handled the way `mode` and `vocab` (the family's
/// spelling) say; `None` when there are no tags at all.
pub fn shape_tags(input: &str, mode: TagMode, vocab: &[String]) -> Option<(String, TagCounts)> {
    let tags = spans(input);
    if tags.is_empty() {
        return None;
    }
    let mut counts = TagCounts::default();
    let mut out = String::with_capacity(input.len());
    let mut at = 0;
    for t in &tags {
        out.push_str(&input[at..t.start]);
        at = t.end;
        let keep: Option<String> = match mode {
            TagMode::Free => Some(t.name.clone()),
            TagMode::Fixed => {
                let direct = vocab.iter().find(|v| v.eq_ignore_ascii_case(&t.name));
                let mapped = || {
                    let sound = stage_direction(&t.name)?;
                    vocab.iter().find(|v| v.eq_ignore_ascii_case(sound))
                };
                direct.or_else(mapped).cloned()
            }
            TagMode::None => None,
        };
        match keep {
            Some(name) => {
                if t.canonical && name == t.name {
                    counts.kept += 1;
                    out.push_str(&input[t.start..t.end]);
                } else {
                    // A stage direction's form, or another spelling.
                    if name == t.name {
                        counts.kept += 1;
                    } else {
                        counts.mapped += 1;
                    }
                    out.push('[');
                    out.push_str(&name);
                    out.push(']');
                }
            }
            None => {
                counts.stripped += 1;
                strip_join(&mut out, &input[at..]);
                // Whitespace after the tag goes when the text already has
                // some (or nothing yet) before it.
                let skip = if out.is_empty() || out.ends_with(char::is_whitespace) {
                    input[at..].len() - input[at..].trim_start_matches([' ', '\t']).len()
                } else {
                    0
                };
                at += skip;
            }
        }
    }
    out.push_str(&input[at..]);
    // A tag stripped at the very end leaves the space before it.
    if tags.last().is_some_and(|t| t.end == input.len()) && counts.stripped > 0 {
        let trimmed = out.trim_end_matches([' ', '\t']).len();
        out.truncate(trimmed);
    }
    Some((out, counts))
}

/// Before the text after a stripped tag: a space left in front of
/// punctuation goes (`Hello [laughs].` → `Hello.`).
fn strip_join(out: &mut String, next: &str) {
    if next.starts_with(['.', ',', '!', '?', ';', ':']) {
        let trimmed = out.trim_end_matches([' ', '\t']).len();
        out.truncate(trimmed);
    }
}

#[cfg(test)]
mod tests;
