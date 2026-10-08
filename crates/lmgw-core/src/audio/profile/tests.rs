//! The profile's extractors against synthetic packages shaped like the real
//! ones (read off the owner's GGUFs: Supertonic-3, Qwen3-TTS 1.7B
//! CustomVoice/VoiceDesign, MagpieTTS, Kokoro, Pocket TTS).

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::audio::parse_spec;
use crate::gguf::synth;

fn row(family: &str, path: &str) -> AudioModel {
    serde_json::from_value(json!({
        "id": 1, "model_id": "m", "family": family, "path": path, "task": "tts",
        "mode": "offline", "load_options": {}, "session_options": {},
        "voice_presets": {}, "default_voice_preset": null, "enabled": true,
        "image": null, "extra_run_args": null, "warm_start": false
    }))
    .unwrap()
}

fn write(dir: &Path, name: &str, h: &synth::Header) -> PathBuf {
    let p = dir.join(name);
    h.write_to(&p);
    p
}

fn qwen3_config(model_type: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "tts_model_type": model_type,
        "talker_config": {
            "spk_id": {"serena": 3066, "ryan": 3061, "aiden": 2861},
            "codec_language_id": {"chinese": 2055, "english": 2050, "german": 2053}
        }
    }))
    .unwrap()
}

#[test]
fn supertonic_voices_come_from_the_embedded_voice_styles() {
    let d = tempfile::tempdir().unwrap();
    let spec = json!({
        "family": "supertonic", "modes": ["offline", "streaming"],
        "languages": ["en", "de"],
        "sources": [{"files": {
            "voice_style_M1": "model:voice_styles/M1.json",
            "voice_style_F1": "model:voice_styles/F1.json",
            "config": "model:config.json",
            "unicode_indexer": "model:config/unicode_indexer.json"
        }}]
    });
    let g = write(
        d.path(),
        "supertonic-3-q8_0.gguf",
        &synth::audiocpp(
            "supertonic",
            &spec.to_string(),
            &[
                ("README.md", b"x"),
                (
                    "config/unicode_indexer.json",
                    include_bytes!(
                        "../../../tests/fixtures/audio/supertonic3_unicode_indexer_trimmed.json"
                    ),
                ),
            ],
        ),
    );
    let p = compute(&row("supertonic", "s"), None, Some(&g));
    assert_eq!(p.native_voices, ["F1", "M1"]);
    assert_eq!(p.char_vocab.as_ref().map(|v| v.len()), Some(661));
    assert_eq!(p.voice_field, VoiceField::Voice);
    assert!(p.streaming, "the embedded spec is the fallback spec");
    // Its engine validates a request against exactly these codes
    // (`families::takes_language`): they are its request vocabulary.
    assert_eq!(p.language_vocab.source, VocabSource::SpecRequest);
    assert_eq!(p.language_vocab.entries, ["en", "de"]);
    assert!(p.problems.is_empty(), "{:?}", p.problems);
}

#[test]
fn nemotron_s_language_prompts_come_from_its_processor_config() {
    let d = tempfile::tempdir().unwrap();
    let cfg = serde_json::to_vec(&json!({
        "prompt_dictionary": {"auto": 0, "de": 7, "de-DE": 8, "en-US": 3, "ja-JP": 21}
    }))
    .unwrap();
    let g = write(
        d.path(),
        "nemotron-asr-q8_0.gguf",
        &synth::audiocpp(
            "nemotron_asr",
            r#"{"family":"nemotron_asr","languages":["de-DE","en-US","ja-JP"]}"#,
            &[("processor_config.json", &cfg)],
        ),
    );
    let mut r = row("nemotron_asr", "n");
    r.task = "asr".into();
    let p = compute(&r, None, Some(&g));
    assert_eq!(p.language_vocab.source, VocabSource::Gguf);
    assert_eq!(
        p.language_vocab.entries,
        ["auto", "de", "de-DE", "en-US", "ja-JP"]
    );
    assert!(p.problems.is_empty(), "{:?}", p.problems);
}

