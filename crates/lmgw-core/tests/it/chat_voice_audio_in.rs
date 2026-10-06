//! Voice turns the model hears (voice-audio-input design §2, §5), WP1: the
//! `chat_voice_audio_input` setting on both save paths and the self-admin
//! schema, a thread's and a folder's `voice.audio_input`, and the thread
//! JSON's `voice_resolved.audio_input` verdict — a model that takes audio
//! hears the turn wherever it runs, one lmgw cannot judge gets the
//! transcript and says why (changed 2026-10-06: capability, not locality).
//! Mock upstreams only; nothing is started.
//!
//! WP2's request — on local rows behind the real gate — is `turns`; WP3's
//! bound session end to end (the hold, the veto, the row) is `session`,
//! and when its transcript does not come `transcripts`; a heard turn's
//! tools waiting for its row are `tools`, and the GPU claim they let go
//! meanwhile `tool_claim`; the `off` golden on a managed server is
//! `managed_off`; a turn handed to a fallback — the hold's, admission's —
//! is `fallbacks`; what a llama-server's `/props` says of audio, `props`;
//! a connection that drops under the audio, `drops`; a hold that comes on
//! while a heard turn's tools wait, `tool_hold`.

mod drops;
mod fallbacks;
mod managed_off;
mod props;
mod session;
mod tool_claim;
mod tool_hold;
mod tools;
mod transcripts;
mod turns;

use lmgw_core::config::{HoldFallbackMode, LlamaParams};
use lmgw_core::ops::settings_set;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewLocalModel};
use serde_json::{json, Value};

use crate::chat_attach_kinds::{new_thread, patch};
use crate::chat_voice_settings::{gateway, get_json, mocks, post, set_voice, thread};

/// A public local chat row whose capabilities say it takes audio (an owner
/// override: the test has no weights to read).
async fn hearing_row(state: &SharedState, model_id: &str) {
    store::insert_local_model(
        &state.db,
        &NewLocalModel {
            model_id: model_id.into(),
            gguf_path: format!("{model_id}.gguf"),
            params: LlamaParams::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::default(),
            hold_fallback: None,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "chat", "input_modalities": ["text", "image", "audio"]
            } })),
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn the_setting_defaults_to_off_and_saves_on_both_paths() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    assert_eq!(
        get_json(&gw, "/api/settings-full").await["chat_voice_audio_input"],
        "off"
    );
    assert_eq!(
        lmgw_core::ops::settings(&state).await.unwrap()["chat_voice_audio_input"],
        "off"
    );

    // The self-admin path, case folded, read back through both reads.
    let res = settings_set(&state, patch(json!({ "chat_voice_audio_input": " On " })))
        .await
        .unwrap();
    assert!(
        res["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("chat_voice_audio_input")),
        "{res}"
    );
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "on");
    assert_eq!(
        get_json(&gw, "/api/settings-full").await["chat_voice_audio_input"],
        "on"
    );
    let e = settings_set(&state, patch(json!({ "chat_voice_audio_input": "any" })))
        .await
        .unwrap_err();
    assert!(e.contains("off, on"), "{e}");
    // The value's name before 2026-10-06 is refused on input.
    let e = settings_set(&state, patch(json!({ "chat_voice_audio_input": "local" })))
        .await
        .unwrap_err();
    assert!(e.contains("'local' is not one of off, on"), "{e}");
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "on");

    // The dashboard path takes and refuses the same.
    let (status, res) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "chat_voice_audio_input": "off" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "off");
    let (status, res) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "chat_voice_audio_input": "cloud" }),
    )
    .await;
    assert_eq!(status, 400, "{res}");
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "off");
}

#[test]
fn the_settings_tool_lists_the_key_with_its_values() {
    let (tool, _) = lmgw_core::mcp::selfadmin::full_catalog()
        .into_iter()
        .find(|(t, _)| t["name"] == "lmgw__settings_set")
        .expect("lmgw__settings_set");
    let key = &tool["inputSchema"]["properties"]["chat_voice_audio_input"];
    assert_eq!(key["type"], "string", "{key}");
    assert_eq!(key["enum"], json!(["off", "on"]));
    let text = key["description"].as_str().unwrap();
    for word in [
        "experimental",
        "the model that answers it",
        "configured fallback is always used, wherever it runs",
        "a capabilities override with task chat and input_modalities text and audio",
        "a passthrough model needs an alias",
        "not Anthropic",
        "a cloud speech-to-text model still receives each turn's audio",
    ] {
        assert!(text.contains(word), "{word}: {text}");
    }
    // Capability, not locality (changed 2026-10-06).
    for gone in ["local models only", "never gets audio", "lmgw runs"] {
        assert!(!text.contains(gone), "{gone}: {text}");
    }
}

