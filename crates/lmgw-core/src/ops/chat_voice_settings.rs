//! The Chat's Voice settings (chat-voice design §2.1): the eight `chat_*`
//! keys beside `chat_stt_alias`, applied and checked here for both save
//! paths — `settings_set` (the self-admin tool) and the dashboard's
//! `settings_set_full` — so the two cannot drift.
//!
//! The checks are realtime's own: a TTS alias must be one whose capability
//! task speaks (`tts` or `vdes`, [`super::validate_tts_alias`]), both
//! languages — the one the user speaks and the one replies are in — are
//! ISO 639-1 codes, turn detection is one of three names, and audio
//! input one of two (voice-audio-input design §2.1). An empty alias, voice,
//! style or language is never refused: each "empty" falls through to
//! realtime's setting or means "none".

use crate::config::Settings;
use crate::state::SharedState;
use crate::store::{AudioInputMode, TurnDetection};

/// The eight keys as a sparse patch; `None` leaves a key alone.
#[derive(Debug, Default)]
pub struct ChatVoicePatch {
    pub chat_tts_alias: Option<String>,
    pub chat_voice: Option<String>,
    pub chat_speech_style: Option<String>,
    pub chat_voice_language: Option<String>,
    pub chat_voice_reply_language: Option<String>,
    pub chat_read_aloud: Option<bool>,
    pub chat_turn_detection: Option<String>,
    pub chat_voice_audio_input: Option<String>,
}

/// `chat_voice_language`: empty (none) or an ISO 639-1 code, case folded.
pub fn validate_chat_voice_language(v: &str) -> Result<String, String> {
    lmgw_api_types::chat_voice::language_hint(v).ok_or_else(|| {
        format!(
            "chat_voice_language '{v}' is not an ISO 639-1 code (two letters, such as de or en); \
             empty is none"
        )
    })
}

/// `chat_voice_reply_language`: empty (replies follow `chat_voice_language`)
/// or an ISO 639-1 code, case folded.
pub fn validate_chat_voice_reply_language(v: &str) -> Result<String, String> {
    lmgw_api_types::chat_voice::language_hint(v).ok_or_else(|| {
        format!(
            "chat_voice_reply_language '{v}' is not an ISO 639-1 code (two letters, such as de \
             or en); empty follows chat_voice_language"
        )
    })
}

/// `chat_turn_detection`: one of [`TurnDetection::NAMES`].
pub fn validate_chat_turn_detection(v: &str) -> Result<String, String> {
    TurnDetection::parse(v)
        .map(|t| t.as_str().to_string())
        .ok_or_else(|| {
            format!(
                "chat_turn_detection '{v}' is not one of {}",
                TurnDetection::NAMES.join(", ")
            )
        })
}

/// `chat_voice_audio_input`: one of [`AudioInputMode::NAMES`].
pub fn validate_chat_voice_audio_input(v: &str) -> Result<String, String> {
    AudioInputMode::parse(v)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            format!(
                "chat_voice_audio_input '{v}' is not one of {}",
                AudioInputMode::NAMES.join(", ")
            )
        })
}

/// The keys as loaded from the DB: a hand edit, or a value a newer build
/// added, falls back (logged) rather than reaching a session — turn
/// detection to `semantic_vad`, a language that is no ISO 639-1 code to no
/// hint, audio input to `off`. What reads is normalised as a save would
/// store it: an audio input of `local`, the value's name before 2026-10-06,
/// reads as `on`, and the next save of any setting stores that.
pub fn normalise_loaded_chat_voice(s: &mut Settings) {
    match TurnDetection::parse(&s.chat_turn_detection) {
        Some(t) => s.chat_turn_detection = t.as_str().to_string(),
        None => {
            tracing::warn!(
                "stored setting chat_turn_detection = {:?} is not one of {}; using semantic_vad \
                 until it is saved again",
                s.chat_turn_detection,
                TurnDetection::NAMES.join(", ")
            );
            s.chat_turn_detection = TurnDetection::default().as_str().to_string();
        }
    }
    match validate_chat_voice_language(&s.chat_voice_language) {
        Ok(l) => s.chat_voice_language = l,
        Err(_) => {
            tracing::warn!(
                "stored setting chat_voice_language = {:?} is not an ISO 639-1 code; no language \
                 hint until it is saved again",
                s.chat_voice_language
            );
            s.chat_voice_language = String::new();
        }
    }
    match validate_chat_voice_reply_language(&s.chat_voice_reply_language) {
        Ok(l) => s.chat_voice_reply_language = l,
        Err(_) => {
            tracing::warn!(
                "stored setting chat_voice_reply_language = {:?} is not an ISO 639-1 code; \
                 replies follow chat_voice_language until it is saved again",
                s.chat_voice_reply_language
            );
            s.chat_voice_reply_language = String::new();
        }
    }
    match AudioInputMode::parse_stored(&s.chat_voice_audio_input) {
        Some(m) => {
            // Said once per process: every snapshot reload reads it again
            // until a settings save stores the new name.
            static SAID: std::sync::Once = std::sync::Once::new();
            if AudioInputMode::parse(&s.chat_voice_audio_input).is_none() {
                SAID.call_once(|| {
                    tracing::info!(
                        "stored setting chat_voice_audio_input = {:?} reads as '{}' (renamed \
                     2026-10-06); the next settings save stores it",
                        s.chat_voice_audio_input,
                        m.as_str()
                    )
                });
            }
            s.chat_voice_audio_input = m.as_str().to_string();
        }
        None => {
            tracing::warn!(
                "stored setting chat_voice_audio_input = {:?} is not one of {}; using off until \
                 it is saved again",
                s.chat_voice_audio_input,
                AudioInputMode::NAMES.join(", ")
            );
            s.chat_voice_audio_input = AudioInputMode::Off.as_str().to_string();
        }
    }
}

