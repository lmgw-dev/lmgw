//! A late result in a Chat thread (MCP Tasks design §3.1–§3.3, §3.5), over
//! the router with the tool hosted by the fake task device and the model
//! scripted by the chat fake:
//!
//! - the turn's tool result is `started, job …`, its frame names the task,
//!   and the device's call says where the result goes;
//! - delivery only while no turn runs: a task that ends during its own
//!   turn, or during a later one, enters after that turn's reply; one that
//!   ends with the thread idle enters at once;
//! - the pair renders where the result is stored (chronological order,
//!   design T11): a send after the result follows the pair, joined to the
//!   reply before it, and a later send replays the same bytes; `answer`
//!   joins the call to the reply before it and refuses `409
//!   nothing_to_answer`; edit and regenerate keep the result, and the reply
//!   after it answers again; every request passes the wire check;
//! - an approved `required` call carries `lmgw/approval` and `lmgw/task`;
//!   a result that ends between a decision's commit and the start of the
//!   turn that runs the decided calls waits for that turn; a result a
//!   reply's waiting call holds off enters before the message of a send
//!   that declines the call, and the model answers the message;
//! - `answer` while a turn runs is `409 turn_running`;
//! - a cleared hosting grant ends the task abandoned, into the thread;
//! - [`goldens`]: the request an OpenAI-shaped, an Anthropic-shaped and a
//!   llama-server upstream receive.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{next, row_in, task_world, Script};
use crate::chat_approvals::{approval_frames, decide, done, send};
use crate::common::patience;
use crate::device_chat::{chat_thread, op, post, sse};
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::{Step, Turn};
use crate::support::realtime_mcp::calls;

mod goldens;

/// A thread of the owner's with the device's label attached (`extra`, the
/// label entry's other fields): its id.
pub(crate) async fn tool_thread(w: &World, extra: Value) -> i64 {
    let owner = w.gw.client();
    let tid = chat_thread(w, &owner, "chatty").await;
    let mut label = json!({"server_label": "desktop"});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        label[k] = v;
    }
    let (s, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [label] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    tid
}

/// The next model turn calls `desktop__build`.
pub(crate) fn build_call(w: &World) {
    w.chat.push(calls(
        &[(0, "call_1", "desktop__build", "{}")],
        "tool_calls",
    ));
}

/// A turn that says `text`, then waits for `gate` before it ends.
fn held_text(text: &'static str, gate: &Arc<Notify>) -> Turn {
    Turn::Stream(vec![
        Step::Text(text),
        Step::Wait(gate.clone()),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ])
}

/// The thread's messages, as `GET /chat/api/threads/{id}` reads them.
pub(crate) async fn messages(w: &World, tid: i64) -> Vec<Value> {
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    v["messages"].as_array().cloned().unwrap_or_default()
}

/// Wait until the thread holds `n` result rows: its messages.
pub(crate) async fn until_results(w: &World, tid: i64, n: usize) -> Vec<Value> {
    patience::until_async(&format!("{n} result rows in the thread"), || async {
        messages(w, tid)
            .await
            .iter()
            .filter(|m| m["role"] == "tool")
            .count()
            == n
    })
    .await;
    messages(w, tid).await
}

/// The roles of `msgs`, in order.
fn roles(msgs: &[Value]) -> Vec<&str> {
    msgs.iter().map(|m| m["role"].as_str().unwrap()).collect()
}

/// The `tool {event: result}` frames of a turn.
fn results(frames: &[(String, Value)]) -> Vec<Value> {
    frames
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == "result")
        .map(|(_, d)| d.clone())
        .collect()
}

/// The messages of the chat fake's request `n`.
pub(crate) fn sent(w: &World, n: usize) -> Vec<Value> {
    w.chat.seen.chat(n)["messages"].as_array().cloned().unwrap()
}