#[test]
fn qwen3_custom_voice_reads_speakers_languages_and_variant_from_config_json() {
    let d = tempfile::tempdir().unwrap();
    let g = write(
        d.path(),
        "qwen3-tts-cv-q8_0.gguf",
        &synth::audiocpp(
            "qwen3_tts",
            r#"{"family":"qwen3_tts","tasks":["tts","clone","design"]}"#,
            &[
                ("README.md", b"x"),
                ("config.json", &qwen3_config("custom_voice")),
            ],
        ),
    );
    let p = compute(&row("qwen3_tts", "q"), None, Some(&g));
    assert_eq!(p.native_voices, ["aiden", "ryan", "serena"]);
    assert_eq!(
        p.native("Ryan"),
        Some("ryan"),
        "any case, canonical spelling"
    );
    assert_eq!(p.language_vocab.source, VocabSource::Gguf);
    assert_eq!(
        p.language_vocab.entries,
        ["chinese", "english", "german", "auto"]
    );
    assert_eq!(p.variant_task.as_deref(), Some("tts"));

    let g = write(
        d.path(),
        "qwen3-tts-vd-q8_0.gguf",
        &synth::audiocpp(
            "qwen3_tts",
            "{}",
            &[("config.json", &qwen3_config("voice_design"))],
        ),
    );
    let p = compute(&row("qwen3_tts", "q"), None, Some(&g));
    assert!(
        p.native_voices.is_empty(),
        "VoiceDesign has no speaker to name"
    );
    assert_eq!(p.variant_task.as_deref(), Some("vdes"));
}

#[test]
fn magpie_names_come_from_the_spec_voice_id_enum_and_go_to_options() {
    let spec = parse_spec(&json!({
        "family": "magpie_tts", "languages": ["de", "en"],
        "options": {"request": [
            {"name": "language", "type": "string", "default": "en"},
            {"name": "voice_id", "type": "enum", "default": "Aria",
             "values": ["Aria", "Jason", "John", "Leo", "Sofia"]}
        ]}
    }));
    let p = compute(&row("magpie_tts", "m"), Some(&spec), None);
    assert_eq!(p.voice_field, VoiceField::OptionsVoiceId);
    assert_eq!(p.native_voices, ["Aria", "Jason", "John", "Leo", "Sofia"]);
    assert_eq!(p.default_voice.as_deref(), Some("Aria"));
    assert!(p.language_option);
    // Its engine refuses a language its tokenizers lack
    // (`families::takes_language`): the spec's languages are what a request
    // takes.
    assert_eq!(p.language_vocab.source, VocabSource::SpecRequest);
    assert_eq!(p.language_vocab.entries, ["de", "en"]);

    // The older embedded spec's index value is no name.
    let old = parse_spec(&json!({
        "family": "magpie_tts",
        "options": {"request": [{"name": "voice_id", "type": "enum", "default": "0",
                                 "values": ["0", "Sofia", "Jason"]}]}
    }));
    let p = compute(&row("magpie_tts", "m"), Some(&old), None);
    assert_eq!(p.native_voices, ["Jason", "Sofia"]);
    assert_eq!(p.default_voice, None);
}

#[test]
fn builtin_voices_a_family_keeps_as_files_and_the_kokoro_vocabulary() {
    let pocket = parse_spec(&json!({
        "family": "pocket_tts",
        "ui": {"builtin_voices": ["alba"], "default_voice": "alba"},
        "packages": [
            {"id": "en", "files": ["PocketTTS-GGUF/english/p.gguf",
                                   "PocketTTS-GGUF/english/embeddings/alba.safetensors"]},
            {"id": "de", "files": ["PocketTTS-GGUF/german/p.gguf"]}
        ]
    }));
    let p = compute(&row("pocket_tts", "p"), Some(&pocket), None);
    assert_eq!(p.native_voices, ["alba"]);
    assert_eq!(p.file_backed, ["alba"]);
    assert_eq!(p.default_voice.as_deref(), Some("alba"));

    let kokoro = parse_spec(&json!({
        "family": "kokoro_tts", "languages": ["en-us", "en-gb", "pt-br"],
        "ui": {"builtin_voices": ["af_heart", "bm_george"], "default_voice": "af_heart"}
    }));
    let p = compute(&row("kokoro_tts", "k"), Some(&kokoro), None);
    assert!(p.file_backed.is_empty(), "kokoro's voices live in its GGUF");
    assert_eq!(p.language_vocab.source, VocabSource::FamilyTable);
    assert_eq!(p.language_vocab.entries, ["en-us", "en-gb", "pt-br"]);

    let names = parse_spec(&json!({"family": "x", "languages": ["english", "german"]}));
    let p = compute(&row("x", "x"), Some(&names), None);
    assert_eq!(p.language_vocab.source, VocabSource::SpecNames);
}