/// Apply `p` onto `s`, checking each key it sets; the keys it touched. `s`
/// is the caller's working copy: on an error the caller stores nothing.
pub async fn apply_chat_voice(
    state: &SharedState,
    s: &mut Settings,
    p: ChatVoicePatch,
) -> Result<Vec<&'static str>, String> {
    let mut changed = Vec::new();
    if let Some(v) = p.chat_tts_alias {
        let v = v.trim();
        if !v.is_empty() {
            super::validate_tts_alias(state, v)
                .await
                .map_err(|e| format!("chat_tts_alias: {e}"))?;
        }
        s.chat_tts_alias = v.to_string();
        changed.push("chat_tts_alias");
    }
    if let Some(v) = p.chat_voice {
        s.chat_voice = v.trim().to_string();
        changed.push("chat_voice");
    }
    if let Some(v) = p.chat_speech_style {
        // Any text is a style or a voice description, as realtime's own.
        s.chat_speech_style = v.trim().to_string();
        changed.push("chat_speech_style");
    }
    if let Some(v) = p.chat_voice_language {
        s.chat_voice_language = validate_chat_voice_language(&v)?;
        changed.push("chat_voice_language");
    }
    if let Some(v) = p.chat_voice_reply_language {
        s.chat_voice_reply_language = validate_chat_voice_reply_language(&v)?;
        changed.push("chat_voice_reply_language");
    }
    if let Some(v) = p.chat_read_aloud {
        s.chat_read_aloud = v;
        changed.push("chat_read_aloud");
    }
    if let Some(v) = p.chat_turn_detection {
        s.chat_turn_detection = validate_chat_turn_detection(&v)?;
        changed.push("chat_turn_detection");
    }
    if let Some(v) = p.chat_voice_audio_input {
        s.chat_voice_audio_input = validate_chat_voice_audio_input(&v)?;
        changed.push("chat_voice_audio_input");
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_and_turn_detection_are_checked_and_normalised() {
        assert_eq!(validate_chat_voice_language(" DE ").unwrap(), "de");
        assert_eq!(validate_chat_voice_language("").unwrap(), "");
        for bad in ["deu", "german", "d", "de-AT"] {
            assert!(validate_chat_voice_language(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_chat_voice_reply_language(" EN ").unwrap(), "en");
        assert_eq!(validate_chat_voice_reply_language("").unwrap(), "");
        for bad in ["eng", "english", "auto", "en-GB"] {
            let e = validate_chat_voice_reply_language(bad).unwrap_err();
            assert!(e.starts_with("chat_voice_reply_language"), "{e}");
        }
        assert_eq!(
            validate_chat_turn_detection(" Push_To_Talk ").unwrap(),
            "push_to_talk"
        );
        let e = validate_chat_turn_detection("auto").unwrap_err();
        assert!(e.contains("semantic_vad, server_vad, push_to_talk"), "{e}");
        assert_eq!(validate_chat_voice_audio_input(" On ").unwrap(), "on");
        let e = validate_chat_voice_audio_input("any").unwrap_err();
        assert!(e.contains("off, on"), "{e}");
        // The value's name before 2026-10-06 is refused on input.
        let e = validate_chat_voice_audio_input("local").unwrap_err();
        assert!(e.contains("'local' is not one of off, on"), "{e}");
    }

    #[test]
    fn a_bad_stored_value_falls_back_at_load_and_a_good_one_is_normalised() {
        let mut s = Settings {
            chat_turn_detection: "auto".into(),
            chat_voice_language: "german".into(),
            chat_voice_reply_language: "english".into(),
            chat_voice_audio_input: "always".into(),
            ..Default::default()
        };
        normalise_loaded_chat_voice(&mut s);
        assert_eq!(
            (
                s.chat_turn_detection.as_str(),
                s.chat_voice_language.as_str(),
                s.chat_voice_audio_input.as_str()
            ),
            ("semantic_vad", "", "off")
        );
        assert_eq!(s.chat_voice_reply_language, "");
        s.chat_turn_detection = " Server_VAD ".into();
        s.chat_voice_language = " DE ".into();
        s.chat_voice_reply_language = " En ".into();
        // A stored `local`, the value's name before 2026-10-06, reads as
        // `on`.
        s.chat_voice_audio_input = " LOCAL ".into();
        normalise_loaded_chat_voice(&mut s);
        assert_eq!(
            (
                s.chat_turn_detection.as_str(),
                s.chat_voice_language.as_str(),
                s.chat_voice_audio_input.as_str()
            ),
            ("server_vad", "de", "on")
        );
        assert_eq!(s.chat_voice_reply_language, "en");
    }
}
