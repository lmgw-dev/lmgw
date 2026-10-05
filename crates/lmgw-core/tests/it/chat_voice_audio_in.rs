//! Voice turns the model hears (voice-audio-input design §2, §5), WP1: the
//! `chat_voice_audio_input` setting on both save paths and the self-admin
//! schema, a thread's and a folder's `voice.audio_input`, and the thread
//! JSON's `voice_resolved.audio_input` verdict — a local model that takes
//! audio hears the turn, a model lmgw does not run gets the transcript and
//! says why.
//! Mock upstreams only; nothing is started.
//!
//! WP2's request — on local rows behind the real gate — is `turns`; WP3's
//! bound session end to end (the hold, the veto, the row) is `session`,
//! and when its transcript does not come `transcripts`; a heard turn's
//! tools waiting for its row are `tools`, and the GPU claim they let go
//! meanwhile `tool_claim`; the `off` golden on a managed server is
//! `managed_off`.

mod managed_off;
mod session;
mod tool_claim;
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
    let res = settings_set(
        &state,
        patch(json!({ "chat_voice_audio_input": " Local " })),
    )
    .await
    .unwrap();
    assert!(
        res["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("chat_voice_audio_input")),
        "{res}"
    );
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "local");
    assert_eq!(
        get_json(&gw, "/api/settings-full").await["chat_voice_audio_input"],
        "local"
    );
    let e = settings_set(&state, patch(json!({ "chat_voice_audio_input": "any" })))
        .await
        .unwrap_err();
    assert!(e.contains("off, local"), "{e}");
    assert_eq!(state.snapshot().settings.chat_voice_audio_input, "local");

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
    assert_eq!(key["enum"], json!(["off", "local"]));
    let text = key["description"].as_str().unwrap();
    for word in [
        "experimental",
        "local models only",
        "a cloud chat model never gets audio",
        "a cloud speech-to-text model still receives each turn's audio",
    ] {
        assert!(text.contains(word), "{word}: {text}");
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

    settings_set(&state, patch(json!({ "chat_voice_audio_input": "local" })))
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
    assert_eq!(
        thread(&gw, local).await["voice_resolved"]["audio_input"],
        json!({ "value": "local", "source": "chat", "path": "audio", "model": "hears",
                "why": null })
    );
    let r = thread(&gw, cloud).await["voice_resolved"]["audio_input"].clone();
    assert_eq!(
        (r["path"].as_str(), r["model"].as_str()),
        (Some("transcript"), Some("plain"))
    );
    assert_eq!(
        r["why"],
        "plain is a model lmgw does not run: your voice stays on this machine"
    );

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
        json!({ "name": "Heard", "defaults": { "voice": { "audio_input": "local" } } }),
    )
    .await;
    assert_eq!(status, 200, "{folder}");
    assert_eq!(
        folder["defaults"]["voice"],
        json!({ "audio_input": "local" })
    );
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
    assert_eq!(t["voice"], json!({ "audio_input": "local" }));
    let r = &t["voice_resolved"]["audio_input"];
    assert_eq!(
        (
            r["value"].as_str(),
            r["source"].as_str(),
            r["path"].as_str()
        ),
        (Some("local"), Some("thread"), Some("audio")),
        "Settings say off; the folder's default made it the thread's own: {r}"
    );
}
