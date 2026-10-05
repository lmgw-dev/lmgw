//! The style a response speaks with, the seed and the hint (WP10 D4–D7).

use lmgw_api_types::realtime::{
    cue_hint_text, speech_hint_text, tag_hint_text, takes_cues, HINT_SOUNDS,
};

use super::*;
use crate::audio::tags::STAGE_DIRECTIONS;

fn facts(mode: InstructionsMode, row: Option<&str>) -> SpeechFacts {
    SpeechFacts {
        mode,
        row_description: row.map(str::to_string),
        ..SpeechFacts::default()
    }
}

fn asked<'a>(response: Option<&'a str>, session: Option<&'a str>, setting: &'a str) -> Asked<'a> {
    Asked {
        response,
        session,
        setting,
    }
}

/// `(send, text, source, dropped)` of a resolution.
fn style(
    f: &SpeechFacts,
    a: Asked<'_>,
) -> (Option<String>, Option<String>, Option<&'static str>, bool) {
    let s = resolve_style(f, &a);
    (s.send, s.text, s.source.map(Source::as_str), s.dropped)
}

fn some(t: &str) -> Option<String> {
    Some(t.to_string())
}

#[test]
fn the_response_wins_then_the_session_then_the_setting_then_the_row() {
    let custom = facts(InstructionsMode::Style, None);
    let table = [
        (
            asked(Some("whisper"), Some("excited"), "calm"),
            (some("whisper"), some("whisper"), Some("response"), false),
        ),
        (
            asked(None, Some("excited"), "calm"),
            (some("excited"), some("excited"), Some("session"), false),
        ),
        (
            asked(None, None, "calm"),
            (some("calm"), some("calm"), Some("setting"), false),
        ),
        // `""` is "none": the levels below do not fill in.
        (
            asked(Some(""), Some("excited"), "calm"),
            (None, None, None, false),
        ),
        (asked(None, Some("  "), "calm"), (None, None, None, false)),
        (asked(None, None, ""), (None, None, None, false)),
    ];
    for (a, want) in table {
        assert_eq!(style(&custom, a), want, "{a:?}");
    }
    // Text is trimmed.
    assert_eq!(
        style(&custom, asked(None, Some(" calm "), "")).0,
        some("calm")
    );
}

#[test]
fn a_row_s_own_description_holds_where_lmgw_sends_none() {
    let design = facts(InstructionsMode::VoiceDesign, Some("a deep narrator"));
    // The owner's style stands back: a style is not a voice.
    assert_eq!(
        style(&design, asked(None, None, "cheerful")),
        (None, some("a deep narrator"), Some("row"), false)
    );
    // A client's description replaces the row's: it is sent, and shaping
    // writes it under the row's key too (`audio::shape`, R2), so the engine
    // never sees the two side by side.
    assert_eq!(
        style(&design, asked(None, Some("an old sailor"), "cheerful")),
        (
            some("an old sailor"),
            some("an old sailor"),
            Some("session"),
            false
        )
    );
    // `""` sends nothing; the row's description is what the engine uses.
    assert_eq!(
        style(&design, asked(None, Some(""), "")),
        (None, some("a deep narrator"), Some("row"), false)
    );
    // Without one of its own, the owner's text is the description.
    let bare = facts(InstructionsMode::VoiceDesign, None);
    assert_eq!(
        style(&bare, asked(None, None, "a calm voice")),
        (
            some("a calm voice"),
            some("a calm voice"),
            Some("setting"),
            false
        )
    );
    assert_eq!(
        style(&bare, asked(None, None, "")),
        (None, None, None, false)
    );
    // A style row's description is a style, and the setting goes over it.
    let custom = facts(InstructionsMode::Style, Some("calm"));
    assert_eq!(style(&custom, asked(None, None, "")).2, Some("row"));
    assert_eq!(
        style(&custom, asked(None, None, "excited")).2,
        Some("setting")
    );
}

#[test]
fn a_model_that_reads_none_has_the_text_dropped() {
    let kokoro = facts(InstructionsMode::None, None);
    assert_eq!(
        style(&kokoro, asked(None, Some("cheerful"), "")),
        (some("cheerful"), some("cheerful"), Some("session"), true)
    );
    assert_eq!(
        style(&kokoro, asked(None, None, "")),
        (None, None, None, false)
    );
}

