//! The configured language for a speech-to-text row ([`asr_fit`]): sent as
//! the transcription's `language` — in the row's own spelling where it
//! declares a vocabulary (Nemotron ASR's prompts), else as it came, as the
//! transcription path sends any language (`proxy::audio`'s `asr_language`)
//! — and a note where it does not reach the model:
//! - an engine that detects the language itself (Parakeet TDT);
//! - a set it lacks the language from: the row's vocabulary, the language
//!   query tokens its source reads (SenseVoice), or the spec's codes where
//!   the spec declares a `language` option (Canary, Cohere, Hviske, …) —
//!   the note says whether the engine refuses the call
//!   ([`families::HearsLanguage::RefusesOthers`]) or hears past it;
//! - a package of another language (Kroko ASR refuses any but its own).

use super::{all_codes, display_name, in_vocab, parts, takes_list, Fit};
use crate::audio::families::{self, HearsLanguage};
use crate::audio::profile::{SpeechProfile, VocabSource};

/// The configured language `code` for a speech-to-text row of `profile`
/// (module doc).
pub fn asr_fit(code: &str, profile: &SpeechProfile) -> Fit {
    let code = code.trim();
    if code.is_empty() {
        return Fit::default();
    }
    let name = display_name(code);
    let as_came = |note: Option<String>| Fit {
        send: Some(code.to_string()),
        note,
        ..Fit::default()
    };
    let hears = families::hears_language(&profile.family);
    match hears {
        Some(HearsLanguage::Detects) => {
            return as_came(Some(format!(
                "detects the language itself: the language setting ({name}) does not reach it — \
                 to make sure it hears {name}, choose a speech-to-text model that takes a \
                 language"
            )))
        }
        Some(HearsLanguage::Package) => {
            return as_came(match profile.package_language.as_deref() {
                Some(own) if own != parts(code).0 => Some(format!(
                    "hears only its package's language, {}, so it refuses every transcription \
                     while the language is set to {name} — clear the language, or choose another \
                     speech-to-text model",
                    display_name(own)
                )),
                _ => None,
            })
        }
        Some(HearsLanguage::Only(set)) => {
            let entries: Vec<String> = set.iter().map(|s| s.to_string()).collect();
            return match in_vocab(code, &profile.family, &entries) {
                Some(entry) => as_came(None).with_send(entry),
                None => as_came(Some(format!(
                    "takes only {} as a language: with {name} ({code}) it detects the language \
                     itself",
                    entries.join(", ")
                ))),
            };
        }
        Some(HearsLanguage::RefusesOthers) | None => {}
    }
    let vocab = &profile.language_vocab;
    let (entries, prompts) = if vocab.source != VocabSource::None && !vocab.entries.is_empty() {
        (&vocab.entries, vocab.source == VocabSource::Gguf)
    } else if profile.language_option && all_codes(&profile.spec_languages) {
        (&profile.spec_languages, false)
    } else {
        return as_came(None);
    };
    match in_vocab(code, &profile.family, entries) {
        Some(entry) => as_came(None).with_send(entry),
        None if prompts => as_came(Some(format!(
            "has no prompt for {name} ({code}), so it refuses every transcription while the \
             language is set — clear the language, or choose another speech-to-text model"
        ))),
        None if hears == Some(HearsLanguage::RefusesOthers) => as_came(Some(format!(
            "takes only {} as a language, not {name} ({code}), so it refuses every transcription \
             while the language is set — clear the language, or choose another speech-to-text \
             model",
            takes_list(profile, entries)
        ))),
        None => as_came(Some(format!(
            "takes only {} as a language: the language setting ({name}) does not reach it",
            takes_list(profile, entries)
        ))),
    }
}

impl Fit {
    /// This fit sending `value` instead.
    fn with_send(mut self, value: String) -> Self {
        self.send = Some(value);
        self
    }
}
