//! Language naming: a client's ISO 639-1 / BCP-47 code in the spelling a
//! family's engine takes (audio-class gap 7).
//!
//! Qwen3-TTS looks the lowercased language up in its `codec_language_id`
//! table, whose keys are names — `german`, `english` — so `de` is refused
//! (`qwen3_tts/talker.cpp`); its spec's `languages` lists ISO codes, which
//! describe the model, not its request. Kokoro takes `en-us`, `pt-br`. The
//! vocabulary comes from the [`super::profile`] (the package's own table,
//! the spec's `language` enum, the spec's `languages` when they are names,
//! or a family the [`super::families`] table lists), and the mapping is:
//! 1. an exact entry, without case, is sent in the vocabulary's spelling;
//! 2. an ISO 639-1 code (or a BCP-47 tag's primary subtag) whose English
//!    name is an entry — `de`, `de-DE` → `german`;
//! 3. a BCP-47 tag whose region form or primary subtag is an entry, then
//!    the family's preferred region for a bare primary (`en` → `en-us`),
//!    then the one entry with that primary subtag;
//! 4. anything else is sent as it came — the engine is the judge.
//!
//! A language lmgw only *infers* — a realtime session's transcription
//! language — is a hint, not a request ([`hint_language`]): sent only where
//! it maps into a vocabulary the engine declares, or into the spec's
//! languages of a family with a `language` request option, and never to a
//! family whose table lmgw supplies (Kokoro derives the language from its
//! voice and refuses a mismatch; Supertonic's request codes are lmgw's
//! reading of its source, and a session's transcription language says what
//! the user speaks, not what the reply is in). A family that takes exactly
//! its spec's languages and declares a `language` option (FireRedTTS3,
//! MagpieTTS) gets the hint as it did before lmgw knew that. Elsewhere the
//! engine's default stands, as before: a stock session sends what it sent
//! before the conversation language existed.
//!
//! The owner's own languages — the Chat's language settings (the spoken one
//! for speech recognition, the reply's for the voice), a thread's
//! overrides — are a request ([`configured`]): sent wherever the row
//! takes it, in its spelling, and where a row cannot take it, not sent and
//! said ([`Fit::note`]).

use super::families;
use super::profile::{LanguageVocab, SpeechProfile, VocabSource};

mod configured;
pub use configured::{asr_fit, tts_fit, Field, Fit, SpeechLanguage};

/// ISO 639-1 (and the few three-letter primary subtags in use) → the
/// English language name, lowercase — Qwen3's spelling.
const NAMES: &[(&str, &str)] = &[
    ("ar", "arabic"),
    ("bg", "bulgarian"),
    ("ca", "catalan"),
    ("cs", "czech"),
    ("da", "danish"),
    ("de", "german"),
    ("el", "greek"),
    ("en", "english"),
    ("es", "spanish"),
    ("et", "estonian"),
    ("fa", "persian"),
    ("fi", "finnish"),
    ("fr", "french"),
    ("he", "hebrew"),
    ("hi", "hindi"),
    ("hr", "croatian"),
    ("hu", "hungarian"),
    ("id", "indonesian"),
    ("it", "italian"),
    ("ja", "japanese"),
    ("ko", "korean"),
    ("lt", "lithuanian"),
    ("lv", "latvian"),
    ("ms", "malay"),
    ("nb", "norwegian"),
    ("nl", "dutch"),
    ("no", "norwegian"),
    ("pl", "polish"),
    ("pt", "portuguese"),
    ("ro", "romanian"),
    ("ru", "russian"),
    ("sk", "slovak"),
    ("sl", "slovenian"),
    ("sv", "swedish"),
    ("th", "thai"),
    ("tr", "turkish"),
    ("uk", "ukrainian"),
    ("vi", "vietnamese"),
    ("yue", "cantonese"),
    ("zh", "chinese"),
];

/// The English name of `code`'s language, lowercase (`de`, `de-AT` →
/// `german`); `None` for a code [`NAMES`] lacks.
pub fn english_name(code: &str) -> Option<&'static str> {
    let (primary, _) = parts(code);
    NAMES
        .iter()
        .find(|(c, _)| *c == primary)
        .map(|(_, name)| *name)
}

/// The ISO 639-1 code of a language's English name, lowercase (`german`
/// → `de`); `None` for a name [`NAMES`] lacks.
pub fn code_of_name(name: &str) -> Option<&'static str> {
    NAMES
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(code, _)| *code)
}

