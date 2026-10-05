//! A clause's leading cue, how it joins the style, and the routes that take
//! one (WP9b C1, C2, C4, C5).

use serde_json::{json, Value};

use super::*;
use crate::audio::profile::InstructionsMode;
use crate::audio::tags::TagMode;

/// The cue of `input` and the text from its words on.
fn led(input: &str) -> Option<(String, &str)> {
    lead(input).map(|(cue, at)| (cue, &input[at..]))
}

fn row(defaults: Value) -> AudioModel {
    serde_json::from_value(json!({
        "id": 1, "model_id": "m", "family": "qwen3_tts", "path": "p", "task": "tts",
        "mode": "offline", "load_options": {}, "session_options": {},
        "default_request_options": defaults,
        "voice_presets": {}, "default_voice_preset": null, "enabled": true,
        "image": null, "extra_run_args": null, "warm_start": false
    }))
    .unwrap()
}

fn rules(instructions: InstructionsMode, tags: TagMode, vocab: &[&str]) -> Expressive {
    Expressive {
        instructions,
        tags,
        vocab: vocab.iter().map(|t| t.to_string()).collect(),
        ..Expressive::default()
    }
}

#[test]
fn every_tag_that_opens_a_clause_is_its_cue() {
    let cue = |c: &str, rest: &'static str| Some((c.to_string(), rest));
    assert_eq!(led("[laughing] Oh no."), cue("laughing", "Oh no."));
    // Several join, and what punctuated them goes with them.
    assert_eq!(
        led("[excited] [laughing], ja."),
        cue("excited, laughing", "ja.")
    );
    assert_eq!(led("[quick_breath] Wait."), cue("quick breath", "Wait."));
    assert_eq!(led("[half-whispered] so."), cue("half whispered", "so."));
    // A tag inside the clause is none; nor is a bracket the grammar does
    // not take, or a link's text.
    for input in [
        "Oh [laughing] no.",
        "[1] Quelle",
        "[S1] Hallo.",
        "[Note] this.",
        "[flüsternd] Hallo.",
        "[see](u) x",
        "Plain.",
        "",
    ] {
        assert_eq!(lead(input), None, "{input:?}");
    }
    // The grammar's 31 bytes: a longer bracket is text, read out.
    let long = format!("[{}] x", "a".repeat(32));
    assert_eq!(lead(&long), None);
    // A link after a cue stays text.
    assert_eq!(led("[laughing] [see](u) x"), cue("laughing", "[see](u) x"));
}

#[test]
fn a_cue_goes_after_the_style() {
    assert_eq!(combine(None, "laughing"), "laughing");
    // A contrast, not one more adjective: Qwen3 CustomVoice muted a laugh
    // sent as "calm, warm; laughing".
    assert_eq!(
        combine(Some("calm, warm"), "laughing"),
        "calm, warm, but laughing right now"
    );
    assert_eq!(combine(Some("  "), "laughing"), "laughing", "blank is none");
    // The base's closing punctuation does not stand before the cue's.
    assert_eq!(
        combine(Some("Speak calmly and warmly. "), "laughing"),
        "Speak calmly and warmly, but laughing right now"
    );
    assert_eq!(
        combine(Some("calm;"), "laughing"),
        "calm, but laughing right now"
    );
    assert_eq!(
        combine(Some("Be warm!"), "excited"),
        "Be warm, but excited right now"
    );
    assert_eq!(
        combine(Some("温柔地说。"), "laughing"),
        "温柔地说, but laughing right now"
    );
    assert_eq!(combine(Some(" . "), "laughing"), "laughing", "nothing left");
    // What closes a parenthesis or a quote stays.
    assert_eq!(
        combine(Some("a narrator (calm)"), "laughing"),
        "a narrator (calm), but laughing right now"
    );
}

#[test]
fn a_row_s_description_is_its_instruction_else_its_instruct() {
    assert_eq!(
        row_description(&row(json!({"instruct": " a narrator "}))),
        Some("a narrator".to_string())
    );
    assert_eq!(
        row_description(&row(json!({"instruction": "", "instruct": "calm"}))),
        Some("calm".to_string())
    );
    assert_eq!(
        row_description(&row(json!({"instruction": "warm", "instruct": "calm"}))),
        Some("warm".to_string())
    );
    assert_eq!(row_description(&row(json!({"seed": 5}))), None);
}

