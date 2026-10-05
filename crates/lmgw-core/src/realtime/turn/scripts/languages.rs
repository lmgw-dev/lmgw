//! Which scripts a transcription language is written in (realtime design
//! §6.4, B4 review): a session whose `audio.input.transcription.language`
//! is written in a script `barge_in_check_scripts` leaves out hears none of
//! the user's words during an answer — every one is "no word" — so the
//! session warns once (`input::word_check`).
//!
//! Only languages whose script is certain are listed, by their ISO 639-1
//! code (a region or script subtag after `-` or `_` is ignored): an unknown
//! code says nothing, rather than a guess that warns wrongly. The same parse
//! picks the code the session's ASR calls are told ([`iso639_1`]).

/// The scripts `language` is written in, by [`super::SCRIPTS`] name;
/// `None` for a code this table does not know.
pub fn scripts_of(language: &str) -> Option<&'static [&'static str]> {
    let scripts: &[&str] = match primary(language).as_str() {
        "af" | "az" | "bs" | "ca" | "cs" | "cy" | "da" | "de" | "en" | "es" | "et" | "eu"
        | "fi" | "fr" | "ga" | "gl" | "hr" | "hu" | "id" | "is" | "it" | "lb" | "lt" | "lv"
        | "ms" | "mt" | "nb" | "nl" | "nn" | "no" | "pl" | "pt" | "ro" | "sk" | "sl" | "sq"
        | "sv" | "sw" | "tl" | "tr" | "uz" | "vi" => &["Latin"],
        "be" | "bg" | "kk" | "ky" | "mk" | "mn" | "ru" | "sr" | "tg" | "uk" => &["Cyrillic"],
        "el" => &["Greek"],
        "hy" => &["Armenian"],
        "he" | "iw" | "yi" => &["Hebrew"],
        "ar" | "fa" | "ps" | "ur" => &["Arabic"],
        "hi" | "mr" | "ne" => &["Devanagari"],
        "bn" => &["Bengali"],
        "th" => &["Thai"],
        "ka" => &["Georgian"],
        "ko" => &["Hangul"],
        "zh" | "yue" => &["Han"],
        "ja" => &["Hiragana", "Katakana", "Han"],
        _ => return None,
    };
    Some(scripts)
}

/// The primary subtag of `language`, lowercased: what comes before a
/// region or script subtag after `-` or `_` ("de" for "de-DE", "pt" for
/// " PT_br ").
fn primary(language: &str) -> String {
    language
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The ISO 639-1 code `language` names: its primary subtag ([`primary`])
/// when that is two ASCII letters, else `None` — "german" and "deu" name
/// no code an ASR call is told (fix package B6, realtime design §5.2).
pub fn iso639_1(language: &str) -> Option<String> {
    let code = primary(language);
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase())).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::super::{covers, known};
    use super::*;

    #[test]
    fn languages_name_their_scripts_and_unknown_ones_say_nothing() {
        assert_eq!(scripts_of("de"), Some(&["Latin"][..]));
        assert_eq!(scripts_of(" RU "), Some(&["Cyrillic"][..]));
        assert_eq!(scripts_of("zh-CN"), Some(&["Han"][..]));
        assert_eq!(scripts_of("pt_BR"), Some(&["Latin"][..]));
        assert_eq!(scripts_of("ja").map(<[_]>::len), Some(3));
        assert_eq!(scripts_of("ta"), None, "Tamil is not in the table");
        assert_eq!(scripts_of(""), None);
        // Every script named is one the table knows.
        for code in [
            "de", "ru", "el", "hy", "he", "ar", "hi", "bn", "th", "ka", "ko", "zh", "ja",
        ] {
            for s in scripts_of(code).unwrap() {
                assert!(known().contains(s), "{s}");
            }
        }
        let latin = vec!["Latin".to_string()];
        assert!(!covers(&latin, scripts_of("ru").unwrap()[0]));
    }

    #[test]
    fn only_a_two_letter_primary_subtag_is_a_language_code() {
        for (language, code) in [
            ("de", Some("de")),
            ("de-DE", Some("de")),
            (" DE ", Some("de")),
            ("pt_BR", Some("pt")),
            ("zh-Hans-CN", Some("zh")),
            ("german", None),
            ("deu", None),
            ("yue", None),
            ("d", None),
            ("", None),
            ("-DE", None),
            ("d1", None),
            ("dé", None),
        ] {
            assert_eq!(iso639_1(language).as_deref(), code, "{language:?}");
        }
    }
}
