//! What a speech request is refused for **before admission** — before a
//! container is started, a model evicted or a claim taken for a request the
//! engine would only turn down.
//!
//! [`refuse_speech`] runs on the route the gate resolved (admission may
//! still answer with a fallback; that one is not judged here — it gets the
//! client's body, shaped for it). Since live run 3 it also refuses a
//! request with no voice for a family that clones from reference audio
//! ([`refuse_unreferenced`]). [`crate::proxy::synthesize`] runs
//! [`refuse_row`] and [`refuse_undescribed`] once when a realtime response
//! opens its route, before admission, and the same checks per clause.
//! [`refuse_unsayable`] judges a body after shaping: the speech route tries
//! the resolved row's shaping before admission, and every TTS path checks
//! what it is about to send.

use serde_json::{Map, Value};

use super::profile::{InstructionsMode, SpeechProfile, Unvoiced};
use super::tags::only_tags;
use crate::config::AudioModel;
use crate::error::GatewayError;

/// `Err` when the request cannot be served: an empty or blank input, or one of
/// nothing but inline tags, on any route; on an lmgw audio row (`row`, with its profile), the
/// row's own misconfiguration or a feature it does not have.
pub fn refuse_speech(
    body: &Map<String, Value>,
    row: Option<(&AudioModel, &SpeechProfile)>,
) -> Result<(), GatewayError> {
    if body
        .get("input")
        .and_then(Value::as_str)
        .is_some_and(only_tags)
    {
        return Err(GatewayError::InvalidRequest {
            code: "empty_input",
            message: "input holds only inline tags and no text to speak — a tag such as \
                      [laughs] is voiced around text, never alone, and is not read out"
                .into(),
        });
    }
    // Every engine turns an empty text down, after a container start and
    // as a 502 (review TC-21).
    if body
        .get("input")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().is_empty())
    {
        return Err(GatewayError::InvalidRequest {
            code: "empty_input",
            message: match body.get("model").and_then(Value::as_str) {
                Some(model) => format!("input is empty: '{model}' has nothing to speak"),
                None => "input is empty: there is nothing to speak".into(),
            },
        });
    }
    let Some((row, profile)) = row else {
        return Ok(());
    };
    refuse_row(row, profile)?;
    // Streaming takes a row in streaming mode (audio-class gap 8): an
    // offline one fails the request in its engine, after a start.
    if wants_stream(body) && row.mode != "streaming" {
        let how = if profile.streaming {
            "set the row's mode to streaming (its family streams; a streaming row still answers \
             a plain request with one WAV)"
        } else {
            "its family has no streaming mode"
        };
        return Err(GatewayError::InvalidRequest {
            code: "streaming_unsupported",
            message: format!(
                "'{}' runs in offline mode, so it cannot stream (stream_format, stream) — leave \
                 them out for one WAV, or {how}",
                row.model_id
            ),
        });
    }
    refuse_undescribed(row, profile, sent_description(body))?;
    refuse_unreferenced(row, profile, body)
}

/// `Err` (`reference_required`) when the row's engine clones from reference
/// audio and refuses without it ([`Unvoiced::NeedsReference`], CosyVoice3)
/// and nothing would give it a clip (live run 3, N4): the request names no
/// `voice` and sends no `voice_ref`, and the row has no default voice
/// preset that loads one — audio.cpp applies that preset only when the
/// request names no voice. Before, the container started and the engine
/// answered 500 "requires reference audio". A `voice` the request names is
/// the engine's to judge (a library clip or a preset with one clones).
pub fn refuse_unreferenced(
    row: &AudioModel,
    profile: &SpeechProfile,
    body: &Map<String, Value>,
) -> Result<(), GatewayError> {
    let names_voice = said(body.get("voice")).is_some() || body.contains_key("voice_ref");
    if profile.unvoiced != Unvoiced::NeedsReference || names_voice || default_clip(row) {
        return Ok(());
    }
    Err(GatewayError::InvalidRequest {
        code: "reference_required",
        message: format!(
            "'{}' clones its voice from reference audio and cannot speak without one — name a \
             voice-library clip (or a preset with a clip) in `voice`, send `voice_ref`, or give \
             the row a default voice preset with a `voice_ref`",
            row.model_id
        ),
    })
}

/// The row's default voice preset loads a clip (`voice_ref`): inline, or
/// the preset it names.
pub fn default_clip(row: &AudioModel) -> bool {
    let preset = match &row.default_voice_preset {
        Some(Value::String(name)) => row.voice_presets.get(name.trim()),
        other => other.as_ref(),
    };
    preset
        .and_then(Value::as_object)
        .is_some_and(|p| p.contains_key("voice_ref"))
}

