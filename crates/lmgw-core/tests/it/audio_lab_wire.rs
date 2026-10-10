//! The Audio lab's routes against their documented types
//! (`lmgw-api-types::audio_lab`): each JSON answer, read into its type and
//! written back, is the answer again, key by key. A field the gateway adds
//! to an answer without adding it to the type is dropped by the read and
//! fails here. The binary answers are checked by content type.

use lmgw_api_types::audio_lab::{
    AudioLabModels, ClipList, ClipTranscribed, ClipTranscription, ClipsAck, ClipsUploaded,
    LabError, VoicesNotRunning,
};
use lmgw_api_types::OpenAiError;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::common::round_trips;
use crate::support::audio_world::{wav_bytes, world, World};

async fn get(w: &World, route: &str) -> reqwest::Response {
    w.gw.client()
        .get(format!("{}{route}", w.gw))
        .send()
        .await
        .unwrap()
}

async fn post_json(w: &World, route: &str, body: Value) -> (u16, Value) {
    let r =
        w.gw.client()
            .post(format!("{}{route}", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

/// A TTS row `tts` and a speech-to-text row `asr` on the world's container,
/// which answers a WAV and "hello there"; one clip `me.wav` in the library.
async fn lab_world() -> World {
    let w = world().await;
    w.row("tts", "kokoro_tts", |_| {}).await;
    w.row_with(
        "asr",
        "qwen3_asr",
        |_| {},
        |row| {
            row.task = "asr".into();
        },
    )
    .await;
    w.answer_wav().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello there"})))
        .mount(&w.container)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello there",
            "words": [{"word": "hello", "start": 0.0, "end": 0.4}]
        })))
        .mount(&w.container)
        .await;
    let voices = w.models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("me.wav"), wav_bytes(16_000, 1600)).unwrap();
    w
}

#[tokio::test]
async fn the_model_list_and_the_voices_answer_their_types() {
    let w = lab_world().await;

    // Before anything ran: no container rows.
    let live: Value = get(&w, "/audio-lab/api/models").await.json().await.unwrap();
    let models = round_trips::<AudioLabModels>("models", &live);
    assert_eq!(models.models.len(), 2);
    assert!(models.runtime.is_empty());
    assert_eq!(models.container_voices_dir, "/models/voices");

    // A speech call starts the container; its row rides along.
    let resp = w
        .speak(json!({"model": "audio/tts", "input": "hello"}))
        .await;
    assert_eq!(resp.status(), 200);
    let live: Value = get(&w, "/audio-lab/api/models").await.json().await.unwrap();
    let models: AudioLabModels = serde_json::from_value(live.clone()).unwrap();
    assert_eq!(models.runtime.len(), 1, "{live}");
    // The container row is the runtime view, mirrored by `RuntimeStatus`,
    // which also lists the fields a view leaves out: every key the view
    // sends must be in the mirror with the same value.
    let back = serde_json::to_value(&models).unwrap();
    for (key, value) in live["runtime"][0].as_object().unwrap() {
        assert!(
            back["runtime"][0].get(key).is_some(),
            "runtime row key {key} is missing from RuntimeStatus"
        );
        assert_eq!(&back["runtime"][0][key], value, "runtime row key {key}");
    }
    let mut live_rest = live.clone();
    let mut back_rest = back.clone();
    live_rest["runtime"] = json!([]);
    back_rest["runtime"] = json!([]);
    assert_eq!(live_rest, back_rest);

    // A local model that is not running answers nothing and starts nothing.
    let runs = w.runs();
    let live: Value = get(&w, "/audio-lab/api/voices?model=audio/asr")
        .await
        .json()
        .await
        .unwrap_or(Value::Null);
    assert_eq!(live["running"], json!(false), "{live}");
    round_trips::<VoicesNotRunning>("voices (not running)", &live);
    assert_eq!(w.runs(), runs, "listing voices started a container");
}

