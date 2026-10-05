//! Which writing systems count as words for the barge-in word check
//! (realtime design §6.4, `realtime.barge_in_check_scripts`).
//!
//! ASR models write noise in scripts nobody spoke: on the owner's own
//! recordings qwen3-asr heard a hum, a throat-clear and a sigh as "嗯。" and a
//! cough as "咳。", and nemotron heard "Mhm" as "Угу." To a German or English
//! speaker those are not words — taken as words they would cut the answer.
//! So a word whose letters are all outside the session's scripts is no
//! word, and a transcript of only such is empty: a backchannel. A token
//! with no letter at all — "2", "15:30", the "5" of "5 mm" — is a word
//! ([`counts_as_word`], B4 review M3): a "Zwei!" during playback, written
//! "2.", must still cut. An owner who speaks a language in another script
//! has to add it (or clear the list: every script counts); the session
//! warns once when its transcription language says so (`languages`).
//!
//! The table names the scripts by their Unicode names (or their ISO 15924
//! codes) and covers the blocks each one's letters live in — the scripts an
//! ASR model writes, not all of Unicode. A name it does not know is refused
//! in a client's `session.update`, and warned about at session start when
//! the owner's setting has it ([`unknown`]): it matches no letter.

mod languages;

pub use languages::{iso639_1, scripts_of};

/// The scripts this table knows, by their Unicode names, with the ranges of
/// their letters.
const SCRIPTS: &[(&str, &[(u32, u32)])] = &[
    (
        "Latin",
        &[
            (0x41, 0x5A),
            (0x61, 0x7A),
            (0xAA, 0xAA),
            (0xBA, 0xBA),
            (0xC0, 0xD6),
            (0xD8, 0xF6),
            (0xF8, 0x24F),
            (0x250, 0x2AF),
            (0x1D00, 0x1D7F),
            (0x1E00, 0x1EFF),
            (0x2C60, 0x2C7F),
            (0xA720, 0xA7FF),
            (0xAB30, 0xAB6F),
            (0xFB00, 0xFB06),
            (0xFF21, 0xFF3A),
            (0xFF41, 0xFF5A),
        ],
    ),
    ("Greek", &[(0x370, 0x3FF), (0x1F00, 0x1FFF)]),
    (
        "Cyrillic",
        &[
            (0x400, 0x52F),
            (0x1C80, 0x1C8F),
            (0x2DE0, 0x2DFF),
            (0xA640, 0xA69F),
        ],
    ),
    ("Armenian", &[(0x531, 0x58F)]),
    ("Hebrew", &[(0x591, 0x5FF), (0xFB1D, 0xFB4F)]),
    (
        "Arabic",
        &[
            (0x600, 0x6FF),
            (0x750, 0x77F),
            (0x8A0, 0x8FF),
            (0xFB50, 0xFDFF),
            (0xFE70, 0xFEFF),
        ],
    ),
    ("Devanagari", &[(0x900, 0x97F), (0xA8E0, 0xA8FF)]),
    ("Bengali", &[(0x980, 0x9FF)]),
    ("Thai", &[(0xE00, 0xE7F)]),
    (
        "Georgian",
        &[(0x10A0, 0x10FF), (0x1C90, 0x1CBF), (0x2D00, 0x2D2F)],
    ),
    (
        "Hangul",
        &[
            (0x1100, 0x11FF),
            (0x3130, 0x318F),
            (0xA960, 0xA97F),
            (0xAC00, 0xD7FF),
        ],
    ),
    ("Hiragana", &[(0x3040, 0x309F)]),
    (
        "Katakana",
        &[(0x30A0, 0x30FF), (0x31F0, 0x31FF), (0xFF66, 0xFF9F)],
    ),
    (
        "Han",
        &[
            (0x2E80, 0x2FDF),
            (0x3005, 0x3007),
            (0x3021, 0x3029),
            (0x3038, 0x303B),
            (0x3400, 0x4DBF),
            (0x4E00, 0x9FFF),
            (0xF900, 0xFAFF),
            (0x20000, 0x323AF),
        ],
    ),
];

