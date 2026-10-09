//! A turn that took its thread and was refused before its worker (the
//! review's L1), and `answer`'s start (L3): neither strands a waiting
//! result, and an answer never cancels the turn that runs nor runs on a
//! history whose results were answered meanwhile. A send's message (moment
//! 3): what waited enters before it, and one that ends after it waits
//! until the hold the message's turn takes over is let go.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::ir::ToolResultBlock;
use crate::state::{AppState, SharedState};
use crate::store::{self, mcp_tasks, ChatThread, Decider};
use crate::web::chat_attach_gate::Caps;
use crate::web::chat_repo::ChatRepo;
use crate::web::chat_turn::{start_turn_into, TurnFrame, TurnMode, TurnOpts};

/// A thread holding `build it` and the plain reply `Started.`: the thread
/// and the reply's id.
async fn answered_thread(state: &SharedState) -> (ChatThread, i64) {
    let db = &state.db;
    let tid = store::create_chat_thread(db, "m", "chat").await.unwrap();
    store::append_user_message_by(db, tid, ("build it", &[], &[], None), &Decider::gateway())
        .await
        .unwrap();
    let reply = store::ChatReply {
        content: "Started.".into(),
        ..Default::default()
    };
    let id = store::append_chat_reply_by(db, tid, &reply, None)
        .await
        .unwrap();
    (store::get_chat_thread(db, tid).await.unwrap().unwrap(), id)
}

/// An ended task's row for thread `tid`, written straight to the store, so
/// no delivery was asked for.
async fn ended_task(state: &AppState, tid: i64) {
    ended_task_as(state, tid, "t1").await;
}

/// [`ended_task`] with the server's task id `task_id`.
async fn ended_task_as(state: &AppState, tid: i64, task_id: &str) {
    let id = mcp_tasks::insert(
        &state.db,
        &mcp_tasks::NewMcpTask {
            server_id: 9,
            server_label: "desktop",
            task_id,
            thread_id: tid,
            tool: "desktop__build",
            call_id: "call_0",
            started_by: None,
            status: "working",
            status_message: None,
            poll_interval_ms: None,
            ttl_ms: None,
        },
    )
    .await
    .unwrap();
    let result = serde_json::to_string(&ToolResultBlock::one(format!(
        "job {task_id} (desktop__build) completed\n42 files"
    )))
    .unwrap();
    let ended = mcp_tasks::Ended {
        status: "completed",
        status_message: None,
        result: &result,
        ended_by: None,
    };
    assert!(mcp_tasks::end(&state.db, id, &ended).await.unwrap());
}

async fn roles(state: &AppState, tid: i64) -> Vec<String> {
    store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.role)
        .collect()
}