#[tokio::test]
async fn the_voice_library_answers_its_types() {
    let w = lab_world().await;

    let live: Value = get(&w, "/audio-lab/api/refs").await.json().await.unwrap();
    let list = round_trips::<ClipList>("refs", &live);
    assert_eq!(list.clips[0].name, "me.wav");
    assert_eq!(list.clips[0].server_path, "/models/voices/me.wav");

    // Upload with a typed transcript: nothing is transcribed.
    let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
        .file_name("typed.wav")
        .mime_str("audio/wav")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("transcript", "typed words")
        .part("file", part);
    let resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/refs", w.gw))
            .multipart(form)
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let up = round_trips::<ClipsUploaded>("upload", &live);
    assert_eq!(up.saved, ["typed.wav"]);
    assert!(up.transcribed.is_empty());

    // The setting names a model: an upload without a transcript is
    // transcribed, and a clip whose model fails is reported, not lost.
    let (status, body) = post_json(
        &w,
        "/api/op/settings_set_full",
        json!({"audio": {"voice_transcribe_alias": "audio/asr"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
        .file_name("new.wav")
        .mime_str("audio/wav")
        .unwrap();
    let resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/refs", w.gw))
            .multipart(reqwest::multipart::Form::new().part("file", part))
            .send()
            .await
            .unwrap();
    let live: Value = resp.json().await.unwrap();
    let up = round_trips::<ClipsUploaded>("upload, transcribed", &live);
    assert!(
        matches!(&up.transcribed[..], [ClipTranscription::Written(w)] if w.clip == "new.wav"),
        "{live}"
    );

    // A model that fails (once): the entry says why.
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&w.container)
        .await;
    {
        let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
            .file_name("bad.wav")
            .mime_str("audio/wav")
            .unwrap();
        let live: Value =
            w.gw.client()
                .post(format!("{}/audio-lab/api/refs", w.gw))
                .multipart(reqwest::multipart::Form::new().part("file", part))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let failed = round_trips::<ClipsUploaded>("upload, transcription failed", &live);
        assert!(
            matches!(&failed.transcribed[..], [ClipTranscription::Failed(_)]),
            "{live}"
        );
    }

    // Transcript set, transcribe, delete.
    let (status, live) = post_json(
        &w,
        "/audio-lab/api/refs/me.wav/text",
        json!({"transcript": "typed"}),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let ack = round_trips::<ClipsAck>("set text", &live);
    assert_eq!(
        ack.clips
            .iter()
            .find(|c| c.name == "me.wav")
            .unwrap()
            .transcript,
        "typed"
    );

    let (status, live) = post_json(
        &w,
        "/audio-lab/api/refs/me.wav/transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let done = round_trips::<ClipTranscribed>("transcribe", &live);
    assert_eq!(done.transcript, "hello there");

    let (status, live) = post_json(&w, "/audio-lab/api/refs/me.wav/delete", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let ack = round_trips::<ClipsAck>("delete", &live);
    assert!(ack.clips.iter().all(|c| c.name != "me.wav"));

    // Failures are `{error}`.
    let (status, live) = post_json(&w, "/audio-lab/api/refs/.hidden/delete", json!({})).await;
    assert_eq!(status, 400);
    round_trips::<LabError>("invalid clip", &live);
    let (status, live) = post_json(
        &w,
        "/audio-lab/api/refs/gone.wav/text",
        json!({"transcript": "x"}),
    )
    .await;
    assert_eq!(status, 404);
    round_trips::<LabError>("no such clip", &live);

    // A body the lab cannot read is a `{error}` too, with axum's status.
    let (status, live) = post_json(
        &w,
        "/audio-lab/api/refs/me.wav/text",
        json!({"transcript": 7}),
    )
    .await;
    assert_eq!(status, 422);
    assert!(!round_trips::<LabError>("mistyped transcript", &live)
        .error
        .is_empty());
    let (status, live) = post_json(
        &w,
        "/audio-lab/api/refs/me.wav/transcribe",
        json!({"alias": 7}),
    )
    .await;
    assert_eq!(status, 422);
    round_trips::<LabError>("mistyped alias", &live);
    // The hand-offs refuse a body as `/v1` does: the OpenAI envelope.
    let r =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech", w.gw))
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
    assert!(r.status().is_client_error());
    let live: Value = r.json().await.unwrap();
    round_trips::<OpenAiError>("speech with a broken body", &live);
    // The preview's failures are `{error}` as well.
    let missing = get(&w, "/audio-lab/api/refs/gone.wav").await;
    assert_eq!(missing.status(), 404);
    round_trips::<LabError>("preview of a missing clip", &missing.json().await.unwrap());
}

#[tokio::test]
async fn the_binary_answers_keep_their_content_types() {
    let w = lab_world().await;

    let clip = get(&w, "/audio-lab/api/refs/me.wav").await;
    assert_eq!(clip.status(), 200);
    assert_eq!(clip.headers()["content-type"], "audio/wav");
    assert_eq!(clip.bytes().await.unwrap().len(), 44 + 3200);

    let speech =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech", w.gw))
            .json(&json!({"model": "audio/tts", "input": "hello"}))
            .send()
            .await
            .unwrap();
    assert_eq!(speech.status(), 200);
    assert_eq!(speech.headers()["content-type"], "audio/wav");

    // Transcriptions: the plain answer and the detailed one.
    for (query, detailed) in [("", false), ("?details=1", true)] {
        let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
            .file_name("c.wav")
            .mime_str("audio/wav")
            .unwrap();
        let form = reqwest::multipart::Form::new()
            .text("model", "audio/asr")
            .part("file", part);
        let r =
            w.gw.client()
                .post(format!("{}/audio-lab/api/transcriptions{query}", w.gw))
                .multipart(form)
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["text"], "hello there");
        assert_eq!(v.get("words").is_some(), detailed, "{v}");
    }
}
