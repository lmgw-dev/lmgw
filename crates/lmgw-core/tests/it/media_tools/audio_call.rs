//! `audio__speak` and `audio__transcribe` (design §4.2), on cloud speech
//! aliases: one model row each, the recording inline on `/mcp`, the
//! transcript as text.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::{b64, call, cloud_alias, gateway, rows_of, text_of};
use crate::support::audio_world::{wav, wav_bytes};

#[tokio::test]
async fn speak_returns_one_recording_and_writes_one_row() {
    let sd = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(wav())
        .mount(&cloud)
        .await;
    let (state, gw) = gateway(&sd, &[]).await;
    cloud_alias(&state, &cloud, "tts-cloud.example", "tts").await;

    let r = call(
        &gw,
        "audio__speak",
        json!({"model": "tts-cloud.example", "text": "Hello there. How are you?", "voice": "alloy"}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = text_of(&r);
    assert!(text.starts_with("Spoke a recording ("), "{text}");
    assert!(
        text.contains("with tts-cloud.example, voice alloy"),
        "{text}"
    );
    assert_eq!(r["content"][1]["type"], json!("audio"));
    assert_eq!(r["content"][1]["mimeType"], json!("audio/wav"));

    let rows = rows_of(&state, "audio", 1).await;
    assert_eq!(rows.len(), 1, "one TTS row for the whole text: {rows:?}");
    assert_eq!(rows[0]["alias"], json!("tts-cloud.example"));
}

#[tokio::test]
async fn transcribe_returns_the_text() {
    let sd = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({"text": "hello there", "language": "en"})),
        )
        .mount(&cloud)
        .await;
    let (state, gw) = gateway(&sd, &[]).await;
    cloud_alias(&state, &cloud, "asr-cloud.example", "asr").await;

    let clip = format!("data:audio/wav;base64,{}", b64(&wav_bytes(16_000, 1600)));
    let r = call(
        &gw,
        "audio__transcribe",
        json!({"model": "asr-cloud.example", "audio": clip, "language": "en"}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = text_of(&r);
    assert!(
        text.contains("by asr-cloud.example (language en)"),
        "{text}"
    );
    assert!(text.ends_with("hello there"), "{text}");
    assert_eq!(r["content"].as_array().unwrap().len(), 1, "text only: {r}");
    let rows = rows_of(&state, "audio", 1).await;
    assert_eq!(rows[0]["alias"], json!("asr-cloud.example"));

    // A picture is no recording.
    let r = call(
        &gw,
        "audio__transcribe",
        json!({"model": "asr-cloud.example", "audio": format!("data:image/png;base64,{}", b64(&super::png()))}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(text_of(&r).contains("takes audio"), "{}", text_of(&r));
}