/// `Err` (`instructions_required`) when the row designs its voice from a
/// description (a voice-design profile) and none would reach its engine:
/// not `sent` — the description the caller sends with every request, `None`
/// for none — and not one in the row's default request options, which
/// audio.cpp merges into every request.
///
/// `POST /v1/audio/speech` passes the request's own ([`refuse_speech`]); a
/// realtime response passes what its clauses will carry, checked once when
/// its route opens and before admission ([`crate::proxy::synthesize`]).
pub fn refuse_undescribed(
    row: &AudioModel,
    profile: &SpeechProfile,
    sent: Option<&str>,
) -> Result<(), GatewayError> {
    let sent = sent.is_some_and(|t| !t.trim().is_empty());
    if profile.instructions != InstructionsMode::VoiceDesign || sent || row_describes(row) {
        return Ok(());
    }
    Err(GatewayError::InvalidRequest {
        code: "instructions_required",
        message: format!(
            "'{}' is a voice-design model: it designs the voice from a description and cannot \
             speak without one — send `instructions` (e.g. \"a warm, calm female voice in her \
             forties\"), or give the row a default under its default request options \
             (`instruct`)",
            row.model_id
        ),
    })
}

/// The request asks for a streamed answer — as audio.cpp reads it: any
/// `stream_format`, or `stream: true`.
pub fn wants_stream(body: &Map<String, Value>) -> bool {
    body.get("stream_format").is_some() || body.get("stream").and_then(Value::as_bool) == Some(true)
}

/// `Err` when the row cannot speak at all, whatever is asked: its task is
/// not the one its package's variant runs (gap 2,
/// [`super::variant::mismatch`]) — audio.cpp would start the container and
/// fail the request in it.
pub fn refuse_row(row: &AudioModel, profile: &SpeechProfile) -> Result<(), GatewayError> {
    let message = profile
        .variant
        .as_deref()
        .and_then(|v| super::variant::mismatch(&row.model_id, &row.task, v));
    match message {
        Some(message) => Err(GatewayError::InvalidRequest {
            code: "task_mismatch",
            message,
        }),
        None => Ok(()),
    }
}

/// The description a request carries: its `instructions`, else its
/// `options.instruction` or `options.instruct` — the first that says
/// something.
fn sent_description(body: &Map<String, Value>) -> Option<&str> {
    let options = body.get("options");
    std::iter::once(body.get("instructions"))
        .chain(["instruction", "instruct"].map(|k| options.and_then(|o| o.get(k))))
        .find_map(said)
}

/// The row's default request options hold a description.
fn row_describes(row: &AudioModel) -> bool {
    ["instruction", "instruct"]
        .iter()
        .any(|k| said(row.default_request_options.get(*k)).is_some())
}

/// A string that is not blank.
fn said(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|t| !t.trim().is_empty())
}