/// audio.cpp hands every `instructions` to the engine as
/// `options.instruction`. OmniVoice and MOSS-VoiceGen read it although
/// their specs declare no such option — the family table says so; a family
/// neither declaring nor listed has them dropped.
#[test]
fn engines_that_read_instructions_their_spec_does_not_declare() {
    let omni = parse_spec(&json!({"family": "omnivoice", "tasks": ["tts"]}));
    let p = compute(&row("omnivoice", "o"), Some(&omni), None);
    assert_eq!(p.instructions, InstructionsMode::Passthrough);
    assert_eq!(p.instructions_field, InstructionsField::Instructions);

    let moss = parse_spec(&json!({"family": "moss_voicegen", "tasks": ["design"],
                                  "options": null}));
    let p = compute(&row("moss_voicegen", "m"), Some(&moss), None);
    assert_eq!(p.instructions, InstructionsMode::VoiceDesign);
    assert_eq!(p.instructions_field, InstructionsField::Instructions);
    // With no spec at all, the table still holds.
    let p = compute(&row("moss_voicegen", "m"), None, None);
    assert_eq!(p.instructions, InstructionsMode::VoiceDesign);

    let kokoro = parse_spec(&json!({"family": "kokoro_tts", "tasks": ["tts"]}));
    let p = compute(&row("kokoro_tts", "k"), Some(&kokoro), None);
    assert_eq!(p.instructions, InstructionsMode::None);
}

/// A `seed` reaches the engine where its spec declares one, and for
/// Qwen3-TTS and OmniVoice, which read it undeclared (WP10 D6, R4 M1);
/// anywhere else none is sent.
#[test]
fn a_seed_is_read_where_the_spec_declares_one_or_the_family_table_says() {
    let seeded = parse_spec(&json!({"family": "cosyvoice3", "tasks": ["tts"],
        "options": {"request": [{"name": "seed", "type": "int"}]}}));
    assert!(compute(&row("cosyvoice3", "c"), Some(&seeded), None).reads_seed);
    let qwen3 = parse_spec(&json!({"family": "qwen3_tts", "tasks": ["tts"]}));
    assert!(compute(&row("qwen3_tts", "q"), Some(&qwen3), None).reads_seed);
    let omni = parse_spec(&json!({"family": "omnivoice", "tasks": ["tts"]}));
    assert!(compute(&row("omnivoice", "o"), Some(&omni), None).reads_seed);
    let kokoro = parse_spec(&json!({"family": "kokoro_tts", "tasks": ["tts"]}));
    assert!(!compute(&row("kokoro_tts", "k"), Some(&kokoro), None).reads_seed);
}

#[test]
fn a_broken_gguf_is_a_problem_not_a_failure() {
    let d = tempfile::tempdir().unwrap();
    let g = d.path().join("w.gguf");
    std::fs::write(&g, b"not a gguf at all").unwrap();
    let p = compute(&row("supertonic", "s"), None, Some(&g));
    assert!(p.native_voices.is_empty());
    assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
}

#[tokio::test]
async fn the_cache_computes_once_per_change_of_its_inputs() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("q");
    std::fs::create_dir_all(&root).unwrap();
    let h = synth::audiocpp(
        "qwen3_tts",
        "{}",
        &[("config.json", &qwen3_config("custom_voice"))],
    );
    write(&root, "q-q8_0.gguf", &h);
    let cache = Arc::new(ProfileCache::default());
    let dir = d.path().display().to_string();
    let r = row("qwen3_tts", "q");
    let a = profile_of(&cache, &dir, &r, None).await;
    let b = profile_of(&cache, &dir, &r, None).await;
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(cache.computed(), 1);

    // A new catalog snapshot is a new input.
    let spec = parse_spec(&json!({"family": "qwen3_tts", "modes": ["offline"]}));
    profile_of(&cache, &dir, &r, Some(("t1".into(), spec))).await;
    assert_eq!(cache.computed(), 2);

    // So is a different package file (here: a different length).
    let mut bigger = synth::audiocpp(
        "qwen3_tts",
        "{}",
        &[("config.json", &qwen3_config("custom_voice"))],
    );
    bigger.str("general.extra", "x");
    write(&root, "q-q8_0.gguf", &bigger);
    let c = profile_of(&cache, &dir, &r, None).await;
    assert_eq!(cache.computed(), 3);
    assert_eq!(c.native_voices, ["aiden", "ryan", "serena"]);
}