/// Start a turn of `thread` in `mode` with nothing else asked: the refusal,
/// as its status and `code`, when it was refused.
async fn start(state: &SharedState, thread: &ChatThread, mode: TurnMode) -> Option<(u16, String)> {
    let (tx, _rx) = mpsc::channel::<TurnFrame>(8);
    let refused = start_turn_into(
        state,
        ChatRepo::Db,
        thread,
        mode,
        Caps::default(),
        tx,
        TurnOpts::default(),
    )
    .await
    .err()?;
    let status = refused.status().as_u16();
    let body = axum::body::to_bytes(refused.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    Some((status, v["code"].as_str().unwrap_or_default().to_string()))
}

/// A turn refused after it took the thread — here a resume of a reply
/// that stopped on no calls, which passes the check before the ticket and
/// fails the one after it — lets in what waited when it lets the thread
/// go. Nothing else would: no worker ran to deliver at its end.
#[tokio::test]
async fn a_turn_refused_after_its_ticket_delivers_what_waits() {
    let state = AppState::init_for_tests().await.unwrap();
    let (thread, reply) = answered_thread(&state).await;
    ended_task(&state, thread.id).await;
    let refused = start(&state, &thread, TurnMode::Resume { message_id: reply }).await;
    assert_eq!(refused.map(|r| r.0), Some(409));
    let delivered = tokio::time::timeout(Duration::from_secs(10), async {
        while roles(&state, thread.id).await.len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(delivered.is_ok(), "the result entered");
    assert_eq!(
        roles(&state, thread.id).await,
        ["user", "assistant", "tool"]
    );
}

/// `answer`'s start (the review's L3): on a thread whose turn runs it is
/// refused `turn_running` and that turn goes on; on a history no result
/// waits in any more — a turn answered it after the route's check — it is
/// refused `nothing_to_answer` under its ticket, and no turn is left
/// running.
#[tokio::test]
async fn an_answer_never_cancels_a_turn_nor_runs_on_an_answered_history() {
    let state = AppState::init_for_tests().await.unwrap();
    let (thread, _) = answered_thread(&state).await;
    let running = state.chat_live.begin(thread.id).await;
    assert_eq!(
        start(&state, &thread, TurnMode::Answer).await,
        Some((409, "turn_running".to_string()))
    );
    assert!(!running.is_superseded(), "the running turn goes on");
    assert!(
        running.save_lock().await.is_some(),
        "its history did not move"
    );
    drop(running);

    assert_eq!(
        start(&state, &thread, TurnMode::Answer).await,
        Some((409, "nothing_to_answer".to_string()))
    );
    assert!(!state.chat_live.running(thread.id));
    assert_eq!(roles(&state, thread.id).await, ["user", "assistant"]);
}

/// Wait until thread `tid` holds `n` messages.
async fn until_messages(state: &AppState, tid: i64, n: usize) {
    let reached = tokio::time::timeout(Duration::from_secs(10), async {
        while roles(state, tid).await.len() < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "{n} messages: {:?}",
        roles(state, tid).await
    );
}

/// A send's message (moment 3): the result that waited when it came is
/// written before it, in its transaction, so the model answers the message
/// with the result as context. From before the write until the hold is let
/// go (the turn has begun, or was refused) the thread counts as running: a
/// result that ends after the message waits — no idle delivery, no turn
/// that starts only on an idle thread — and enters once the hold drops with
/// no turn live.
#[tokio::test]
async fn a_send_writes_what_waits_before_its_message_and_holds_off_what_ends_after() {
    let state = AppState::init_for_tests().await.unwrap();
    let (thread, _) = answered_thread(&state).await;
    ended_task(&state, thread.id).await;
    let hold = super::sent(&state, thread.id, true).expect("a stored thread's hold");
    assert!(state.chat_live.running(thread.id));
    let caller = crate::web::chat_caller::Caller::default();
    let sent = ChatRepo::Db
        .append_user_message(&state, thread.id, "and?", &[], &[], None, &caller)
        .await
        .unwrap();
    assert!(matches!(sent, store::SendMessageOutcome::Sent(_)));
    let rows = store::list_chat_messages(&state.db, thread.id)
        .await
        .unwrap();
    let shape: Vec<(&str, &str)> = rows
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(
        shape[..2],
        [("user", "build it"), ("assistant", "Started.")]
    );
    assert_eq!(rows[2].role, "tool");
    assert!(rows[2].content.contains("42 files"), "{:?}", rows[2]);
    assert_eq!(shape[3], ("user", "and?"));
    assert!(mcp_tasks::waiting(&state.db, thread.id)
        .await
        .unwrap()
        .is_empty());

    // A result that ends after the message waits while the hold is held.
    ended_task_as(&state, thread.id, "t2").await;
    assert_eq!(super::when_idle(&state, thread.id).await, 0);
    assert!(
        state
            .chat_live
            .begin_idle_as(thread.id, None, 0)
            .await
            .is_none(),
        "no idle-only turn starts between a message and its turn"
    );
    assert_eq!(roles(&state, thread.id).await.len(), 4);
    // Let go with no turn live (the turn was refused before it began): it
    // enters as on an idle thread.
    drop(hold);
    assert!(!state.chat_live.running(thread.id));
    until_messages(&state, thread.id, 5).await;
    assert_eq!(
        roles(&state, thread.id).await,
        ["user", "assistant", "tool", "user", "tool"]
    );
}

/// A send's message rolled back (an attachment no longer a draft) lets no
/// result in: what waited still waits, and enters once the hold drops.
#[tokio::test]
async fn a_send_that_rolls_back_leaves_what_waits_waiting() {
    let state = AppState::init_for_tests().await.unwrap();
    let (thread, _) = answered_thread(&state).await;
    ended_task(&state, thread.id).await;
    let hold = super::sent(&state, thread.id, true).unwrap();
    let caller = crate::web::chat_caller::Caller::default();
    let sent = ChatRepo::Db
        .append_user_message(&state, thread.id, "and?", &[4242], &[], None, &caller)
        .await
        .unwrap();
    assert!(matches!(
        sent,
        store::SendMessageOutcome::AttachmentNotDraft
    ));
    assert_eq!(roles(&state, thread.id).await, ["user", "assistant"]);
    assert_eq!(
        mcp_tasks::waiting(&state.db, thread.id)
            .await
            .unwrap()
            .len(),
        1
    );
    drop(hold);
    until_messages(&state, thread.id, 3).await;
    assert_eq!(
        roles(&state, thread.id).await,
        ["user", "assistant", "tool"]
    );
}

/// A turn that answers a send's message begins with its hold: its start
/// delivers nothing — what ends after the message waits for its end — and
/// the turn's end lets it in.
#[tokio::test]
async fn a_send_s_turn_start_delivers_nothing() {
    let state = AppState::init_for_tests().await.unwrap();
    let (thread, _) = answered_thread(&state).await;
    let hold = super::sent(&state, thread.id, true).unwrap();
    let caller = crate::web::chat_caller::Caller::default();
    let store::SendMessageOutcome::Sent(mid) = ChatRepo::Db
        .append_user_message(&state, thread.id, "and?", &[], &[], None, &caller)
        .await
        .unwrap()
    else {
        panic!("not sent");
    };
    ended_task(&state, thread.id).await;
    let (tx, _rx) = mpsc::channel::<TurnFrame>(8);
    let opts = TurnOpts {
        sent: Some(hold),
        ..TurnOpts::default()
    };
    let started = start_turn_into(
        &state,
        ChatRepo::Db,
        &thread,
        TurnMode::Fresh {
            user_message_id: Some(mid),
        },
        Caps::default(),
        tx,
        opts,
    )
    .await;
    assert!(started.is_ok());
    // The thread's lock, before the worker (spawned, not yet run on this
    // test's one thread) can end and deliver: whatever entered, its start
    // let in.
    let held = state.chat_live.hold(thread.id).await;
    assert_eq!(
        mcp_tasks::waiting(&state.db, thread.id)
            .await
            .unwrap()
            .len(),
        1,
        "the start let nothing in"
    );
    assert_eq!(
        roles(&state, thread.id).await,
        ["user", "assistant", "user"]
    );
    drop(held);
    until_messages(&state, thread.id, 4).await;
    let rows = store::list_chat_messages(&state.db, thread.id)
        .await
        .unwrap();
    assert_eq!(rows.last().unwrap().role, "tool");
}