/// What the OpenAI wire, and the chat templates that follow it, need of
/// `msgs` (an OpenAI-shaped request's messages): every tool message right
/// after the assistant message whose `tool_calls` name its id (or a sibling
/// result of it), no two assistant messages in a row. A user message after
/// a tool result is valid (design T11, the owner's decision of
/// 2026-10-09).
pub(crate) fn assert_strict(msgs: &[Value]) {
    let mut open: Vec<String> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| msgs[p]["role"].as_str().unwrap());
        match m["role"].as_str().unwrap() {
            "assistant" => {
                assert_ne!(prev, Some("assistant"), "two assistants: {msgs:#?}");
                open = m["tool_calls"]
                    .as_array()
                    .map(|c| {
                        c.iter()
                            .map(|c| c["id"].as_str().unwrap().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                continue;
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap();
                assert!(
                    open.iter().any(|o| o == id),
                    "{id} does not follow its call: {msgs:#?}"
                );
                continue;
            }
            _ => {}
        }
        open.clear();
    }
}

/// The turn answers `started, job t1` at once, its result frame names the
/// task, and the device's call says the result lands in the thread; when
/// the job ends with the thread idle, the result enters at once as a row of
/// role `tool` with the task's facts, and the thread lists no task.
#[tokio::test]
async fn the_turn_answers_started_and_the_result_enters_the_idle_thread() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started", " it."]));
    let frames = send(&w, &w.gw.client(), tid, "build it").await;
    let result = &results(&frames)[0];
    assert_eq!(result["output"], "started, job t1", "{result}");
    let task = &result["task"];
    assert_eq!(task["task_id"], "t1", "{result}");
    assert_eq!(task["server_label"], "desktop");
    let id = task["id"].as_i64().unwrap();
    assert_eq!(done(&frames)["saved"], true);
    let call = next("the device's call", &mut dev.seen.calls).await;
    assert_eq!(
        call["params"]["_meta"]["lmgw/task"],
        json!({"delivery": "thread", "thread_id": tid}),
        "{call}"
    );
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert_eq!(v["tasks"][0]["id"], id, "{v}");
    assert_eq!(v["tasks"][0]["status"], "working");
    assert_eq!(v["tasks"][0]["by"], "the dashboard");

    dev.complete("t1", "42 files", true);
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(roles(&msgs), ["user", "assistant", "tool"]);
    let r = &msgs[2];
    assert_eq!(
        r["content"], "job t1 (desktop__build) completed\n42 files",
        "{r}"
    );
    assert_eq!(
        r["task"],
        json!({"task_id": "t1", "server_label": "desktop", "tool": "desktop__build",
               "status": "completed", "ended_by": null})
    );
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert_eq!(v["tasks"], json!([]), "delivered: no task left");
    assert!(super::rows(&w).await.is_empty(), "the task row went");
}

/// A job that ends while its own turn still streams waits for the turn's
/// reply, and enters after it, never in between.
#[tokio::test]
async fn a_task_that_ends_during_its_turn_enters_after_the_reply() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    let gate = Arc::new(Notify::new());
    w.chat.push(held_text("Started.", &gate));
    let owner = w.gw.client();
    let sending = send(&w, &owner, tid, "build it");
    let meanwhile = async {
        next("the device's call", &mut dev.seen.calls).await;
        dev.complete("t1", "done early", true);
        let row = row_in(&w, "ended").await;
        assert_eq!(row.status, "completed");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            row_in(&w, "ended").await.id,
            row.id,
            "still waiting while the turn runs"
        );
        assert!(messages(&w, tid).await.iter().all(|m| m["role"] != "tool"));
        let v = w.get(&format!("/chat/api/threads/{tid}")).await;
        assert_eq!(
            v["tasks"][0]["waiting_for"], "the turn of the thread that is running",
            "{v}"
        );
        gate.notify_one();
    };
    let (frames, ()) = tokio::join!(sending, meanwhile);
    let reply = done(&frames)["message_id"].as_i64().unwrap();
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(roles(&msgs), ["user", "assistant", "tool"]);
    assert_eq!(msgs[1]["id"], reply);
    assert!(msgs[2]["id"].as_i64().unwrap() > reply);
}

/// A job that ends during a later turn waits for that turn too, and its
/// pair is answered by the turn after it.
#[tokio::test]
async fn a_task_that_ends_during_a_later_turn_waits_for_it() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    let owner = w.gw.client();
    send(&w, &owner, tid, "build it").await;
    next("the device's call", &mut dev.seen.calls).await;

    let gate = Arc::new(Notify::new());
    w.chat.push(held_text("Chatting.", &gate));
    let sending = send(&w, &owner, tid, "meanwhile?");
    let meanwhile = async {
        patience::until("the second turn streams", || w.chat.seen.chat_count() == 3).await;
        dev.complete("t1", "late", true);
        row_in(&w, "ended").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(messages(&w, tid).await.iter().all(|m| m["role"] != "tool"));
        gate.notify_one();
    };
    tokio::join!(sending, meanwhile);
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(
        roles(&msgs),
        ["user", "assistant", "user", "assistant", "tool"]
    );
}

