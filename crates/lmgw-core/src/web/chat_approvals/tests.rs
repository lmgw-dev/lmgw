//! A decision whose turn never runs (review finding 3): the calls are
//! closed as not run, and the feed says what happened.

use serde_json::json;
use tokio::sync::mpsc;

use super::*;
use crate::ir::{ContentPart, Message, Role, ToolResultBlock};
use crate::store::{self, ChatReply, Decider};
use crate::web::chat_attach_gate::Caps;
use crate::web::chat_turn::{TurnFrame, TurnOpts};

const GATED: &str = "mcpr_00000000000000aa";

fn call(call_id: &str, name: &str, gated: bool) -> PendingCall {
    PendingCall {
        approval_id: if gated { GATED.into() } else { String::new() },
        call_id: call_id.into(),
        name: name.into(),
        args: json!({}),
        server_label: "desktop".into(),
        needs_approval: gated,
    }
}

/// A thread whose last reply stopped on `desktop__notify` (gated) beside
/// `desktop__echo`, started by the gateway itself: the thread and the
/// reply's id.
async fn gated_thread(state: &SharedState) -> (ChatThread, i64) {
    let db = &state.db;
    let tid = store::create_chat_thread(db, "chatty", "chat")
        .await
        .unwrap();
    store::append_user_message_by(db, tid, ("go", &[], &[], None), &Decider::gateway())
        .await
        .unwrap();
    let record = vec![
        Message::text(Role::User, "go"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "desktop__echo".into(),
                    args: json!({}),
                },
                ContentPart::ToolUse {
                    id: "call_2".into(),
                    name: "desktop__notify".into(),
                    args: json!({}),
                },
            ],
        },
    ];
    let reply = ChatReply {
        ir_messages: Some(serde_json::to_string(&record).unwrap()),
        pending_approvals: Some(PendingApprovals {
            calls: vec![
                call("call_1", "desktop__echo", false),
                call("call_2", "desktop__notify", true),
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let id = store::append_chat_reply_by(db, tid, &reply, None)
        .await
        .unwrap();
    (store::get_chat_thread(db, tid).await.unwrap().unwrap(), id)
}

/// The results the reply's record ends with, as `(call id, text)`.
async fn closing(state: &SharedState, tid: i64, id: i64) -> Vec<(String, String)> {
    let row = store::get_chat_message(&state.db, tid, id)
        .await
        .unwrap()
        .unwrap();
    let msgs: Vec<Message> = serde_json::from_str(row.ir_messages.as_deref().unwrap()).unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last.role, Role::Tool, "{msgs:?}");
    last.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolResult { id, content, .. } => match content.as_slice() {
                [ToolResultBlock::Text { text }] => Some((id.clone(), text.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

async fn decided_records(state: &SharedState, tid: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_feed WHERE type = 'approval.decided' AND thread_id = ?1",
    )
    .bind(tid)
    .fetch_one(&state.db)
    .await
    .unwrap()
}

fn approve_it() -> Vec<Verdict> {
    vec![Verdict {
        approval_request_id: GATED.into(),
        approve: true,
        reason: None,
    }]
}

const VIA: Via = Via {
    proto: ClientProto::Chat,
    holds: None,
};

/// A send lands between the claim and the resume: the new message closes
/// the record (the turn might have been running), the resume is refused as
/// moved on, and the calls then read as never run — the approved one and
/// its sibling — with the feed saying so in a second `approval.decided`.
#[tokio::test]
async fn a_send_between_the_claim_and_the_resume_leaves_the_calls_not_run() {
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let (thread, reply) = gated_thread(&state).await;
    let owner = Caller::default();
    let repo = ChatRepo::Db;
    let approved = approve(&state, &owner, repo, &thread, &approve_it(), VIA)
        .await
        .map_err(|r| r.message)
        .unwrap();
    assert_eq!(decided_records(&state, thread.id).await, 1);

    // The send, before the resume starts.
    store::append_user_message_by(
        &state.db,
        thread.id,
        ("never mind", &[], &[], None),
        &Decider::gateway(),
    )
    .await
    .unwrap();
    assert_eq!(
        closing(&state, thread.id, reply).await,
        vec![
            ("call_1".to_string(), store::APPROVED_UNSAVED.to_string()),
            ("call_2".to_string(), store::APPROVED_UNSAVED.to_string()),
        ],
        "the send cannot know whether the turn ran"
    );

    let (tx, _rx) = mpsc::channel::<TurnFrame>(8);
    let opts = TurnOpts {
        resume: Some(approved.resume),
        ..TurnOpts::default()
    };
    let mode = super::super::chat_turn::TurnMode::Resume { message_id: reply };
    let refused = super::super::chat_turn::start_turn_into(
        &state,
        repo,
        &thread,
        mode,
        Caps::default(),
        tx,
        opts,
    )
    .await;
    assert_eq!(
        refused.err().map(|r| r.status()),
        Some(StatusCode::CONFLICT)
    );

    assert_eq!(
        closing(&state, thread.id, reply).await,
        vec![
            ("call_1".to_string(), store::UNRUN.to_string()),
            ("call_2".to_string(), store::UNRUN.to_string()),
        ]
    );
    let row = store::get_chat_message(&state.db, thread.id, reply)
        .await
        .unwrap()
        .unwrap();
    let d = row
        .pending_approvals
        .unwrap()
        .decision(GATED)
        .cloned()
        .unwrap();
    assert!(d.approve && d.not_run && !d.runs(), "{d:?}");
    assert_eq!(
        decided_records(&state, thread.id).await,
        2,
        "the feed records what happened"
    );
}

/// A reply that is no longer the thread's last message is refused in the
/// claim's own write: nothing is decided.
#[tokio::test]
async fn a_claim_on_a_reply_that_moved_on_decides_nothing() {
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let (thread, reply) = gated_thread(&state).await;
    // A later message the decline did not touch (a reply written by a path
    // that does not decline, as an import would).
    store::append_chat_reply_by(&state.db, thread.id, &ChatReply::default(), None)
        .await
        .unwrap();
    let refused = approve(
        &state,
        &Caller::default(),
        ChatRepo::Db,
        &thread,
        &approve_it(),
        VIA,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(refused.code, code::APPROVAL_MOVED_ON, "{}", refused.message);
    let row = store::get_chat_message(&state.db, thread.id, reply)
        .await
        .unwrap()
        .unwrap();
    assert!(row.pending_approvals.unwrap().is_open());
    assert_eq!(decided_records(&state, thread.id).await, 0);
}

/// A resume refused or stopped before its tool loop while its reply is
/// still the last message closes the calls at once: a declined one keeps
/// its words, the sibling reads as not run.
#[tokio::test]
async fn a_declined_batch_whose_turn_never_started_closes_its_sibling_as_not_run() {
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let (thread, reply) = gated_thread(&state).await;
    let decline = vec![Verdict {
        approval_request_id: GATED.into(),
        approve: false,
        reason: Some("no".into()),
    }];
    let approved = approve(
        &state,
        &Caller::default(),
        ChatRepo::Db,
        &thread,
        &decline,
        VIA,
    )
    .await
    .map_err(|r| r.message)
    .unwrap();
    unrun(&state, ChatRepo::Db, thread.id, &approved.resume).await;
    assert_eq!(
        closing(&state, thread.id, reply).await,
        vec![
            ("call_1".to_string(), store::UNRUN.to_string()),
            (
                "call_2".to_string(),
                "The user declined this tool call: no".to_string()
            ),
        ]
    );
    assert_eq!(
        decided_records(&state, thread.id).await,
        1,
        "a declined call is recorded once: nothing about it changed"
    );
}

/// An ended MCP task's row for thread `tid`, its result waiting (MCP Tasks
/// design §3.1): the row's id. Written straight to the store, so no
/// delivery was asked for.
async fn ended_task(state: &SharedState, tid: i64) -> i64 {
    use crate::store::mcp_tasks;
    let id = mcp_tasks::insert(
        &state.db,
        &mcp_tasks::NewMcpTask {
            server_id: 9,
            server_label: "desktop",
            task_id: "t1",
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
    let result = serde_json::to_string(&ToolResultBlock::one(
        "job t1 (desktop__build) completed\n42 files",
    ))
    .unwrap();
    let ended = mcp_tasks::Ended {
        status: "completed",
        status_message: None,
        result: &result,
        ended_by: None,
    };
    assert!(mcp_tasks::end(&state.db, id, &ended).await.unwrap());
    id
}

/// The roles of thread `tid`'s messages, in order.
async fn roles(state: &SharedState, tid: i64) -> Vec<String> {
    store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.role)
        .collect()
}

/// The review's M1: once the decision committed, and until the turn that
/// runs the decided calls began, the reply's record still ends in them. A
/// task result that ends in that window waits: written after the reply, it
/// would refuse the resume as moved on and leave the calls open before a
/// pair. When the turn never starts, its calls are closed as not run and
/// the result enters then.
#[tokio::test]
async fn a_task_result_waits_between_the_decision_and_its_turn() {
    use crate::web::chat_tasks::deliver;
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let (thread, reply) = gated_thread(&state).await;
    ended_task(&state, thread.id).await;
    assert_eq!(deliver::when_idle(&state, thread.id).await, 0, "undecided");
    let approved = approve(
        &state,
        &Caller::default(),
        ChatRepo::Db,
        &thread,
        &approve_it(),
        VIA,
    )
    .await
    .map_err(|r| r.message)
    .unwrap();
    let row = store::get_chat_message(&state.db, thread.id, reply)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.pending_approvals.as_ref().unwrap().is_open());
    assert!(store::record_open(row.ir_messages.as_deref()));
    assert_eq!(
        deliver::when_idle(&state, thread.id).await,
        0,
        "decided, its turn not begun: still held"
    );
    assert!(deliver::held_by(&state, thread.id).await.is_some());
    assert_eq!(roles(&state, thread.id).await, ["user", "assistant"]);

    // The turn never starts: its calls close as not run, and the result
    // enters after them.
    unrun(&state, ChatRepo::Db, thread.id, &approved.resume).await;
    assert_eq!(
        roles(&state, thread.id).await,
        ["user", "assistant", "tool"]
    );
    assert!(deliver::held_by(&state, thread.id).await.is_none());
}
