//! An `input` fitted to an engine's vocabulary: every character it says is
//! kept, one it cannot say is replaced by an equivalent it says, or dropped.

use unicode_normalization::char::{decompose_compatible, is_combining_mark};

use super::CharVocab;

/// What [`fit`] made of a text, and what it changed: each codepoint once, in
/// the order the text first has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fitted {
    pub text: String,
    /// Replaced by an equivalent the engine says.
    pub replaced: Vec<u32>,
    /// Dropped: nothing equivalent is in the vocabulary.
    pub dropped: Vec<u32>,
}

/// `input` with every character `vocab` cannot say ([`CharVocab::says`])
/// replaced or dropped; `None` when it says every one, and the text goes as
/// it came. A character it cannot say becomes, in this order:
/// 1. its typographic equivalent, the first of them the engine says —
///    double quotes (`„“”‟«»` and the like) `"`, else `'`; single ones
///    (`‚‘’‛‹›`, primes, accents used as one) `'`, else `"`; a dash or a
///    minus sign `-`; `…` `...`, else `.`; any other space or line break a
///    space; the capital sharp s `ẞ` `ß`, else `SS`;
/// 2. its compatibility decomposition (NFKD) without its combining marks,
///    when the engine says every base character left (`ő` → `o` when the
///    double acute is what it lacks);
/// 3. nothing: it is dropped (an emoji, a symbol outside the vocabulary).
pub fn fit(input: &str, vocab: &CharVocab) -> Option<Fitted> {
    if vocab.says_all(input) {
        return None;
    }
    let mut out = Fitted {
        text: String::with_capacity(input.len()),
        replaced: Vec::new(),
        dropped: Vec::new(),
    };
    for c in input.chars() {
        if vocab.says(c) {
            out.text.push(c);
            continue;
        }
        let instead = equivalents(c)
            .iter()
            .find(|e| vocab.says_all(e))
            .map(|e| e.to_string())
            .or_else(|| base_letters(c, vocab));
        let cp = u32::from(c);
        match instead {
            Some(s) => {
                out.text.push_str(&s);
                if !out.replaced.contains(&cp) {
                    out.replaced.push(cp);
                }
            }
            None => {
                if !out.dropped.contains(&cp) {
                    out.dropped.push(cp);
                }
            }
        }
    }
    Some(out)
}

/// What stands for `c` in plain ASCII typography, best first.
pub(super) fn equivalents(c: char) -> &'static [&'static str] {
    match c {
        // „ “ ” ‟ « » ″ 〝 〞 〟
        '\u{201E}' | '\u{201C}' | '\u{201D}' | '\u{201F}' | '\u{00AB}' | '\u{00BB}'
        | '\u{2033}' | '\u{301D}' | '\u{301E}' | '\u{301F}' => &["\"", "'"],
        // ‚ ‘ ’ ‛ ‹ › ′ ´ `
        '\u{201A}' | '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2039}' | '\u{203A}'
        | '\u{2032}' | '\u{00B4}' | '`' => &["'", "\""],
        // ‐ ‑ ‒ – — ― ⁃ − and the small and fullwidth hyphen-minus
        '\u{2010}'..='\u{2015}'
        | '\u{2043}'
        | '\u{2212}'
        | '\u{FE58}'
        | '\u{FE63}'
        | '\u{FF0D}' => &["-"],
        '\u{2026}' => &["...", "."],
        // ẞ: no decomposition, and Supertonic 3 has only the small one.
        '\u{1E9E}' => &["\u{00DF}", "SS"],
        c if c.is_whitespace() => &[" "],
        _ => &[],
    }
}

/// Every text [`fit`] may put in place of a character: what a live probe
/// sends a real engine, to see it says them all (`audio_probes_live`).
pub fn replacements() -> &'static [&'static str] {
    &["\"", "'", "-", "...", ".", " ", "\u{00DF}", "SS"]
}

/// `c`'s compatibility decomposition with its combining marks left out,
/// when something is left and the engine says all of it.
fn base_letters(c: char, vocab: &CharVocab) -> Option<String> {
    let mut base = String::new();
    decompose_compatible(c, |d| {
        if !is_combining_mark(d) {
            base.push(d);
        }
    });
    (!base.is_empty() && vocab.says_all(&base)).then_some(base)
}
