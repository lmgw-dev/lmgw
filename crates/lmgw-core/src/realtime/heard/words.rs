//! The heard part of a clause ends at a whole word (realtime design §7.3;
//! chat-voice design §8.4, WP9 review B).
//!
//! The character share a cut gives is only good to a word: a voice does not
//! speak at a uniform rate, and the count of what was heard already runs a
//! little late. So the heard part is cut back to the end of the last word
//! heard whole — never forward: a word the model repeats costs nothing, one
//! it wrongly believes was said does. "… bestimmte Lichtw" is "… bestimmte".
//!
//! - Words are UAX #29 word segments (`unicode-segmentation`), not
//!   whitespace runs: Han and Hiragana still cut per character, a number
//!   such as "21,5" or a contraction stays one word, and a combining mark
//!   is part of its letter.
//! - A hyphenated compound ("Rayleigh-Streuung") is one word: UAX #29 parts
//!   it at the hyphen, the ear does not. So is a clock time ("10:30": UAX
//!   #29 has no colon inside numbers) and a URL, any run without whitespace
//!   that holds "://" (WP11 binding review NIT 4): a cut inside them stored
//!   "10:" or "https://" as heard.
//! - The punctuation written right after the last whole word stays with it
//!   ("Hallo Jürgen," rather than "Hallo Jürgen"): it is no word that could
//!   have been missed.
//! - A cut inside the first word leaves nothing of the clause.

use unicode_segmentation::UnicodeSegmentation;

/// The heard part of `text` when its first `keep` characters were heard
/// (module doc): a prefix of `text` ending at a whole word, possibly empty.
pub(super) fn heard_words(text: &str, keep: usize) -> &str {
    let keep_at = text.char_indices().nth(keep).map_or(text.len(), |(i, _)| i);
    let mut end = 0;
    let mut segs = text.split_word_bound_indices().peekable();
    while let Some((at, seg)) = segs.next() {
        let seg_end = at + seg.len();
        if seg_end > keep_at {
            break;
        }
        if is_word(seg) && !in_compound(text, seg_end) && !in_joined(text, seg_end) {
            end = seg_end;
            // Its punctuation, written right after it.
            while let Some(&(p_at, p)) = segs.peek() {
                if !is_punctuation(p) {
                    break;
                }
                end = p_at + p.len();
                segs.next();
            }
        }
    }
    &text[..end]
}

fn is_word(seg: &str) -> bool {
    seg.chars().any(char::is_alphanumeric)
}

/// Punctuation that can follow a word: not an opening bracket, which
/// belongs to the word after it.
fn is_punctuation(seg: &str) -> bool {
    let closing = |c: char| {
        !c.is_whitespace()
            && !c.is_alphanumeric()
            && !is_hyphen(c)
            && !matches!(c, '(' | '[' | '{' | '¿' | '¡')
    };
    seg.chars().all(closing) || seg.chars().all(is_hyphen)
}

fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{2011}')
}

/// A letter, a digit or a mark on one.
fn is_wordish(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || super::is_combining(c))
}

/// Byte `at` of `text` sits inside a hyphenated compound: a hyphen between
/// two word characters just before or just after it.
fn in_compound(text: &str, at: usize) -> bool {
    let mut back = text[..at].chars().rev();
    let mut ahead = text[at..].chars();
    let (b1, b2) = (back.next(), back.next());
    let (a1, a2) = (ahead.next(), ahead.next());
    match (b1, a1) {
        (Some(h), _) if is_hyphen(h) => is_wordish(b2) && is_wordish(a1),
        (_, Some(h)) if is_hyphen(h) => is_wordish(b1) && is_wordish(a2),
        _ => false,
    }
}

/// Byte `at` of `text` sits inside a clock time — a digit before it, a
/// colon and a digit after it — or inside a URL: before the end of a run
/// without whitespace that holds "://", its closing punctuation aside.
fn in_joined(text: &str, at: usize) -> bool {
    let mut ahead = text[at..].chars();
    let before = text[..at].chars().next_back();
    if before.is_some_and(|c| c.is_ascii_digit())
        && ahead.next() == Some(':')
        && ahead.next().is_some_and(|c| c.is_ascii_digit())
    {
        return true;
    }
    let start = text[..at].rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let end = text[at..]
        .find(char::is_whitespace)
        .map_or(text.len(), |i| at + i);
    let run = &text[start..end];
    let word_end = start + run.trim_end_matches(|c: char| is_punctuation_char(c)).len();
    run.contains("://") && at < word_end
}

