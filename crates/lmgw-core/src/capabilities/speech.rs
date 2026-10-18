//! `capabilities.speech` (audio-class gap 9): what a text-to-speech model
//! does with the expressive parts of a request — instructions, inline tags,
//! languages, streaming — so a client (a realtime session's prompt, a voice
//! picker) knows what takes effect before it sends anything.
//!
//! A local audio row's is derived from its speech profile
//! ([`crate::audio::profile`]); a cloud alias has one only through the
//! owner's `capabilities_override` (`{"capabilities": {"speech": {…}}}`),
//! which is also what shaping then goes by
//! ([`crate::audio::shape::Expressive::from_override`]).

use crate::audio::profile::{InstructionsMode, SpeechProfile};
use crate::audio::tags::TagMode;
use crate::config::AudioModel;

pub use lmgw_api_types::capabilities::SpeechCaps;

/// The `instructions` words, as published and as an override may write them.
pub const INSTRUCTIONS_KINDS: [&str; 4] = ["none", "style", "voice_design", "passthrough"];
/// The `inline_tags` words.
pub const INLINE_TAG_KINDS: [&str; 3] = ["none", "fixed", "free"];

/// Whether a model of capability `task` answers `POST /v1/audio/speech`
/// ([`lmgw_api_types::realtime::SPEECH_TASKS`]): a row of it publishes
/// `speech`, and a realtime session may speak with it.
pub fn speaks(task: &str) -> bool {
    lmgw_api_types::realtime::SPEECH_TASKS.contains(&task)
}

/// A local row's speech capabilities, from its profile. `sample_rate` is
/// the row's learned one, when it has answered.
pub fn from_profile(
    profile: &SpeechProfile,
    row: &AudioModel,
    sample_rate: Option<u32>,
) -> SpeechCaps {
    SpeechCaps {
        instructions: Some(profile.instructions.as_str().to_string()),
        instructions_required: Some(profile.instructions == InstructionsMode::VoiceDesign),
        inline_tags: Some(profile.inline_tags.as_str().to_string()),
        tags: if profile.inline_tags == TagMode::Fixed {
            profile.tags.clone()
        } else {
            Vec::new()
        },
        languages: crate::audio::language::iso_languages(profile),
        streaming: Some(row.mode == "streaming"),
        sample_rate,
    }
}

/// The plain-language line of the same facts, for `notes`.
pub fn note(c: &SpeechCaps) -> String {
    let instructions = match c.instructions.as_deref() {
        Some("none") => "instructions are not read by this model (lmgw drops them and says so \
                         in x-lmgw-speech)"
            .to_string(),
        Some("style") => "instructions set a speaking style for the voice".to_string(),
        Some("voice_design") => "instructions describe the voice to design — required, unless \
                                 the row has a default description"
            .to_string(),
        Some(_) => "instructions are sent to the model, which decides what they do".to_string(),
        None => "what instructions do is unknown".to_string(),
    };
    let tags = match c.inline_tags.as_deref() {
        Some("fixed") => format!(
            "inline tags render as sounds: {} (stage directions such as (laughs) map onto \
             them; any other tag is stripped, never read out)",
            c.tags
                .iter()
                .map(|t| format!("[{t}]"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some("free") => "every inline [tag] is passed to the model".to_string(),
        _ => "inline tags such as [laughs] are stripped, never read out".to_string(),
    };
    let mut parts = vec![instructions, tags];
    if !c.languages.is_empty() {
        parts.push(format!(
            "language takes ISO 639-1 codes ({}), sent in the model's own spelling",
            c.languages.join(", ")
        ));
    }
    match c.streaming {
        Some(true) => parts.push(match c.sample_rate {
            Some(rate) => format!(
                "stream_format sse or audio streams PCM16 at {rate} Hz (response_format pcm; \
                 x-lmgw-sample-rate says so on the answer)"
            ),
            None => "stream_format sse or audio streams PCM16 (response_format pcm; its rate is \
                     in x-lmgw-sample-rate once the model has answered one WAV)"
                .to_string(),
        }),
        Some(false) => parts
            .push("streaming (stream_format) is refused: the row runs in offline mode".to_string()),
        None => {}
    }
    format!("Speech: {}.", parts.join("; "))
}

/// An override's `speech` words must be ones lmgw knows.
pub(super) fn validate(c: &SpeechCaps) -> Result<(), String> {
    if let Some(i) = &c.instructions {
        if !INSTRUCTIONS_KINDS.contains(&i.as_str()) {
            return Err(format!(
                "capabilities_override.capabilities.speech.instructions: '{i}' is not one of: {}",
                INSTRUCTIONS_KINDS.join(", ")
            ));
        }
    }
    if let Some(t) = &c.inline_tags {
        if !INLINE_TAG_KINDS.contains(&t.as_str()) {
            return Err(format!(
                "capabilities_override.capabilities.speech.inline_tags: '{t}' is not one of: {}",
                INLINE_TAG_KINDS.join(", ")
            ));
        }
    }
    Ok(())
}
