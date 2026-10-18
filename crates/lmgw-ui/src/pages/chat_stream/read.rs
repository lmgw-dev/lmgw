//! The Chat stream's frames that the page reads with more tolerance than the
//! gateway's type has, and what an odd one becomes.
//!
//! The gateway writes every frame from a type of
//! `lmgw_api_types::chat_frames`, and [`super::frame`] reads `data` as the
//! type of the SSE `event` name. Three kinds of frame still need the page's
//! own reading:
//! - a `tool` frame is an enum with no variant for an event this build does
//!   not know ([`ToolEvent`]);
//! - a `done` frame is a [`DoneFrame`], which refuses the voice warm-up's
//!   `done`, an empty object ([`done`]);
//! - a `speech_error` frame must still end the read-aloud and say why when
//!   it does not read whole ([`voice`]).
//!
//! A non-finite number of the `stats` frame, and of the `timings` of `done`,
//! is written as `null`; the type reads it (as NaN) and the page shows no
//! rate for it (`chat::stats`).

use lmgw_api_types::chat_frames::{
    DoneFrame, SpeechDone, SpeechError, SpeechFrame, ToolFrame, VoiceFrame,
};
use serde_json::{Map, Value};

/// A `tool` frame, as the page applies it to a card.
#[derive(Debug, Clone)]
// A frame is matched once as it arrives and dropped; boxing it would ripple
// through every match arm for no gain.
#[allow(clippy::large_enum_variant)]
pub enum ToolEvent {
    /// `start`, `args`, `ready`, `approval` or `result`, whole.
    Frame(ToolFrame),
    /// An `event` this build has no type for (a newer gateway's), or a known
    /// one that does not read whole. The call's card is still opened at its
    /// `index` (0 when there is none), and nothing else is applied.
    Unknown { index: i64 },
}

impl ToolEvent {
    /// The call's position in its turn; none for an `approval`, which names
    /// no call by position.
    pub fn index(&self) -> Option<i64> {
        match self {
            Self::Frame(ToolFrame::Start(f)) => Some(f.index as i64),
            Self::Frame(ToolFrame::Args(f)) => Some(f.index as i64),
            Self::Frame(ToolFrame::Ready(f)) => Some(f.index as i64),
            Self::Frame(ToolFrame::Result(f)) => Some(f.index as i64),
            Self::Frame(ToolFrame::Approval(_)) => None,
            Self::Unknown { index } => Some(*index),
        }
    }
}

/// Read the data of a `tool` frame. The frame is moved into its type, not
/// copied: a result can carry whole images.
pub(super) fn tool(v: Value) -> ToolEvent {
    let index = v["index"].as_i64().unwrap_or(0);
    match serde_json::from_value::<ToolFrame>(v) {
        Ok(f) => ToolEvent::Frame(f),
        Err(_) => ToolEvent::Unknown { index },
    }
}

/// Read the data of a `done` frame. The voice warm-up's `done` has no field
/// at all (`WarmDone`: every stage has settled), and reads as the end of a
/// turn that saved nothing and did not abort. Any other `done` is a whole
/// [`DoneFrame`], or the frame is not read: a damaged one must not look like
/// a turn that saved nothing.
pub(super) fn done(v: Value) -> Option<DoneFrame> {
    if v.as_object().is_some_and(Map::is_empty) {
        return Some(DoneFrame::default());
    }
    serde_json::from_value(v).ok()
}

/// A voice frame of a turn's stream or of a read-aloud's.
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// A model's `state`, read by the status line (`ModelState`).
    State(Value),
    /// The read-aloud's route is open.
    Voice(VoiceFrame),
    /// One clause with its audio.
    Speech(SpeechFrame),
    /// The read-aloud ended, whole or stopped.
    SpeechDone(SpeechDone),
    /// The read-aloud cannot speak or failed.
    SpeechError(SpeechError),
}

/// Read the data of a voice frame named `event`; none for another name, or
/// for a frame that does not read whole, except `speech_error`: it ends the
/// speech and must say so, so what it carries is kept (the code, and a
/// message the page words).
pub(super) fn voice(event: &str, v: Value) -> Option<VoiceEvent> {
    Some(match event {
        "state" => VoiceEvent::State(v),
        "voice" => VoiceEvent::Voice(serde_json::from_value(v).ok()?),
        "speech" => VoiceEvent::Speech(serde_json::from_value(v).ok()?),
        "speech_done" => VoiceEvent::SpeechDone(serde_json::from_value(v).ok()?),
        "speech_error" => VoiceEvent::SpeechError(speech_error(v)),
        _ => return None,
    })
}

