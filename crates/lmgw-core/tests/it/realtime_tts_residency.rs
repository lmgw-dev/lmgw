//! A voice session's local TTS model and the VRAM ledger (realtime design
//! §9.1, §9.4): audio.cpp loads a lazy row's weights on its first request,
//! and the ledger keeps that load free until a clause has been synthesized.
//! Since live run 3b (D3') the connect's warm sends that clause itself —
//! one word in the session's voice — so the model is loaded, and nothing
//! pending, before the session's first response.

use std::time::Duration;

use lmgw_core::runtime::Class;
use lmgw_core::store::{self, NewAudioModel};
use serde_json::json;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    add_chat_aliases, chat_fake, events_until, gpu_gateway, send, user_text, Turn,
};
use crate::support::realtime_tts::spoken_session;

/// What the ledger keeps free for the TTS container, once it is up.
async fn pending(g: &Gpu) -> Option<Option<u64>> {
    let view = g.state.vram.view(&g.state).await;
    view.resident
        .iter()
        .find(|r| r.container == Class::Audio && r.model == "voice-tts" && r.state == "ready")
        .map(|r| r.pending_bytes)
}

#[tokio::test]
async fn a_warmed_voice_is_loaded_before_its_first_response() {
    let g = Gpu::new(24 * GIB, 3, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    // A 1 GiB model directory: what an unlearned row is charged.
    let dir = g.models_dir().join("pocket");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("model.gguf"))
        .unwrap()
        .set_len(GIB)
        .unwrap();
    // Pocket TTS keeps its built-in voice as a file of the model root, which
    // is how lmgw's voice list knows `alba` (audio-class gap 1). Empty: the
    // directory's size is what the row is charged.
    std::fs::create_dir_all(dir.join("embeddings")).unwrap();
    std::fs::write(dir.join("embeddings/alba.safetensors"), b"").unwrap();
    store::insert_audio_model(
        &g.state.db,
        &NewAudioModel {
            model_id: "voice-tts".into(),
            family: "pocket_tts".into(),
            path: "pocket".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
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
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    let models = g.models_dir().display().to_string();
    let addr = gpu_gateway(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = "audio/voice-tts".into();
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = true;
    })
    .await;
    chat.push(Turn::text(&["Hallo du. "]));
    let (mut ws, _) = spoken_session(&addr, &[], 200, json!({})).await;

    // The connect warmed it and loaded it: one warm-up clause answered,
    // and the load is no longer kept free.
    let mut warmed = None;
    for _ in 0..500 {
        warmed = pending(&g).await;
        if warmed == Some(None) && !g.world().speeches.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        warmed,
        Some(None),
        "the warm's clause loaded it, so nothing is pending any more"
    );
    assert_eq!(g.world().speeches, vec!["voice-tts".to_string()]);
    assert_eq!(g.world().speech_bodies[0]["voice"], "alba");

    send(&mut ws, user_text("Hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.done").await;
    assert_eq!(
        g.world().speeches,
        vec!["voice-tts".to_string(), "voice-tts".to_string()]
    );
    assert_eq!(pending(&g).await, Some(None));
}