#[test]
fn a_style_or_passthrough_route_without_tags_takes_cues() {
    let (style, pass) = (InstructionsMode::Style, InstructionsMode::Passthrough);
    assert!(takes(&rules(style, TagMode::None, &[])), "CustomVoice");
    assert!(takes(&rules(pass, TagMode::None, &[])), "Auk");
    // A cloud TTS is passthrough by assumption only: it takes none until
    // its alias declares what its instructions do.
    assert!(
        !takes(&Expressive::remote()),
        "a cloud TTS nobody described"
    );
    let declared = |i: &str| Expressive::from_override(Some(&json!({"instructions": i})));
    assert!(takes(&declared("style")), "a cloud alias declared a style");
    assert!(takes(&declared("passthrough")));
    assert!(
        !takes(&Expressive::from_override(Some(
            &json!({"inline_tags": "none"})
        ))),
        "tags declared, instructions still assumed"
    );
    assert!(takes(&rules(pass, TagMode::Fixed, &[])), "no tag listed");
    assert!(
        !takes(&rules(pass, TagMode::Fixed, &["laughter"])),
        "OmniVoice"
    );
    assert!(!takes(&rules(style, TagMode::Free, &[])));
    let design = InstructionsMode::VoiceDesign;
    assert!(!takes(&rules(design, TagMode::None, &[])));
    assert!(
        !takes(&rules(InstructionsMode::None, TagMode::None, &[])),
        "Kokoro"
    );
}

#[test]
fn a_cue_is_folded_only_on_a_route_that_takes_one() {
    let custom = rules(InstructionsMode::Style, TagMode::None, &[]);
    let input = "[laughing] Oh no,";
    // The style in effect, the cue after it; the text from its words on.
    assert_eq!(
        fold(&custom, None, input, Some("calm"), Some("laughing")),
        Some(Folded {
            input: "Oh no,",
            instructions: "calm, but laughing right now".into(),
        })
    );
    // No style sent: the row's own description is the base, so the cue
    // does not replace it.
    let narrator = row(json!({"instruct": "a narrator"}));
    assert_eq!(
        fold(&custom, Some(&narrator), input, None, Some("laughing")).map(|f| f.instructions),
        Some("a narrator, but laughing right now".into())
    );
    assert_eq!(
        fold(&custom, Some(&narrator), input, Some(" "), Some("laughing")).map(|f| f.instructions),
        Some("a narrator, but laughing right now".into()),
        "a blank style is none"
    );
    // A clause in a cue's scope that opens with no tag of its own.
    assert_eq!(
        fold(&custom, None, "that is funny.", None, Some("laughing")),
        Some(Folded {
            input: "that is funny.",
            instructions: "laughing".into(),
        })
    );
    // No cue, or a route that takes none: as it came.
    assert_eq!(fold(&custom, None, input, Some("calm"), None), None);
    let omni = rules(InstructionsMode::Passthrough, TagMode::Fixed, &["laughter"]);
    assert_eq!(fold(&omni, None, input, None, Some("laughing")), None);
    let design = rules(InstructionsMode::VoiceDesign, TagMode::None, &[]);
    assert_eq!(
        fold(&design, None, input, Some("a sailor"), Some("laughing")),
        None
    );
    // A cloud alias nobody described, or one the owner marked as reading
    // no instructions: no cue, and shaping strips the tag. One declared a
    // style takes it.
    let deaf = Expressive::from_override(Some(&json!({"instructions": "none"})));
    assert_eq!(fold(&deaf, None, input, None, Some("laughing")), None);
    let undescribed = Expressive::from_override(None);
    assert_eq!(
        fold(&undescribed, None, input, Some("calm"), Some("laughing")),
        None
    );
    let styled = Expressive::from_override(Some(&json!({"instructions": "style"})));
    assert!(fold(&styled, None, input, None, Some("laughing")).is_some());
}
