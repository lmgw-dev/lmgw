//! The voice, language and instruction rules of [`super::shape_speech`],
//! per family.

use serde_json::{json, Map, Value};

use super::*;
use crate::audio::profile::VoiceField;
use crate::audio::voices::{row_voices, LibraryClip};

fn row(presets: Value, default: Value, defaults: Value) -> AudioModel {
    serde_json::from_value(json!({
        "id": 1, "model_id": "m", "family": "f", "path": "p", "task": "tts",
        "mode": "offline", "load_options": {}, "session_options": {},
        "default_request_options": defaults,
        "voice_presets": presets, "default_voice_preset": default, "enabled": true,
        "image": null, "extra_run_args": null, "warm_start": false
    }))
    .unwrap()
}

fn magpie() -> SpeechProfile {
    SpeechProfile {
        family: "magpie_tts".into(),
        native_voices: vec!["Aria".into(), "Jason".into(), "Sofia".into()],
        voice_field: VoiceField::OptionsVoiceId,
        default_voice: Some("Aria".into()),
        ..Default::default()
    }
}

fn shape(profile: &SpeechProfile, row: &AudioModel, body: Value) -> (Value, ShapeReport) {
    shape_with(profile, row, body, None)
}

fn shape_with(
    profile: &SpeechProfile,
    row: &AudioModel,
    body: Value,
    library: Option<&[LibraryClip]>,
) -> (Value, ShapeReport) {
    let voices = row_voices(profile, row, &[], library);
    let mut obj: Map<String, Value> = body.as_object().unwrap().clone();
    let report = shape_speech(profile, &voices, row, &mut obj);
    (Value::Object(obj), report)
}

fn plain() -> AudioModel {
    row(json!({}), Value::Null, json!({}))
}

#[test]
fn magpie_natives_go_to_options_voice_id_in_the_packages_spelling() {
    let (b, r) = shape(
        &magpie(),
        &plain(),
        json!({"input": "hi", "voice": "jason"}),
    );
    assert_eq!(b, json!({"input": "hi", "options": {"voice_id": "Jason"}}));
    assert_eq!(
        r.header_value().unwrap().to_str().unwrap(),
        "voice=options.voice_id"
    );

    // The client's own option wins; its `voice` is left to the engine.
    let body = json!({"input": "hi", "voice": "Jason", "options": {"voice_id": "Sofia"}});
    let (b, r) = shape(&magpie(), &plain(), body.clone());
    assert_eq!(b, body);
    assert!(r.is_empty());

    // Not a native voice: left as it came.
    let body = json!({"input": "hi", "voice": "alloy"});
    assert_eq!(shape(&magpie(), &plain(), body.clone()).0, body);
}

#[test]
fn a_library_clip_or_a_preset_of_that_name_wins_as_in_the_engine() {
    let clips = [LibraryClip {
        voice: "Jason".into(),
        transcript: true,
    }];
    let body = json!({"input": "hi", "voice": "Jason"});
    let (b, r) = shape_with(&magpie(), &plain(), body.clone(), Some(&clips));
    assert_eq!(b, body, "the clip clones");
    assert!(r.is_empty());

    // A preset named like the voice it carries: the preset speaks, and its
    // voice id is what Magpie reads.
    let presets = row(
        json!({"narrator": {"voice_id": "sofia"}}),
        Value::Null,
        json!({}),
    );
    let (b, _) = shape(
        &magpie(),
        &presets,
        json!({"input": "hi", "voice": "narrator"}),
    );
    assert_eq!(
        b,
        json!({"input": "hi", "voice": "narrator", "options": {"voice_id": "Sofia"}})
    );
}

#[test]
fn the_rows_default_preset_reaches_magpie_too_unless_the_row_sets_the_option() {
    let inline = row(json!({}), json!({"voice_id": "Jason"}), json!({}));
    let (b, r) = shape(&magpie(), &inline, json!({"input": "hi"}));
    assert_eq!(b, json!({"input": "hi", "options": {"voice_id": "Jason"}}));
    assert_eq!(
        r.changes,
        [ShapeChange::PresetToOptions {
            name: "Jason".into()
        }]
    );

    let bare = row(json!({}), json!("Sofia"), json!({}));
    assert_eq!(
        shape(&magpie(), &bare, json!({"input": "hi"})).0["options"]["voice_id"],
        "Sofia"
    );

    let owned = row(
        json!({}),
        json!({"voice_id": "Jason"}),
        json!({"voice_id": "Aria"}),
    );
    let body = json!({"input": "hi"});
    assert_eq!(shape(&magpie(), &owned, body.clone()).0, body);
    let cloning = json!({"input": "hi", "voice_ref": "/models/voices/x.wav"});
    assert_eq!(shape(&magpie(), &inline, cloning.clone()).0, cloning);
}

#[test]
fn voice_families_get_the_packages_spelling_in_voice() {
    let qwen3 = SpeechProfile {
        native_voices: vec!["ryan".into(), "serena".into()],
        ..Default::default()
    };
    let (b, r) = shape(&qwen3, &plain(), json!({"input": "hi", "voice": "Ryan"}));
    assert_eq!(b["voice"], "ryan");
    assert_eq!(r.header_value().unwrap().to_str().unwrap(), "voice=ryan");
    let (b, r) = shape(&qwen3, &plain(), json!({"input": "hi", "voice": "ryan"}));
    assert_eq!(b["voice"], "ryan");
    assert!(r.is_empty());
}