#[test]
fn the_seed_goes_where_the_row_reads_one() {
    let drawn = SessionSeed {
        value: 7,
        pinned: false,
    };
    let pinned = SessionSeed {
        value: 42,
        pinned: true,
    };
    let design = SpeechFacts {
        mode: InstructionsMode::VoiceDesign,
        reads_seed: true,
        ..SpeechFacts::default()
    };
    assert_eq!(
        seed_in_effect(&design, drawn),
        Some(7),
        "a designed voice keeps"
    );
    assert_eq!(seed_in_effect(&design, pinned), Some(42));
    // The owner's per-row pin wins over a drawn seed, not over the client's.
    let row_pinned = SpeechFacts {
        row_seed: true,
        ..design.clone()
    };
    assert_eq!(seed_in_effect(&row_pinned, drawn), None);
    assert_eq!(seed_in_effect(&row_pinned, pinned), Some(42));
    // OmniVoice draws its speaker from its sampler when it is named none
    // (R4 M1): its voice comes from the seed too.
    let omni = SpeechFacts {
        mode: InstructionsMode::Passthrough,
        reads_seed: true,
        draws_speaker: true,
        ..SpeechFacts::default()
    };
    assert_eq!(
        seed_in_effect(&omni, drawn),
        Some(7),
        "a drawn speaker keeps"
    );
    let omni_pinned = SpeechFacts {
        row_seed: true,
        ..omni.clone()
    };
    assert_eq!(seed_in_effect(&omni_pinned, drawn), None);
    assert_eq!(seed_in_effect(&omni_pinned, pinned), Some(42));
    // A style row gets one only when the client pins it.
    let custom = SpeechFacts {
        mode: InstructionsMode::Style,
        reads_seed: true,
        ..SpeechFacts::default()
    };
    assert_eq!(seed_in_effect(&custom, drawn), None);
    assert_eq!(seed_in_effect(&custom, pinned), Some(42));
    // A remote alias reads none.
    let remote = SpeechFacts::of_rules(&Expressive::remote());
    assert_eq!(remote.mode, InstructionsMode::Passthrough);
    assert_eq!(seed_in_effect(&remote, pinned), None);
}

#[test]
fn the_hint_names_the_sounds_a_tts_renders() {
    let omni: Vec<String> = crate::audio::families::tag_vocabulary("omnivoice")
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    let text = tag_hint_text("fixed", &omni).unwrap();
    assert!(
        text.contains("can also make these sounds: [laughter] [sigh]."),
        "{text}"
    );
    assert!(
        !text.contains("question-en"),
        "no token a model cannot place"
    );
    let cosy: Vec<String> = crate::audio::families::tag_vocabulary("cosyvoice3")
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(tag_hint_text("fixed", &cosy)
        .unwrap()
        .contains("[breath] [laughter] [cough] [quick_breath] [sigh] [lipsmack]."));
    // A vocabulary with none of the sounds is named whole.
    let odd = vec!["mn".to_string()];
    assert!(tag_hint_text("fixed", &odd).unwrap().contains("[mn]."));
    let free = tag_hint_text("free", &[]).unwrap();
    assert!(free.contains("such as [laughs] or [whispers]"), "{free}");
    assert_eq!(tag_hint_text("none", &omni), None);
    assert_eq!(tag_hint_text("fixed", &[]), None);
    // Off, or a TTS that renders none: no hint.
    let f = SpeechFacts {
        tags: TagMode::Fixed,
        vocab: omni,
        ..SpeechFacts::default()
    };
    assert!(hint(&f, true).is_some());
    assert_eq!(hint(&f, false), None);
    assert_eq!(hint(&SpeechFacts::default(), true), None);
}

fn omnivoice_vocab() -> Vec<String> {
    crate::audio::families::tag_vocabulary("omnivoice")
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect()
}

/// WP9b C1: a `style` or `passthrough` TTS that renders no tags takes cues;
/// one that renders tags takes those instead, never both. One whose
/// instructions nobody declared — a cloud alias described no further —
/// takes none.
#[test]
fn a_tts_takes_tags_or_cues_never_both() {
    let omni = omnivoice_vocab();
    assert!(takes_cues(Some("style"), "none", &[]), "CustomVoice");
    assert!(takes_cues(Some("passthrough"), "none", &[]), "Auk");
    assert!(!takes_cues(None, "none", &[]), "a cloud TTS");
    assert!(
        takes_cues(Some("passthrough"), "fixed", &[]),
        "no tag listed"
    );
    assert!(
        !takes_cues(Some("passthrough"), "fixed", &omni),
        "OmniVoice"
    );
    assert!(!takes_cues(Some("voice_design"), "none", &[]));
    assert!(!takes_cues(Some("none"), "none", &[]), "Kokoro");
    assert!(!takes_cues(Some("style"), "free", &[]));
}

