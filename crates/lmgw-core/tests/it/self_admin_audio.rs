//! The text of the audio tools whose answers no OpenAPI document describes in
//! full, read into their types and written out again: `lmgw__audio_catalog`
//! (the cached listing, a refresh, a queued download with its `next_step`) and
//! `lmgw__audio_model_set` (a `next_step` after a create only).
//!
//! The text is the answer's pretty JSON with sorted keys, so a key a type
//! adds, drops, renames or turns from absent into `null` shows up as a
//! difference between the text and what the type writes back.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::AudioCatalogList;
use lmgw_core::config::Settings;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use crate::support::self_admin::{parse, read_back, Admin};

async fn gateway() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(models.path().join("pocket/german")).unwrap();
    let mut s = Settings::default();
    s.audio.models_dir = models.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, models)
}

/// A catalog host with two families: one whose package repository lists, one
/// whose does not (so a refresh has a warning to keep).
async fn hub() -> MockServer {
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [
                {"path": "model_specs/pocket_tts.json"},
                {"path": "model_specs/second_tts.json"},
            ],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "family": "pocket_tts",
            "display_name": "Pocket TTS",
            "description": "Small multilingual TTS.",
            "category": "tts",
            "tasks": ["tts"],
            "languages": ["de"],
            "options": {
                "load": [{"name": "language", "kind": "string"}],
                "request": [{"name": "temperature", "kind": "float",
                             "min": 0.0, "max": 2.0, "default": 0.7}],
            },
            "packages": [{
                "id": "pocket_tts_german_q8_0",
                "display_name": "German q8_0",
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "pocket-tts/german",
                "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF"},
                "files": ["german/model.gguf"],
            }],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/second_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "family": "second_tts",
            "display_name": "Second TTS",
            "description": "Another one.",
            "category": "tts",
            "tasks": ["tts"],
            "languages": ["en"],
            "packages": [{
                "id": "second_q4",
                "display_name": "q4",
                "format": "gguf",
                "precision": "q4_0",
                "target_directory": "second",
                "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/Second-GGUF"},
                "files": ["model.gguf"],
            }],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/models/audio-cpp/PocketTTS-GGUF/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"type": "file", "path": "german/model.gguf", "size": 8},
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/audio-cpp/PocketTTS-GGUF/resolve/main/german/model.gguf",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GGUF\x00pock".to_vec()))
        .mount(&hub)
        .await;
    hub
}