/// A send after the result entered follows the pair, whose call joins the
/// reply that started the job (chronological order, design T11): the
/// request ends with the user's message, which the model answers. Every
/// later request replays those bytes.
#[tokio::test]
async fn a_send_follows_the_pair_and_replays_the_same_bytes() {
    let (w, _d, dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    let owner = w.gw.client();
    send(&w, &owner, tid, "build it").await;
    dev.complete("t1", "42 files", true);
    until_results(&w, tid, 1).await;

    w.chat.push(Turn::text(&["It built 42 files."]));
    let n = w.chat.seen.chat_count();
    send(&w, &owner, tid, "and?").await;
    let first = sent(&w, n);
    assert_strict(&first);
    let tail: Vec<&str> = roles(&first).into_iter().rev().take(3).collect();
    assert_eq!(tail, ["user", "tool", "assistant"], "{first:#?}");
    assert_eq!(first.last().unwrap()["content"], "and?");
    let call = &first[first.len() - 3];
    assert_eq!(call["content"], "Started.", "joined to the reply: {call}");
    let fid = call["tool_calls"][0]["id"].as_str().unwrap();
    assert!(fid.starts_with("lmgw_task_"), "{call}");
    assert_eq!(
        call["tool_calls"][0]["function"]["name"],
        "lmgw__job_result"
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            call["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        json!({"job": "t1", "tool": "desktop__build"})
    );
    let result = &first[first.len() - 2];
    assert_eq!(result["tool_call_id"], fid);
    assert_eq!(
        result["content"],
        "job t1 (desktop__build) completed\n42 files"
    );

    w.chat.push(Turn::text(&["You are welcome."]));
    send(&w, &owner, tid, "thanks").await;
    let second = sent(&w, n + 1);
    assert_strict(&second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second[..first.len()]).unwrap(),
        "the replay changed what the turn before it sent"
    );
}

/// `answer` runs a continuation: the call joins the reply that started the
/// job, the model answers, and the reply after the result is stored. With
/// nothing to answer — before a result, and once it was answered — it is
/// `409 nothing_to_answer`.
#[tokio::test]
async fn answer_joins_the_call_to_the_reply_and_refuses_with_nothing_to_answer() {
    let (w, _d, dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    let owner = w.gw.client();
    let path = format!("/chat/api/threads/{tid}/answer");
    let (s, v) = post(&w, &owner, &path, json!({})).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("nothing_to_answer")));

    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    send(&w, &owner, tid, "build it").await;
    let (s, _) = post(&w, &owner, &path, json!({})).await;
    assert_eq!(s, 409, "a job still running is nothing to answer");
    dev.complete("t1", "42 files", true);
    until_results(&w, tid, 1).await;

    w.chat.push(Turn::text(&["It built 42 files."]));
    let n = w.chat.seen.chat_count();
    // An empty body, as a client without `speak` sends it.
    let resp = owner.post(format!("{}{path}", w.gw)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let frames = crate::device_chat::frames(&resp.text().await.unwrap());
    let end = done(&frames);
    assert_eq!(end["saved"], true, "{end}");
    assert!(
        frames
            .iter()
            .all(|(e, d)| e != "turn" || d.get("user_message_id").is_none()),
        "no user message: {frames:?}"
    );
    let req = sent(&w, n);
    assert_strict(&req);
    let joined = &req[req.len() - 2];
    assert_eq!(joined["role"], "assistant");
    assert_eq!(joined["content"], "Started.", "{joined}");
    assert_eq!(
        joined["tool_calls"][0]["function"]["name"],
        "lmgw__job_result"
    );
    assert_eq!(req.last().unwrap()["role"], "tool");
    let msgs = messages(&w, tid).await;
    assert_eq!(roles(&msgs), ["user", "assistant", "tool", "assistant"]);
    assert_eq!(msgs[3]["content"], "It built 42 files.");

    let (s, v) = post(&w, &owner, &path, json!({})).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("nothing_to_answer")));

    // A later send replays the continuation's request as it went out.
    w.chat.push(Turn::text(&["Sure."]));
    send(&w, &owner, tid, "again?").await;
    let later = sent(&w, n + 1);
    assert_strict(&later);
    assert_eq!(
        serde_json::to_string(&req).unwrap(),
        serde_json::to_string(&later[..req.len()]).unwrap()
    );
}

