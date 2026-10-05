//! Native voices by name (audio-class gap 1): the voices a package ships —
//! Magpie's speakers, Supertonic's voice styles, Qwen3's CustomVoice
//! speakers — are listed by `GET /v1/audio/voices` from the package itself,
//! and a request naming one reaches the engine where it reads it: Magpie's
//! `options.voice_id`, the others' `voice` in the package's own spelling.
//! The packages are synthetic (`support::audiocpp_gguf`).

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::Mock;

use crate::support::audio_world::{wav, world};
use crate::support::audiocpp_gguf;

#[tokio::test]
async fn the_shipped_voices_are_listed_from_the_package_without_a_start() {
    let w = world().await;
    w.row("magpie", "magpie_tts", audiocpp_gguf::magpie).await;
    w.row("supertonic", "supertonic", audiocpp_gguf::supertonic)
        .await;
    w.row("qwen3", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "custom_voice")
    })
    .await;

    let v = w.voices("magpie").await;
    assert_eq!(
        v["voices"],
        json!(["Aria", "Jason", "John", "Leo", "Sofia"])
    );
    assert_eq!(
        v["lmgw"]["entries"][1],
        json!({"id": "Jason", "kind": "native", "send": "options.voice_id"})
    );
    assert_eq!(v["lmgw"]["default"], "Aria");

    let v = w.voices("supertonic").await;
    assert_eq!(v["voices"], json!(["F1", "M1"]));
    assert_eq!(v["lmgw"]["entries"][0]["send"], "voice");

    let v = w.voices("qwen3").await;
    assert_eq!(v["voices"], json!(["aiden", "ryan", "serena"]));

    assert_eq!(*w.podman.runs.lock().unwrap(), 0, "nothing was started");
}

#[tokio::test]
async fn magpies_jason_arrives_as_options_voice_id() {
    let w = world().await;
    w.row("magpie", "magpie_tts", audiocpp_gguf::magpie).await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(body_partial_json(json!({"options": {"voice_id": "Jason"}})))
        .respond_with(wav())
        .expect(1)
        .mount(&w.container)
        .await;

    let resp = w
        .speak(json!({"model": "audio/magpie", "input": "Hallo.", "voice": "jason"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-speech"], "voice=options.voice_id");
    let sent = w.sent().await;
    assert_eq!(
        sent[0],
        json!({"model": "magpie", "input": "Hallo.", "options": {"voice_id": "Jason"}}),
        "Magpie ignores `voice`, so it is not sent"
    );
}

#[tokio::test]
async fn qwen3_gets_the_packages_spelling_in_voice() {
    let w = world().await;
    w.row("qwen3", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "custom_voice")
    })
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(wav())
        .mount(&w.container)
        .await;

    let resp = w
        .speak(json!({"model": "audio/qwen3", "input": "Hi.", "voice": "Ryan"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-speech"], "voice=ryan");
    // Already the package's spelling: sent as it came, no header.
    let resp = w
        .speak(json!({"model": "audio/qwen3", "input": "Hi.", "voice": "serena"}))
        .await;
    assert!(resp.headers().get("x-lmgw-speech").is_none());
    let sent = w.sent().await;
    assert_eq!(sent[0]["voice"], "ryan");
    assert_eq!(sent[1]["voice"], "serena");
}

/// Language naming (audio-class gap 7): Qwen3 takes `german`, not `de`.
#[tokio::test]
async fn qwen3_hears_an_iso_code_as_its_language_name() {
    let w = world().await;
    w.row("qwen3", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "custom_voice")
    })
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(body_partial_json(json!({"language": "german"})))
        .respond_with(wav())
        .expect(1)
        .mount(&w.container)
        .await;
    let resp = w
        .speak(
            json!({"model": "audio/qwen3", "input": "Hallo.", "voice": "ryan",
                      "language": "de"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-speech"], "language=de->german");
}