fn speech_error(v: Value) -> SpeechError {
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let (code, message) = (text("code"), text("message"));
    serde_json::from_value(v).unwrap_or_else(|_| SpeechError {
        code: code.unwrap_or_default(),
        message: message.unwrap_or_else(|| "the voice failed".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{frame, ChatEvent};
    use super::*;
    use lmgw_api_types::mcp_apps::ToolResultFrame;
    use serde_json::json;

    fn tool_event(v: Value) -> ToolEvent {
        match frame("tool", v) {
            Some(ChatEvent::Tool(t)) => t,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tool_frame_reads_as_its_step() {
        assert!(matches!(
            tool_event(json!({"event": "start", "index": 1, "name": "docs__read"})),
            ToolEvent::Frame(ToolFrame::Start(f)) if (f.index, f.name.as_str()) == (1, "docs__read")
        ));
        assert!(matches!(
            tool_event(json!({"event": "args", "index": 1, "fragment": "{\"a\""})),
            ToolEvent::Frame(ToolFrame::Args(f)) if f.fragment == "{\"a\""
        ));
        let ready = tool_event(json!({
            "event": "ready", "index": 1, "name": "docs__read", "arguments": {"a": 1},
            "call_id": "c1", "server_label": "docs", "needs_approval": false, "ui_resource": null
        }));
        assert!(matches!(
            &ready, ToolEvent::Frame(ToolFrame::Ready(f)) if f.arguments == json!({"a": 1})
        ));
        let approval = tool_event(json!({
            "event": "approval", "approval_request_id": "mcpr_1", "server_label": "mail",
            "name": "send", "arguments": "{}", "call_id": "c2"
        }));
        assert!(matches!(
            &approval, ToolEvent::Frame(ToolFrame::Approval(r)) if r.approval_request_id == "mcpr_1"
        ));
        assert_eq!((ready.index(), approval.index()), (Some(1), None));
        let result = tool_event(json!({
            "event": "result", "index": 2, "name": "docs__read", "output": "text",
            "is_error": true, "ms": 41
        }));
        assert!(matches!(
            result,
            ToolEvent::Frame(ToolFrame::Result(ToolResultFrame {
                index: 2,
                ms: 41,
                is_error: true,
                ..
            }))
        ));
    }

    #[test]
    fn an_unknown_tool_event_keeps_its_place_and_nothing_else() {
        // A newer gateway's event: the card at its index, nothing applied.
        let e = tool_event(json!({"event": "progress", "index": 3, "percent": 40}));
        assert!(matches!(e, ToolEvent::Unknown { index: 3 }), "{e:?}");
        assert_eq!(e.index(), Some(3));
        // No event, no index: the first card, as the page always did.
        assert!(matches!(
            tool_event(json!({})),
            ToolEvent::Unknown { index: 0 }
        ));
        // A known event that does not read whole is no step either.
        assert!(matches!(
            tool_event(json!({"event": "start", "index": 5})),
            ToolEvent::Unknown { index: 5 }
        ));
    }

    #[test]
    fn a_stats_frame_with_a_non_finite_rate_still_reads() {
        // The gateway writes NaN and the infinities as `null`.
        let Some(ChatEvent::Stats(t)) = frame(
            "stats",
            json!({
                "prompt_n": 20, "prompt_ms": 8.5, "prompt_per_second": null,
                "predicted_n": 3, "predicted_ms": null, "predicted_per_second": 41.5,
                "cache_n": 100
            }),
        ) else {
            panic!("the frame was not read");
        };
        assert_eq!((t.prompt_n, t.predicted_n, t.cache_n), (20, 3, Some(100)));
        assert_eq!(
            (t.prompt_per_second, t.predicted_ms),
            (None, None),
            "a `null` is an unknown rate"
        );
        assert_eq!(
            (t.prompt_ms, t.predicted_per_second),
            (Some(8.5), Some(41.5))
        );
    }

    fn saved(extra: Value) -> Value {
        let mut done = json!({
            "aborted": false, "message_id": 7, "saved": true, "model": "m",
            "answered_by": null, "prompt_tokens": 12, "completion_tokens": 3,
            "ttfb_ms": 80, "total_ms": 900, "timings": null,
            "reasoning_ignored": [], "reasoning_note": null
        });
        done.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        done
    }

    fn done_frame(v: Value) -> Option<DoneFrame> {
        match frame("done", v) {
            Some(ChatEvent::Done(d)) => Some(d),
            None => None,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_warm_ups_empty_done_reads_as_a_turn_that_saved_nothing() {
        let d = done_frame(json!({})).expect("the warm-up's done is read");
        assert_eq!(d, DoneFrame::default());
        assert!(!d.aborted && d.saved.is_none());
    }

    #[test]
    fn a_done_frame_reads_whole_or_not_at_all() {
        let d = done_frame(saved(json!({}))).unwrap();
        let s = d.saved.expect("saved part");
        assert_eq!((s.message_id, s.saved, s.total_ms), (7, true, 900));
        // The end of a turn that saved nothing.
        assert_eq!(
            done_frame(json!({"aborted": true})),
            Some(DoneFrame::aborted())
        );
        // More than `aborted` that is not a whole saved part is a damaged
        // frame, not a turn that saved nothing.
        assert!(done_frame(json!({"aborted": false, "message_id": 3})).is_none());
        assert!(done_frame(saved(json!({"message_id": "seven"}))).is_none());
    }

    #[test]
    fn a_done_frame_with_a_non_finite_timing_still_reads() {
        let timings = json!({
            "prompt_n": 20, "prompt_ms": 8.5, "prompt_per_second": null,
            "predicted_n": 3, "predicted_ms": 70.0, "predicted_per_second": null
        });
        let d = done_frame(saved(json!({ "timings": timings }))).expect("read");
        let t = d.saved.and_then(|s| s.timings).expect("timings");
        assert_eq!((t.prompt_per_second, t.predicted_per_second), (None, None));
        assert_eq!(
            (t.prompt_n, t.prompt_ms, t.predicted_ms),
            (20, Some(8.5), Some(70.0))
        );
    }

    fn voice_event(event: &str, v: Value) -> Option<VoiceEvent> {
        match frame(event, v) {
            Some(ChatEvent::Voice(e)) => Some(e),
            None => None,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_speech_frames_read_as_their_types() {
        assert!(matches!(
            voice_event("state", json!({"stage": "tts"})),
            Some(VoiceEvent::State(v)) if v["stage"] == "tts"
        ));
        assert!(matches!(
            voice_event(
                "voice",
                json!({"tts": "audio/a", "voice": null, "tts_answered_by": "audio/b"})
            ),
            Some(VoiceEvent::Voice(f)) if f.tts_answered_by.as_deref() == Some("audio/b")
        ));
        assert!(matches!(
            voice_event("speech", json!({"seq": 0, "text": "Hi.", "pcm": "AAA="})),
            Some(VoiceEvent::Speech(f)) if f.pcm == "AAA="
        ));
        assert!(matches!(
            voice_event(
                "speech_done",
                json!({"chars": 3, "audio_ms": 400, "first_audio_ms": null, "tts": "audio/a",
                       "tts_answered_by": null, "stopped": false})
            ),
            Some(VoiceEvent::SpeechDone(f)) if (f.audio_ms, f.first_audio_ms) == (400, None)
        ));
        // A frame that is not whole says nothing, but a failure still speaks.
        assert!(voice_event("speech", json!({"seq": 0})).is_none());
        assert!(voice_event("speech_done", json!({"chars": 3})).is_none());
    }

    #[test]
    fn a_speech_error_that_does_not_read_whole_keeps_what_it_says() {
        let error = |v| match voice_event("speech_error", v) {
            Some(VoiceEvent::SpeechError(e)) => (e.code, e.message),
            other => panic!("{other:?}"),
        };
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            error(json!({"code": "voice_not_found", "message": "no such voice"})),
            s("voice_not_found", "no such voice")
        );
        assert_eq!(
            error(json!({"code": "voice_not_found"})),
            s("voice_not_found", "the voice failed")
        );
        assert_eq!(error(json!({"message": "boom"})), s("", "boom"));
        assert_eq!(error(json!({})), s("", "the voice failed"));
    }
}
