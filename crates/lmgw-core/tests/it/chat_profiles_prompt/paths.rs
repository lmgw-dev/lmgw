//! The profile on the turn paths beside a plain send (the profiles review's
//! test gaps): a continue, an Admin Chat read-aloud turn, a thread a
//! folder's rollover created, and a temporary thread each send what a
//! stored chat thread with the same profile sends (Admin Chat inside its
//! wrapper), pinned in the golden
//! `tests/fixtures/chat_system/profiles_paths.json` (`LMGW_BLESS=1`
//! rewrites it). The heard turns are `chat_voice_audio_in::profile`.

use lmgw_core::config::SelfAdmin;
use serde_json::{json, Map, Value};

use super::{assign, examples, profile, thread, LENGTH, PERSONA};
use crate::chat_golden::{last_sent, plain_thread, text_turn, voice_turn};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::realtime_chat_thread::{self as rt, settings};
use crate::support::realtime_fakes::Turn;

/// Every prompt part: persona, length rule, examples, an own voice block.
fn full() -> Value {
    json!({"name": "Full", "persona": PERSONA, "length_rule": LENGTH,
           "examples": examples(), "voice_block": "Talk in short sentences."})
}

#[tokio::test]
async fn admin_rollover_and_temporary_threads_take_the_profile() {
    let w = rt::world(|_| {}).await;
    let mut cases = Map::new();
    let p = profile(&w, full()).await;
    let stored = thread(&w, None, Some(p)).await;
    let text = text_turn(&w, stored, false).await;
    let voice = voice_turn(&w, stored).await;
    cases.insert("text_stored".into(), text.clone());
    cases.insert("voice_stored".into(), voice.clone());

    // A rollover's thread: the folder's profile, the default prompt.
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({"profile_id": p})).await;
    let (current, _, _) = current_ok(&w, &owner, folder, false).await;
    let got = text_turn(&w, current, false).await;
    assert_eq!(
        got, text,
        "a rollover's thread sends what a stored one does"
    );
    cases.insert("text_rollover".into(), got);
    let got = voice_turn(&w, current).await;
    assert_eq!(got, voice);
    cases.insert("voice_rollover".into(), got);

    // A temporary thread, its profile held in memory.
    let r = w
        .post(
            "/chat/api/threads",
            json!({"model_alias": "chatty", "temporary": true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let temp = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    assert!(temp < 0);
    w.set(temp, json!({"profile_id": p})).await;
    let got = text_turn(&w, temp, false).await;
    assert_eq!(got, text, "a temporary thread sends what a stored one does");
    cases.insert("text_temporary".into(), got);

    // Admin Chat has no voice mode (a bound session is refused: its spoken
    // turn would skip reading a reply before it is sent; its heard turn is
    // `chat_voice_audio_in::profile`). Read aloud with a reply language:
    // the wrapper around the profile's speak message.
    settings(&w.state, |s| s.self_admin = SelfAdmin::ReadOnly).await;
    let admin = plain_thread(&w, Some("admin"), Some("Du bist ein Assistent.")).await;
    assign(&w, admin, Some(p)).await;
    w.set(admin, json!({"voice": {"language": "de"}})).await;
    w.set(stored, json!({"voice": {"language": "de"}})).await;
    let inner = text_turn(&w, stored, true).await;
    let got = text_turn(&w, admin, true).await;
    let (system, inner) = (
        got["system"].as_str().unwrap(),
        inner["system"].as_str().unwrap(),
    );
    assert!(
        system.contains(inner) && system.len() > inner.len(),
        "the admin wrapper around the speak turn's message:\n{system}"
    );
    cases.insert("admin_speak_de".into(), got);

    crate::chat_golden::golden_system("profiles_paths", &cases);
}

/// A continue (llama-server only) sends the profile's text-turn message.
#[tokio::test]
async fn a_continue_takes_the_profile() {
    let w = rt::world(|_| {}).await;
    sqlx::query(
        "UPDATE upstreams SET protocol = 'llama_cpp', kind = 'llama_server' WHERE name = 'fake'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let p = profile(&w, full()).await;
    let tid = thread(&w, None, Some(p)).await;
    let text = text_turn(&w, tid, false).await;

    w.chat.push(Turn::text(&[" and more."]));
    let r = w
        .post(&format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("event: done"), "{body}");
    let got = last_sent(&w);
    assert_eq!(got["system"], text["system"], "{got}");
    let mut cases = Map::new();
    cases.insert("continue".into(), got);
    crate::chat_golden::golden_system("profiles_continue", &cases);
}
