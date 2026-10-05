//! Voice-library clip transcripts (audio-class gap 5): a local
//! speech-to-text row writes a clip's `prompt_text` line — on request, for
//! every clip without one, or on upload when the owner chose a model — and
//! nothing else may hear the clip: not a cloud alias, not the GPU hold's
//! cloud fallback, not a model that is no speech-to-text row. The clips here
//! are a few synthetic bytes; no recording is in the repo.

use lmgw_core::config::{HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::audio_world::{wav_bytes, world, World};
use crate::support::audiocpp_gguf;

/// An ASR row `asr` on the world's container, which hears "hello there",
/// and one clip, `me.wav`, in the voice library.
async fn asr_world(fallback: Option<&str>) -> World {
    let w = world().await;
    w.row_with(
        "asr",
        "qwen3_asr",
        |_| {},
        |row| {
            row.task = "asr".into();
            if let Some(fb) = fallback {
                row.hold_fallback_mode = HoldFallbackMode::Alias;
                row.hold_fallback = Some(fb.into());
            }
        },
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello there"})))
        .mount(&w.container)
        .await;
    let voices = w.models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("me.wav"), wav_bytes(16_000, 1600)).unwrap();
    w
}

/// A cloud speech-to-text alias on its own wiremock upstream, which must
/// never be asked.
async fn cloud_asr(w: &World) -> MockServer {
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "leaked"})))
        .expect(0)
        .mount(&cloud)
        .await;
    let up = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "cloud-asr".into(),
            upstream_id: up,
            upstream_model_id: "whisper-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
            }})),
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    cloud
}

async fn post(w: &World, p: &str, body: Value) -> (u16, Value) {
    let resp =
        w.gw.client()
            .post(format!("{}{p}", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn prompt_text(w: &World) -> String {
    std::fs::read_to_string(w.models.path().join("voices/prompt_text")).unwrap_or_default()
}

/// The container's transcription requests.
async fn heard(w: &World) -> usize {
    w.container
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/v1/audio/transcriptions")
        .count()
}

#[tokio::test]
async fn a_clip_is_transcribed_by_a_local_model_and_its_transcript_recorded() {
    let w = asr_world(None).await;
    // No model named and none set: nothing is transcribed.
    let (status, body) = post(&w, "/audio-lab/api/refs/me.wav/transcribe", json!({})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("no transcription model"));
    assert_eq!(heard(&w).await, 0);

    let (status, body) = post(
        &w,
        "/audio-lab/api/refs/me.wav/transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["transcript"], "hello there");
    assert_eq!(body["transcript_source"], "asr:audio/asr");
    assert_eq!(prompt_text(&w), "me|hello there\n");

    // A TTS row's voice list shows the clip with its transcript.
    w.row("tts", "fish_audio", |r| {
        audiocpp_gguf::with_options(r, "fish_audio", &["reference_text"], &["offline"])
    })
    .await;
    let v = w.voices("tts").await;
    let me = v["lmgw"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "me")
        .unwrap()
        .clone();
    assert_eq!(me["transcript"], true, "{v}");
}

#[tokio::test]
async fn clips_without_a_transcript_are_transcribed_together_and_on_upload_when_chosen() {
    let w = asr_world(None).await;
    let voices = w.models.path().join("voices");
    std::fs::write(voices.join("you.wav"), wav_bytes(16_000, 1600)).unwrap();
    std::fs::write(voices.join("prompt_text"), "you|already said\n").unwrap();
    // A cloning model's notes name the clip that lacks one — Fish Audio S2
    // refuses it outright (`voice_needs_transcript`).
    w.row("tts", "fish_audio", |r| {
        audiocpp_gguf::with_options(r, "fish_audio", &["reference_text"], &["offline"])
    })
    .await;
    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let notes = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "audio/tts")
        .map(|m| m["notes"].to_string())
        .unwrap();
    assert!(
        notes.contains("refused (400 voice_needs_transcript) until they have one: me"),
        "{notes}"
    );

    let (status, body) = post(
        &w,
        "/api/op/voice_transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["transcribed"],
        json!([{"clip": "me.wav", "chars": 11, "transcript_source": "asr:audio/asr"}])
    );
    assert_eq!(body["already"], 1);
    assert!(
        !body.to_string().contains("hello there"),
        "the answer never carries the text: {body}"
    );
    assert_eq!(prompt_text(&w), "me|hello there\nyou|already said\n");

    // The setting: a cloud alias is refused, a local ASR row taken.
    let _cloud = cloud_asr(&w).await;
    let (status, body) = post(
        &w,
        "/api/op/settings_set_full",
        json!({"audio": {"voice_transcribe_alias": "cloud-asr"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body
        .to_string()
        .contains("not one of this machine's audio models"));
    let (status, body) = post(
        &w,
        "/api/op/settings_set_full",
        json!({"audio": {"voice_transcribe_alias": "audio/asr"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // An upload without a typed transcript is transcribed by it.
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
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["transcribed"],
        json!([{"clip": "new.wav", "transcript_source": "asr:audio/asr"}])
    );
    assert!(prompt_text(&w).contains("new|hello there"));
}

/// The privacy rule: the owner's voice reaches no cloud route — not the GPU
/// hold's fallback, not a cloud alias named, not a model that is no local
/// speech-to-text row. The cloud upstream's `expect(0)` is checked when it
/// drops.
#[tokio::test]
async fn the_owners_voice_never_reaches_a_cloud_route() {
    let w = asr_world(Some("cloud-asr")).await;
    let cloud = cloud_asr(&w).await;
    w.row("tts", "kokoro_tts", |_| {}).await;

    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();
    let (status, body) = post(
        &w,
        "/audio-lab/api/refs/me.wav/transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("holding the GPU"),
        "{body}"
    );
    let (status, body) = post(
        &w,
        "/api/op/voice_transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    lmgw_core::ops::hold_set(&w.state, false).await.unwrap();

    for (alias, why) in [
        ("cloud-asr", "not one of this machine's audio models"),
        ("audio/tts", "not speech-to-text"),
    ] {
        let (status, body) = post(
            &w,
            "/audio-lab/api/refs/me.wav/transcribe",
            json!({"alias": alias}),
        )
        .await;
        assert_eq!(status, 400, "{alias}: {body}");
        assert!(body["error"].as_str().unwrap().contains(why), "{body}");
    }

    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "the cloud was asked"
    );
    assert_eq!(heard(&w).await, 0, "nothing was heard at all");
    assert_eq!(prompt_text(&w), "", "nothing recorded");
    assert_eq!(w.runs(), 0, "nothing was started");
}
