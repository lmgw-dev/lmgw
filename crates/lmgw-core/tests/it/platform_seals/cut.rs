//! A reply's text cut short keeps its seals where they can stand (WP8
//! verify V-F2): a continue trims the reply's trailing whitespace, and an
//! item answered there stands at the end of what is kept, ahead of the
//! continuation rather than inside it; a spoken reply cut to what was heard
//! loses the seals answered past the cut, with the unheard rest.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{Seal, SealScope};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, ReplySeal};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_actions::{gateway, mount_openai_reply, post};

fn seal(id: &str) -> Seal {
    Seal::openai(&SealScope::new(1, "cfg"), id, "enc")
}

async fn set_seals(state: &SharedState, id: i64, seals: &[ReplySeal]) {
    sqlx::query("UPDATE chat_messages SET reasoning_seals = ?1 WHERE id = ?2")
        .bind(serde_json::to_string(seals).unwrap())
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
}

async fn last_message(state: &SharedState, tid: i64) -> store::ChatMessageRow {
    store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap()
}

#[tokio::test]
async fn a_continued_reply_s_trailing_seal_stays_ahead_of_the_continuation() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "The answer.", 12, 5).await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    store::append_chat_message(&state.db, tid, "user", "check it", "", None, None, None)
        .await
        .unwrap();
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "Checking.  \n",
        "Hm.",
        Some(8),
        Some(3),
        None,
    )
    .await
    .unwrap();
    // The second item was answered after the whitespace the continue trims.
    set_seals(
        &state,
        rid,
        &[
            ReplySeal::new(0, seal("rs_1")),
            ReplySeal::new(12, seal("rs_2")),
        ],
    )
    .await;
    let r = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({})).await;
    assert_eq!(r.status(), 200);
    r.text().await.unwrap();
    let reply = last_message(&state, tid).await;
    assert_eq!(reply.content, "Checking.The answer.");
    assert_eq!(
        reply.reasoning_seals,
        [
            ReplySeal::new(0, seal("rs_1")),
            ReplySeal::new(9, seal("rs_2")),
        ],
        "at the end of the kept text, not inside the continuation"
    );
}

#[tokio::test]
async fn a_spoken_reply_cut_to_what_was_heard_keeps_only_the_heard_seals() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "It is late. Very late.",
        "Hm.",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let seals = [
        ReplySeal::new(0, seal("rs_1")),
        ReplySeal::new(11, seal("rs_2")),
        ReplySeal::new(12, seal("rs_3")),
    ];
    set_seals(&state, rid, &seals).await;
    let voice = store::MessageVoice::default();
    assert!(
        store::cut_chat_reply(&state.db, tid, rid, "It is late.", &voice, None)
            .await
            .unwrap()
    );
    let reply = last_message(&state, tid).await;
    assert_eq!(reply.content, "It is late.");
    assert_eq!(reply.reasoning, "Hm.", "the trace stays");
    assert_eq!(reply.reasoning_seals, seals[..2]);
}