/// An input that character shaping left nothing to say: `empty_input`.
mod unsayable;
pub use unsayable::refuse_unsayable;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(defaults: Value) -> AudioModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": "design", "family": "qwen3_tts", "path": "p", "task": "vdes",
            "mode": "offline", "load_options": {}, "session_options": {},
            "default_request_options": defaults,
            "voice_presets": {}, "default_voice_preset": null, "enabled": true, "image": null,
            "extra_run_args": null, "warm_start": false
        }))
        .unwrap()
    }

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn code(r: Result<(), GatewayError>) -> Option<&'static str> {
        r.err().map(|e| e.kind())
    }

    /// Live run 3, N4: CosyVoice3 without a voice passed preflight, its
    /// container started, and the engine answered 500 "CosyVoice3 requires
    /// reference audio". Refused before anything starts now — unless the
    /// request names a voice or a clip, or the row's default preset loads
    /// one.
    #[test]
    fn a_cloning_family_without_a_reference_is_refused() {
        let clones = SpeechProfile {
            unvoiced: Unvoiced::NeedsReference,
            ..Default::default()
        };
        let mut cosy = row(json!({}));
        cosy.model_id = "cosy".into();
        cosy.task = "tts".into();
        let bare = body(json!({"model": "m", "input": "Hi."}));
        let e = refuse_speech(&bare, Some((&cosy, &clones))).unwrap_err();
        assert_eq!(e.kind(), "reference_required");
        assert!(e.to_string().contains("'cosy' clones its voice"), "{e}");
        for ok in [
            json!({"model": "m", "input": "Hi.", "voice": "me_11s"}),
            json!({"model": "m", "input": "Hi.", "voice_ref": "/voices/a.wav"}),
        ] {
            assert_eq!(code(refuse_speech(&body(ok), Some((&cosy, &clones)))), None);
        }
        // The row's default preset: a clip clones, a voice id does not.
        cosy.voice_presets = json!({"anna": {"voice_ref": "/voices/anna.wav"},
                                    "id": {"voice_id": "x"}})
        .as_object()
        .cloned()
        .unwrap();
        for (preset, refused) in [
            (json!("anna"), false),
            (json!({"voice_ref": "/voices/b.wav"}), false),
            (json!("id"), true),
            (json!({"voice_id": "x"}), true),
        ] {
            cosy.default_voice_preset = Some(preset.clone());
            assert_eq!(
                code(refuse_speech(&bare, Some((&cosy, &clones)))).is_some(),
                refused,
                "{preset}"
            );
        }
        // A family that speaks without a voice, or one not known: as before.
        cosy.default_voice_preset = None;
        for unvoiced in [
            Unvoiced::EngineDefault { voice: None },
            Unvoiced::DrawsSpeaker,
            Unvoiced::Unknown,
        ] {
            let p = SpeechProfile {
                unvoiced,
                ..Default::default()
            };
            assert_eq!(code(refuse_speech(&bare, Some((&cosy, &p)))), None);
        }
    }

    #[test]
    fn nothing_but_tags_is_refused_on_any_route() {
        let b = body(json!({"model": "m", "input": "[laughs]"}));
        assert_eq!(code(refuse_speech(&b, None)), Some("empty_input"));
        let b = body(json!({"model": "m", "input": "[laughs] Ha."}));
        assert_eq!(code(refuse_speech(&b, None)), None);
        for blank in ["", " \n "] {
            let b = body(json!({"model": "m", "input": blank}));
            assert_eq!(
                code(refuse_speech(&b, None)),
                Some("empty_input"),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn a_voice_design_row_needs_a_description_somewhere() {
        let design = SpeechProfile {
            instructions: InstructionsMode::VoiceDesign,
            ..Default::default()
        };
        let bare = row(json!({}));
        let b = body(json!({"model": "m", "input": "Hi."}));
        assert_eq!(
            code(refuse_speech(&b, Some((&bare, &design)))),
            Some("instructions_required")
        );
        for ok in [
            json!({"model": "m", "input": "Hi.", "instructions": "a calm voice"}),
            json!({"model": "m", "input": "Hi.", "options": {"instruct": "a calm voice"}}),
        ] {
            assert_eq!(code(refuse_speech(&body(ok), Some((&bare, &design)))), None);
        }
        let defaulted = row(json!({"instruct": "a calm voice"}));
        assert_eq!(code(refuse_speech(&b, Some((&defaulted, &design)))), None);
        let blank = body(json!({"model": "m", "input": "Hi.", "instructions": " ",
                                "options": {"instruction": ""}}));
        assert_eq!(
            code(refuse_speech(&blank, Some((&bare, &design)))),
            Some("instructions_required"),
            "a blank one says nothing"
        );
    }

    /// What a caller that builds its own requests (a realtime response)
    /// checks before it opens a route: the description it will send, or the
    /// row's own.
    #[test]
    fn a_description_the_caller_will_send_or_the_rows_own_satisfies_it() {
        let design = SpeechProfile {
            instructions: InstructionsMode::VoiceDesign,
            ..Default::default()
        };
        let bare = row(json!({}));
        assert_eq!(
            code(refuse_undescribed(&bare, &design, None)),
            Some("instructions_required")
        );
        assert_eq!(
            code(refuse_undescribed(&bare, &design, Some("  "))),
            Some("instructions_required")
        );
        assert_eq!(
            code(refuse_undescribed(&bare, &design, Some("a calm voice"))),
            None
        );
        let defaulted = row(json!({"instruction": "a calm voice"}));
        assert_eq!(code(refuse_undescribed(&defaulted, &design, None)), None);
        // Any other row needs none.
        assert_eq!(
            code(refuse_undescribed(&bare, &SpeechProfile::default(), None)),
            None
        );
    }

    #[test]
    fn an_offline_row_cannot_stream() {
        let streams = SpeechProfile {
            streaming: true,
            ..Default::default()
        };
        let mut offline = row(json!({}));
        offline.task = "tts".into();
        for b in [
            json!({"model": "m", "input": "Hi.", "stream_format": "sse"}),
            json!({"model": "m", "input": "Hi.", "stream": true}),
        ] {
            assert_eq!(
                code(refuse_speech(&body(b), Some((&offline, &streams)))),
                Some("streaming_unsupported")
            );
        }
        let mut streaming = offline.clone();
        streaming.mode = "streaming".into();
        let b = body(json!({"model": "m", "input": "Hi.", "stream_format": "audio"}));
        assert_eq!(code(refuse_speech(&b, Some((&streaming, &streams)))), None);
    }

    #[test]
    fn a_row_whose_task_its_package_does_not_run_is_refused_first() {
        let variant = |v: &str| SpeechProfile {
            variant: Some(v.into()),
            instructions: InstructionsMode::VoiceDesign,
            ..Default::default()
        };
        let mut tts = row(json!({}));
        tts.task = "tts".into();
        let b = body(json!({"model": "m", "input": "Hi."}));
        assert_eq!(
            code(refuse_speech(&b, Some((&tts, &variant("voice_design"))))),
            Some("task_mismatch")
        );
        let vdes = row(json!({}));
        assert_eq!(
            code(refuse_speech(&b, Some((&vdes, &variant("voice_design"))))),
            Some("instructions_required"),
            "the right task: on to the next check"
        );
    }
}