/// A package file read from beside the GGUF is an input too (review
/// TC-7): Supertonic's vocabulary added there, then edited, computes the
/// profile again.
#[tokio::test]
async fn a_file_beside_the_gguf_is_an_input_of_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("s");
    std::fs::create_dir_all(&root).unwrap();
    let spec = json!({"family": "supertonic", "sources": [{"files": {
        "unicode_indexer": "model:config/unicode_indexer.json"
    }}]});
    write(
        &root,
        "supertonic-3-q8_0.gguf",
        &synth::audiocpp("supertonic", &spec.to_string(), &[]),
    );
    let cache = Arc::new(ProfileCache::default());
    let dir = d.path().display().to_string();
    let r = row("supertonic", "s");
    assert!(profile_of(&cache, &dir, &r, None)
        .await
        .char_vocab
        .is_none());
    profile_of(&cache, &dir, &r, None).await;
    assert_eq!(cache.computed(), 1);

    std::fs::create_dir(root.join("config")).unwrap();
    let file = root.join("config/unicode_indexer.json");
    std::fs::write(&file, b"{\"65\": 1}").unwrap();
    let p = profile_of(&cache, &dir, &r, None).await;
    assert_eq!(cache.computed(), 2);
    assert_eq!(p.char_vocab.as_ref().unwrap().len(), 1);
    profile_of(&cache, &dir, &r, None).await;
    assert_eq!(cache.computed(), 2);

    std::fs::write(&file, b"{\"65\": 1, \"66\": 2}").unwrap();
    let p = profile_of(&cache, &dir, &r, None).await;
    assert_eq!(cache.computed(), 3);
    assert_eq!(p.char_vocab.as_ref().unwrap().len(), 2);
}

/// A package file's stamp is taken when the file is noted, before it is
/// read (review TC-17): an edit after that shows, and a stamp taken later,
/// with the compute, would hide it.
#[test]
fn a_beside_file_is_stamped_when_noted_not_when_checked() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("config.json");
    std::fs::write(&file, b"{}").unwrap();
    let mut p = SpeechProfile::default();
    p.note_beside(file.clone());
    assert!(p.beside_unchanged());
    std::fs::write(&file, b"{\"a\": 1}").unwrap();
    assert!(!p.beside_unchanged(), "the edit after the stamp shows");
    // Looked for and absent: the file added later shows too.
    let absent = d.path().join("later.json");
    let mut p = SpeechProfile::default();
    p.note_beside(absent.clone());
    assert!(p.beside_unchanged());
    std::fs::write(&absent, b"{}").unwrap();
    assert!(!p.beside_unchanged());
}

/// The owner's real packages, when `LMGW_TEST_AUDIO_MODELS_DIR` points at an
/// audio models dir holding them (skipped otherwise): the extractors read
/// what the synthetic packages above claim they read.
#[test]
fn real_packages() {
    let Some(root) = std::env::var_os("LMGW_TEST_AUDIO_MODELS_DIR") else {
        eprintln!("skipping: LMGW_TEST_AUDIO_MODELS_DIR is not set");
        return;
    };
    let base = PathBuf::from(root).join("audio-cpp/audio.cpp-gguf");
    let read = |rel: &str, family: &str| {
        let g = base.join(rel);
        g.is_file()
            .then(|| compute(&row(family, "x"), None, Some(&g)))
    };
    if let Some(p) = read("Supertonic-3-GGUF/supertonic-3-q8_0.gguf", "supertonic") {
        assert_eq!(
            p.native_voices,
            ["F1", "F2", "F3", "F4", "F5", "M1", "M2", "M3", "M4", "M5"]
        );
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        // The characters its engine says (`audio::charset`): not the German
        // opening quote that failed a voice turn, the ASCII one it becomes.
        let v = p
            .char_vocab
            .as_ref()
            .expect("the package's unicode_indexer");
        assert_eq!(v.file, "config/unicode_indexer.json");
        assert!(!v.says('\u{201E}') && v.says('"'));
    }
    if let Some(p) = read(
        "Qwen3-TTS-12Hz-1.7B-CustomVoice-GGUF/qwen3-tts-12hz-1.7b-customvoice-q8_0.gguf",
        "qwen3_tts",
    ) {
        assert!(p.native("Ryan").is_some(), "{:?}", p.native_voices);
        assert_eq!(p.native_voices.len(), 9);
        assert!(p.language_vocab.entries.contains(&"german".to_string()));
        assert_eq!(p.variant_task.as_deref(), Some("tts"));
    }
    if let Some(p) = read(
        "Qwen3-TTS-12Hz-1.7B-VoiceDesign-GGUF/qwen3-tts-12hz-1.7b-voicedesign-q8_0.gguf",
        "qwen3_tts",
    ) {
        assert_eq!(p.variant_task.as_deref(), Some("vdes"));
    }
    if let Some(p) = read(
        "MagpieTTS-Multilingual-357M-GGUF/magpie-tts-multilingual-357m-q8_0.gguf",
        "magpie_tts",
    ) {
        assert_eq!(p.voice_field, VoiceField::OptionsVoiceId);
        assert!(p.native("Jason").is_some(), "{:?}", p.native_voices);
    }
    if let Some(p) = read("Kokoro-82M-GGUF/kokoro-82m-q8_0.gguf", "kokoro_tts") {
        assert!(p.native("af_heart").is_some());
        assert_eq!(p.default_voice.as_deref(), Some("af_heart"));
    }
}