/// The names [`SCRIPTS`] knows, for an error that lists them.
pub fn known() -> Vec<&'static str> {
    SCRIPTS.iter().map(|(name, _)| *name).collect()
}

/// The first of `names` the table does not know (case is ignored).
pub fn unknown(names: &[String]) -> Option<&str> {
    names
        .iter()
        .find(|n| ranges(n).is_none())
        .map(String::as_str)
}

/// Each script's ISO 15924 code, in [`SCRIPTS`]' order.
const CODES: &[&str] = &[
    "Latn", "Grek", "Cyrl", "Armn", "Hebr", "Arab", "Deva", "Beng", "Thai", "Geor", "Hang", "Hira",
    "Kana", "Hani",
];

fn ranges(name: &str) -> Option<&'static [(u32, u32)]> {
    let name = name.trim();
    SCRIPTS
        .iter()
        .zip(CODES)
        .find(|((n, _), code)| n.eq_ignore_ascii_case(name) || code.eq_ignore_ascii_case(name))
        .map(|((_, r), _)| *r)
}

/// Whether `token` counts as a word for the word check (module doc): one
/// without letters always does, one with letters when a letter of it is in
/// `scripts`.
pub fn counts_as_word(token: &str, scripts: &[String]) -> bool {
    !token.chars().any(char::is_alphabetic) || has_letter_in(token, scripts)
}

/// Whether `scripts` covers `script` (a name or a code): the empty list
/// covers every one.
pub fn covers(scripts: &[String], script: &str) -> bool {
    let Some(want) = ranges(script) else {
        return false;
    };
    scripts.is_empty() || scripts.iter().any(|s| ranges(s) == Some(want))
}

/// Whether `word` has a letter in one of `scripts` — always, for an empty
/// list (every script counts). An unknown name matches nothing.
pub fn has_letter_in(word: &str, scripts: &[String]) -> bool {
    if scripts.is_empty() {
        return true;
    }
    let tables: Vec<&[(u32, u32)]> = scripts.iter().filter_map(|s| ranges(s)).collect();
    word.chars().filter(|c| c.is_alphabetic()).any(|c| {
        let c = u32::from(c);
        tables
            .iter()
            .any(|t| t.iter().any(|&(lo, hi)| (lo..=hi).contains(&c)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin() -> Vec<String> {
        vec!["Latin".into()]
    }

    #[test]
    fn asr_noise_in_other_scripts_has_no_latin_letter() {
        for noise in ["嗯", "咳", "угу", "ああ", "음"] {
            assert!(!has_letter_in(noise, &latin()), "{noise}");
            assert!(has_letter_in(noise, &[]), "{noise}: every script counts");
        }
        for word in ["stopp", "über", "ça", "ﬁ"] {
            assert!(has_letter_in(word, &latin()), "{word}");
        }
        // M3: a token without letters is no letter of any script — and a
        // word all the same, so "2." or "15:30" cut.
        assert!(!has_letter_in("42", &latin()), "no letter at all");
        assert!(counts_as_word("42", &latin()));
        assert!(counts_as_word("15", &latin()) && counts_as_word("stopp", &latin()));
        assert!(!counts_as_word("嗯", &latin()) && counts_as_word("嗯", &[]));
        assert!(has_letter_in("угу", &["cyrillic".into()]), "case ignored");
        assert!(has_letter_in("嗯", &["Latin".into(), "Han".into()]));
        assert!(has_letter_in("嗯", &["hani".into()]), "an ISO 15924 code");
        assert_eq!(CODES.len(), SCRIPTS.len());
    }

    #[test]
    fn a_list_covers_a_script_by_name_or_code() {
        assert!(covers(&latin(), "Latin") && covers(&["latn".into()], "Latin"));
        assert!(!covers(&latin(), "Cyrillic"));
        assert!(covers(&[], "Cyrillic"), "the empty list covers every one");
        assert!(!covers(&latin(), "Klingon"));
    }

    #[test]
    fn unknown_names_are_found() {
        assert_eq!(unknown(&latin()), None);
        assert_eq!(
            unknown(&["Latin".into(), "Klingon".into()]),
            Some("Klingon")
        );
        assert!(known().contains(&"Cyrillic"));
    }
}