#[test]
fn a_native_whose_file_is_missing_is_not_shaped() {
    let pocket = SpeechProfile {
        native_voices: vec!["alba".into()],
        file_backed: vec!["alba".into()],
        ..Default::default()
    };
    let (b, r) = shape(&pocket, &plain(), json!({"input": "hi", "voice": "Alba"}));
    assert_eq!(b["voice"], "Alba");
    assert!(r.is_empty());
}

#[test]
fn the_language_goes_in_the_familys_vocabulary() {
    use crate::audio::profile::{LanguageVocab, VocabSource};
    let qwen3 = SpeechProfile {
        family: "qwen3_tts".into(),
        language_vocab: LanguageVocab {
            source: VocabSource::Gguf,
            entries: vec!["english".into(), "german".into(), "auto".into()],
        },
        ..Default::default()
    };
    let (b, r) = shape(
        &qwen3,
        &plain(),
        json!({"input": "Hallo.", "language": "de"}),
    );
    assert_eq!(b["language"], "german");
    assert_eq!(
        r.header_value().unwrap().to_str().unwrap(),
        "language=de->german"
    );
    // Unknown, or already the vocabulary's: sent as it came.
    let body = json!({"input": "x", "language": "ja"});
    assert_eq!(shape(&qwen3, &plain(), body.clone()).0, body);
    let body = json!({"input": "x", "language": "german"});
    assert!(shape(&qwen3, &plain(), body).1.is_empty());
    // A family without a vocabulary: untouched.
    let body = json!({"input": "x", "language": "de"});
    assert_eq!(shape(&magpie(), &plain(), body.clone()).0, body);
}

fn designer() -> SpeechProfile {
    SpeechProfile {
        family: "qwen3_tts".into(),
        instructions: InstructionsMode::VoiceDesign,
        ..Default::default()
    }
}

/// The request's `instruction` (from `instructions`) over a row whose
/// defaults describe the voice under `instruct`: audio.cpp would merge the
/// row's in, and Qwen3 refuse the two as conflicting — so the text goes
/// under `instruct` too (R2).
#[test]
fn a_description_over_the_rows_instruct_goes_under_both_keys() {
    let described = row(
        json!({}),
        Value::Null,
        json!({"instruct": "a deep narrator"}),
    );
    let (b, r) = shape(
        &designer(),
        &described,
        json!({"input": "hi", "instructions": "an old sailor"}),
    );
    assert_eq!(
        b,
        json!({"input": "hi", "instructions": "an old sailor",
               "options": {"instruct": "an old sailor"}})
    );
    assert_eq!(
        r.header_value().unwrap().to_str().unwrap(),
        "instructions=also:options.instruct"
    );
    // A client's `options.instruction` is the same key to audio.cpp.
    let (b, _) = shape(
        &designer(),
        &described,
        json!({"input": "hi", "options": {"instruction": "an old sailor"}}),
    );
    assert_eq!(b["options"]["instruct"], "an old sailor");
    // Nothing to reconcile: the row's own text, a key the client set
    // itself, nothing sent, or a row with no description there.
    for body in [
        json!({"input": "hi", "instructions": "a deep narrator"}),
        json!({"input": "hi", "instructions": "a sailor", "options": {"instruct": "a sailor"}}),
        json!({"input": "hi", "instructions": ""}),
        json!({"input": "hi"}),
    ] {
        let (b, r) = shape(&designer(), &described, body.clone());
        assert_eq!((b, r.is_empty()), (body, true));
    }
    for defaults in [
        json!({}),
        json!({"instruct": ""}),
        json!({"instruction": "x"}),
    ] {
        let body = json!({"input": "hi", "instructions": "an old sailor"});
        let (b, r) = shape(
            &designer(),
            &row(json!({}), Value::Null, defaults),
            body.clone(),
        );
        assert_eq!((b, r.is_empty()), (body, true));
    }
}

/// The other way round: a family that reads `instruct` (its instructions
/// moved there) on a row whose defaults say `instruction`.
#[test]
fn an_instruct_over_the_rows_instruction_goes_under_both_keys() {
    let moss = SpeechProfile {
        family: "moss_tts_v15".into(),
        instructions: InstructionsMode::Passthrough,
        instructions_field: InstructionsField::OptionsInstruct,
        ..Default::default()
    };
    let styled = row(json!({}), Value::Null, json!({"instruction": "calm"}));
    let (b, r) = shape(
        &moss,
        &styled,
        json!({"input": "hi", "instructions": "excited"}),
    );
    assert_eq!(
        b,
        json!({"input": "hi", "options": {"instruct": "excited", "instruction": "excited"}})
    );
    assert_eq!(
        r.header_value().unwrap().to_str().unwrap(),
        "instructions=options.instruct; instructions=also:options.instruction"
    );
    // The client's own `options.instruct`, with no `instructions`.
    let (b, r) = shape(
        &moss,
        &styled,
        json!({"input": "hi", "options": {"instruct": "excited"}}),
    );
    assert_eq!(b["options"]["instruction"], "excited");
    assert_eq!(
        r.changes,
        [ShapeChange::InstructionsOverRow { key: "instruction" }]
    );
}
