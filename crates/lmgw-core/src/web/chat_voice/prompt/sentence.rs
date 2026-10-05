//! The language sentence of a turn's system message (chat-voice design
//! §8.5, split 2026-10-05): what the model is told about the two languages
//! — the reply's, which it answers in and the voice speaks, and the one the
//! user speaks, when that is set. With both the same (or only the spoken
//! one set, which the reply then follows) it is the sentence from before
//! the split, word for word.
//!
//! | spoken | reply heard (`spoken: true`) | reply read |
//! |---|---|---|
//! | = reply | The user speaks R and hears your reply in a R voice, so answer in R … | The user speaks R, so answer in R … |
//! | S ≠ reply | The user speaks S and hears your reply in a R voice, so answer in R … | The user speaks S, but answer in R … |
//! | none | The user hears your reply in a R voice, so answer in R … | Answer in R … |
//!
//! Each ends "unless the user asks for another language."; "a" is "an"
//! before a name that starts with a vowel sound ("an English voice").

/// The sentence for a reply in `reply` from a user who speaks `speaks`
/// (module doc); `heard` when the reply is spoken aloud. Languages go by
/// their English names, a code lmgw has none for as the code itself.
pub(crate) fn language_sentence(speaks: Option<&str>, reply: &str, heard: bool) -> String {
    let r = crate::audio::language::display_name(reply);
    let speaks = speaks
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(crate::audio::language::display_name);
    const TAIL: &str = "unless the user asks for another language.";
    match (speaks, heard) {
        (Some(s), true) => format!(
            "The user speaks {s} and hears your reply in {} {r} voice, so answer in {r} {TAIL}",
            article(&r)
        ),
        (None, true) => format!(
            "The user hears your reply in {} {r} voice, so answer in {r} {TAIL}",
            article(&r)
        ),
        (Some(s), false) if s == r => format!("The user speaks {s}, so answer in {r} {TAIL}"),
        (Some(s), false) => format!("The user speaks {s}, but answer in {r} {TAIL}"),
        (None, false) => format!("Answer in {r} {TAIL}"),
    }
}

/// "an" before a language name that starts with a vowel sound, else "a".
/// Only a name counts: a bare code (lowercase, lmgw has no name for it)
/// keeps "a", as before.
fn article(name: &str) -> &'static str {
    let vowel =
        name.starts_with(['A', 'E', 'I', 'O']) || ["Ur", "Uz"].iter().any(|p| name.starts_with(p));
    if vowel {
        "an"
    } else {
        "a"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAIL: &str = "unless the user asks for another language.";

    #[test]
    fn one_language_reads_as_before_the_split() {
        assert_eq!(
            language_sentence(Some("de"), "de", true),
            format!("The user speaks German and hears your reply in a German voice, so answer in German {TAIL}")
        );
        assert_eq!(
            language_sentence(Some("de"), "de", false),
            format!("The user speaks German, so answer in German {TAIL}")
        );
    }

    #[test]
    fn two_languages_say_what_the_user_speaks_and_what_to_answer_in() {
        assert_eq!(
            language_sentence(Some("de"), "en", true),
            format!("The user speaks German and hears your reply in an English voice, so answer in English {TAIL}")
        );
        assert_eq!(
            language_sentence(Some("de"), "en", false),
            format!("The user speaks German, but answer in English {TAIL}")
        );
    }

    #[test]
    fn a_reply_language_alone_claims_nothing_about_the_user() {
        assert_eq!(
            language_sentence(None, "en", true),
            format!("The user hears your reply in an English voice, so answer in English {TAIL}")
        );
        assert_eq!(
            language_sentence(Some(" "), "en", false),
            format!("Answer in English {TAIL}")
        );
    }

    #[test]
    fn the_article_follows_the_name() {
        for (name, a) in [
            ("English", "an"),
            ("Italian", "an"),
            ("Urdu", "an"),
            ("Ukrainian", "a"),
            ("German", "a"),
            ("sw", "a"),
        ] {
            assert_eq!(article(name), a, "{name}");
        }
    }
}
