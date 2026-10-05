//! The owner's languages on their way to the speech models — the spoken one
//! to speech recognition, the reply's to the voice (split 2026-10-05): a
//! **request**, not a hint (chat-voice design §2.1, changed 2026-10-04).
//!
//! The Chat's reply language (a thread's override, else
//! `chat_voice_reply_language`, else the spoken one) is what the owner wants
//! to hear: a text-to-speech row gets it where its engine reads a language,
//! in the engine's spelling and in the field it reads ([`tts_fit`], by
//! [`families::takes_language`] — a table of every text-to-speech family
//! audio.cpp has). The spoken language (`voice.language`, else
//! `chat_voice_language`) is what the owner speaks: a speech-to-text row
//! gets it in its vocabulary's spelling where it declares one
//! ([`asr_fit`]). Where a row cannot take it the reason is
//! a [`Fit::note`] — shown beside the resolved voice and logged — never a
//! refusal per clause:
//! - a row whose accepted set is known and lacks the language is not sent it
//!   (Supertonic, Chatterbox, FireRedTTS3, Sopro answer a code they lack
//!   with a 500; Qwen3-TTS likewise for a name its table lacks);
//! - Kokoro is never sent one: it speaks its voice's language and refuses a
//!   mismatch, so the note names the voice's language when it differs;
//! - a family with one language per package (Pocket TTS, SanoTTS) is sent
//!   none, and noted only when its package speaks another;
//! - an engine that never reads a language (Fish Audio S2, CosyVoice3, …),
//!   or reads only `auto` (VieNeu-TTS), is said to;
//! - a family the table lacks — one audio.cpp added after it was written —
//!   is sent none: lmgw does not know whether it takes one.
//!
//! A language lmgw only infers stays a hint ([`SpeechLanguage::Hint`],
//! [`super::hint_language`]): a stock realtime session synthesizes as
//! before.

use super::{display_name, hint_language, in_vocab, iso_languages, parts};
use crate::audio::families::{self, TakesLanguage};
use crate::audio::profile::{SpeechProfile, VocabSource};

mod asr;
pub use asr::asr_fit;

/// A language clauses are synthesized in, and whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechLanguage {
    /// lmgw inferred it — a realtime session's transcription language: sent
    /// only where the row declares it ([`hint_language`]).
    Hint(String),
    /// The owner set it — the Chat's reply language: sent wherever the
    /// row takes it ([`tts_fit`]).
    Request(String),
}

impl SpeechLanguage {
    /// The language as it was given (an ISO 639-1 code from the Chat).
    pub fn code(&self) -> &str {
        match self {
            Self::Hint(c) | Self::Request(c) => c,
        }
    }

    /// The `language` a clause to a row of `profile`, speaking `voice`, is
    /// sent, and the field it goes in; `None` sends none. A hint goes where
    /// it always went, the request's `language`.
    pub fn for_row(&self, profile: &SpeechProfile, voice: Option<&str>) -> Option<(Field, String)> {
        match self {
            Self::Hint(h) => hint_language(h, profile).map(|l| (Field::Language, l)),
            Self::Request(r) => {
                let fit = tts_fit(r, profile, voice);
                fit.send.map(|s| (fit.field, s))
            }
        }
    }
}

/// Where a row reads the language it is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Field {
    /// The request's `language`, which audio.cpp hands the engine as
    /// `text_input.language`.
    #[default]
    Language,
    /// `options.language`: the family reads only that
    /// ([`TakesLanguage::OptionCode`], [`TakesLanguage::OptionName`]).
    Options,
}

/// What a row does with the configured language.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fit {
    /// The value sent; `None` sends none.
    pub send: Option<String>,
    /// The field it goes in.
    pub field: Field,
    /// Why the language does not reach the model as set, in words that
    /// follow the model's name ("does not speak German (de) — …"); `None`
    /// when it does.
    pub note: Option<String>,
}

impl Fit {
    /// Nothing sent, and why.
    fn unsent(note: String) -> Self {
        Self {
            note: Some(note),
            ..Self::default()
        }
    }

    /// `value` sent in `field`, nothing to say.
    fn sent(value: String, field: Field) -> Self {
        Self {
            send: Some(value),
            field,
            note: None,
        }
    }
}

