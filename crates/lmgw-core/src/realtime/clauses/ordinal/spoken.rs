//! German dates as a voice should read them (realtime §8.1, live run 2 E2,
//! narrowed in fix package B6): the clause splitter keeps "am 3. Oktober"
//! one clause (`super`), but Pocket TTS, handed "3. Oktober", said "drei" and
//! then nothing for ~2.7 s — in 5 of 8 probes — while "3 Oktober" and "der
//! dritte Oktober" came out clean 8 of 8.
//!
//! So the speakable pass writes a **date** out: a day 1 to 31 with its dot,
//! followed by a German month name. Months are masculine, so the word before
//! the day settles its ending:
//! - after "der": "-te" ("der dritte Oktober");
//! - after den, dem, am, vom, zum, im, beim or des, or a bare preposition
//!   ([`PREPOSITIONS`]: "bis dritten Oktober"): "-ten";
//! - at the start of the clause: "-ter", with a capital ("Dritter Oktober
//!   ist …");
//! - after und, oder, bis, a comma or a dash that follows another date: that
//!   date's ending ("am 3. und 4. Oktober" → "am dritten und vierten
//!   Oktober"). A day joined so to a date is a date itself, its month named
//!   once at the end;
//! - after any other word: as written.
//!
//! Every other "N." keeps its number and its dot exactly as written: "in der
//! 3. Klasse", "der 1. und 2. Platz", "ab 18.", "bis 17. Danach". An
//! ordinal's ending follows its noun's gender and case, which no text pass
//! knows (B5 review: "in der dritte Klasse", "ab achtzehnten."); a date's
//! noun is always a month. An English month never counts ("Step 3. May I
//! …"), and a number at the clause end is never a date ("I am 3."). The
//! spoken transcript says what was spoken.

use super::{next_word, GERMAN_MONTHS};

/// The stems of the German ordinals 1 to 31 ("dritt" + "e"/"en"/"er").
const STEMS: [&str; 31] = [
    "erst",
    "zweit",
    "dritt",
    "viert",
    "fünft",
    "sechst",
    "siebt",
    "acht",
    "neunt",
    "zehnt",
    "elft",
    "zwölft",
    "dreizehnt",
    "vierzehnt",
    "fünfzehnt",
    "sechzehnt",
    "siebzehnt",
    "achtzehnt",
    "neunzehnt",
    "zwanzigst",
    "einundzwanzigst",
    "zweiundzwanzigst",
    "dreiundzwanzigst",
    "vierundzwanzigst",
    "fünfundzwanzigst",
    "sechsundzwanzigst",
    "siebenundzwanzigst",
    "achtundzwanzigst",
    "neunundzwanzigst",
    "dreißigst",
    "einunddreißigst",
];

/// Articles and contractions a date takes "-ten" after ("am dritten Mai").
const TEN: &[&str] = &["den", "dem", "am", "vom", "zum", "im", "beim", "des"];

/// Prepositions a date follows without an article ("bis dritten Mai"):
/// "-ten" as well.
const PREPOSITIONS: &[&str] = &["bis", "ab", "seit", "vor", "nach", "für", "gegen"];

/// Words that join a date to the one before ("3. und 4. Mai"). "bis" is
/// both: a join after a date, a bare preposition otherwise.
const JOINS: &[&str] = &["und", "oder", "bis", "sowie"];

/// Marks that join a date to the one before ("1., 2. und 3. Mai",
/// "3.–5. Mai").
const JOIN_MARKS: &[&str] = &[",", "-", "–", "—"];

/// A date's ending (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// "der dritte".
    Te,
    /// "am dritten".
    Ten,
    /// "Dritter Oktober" at the clause start.
    Ter,
}

impl Ending {
    fn suffix(self) -> &'static str {
        match self {
            Self::Te => "e",
            Self::Ten => "en",
            Self::Ter => "er",
        }
    }
}

/// A number with its dot in the clause: `from..dot` are its digits.
struct Number {
    from: usize,
    dot: usize,
}

