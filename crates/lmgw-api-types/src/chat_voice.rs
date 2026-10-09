//! The Chat's voice rules both sides apply (chat-voice design §2): the
//! turn-detection and audio-input names with their labels, and the
//! voice languages' shape. The gateway checks with them, and the dashboard says before Save
//! what the gateway would refuse, so the two cannot drift.

use serde::{Deserialize, Serialize};

/// A speech model its language does not reach as set — the ASR the spoken
/// language, the TTS the reply language: a thread's `voice_resolved.language_notes`,
/// and Settings' `chat_voice_language_notes` for the Chat's own models.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LanguageNote {
    /// `asr` or `tts`.
    pub stage: String,
    /// The stage's alias.
    pub alias: String,
    /// The whole sentence the page shows: the model, then what it does
    /// ("text-to-speech model 'audio/…' takes no language: …").
    pub message: String,
}

// The chat-voice design record, §4.3. The gateway writes these frames
// from its own struct (`realtime::warm::outcome::ModelState`, whose enums
// are its own); a test there reads every frame it writes into this type,
// field for field (review W6-9).
/// A voice stage's model state: a chat stream's `state` frame and a bound
/// realtime session's `lmgw.model.state` event.
/// Read leniently: a field a client does not know of is ignored, a missing
/// one is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelState {
    /// `asr`, `chat` or `tts`.
    pub stage: String,
    pub alias: String,
    /// `loading`, `ready`, `held`, `fallback`, `skipped` or `failed`.
    pub state: String,
    /// How long the load took, for `ready`.
    pub ms: Option<u64>,
    /// `held`: `gpu_hold` or `benchmark`.
    pub cause: Option<String>,
    /// `fallback`: the alias that answers in its place.
    pub answered_by: Option<String>,
    /// `skipped`: `full`, `does_not_fit` or `cannot_speak`.
    pub reason: Option<String>,
    /// The sentence a client shows.
    pub message: Option<String>,
    /// `skipped` with `does_not_fit`: what the stage's group needs on the
    /// card, in bytes.
    pub needed_bytes: Option<u64>,
    /// `skipped` with `does_not_fit`: what the group could have together,
    /// in bytes.
    pub capacity_bytes: Option<u64>,
}

/// How realtime mode detects the end of a turn — the names
/// `chat_turn_detection` and a thread's `voice.turn_detection` take, with
/// the label the dashboard shows for each. The gateway's
/// `store::TurnDetection` parses exactly these names (its tests keep the
/// two in step).
pub const TURN_DETECTIONS: &[(&str, &str)] = &[
    ("semantic_vad", "Smart Turn (semantic_vad)"),
    ("server_vad", "silence only (server_vad)"),
    ("push_to_talk", "push to talk"),
];

/// The label of a turn-detection name; the name itself when it is none of
/// [`TURN_DETECTIONS`].
pub fn turn_detection_label(name: &str) -> &str {
    TURN_DETECTIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(name, |(_, label)| label)
}

/// Whether a voice turn may reach the chat model as audio — the names
/// `chat_voice_audio_input` and a thread's `voice.audio_input` take, with
/// the label the dashboard shows for each (voice-audio-input design §2.1).
/// The gateway's `store::AudioInputMode` parses exactly these names (its
/// tests keep the two in step).
pub const AUDIO_INPUTS: &[(&str, &str)] = &[
    ("off", "off: the model reads the transcript"),
    ("on", "on: models that take audio (experimental)"),
];

/// The label of an audio-input name ([`AUDIO_INPUTS`]); an unknown name is
/// shown as it is.
pub fn audio_input_label(name: &str) -> &str {
    AUDIO_INPUTS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(name, |(_, label)| label)
}

/// `^[a-z]{2}$` — both voice languages' shape (ISO 639-1), the
/// realtime session's own rule. Case is folded before the check by whoever
/// takes the text, so `DE` is a typing variant of `de`, not an error.
pub fn is_language_code(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_lowercase())
}

/// What a typed `chat_voice_language` or `chat_voice_reply_language` means:
/// `Some("")` for none (the spoken language: the ASR detects; the reply
/// language: it follows the spoken one), `Some(code)` folded to lowercase,
/// `None` when it is not two letters.
pub fn language_hint(typed: &str) -> Option<String> {
    let l = typed.trim().to_ascii_lowercase();
    (l.is_empty() || is_language_code(&l)).then_some(l)
}

/// A thread's (or a folder default's) `voice.language` that overrides a
/// language set in Settings → Chat → Voice with none: the ASR detects, as
/// with no language anywhere (chat-voice design §2.2). On
/// `voice.reply_language` it passes over Settings' reply language: replies
/// follow the thread's spoken language as resolved (changed 2026-10-05).
/// Only a thread or folder level takes it; the Chat's own keys say none by
/// being empty.
pub const FOLLOW_USER: &str = "auto";

/// What a typed thread `voice.language` or `voice.reply_language` means:
/// `Some("")` to inherit, `Some("auto")` ([`FOLLOW_USER`]), `Some(code)` folded to lowercase,
/// `None` when it is
/// neither two letters nor `auto`.
pub fn thread_language(typed: &str) -> Option<String> {
    let l = typed.trim().to_ascii_lowercase();
    (l.is_empty() || l == FOLLOW_USER || is_language_code(&l)).then_some(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_hint_is_two_letters_or_none() {
        for (typed, want) in [("", ""), (" DE ", "de"), ("en", "en")] {
            assert_eq!(language_hint(typed).as_deref(), Some(want), "{typed:?}");
        }
        for bad in ["deu", "d", "d1", "de-AT", "ü1"] {
            assert!(language_hint(bad).is_none(), "{bad}");
        }
        assert!(!is_language_code("DE"), "the code itself is lowercase");
    }

    #[test]
    fn a_thread_language_may_also_follow_the_user() {
        for (typed, want) in [("", ""), (" Auto ", "auto"), ("DE", "de")] {
            assert_eq!(thread_language(typed).as_deref(), Some(want), "{typed:?}");
        }
        for bad in ["deu", "automatic", "de-AT"] {
            assert!(thread_language(bad).is_none(), "{bad}");
        }
        assert!(language_hint("auto").is_none(), "not the Chat's own key");
    }

    #[test]
    fn a_turn_detection_label_falls_back_to_the_name() {
        assert_eq!(turn_detection_label("push_to_talk"), "push to talk");
        assert_eq!(turn_detection_label("auto"), "auto");
    }
}

/// `POST /chat/api/threads/{id}/transcribe`'s answer (chat-voice design
/// §5): a recording transcribed by the thread's speech-to-text model —
/// dictation, and a voice answer to an approval that must stay out of the
/// conversation (client-apps design §6.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Dictation {
    pub text: String,
    /// The speech-to-text alias the thread resolves to.
    pub alias: String,
    /// The alias that answered in its place (a GPU-hold or outside-VRAM
    /// fallback); `null` when it answered itself.
    pub asr_answered_by: Option<String>,
    /// The transcription call, admission included.
    pub asr_ms: Option<u64>,
    /// The recording's length, from its WAV header; `null` for any other
    /// container.
    pub audio_ms: Option<u64>,
    /// What the model says it heard, in its own spelling, else the language
    /// the thread's user speaks; `null` when neither is known.
    pub language: Option<String>,
}
