//! Voice-library clip transcripts (audio-class gap 5): a speech-to-text
//! model writes a clip's `prompt_text` line — on request, for every clip
//! without one, or on upload when the owner chose a model. Capability, not
//! locality (changed 2026-10-06, the owner's ruling): a cloud speech-to-text
//! alias may transcribe, and the GPU hold's configured fallback does; a
//! model that does not transcribe never hears the clip. The clips here are a
//! few synthetic bytes; no recording is in the repo.

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

/// A cloud speech-to-text alias `cloud-asr` on its own wiremock upstream,
/// which hears "from the cloud", and a chat alias `cloud-chat` beside it.
async fn cloud_asr(w: &World) -> MockServer {
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "from the cloud"})))
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
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "cloud-chat".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat", "endpoints": ["/v1/chat/completions"], "source": "owner"
            }})),
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    cloud
}

/// The cloud's transcription requests.
async fn cloud_heard(cloud: &MockServer) -> usize {
    cloud
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/v1/audio/transcriptions")
        .count()
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
    crate::common::round_trips::<lmgw_api_types::audio_lab::VoiceTranscribed>(
        "voice_transcribe",
        &body,
    );
    assert_eq!(
        body["transcribed"],
        json!([{"clip": "me.wav", "chars": 11, "transcript_source": "asr:audio/asr",
                "answered_by": null, "fallback_reason": null, "by": "'audio/asr'"}])
    );
    assert_eq!(body["already"], 1);
    assert!(
        !body.to_string().contains("hello there"),
        "the answer never carries the text: {body}"
    );
    assert_eq!(prompt_text(&w), "me|hello there\nyou|already said\n");

    // The setting takes a speech-to-text model wherever it runs (changed
    // 2026-10-06), and refuses one that does not transcribe.
    let _cloud = cloud_asr(&w).await;
    let (status, body) = post(
        &w,
        "/api/op/settings_set_full",
        json!({"audio": {"voice_transcribe_alias": "cloud-chat"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("speech-to-text (asr)"), "{body}");
    let (status, body) = post(
        &w,
        "/api/op/settings_set_full",
        json!({"audio": {"voice_transcribe_alias": "cloud-asr"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
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
        json!([{"clip": "new.wav", "transcript_source": "asr:audio/asr",
                "answered_by": null, "fallback_reason": null, "by": "'audio/asr'"}])
    );
    assert!(prompt_text(&w).contains("new|hello there"));
}

/// Capability, not locality (changed 2026-10-06): under the GPU hold the ASR
/// row's configured fallback transcribes the clip, as it would any request,
/// and the request row names it; a cloud speech-to-text alias named
/// transcribes too; a model that does not transcribe — a TTS row, a chat
/// model, a fallback that is one — never hears the clip.
#[tokio::test]
async fn a_configured_fallback_transcribes_a_clip_and_only_speech_to_text_hears_one() {
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
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["transcript"], "from the cloud");
    // Who answered is said (review V1): the fallback, as one.
    assert_eq!(body["transcript_source"], "asr:cloud-asr", "{body}");
    assert_eq!(body["answered_by"], "cloud-asr");
    assert_eq!(body["fallback_reason"], "hold");
    assert_eq!(
        body["message"],
        "me.wav transcribed by 'cloud-asr' (the GPU hold's fallback for 'audio/asr')"
    );
    assert_eq!(cloud_heard(&cloud).await, 1, "the hold's fallback heard it");
    assert_eq!(heard(&w).await, 0, "the held row did not");
    assert_eq!(w.runs(), 0, "nothing was started");
    assert_eq!(prompt_text(&w), "me|from the cloud\n");
    let fallback: Option<String> =
        sqlx::query_scalar("SELECT fallback_reason FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert_eq!(
        fallback.as_deref(),
        Some("hold"),
        "the row says who answered"
    );
    lmgw_core::ops::hold_set(&w.state, false).await.unwrap();

    // A cloud speech-to-text alias, named.
    let (status, body) = post(
        &w,
        "/audio-lab/api/refs/me.wav/transcribe",
        json!({"alias": "cloud-asr"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(cloud_heard(&cloud).await, 2);
    assert_eq!(body["transcript_source"], "asr:cloud-asr");
    assert!(body["answered_by"].is_null(), "{body}");
    // The op says it too, a fallback named as one.
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();
    std::fs::write(w.models.path().join("voices/prompt_text"), "").unwrap();
    let (status, body) = post(
        &w,
        "/api/op/voice_transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["transcribed"][0]["transcript_source"], "asr:cloud-asr");
    assert_eq!(
        body["message"],
        "1 clip(s) transcribed by 'cloud-asr' (the GPU hold's fallback for 'audio/asr')"
    );
    lmgw_core::ops::hold_set(&w.state, false).await.unwrap();
    assert_eq!(cloud_heard(&cloud).await, 3);

    // Models that do not transcribe are refused before anything is sent.
    for (alias, why) in [
        ("audio/tts", "'audio/tts' is a 'tts' model"),
        ("cloud-chat", "'cloud-chat' is a 'chat' model"),
    ] {
        let (status, body) = post(
            &w,
            "/audio-lab/api/refs/me.wav/transcribe",
            json!({"alias": alias}),
        )
        .await;
        assert_eq!(status, 400, "{alias}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(why), "{alias}: {error}");
        assert!(error.contains("only by a speech-to-text model"), "{error}");
    }
    assert_eq!(cloud_heard(&cloud).await, 3, "the cloud heard nothing more");
    assert_eq!(heard(&w).await, 0);
    assert_eq!(w.runs(), 0, "nothing was started");
}

/// The hold's fallback must transcribe too: a chat alias as the ASR row's
/// fallback is refused before anything is sent.
#[tokio::test]
async fn a_fallback_that_does_not_transcribe_never_hears_a_clip() {
    let w = asr_world(Some("cloud-chat")).await;
    let cloud = cloud_asr(&w).await;
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();
    let (status, body) = post(
        &w,
        "/api/op/voice_transcribe",
        json!({"alias": "audio/asr"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string().contains("'cloud-chat' is a 'chat' model"),
        "{body}"
    );
    // Its catalog may be read to judge it; it heard nothing.
    assert_eq!(cloud_heard(&cloud).await, 0);
    assert_eq!(heard(&w).await, 0);
    assert_eq!(prompt_text(&w), "", "nothing recorded");
    // The refusal's row keeps the swap's headers (review V15).
    let fallback: Option<String> =
        sqlx::query_scalar("SELECT fallback_reason FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert_eq!(fallback.as_deref(), Some("hold"));
}

/// Review V11: admission's outside-VRAM verdict hands the clip to the row's
/// fallback after the settled name passed — that fallback is judged too.
/// A chat alias is refused `asr_required` before anything is sent, and
/// nothing was started for it.
#[tokio::test]
async fn admissions_fallback_that_does_not_transcribe_never_hears_a_clip() {
    use crate::chat_voice_dictation::{asr_chat_rows, world as gpu_world};
    use crate::support::gpu_world::GIB;

    let (g, _gw) = gpu_world(10 * GIB, 2).await;
    g.model("ears", 3 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "leaked"})))
        .mount(&cloud)
        .await;
    let up = store::insert_upstream(
        &g.state.db,
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
        &g.state.db,
        &NewAlias {
            alias: "cloud-chat".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat", "endpoints": ["/v1/chat/completions"], "source": "owner"
            }})),
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE local_models SET hold_fallback_mode = 'alias', hold_fallback = 'cloud-chat'
         WHERE model_id = 'ears'",
    )
    .execute(&g.state.db)
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 8 * GIB;
    }
    let e = lmgw_core::proxy::transcribe_voice_clip(
        &g.state,
        "ears",
        bytes::Bytes::from(wav_bytes(16_000, 1600)),
        "me.wav",
        "audio/wav",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), "asr_required", "{e}");
    assert!(
        e.to_string().contains("'cloud-chat' is a 'chat' model"),
        "{e}"
    );
    let posted: Vec<String> = cloud
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(posted.is_empty(), "nothing was sent: {posted:?}");
    assert!(g.runs().is_empty(), "nothing was started: {:?}", g.runs());
    assert!(g.world().transcriptions.is_empty());
    let fallback: Option<String> =
        sqlx::query_scalar("SELECT fallback_reason FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(&g.state.db)
            .await
            .unwrap();
    assert_eq!(fallback.as_deref(), Some("external_vram"));
}
