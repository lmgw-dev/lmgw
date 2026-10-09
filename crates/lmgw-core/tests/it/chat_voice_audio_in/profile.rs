//! A heard turn and the thread's personality profile (the profiles
//! review's test gap): the turn the model hears — through the turn seam and
//! through the bound responder, both with text output, on a chat thread and
//! on Admin Chat — sends the profile's static part (persona, length rule,
//! examples; no voice block, as a text-output turn has none) and its
//! reasoning, pinned in the golden `tests/fixtures/chat_system/profiles_heard.json` (`LMGW_BLESS=1`
//! rewrites it).

use lmgw_core::config::chat_profile::{create_kind, ProfileCreate};
use lmgw_core::realtime::heard_response_for_tests;
use lmgw_core::web::spoken_turn_for_tests;
use serde_json::{json, Map, Value};

use super::turns::{audio, row_written, thread, world};
use crate::chat_golden::sent;
use crate::support::gpu_world::Gpu;

async fn profile(g: &Gpu) -> i64 {
    let c: ProfileCreate = serde_json::from_value(json!({
        "name": "Heard", "persona": "You are {{model}}, a calm voice.",
        "length_rule": "Answer in one sentence.",
        "examples": [{"user": "Is it raining?", "reply": "I can't see outside."}],
        "voice_block": "Talk in short sentences.", "reasoning": "on",
    }))
    .unwrap();
    let id = lmgw_core::store::chat_profiles::create_chat_profile(
        &g.state.db,
        &create_kind(&c).unwrap(),
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    g.state.reload_snapshot().await.unwrap();
    id
}

async fn assign(g: &Gpu, tid: i64, profile: i64) {
    sqlx::query("UPDATE chat_threads SET profile_id = ?1 WHERE id = ?2")
        .bind(profile)
        .bind(tid)
        .execute(&g.state.db)
        .await
        .unwrap();
}

fn last_streamed(g: &Gpu) -> Value {
    let bodies = g.world().streamed_bodies.clone();
    sent(bodies.last().expect("a request reached the model"))
}

#[tokio::test]
async fn a_heard_turn_takes_the_profile_on_both_paths_and_in_admin_chat() {
    let g = world().await;
    let p = profile(&g).await;
    let mut cases = Map::new();

    let chat = thread(&g, "gemma", "chat").await;
    assign(&g, chat, p).await;
    spoken_turn_for_tests(&g.state, chat, vec![audio()]).await;
    let seam = last_streamed(&g);
    let system = seam["system"].as_str().unwrap();
    assert!(
        system.starts_with("You are gemma, a calm voice.\n\nAnswer in one sentence.")
            && system.contains("You: I can't see outside."),
        "{system}"
    );
    cases.insert("heard_seam".into(), seam.clone());

    let id = row_written(&g, chat, "Und morgen?").await;
    let out = heard_response_for_tests(&g.state, chat, vec![audio()], id).await;
    assert!(out.result.is_ok(), "{out:?}");
    let bound = last_streamed(&g);
    assert_eq!(bound, seam, "the bound responder sends what the seam does");
    cases.insert("heard_bound".into(), bound);

    let admin = thread(&g, "gemma", "admin").await;
    assign(&g, admin, p).await;
    spoken_turn_for_tests(&g.state, admin, vec![audio()]).await;
    let got = last_streamed(&g);
    let wrapped = got["system"].as_str().unwrap();
    assert!(
        wrapped.contains(system) && wrapped.len() > system.len(),
        "the admin wrapper around the heard turn's message:\n{wrapped}"
    );
    cases.insert("heard_admin".into(), got);

    crate::chat_golden::golden_system("profiles_heard", &cases);
}