/// WP9b C6: the prompt is told about the sounds a TTS makes, else about
/// the cues it takes, else nothing.
#[test]
fn the_hint_is_the_tags_or_the_cues() {
    let omni = omnivoice_vocab();
    assert_eq!(
        speech_hint_text(Some("passthrough"), "fixed", &omni),
        tag_hint_text("fixed", &omni)
    );
    let cues = speech_hint_text(Some("style"), "none", &[]).unwrap();
    assert_eq!(cues, cue_hint_text());
    assert!(
        cues.contains("such as [laughing], [whispering] or [excited]")
            && cues.contains("one or two lowercase English words")
            && cues.contains("until that sentence ends"),
        "{cues}"
    );
    assert_eq!(speech_hint_text(Some("voice_design"), "none", &[]), None);
    assert_eq!(speech_hint_text(Some("none"), "none", &[]), None, "Kokoro");
    assert_eq!(speech_hint_text(None, "none", &[]), None, "a cloud TTS");
    let custom = SpeechFacts {
        mode: InstructionsMode::Style,
        ..SpeechFacts::default()
    };
    assert_eq!(hint(&custom, true), Some(cue_hint_text()));
    assert_eq!(hint(&custom, false), None);
}

/// WP9b C7: the echo's `cues` is whether the primary takes them; the hint
/// text stays in `tag_hint`.
#[test]
fn the_echo_says_whether_the_tts_takes_cues() {
    let settings = RealtimeSettings::default();
    let s = crate::realtime::merge::initial_session(&settings, "sess_1".into(), None);
    let omni = SpeechFacts {
        mode: InstructionsMode::Passthrough,
        tags: TagMode::Fixed,
        vocab: omnivoice_vocab(),
        ..SpeechFacts::default()
    };
    let rows = [
        ("CustomVoice", facts(InstructionsMode::Style, None), true),
        (
            "a cloud TTS",
            SpeechFacts::of_rules(&Expressive::remote()),
            false,
        ),
        (
            "a cloud TTS declared a style",
            SpeechFacts::of_rules(&Expressive::from_override(Some(
                &serde_json::json!({"instructions": "style"}),
            ))),
            true,
        ),
        ("OmniVoice", omni, false),
        (
            "VoiceDesign",
            facts(InstructionsMode::VoiceDesign, Some("a narrator")),
            false,
        ),
        ("Kokoro", facts(InstructionsMode::None, None), false),
    ];
    for (name, f, cues) in rows {
        let r = resolved(&f, &s, &settings, 7);
        assert_eq!(r.cues, cues, "{name}");
        let text = r.tag_hint.as_deref();
        assert_eq!(text == Some(cue_hint_text().as_str()), cues, "{name}");
    }
}

/// The dashboard's preview and the gateway name the same sounds, and those
/// are every sound a stage direction maps onto — but `pause`, which is no
/// sound.
#[test]
fn the_hint_s_sounds_are_the_stage_directions_targets() {
    let mut targets: Vec<&str> = STAGE_DIRECTIONS
        .iter()
        .map(|(_, sound)| *sound)
        .filter(|s| *s != "pause")
        .collect();
    targets.sort_unstable();
    targets.dedup();
    let mut sounds = HINT_SOUNDS.to_vec();
    sounds.sort_unstable();
    assert_eq!(sounds, targets);
}

#[test]
fn shaping_losses_name_what_was_dropped_or_stripped() {
    let report = ShapeReport {
        changes: vec![
            ShapeChange::InstructionsDropped,
            ShapeChange::Tags {
                mapped: 1,
                stripped: 2,
            },
        ],
    };
    assert_eq!(shaping_losses(&report, false).len(), 2);
    // The resolution said the drop already.
    assert_eq!(
        shaping_losses(&report, true),
        ["2 inline tag(s) were stripped (the model does not render them)"]
    );
    let mapped = ShapeReport {
        changes: vec![ShapeChange::Tags {
            mapped: 1,
            stripped: 0,
        }],
    };
    assert!(
        shaping_losses(&mapped, false).is_empty(),
        "a mapped tag is heard"
    );
}
