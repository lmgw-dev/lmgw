//! A spoken answer's TTS model under the GPU hold, end to end (realtime
//! design §8.2, §9.1, §9.2; B2 review 9). The TTS is a local audio model on
//! the fake GPU world, so its claim is the registry's: the synthesis bound
//! keeps it for most of a long answer's playback, and the hold switched on
//! mid-answer lifts the bound — the rest is synthesized at once and the
//! model let go, long before the answer has played.

use std::time::Duration;

use lmgw_core::store::{self, NewAudioModel};
use serde_json::json;
use tokio::time::Instant;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    add_chat_aliases, chat_fake, events_until, gpu_gateway, send, user_text, Turn,
};
use crate::support::realtime_tts::spoken_session;

/// The local TTS model, as the session names it.
const TTS: &str = "audio/voice-tts";

async fn local_tts(g: &Gpu) {
    // Pocket TTS keeps its built-in voice as a file of the model root, which
    // is how lmgw's voice list knows `alba` (audio-class gap 1).
    let voices = g.models_dir().join("pocket/embeddings");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.safetensors"), b"").unwrap();
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
    g.state.reload_snapshot().await.unwrap();
}

/// The TTS model's claims in flight, once it runs.
fn claims(g: &Gpu) -> Option<u32> {
    g.state
        .runtime()
        .list()
        .iter()
        .find(|v| v.model_id == "voice-tts")
        .map(|v| v.in_flight)
}

/// Twenty one-second clauses asked of the local TTS, two seconds ahead of
/// the paced send: once 800 ms in, the bound holds and the model is claimed.
/// The session and the chat model's fake, both kept for the answer.
async fn a_long_answer(
    g: &Gpu,
) -> (
    crate::support::realtime_fakes::Ws,
    crate::support::realtime_fakes::ChatFake,
) {
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    let models = g.models_dir().display().to_string();
    let addr = gpu_gateway(g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = TTS.into();
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = false;
    })
    .await;
    // Twenty sentences: twenty clauses of a second of speech each. One
    // sentence to a line: each clause is its own TTS request, none joins
    // another across a line (`speech/batch.rs`).
    let parts: Vec<&'static str> = (1..=20)
        .map(|k| &*Box::leak(format!("Satz Nummer {k}.\n").into_boxed_str()))
        .collect();
    chat.push(Turn::text(&parts));
    let (mut ws, _) = spoken_session(
        &addr,
        &[],
        200,
        json!({"lmgw": {"output_lead_ms": 200, "synthesis_ahead_s": 2}}),
    )
    .await;
    send(&mut ws, user_text("Erzähl")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_audio.delta").await;

    // The bound holds synthesis two seconds ahead of the paced send, and
    // the model stays claimed meanwhile.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = g.world().speeches.len();
    assert!(
        (1..10).contains(&before),
        "{before} clauses: the bound holds"
    );
    assert_eq!(claims(g), Some(1), "claimed while it waits");
    (ws, chat)
}

/// Until all twenty clauses are synthesized and the claim is gone, well
/// inside the fifteen seconds the answer still has to play.
async fn lifted_and_let_go(g: &Gpu) {
    let start = Instant::now();
    loop {
        let (spoken, held) = (g.world().speeches.len(), claims(g));
        if spoken == 20 && held == Some(0) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "after {:?}: {spoken} clauses, {held:?} claims",
            start.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn under_the_gpu_hold_a_speaking_answer_lets_its_tts_model_go_long_before_it_has_played() {
    let g = Gpu::new(24 * GIB, 3, 5).await;
    local_tts(&g).await;
    let _session = a_long_answer(&g).await;

    // The hold comes on: at the writer's next release the bound lifts, the
    // rest is synthesized at once, and the claim goes — while some fifteen
    // seconds of the answer are still to play.
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    lifted_and_let_go(&g).await;
}

/// The same answer from a TTS row on the CPU: the hold has no claim on it,
/// so the bound and the claim stay — the answer is synthesized as it plays.
/// A benchmark's lease covers it, and its drain waits for the claim: the
/// lease lifts the bound and the model is let go after the last clause.
#[tokio::test]
async fn a_tts_model_on_the_cpu_keeps_its_bound_under_the_hold_and_not_under_a_benchmark() {
    let g = Gpu::new(24 * GIB, 3, 5).await;
    local_tts(&g).await;
    sqlx::query("UPDATE audio_models SET backend = 'cpu' WHERE model_id = 'voice-tts'")
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let _session = a_long_answer(&g).await;

    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let before = g.world().speeches.len();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = g.world().speeches.len();
    assert!(
        after < 20 && after <= before + 2,
        "{before} -> {after}: the bound holds"
    );
    assert_eq!(claims(&g), Some(1), "still claimed under the hold");

    let _lease = lmgw_core::bench::lease::LeaseGuard::take(&g.state, 7, "chat-model");
    lifted_and_let_go(&g).await;
}
