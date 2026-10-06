//! A voice-library clip without its transcript, for an engine that refuses
//! to clone one ([`SpeechProfile::requires_reference_text`], read off
//! audio.cpp's source in [`super::families::clone_requires_transcript`]).
//!
//! audio.cpp hands the engine a library clip's transcript from the
//! library's `prompt_text` index; a clip with no line there reaches
//! OmniVoice, Fish Audio S2 or Qwen3-TTS Base without one, and the engine
//! answers 500. lmgw sees the index too, so it refuses such a request
//! itself — `voice_needs_transcript`, a 400 naming the clip and the fix —
//! before anything is started for it:
//! - `POST /v1/audio/speech` in its preflight ([`refuse_body`]);
//! - a realtime response's and a Chat read-aloud's TTS route when it opens,
//!   and the warm of either ([`refuse_clip`], via
//!   `proxy::synthesize::refuse_route`);
//! - the Chat's voice resolution, read-aloud and voice mode
//!   (`web::chat_voice`), from [`untranscribed`].
//!
//! Never a fallback to another voice: the owner chose this clip, and only
//! the owner can transcribe it (the Audio lab's Transcribe, or
//! `lmgw__voice_transcribe`) — nothing here sends a clip to a speech-to-text
//! model on its own.

use serde_json::{Map, Value};

use super::profile::SpeechProfile;
use super::voices::{RowSpeech, RowVoices, VoiceKind};
use crate::config::AudioModel;
use crate::error::GatewayError;

/// The refusal's code, here and in audio.cpp's own 500 worded the same way
/// ([`super::engine_errors`]).
pub const CODE: &str = "voice_needs_transcript";

/// The library clips `row`'s engine would refuse: its family needs a
/// transcript, the clip has none recorded, and neither does the row's
/// default request options (audio.cpp merges them into every request, so a
/// `reference_text` there reaches the engine). A name a preset of the row
/// also carries is the preset's — audio.cpp takes a preset first — so it is
/// no library clip here ([`RowVoices`] lists one entry per name).
pub fn untranscribed(row: &AudioModel, profile: &SpeechProfile, voices: &RowVoices) -> Vec<String> {
    if !refuses_untranscribed(row, profile) {
        return Vec::new();
    }
    voices
        .entries
        .iter()
        .filter(|e| e.kind == VoiceKind::Library && e.transcript == Some(false))
        .map(|e| e.id.clone())
        .collect()
}

/// `row`'s engine refuses a library clip without its transcript, and the
/// row's default request options give it none: what `GET
/// /v1/audio/voices` says as `lmgw.needs_transcript`.
pub fn refuses_untranscribed(row: &AudioModel, profile: &SpeechProfile) -> bool {
    profile.requires_reference_text && !row_has_text(row)
}

/// Why `clip` cannot be spoken by `model`, and what to do: transcribe it in
/// the Audio lab, or pick another voice. One wording for every refusal of
/// this kind.
pub fn message(model: &str, clip: &str) -> String {
    format!(
        "the voice clip '{clip}' has no transcript, and the text-to-speech model '{model}' \
         cannot clone a clip without one — transcribe it in the Audio lab (Voice library → \
         Transcribe, with a speech-to-text model), or pick another voice"
    )
}

/// `Err` ([`CODE`]) when `voice` names a clip of [`untranscribed`]: what a
/// TTS route is refused for when it opens for a voice, before admission.
pub fn refuse_clip(
    row: &AudioModel,
    speech: &RowSpeech,
    voice: Option<&str>,
) -> Result<(), GatewayError> {
    let Some(clip) = voice.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    if untranscribed(row, &speech.profile, &speech.voices)
        .iter()
        .any(|c| c == clip)
    {
        return Err(GatewayError::InvalidRequest {
            code: CODE,
            message: message(&row.model_id, clip),
        });
    }
    Ok(())
}

/// [`refuse_clip`] for a `POST /v1/audio/speech` body: its `voice`, unless
/// it sends a transcript of its own (`reference_text`, top level or in
/// `options`) or a `voice_ref` — then audio.cpp does not take the library
/// clip at all.
pub fn refuse_body(
    row: &AudioModel,
    speech: &RowSpeech,
    body: &Map<String, Value>,
) -> Result<(), GatewayError> {
    let options = body.get("options");
    let sends_text = [
        body.get("reference_text"),
        options.and_then(|o| o.get("reference_text")),
    ]
    .into_iter()
    .any(said);
    if sends_text || body.contains_key("voice_ref") {
        return Ok(());
    }
    refuse_clip(row, speech, body.get("voice").and_then(Value::as_str))
}

fn row_has_text(row: &AudioModel) -> bool {
    said(row.default_request_options.get("reference_text"))
}

fn said(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests;