/// `code`'s language for a sentence: its English name, capitalised
/// (`German`), or the code as written when lmgw has no name for it.
pub fn display_name(code: &str) -> String {
    match english_name(code) {
        Some(name) => {
            let mut chars = name.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        }
        None => code.trim().to_string(),
    }
}

/// `(primary subtag, normalised tag)`: lowercase, `_` read as `-`.
fn parts(tag: &str) -> (String, String) {
    let norm = tag.trim().to_ascii_lowercase().replace('_', "-");
    let primary = norm.split('-').next().unwrap_or_default().to_string();
    (primary, norm)
}

/// The entry of `entries` that `requested` names, by rules 1–3 of the
/// module doc; `None` when none does.
fn in_vocab(requested: &str, family: &str, entries: &[String]) -> Option<String> {
    let requested = requested.trim();
    if requested.is_empty() || entries.is_empty() {
        return None;
    }
    let find = |want: &str| {
        entries
            .iter()
            .find(|e| e.eq_ignore_ascii_case(want) || parts(e).1 == want)
            .cloned()
    };
    let (primary, norm) = parts(requested);
    // 1. Exact (case and `_`/`-` aside).
    if let Some(e) = find(&norm) {
        return Some(e);
    }
    // 2. The code's English name.
    if let Some((_, name)) = NAMES.iter().find(|(code, _)| *code == primary) {
        if let Some(e) = find(name) {
            return Some(e);
        }
    }
    // 3. The primary subtag, the family's region for it, the one entry
    //    with it.
    if norm != primary {
        if let Some(e) = find(&primary) {
            return Some(e);
        }
    }
    if let Some(region) = families::language_regions(family)
        .and_then(|t| t.iter().find(|(p, _)| *p == primary))
        .map(|(_, r)| *r)
    {
        if let Some(e) = find(region) {
            return Some(e);
        }
    }
    let mut same = entries.iter().filter(|e| parts(e).0 == primary);
    match (same.next(), same.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// A client's `language` for `family`'s vocabulary: `Some(spelling)` when
/// it maps to a different spelling, `None` to send it as it came (an exact
/// entry, or nothing it maps to — rule 4).
pub fn map_language(requested: &str, family: &str, vocab: &LanguageVocab) -> Option<String> {
    in_vocab(requested, family, &vocab.entries).filter(|e| e != requested)
}

/// A language lmgw infers (a realtime session's) for a TTS row: the value to
/// send, or `None` to send none (module doc).
pub fn hint_language(hint: &str, profile: &SpeechProfile) -> Option<String> {
    match profile.language_vocab.source {
        VocabSource::Gguf | VocabSource::SpecEnum | VocabSource::SpecNames => {
            in_vocab(hint, &profile.family, &profile.language_vocab.entries)
        }
        // A family that takes exactly its spec's languages gets a hint where
        // it got one before it was known to: through its `language` option
        // (FireRedTTS3, MagpieTTS — module doc).
        VocabSource::SpecRequest if profile.language_option => {
            in_vocab(hint, &profile.family, &profile.language_vocab.entries)
        }
        // lmgw's own tables: Kokoro's voice decides, Supertonic's codes are a
        // request vocabulary (module doc).
        VocabSource::FamilyTable | VocabSource::SpecRequest => None,
        VocabSource::None if profile.language_option => {
            in_vocab(hint, &profile.family, &profile.spec_languages)
        }
        VocabSource::None => None,
    }
}

/// The ISO 639-1 codes (or the primary subtags in use) a row speaks, for
/// `capabilities.speech.languages`: its vocabulary's entries — a name by
/// its code, a code or BCP-47 tag by its primary subtag — else the spec's
/// `languages` when they are codes. `auto` and names lmgw has no code for
/// are left out. Sorted, without duplicates.
pub fn iso_languages(profile: &SpeechProfile) -> Vec<String> {
    let code = |entry: &str| -> Option<String> {
        let (primary, norm) = parts(entry);
        if let Some((c, _)) = NAMES.iter().find(|(_, name)| *name == norm) {
            return Some(c.to_string());
        }
        let is_code =
            (2..=3).contains(&primary.len()) && primary.chars().all(|c| c.is_ascii_lowercase());
        is_code.then_some(primary)
    };
    let source: &[String] = if profile.language_vocab.entries.is_empty() {
        &profile.spec_languages
    } else {
        &profile.language_vocab.entries
    };
    let mut out: Vec<String> = source.iter().filter_map(|e| code(e)).collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_published_as_iso_codes() {
        let qwen3 = SpeechProfile {
            language_vocab: vocab(VocabSource::Gguf, &["english", "german", "auto"]),
            ..Default::default()
        };
        assert_eq!(iso_languages(&qwen3), ["de", "en"]);
        let kokoro = SpeechProfile {
            language_vocab: vocab(VocabSource::FamilyTable, &["en-us", "en-gb", "pt-br"]),
            ..Default::default()
        };
        assert_eq!(iso_languages(&kokoro), ["en", "pt"]);
        let magpie = SpeechProfile {
            spec_languages: vec!["de".into(), "en".into(), "es".into()],
            ..Default::default()
        };
        assert_eq!(iso_languages(&magpie), ["de", "en", "es"]);
    }

    fn vocab(source: VocabSource, entries: &[&str]) -> LanguageVocab {
        LanguageVocab {
            source,
            entries: entries.iter().map(|e| e.to_string()).collect(),
        }
    }

    #[test]
    fn the_mapping_table() {
        let qwen3 = vocab(VocabSource::Gguf, &["chinese", "english", "german", "auto"]);
        let kokoro = vocab(
            VocabSource::FamilyTable,
            &["en-us", "en-gb", "es", "fr-fr", "pt-br", "zh"],
        );
        let none = LanguageVocab::default();
        for (asked, family, v, sent) in [
            ("de", "qwen3_tts", &qwen3, Some("german")),
            ("de-DE", "qwen3_tts", &qwen3, Some("german")),
            ("German", "qwen3_tts", &qwen3, Some("german")),
            ("german", "qwen3_tts", &qwen3, None),
            ("Auto", "qwen3_tts", &qwen3, Some("auto")),
            ("zh-TW", "qwen3_tts", &qwen3, Some("chinese")),
            ("ja", "qwen3_tts", &qwen3, None),
            ("en", "kokoro_tts", &kokoro, Some("en-us")),
            ("en-GB", "kokoro_tts", &kokoro, Some("en-gb")),
            ("en_gb", "kokoro_tts", &kokoro, Some("en-gb")),
            ("pt", "kokoro_tts", &kokoro, Some("pt-br")),
            ("es-ES", "kokoro_tts", &kokoro, Some("es")),
            ("de", "kokoro_tts", &kokoro, None),
            ("de", "magpie_tts", &none, None),
            ("de", "fish_audio", &none, None),
        ] {
            assert_eq!(
                map_language(asked, family, v).as_deref(),
                sent,
                "{asked} for {family}"
            );
        }
    }

    #[test]
    fn an_inferred_language_is_sent_only_where_the_engine_declares_it() {
        let qwen3 = SpeechProfile {
            family: "qwen3_tts".into(),
            language_vocab: vocab(VocabSource::Gguf, &["english", "german", "auto"]),
            ..Default::default()
        };
        assert_eq!(hint_language("de-DE", &qwen3).as_deref(), Some("german"));
        assert_eq!(hint_language("ja", &qwen3), None, "unknown: none sent");

        let magpie = SpeechProfile {
            family: "magpie_tts".into(),
            language_option: true,
            spec_languages: vec!["de".into(), "en".into(), "pt-BR".into()],
            ..Default::default()
        };
        assert_eq!(hint_language("de-DE", &magpie).as_deref(), Some("de"));
        assert_eq!(hint_language("pt", &magpie).as_deref(), Some("pt-BR"));

        let kokoro = SpeechProfile {
            family: "kokoro_tts".into(),
            language_option: true,
            language_vocab: vocab(VocabSource::FamilyTable, &["en-us", "es"]),
            ..Default::default()
        };
        assert_eq!(hint_language("es", &kokoro), None, "its voice decides");

        // Supertonic's request codes are lmgw's reading of its source: an
        // inferred language stays a hint, and goes to it no more than before
        // (the owner's configured one does, `configured`).
        let supertonic = SpeechProfile {
            family: "supertonic".into(),
            spec_languages: vec!["en".into(), "de".into()],
            language_vocab: vocab(VocabSource::SpecRequest, &["en", "de"]),
            ..Default::default()
        };
        assert_eq!(
            hint_language("de", &supertonic),
            None,
            "a hint, not a request"
        );
        assert_eq!(
            tts_fit("de", &supertonic, None).send.as_deref(),
            Some("de"),
            "a request"
        );
    }

    #[test]
    fn a_code_is_named_in_english() {
        assert_eq!(display_name("de"), "German");
        assert_eq!(display_name("de-AT"), "German");
        assert_eq!(display_name("sw"), "sw", "no name: the code");
    }
}
