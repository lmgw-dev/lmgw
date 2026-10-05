//! `GET /v1/audio/voices` on a local audio row (audio-class gap 6): answered
//! from lmgw's own catalog without starting anything — no `podman run`, no
//! registry entry, no `request_logs` row — and `?probe=engine` is the old
//! admitted read of the model's own server.

use sqlx::Row;

use super::audio_residency::add_audio_model;
use super::*;

/// The row's root gets an `embeddings/alba.safetensors` (Pocket TTS's
/// built-in voice layout) and the voice library a clip, `me.wav`, with a
/// recorded transcript.
fn voice_files(f: &Fixture, model_id: &str) {
    let dir = f._models_dir.path();
    std::fs::create_dir_all(dir.join(model_id).join("embeddings")).unwrap();
    std::fs::write(dir.join(model_id).join("embeddings/alba.safetensors"), b"x").unwrap();
    std::fs::create_dir_all(dir.join("voices")).unwrap();
    std::fs::write(dir.join("voices/me.wav"), b"RIFF").unwrap();
    std::fs::write(dir.join("voices/prompt_text"), "me|hello there\n").unwrap();
}

async fn logs(f: &Fixture) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM request_logs")
        .fetch_one(&f.state.db)
        .await
        .unwrap()
        .get("n")
}

async fn voices(f: &Fixture, query: &str) -> reqwest::Response {
    f.gateway
        .client()
        .get(format!(
            "{}/v1/audio/voices?model=audio/tts{query}",
            f.gateway
        ))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_local_rows_voices_come_from_lmgw_and_start_nothing() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    voice_files(&f, "tts");

    let resp = voices(&f, "").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-voices-source"], "config");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["voices"], json!(["alba", "me"]), "{v}");
    assert_eq!(v["lmgw"]["source"], "config");
    assert_eq!(v["lmgw"]["engine_asked"], false);
    assert_eq!(
        v["lmgw"]["entries"],
        json!([
            {"id": "alba", "kind": "embedding", "send": "voice"},
            {"id": "me", "kind": "library", "send": "voice", "transcript": true},
        ])
    );

    assert!(f.runs().is_empty(), "nothing started: {:?}", f.runs());
    assert!(f.state.runtime().list().is_empty());
    assert_eq!(logs(&f).await, 0, "metadata writes no request row");
}

#[tokio::test]
async fn probe_engine_asks_the_models_own_server_and_starts_it() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    voice_files(&f, "tts");
    for server in [&f.first, &f._second, &f._third] {
        Mock::given(method("GET"))
            .and(path("/v1/audio/voices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"voices": ["alba"]})))
            .mount(server)
            .await;
    }

    let resp = voices(&f, "&probe=engine").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-voices-source"], "engine");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v, json!({"voices": ["alba"]}), "audio.cpp's own answer");
    assert_eq!(f.runs(), ["tts"], "admission started the container");

    let resp = voices(&f, "&probe=nope").await;
    assert_eq!(resp.status(), 400);
}

/// Under the GPU hold, with no fallback for the row, its voices are still
/// lmgw's config to read — the hold is a zero VRAM allowance, not a model
/// that is gone — and say `held`; its speech stays refused.
#[tokio::test]
async fn a_held_rows_voices_are_still_answered_from_config_and_say_held() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    voice_files(&f, "tts");
    lmgw_core::ops::hold_set(&f.state, true).await.unwrap();

    let resp = voices(&f, "").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-voices-source"], "config");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["voices"], json!(["alba", "me"]), "{v}");
    assert_eq!(v["lmgw"]["held"], true);

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/tts", "input": "hi", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "speech is what the hold refuses");
    // The engine probe needs the container, so the hold refuses it.
    assert_eq!(voices(&f, "&probe=engine").await.status(), 503);
    assert!(f.runs().is_empty(), "nothing started: {:?}", f.runs());

    lmgw_core::ops::hold_set(&f.state, false).await.unwrap();
    let v: Value = voices(&f, "").await.json().await.unwrap();
    assert_eq!(v["lmgw"]["held"], false);
}