/// `text` — a whole clause — with every German date said as the voice
/// should (module doc), everything else as written.
pub(in crate::realtime::clauses) fn spoken(text: &str) -> String {
    let numbers = numbers(text);
    // Which numbers are dates, from the last: a German month follows, or a
    // join and another date.
    let mut dated = vec![false; numbers.len()];
    for k in (0..numbers.len()).rev() {
        let after = &text[numbers[k].dot + 1..];
        dated[k] = german_month(after)
            || numbers
                .get(k + 1)
                .is_some_and(|next| dated[k + 1] && joined(&text[numbers[k].dot + 1..next.from]));
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut copied = 0;
    // The ending of the date before, in this clause.
    let mut last = None;
    for (number, _) in numbers.iter().zip(&dated).filter(|(_, d)| **d) {
        let lead = &text[..number.from];
        let at_start = !lead.chars().any(char::is_alphanumeric);
        let ending = if at_start {
            Some(Ending::Ter)
        } else {
            ending(lead, last)
        };
        last = ending;
        let Some(word) = ending.and_then(|e| written(&text[number.from..number.dot], e, at_start))
        else {
            continue;
        };
        out.push_str(&text[copied..number.from]);
        out.push_str(&word);
        copied = number.dot + 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// Every number of one to three digits with a dot after it in `text`: not
/// part of a word ("Version2."), no digit after the dot ("3.14",
/// "3.10.2026").
fn numbers(text: &str) -> Vec<Number> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() || (i > 0 && is_word_byte(text, i)) {
            i += 1;
            continue;
        }
        let digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        let dot = i + digits;
        if digits <= 3
            && bytes.get(dot) == Some(&b'.')
            && !bytes.get(dot + 1).is_some_and(u8::is_ascii_digit)
        {
            found.push(Number { from: i, dot });
        }
        i = dot + 1;
    }
    found
}

/// Whether the character before byte `i` of `text` is a letter or a digit:
/// the digits at `i` are no number of their own then ("Version2.").
fn is_word_byte(text: &str, i: usize) -> bool {
    text[..i]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
}

/// Whether the word starting `after` is a German month name.
fn german_month(after: &str) -> bool {
    next_word(after, true)
        .is_some_and(|(w, whole)| whole && GERMAN_MONTHS.contains(&w.to_lowercase().as_str()))
}

/// Whether `gap`, the text between one number's dot and the next number,
/// joins them as two dates: a join word or mark, and at most one word of
/// the next date's own ("vom 1. bis zum 24. Dezember").
fn joined(gap: &str) -> bool {
    let mut words = gap.split_whitespace().map(str::to_lowercase);
    let Some(join) = words.next() else {
        return false;
    };
    if !JOINS.contains(&join.as_str()) && !JOIN_MARKS.contains(&join.as_str()) {
        return false;
    }
    match (words.next(), words.next()) {
        (None, _) => true,
        (Some(own), None) => own == "der" || TEN.contains(&own.as_str()),
        _ => false,
    }
}

/// The ending of a date after `lead` — the clause before it, not empty of
/// words — where `last` is the ending of the date before it (module doc).
fn ending(lead: &str, last: Option<Ending>) -> Option<Ending> {
    let lead = lead.trim_end();
    let c = lead.chars().next_back()?;
    if JOIN_MARKS.iter().any(|m| m.starts_with(c)) {
        return last;
    }
    if !c.is_alphabetic() {
        return None;
    }
    let word = lead
        .rsplit(|c: char| !c.is_alphabetic())
        .next()
        .unwrap_or("")
        .to_lowercase();
    match word.as_str() {
        "der" => Some(Ending::Te),
        "bis" => last.or(Some(Ending::Ten)),
        w if JOINS.contains(&w) => last,
        w if TEN.contains(&w) || PREPOSITIONS.contains(&w) => Some(Ending::Ten),
        _ => None,
    }
}

/// The day `digits` written out with `ending`, a capital at the clause
/// start; `None` past 31.
fn written(digits: &str, ending: Ending, capital: bool) -> Option<String> {
    let n: usize = digits.parse().ok()?;
    let stem = STEMS.get(n.checked_sub(1)?)?;
    let word = format!("{stem}{}", ending.suffix());
    if !capital {
        return Some(word);
    }
    let mut chars = word.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_german_date_is_written_out_with_its_ending() {
        for (text, want) in [
            // E2's clause, live: the voice said "drei" and went silent.
            (
                "3. Oktober ist ein Feiertag.",
                "Dritter Oktober ist ein Feiertag.",
            ),
            ("1. Mai ist Feiertag", "Erster Mai ist Feiertag"),
            (
                "Wir treffen uns am 3. Oktober.",
                "Wir treffen uns am dritten Oktober.",
            ),
            // der: "-te".
            ("Der 3. Oktober ist frei.", "Der dritte Oktober ist frei."),
            ("Es ist der 1. Mai", "Es ist der erste Mai"),
            // den/dem/am/vom/zum/im/beim/des and bare prepositions: "-ten".
            ("bis 3. Oktober", "bis dritten Oktober"),
            ("seit 12. März", "seit zwölften März"),
            ("ab dem 31. Mai", "ab dem einunddreißigsten Mai"),
            ("den 20. Juni", "den zwanzigsten Juni"),
            ("Am 3. oktober", "Am dritten oktober"),
            ("Leading zero am 03. Mai", "Leading zero am dritten Mai"),
            // Two dates: the second reuses the first's ending, unless it
            // has a word of its own.
            ("am 3. und 4. Oktober", "am dritten und vierten Oktober"),
            (
                "Der 3. und 4. Oktober sind frei.",
                "Der dritte und vierte Oktober sind frei.",
            ),
            (
                "3. oder 4. Oktober passt.",
                "Dritter oder vierter Oktober passt.",
            ),
            ("der 1. bis 3. Mai", "der erste bis dritte Mai"),
            ("vom 1. bis 3. Mai", "vom ersten bis dritten Mai"),
            (
                "vom 1. bis zum 24. Dezember",
                "vom ersten bis zum vierundzwanzigsten Dezember",
            ),
            (
                "am 1., 2. und 3. Oktober",
                "am ersten, zweiten und dritten Oktober",
            ),
            ("vom 3.–5. Oktober", "vom dritten–fünften Oktober"),
            (
                "Am 3. Oktober und 4. November",
                "Am dritten Oktober und vierten November",
            ),
        ] {
            assert_eq!(spoken(text), want, "{text}");
        }
    }

    #[test]
    fn every_other_number_keeps_its_number_and_dot() {
        for text in [
            // B5 review: no month, so no case to guess.
            "in der 3. Klasse",
            "der 1. und 2. Platz",
            "die 3. Version",
            "Die 2. Auflage ist da.",
            "Ende der 1. Woche",
            "das 1. Mal",
            "zur 2. Auflage",
            "im 5. Stock",
            "beim 4. Versuch",
            "des 7. Tages",
            "Die 100. Auflage",
            "Pick 1. or 2. today",
            // No trigger at a clause end, nor before a capital that is no
            // month.
            "Der Film ist ab 18.",
            "ab 18.",
            "I am 3.",
            "Er wurde der 3.",
            "Seite 3. Dann",
            "bis 17. Danach",
            "von 9 bis 17. Danach",
            // English months never count.
            "Step 3. May I help?",
            "Bis 24. December.",
            // A month, but no day or no word that settles the ending.
            "am 32. Mai",
            "am 0. Mai",
            "Heute ist 3. Oktober",
            "Montag, 3. Oktober",
            "Montag 3. und 4. Oktober",
            // No ordinal: numbers, decimals, dates in digits, sentence ends.
            "Das kostet 30. Dann geht es los.",
            "Pi ist 3.14 am 3.10.2026.",
            "Im Jahr 2026. war",
            "Version2. Mai",
            "Es sind 3.",
            "Um 15.30 Uhr",
            "ohne Zahl",
        ] {
            assert_eq!(spoken(text), text, "{text}");
        }
    }
}
