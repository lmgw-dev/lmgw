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
    /// A local model lmgw runs whose input modalities include audio hears
    /// the turn; the ASR still transcribes it beside. A cloud model never
    /// gets audio.
    Local,
}

impl AudioInputMode {
    /// Every value, as the setting and the thread override spell it.
    pub const NAMES: [&'static str; 2] = ["off", "local"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
        }
    }

    /// The setting's text (trimmed, any case); anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "local" => Some(Self::Local),
            _ => None,
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
        let v: ThreadVoice = serde_json::from_value(json!({ "audio_input": "local" })).unwrap();
        assert_eq!(v.audio_input, Some(AudioInputMode::Local));
        assert_eq!(v.to_stored(), r#"{"audio_input":"local"}"#);
        let e = serde_json::from_value::<ThreadVoice>(json!({ "audio_input": "any" })).unwrap_err();
        assert!(e.to_string().contains("any"), "{e}");
        // A value a newer build wrote goes alone.
        let v = ThreadVoice::from_stored(r#"{"audio_input":"any","language":"de"}"#);
        assert_eq!((v.audio_input, v.language.as_deref()), (None, Some("de")));
        // A folder's default lays over a new thread; a thread's own stays.
        let mut t = ThreadVoice::default();
        t.overlay(&ThreadVoice {
            audio_input: Some(AudioInputMode::Local),
            ..Default::default()
        });
        assert_eq!(t.audio_input, Some(AudioInputMode::Local));
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
        assert_eq!(
            AudioInputMode::parse(" LOCAL "),
            Some(AudioInputMode::Local)
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
