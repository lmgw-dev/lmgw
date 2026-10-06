//! A heard tool turn whose GPU hold came on while its tools waited for the
//! user row (review V12): the loop let its claim go for the wait, and the
//! hold refuses taking it again — so the next model call goes where any
//! request goes under the hold, the row's configured fallback. A fallback
//! that hears gets the turn's audio; one that reads text only gets the
//! user row's words (review V2). Driven through the turn seam with the row
//! settled by the test, on `gpu_world`'s `gemma` and an admin thread whose
//! model asks for one tool.

use serde_json::Value;

use lmgw_core::web::spoken_turn_held_for_tests;

use super::fallbacks::{audio_parts, falls_back_to_cloud, text_only, Cloud, CLOUD_SAYS};
use super::turns::{audio, hears, row_written, thread, world};
use crate::support::gpu_world::Gpu;

/// The turn: `gemma` hears it and asks for a tool; while the tool waits for
/// the row the hold comes on; then the row is written. Its frames, and the
/// cloud fallback's chat bodies.
async fn held_mid_loop(caps: Value) -> (Gpu, Vec<(String, Value)>, Vec<Value>) {
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let cloud = Cloud::new(&g, caps, false).await;
    falls_back_to_cloud(&g, false).await;
    let tid = thread(&g, "gemma", "admin").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("the turn starts");
    let mut frames = Vec::new();
    while let Some(f) = held.frames.recv().await {
        let tool = f.0 == "tool";
        frames.push(f);
        if tool {
            break;
        }
    }
    // The tool waits for the user row, its claim let go: the hold comes on.
    falls_back_to_cloud(&g, true).await;
    let id = row_written(&g, tid, "Wie spät ist es?").await;
    held.written(id);
    frames.extend(held.rest().await);
    let bodies = cloud.bodies().await;
    (g, frames, bodies)
}

#[tokio::test]
async fn under_a_hold_switched_on_mid_loop_the_next_call_goes_to_the_fallback() {
    let (g, frames, cloud) = held_mid_loop(hears()).await;
    assert!(
        !frames.iter().any(|(e, _)| e == "error"),
        "not refused: {frames:?}"
    );
    assert!(frames
        .iter()
        .any(|(e, d)| e == "tool" && d["event"] == "result"));
    let (_, done) = frames.last().expect("frames");
    assert_eq!(done["saved"], true, "{frames:?}");
    assert_eq!(done["answered_by"], "cloud", "{done}");
    // `gemma` heard the first call; the fallback the second, as audio.
    let local = g.world().streamed_bodies.clone();
    assert_eq!(local.len(), 1, "{local:?}");
    assert_eq!(audio_parts(&local[0]), 1);
    assert_eq!(cloud.len(), 1, "{cloud:?}");
    assert_eq!(audio_parts(&cloud[0]), 1, "the fallback heard it");
    let rows = lmgw_core::store::list_chat_messages(&g.state.db, frames_thread(&g).await)
        .await
        .unwrap();
    assert_eq!(rows.last().unwrap().content, CLOUD_SAYS);
}

/// The same with a fallback that reads text only: the call goes with the
/// user row's words in place of the audio (review V2), not a refusal.
#[tokio::test]
async fn a_fallback_that_reads_text_only_gets_the_rows_words_mid_loop() {
    let (_g, frames, cloud) = held_mid_loop(text_only()).await;
    assert!(
        !frames.iter().any(|(e, _)| e == "error"),
        "not refused: {frames:?}"
    );
    assert_eq!(frames.last().unwrap().1["saved"], true, "{frames:?}");
    assert_eq!(cloud.len(), 1, "{cloud:?}");
    assert_eq!(audio_parts(&cloud[0]), 0, "{}", cloud[0]);
    let said = cloud[0].to_string();
    assert!(said.contains("Wie spät ist es?"), "the row's words: {said}");
}

/// The one thread `held_mid_loop` made.
async fn frames_thread(g: &Gpu) -> i64 {
    sqlx::query_scalar("SELECT id FROM chat_threads ORDER BY id DESC LIMIT 1")
        .fetch_one(&g.state.db)
        .await
        .unwrap()
}