/// Edit and regenerate cut replies, never results (T13): a regenerated
/// continuation answers the result again, and an edited first message is
/// followed by the result, still answered.
#[tokio::test]
async fn edit_and_regenerate_keep_the_result_and_answer_it_again() {
    let (w, _d, dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    let owner = w.gw.client();
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    send(&w, &owner, tid, "build it").await;
    dev.complete("t1", "42 files", true);
    until_results(&w, tid, 1).await;
    w.chat.push(Turn::text(&["Done: 42."]));
    let (s, _) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/answer"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200);
    let msgs = messages(&w, tid).await;
    let answer = msgs[3]["id"].as_i64().unwrap();

    // Regenerate the continuation: the result stays and is answered again.
    w.chat.push(Turn::text(&["Again: 42."]));
    let n = w.chat.seen.chat_count();
    let (s, frames) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/messages/{answer}/regenerate"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let req = sent(&w, n);
    assert_strict(&req);
    assert_eq!(req.last().unwrap()["role"], "tool");
    let msgs = messages(&w, tid).await;
    assert_eq!(roles(&msgs), ["user", "assistant", "tool", "assistant"]);
    assert_eq!(msgs[3]["content"], "Again: 42.");

    // Edit the first message: the replies go, the result stays after it.
    let first = msgs[0]["id"].as_i64().unwrap();
    w.chat.push(Turn::text(&["Fresh answer."]));
    let n = w.chat.seen.chat_count();
    let (s, frames) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/messages/{first}/edit"),
        json!({"content": "build it twice"}),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let req = sent(&w, n);
    assert_strict(&req);
    let tail: Vec<&str> = roles(&req).into_iter().rev().take(3).collect();
    assert_eq!(tail, ["tool", "assistant", "user"], "{req:#?}");
    let msgs = messages(&w, tid).await;
    assert_eq!(roles(&msgs), ["user", "tool", "assistant"]);
}

/// An approved `required` call becomes a task (client-apps M3 with MCP
/// Tasks): the device's call carries both `lmgw/approval` and
/// `lmgw/task`.
#[tokio::test]
async fn an_approved_required_call_carries_approval_and_task() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({"require_approval": "always"})).await;
    let owner = w.gw.client();
    build_call(&w);
    let frames = send(&w, &owner, tid, "build it").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["Started."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let call = next("the approved call", &mut dev.seen.calls).await;
    let meta = &call["params"]["_meta"];
    assert_eq!(
        meta["lmgw/approval"],
        json!({"decision": "approved", "by": {"kind": "owner", "name": "dashboard"}}),
        "{call}"
    );
    assert_eq!(
        meta["lmgw/task"],
        json!({"delivery": "thread", "thread_id": tid})
    );
    assert!(call["params"].get("task").is_some(), "{call}");
    assert_eq!(results(&frames)[0]["task"]["task_id"], "t1");
}

/// A cleared hosting grant removes the device's row, and its job ends
/// abandoned in that write; the result enters the idle thread saying why.
#[tokio::test]
async fn a_cleared_grant_ends_the_job_abandoned_into_the_thread() {
    let (w, d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    send(&w, &w.gw.client(), tid, "build it").await;
    next("the device's call", &mut dev.seen.calls).await;
    let (s, r) = op(&w, "key_set", json!({"id": d.id, "hosts_label": ""})).await;
    assert_eq!(s, 200, "{r}");
    let msgs = until_results(&w, tid, 1).await;
    let r = &msgs[2];
    assert_eq!(r["task"]["status"], "abandoned", "{r}");
    assert_eq!(
        r["content"],
        "job t1 (desktop__build) abandoned\nserver 'device:desktop' was removed (its device's \
         hosting grant was cleared): the job was abandoned; it may or may not have finished"
    );
}

/// The review's M1: a job that ends after a decision committed and before
/// the resumed turn took the thread waits for that turn, whose reply it
/// then follows; the decision is never refused as moved on. The thread's
/// lock is held while the job ends, so its delivery waits for the lock
/// ahead of the resumed turn's start, and is let go once the decision is
/// written.
#[tokio::test]
async fn a_result_that_ends_between_a_decision_and_its_turn_waits_for_the_turn() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({"require_approval": "always"})).await;
    let owner = w.gw.client();
    let approve = |id: Value| json!([{"approval_request_id": id, "approve": true}]);
    build_call(&w);
    let frames = send(&w, &owner, tid, "build it").await;
    let first = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["Started."]));
    let (s, frames) = decide(&w, &owner, tid, approve(first)).await;
    assert_eq!(s, 200, "{frames:?}");
    next("t1's call", &mut dev.seen.calls).await;

    // The next reply stops on a call that waits for its decision.
    build_call(&w);
    let frames = send(&w, &owner, tid, "and once more").await;
    let second = approval_frames(&frames)[0]["approval_request_id"].clone();

    let held = w.state.hold_chat_thread_for_tests(tid).await;
    dev.complete("t1", "42 files", true);
    patience::until_async("t1 ended", || async {
        super::rows(&w)
            .await
            .iter()
            .any(|r| r.task_id == "t1" && r.state == "ended")
    })
    .await;
    // Its delivery waits for the lock now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    w.chat.push(Turn::text(&["Started again."]));
    let decided = async {
        patience::until_async("the decision is written", || async {
            lmgw_core::store::last_chat_message(&w.state.db, tid)
                .await
                .unwrap()
                .and_then(|m| m.pending_approvals)
                .is_some_and(|p| !p.is_open())
        })
        .await;
        drop(held);
    };
    let ((s, frames), ()) = tokio::join!(decide(&w, &owner, tid, approve(second)), decided);
    assert_eq!(s, 200, "never moved on: {frames:?}");
    assert_eq!(done(&frames)["saved"], true, "{frames:?}");
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(
        roles(&msgs),
        ["user", "assistant", "user", "assistant", "tool"]
    );
    assert_eq!(msgs[4]["task"]["task_id"], "t1");
    assert_eq!(msgs[3]["content"], "Started again.", "{:?}", msgs[3]);
}

