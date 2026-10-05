//! Sentence ends beyond `.`, `!` and `?` (realtime §8.1; TTS batches,
//! 2026-10-05). With no word cap, a script whose sentences end with another
//! mark got no cut until a line end, and its first audio waited for a whole
//! paragraph.
//!
//! - **A spaced stop** ([`spaced`]) is written with a space after it, as `!`
//!   and `?` are: the Devanagari and Bengali danda `।` and double danda `॥`,
//!   the Armenian full stop `։`, the Arabic question mark `؟`, the Urdu full
//!   stop `۔`, the Ethiopic full stop `።`. It cuts like they do, once whitespace follows. None of
//!   the `.`-only logic (abbreviations, ordinals, decimals) applies to them.
//! - **A wide stop** ([`wide`]) is the CJK `。` `！` `？` and half-width `｡`:
//!   no space follows it, so it cuts at once — but a closing quote or bracket
//!   after it belongs to its sentence ("彼は「行く。」と言った"), as does
//!   another stop of the run ("えっ？！"). [`run_end`] finds the cut after
//!   them; at the buffer's end the next character may still be one, so the
//!   scan waits.

/// A stop written with whitespace after it (module doc): `!`, `?` and the
/// scripts' own.
pub(super) fn spaced(c: char) -> bool {
    matches!(
        c,
        '!' | '?' | '\u{0964}' | '\u{0965}' | '\u{0589}' | '\u{061F}' | '\u{06D4}' | '\u{1362}'
    )
}

/// A stop of CJK text, no space after it (module doc).
pub(super) fn wide(c: char) -> bool {
    matches!(c, '。' | '！' | '？' | '｡')
}

/// A closing quote or bracket, which stays with the sentence it closes.
pub(super) fn closer(c: char) -> bool {
    matches!(
        c,
        '」' | '』'
            | '）'
            | '］'
            | '】'
            | '〕'
            | '〉'
            | '》'
            | '｣'
            | ')'
            | ']'
            | '"'
            | '\''
            | '”'
            | '’'
            | '»'
    )
}

/// Where the sentence ended by the wide stop that ends at byte `from` of
/// `buf` ends: after the closers and further wide stops that follow it. `None`
/// when `buf` ends inside that run — the next character may belong to it.
pub(super) fn run_end(buf: &str, from: usize) -> Option<usize> {
    let mut end = from;
    for c in buf[from..].chars() {
        if !closer(c) && !wide(c) {
            return Some(end);
        }
        end += c.len_utf8();
    }
    None
}