#[tokio::test]
async fn the_listing_is_the_cached_catalog_and_says_where_the_options_are() {
    let _env = common::process_env_lock().await;
    let hub = hub().await;
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let (state, _models) = gateway().await;
    let a = Admin::new(&state).await;

    // Nothing fetched yet: no `fetched_at` and no `warnings` key.
    let text = a
        .text("lmgw__audio_catalog", json!({"action": "list"}))
        .await;
    let none: AudioCatalogList = read_back(&text);
    assert_eq!(none.fetched_at, None);
    assert!(none.families.is_empty() && none.warnings.is_empty());
    assert!(none.message.contains("action=refresh"), "{}", none.message);
    assert!(parse(&text).get("fetched_at").is_none(), "{text}");
    assert!(parse(&text).get("warnings").is_none(), "{text}");

    let text = a
        .text("lmgw__audio_catalog", json!({"action": "refresh"}))
        .await;
    let refreshed: dto::AudioCatalogRefreshed = read_back(&text);
    assert_eq!((refreshed.ok, refreshed.families), (true, 2));
    assert!(
        !refreshed.warnings.is_empty(),
        "the unlisted repository is a warning"
    );
    assert!(
        refreshed.message.contains("warning"),
        "{}",
        refreshed.message
    );

    // The full listing: every family, its options replaced by a sentence.
    let text = a
        .text("lmgw__audio_catalog", json!({"action": "list"}))
        .await;
    let all: AudioCatalogList = read_back(&text);
    assert_eq!(
        all.fetched_at.as_deref(),
        Some(refreshed.fetched_at.as_str())
    );
    assert_eq!(all.warnings, refreshed.warnings);
    assert!(all.message.starts_with("2 families"), "{}", all.message);
    for f in &all.families {
        assert!(
            f.options.is_none(),
            "{} has no options in the full listing",
            f.family
        );
        let said = f.options_summary.as_deref().expect("the sentence");
        assert!(said.contains(&format!("family={}", f.family)), "{said}");
    }
    assert_eq!(all.families[0].family, "pocket_tts");
    assert_eq!(all.families[0].packages[0].id, "pocket_tts_german_q8_0");
    let first = &parse(&text)["families"][0];
    assert!(first.get("options").is_none(), "{text}");
    assert!(first["options_summary"].is_string(), "{text}");

    // One family: its options, as the dashboard's `AudioFamilyOptions`.
    let text = a
        .text(
            "lmgw__audio_catalog",
            json!({"action": "list", "family": " pocket_tts "}),
        )
        .await;
    let one: AudioCatalogList = read_back(&text);
    assert!(one.message.starts_with("1 family"), "{}", one.message);
    assert_eq!(one.families[0].options_summary, None);
    let options = one.families[0]
        .options
        .as_ref()
        .expect("a named family comes with its options");
    assert_eq!(options.load[0].name, "language");
    assert_eq!(options.request[0].default, Some(json!(0.7)));
    assert_eq!(options.request[0].max, Some(2.0));
    assert!(parse(&text)["families"][0]["options"].is_object(), "{text}");
    assert!(
        parse(&text)["families"][0].get("options_summary").is_none(),
        "{text}"
    );

    // The dashboard's own read of the same snapshot has the same families
    // and the same options inside them.
    let gw = common::serve(state.clone()).await;
    let catalog: dto::AudioCatalog = gw
        .client()
        .get(format!("{gw}/api/audio/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog.families[0].options.as_ref(), Some(options));
    assert_eq!(catalog.warnings, refreshed.warnings);

    // A search that matches nothing is an empty list with its own sentence.
    let text = a
        .text(
            "lmgw__audio_catalog",
            json!({"action": "list", "search": "no-such-thing"}),
        )
        .await;
    let nothing: AudioCatalogList = read_back(&text);
    assert!(nothing.families.is_empty());
    assert_eq!(nothing.message, "no family matches");
    assert!(
        !nothing.warnings.is_empty(),
        "a fetched catalog keeps its warnings: {text}"
    );
}

#[tokio::test]
async fn a_download_says_how_to_go_on_and_a_refresh_does_not() {
    let _env = common::process_env_lock().await;
    let hub = hub().await;
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let (state, _models) = gateway().await;
    let a = Admin::new(&state).await;
    let text = a
        .text("lmgw__audio_catalog", json!({"action": "refresh"}))
        .await;
    assert!(parse(&text).get("next_step").is_none(), "{text}");

    let text = a
        .text(
            "lmgw__audio_catalog",
            json!({"action": "download", "family": "pocket_tts",
                   "package": "pocket_tts_german_q8_0"}),
        )
        .await;
    let queued: dto::WithNextStep<dto::AudioCatalogInstall> = read_back(&text);
    assert_eq!(queued.answer.files_queued, 1);
    assert_eq!(queued.answer.package, "pocket_tts_german_q8_0");
    assert!(
        queued
            .next_step
            .contains("lmgw__audio_catalog action=list family=pocket_tts shows them"),
        "{}",
        queued.next_step
    );

    // The dashboard's op answers the same install without the hint.
    let gw = common::serve(state.clone()).await;
    let body: serde_json::Value = gw
        .client()
        .post(format!("{gw}/api/op/audio_catalog"))
        .json(&json!({"action": "download", "family": "pocket_tts",
                      "package": "pocket_tts_german_q8_0"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("next_step").is_none(), "{body}");
    let install: dto::AudioCatalogInstall = common::round_trips("audio_catalog download", &body);
    assert_eq!(install.package, queued.answer.package);

    let (failed, said) = a
        .call(
            "lmgw__audio_catalog",
            json!({"action": "download", "family": "pocket_tts"}),
        )
        .await;
    assert!(failed && said.contains("pass family and package"), "{said}");
}

#[tokio::test]
async fn a_create_says_how_to_prove_the_model_and_the_other_actions_do_not() {
    let (state, _models) = gateway().await;
    let a = Admin::new(&state).await;
    let text = a
        .text(
            "lmgw__audio_model_set",
            json!({"action": "create", "model_id": "tts-de", "family": "pocket_tts",
                   "path": "pocket/german", "task": "tts"}),
        )
        .await;
    let made: dto::WithNextStep<dto::AudioModelSaved> = read_back(&text);
    let id = made.answer.id;
    assert!(made.answer.ok);
    assert!(
        made.next_step
            .contains("lmgw__local_model_get target=audio model_id=tts-de"),
        "{}",
        made.next_step
    );
    assert!(
        made.next_step.contains("with model 'audio/tts-de'"),
        "{}",
        made.next_step
    );

    let text = a
        .text(
            "lmgw__audio_model_set",
            json!({"action": "update", "id": id, "lazy": true}),
        )
        .await;
    let updated: dto::AudioModelSaved = read_back(&text);
    assert_eq!(updated.id, id);
    assert!(parse(&text).get("next_step").is_none(), "{text}");

    let text = a
        .text(
            "lmgw__audio_model_set",
            json!({"action": "delete", "model_id": "tts-de"}),
        )
        .await;
    let gone: dto::AudioModelSaved = read_back(&text);
    assert_eq!(gone.id, id);
    assert!(parse(&text).get("next_step").is_none());
}