#[tokio::test]
async fn the_thread_json_says_who_hears_the_turn_and_a_thread_overrides_it() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    hearing_row(&state, "hears").await;
    let local = new_thread(&gw, "hears", false).await;
    let cloud = new_thread(&gw, "plain", false).await;

    // Off by default: the transcript, and the level that says so.
    assert_eq!(
        thread(&gw, local).await["voice_resolved"]["audio_input"],
        json!({ "value": "off", "source": "chat", "path": "transcript", "model": "hears",
                "why": "audio input is off (Settings → Chat → Voice)" })
    );

    settings_set(&state, patch(json!({ "chat_voice_audio_input": "on" })))
        .await
        .unwrap();
    // No speech recognition: only a transcript can veto noise (WP3
    // review #2).
    assert_eq!(
        thread(&gw, local).await["voice_resolved"]["audio_input"]["why"],
        "no speech recognition is set up, and only a transcript tells your words from noise"
    );
    settings_set(&state, patch(json!({ "chat_stt_alias": "my-asr" })))
        .await
        .unwrap();
    // With what a GPU block would bring (decision D3): this gateway sets no
    // fallback, so the turn would be refused.
    assert_eq!(
        thread(&gw, local).await["voice_resolved"]["audio_input"],
        json!({ "value": "on", "source": "chat", "path": "audio", "model": "hears",
                "why": null,
                "blocked": { "path": "transcript", "model": "hears",
                             "why": "hears has no fallback, so the turn is refused" } })
    );
    // A cloud model is judged by capability alone (changed 2026-10-06): its
    // catalog lists nothing, so lmgw cannot tell, and says how to make it
    // hear. No block touches it. (The first look may find the catalog still
    // being read in the background, review V9.)
    let mut r = Value::Null;
    for _ in 0..50 {
        r = thread(&gw, cloud).await["voice_resolved"]["audio_input"].clone();
        if !r["why"]
            .as_str()
            .unwrap_or_default()
            .contains("still reading")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        (r["path"].as_str(), r["model"].as_str()),
        (Some("transcript"), Some("plain"))
    );
    assert_eq!(
        r["why"],
        "lmgw cannot tell whether plain takes audio: if it does, give its alias a capabilities \
         override with task chat and input modalities text and audio"
    );
    assert!(r.get("blocked").is_none(), "{r}");

    // The thread's own value wins; strict on input; the answer carries it.
    let (status, res) = set_voice(&gw, local, json!({ "audio_input": "off" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice"], json!({ "audio_input": "off" }));
    assert_eq!(
        res["voice_resolved"]["audio_input"]["why"],
        "audio input is off (this thread)"
    );
    assert_eq!(res["voice_resolved"]["audio_input"]["source"], "thread");
    let (status, res) = set_voice(&gw, local, json!({ "audio_input": "any" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(res["message"].as_str().unwrap().contains("any"), "{res}");
    assert_eq!(thread(&gw, local).await["voice"]["audio_input"], "off");
    // Cleared, it inherits again.
    set_voice(&gw, local, Value::Null).await;
    assert_eq!(
        thread(&gw, local).await["voice_resolved"]["audio_input"]["path"],
        "audio"
    );
}

#[tokio::test]
async fn a_folder_carries_the_value_into_a_new_thread() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    hearing_row(&state, "hears").await;
    settings_set(&state, patch(json!({ "chat_stt_alias": "my-asr" })))
        .await
        .unwrap();
    let (status, folder) = post(
        &gw,
        "/chat/api/folders",
        json!({ "name": "Heard", "defaults": { "voice": { "audio_input": "on" } } }),
    )
    .await;
    assert_eq!(status, 200, "{folder}");
    assert_eq!(folder["defaults"]["voice"], json!({ "audio_input": "on" }));
    let fid = folder["id"].as_i64().unwrap();
    let (status, res) = post(
        &gw,
        &format!("/chat/api/folders/{fid}"),
        json!({ "defaults": { "voice": { "audio_input": "always" } } }),
    )
    .await;
    assert_eq!(status, 400, "{res}");

    let (status, t) = post(
        &gw,
        "/chat/api/threads",
        json!({ "model_alias": "hears", "folder_id": fid }),
    )
    .await;
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["voice"], json!({ "audio_input": "on" }));
    let r = &t["voice_resolved"]["audio_input"];
    assert_eq!(
        (
            r["value"].as_str(),
            r["source"].as_str(),
            r["path"].as_str()
        ),
        (Some("on"), Some("thread"), Some("audio")),
        "Settings say off; the folder's default made it the thread's own: {r}"
    );
}

/// F7: `local`, the value's name before 2026-10-06, as a store written
/// before then holds it — the settings blob, a thread's voice, a folder's
/// default — reads as `on`; on input it is refused (above).
#[tokio::test]
async fn a_stored_local_reads_as_on() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    hearing_row(&state, "hears").await;
    let tid = new_thread(&gw, "hears", false).await;
    let (status, folder) = post(
        &gw,
        "/chat/api/folders",
        json!({ "name": "Heard", "defaults": {} }),
    )
    .await;
    assert_eq!(status, 200, "{folder}");
    let fid = folder["id"].as_i64().unwrap();
    // Written as a build before the rename wrote it, past every check.
    let mut s = state.snapshot().settings.clone();
    s.chat_voice_audio_input = "local".into();
    store::save_settings(&state.db, &s).await.unwrap();
    for sql in [
        "UPDATE chat_threads SET voice = '{\"audio_input\":\"local\"}'",
        "UPDATE chat_folders SET defaults = '{\"voice\":{\"audio_input\":\"local\"}}'",
    ] {
        sqlx::query(sql).execute(&state.db).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "on");
    let t = thread(&gw, tid).await;
    assert_eq!(t["voice"], json!({ "audio_input": "on" }), "{t}");
    assert_eq!(t["voice_resolved"]["audio_input"]["value"], "on");
    let (status, t) = post(
        &gw,
        "/chat/api/threads",
        json!({ "model_alias": "hears", "folder_id": fid }),
    )
    .await;
    assert_eq!(status, 200, "{t}");
    assert_eq!(
        t["voice"],
        json!({ "audio_input": "on" }),
        "the folder's default"
    );
    // The next save of any setting stores the new name.
    settings_set(&state, patch(json!({ "chat_read_aloud": false })))
        .await
        .unwrap();
    let stored: String = sqlx::query_scalar(
        "SELECT json_extract(value, '$.chat_voice_audio_input') FROM settings \
         WHERE key = 'settings'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(stored, "on");
}

/// Review V9: the thread view never waits on a provider's catalog. A cloud
/// model whose catalog lmgw has not read yet reads the transcript, and the
/// verdict says it is still being read; the read goes on in the background,
/// and the next look knows the model hears.
#[tokio::test]
async fn the_verdict_never_waits_on_a_catalog_it_has_not_read() {
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    settings_set(
        &state,
        patch(json!({ "chat_voice_audio_input": "on", "chat_stt_alias": "my-asr" })),
    )
    .await
    .unwrap();
    // A provider slow to list its models: one that hears.
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "omni", "architecture": {
                    "input_modalities": ["text", "audio"], "output_modalities": ["text"]
                }}]}))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&slow)
        .await;
    let up = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "slow-up".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", slow.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
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
        &store::NewAlias {
            alias: "omni".into(),
            upstream_id: up,
            upstream_model_id: "omni".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = new_thread(&gw, "omni", false).await;
    let asked = Instant::now();
    let v = thread(&gw, tid).await["voice_resolved"]["audio_input"].clone();
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "the view did not wait for the catalog: {:?}",
        asked.elapsed()
    );
    assert_eq!(v["path"], "transcript", "{v}");
    assert_eq!(
        v["why"],
        "lmgw is still reading the model list omni is in, so it cannot tell yet whether it \
         takes audio"
    );
    // The background read lands; the next look knows.
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let v = thread(&gw, tid).await["voice_resolved"]["audio_input"].clone();
        if v["path"] == "audio" {
            assert_eq!(v["model"], "omni");
            break;
        }
        assert!(Instant::now() < until, "never known: {v}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
