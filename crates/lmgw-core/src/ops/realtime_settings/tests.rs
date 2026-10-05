use serde_json::json;

use super::*;

fn patch(v: serde_json::Value) -> RealtimeSettingsPatch {
    serde_json::from_value(v).unwrap()
}

fn applied(v: serde_json::Value) -> Result<(RealtimeSettings, Vec<&'static str>), String> {
    let mut r = RealtimeSettings::default();
    let touched = patch(v).apply(&mut r)?;
    Ok((r, touched))
}

#[test]
fn every_field_is_settable_and_named_when_touched() {
    let (r, touched) = applied(json!({
        "default_model": " my-chat ",
        "model_map": {"gpt-realtime": "my-chat", " whisper-1 ": " my-asr "},
        "asr_alias": "my-asr",
        "tts_alias": "my-tts",
        "default_voice": "F5",
        "voice_map": {"alloy": "M5"},
        "speech_instructions": " calm, warm ",
        "tag_hint": false,
        "threshold": 0.6,
        "prefix_padding_ms": 200,
        "silence_duration_ms": 700,
        "semantic_vad_engine": "server_vad",
        "semantic_vad": {"high": {"threshold": 0.7}, "low": {"max_wait_ms": 5000}},
        "semantic_floor_window_ms": 400,
        "post_interrupt_silence_ms": 900,
        "barge_in_min_ms": 300,
        "barge_in_guard_ms": 250,
        "half_duplex": true,
        "echo_tail_ms": 400,
        "barge_in_check": "duration",
        "backchannel_words": [" mhm ", "", "ach so"],
        "barge_in_check_scripts": ["Latin", "Cyrillic"],
        "barge_in_check_timeout_ms": 0,
        "barge_in_check_alias": "check-asr",
        "output_lead_ms": 300,
        "synthesis_ahead_s": 0,
        "longest_pause_ms": 250,
        "warm_on_connect": false,
        "max_message_mb": 0,
        "max_frame_mb": 8,
        "ping_interval_s": 0,
    }))
    .unwrap();
    assert_eq!(r.default_model, "my-chat");
    assert_eq!(r.model_map["whisper-1"], "my-asr");
    assert_eq!(r.semantic_vad_engine, SemanticVadEngine::ServerVad);
    assert_eq!(r.semantic_vad.high.threshold, 0.7);
    let d = RealtimeSettings::default();
    assert_eq!(r.semantic_vad.high.floor, d.semantic_vad.high.floor);
    assert_eq!(r.semantic_vad.medium, d.semantic_vad.medium);
    assert_eq!(r.semantic_vad.low.max_wait_ms, 5000);
    assert_eq!(r.barge_in_check, BargeInCheck::Duration);
    assert_eq!(r.backchannel_words, ["mhm", "ach so"]);
    assert_eq!((r.max_message_mb, r.max_frame_mb), (0, 8));
    assert_eq!(r.longest_pause_ms, 250);
    assert!(!r.warm_on_connect && r.half_duplex);
    assert_eq!(r.speech_instructions, "calm, warm");
    assert!(!r.tag_hint);
    assert_eq!(touched.len(), 31, "{touched:?}");
    assert!(touched.contains(&"realtime.semantic_vad"));
    // Nothing named, nothing touched.
    assert_eq!(applied(json!({})).unwrap().1, Vec::<&str>::new());
}

#[test]
fn the_instructions_are_unset_empty_or_text() {
    let mut r = RealtimeSettings::default();
    patch(json!({"default_instructions": "Be brief."}))
        .apply(&mut r)
        .unwrap();
    assert_eq!(r.default_instructions.as_deref(), Some("Be brief."));
    assert_eq!(realtime_view(&r).default_instructions, "Be brief.");
    assert!(!realtime_view(&r).default_instructions_is_builtin);
    // Empty is "none", and kept as that.
    patch(json!({"default_instructions": "  "}))
        .apply(&mut r)
        .unwrap();
    assert_eq!(r.default_instructions.as_deref(), Some(""));
    assert_eq!(r.voice_instructions(), None);
    // The built-in text is "unset": not stored, so it follows releases.
    patch(json!({"default_instructions": format!("\n{DEFAULT_VOICE_INSTRUCTIONS} ")}))
        .apply(&mut r)
        .unwrap();
    assert_eq!(r.default_instructions, None);
    let v = realtime_view(&r);
    assert!(v.default_instructions_is_builtin);
    assert_eq!(v.default_instructions, DEFAULT_VOICE_INSTRUCTIONS);
    assert_eq!(v.default_instructions_builtin, DEFAULT_VOICE_INSTRUCTIONS);
}

