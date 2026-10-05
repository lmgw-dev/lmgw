//! A heard tool turn's GPU claim while its tools wait for the user row
//! (voice-audio-input design §3.4, verification review): the loop lets its
//! claim go for the wait, so the ASR the row waits for is admitted even
//! when it needs the chat model's room, and the next model call takes the
//! claim again through admission.
//!
//! A bound session on `gpu_world`, whose speech-to-text is the GPU row
//! `ears`: 8 + 6 GiB on a 10 GiB card, so the two never fit together, and
//! `vram.queue_timeout_seconds` 0 — an admission waits for room as long as
//! it takes. The test holds a claim of its own on `gemma` until the model
//! asked for its tool, so the ASR's admission is queued behind the chat
//! model whichever way the commit's races go; then only the loop's claim
//! could keep it waiting. With the claim held through the wait, the turn
//! hung for good, silent.

use std::time::Duration;

use serde_json::json;

use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::acquire_spec;
use lmgw_core::runtime::Class;

use super::session::session;
use super::tools::{register, stub, Calls};
use super::turns::row;
use crate::chat_voice_dictation::asr_chat_rows;
use crate::realtime_chat_thread::{eventually, of_type, say, until_type, world_on};
use crate::support::gpu_world::{Gpu, ANSWER, GIB};

#[tokio::test]
async fn the_asr_a_heard_tool_turn_waits_for_gets_the_chat_model_s_room() {
    let g = Gpu::new(10 * GIB, 6, 0).await;
    g.row(row("gemma"), 8 * GIB).await;
    g.world().thinking.insert("gemma".into());
    g.model("ears", 6 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    g.world().transcript = "Wie spät ist es?".into();
    let w = world_on(g.state.clone(), |s| {
        s.chat_voice_audio_input = "local".into();
        s.chat_stt_alias = "ears".into();
    })
    .await;
    let calls = Calls::default();
    let url = stub(calls.clone()).await;
    register(&g.state, &url).await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "stub__echo".into());

    // `gemma` up, and claimed by the test until the model has asked.
    let snap = g.state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, "gemma").unwrap();
    let pin = g
        .state
        .runtime()
        .acquire(&acquire_spec(&g.state, &snap, &rt))
        .await
        .unwrap();

    let (tid, mut ws) = session(&w).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let _ = calls.thread.set((g.state.clone(), tid));
    say(&mut ws).await;
    eventually(
        "the model to hear the turn and ask for the tool",
        || async { !g.world().streamed_bodies.is_empty() },
    )
    .await;
    // The ASR waits for room behind `gemma`; nothing ran.
    assert!(g.world().transcriptions.is_empty(), "the ASR waits");
    assert!(!g.runs().contains(&"ears".to_string()), "{:?}", g.runs());
    assert_eq!(calls.count(), 0, "not before the transcript");

    // Only the loop could keep it waiting now.
    drop(pin);
    let events = tokio::time::timeout(
        Duration::from_secs(30),
        until_type(&mut ws, "lmgw.response.timing"),
    )
    .await
    .expect("the turn ends: no circular wait on the GPU");
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status"], "completed", "{events:?}");
    assert_eq!(g.world().transcriptions, ["ears"], "the ASR got the room");
    assert_eq!(
        *calls.rows_at_call.lock().unwrap(),
        [1],
        "the tool ran once, after the user row"
    );
    // `gemma` was evicted for the ASR, and started again for the loop's
    // next model call.
    assert!(g.stops().contains(&"gemma".to_string()), "{:?}", g.stops());
    assert_eq!(
        g.runs().iter().filter(|m| *m == "gemma").count(),
        2,
        "{:?}",
        g.runs()
    );
    let rows = w.messages(tid).await;
    let shape: Vec<(&str, &str)> = rows.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
    assert_eq!(shape, [("user", "Wie spät ist es?"), ("assistant", ANSWER)]);
}
