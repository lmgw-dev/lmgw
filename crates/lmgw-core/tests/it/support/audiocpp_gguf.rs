//! Synthetic audio.cpp package GGUFs (audio-class gaps 1 and 7): the shape
//! of the owner's real packages — the embedded `model_spec.json`, the
//! embedded file table — with none of their weights, written through
//! `lmgw_core::gguf::synth::audiocpp`. No real model file is in the repo.

#![allow(dead_code)]

use std::path::Path;

use lmgw_core::gguf::synth;
use serde_json::json;

/// MagpieTTS: speakers in the spec's `voice_id` request enum, read from
/// `options.voice_id`.
pub fn magpie(root: &Path) {
    let spec = json!({
        "family": "magpie_tts", "tasks": ["tts"], "modes": ["offline"],
        "languages": ["de", "en", "es"],
        "options": {"request": [
            {"name": "language", "type": "string", "default": "en"},
            {"name": "voice_id", "type": "enum", "default": "Aria",
             "values": ["Aria", "Jason", "John", "Leo", "Sofia"]}
        ]}
    });
    synth::audiocpp(
        "magpie_tts",
        &spec.to_string(),
        &[(
            "speakers.json",
            br#"{"Aria":0,"Jason":1,"John":2,"Leo":3,"Sofia":4}"#,
        )],
    )
    .write_to(&root.join("magpie-tts-multilingual-357m-q8_0.gguf"));
}

/// Supertonic 3: one `voice_style_<name>` source file per built-in voice.
pub fn supertonic(root: &Path) {
    let spec = json!({
        "family": "supertonic", "tasks": ["tts"], "modes": ["offline", "streaming"],
        "languages": ["en", "de"],
        "sources": [{"files": {
            "voice_style_F1": "model:voice_styles/F1.json",
            "voice_style_M1": "model:voice_styles/M1.json"
        }}]
    });
    synth::audiocpp(
        "supertonic",
        &spec.to_string(),
        &[
            ("voice_styles/F1.json", b"{}"),
            ("voice_styles/M1.json", b"{}"),
        ],
    )
    .write_to(&root.join("supertonic-3-q8_0.gguf"));
}

/// Qwen3-TTS: speakers, languages and variant in the embedded
/// `config.json`. `model_type` is `custom_voice`, `voice_design` or `base`.
pub fn qwen3(root: &Path, model_type: &str) {
    let config = json!({
        "tts_model_type": model_type,
        "talker_config": {
            "spk_id": {"serena": 3066, "ryan": 3061, "aiden": 2861},
            "codec_language_id": {"chinese": 2055, "english": 2050, "german": 2053}
        }
    });
    let spec = json!({"family": "qwen3_tts", "tasks": ["tts", "clone", "design"],
                      "modes": ["offline"], "languages": ["zh", "en", "de"]});
    synth::audiocpp(
        "qwen3_tts",
        &spec.to_string(),
        &[("config.json", config.to_string().as_bytes())],
    )
    .write_to(&root.join(format!("qwen3-tts-12hz-1.7b-{model_type}-q8_0.gguf")));
}

/// MOSS-VoiceGen, as its spec stands: the `design` task only and no request
/// options — its engine reads `options.instruction` all the same.
pub fn moss_voicegen(root: &Path) {
    let spec = json!({"family": "moss_voicegen", "tasks": ["design"], "modes": ["offline"],
                      "options": null});
    synth::audiocpp("moss_voicegen", &spec.to_string(), &[])
        .write_to(&root.join("moss-voicegen-q8_0.gguf"));
}

/// A package of `family` whose embedded spec declares the request options
/// `options` (by name) and the modes `modes` — for families whose behaviour
/// the spec decides (an `instruction` or `instruct` option, streaming).
pub fn with_options(root: &Path, family: &str, options: &[&str], modes: &[&str]) {
    let request: Vec<_> = options
        .iter()
        .map(|o| json!({"name": o, "type": "string"}))
        .collect();
    let spec = json!({
        "family": family, "tasks": ["tts"], "modes": modes, "languages": ["en"],
        "options": {"request": request}
    });
    synth::audiocpp(family, &spec.to_string(), &[])
        .write_to(&root.join(format!("{family}-q8_0.gguf")));
}
