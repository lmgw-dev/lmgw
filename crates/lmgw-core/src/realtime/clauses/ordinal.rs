//! German ordinals and dates (realtime §8.1, B2 review follow-up): "am 3.
//! Oktober" is one phrase, not the sentence "am 3." and a new one — cut
//! there, a voice pauses and reads "drei" instead of "dritten".
//!
//! A dot after a number of one to three digits is **no sentence end** when
//! - a month name follows (German or English, any case: "3. Oktober",
//!   "1. Mai", "24. December");
//! - a lowercase word follows ("der 3. und letzte") — a new sentence starts
//!   with a capital, in both languages;
//! - or the word before the number is one German puts before an ordinal:
//!   am, vom, zum, zur, bis, ab, seit, der, die, das, den, dem, des, im,
//!   beim ("Die 2. Auflage", "seit 3. März").
//!
//! Otherwise the dot ends the sentence as before: "Das kostet 30. Dann …".
//! Four digits are a year ("im Jahr 2026. Dann …") and never an ordinal
//! here. The same month rule keeps "1. Mai ist Feiertag" at a line start
//! from being read as a list item (`blocks`).
//!
//! Kept whole, the clause still holds "3." — which a voice reads as "drei",
//! and Pocket TTS then went silent: the speakable pass writes a German date
//! out ("am dritten Oktober", [`spoken`], live run 2 E2) and leaves every
//! other "N." as written (fix package B6).

/// German month names, lowercase: the only ones a voice reads a date for
/// (`spoken`).
const GERMAN_MONTHS: &[&str] = &[
    "januar",
    "jänner",
    "februar",
    "märz",
    "maerz",
    "april",
    "mai",
    "juni",
    "juli",
    "august",
    "september",
    "oktober",
    "november",
    "dezember",
];

/// English month names, lowercase, those not spelt as a German one: they
/// keep a date one clause ("24. December"), and are never written out.
const ENGLISH_MONTHS: &[&str] = &[
    "january", "february", "march", "may", "june", "july", "october", "december",
];

/// Whether `lower` is a month name, German or English (module doc).
fn is_month(lower: &str) -> bool {
    GERMAN_MONTHS.contains(&lower) || ENGLISH_MONTHS.contains(&lower)
}

/// Words German puts before an ordinal (module doc).
const BEFORE: &[&str] = &[
    "am", "vom", "zum", "zur", "bis", "ab", "seit", "der", "die", "das", "den", "dem", "des", "im",
    "beim",
];

mod spoken;

pub(super) use spoken::spoken;

/// What the text after a dot says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ordinal {
    /// An ordinal or a date: no sentence end.
    Yes,
    /// The word that would tell has not arrived yet.
    Pending,
    No,
}

/// The first word of `text` after its leading spaces, whether it is whole
/// (something not a letter follows it), or `None` when nothing but spaces
/// has arrived. `whole_at_end`: the text is all there is (a complete line).
fn next_word(text: &str, whole_at_end: bool) -> Option<(&str, bool)> {
    let t = text.trim_start_matches([' ', '\t']);
    if t.is_empty() {
        return None;
    }
    let len = t
        .char_indices()
        .find(|(_, c)| !c.is_alphabetic())
        .map_or(t.len(), |(i, _)| i);
    Some((&t[..len], len < t.len() || whole_at_end))
}

/// Whether the word starting `text` is a month name: `Pending` while a
/// month could still be what it becomes.
pub(super) fn month(text: &str, whole_at_end: bool) -> Ordinal {
    let Some((word, whole)) = next_word(text, whole_at_end) else {
        return Ordinal::Pending;
    };
    if word.is_empty() {
        return Ordinal::No;
    }
    let lower = word.to_lowercase();
    if whole {
        return if is_month(&lower) {
            Ordinal::Yes
        } else {
            Ordinal::No
        };
    }
    if GERMAN_MONTHS
        .iter()
        .chain(ENGLISH_MONTHS)
        .any(|m| m.starts_with(&lower))
    {
        Ordinal::Pending
    } else {
        Ordinal::No
    }
}

/// Whether the `.` ending at byte `end` of `buf` follows a one-to-three
/// digit number that is an ordinal or a date (module doc).
pub(super) fn at_dot(buf: &str, end: usize) -> Ordinal {
    let (before, after) = buf.split_at(end);
    let number = &before[..before.len() - 1];
    let digits = number.bytes().rev().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return Ordinal::No;
    }
    let lead = &number[..number.len() - digits];
    if lead.chars().next_back().is_some_and(char::is_alphanumeric) {
        return Ordinal::No;
    }
    let prev = lead
        .trim_end()
        .rsplit(|c: char| !c.is_alphabetic())
        .next()
        .unwrap_or("")
        .to_lowercase();
    if BEFORE.contains(&prev.as_str()) {
        return Ordinal::Yes;
    }
    let Some((word, _)) = next_word(after, false) else {
        return Ordinal::Pending;
    };
    if word.chars().next().is_some_and(char::is_lowercase) {
        return Ordinal::Yes;
    }
    month(after, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `at_dot` for the first dot of `s`.
    fn judge(s: &str) -> Ordinal {
        let end = s.find('.').unwrap() + 1;
        at_dot(s, end)
    }

    #[test]
    fn dates_and_ordinals_are_no_sentence_end() {
        for s in [
            "am 3. Oktober",
            "Am 3. Oktober",
            "bis zum 24. December",
            "der 3. und letzte",
            "Die 2. Auflage",
            "beim 4. Versuch",
            "seit 12. Mai",
            "Es ist der 1. Mai",
            "3. oktober",
        ] {
            assert_eq!(judge(s), Ordinal::Yes, "{s}");
        }
    }

    #[test]
    fn real_sentence_ends_stay() {
        for s in [
            "Das kostet 30. Dann geht es weiter.",
            "im Jahr 2026. Dann",
            "Version2. Dann",
            "Es sind 3. Ich weiß",
            "Wir waren 4. Mai war auch da",
        ] {
            let want = if s.starts_with("Wir") {
                // A month word after any number: read as a date, the cost
                // of the rule — the voice reads one clause, nothing is lost.
                Ordinal::Yes
            } else {
                Ordinal::No
            };
            assert_eq!(judge(s), want, "{s}");
        }
    }

    #[test]
    fn the_next_word_decides_once_it_is_here() {
        assert_eq!(judge("Das kostet 30. "), Ordinal::Pending);
        assert_eq!(judge("Das kostet 30. Ok"), Ordinal::Pending, "Oktober?");
        assert_eq!(judge("Das kostet 30. Okt"), Ordinal::Pending);
        assert_eq!(
            judge("Das kostet 30. Oktober"),
            Ordinal::Pending,
            "or more?"
        );
        assert_eq!(judge("Das kostet 30. Oktober "), Ordinal::Yes);
        assert_eq!(
            judge("Das kostet 30. Da"),
            Ordinal::No,
            "no month starts so"
        );
        assert_eq!(month("Mai", true), Ordinal::Yes);
        assert_eq!(month("Maigrün", true), Ordinal::No);
    }
}
