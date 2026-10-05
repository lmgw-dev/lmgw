//! Streaming speech rows (audio-class gap 8): an offline row asked to stream
//! is refused before anything starts, and a streamed answer — PCM with no
//! header of its own — says its sample rate in `x-lmgw-sample-rate`,
//! learned from the row's last WAV through lmgw. The packages are synthetic.

use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::support::audio_world::{wav_bytes, world};
use crate::support::audiocpp_gguf;

fn supertonic(r: &std::path::Path) {
    audiocpp_gguf::with_options(r, "supertonic", &[], &["offline", "streaming"])
}

#[tokio::test]
async fn an_offline_row_asked_to_stream_is_refused_before_anything_starts() {
    let w = world().await;
    w.row("offline", "supertonic", supertonic).await;
    w.answer_wav().await;
    for ask in [
        json!({"model": "audio/offline", "input": "Hi.", "stream_format": "sse"}),
        json!({"model": "audio/offline", "input": "Hi.", "stream": true}),
    ] {
        let resp = w.speak(ask).await;
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "streaming_unsupported", "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("set the row's mode to streaming"),
            "{body}"
        );
    }
    assert_eq!(w.runs(), 0, "nothing was started for it");
    assert!(w.sent().await.is_empty());
}

#[tokio::test]
async fn a_streamed_answer_states_the_sample_rate_its_rows_last_wav_said() {
    let w = world().await;
    w.row_with("live", "supertonic", supertonic, |row| {
        row.mode = "streaming".into()
    })
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(body_partial_json(json!({"stream_format": "sse"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"type\":\"speech.audio.delta\",\"audio\":\"AAAA\"}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .with_priority(1)
        .mount(&w.container)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(wav_bytes(44_100, 441)),
        )
        .mount(&w.container)
        .await;
    let stream = json!({"model": "audio/live", "input": "Hi.", "stream_format": "sse",
                        "response_format": "pcm"});

    // Nothing learned yet: no header, rather than a guessed rate.
    let resp = w.speak(stream.clone()).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-sample-rate").is_none());
    resp.bytes().await.unwrap();

    // A plain request answers one WAV — a streaming row still does — and
    // teaches the rate.
    let resp = w
        .speak(json!({"model": "audio/live", "input": "Hi."}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("x-lmgw-sample-rate").is_none(),
        "a WAV says it itself"
    );
    resp.bytes().await.unwrap();

    let resp = w.speak(stream).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-sample-rate"], "44100");

    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let speech = &models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "audio/live")
        .unwrap()["capabilities"]["speech"];
    assert_eq!(speech["streaming"], true, "{speech}");
    assert_eq!(speech["sample_rate"], 44_100);
}
