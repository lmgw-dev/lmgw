use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::audio::voices::{row_voices, LibraryClip};

fn row(defaults: Value, presets: Value) -> AudioModel {
    serde_json::from_value(json!({
        "id": 1, "model_id": "omni", "family": "omnivoice", "path": "p", "task": "tts",
        "mode": "offline", "load_options": {}, "session_options": {},
        "default_request_options": defaults,
        "voice_presets": presets, "default_voice_preset": null, "enabled": true, "image": null,
        "extra_run_args": null, "warm_start": false
    }))
    .unwrap()
}

fn speech(row: &AudioModel, requires: bool) -> RowSpeech {
    let profile = SpeechProfile {
        family: "omnivoice".into(),
        requires_reference_text: requires,
        needs_reference_text: requires,
        ..SpeechProfile::default()
    };
    let library = [
        LibraryClip {
            voice: "anna".into(),
            transcript: false,
        },
        LibraryClip {
            voice: "bert".into(),
            transcript: true,
        },
        LibraryClip {
            voice: "narrator".into(),
            transcript: false,
        },
    ];
    let voices = row_voices(&profile, row, &[], Some(&library));
    RowSpeech {
        profile: Arc::new(profile),
        voices,
    }
}

fn body(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn only_an_untranscribed_library_clip_of_an_engine_that_refuses_one_is_listed() {
    // `narrator` is a preset of the row: audio.cpp takes the preset first.
    let r = row(json!({}), json!({"narrator": {"voice_id": "x"}}));
    let s = speech(&r, true);
    assert_eq!(untranscribed(&r, &s.profile, &s.voices), ["anna"]);
    // An engine that speaks without one refuses nothing.
    let s = speech(&r, false);
    assert!(untranscribed(&r, &s.profile, &s.voices).is_empty());
    // A transcript in the row's defaults reaches every request.
    let r = row(json!({"reference_text": "Hello there."}), json!({}));
    let s = speech(&r, true);
    assert!(untranscribed(&r, &s.profile, &s.voices).is_empty());
}

#[test]
fn a_route_opened_for_the_clip_is_refused_naming_it_and_the_fix() {
    let r = row(json!({}), json!({}));
    let s = speech(&r, true);
    let e = refuse_clip(&r, &s, Some("anna")).unwrap_err();
    assert_eq!(e.code(), "voice_needs_transcript");
    assert_eq!(e.http_status(), axum::http::StatusCode::BAD_REQUEST);
    let m = e.to_string();
    assert!(m.contains("'anna'") && m.contains("'omni'"), "{m}");
    assert!(m.contains("transcribe it in the Audio lab"), "{m}");
    assert!(refuse_clip(&r, &s, Some("bert")).is_ok(), "transcribed");
    assert!(refuse_clip(&r, &s, None).is_ok(), "no voice named");
    assert!(refuse_clip(&r, &s, Some("ryan")).is_ok(), "no clip");
}

#[test]
fn a_speech_body_with_a_transcript_or_a_voice_ref_of_its_own_passes() {
    let r = row(json!({}), json!({}));
    let s = speech(&r, true);
    assert!(refuse_body(&r, &s, &body(json!({"voice": "anna"}))).is_err());
    for ok in [
        json!({"voice": "anna", "reference_text": "Hello there."}),
        json!({"voice": "anna", "options": {"reference_text": "Hello there."}}),
        json!({"voice": "anna", "voice_ref": "/voices/anna.wav"}),
        json!({"voice": "bert"}),
    ] {
        assert!(refuse_body(&r, &s, &body(ok.clone())).is_ok(), "{ok}");
    }
    // A blank transcript is none.
    assert!(refuse_body(
        &r,
        &s,
        &body(json!({"voice": "anna", "reference_text": "  "}))
    )
    .is_err());
}
