//! A text as a stream of comparable characters: the letters and digits of
//! its NFC form, case-folded, each pointing back at the chars of the
//! original text it came from.

use std::ops::Range;

use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// One comparable character and where it came from.
pub(super) struct Unit {
    /// A letter or digit of the case-folded NFC form. Folding can make
    /// several of these out of one letter (`ß` is `ss`), which then share
    /// their `from`/`to`.
    pub ch: char,
    /// The `[from, to)` range of the original text's chars this came from —
    /// more than one char when a base letter and its combining marks make one
    /// NFC character, and the whole run for every unit that run folds to.
    pub from: usize,
    pub to: usize,
    /// First letter or digit after a gap (punctuation, whitespace, a symbol)
    /// or at the start of the text: a word of the text begins here.
    pub starts_token: bool,
}

/// The units of one text.
pub(super) struct Stream {
    pub units: Vec<Unit>,
}

/// Full Unicode case folding (`CaseFolding.txt`, statuses C and F) of one
/// char, which `char::to_lowercase` is not: `ß` folds to `ss` so it meets
/// `SS` and `ss`, and a final `ς`, a medial `σ` and `Σ` all fold to `σ`.
/// `İ` folds to `i` plus a combining dot above; the dot is a mark, so `İ`
/// meets `i`, `I` and the decomposed spelling alike (a model reading
/// `İstanbul` out loud will write whichever it likes).
fn fold(c: char, out: &mut String) {
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
    } else {
        out.push_str(&caseless::default_case_fold_str(c.encode_utf8(&mut [0; 4])));
    }
}

impl Stream {
    /// `text` as comparable units. Punctuation, whitespace and symbols are
    /// left out and make a token boundary.
    ///
    /// Not handled yet: a letter the standard writes as several scalar
    /// values that do not compose in NFC — Hangul conjoining jamo and the
    /// two-part vowel signs of Indic scripts (Tamil `ொ`, Bengali `ো`) — is
    /// read as the marks it is made of. A mark that is not a letter, digit
    /// or canonical combining mark ends the token, and it is not a unit.
    pub fn new(text: &str) -> Stream {
        let chars: Vec<char> = text.chars().collect();
        let mut units = Vec::new();
        let mut boundary = true;
        let mut i = 0;
        while i < chars.len() {
            // A base character with the combining marks after it normalises
            // as one run, so the units made of it all point back at the
            // whole run.
            let mut j = i + 1;
            while j < chars.len() && canonical_combining_class(chars[j]) != 0 {
                j += 1;
            }
            let mut folded = String::new();
            chars[i..j]
                .iter()
                .copied()
                .nfc()
                .for_each(|c| fold(c, &mut folded));
            // Folded first, filtered after: a fold can leave a mark behind
            // (`İ` is `i` and a dot above), which goes the way of every mark
            // that does not compose into its letter.
            let folded: String = folded.nfc().filter(|c| c.is_alphanumeric()).collect();
            if folded.is_empty() {
                boundary = true;
            }
            for ch in folded.chars() {
                units.push(Unit {
                    ch,
                    from: i,
                    to: j,
                    starts_token: std::mem::take(&mut boundary),
                });
            }
            i = j;
        }
        Stream { units }
    }

    /// The unit ranges of the text's words: runs of letters and digits.
    pub fn tokens(&self) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for (n, u) in self.units.iter().enumerate() {
            match out.last_mut() {
                Some(t) if !u.starts_token => t.end = n + 1,
                _ => out.push(n..n + 1),
            }
        }
        out
    }

    /// The units in `range` as a string, for comparing tokens.
    pub fn text(&self, range: Range<usize>) -> String {
        self.units[range].iter().map(|u| u.ch).collect()
    }
}
