//! The Audio lab's voice list (`GET /audio-lab/api/voices`): lmgw's own
//! catalog for a local row, read without a start — also under the GPU hold —
//! plus the names a container that is already up adds.

use lmgw_api_types::audio_lab::{AudioLabModels, LabVoiceKind, LabVoices, LabVoicesSource};
use lmgw_core::config::{HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::store;
use lmgw_core::store::{NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::MockServer;
use wiremock::{Mock, ResponseTemplate};

use super::get;
use crate::common::round_trips;
use crate::support::audio_world::{wav_bytes, world, World};
use crate::support::audiocpp_gguf;

/// A Magpie row (five shipped voices) with one preset, and one clip in the
/// voice library that the class's `voice_dir` points at.
async fn magpie_world() -> World {
    magpie_world_with(|_| {}).await
}

/// [`magpie_world`], with `tweak` applied to the row.
async fn magpie_world_with(tweak: impl FnOnce(&mut store::NewAudioModel)) -> World {
    let w = world().await;
    let mut s = store::load_settings(&w.state.db).await.unwrap();
    s.audio.voice_dir = "/models/voices".into();
    store::save_settings(&w.state.db, &s).await.unwrap();
    w.row_with("magpie", "magpie_tts", audiocpp_gguf::magpie, |row| {
        row.voice_presets
            .insert("narrator".into(), json!({"voice_id": "Jason"}));
        tweak(row);
    })
    .await;
    w.answer_wav().await;
    let voices = w.models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("me.example.wav"), wav_bytes(16_000, 1600)).unwrap();
    w.state.reload_snapshot().await.unwrap();
    w
}

async fn lab_voices(w: &World, model: &str) -> (Value, LabVoices) {
    let resp = get(w, &format!("/audio-lab/api/voices?model=audio/{model}")).await;
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let typed = round_trips::<LabVoices>("lab voices", &live);
    (live, typed)
}

fn kind_of(v: &LabVoices, id: &str) -> LabVoiceKind {
    v.voices
        .iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no voice {id} in {:?}", v.voices))
        .kind
}

async fn request_logs(w: &World) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&w.state.db)
        .await
        .unwrap()
}

/// No runtime row exists for the audio class: nothing was admitted.
async fn no_container(w: &World) {
    let live: Value = get(w, "/audio-lab/api/models").await.json().await.unwrap();
    let models: AudioLabModels = serde_json::from_value(live).unwrap();
    assert!(models.runtime.is_empty(), "{:?}", models.runtime);
    assert_eq!(w.runs(), 0, "listing voices started a container");
}

#[tokio::test]
async fn a_model_that_is_not_running_lists_its_catalog_and_starts_nothing() {
    let w = magpie_world().await;

    let (_, v) = lab_voices(&w, "magpie").await;
    assert_eq!(v.source, LabVoicesSource::Catalog);
    assert!(!v.held && !v.engine_asked);
    for shipped in ["Aria", "Jason", "John", "Leo", "Sofia"] {
        assert_eq!(kind_of(&v, shipped), LabVoiceKind::Native, "{shipped}");
    }
    assert_eq!(kind_of(&v, "narrator"), LabVoiceKind::Preset);
    assert_eq!(kind_of(&v, "me.example"), LabVoiceKind::Library);
    assert_eq!(v.default.as_deref(), Some("Aria"));
    no_container(&w).await;
}

#[tokio::test]
async fn under_the_gpu_hold_the_catalog_is_still_listed_and_marked_held() {
    let w = magpie_world().await;
    let mut s = store::load_settings(&w.state.db).await.unwrap();
    s.hold.active = true;
    store::save_settings(&w.state.db, &s).await.unwrap();
    w.state.reload_snapshot().await.unwrap();

    let (_, v) = lab_voices(&w, "magpie").await;
    assert!(v.held);
    assert_eq!(kind_of(&v, "Jason"), LabVoiceKind::Native);
    assert_eq!(kind_of(&v, "me.example"), LabVoiceKind::Library);
    no_container(&w).await;
}

#[tokio::test]
async fn a_running_container_adds_the_names_only_it_knows() {
    let w = magpie_world().await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"voices": ["Aria", "Jason", "mounted-clip.example"]})),
        )
        .mount(&w.container)
        .await;
    let resp = w
        .speak(json!({"model": "audio/magpie", "input": "hello"}))
        .await;
    assert_eq!(resp.status(), 200);
    let runs = w.runs();
    // The idle age counts whole seconds: let it reach one, so a read that
    // stamped `last_used` would show as an age back at zero.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let age = || {
        w.state
            .runtime()
            .list()
            .into_iter()
            .find(|e| e.model_id == "magpie")
            .expect("the container is up")
            .last_used_age_seconds
    };
    let (age_before, logs_before) = (age(), request_logs(&w).await);
    assert!(age_before >= 1, "{age_before}");

    let (_, v) = lab_voices(&w, "magpie").await;
    assert!(age() >= age_before, "the read reset the idle timer");
    assert_eq!(request_logs(&w).await, logs_before, "the read was logged");
    assert!(v.engine_asked);
    assert_eq!(kind_of(&v, "mounted-clip.example"), LabVoiceKind::Engine);
    // What the catalog has keeps its own kind; the engine adds, not renames.
    assert_eq!(kind_of(&v, "Jason"), LabVoiceKind::Native);
    assert_eq!(
        v.voices.iter().filter(|e| e.id == "Jason").count(),
        1,
        "{:?}",
        v.voices
    );
    assert_eq!(w.runs(), runs, "the read started another container");
}

#[tokio::test]
async fn a_held_row_with_a_cloud_fallback_lists_its_catalog_and_names_the_fallback() {
    let w = magpie_world_with(|row| {
        row.hold_fallback_mode = HoldFallbackMode::Alias;
        row.hold_fallback = Some("cloud-tts".into());
    })
    .await;
    // The fallback's provider: any request reaching it fails the test.
    let cloud = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(404))
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
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "cloud-tts".into(),
            upstream_id: up,
            upstream_model_id: "tts-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            }})),
        },
    )
    .await
    .unwrap();
    let mut s = store::load_settings(&w.state.db).await.unwrap();
    s.hold.active = true;
    store::save_settings(&w.state.db, &s).await.unwrap();
    w.state.reload_snapshot().await.unwrap();

    let (live, v) = lab_voices(&w, "magpie").await;
    assert!(v.held);
    assert_eq!(v.fallback.as_deref(), Some("cloud-tts"));
    assert_eq!(v.source, LabVoicesSource::Catalog);
    assert!(!v.engine_asked);
    assert_eq!(kind_of(&v, "Jason"), LabVoiceKind::Native);
    assert_eq!(live["fallback"], "cloud-tts");
    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "the lab asked the fallback's provider"
    );
    no_container(&w).await;
}