/// What an engine does with no voice named (R3, `families::unvoiced`): read
/// off audio.cpp's source per family, Qwen3-TTS Base by its GGUF's variant,
/// and a built-in default the package lacks not counted on.
#[test]
fn what_an_engine_does_without_a_voice_is_the_family_s() {
    let d = tempfile::tempdir().unwrap();
    for (family, want) in [
        ("omnivoice", Unvoiced::DrawsSpeaker),
        ("magpie_tts", Unvoiced::EngineDefault { voice: None }),
        ("cosyvoice3", Unvoiced::NeedsReference),
        ("chatterbox", Unvoiced::NeedsReference),
        ("pocket_tts", Unvoiced::Unknown),
    ] {
        assert_eq!(
            compute(&row(family, "x"), None, None).unvoiced,
            want,
            "{family}"
        );
    }
    // Supertonic speaks M1: present in this package, missing in that one.
    let styles = |names: &[&str]| {
        let files: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|n| (format!("voice_style_{n}"), json!(format!("model:{n}.json"))))
            .collect();
        json!({"family": "supertonic", "sources": [{"files": files}]}).to_string()
    };
    for (names, want) in [
        (
            &["M1", "F1"][..],
            Unvoiced::EngineDefault { voice: Some("M1") },
        ),
        (&["F1"][..], Unvoiced::Unknown),
    ] {
        let g = write(
            d.path(),
            "supertonic.gguf",
            &synth::audiocpp("supertonic", &styles(names), &[("README.md", b"x")]),
        );
        assert_eq!(
            compute(&row("supertonic", "s"), None, Some(&g)).unvoiced,
            want,
            "{names:?}"
        );
    }
    // Qwen3-TTS: Base clones, CustomVoice wants a speaker — not known.
    for (variant, want) in [
        ("base", Unvoiced::NeedsReference),
        ("custom_voice", Unvoiced::Unknown),
    ] {
        let g = write(
            d.path(),
            "qwen3.gguf",
            &synth::audiocpp(
                "qwen3_tts",
                "{}",
                &[("config.json", &qwen3_config(variant))],
            ),
        );
        assert_eq!(
            compute(&row("qwen3_tts", "q"), None, Some(&g)).unvoiced,
            want,
            "{variant}"
        );
    }
}

/// Which engines refuse a clip without its transcript
/// (`families::clone_requires_transcript`): read off audio.cpp's source,
/// not the spec — OmniVoice's spec declares no options at all, CosyVoice3's
/// declares `reference_text` and speaks without, VoxCPM2 clones its voice
/// reference without text, and Qwen3-TTS refuses only as Base (its GGUF's
/// variant), in ICL mode.
#[test]
fn a_clone_without_its_transcript_is_refused_by_the_family_s_engine() {
    let spec = |family: &str, request: serde_json::Value| {
        parse_spec(&json!({"family": family, "options": {"request": request}}))
    };
    let text = json!([{"name": "reference_text", "type": "string"}]);
    for (family, spec, needs, requires) in [
        ("omnivoice", spec("omnivoice", json!([])), true, true),
        ("fish_audio", spec("fish_audio", text.clone()), true, true),
        ("cosyvoice3", spec("cosyvoice3", text.clone()), true, false),
        ("voxcpm2", spec("voxcpm2", json!([])), false, false),
        ("pocket_tts", spec("pocket_tts", json!([])), false, false),
    ] {
        let p = compute(&row(family, "x"), Some(&spec), None);
        assert_eq!(
            (p.needs_reference_text, p.requires_reference_text),
            (needs, requires),
            "{family}"
        );
    }
    let d = tempfile::tempdir().unwrap();
    for (variant, requires) in [("base", true), ("custom_voice", false)] {
        let g = write(
            d.path(),
            "qwen3.gguf",
            &synth::audiocpp(
                "qwen3_tts",
                "{}",
                &[("config.json", &qwen3_config(variant))],
            ),
        );
        let p = compute(&row("qwen3_tts", "q"), None, Some(&g));
        assert_eq!(p.requires_reference_text, requires, "{variant}");
    }
}