/// A result held off by a reply that waits on a call's decision, then a
/// send (design §3.1's third moment): the send declines the call and, in
/// the same write, lets the result in before its message — so the request
/// ends in the user's message with the result before it as context, not in
/// the result (the case the client's WP18 hit as `… A2 U3 [call R] R`).
#[tokio::test]
async fn a_held_result_enters_before_the_message_of_the_send_that_declines_its_gate() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({"require_approval": "always"})).await;
    let owner = w.gw.client();
    build_call(&w);
    let frames = send(&w, &owner, tid, "build it").await;
    let first = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["Started."]));
    let approve = json!([{"approval_request_id": first, "approve": true}]);
    let (s, frames) = decide(&w, &owner, tid, approve).await;
    assert_eq!(s, 200, "{frames:?}");
    next("t1's call", &mut dev.seen.calls).await;

    // The next reply stops on a call that waits for its decision; t1 ends
    // meanwhile, and that reply holds its result off.
    build_call(&w);
    let frames = send(&w, &owner, tid, "and once more").await;
    assert_eq!(approval_frames(&frames).len(), 1, "{frames:?}");
    dev.complete("t1", "42 files", true);
    patience::until_async("t1 ended", || async {
        super::rows(&w)
            .await
            .iter()
            .any(|r| r.task_id == "t1" && r.state == "ended")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !messages(&w, tid).await.iter().any(|m| m["role"] == "tool"),
        "the waiting call holds the result off"
    );

    w.chat.push(Turn::text(&["Then not."]));
    let frames = send(&w, &owner, tid, "never mind").await;
    assert_eq!(done(&frames)["saved"], true, "{frames:?}");
    let msgs = messages(&w, tid).await;
    assert_eq!(
        roles(&msgs),
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "tool",
            "user",
            "assistant"
        ]
    );
    assert_eq!(msgs[4]["task"]["task_id"], "t1");
    assert_eq!(msgs[5]["content"], "never mind");
    assert_eq!(msgs[6]["content"], "Then not.");

    let n = w.chat.seen.chat_count();
    let wire = sent(&w, n - 1);
    assert_strict(&wire);
    let last = &wire[wire.len() - 1];
    assert_eq!(last["role"], "user", "{wire:#?}");
    assert!(
        last["content"].to_string().contains("never mind"),
        "{wire:#?}"
    );
    let before = &wire[wire.len() - 2];
    assert_eq!(before["role"], "tool", "{wire:#?}");
    assert!(
        before["content"].to_string().contains("42 files"),
        "{wire:#?}"
    );
}

/// `answer` while a turn of the thread runs is `409 turn_running`: it would
/// cancel that turn, whose end lets the results in.
#[tokio::test]
async fn answer_while_a_turn_runs_is_refused() {
    let (w, _d, _dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    let running = w.state.chat_turn_held_for_tests(tid).await;
    let path = format!("/chat/api/threads/{tid}/answer");
    let (s, v) = post(&w, &w.gw.client(), &path, json!({})).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("turn_running")), "{v}");
    drop(running);
    let (s, v) = post(&w, &w.gw.client(), &path, json!({})).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (409, Some("nothing_to_answer")),
        "{v}"
    );
}
