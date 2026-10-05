//! audio.cpp's own error texts for a speech request, worded as lmgw's
//! refusals — the ones lmgw cannot always see coming, so the 500 still
//! happens, but the owner is told what it means and what to do instead of a
//! bare "upstream error (500)". Each stays a visible error; none falls back
//! to another voice or model.
//!
//! - **A clone without its transcript** → [`super::transcript::CODE`]
//!   (`voice_needs_transcript`): the family table lists the engines lmgw
//!   knows refuse ([`super::families::clone_requires_transcript`]), and
//!   this catches the rest — a family upstream adds or changes later.
//!   Matched on the throws in audio.cpp's source: "… requires reference_text
//!   …" (OmniVoice, Fish Audio S2, Audio8, F5-TTS, ZipVoice, GLM-TTS,
//!   Kitten TTS 2, BreezeTTS), "… requires reference text" (Qwen3-TTS
//!   Base), "… requires --reference-text" and "reference_text must not be
//!   empty" (OuteTTS). Not "prompt audio requires prompt_text or
//!   reference_text" (VoxCPM, continuation audio, which lmgw never sends),
//!   nor "reference_text requires …" (the other way round: DotTTS,
//!   MOSS-TTS-Nano).
//! - **No eSpeak NG in the image** → [`ESPEAK_CODE`]: the phonemizing
//!   families (Kokoro, sanoTTS, Piper, Kitten TTS, Inflect, ZipVoice) load
//!   `libespeak-ng.so.1` at run time, "Could not load eSpeak-ng; install the
//!   shared library or provide its path" (`framework/text/
//!   espeak_phonemizer.cpp`) when the image lacks it. lmgw's audio.cpp
//!   builds install it since 2026-10-01 (the `runtime-espeak` edit,
//!   `backends::presets`); an image built before, or upstream's own, does
//!   not. The image's labels do not record its build's edits, so this is
//!   said when it happens, not before.

use crate::error::GatewayError;

/// The code of a phonemizing model on an image without eSpeak NG.
pub const ESPEAK_CODE: &str = "audio_image_lacks_espeak";

/// `e` as one of the refusals above when it is audio.cpp's text for it,
/// else `e` itself. `model` is the alias (or row) that answered, `voice` the
/// voice the request named — the clip, for a clone.
pub fn explain_speech(e: GatewayError, model: &str, voice: Option<&str>) -> GatewayError {
    let GatewayError::Upstream { message, .. } = &e else {
        return e;
    };
    let lower = message.to_ascii_lowercase();
    if needs_transcript(&lower) {
        let clip = voice.map(str::trim).filter(|v| !v.is_empty());
        let mut text = match clip {
            Some(clip) => super::transcript::message(model, clip),
            None => format!(
                "the text-to-speech model '{model}' cannot clone its reference audio without \
                 the clip's transcript — transcribe the clip in the Audio lab (Voice library → \
                 Transcribe), or pick another voice"
            ),
        };
        text.push_str(&format!(" (audio.cpp said: {})", message.trim()));
        return GatewayError::InvalidRequest {
            code: super::transcript::CODE,
            message: text,
        };
    }
    if lower.contains("could not load espeak") {
        return GatewayError::InvalidRequest {
            code: ESPEAK_CODE,
            message: format!(
                "'{model}' needs eSpeak NG, which this audio.cpp image lacks — rebuild the \
                 audio.cpp image on the Backends page (builds since 2026-10-01 include it), then \
                 re-apply the audio containers (audio.cpp said: {})",
                message.trim()
            ),
        };
    }
    e
}

/// audio.cpp's refusal of a clone without its transcript (module doc).
fn needs_transcript(lower: &str) -> bool {
    [
        "requires reference_text",
        "requires reference text",
        "requires --reference-text",
        "reference_text must not be empty",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(message: &str) -> GatewayError {
        GatewayError::Upstream {
            status: 500,
            provider_type: None,
            message: message.into(),
        }
    }

    fn code(e: &GatewayError) -> &'static str {
        e.code()
    }

    #[test]
    fn every_transcript_throw_in_audio_cpp_is_voice_needs_transcript() {
        for text in [
            "OmniVoice native voice clone currently requires reference_text when reference \
             audio is provided",
            "Qwen3 voice clone ICL mode requires reference text",
            "speaker with inline reference audio requires reference_text option",
            "F5-TTS requires reference_text (transcript of the reference audio)",
            "zipvoice requires reference_text (transcript of the reference audio)",
            "GLM-TTS voice cloning requires reference_text",
            "Kitten voice cloning requires reference_text matching the reference audio",
            "BreezeTTS clone requires reference_text",
            "OuteTTS voice cloning requires --reference-text",
            "OuteTTS reference_text must not be empty",
        ] {
            let e = explain_speech(upstream(text), "audio/omni", Some("anna"));
            assert_eq!(code(&e), "voice_needs_transcript", "{text}");
            assert_eq!(e.http_status(), axum::http::StatusCode::BAD_REQUEST);
            let m = e.to_string();
            assert!(m.contains("'anna'") && m.contains("'audio/omni'"), "{m}");
            assert!(m.contains("Audio lab"), "{m}");
            assert!(m.contains(text), "audio.cpp's own words stay: {m}");
        }
    }

    #[test]
    fn the_other_way_round_and_prompt_audio_are_left_alone() {
        for text in [
            "VoxCPM2 prompt audio requires prompt_text or reference_text",
            "DotTTS reference_text requires prompt reference audio",
            "MOSS-TTS-Nano reference_text requires --voice-ref",
            "CosyVoice3 requires reference audio",
        ] {
            let e = explain_speech(upstream(text), "m", Some("anna"));
            assert_eq!(code(&e), "upstream", "{text}");
        }
    }

    #[test]
    fn a_clone_with_no_voice_named_still_says_what_to_do() {
        let e = explain_speech(
            upstream("BreezeTTS clone requires reference_text"),
            "audio/breeze",
            None,
        );
        assert_eq!(code(&e), "voice_needs_transcript");
        assert!(e
            .to_string()
            .contains("transcribe the clip in the Audio lab"));
    }

    #[test]
    fn a_missing_espeak_says_to_rebuild_the_image() {
        let e = explain_speech(
            upstream("Could not load eSpeak-ng; install the shared library or provide its path"),
            "audio/sanotts-de",
            None,
        );
        assert_eq!(code(&e), "audio_image_lacks_espeak");
        let m = e.to_string();
        assert!(m.starts_with("'audio/sanotts-de' needs eSpeak NG"), "{m}");
        assert!(
            m.contains("Backends page") && m.contains("2026-10-01"),
            "{m}"
        );
    }

    #[test]
    fn anything_else_passes_through() {
        let e = explain_speech(GatewayError::Timeout, "m", None);
        assert!(matches!(e, GatewayError::Timeout));
        let e = explain_speech(upstream("out of memory"), "m", None);
        assert_eq!(code(&e), "upstream");
    }
}
