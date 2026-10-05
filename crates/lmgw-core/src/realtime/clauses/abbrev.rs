//! Abbreviations a voice says in full (realtime §8.1, live acceptance L7).
//!
//! The first live run spoke "e.g." as "egg", and cut a sentence after the
//! "z." of "z. B.". So the few abbreviations an answer commonly carries are
//! known by name: the clause splitter never ends a sentence at one of their
//! dots, and the speakable pass writes them out — the spoken transcript is
//! the written-out text, because that is what the listener heard.
//!
//! **Some end a sentence too** (package B review 2): "… Äpfel, Birnen usw.
//! Dann …" hid the sentence's stop inside the abbreviation. The ones that
//! can stand last ([`SENTENCE_FINAL`]) end the sentence when a capital
//! follows, and keep their full stop when written out. `bzw.` and `ca.` are
//! not among them: they always introduce what follows, and German
//! capitalizes nouns ("bzw. Birnen", "ca. Mitte Mai"), so a capital after
//! them says nothing.
//!
//! **Titles** ([`TITLES`]: "Dr. Müller") are never a sentence end either,
//! and are said as written: every voice reads them, and the name follows.
//!
//! The tables are small and visible on purpose: guessing at every word with a
//! dot after it would cut real sentence ends ("…in Africa."). Matching is
//! **case-sensitive** (a sentence-initial form is its own entry, and "Ca."
//! stays calcium) and **word-boundary safe**: no letter or digit may touch
//! either end, so "Africa." is no "ca." and a decimal like "3.14" never
//! matches anything.

/// Each abbreviation as written, and as spoken.
pub const ABBREVIATIONS: &[(&str, &str)] = &[
    ("e.g.", "for example"),
    ("E.g.", "For example"),
    ("i.e.", "that is"),
    ("I.e.", "That is"),
    ("etc.", "et cetera"),
    ("vs.", "versus"),
    ("z. B.", "zum Beispiel"),
    ("z.B.", "zum Beispiel"),
    ("Z. B.", "Zum Beispiel"),
    ("Z.B.", "Zum Beispiel"),
    ("d. h.", "das heißt"),
    ("d.h.", "das heißt"),
    ("D. h.", "Das heißt"),
    ("D.h.", "Das heißt"),
    ("usw.", "und so weiter"),
    ("bzw.", "beziehungsweise"),
    ("ca.", "circa"),
];

/// The abbreviations that may also end a sentence: one followed by a
/// capital does (module doc).
pub const SENTENCE_FINAL: &[&str] = &["etc.", "usw."];

/// Titles: never a sentence end, and not written out (module doc).
pub const TITLES: &[&str] = &["Dr.", "Mr.", "Mrs.", "Prof.", "Nr.", "St."];

/// Every name whose dot is no sentence end by itself.
fn names() -> impl Iterator<Item = &'static str> {
    ABBREVIATIONS
        .iter()
        .map(|(written, _)| *written)
        .chain(TITLES.iter().copied())
}

/// Whether what follows an abbreviation that may end a sentence starts
/// the next one: a capital after the space.
fn starts_sentence(after: &str) -> Option<bool> {
    after.trim_start().chars().next().map(char::is_uppercase)
}

/// Whether a `.` is part of a known abbreviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtDot {
    /// It is: not a sentence end.
    Yes,
    /// It could be, and the text that would tell has not arrived yet.
    Pending,
    No,
}

/// Whether the `.` that ends at byte `end` of `buf` belongs to a known
/// abbreviation — its last dot, or an inner one with the rest of it
/// following — and is no sentence end (module doc).
pub(super) fn at_dot(buf: &str, end: usize) -> AtDot {
    let (before, after) = buf.split_at(end);
    let mut pending = false;
    for abbrev in names() {
        for (k, _) in abbrev.match_indices('.') {
            let head = &abbrev[..=k];
            let Some(start) = before.len().checked_sub(head.len()) else {
                continue;
            };
            if !before.ends_with(head) || !boundary_before(before, start) {
                continue;
            }
            let rest = &abbrev[k + 1..];
            if rest.is_empty() && SENTENCE_FINAL.contains(&abbrev) {
                match starts_sentence(after) {
                    // Its dot is the sentence's too.
                    Some(true) => return AtDot::No,
                    Some(false) => return AtDot::Yes,
                    None => pending = true,
                }
                continue;
            }
            if after.starts_with(rest) {
                return AtDot::Yes;
            }
            if rest.starts_with(after) {
                pending = true;
            }
        }
    }
    if pending {
        AtDot::Pending
    } else {
        AtDot::No
    }
}

/// `text` with every known abbreviation written out. One that ends the text
/// keeps a full stop after it, so the sentence still sounds finished — as
/// does one that ends a sentence before the next (module doc).
pub(super) fn expand(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        if boundary_before(text, i) {
            for (abbrev, spoken) in ABBREVIATIONS {
                let end = i + abbrev.len();
                if text[i..].starts_with(abbrev) && boundary_after(text, end) {
                    out.push_str(spoken);
                    let last = text[end..].trim().is_empty();
                    let ends = SENTENCE_FINAL.contains(abbrev)
                        && starts_sentence(&text[end..]) == Some(true);
                    if last || ends {
                        out.push('.');
                    }
                    i = end;
                    continue 'scan;
                }
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// No letter or digit right before byte `at`.
fn boundary_before(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// No letter or digit right at byte `at`.
fn boundary_after(text: &str, at: usize) -> bool {
    text[at..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric())
}
