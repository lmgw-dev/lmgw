//! Chat voice resolution facts (chat-voice design §2.3, WP1 review M1, m1,
//! m3, n3, WP7 review M2): what the GPU hold (and a benchmark run's lease)
//! would answer with for a local audio row, a CPU row's fallback for the
//! lease, an alias served off this machine, a named fallback
//! that cannot stand in, the voice that belongs to another TTS model, and
//! the thread list that resolves nothing. Mock upstreams and stored rows
//! only; nothing is started.

use lmgw_core::config::{HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewAudioModel, NewUpstream};
use serde_json::{json, Value};

use crate::chat_attach_kinds::{new_thread, patch};
use crate::chat_voice_settings::{gateway, get_json, mocks, thread};

/// A local audio row (`audio/<id>`), with its hold fallback.
async fn audio_row(
    state: &SharedState,
    id: &str,
    task: &str,
    backend: Option<&str>,
    fallback: Option<&str>,
) {
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: id.into(),
            family: "pocket_tts".into(),
            path: id.into(),
            task: task.into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: backend.map(str::to_string),
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: if fallback.is_some() {
                HoldFallbackMode::Alias
            } else {
                HoldFallbackMode::Inherit
            },
            hold_fallback: fallback.map(str::to_string),
        },
    )
    .await
    .unwrap();
}

/// A provider's text-to-speech alias `cloud-tts` (a generic upstream: not
/// on this machine), and three audio rows: `gpu-tts` falling back to it
/// under the hold, `cpu-asr` on the CPU (a fallback only a benchmark run's
/// lease needs), and `gpu-asr` whose fallback is itself local.
async fn rows(state: &SharedState, provider: &wiremock::MockServer) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "provider".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", provider.uri()),
            api_key: Some("sk-test".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "cloud-tts".into(),
            upstream_id: up,
            upstream_model_id: "tts-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    audio_row(state, "gpu-tts", "tts", None, Some("cloud-tts")).await;
    audio_row(state, "cpu-asr", "asr", Some("cpu"), Some("cloud-tts")).await;
    audio_row(state, "gpu-asr", "asr", None, Some("audio/gpu-tts")).await;
    state.reload_snapshot().await.unwrap();
}

/// Store `voice` as the thread's own, past the settings route's alias
/// checks: resolution reads what is stored.
async fn store_voice(state: &SharedState, tid: i64, voice: Value) {
    sqlx::query("UPDATE chat_threads SET voice = ?1 WHERE id = ?2")
        .bind(voice.to_string())
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_resolution_names_hold_fallbacks_cpu_rows_and_aliases_off_this_machine() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    rows(&state, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;

    // A GPU TTS row with a provider as its hold fallback; a CPU ASR row,
    // which the hold never stops, but a benchmark run's lease does: its
    // fallback is offered too, and `cpu` tells the page the hold leaves it
    // alone (WP7 review M2).
    store_voice(
        &state,
        tid,
        json!({ "tts_alias": "audio/gpu-tts", "asr_alias": "audio/cpu-asr" }),
    )
    .await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["tts"]["alias"], "audio/gpu-tts", "{r}");
    assert_eq!(r["tts"]["local"], true);
    assert_eq!(r["tts"]["managed"], true);
    assert_eq!(r["tts"]["cpu"], false);
    assert_eq!(
        r["tts"]["fallback"],
        json!({ "alias": "cloud-tts", "local": false })
    );
    assert_eq!(r["tts"]["fallback_unusable"], Value::Null);
    assert_eq!(r["asr"]["alias"], "audio/cpu-asr");
    assert_eq!(r["asr"]["local"], true);
    assert_eq!(r["asr"]["cpu"], true);
    assert_eq!(
        r["asr"]["fallback"],
        json!({ "alias": "cloud-tts", "local": false })
    );
    assert_eq!(r["problems"], json!([]));

    // The provider's alias itself: off this machine, no hold involved. A
    // GPU ASR row whose fallback is itself local: the hold would refuse,
    // and the page can say why.
    store_voice(
        &state,
        tid,
        json!({ "tts_alias": "cloud-tts", "asr_alias": "audio/gpu-asr" }),
    )
    .await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["tts"]["local"], false, "{r}");
    assert_eq!(r["tts"]["managed"], false);
    assert_eq!(r["tts"]["cpu"], false);
    assert_eq!(r["tts"]["fallback"], Value::Null);
    assert_eq!(r["asr"]["fallback"], Value::Null);
    assert_eq!(
        r["asr"]["fallback_unusable"],
        json!({ "alias": "audio/gpu-tts", "why": "is itself a local model" })
    );
}

#[tokio::test]
async fn a_voice_chosen_for_the_chats_model_is_not_taken_by_a_thread_speaking_with_another() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    rows(&state, &stt2).await;
    lmgw_core::ops::settings_set(
        &state,
        patch(json!({
            "chat_tts_alias": "my-tts",
            "chat_voice": "alba",
            "realtime": { "default_voice": "M5" },
        })),
    )
    .await
    .unwrap();
    let tid = new_thread(&gw, "plain", false).await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["tts"]["source"], "chat", "{r}");
    assert_eq!(r["voice"]["name"], "alba");
    assert_eq!(r["voice"]["source"], "chat");

    // The thread speaks with another model: Settings → Chat's voice is not
    // its voice, realtime's default is only inherited (no name goes out),
    // and the page is told why.
    store_voice(&state, tid, json!({ "tts_alias": "cloud-tts" })).await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["tts"]["source"], "thread", "{r}");
    assert_eq!(r["tts"]["inherited"], "my-tts");
    assert_eq!(r["voice"]["name"], Value::Null);
    assert_eq!(r["voice"]["source"], "realtime");
    assert_eq!(r["voice"]["inherits"], "M5");
    let note = r["voice"]["note"].as_str().unwrap();
    assert!(
        note.contains("'alba'") && note.contains("'my-tts'"),
        "{note}"
    );
}

#[tokio::test]
async fn the_thread_list_carries_each_voice_but_resolves_none() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    store_voice(&state, tid, json!({ "voice": "alba" })).await;
    let temp = new_thread(&gw, "plain", true).await;
    let list = get_json(&gw, "/chat/api/threads").await;
    let row = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == tid)
        .unwrap()
        .clone();
    assert_eq!(row["voice"], json!({ "voice": "alba" }));
    assert!(row.get("voice_resolved").is_none(), "{row}");
    let t = list["temporary"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == temp)
        .unwrap()
        .clone();
    assert!(t.get("voice_resolved").is_none(), "{t}");
    // The open thread has it.
    assert!(thread(&gw, tid).await["voice_resolved"].is_object());
}