/// WP10 D14: any text is a style or a description — no check, no length
/// cap — and the patch round-trips through the view.
#[test]
fn the_speech_style_and_the_tag_hint_round_trip() {
    let d = RealtimeSettings::default();
    assert_eq!(d.speech_instructions, "", "no built-in style");
    assert!(d.tag_hint, "the hint is on by default");
    let long = "an old sailor, hoarse and slow; ".repeat(400);
    let (r, touched) = applied(json!({"speech_instructions": long, "tag_hint": false})).unwrap();
    assert_eq!(r.speech_instructions, long.trim());
    assert_eq!(
        touched,
        ["realtime.speech_instructions", "realtime.tag_hint"]
    );
    let v = realtime_view(&r);
    assert_eq!(
        (v.speech_instructions.as_str(), v.tag_hint),
        (long.trim(), false)
    );
    let mut back = RealtimeSettings::default();
    patch(json!({"speech_instructions": v.speech_instructions, "tag_hint": v.tag_hint}))
        .apply(&mut back)
        .unwrap();
    assert_eq!(
        (back.speech_instructions, back.tag_hint),
        (r.speech_instructions, r.tag_hint)
    );
    // Empty is "none".
    let (r, _) = applied(json!({"speech_instructions": "  "})).unwrap();
    assert_eq!(r.speech_instructions, "");
}

#[test]
fn a_semantic_vad_row_that_cannot_run_is_refused_by_name() {
    let e = applied(json!({"semantic_vad": {"medium": {"floor": 0.9}}})).unwrap_err();
    assert!(e.starts_with("realtime.semantic_vad.medium:"), "{e}");
    assert!(e.contains("above its threshold"), "{e}");
    let e = applied(json!({"semantic_vad": {"high": {"threshold": 1.2}}})).unwrap_err();
    assert!(
        e.contains("realtime.semantic_vad.high") && e.contains("1.2"),
        "{e}"
    );
    // The floor window is judged against every row's maximum wait.
    let e = applied(json!({"semantic_floor_window_ms": 2500})).unwrap_err();
    assert!(e.contains("realtime.semantic_vad.high"), "{e}");
    assert!(e.contains("semantic_floor_window_ms 2500"), "{e}");
    // A stored row a hand edit broke does not refuse an unrelated save.
    let mut r = RealtimeSettings::default();
    r.semantic_vad.low.floor = 2.0;
    patch(json!({"warm_on_connect": false}))
        .apply(&mut r)
        .unwrap();
}

#[test]
fn both_websocket_limits_at_zero_are_refused() {
    let e = applied(json!({"max_message_mb": 0, "max_frame_mb": 0})).unwrap_err();
    assert!(e.contains("realtime.max_message_mb") && e.contains("realtime.max_frame_mb"));
    // One at a time against the stored other.
    let mut r = RealtimeSettings {
        max_frame_mb: 0,
        ..Default::default()
    };
    assert!(patch(json!({"max_message_mb": 0})).apply(&mut r).is_err());
    assert!(applied(json!({"max_message_mb": 0})).is_ok());
    assert!(applied(json!({"max_frame_mb": 0})).is_ok());
}

#[test]
fn names_and_ranges_are_checked() {
    for (v, field) in [
        (json!({"threshold": 1.5}), "realtime.threshold"),
        (json!({"threshold": -0.1}), "realtime.threshold"),
        (
            json!({"semantic_vad_engine": "silero"}),
            "realtime.semantic_vad_engine",
        ),
        (json!({"barge_in_check": "both"}), "realtime.barge_in_check"),
        (
            json!({"barge_in_check_scripts": ["Latin", "Klingon"]}),
            "Klingon",
        ),
        (json!({"model_map": {"": "x"}}), "realtime.model_map"),
        (json!({"voice_map": {"alloy": " "}}), "realtime.voice_map"),
    ] {
        let e = applied(v.clone()).unwrap_err();
        assert!(e.contains(field), "{v}: {e}");
    }
    // Every script, spelled as nothing.
    let (r, _) = applied(json!({"barge_in_check_scripts": []})).unwrap();
    assert!(r.barge_in_check_scripts.is_empty());
    // An unknown field is an error, not a silent no-op.
    assert!(serde_json::from_value::<RealtimeSettingsPatch>(json!({"max_mb": 1})).is_err());
    assert!(
        serde_json::from_value::<RealtimeSettingsPatch>(
            json!({"semantic_vad": {"auto": {"floor": 0.1}}})
        )
        .is_err(),
        "auto is medium, not a row of its own"
    );
}

#[test]
fn the_view_reads_back_what_was_stored() {
    let r = RealtimeSettings::default();
    let v = realtime_view(&r);
    assert_eq!(v.semantic_vad_engine, "smart_turn");
    assert_eq!(v.barge_in_check, "words");
    assert_eq!(v.semantic_vad.low.threshold, 0.95);
    assert_eq!((v.max_message_mb, v.max_frame_mb), (64, 16));
    assert_eq!(v.ping_interval_s, 20);
    assert!(v.warm_on_connect);
    // The view's words are the patch's words: a read-back patches cleanly.
    let mut round = RealtimeSettings::default();
    patch(json!({
        "semantic_vad_engine": v.semantic_vad_engine,
        "barge_in_check": v.barge_in_check,
        "default_instructions": v.default_instructions,
    }))
    .apply(&mut round)
    .unwrap();
    assert_eq!(round.default_instructions, None);
}
