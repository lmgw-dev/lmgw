//! Voice turns the model hears, the store's half (voice-audio-input design
//! §2.1, §5): the setting's values ([`AudioInputMode`]) — Settings → Chat →
//! Voice's `chat_voice_audio_input` and a thread's `voice.audio_input` — and
//! the path a turn's words took to the chat model ([`InputPath`]), as the
//! verdict says it and the timing and the user row record it.

use serde::{Deserialize, Serialize};

/// Whether a voice turn may go to the chat model as audio (§2.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioInputMode {
    /// The chat model reads the ASR's transcript, as before.
    #[default]
    Off,
    /// The model that answers the turn — the thread's, or a fallback it is
    /// handed to, wherever it runs — hears it when it takes audio input and
    /// lmgw can send it audio (`capabilities::hears`); the ASR still
    /// transcribes it beside. Named `local` until 2026-10-06, when the
    /// locality rule went (the owner's ruling): a stored `local` reads as
    /// this ([`Self::parse_stored`]).
    On,
}

/// The value's name before 2026-10-06 ([`AudioInputMode::On`]).
pub(crate) const OLD_ON: &str = "local";

impl AudioInputMode {
    /// Every value, as the setting and the thread override spell it.
    pub const NAMES: [&'static str; 2] = ["off", "on"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    /// The setting's text (trimmed, any case); anything else is `None`.
    /// Strict: it is the save's check too, so `local` is refused on input.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            _ => None,
        }
    }

    /// [`Self::parse`] for a value read back from the store, where a
    /// `local` written before 2026-10-06 still stands for [`Self::On`].
    pub fn parse_stored(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            OLD_ON => Some(Self::On),
            other => Self::parse(other),
        }
    }
}

/// How a voice turn's words reach the chat model (§2.2, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputPath {
    /// The model hears the turn's audio.
    Audio,
    /// The model reads the ASR's transcript.
    Transcript,
}

impl InputPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Transcript => "transcript",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MessageVoice, ThreadVoice, VoiceTiming, VIA_REALTIME};
    use super::*;
    use serde_json::json;

    #[test]
    fn a_threads_audio_input_is_strict_on_input_tolerant_on_read_and_a_folder_default() {
        let v: ThreadVoice = serde_json::from_value(json!({ "audio_input": "on" })).unwrap();
        assert_eq!(v.audio_input, Some(AudioInputMode::On));
        assert_eq!(v.to_stored(), r#"{"audio_input":"on"}"#);
        let e = serde_json::from_value::<ThreadVoice>(json!({ "audio_input": "any" })).unwrap_err();
        assert!(e.to_string().contains("any"), "{e}");
        // The value's name before 2026-10-06: refused on input, `on` when
        // read back from a thread or a folder's default.
        assert!(serde_json::from_value::<ThreadVoice>(json!({ "audio_input": "local" })).is_err());
        let v = ThreadVoice::from_stored(r#"{"audio_input":"local","language":"de"}"#);
        assert_eq!(
            (v.audio_input, v.language.as_deref()),
            (Some(AudioInputMode::On), Some("de"))
        );
        let d = crate::store::ThreadDefaults::from_stored(r#"{"voice":{"audio_input":"local"}}"#);
        assert_eq!(d.voice.unwrap().audio_input, Some(AudioInputMode::On));
        // A value a newer build wrote goes alone.
        let v = ThreadVoice::from_stored(r#"{"audio_input":"any","language":"de"}"#);
        assert_eq!((v.audio_input, v.language.as_deref()), (None, Some("de")));
        // A folder's default lays over a new thread; a thread's own stays.
        let mut t = ThreadVoice::default();
        t.overlay(&ThreadVoice {
            audio_input: Some(AudioInputMode::On),
            ..Default::default()
        });
        assert_eq!(t.audio_input, Some(AudioInputMode::On));
        let mut own = ThreadVoice {
            audio_input: Some(AudioInputMode::Off),
            ..Default::default()
        };
        own.overlay(&ThreadVoice::default());
        assert_eq!(own.audio_input, Some(AudioInputMode::Off));
    }

    #[test]
    fn the_audio_paths_fields_are_absent_unless_set_and_read_tolerantly() {
        // Off is today: a turn without them stores the same bytes as before.
        let today = MessageVoice {
            via: VIA_REALTIME.into(),
            timing: Some(VoiceTiming::default()),
            ..Default::default()
        };
        let text = today.to_stored();
        for key in [
            "\"input\"",
            "input_why",
            "transcript_wait_ms",
            "transcript_error",
        ] {
            assert!(!text.contains(key), "{key} in {text}");
        }
        let heard = MessageVoice {
            via: VIA_REALTIME.into(),
            input: Some(InputPath::Audio),
            transcript_error: Some("the ASR timed out".into()),
            timing: Some(VoiceTiming {
                input: Some(InputPath::Transcript),
                input_why: Some("the server refused the audio".into()),
                transcript_wait_ms: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            MessageVoice::from_stored(Some(heard.to_stored())),
            Some(heard)
        );
        // A path a newer build names goes alone, inside the timing too.
        let v = MessageVoice::from_stored(Some(
            r#"{"via":"realtime","input":"telepathy","transcript_error":"x",
                "timing":{"input":"telepathy","transcript_wait_ms":12}}"#
                .into(),
        ))
        .unwrap();
        assert_eq!((v.input, v.transcript_error.as_deref()), (None, Some("x")));
        let t = v.timing.unwrap();
        assert_eq!((t.input, t.transcript_wait_ms), (None, Some(12)));
    }

    #[test]
    fn audio_input_names_parse_back_and_match_the_dashboards_list() {
        for n in AudioInputMode::NAMES {
            assert_eq!(AudioInputMode::parse(n).unwrap().as_str(), n);
        }
        assert_eq!(AudioInputMode::parse(" ON "), Some(AudioInputMode::On));
        assert_eq!(AudioInputMode::parse("local"), None, "strict on input");
        assert_eq!(
            AudioInputMode::parse_stored(" LOCAL "),
            Some(AudioInputMode::On)
        );
        assert_eq!(
            AudioInputMode::parse_stored("off"),
            Some(AudioInputMode::Off)
        );
        assert_eq!(AudioInputMode::parse("any"), None);
        assert_eq!(AudioInputMode::default(), AudioInputMode::Off);
        let shared: Vec<&str> = lmgw_api_types::chat_voice::AUDIO_INPUTS
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(shared, AudioInputMode::NAMES);
        assert_eq!(
            serde_json::to_value([InputPath::Audio, InputPath::Transcript]).unwrap(),
            serde_json::json!(["audio", "transcript"])
        );
    }
}
