//! `capabilities.languages` and `capabilities.multilingual` on `GET
//! /v1/models`: every local audio row publishes the languages its package
//! names, whatever its task — a client picking a speech-to-text or an
//! alignment model by language reads the same field as one picking a voice.
//! The packages are synthetic (`support::audiocpp_gguf`).

use serde_json::{json, Value};

use crate::support::audio_world::{world, World};
use crate::support::audiocpp_gguf;

/// The entry of `id` on `GET /v1/models`.
async fn entry(w: &World, id: &str) -> Value {
    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} in {models}"))
        .clone()
}

/// A row of `family` and `task` whose package lists `languages`.
async fn row(w: &World, id: &str, family: &str, task: &str, languages: &[&str]) {
    w.row_with(
        id,
        family,
        |root| audiocpp_gguf::with_languages(root, family, &[task], languages),
        |r| r.task = task.into(),
    )
    .await;
}

#[tokio::test]
async fn a_speech_to_text_row_publishes_its_languages() {
    let w = world().await;
    row(&w, "canary", "canary_asr", "asr", &["en", "de", "es"]).await;
    let caps = entry(&w, "audio/canary").await["capabilities"].clone();
    assert_eq!(caps["task"], "asr");
    assert_eq!(caps["languages"], json!(["de", "en", "es"]), "{caps}");
    assert!(caps.get("multilingual").is_none(), "{caps}");
    assert!(caps.get("speech").is_none(), "{caps}");
}

#[tokio::test]
async fn an_alignment_row_publishes_its_languages() {
    let w = world().await;
    row(
        &w,
        "aligner",
        "qwen3_forced_aligner",
        "align",
        &["zh", "en", "yue"],
    )
    .await;
    let caps = entry(&w, "audio/aligner").await["capabilities"].clone();
    assert_eq!(caps["task"], "align");
    assert_eq!(caps["languages"], json!(["en", "yue", "zh"]), "{caps}");
}

#[tokio::test]
async fn a_text_to_speech_row_keeps_its_languages_at_the_same_place() {
    let w = world().await;
    w.row("qwen3", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "custom_voice")
    })
    .await;
    let model = entry(&w, "audio/qwen3").await;
    let caps = &model["capabilities"];
    assert_eq!(caps["languages"], json!(["de", "en", "zh"]), "{caps}");
    assert!(
        caps["speech"].get("languages").is_none(),
        "the field moved, it is not published twice: {caps}"
    );
    // The row's note still names them.
    let notes = model["notes"].to_string();
    assert!(
        notes.contains("language takes ISO 639-1 codes (de, en, zh)"),
        "{notes}"
    );
}

#[tokio::test]
async fn a_family_that_only_counts_its_languages_is_multilingual() {
    let w = world().await;
    row(&w, "many", "fish_audio", "tts", &["80+ languages"]).await;
    row(&w, "also", "mms_forced_aligner", "align", &["multilingual"]).await;
    for id in ["audio/many", "audio/also"] {
        let caps = entry(&w, id).await["capabilities"].clone();
        assert_eq!(caps["multilingual"], true, "{id}: {caps}");
        assert!(
            caps.get("languages").is_none(),
            "{id}: no codes to list: {caps}"
        );
    }
}
