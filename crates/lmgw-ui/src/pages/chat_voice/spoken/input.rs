//! Voice turns the model hears, in the page's words (voice-audio-input
//! design §5): the mic badge's path and a turn the model heard but the
//! speech-to-text model could not transcribe, the timing readout's "heard
//! as audio, held N ms" or "transcript: <why>", and what a regenerate of
//! such a turn answers from.

use super::{ms_text, MsgVoice, VoiceTiming};

/// What a user bubble the model heard, but that has no transcript, says.
pub(crate) const NOT_TRANSCRIBED: &str = "not transcribed — the model heard it";

impl MsgVoice {
    /// The chat model heard this turn — or this reply's turn — as audio.
    pub(crate) fn heard_as_audio(&self) -> bool {
        self.input.as_deref() == Some("audio")
            || self
                .timing
                .as_ref()
                .is_some_and(|t| t.input.as_deref() == Some("audio"))
    }

    /// A user turn the model heard whose transcription failed: its bubble
    /// has no words of its own.
    pub(crate) fn not_transcribed(&self) -> bool {
        self.transcript_error.is_some()
    }

    /// The mic badge title's tail: how the turn reached the chat model.
    pub(super) fn input_words(&self) -> String {
        let mut t = String::new();
        if self.input.as_deref() == Some("audio") {
            t.push_str(" · the chat model heard it as audio");
        }
        if let Some(e) = &self.transcript_error {
            t.push_str(&format!(
                " · not transcribed ({e}): the model heard it, and later turns read it as \"[spoken \
                 turn, not transcribed]\""
            ));
        }
        t
    }
}

impl VoiceTiming {
    /// The timing line's item for the turn's path: "heard as audio, held N
    /// ms", or "transcript: <why>".
    pub(super) fn input_item(&self) -> Option<String> {
        match self.input.as_deref()? {
            "audio" => Some(match self.transcript_wait_ms {
                Some(ms) => format!("heard as audio, held {}", ms_text(ms)),
                None => "heard as audio".to_string(),
            }),
            _ => Some(match &self.input_why {
                Some(why) => format!("transcript: {why}"),
                None => "transcript".to_string(),
            }),
        }
    }

    /// The expanded readout's line for it.
    pub(super) fn input_detail(&self) -> Option<String> {
        match self.input.as_deref()? {
            "audio" => Some(match self.transcript_wait_ms {
                Some(ms) => format!(
                    "input: heard as audio; its first output waited {} for the transcript",
                    ms_text(ms)
                ),
                None => "input: heard as audio".to_string(),
            }),
            _ => Some(match &self.input_why {
                Some(why) => format!("input: the transcript, because {why}"),
                None => "input: the transcript".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_turn_the_model_heard_says_so_and_its_timing_says_the_hold() {
        let v: MsgVoice = serde_json::from_value(json!({
            "via": "realtime", "asr": "parakeet", "asr_ms": 210, "audio_ms": 2400,
            "input": "audio", "transcript_error": "engine fell over"
        }))
        .unwrap();
        assert!(v.heard_as_audio() && v.not_transcribed());
        let title = v.mic_title();
        assert!(
            title.contains("the chat model heard it as audio"),
            "{title}"
        );
        assert!(
            title.contains("not transcribed (engine fell over)"),
            "{title}"
        );

        let t: VoiceTiming = serde_json::from_value(json!({
            "first_token_ms": 95, "input": "audio", "transcript_wait_ms": 180
        }))
        .unwrap();
        assert_eq!(t.line(), "first token 95 ms · heard as audio, held 180 ms");
        assert!(t.details().contains(
            &"input: heard as audio; its first output waited 180 ms for the transcript".into()
        ));
        let reply = MsgVoice {
            via: "realtime".into(),
            timing: Some(t),
            ..Default::default()
        };
        assert!(reply.heard_as_audio(), "a reply to a heard turn");

        let t: VoiceTiming = serde_json::from_value(json!({
            "input": "transcript", "input_why": "gemma does not take audio input"
        }))
        .unwrap();
        assert_eq!(t.line(), "transcript: gemma does not take audio input");
        // With audio input off: nothing about it.
        let t: VoiceTiming = serde_json::from_value(json!({"first_token_ms": 95})).unwrap();
        assert_eq!(t.line(), "first token 95 ms");
        assert!(!MsgVoice::default().heard_as_audio());
    }
}