/// Every entry reads as an ISO 639-1 code or a BCP-47 tag (`de`, `pt-BR`):
/// a spec's `languages` that a request can be held against — not
/// `multilingual`, `auto` or `50+ languages`.
fn all_codes(entries: &[String]) -> bool {
    !entries.is_empty()
        && entries.iter().all(|e| {
            let (primary, norm) = parts(e);
            (2..=3).contains(&primary.len())
                && primary.chars().all(|c| c.is_ascii_lowercase())
                && norm.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// The languages a row takes, for a note: its ISO codes, else the entries
/// as written.
fn takes_list(profile: &SpeechProfile, entries: &[String]) -> String {
    let codes = iso_languages(profile);
    if codes.is_empty() {
        entries.join(", ")
    } else {
        codes.join(", ")
    }
}

/// The entries a request is held against, when they are known: the row's
/// declared vocabulary, else the spec's languages where they are codes.
fn known_entries(profile: &SpeechProfile) -> Option<&[String]> {
    match profile.language_vocab.source {
        VocabSource::Gguf
        | VocabSource::SpecEnum
        | VocabSource::SpecNames
        | VocabSource::SpecRequest
        | VocabSource::FamilyTable
            if !profile.language_vocab.entries.is_empty() =>
        {
            Some(&profile.language_vocab.entries)
        }
        _ if all_codes(&profile.spec_languages) => Some(&profile.spec_languages),
        _ => None,
    }
}

/// How a held language is spelled when it is sent.
#[derive(Clone, Copy)]
enum Spelling {
    /// As the entry it matched (`de`, `German`, `pt-BR`).
    Entry,
    /// The entry's language by its English name (`German`).
    Name,
}

/// `code` held against `entries`: sent in `field` as `spelling` when it
/// names one, else not sent, and why.
fn held(
    code: &str,
    profile: &SpeechProfile,
    entries: &[String],
    (field, spelling): (Field, Spelling),
) -> Fit {
    match in_vocab(code, &profile.family, entries) {
        Some(entry) => Fit::sent(
            match spelling {
                Spelling::Entry => entry,
                Spelling::Name => display_name(&entry),
            },
            field,
        ),
        None => Fit::unsent(format!(
            "does not speak {} ({code}) — it takes {}; the language is not sent, and it speaks its \
             default",
            display_name(code),
            takes_list(profile, entries)
        )),
    }
}

/// The note for a row whose languages lmgw cannot read.
fn unreadable(name: &str, what: &str) -> Fit {
    Fit::unsent(format!(
        "takes only the languages {what}, and lmgw could not read them — the language setting \
         ({name}) is not sent"
    ))
}

/// The configured language `code` for a text-to-speech row of `profile`
/// that speaks `voice` (module doc): the row's spelling of it in the field
/// its engine reads, or none and why.
pub fn tts_fit(code: &str, profile: &SpeechProfile, voice: Option<&str>) -> Fit {
    let code = code.trim();
    if code.is_empty() {
        return Fit::default();
    }
    let name = display_name(code);
    let family = profile.family.as_str();
    let Some(takes) = families::takes_language(family) else {
        return Fit::unsent(format!(
            "is a {family} row: lmgw does not know whether {family} takes a language; not sent"
        ));
    };
    match takes {
        TakesLanguage::Voice => {
            let note = voice
                .and_then(|v| families::voice_language(family, v).map(|l| (v, l)))
                .filter(|(_, spoken)| *spoken != parts(code).0)
                .map(|(v, spoken)| {
                    format!(
                        "takes its language from the voice, and the voice '{v}' speaks {} — the \
                         language setting ({name}) does not reach it; choose a voice of that \
                         language, or another text-to-speech model",
                        display_name(spoken)
                    )
                });
            Fit {
                note,
                ..Fit::default()
            }
        }
        TakesLanguage::Nothing => Fit::unsent(format!(
            "takes no language: it speaks the text as written, so the language setting ({name}) \
             does not reach it"
        )),
        TakesLanguage::AutoOnly => Fit::unsent(format!(
            "takes no language but `auto`: it speaks the text as written, so the language setting \
             ({name}) is not sent"
        )),
        TakesLanguage::Package => match profile.package_language.as_deref() {
            Some(spoken) if spoken == parts(code).0 => Fit::default(),
            Some(spoken) => Fit::unsent(format!(
                "speaks only its package's language, {} — the language setting ({name}) does not \
                 reach it; choose a package of that language, or another text-to-speech model",
                display_name(spoken)
            )),
            None => Fit::unsent(format!(
                "speaks only its package's language, which lmgw cannot tell — the language \
                 setting ({name}) does not reach it"
            )),
        },
        TakesLanguage::SpecLanguages => match known_entries(profile) {
            Some(entries) => held(code, profile, entries, (Field::Language, Spelling::Entry)),
            None => unreadable(&name, "its spec lists"),
        },
        TakesLanguage::PackageTable => match profile.language_vocab.source {
            VocabSource::Gguf if !profile.language_vocab.entries.is_empty() => held(
                code,
                profile,
                &profile.language_vocab.entries,
                (Field::Language, Spelling::Entry),
            ),
            _ => unreadable(&name, "its package lists (its config.json)"),
        },
        TakesLanguage::Code => Fit::sent(code.to_string(), Field::Language),
        TakesLanguage::Name => match known_entries(profile) {
            Some(entries) => held(code, profile, entries, (Field::Language, Spelling::Name)),
            None => Fit::sent(name, Field::Language),
        },
        TakesLanguage::OptionCode => match known_entries(profile) {
            Some(entries) if profile.language_option => {
                held(code, profile, entries, (Field::Options, Spelling::Entry))
            }
            _ => Fit::sent(code.to_string(), Field::Options),
        },
        TakesLanguage::OptionName => match known_entries(profile) {
            Some(entries) => held(code, profile, entries, (Field::Options, Spelling::Name)),
            None => Fit::sent(name, Field::Options),
        },
    }
}

#[cfg(test)]
mod tests;