/// One character of [`is_punctuation`]'s.
fn is_punctuation_char(c: char) -> bool {
    !c.is_alphanumeric() && !matches!(c, '/' | '(' | '[' | '{')
}

#[cfg(test)]
mod tests {
    use super::heard_words;

    #[test]
    fn a_clock_time_and_a_url_are_one_word() {
        let t = "um 10:30 Uhr";
        assert_eq!(at(t, 5), "um", "between the hour and its colon");
        assert_eq!(at(t, 7), "um", "inside the minutes");
        assert_eq!(at(t, 8), "um 10:30");
        // A colon that ends a word is its punctuation.
        assert_eq!(at("Achtung: 10 Uhr", 9), "Achtung:");
        let u = "Siehe https://example.com/pfad. Danach";
        assert_eq!(at(u, 14), "Siehe", "inside the scheme");
        assert_eq!(at(u, 28), "Siehe", "inside the path");
        assert_eq!(at(u, 31), "Siehe https://example.com/pfad.");
    }

    /// The heard part when the cut falls `keep` characters into `text`.
    fn at(text: &str, keep: usize) -> &str {
        heard_words(text, keep)
    }

    #[test]
    fn a_cut_inside_a_word_goes_back_to_the_word_before() {
        let t = "bestimmte Lichtwellenlängen";
        assert_eq!(at(t, 16), "bestimmte", "inside the compound");
        assert_eq!(at(t, 10), "bestimmte", "right at its start");
        assert_eq!(at(t, 9), "bestimmte", "right after the word before");
        assert_eq!(at(t, t.chars().count()), t, "heard whole");
        assert_eq!(at(t, 99), t, "past the end");
    }

    #[test]
    fn a_cut_inside_the_first_word_leaves_nothing() {
        assert_eq!(at("Lichtwellenlängen sind kurz.", 4), "");
        assert_eq!(at("Hallo", 0), "");
        assert_eq!(at("", 3), "");
    }

    #[test]
    fn punctuation_stays_with_its_word() {
        let t = "Hallo Jürgen, schön dich zu hören.";
        assert_eq!(at(t, 12), "Hallo Jürgen,", "the comma after the word");
        assert_eq!(at(t, 13), "Hallo Jürgen,");
        assert_eq!(at(t, 16), "Hallo Jürgen,", "inside schön");
        assert_eq!(
            at(t, t.chars().count() - 1),
            "Hallo Jürgen, schön dich zu hören."
        );
        // An opening bracket is not the word before's.
        assert_eq!(at("Der Wert (etwa zehn) passt.", 12), "Der Wert");
        assert_eq!(at("Der Wert(etwa", 11), "Der Wert");
        assert_eq!(
            at("Der Wert (etwa zehn) passt.", 20),
            "Der Wert (etwa zehn)"
        );
    }

    #[test]
    fn a_hyphenated_compound_is_one_word() {
        let t = "die Rayleigh-Streuung wirkt";
        assert_eq!(at(t, 15), "die", "inside the second part");
        assert_eq!(at(t, 13), "die", "right after the hyphen");
        assert_eq!(at(t, 12), "die", "right before the hyphen");
        assert_eq!(at(t, 21), "die Rayleigh-Streuung");
        // A hyphen that ends a word ("Ein- und Ausgang") is its punctuation.
        assert_eq!(at("Ein- und Ausgang", 5), "Ein-");
    }

    #[test]
    fn numbers_and_marks_stay_whole() {
        assert_eq!(at("Es sind 21,5 Grad.", 10), "Es sind", "inside 21,5");
        assert_eq!(at("Es sind 21,5 Grad.", 12), "Es sind 21,5");
        // "u" + U+0308: the mark is part of the word, never split from it.
        assert_eq!(at("u\u{308}ber alles", 1), "");
        assert_eq!(at("u\u{308}ber alles", 6), "u\u{308}ber");
    }

    #[test]
    fn han_and_hiragana_cut_per_character() {
        // Each Han ideograph is a word of its own; the full stop is the
        // last one's.
        let t = "今日は晴れです。";
        assert_eq!(at(t, 1), "今");
        assert_eq!(at(t, 2), "今日");
        assert_eq!(at(t, 8), t);
        assert_eq!(at("你好，世界。", 3), "你好，");
    }
}
