//! A late MCP task result and a bound session (MCP Tasks design §3.4,
//! §4.2): `lmgw.task.done`, the continuation on a `response.create` with no
//! new words, the owed set at bind, a result that ends during a response,
//! the continuation's refusals, and ruling 22's device that binds to speak
//! a result, and a silent push-to-talk turn that speaks none; a spoken
//! turn's user message lets a waiting result in before itself, as a send's
//! does.
//!
//! The job is started by a dashboard send (the tool hosted by the fake task
//! device of `mcp_tasks`, the model scripted by the chat fake); the session
//! is bound with text output and manual turns.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{next, of_type, say, try_next, until, until_type, World};
use crate::chat_approvals::{approval_frames, decide, send as chat_send};
use crate::chat_feed::Feed;
use crate::mcp_tasks::thread::{assert_strict, build_call, sent, tool_thread, until_results};
use crate::mcp_tasks::{row_in, task_world, Script, TaskDevice};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn, Ws};

/// A thread with the device's label whose dashboard send started job `t1`
/// (answered "Started."): the world, the device and the thread.
async fn started() -> (World, TaskDevice, i64) {
    let (w, _d, dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    chat_send(&w, &w.gw.client(), tid, "build it").await;
    (w, dev, tid)
}

/// A session bound to `tid` with text output and manual turns.
async fn text_voice(w: &World, tid: i64) -> Ws {
    let (mut ws, _) = w.bind(tid).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    ws
}

/// The thread's messages' roles, in order.
async fn roles(w: &World, tid: i64) -> Vec<String> {
    crate::mcp_tasks::thread::messages(w, tid)
        .await
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect()
}

/// A bare `response.create` (no new words): its events up to the first
/// `error` or `response.done`.
async fn answer(ws: &mut Ws) -> Vec<Value> {
    send(ws, json!({"type": "response.create"})).await;
    until(ws, |e| e["type"] == "error" || e["type"] == "response.done").await
}

/// The code a refused response ended with: its `error` event's, or its
/// `response.done {failed}`'s.
fn refusal(events: &[Value]) -> String {
    let last = events.last().unwrap();
    let code = match last["type"].as_str() {
        Some("error") => &last["error"]["code"],
        _ => {
            assert_eq!(last["response"]["status"], "failed", "{events:#?}");
            &last["response"]["status_details"]["error"]["code"]
        }
    };
    code.as_str().unwrap_or_default().to_string()
}

/// The result that entered while the session was bound is said once, with
/// the feed's facts; a `response.create` with no new words then answers it
/// — no user message, the result's pair joined to the reply before it —
/// and once answered, a second one is `empty_turn`.
#[tokio::test]
async fn a_result_that_enters_is_said_and_a_bare_response_create_answers_it() {
    let (w, dev, tid) = started().await;
    let mut ws = text_voice(&w, tid).await;
    let task_id = w.get(&format!("/chat/api/threads/{tid}")).await["tasks"][0]["id"]
        .as_i64()
        .unwrap();

    dev.complete("t1", "42 files", true);
    let events = until_type(&mut ws, "lmgw.task.done").await;
    let said = events.last().unwrap();
    let msgs = until_results(&w, tid, 1).await;
    let result = msgs.last().unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(
        said,
        &json!({"type": "lmgw.task.done", "event_id": said["event_id"], "thread_id": tid,
                "message_id": result["id"], "id": task_id, "task_id": "t1",
                "server_label": "desktop", "tool": "desktop__build", "status": "completed",
                "by": null})
    );

    w.chat.push(Turn::text(&["It built ", "42 files."]));
    let events = answer(&mut ws).await;
    let done = events.last().unwrap();
    assert_eq!(done["type"], "response.done", "{events:#?}");
    assert_eq!(done["response"]["status"], "completed", "{done}");
    assert!(
        of_type(&events, "lmgw.chat.user").is_empty(),
        "a continuation writes no user message: {events:#?}"
    );
    // The model saw the result as a call joined to the reply before it.
    assert_eq!(w.chat.seen.chat_count(), 3);
    let msgs = sent(&w, 2);
    assert_strict(&msgs);
    let n = msgs.len();
    assert_eq!(msgs[n - 2]["role"], "assistant", "{msgs:#?}");
    assert_eq!(msgs[n - 2]["content"], "Started.", "{msgs:#?}");
    assert_eq!(
        msgs[n - 2]["tool_calls"][0]["function"]["name"],
        "lmgw__job_result"
    );
    assert_eq!(msgs[n - 1]["role"], "tool");
    assert!(
        msgs[n - 1]["content"]
            .as_str()
            .is_some_and(|c| c.contains("42 files")),
        "{msgs:#?}"
    );
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "tool", "assistant"]
    );
    let m = crate::mcp_tasks::thread::messages(&w, tid).await;
    assert_eq!(m[3]["content"], "It built 42 files.");
    assert_eq!(m[3]["voice"]["via"], "realtime", "{:?}", m[3]);

    // Answered: nothing is owed any more.
    let events = answer(&mut ws).await;
    assert_eq!(refusal(&events), "empty_turn", "{events:#?}");
    assert_eq!(w.chat.seen.chat_count(), 3, "no model call");
}

/// With no session bound, the result enters the thread and the feed has its
/// `task.done`; a session bound afterwards owes it without saying it again
/// (ruling 22: the client learned of it from the feed), and its bare
/// `response.create` answers it.
#[tokio::test]
async fn a_result_that_entered_before_the_bind_is_owed_at_bind() {
    let (w, dev, tid) = started().await;
    let owner = w.gw.client();
    let mut feed = Feed::open(&w, &owner, "", None).await;
    dev.complete("t1", "42 files", true);
    feed.until(10, |f| f.iter().any(|f| f.event == "task.done"))
        .await;
    let msgs = until_results(&w, tid, 1).await;
    let done = feed.frames.iter().find(|f| f.event == "task.done").unwrap();
    assert_eq!(done.data["message_id"], msgs.last().unwrap()["id"]);

    let mut ws = text_voice(&w, tid).await;
    w.chat.push(Turn::text(&["It built them."]));
    let events = answer(&mut ws).await;
    assert!(
        of_type(&events, "lmgw.task.done").is_empty(),
        "said by the feed, not again: {events:#?}"
    );
    let last = events.last().unwrap();
    assert_eq!(last["response"]["status"], "completed", "{events:#?}");
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "tool", "assistant"]
    );
    assert_strict(&sent(&w, 2));
}

/// A job that ends while a response's turn runs enters only once that turn
/// ended, after its reply, and is said only then; the next bare
/// `response.create` answers it.
#[tokio::test]
async fn a_result_that_ends_during_a_response_waits_for_it() {
    let (w, dev, tid) = started().await;
    let mut ws = text_voice(&w, tid).await;
    let gate = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Wie geht's?"));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Gut."),
        Step::Wait(gate.clone()),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    say(&mut ws).await;
    crate::common::patience::until_async("the spoken turn's model call", || async {
        w.chat.seen.chat_count() == 3
    })
    .await;
    dev.complete("t1", "42 files", true);
    row_in(&w, "ended").await;
    // While the turn runs: no result in the thread, and nothing said.
    let mut seen = Vec::new();
    while let Some(ev) = try_next(&mut ws, 1).await {
        seen.push(ev);
    }
    assert!(
        of_type(&seen, "lmgw.task.done").is_empty(),
        "said during the response: {seen:#?}"
    );
    assert_eq!(roles(&w, tid).await, ["user", "assistant", "user"]);

    gate.notify_one();
    let events = until_type(&mut ws, "lmgw.task.done").await;
    let said = events.last().unwrap();
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "user", "assistant", "tool"],
        "the result after the reply that did not see it"
    );
    assert_eq!(said["message_id"], msgs[4]["id"]);
    // Let the response end, if its done came after the event.
    if of_type(&events, "response.done").is_empty() {
        until_type(&mut ws, "response.done").await;
    }

    w.chat.push(Turn::text(&["Fertig."]));
    let events = answer(&mut ws).await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:#?}"
    );
    let msgs = sent(&w, 3);
    assert_strict(&msgs);
    assert_eq!(msgs.last().unwrap()["role"], "tool", "{msgs:#?}");
    assert_eq!(
        roles(&w, tid).await,
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "tool",
            "assistant"
        ]
    );
}

/// Push-to-talk (K26): a commit of silence and its `response.create` are a
/// cough's equal — refused `empty_turn`, the result not spoken, no model
/// call; the next bare `response.create`, with no commit before it, answers
/// the result.
#[tokio::test]
async fn a_silent_push_to_talk_turn_speaks_no_result() {
    let (w, dev, tid) = started().await;
    let mut ws = text_voice(&w, tid).await;
    dev.complete("t1", "42 files", true);
    until_type(&mut ws, "lmgw.task.done").await;

    w.asr.push(Asr::Text(""));
    say(&mut ws).await;
    // Created before its transcript was in: refused at its launch, an
    // `error` and its `response.done {failed}`.
    let events = until_type(&mut ws, "response.done").await;
    assert_eq!(refusal(&events), "empty_turn", "{events:#?}");
    assert_eq!(
        of_type(&events, "error")[0]["error"]["code"],
        "empty_turn",
        "{events:#?}"
    );
    assert_eq!(w.chat.seen.chat_count(), 2, "no model call");
    assert_eq!(roles(&w, tid).await, ["user", "assistant", "tool"]);

    w.chat.push(Turn::text(&["It built 42 files."]));
    let events = answer(&mut ws).await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:#?}"
    );
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "tool", "assistant"]
    );
}

/// The continuation never cancels a turn of the thread: while one runs it
/// fails `turn_running`, and the running turn goes on; once a turn of
/// another window answered the results, a bare `response.create` is
/// `empty_turn`.
#[tokio::test]
async fn a_continuation_neither_cancels_a_turn_nor_answers_twice() {
    let (w, dev, tid) = started().await;
    let mut ws = text_voice(&w, tid).await;
    dev.complete("t1", "42 files", true);
    until_type(&mut ws, "lmgw.task.done").await;

    let running = w.state.chat_turn_held_for_tests(tid).await;
    let events = answer(&mut ws).await;
    assert_eq!(refusal(&events), "turn_running", "{events:#?}");
    assert_eq!(w.chat.seen.chat_count(), 2, "no model call");
    drop(running);

    // The dashboard answers the result.
    w.chat.push(Turn::text(&["Done, 42 files."]));
    chat_send(&w, &w.gw.client(), tid, "and?").await;
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "tool", "user", "assistant"]
    );
    // Its stream ends a moment before its turn lets the thread go: until
    // then a continuation is `turn_running`, as it should be.
    let mut events = answer(&mut ws).await;
    for _ in 0..100 {
        if refusal(&events) != "turn_running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        events = answer(&mut ws).await;
    }
    assert_eq!(refusal(&events), "empty_turn", "{events:#?}");
    assert_eq!(w.chat.seen.chat_count(), 3, "no model call");
}

/// Ruling 22's flow, as a device runs it: the result entered with no
/// session bound; the device binds with its own key and `takeover=never`,
/// is told nothing again, and its bare `response.create` answers the result
/// as the device's own turn.
#[tokio::test]
async fn a_device_binds_to_speak_a_result_it_learned_of_from_the_feed() {
    let (w, d, dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    chat_send(&w, &w.gw.client(), tid, "build it").await;
    dev.complete("t1", "42 files", true);
    until_results(&w, tid, 1).await;

    let bearer = format!("Bearer {}", d.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={tid}&takeover=never"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    w.chat.push(Turn::text(&["It built 42 files."]));
    let events = answer(&mut ws).await;
    assert!(of_type(&events, "lmgw.task.done").is_empty(), "{events:#?}");
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:#?}"
    );
    let m = crate::mcp_tasks::thread::messages(&w, tid).await;
    assert_eq!(m.last().unwrap()["content"], "It built 42 files.");
    assert_eq!(m.last().unwrap()["role"], "assistant");
}

/// A spoken turn (its transcript, the user message the journal writes for
/// the response) finds a result held off by a reply that waits on a call's
/// decision: the message declines the call and, in the same write, the
/// result enters before it (MCP Tasks design §3.1's third moment) — the
/// model answers the spoken words with the result as context, and the
/// session says the result.
#[tokio::test]
async fn a_spoken_turn_lets_a_held_result_in_before_its_message() {
    let (w, _d, mut dev, _server) = task_world(Script::default()).await;
    let tid = tool_thread(&w, json!({"require_approval": "always"})).await;
    let owner = w.gw.client();
    build_call(&w);
    let frames = chat_send(&w, &owner, tid, "build it").await;
    let first = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["Started."]));
    let approve = json!([{"approval_request_id": first, "approve": true}]);
    let (s, frames) = decide(&w, &owner, tid, approve).await;
    assert_eq!(s, 200, "{frames:?}");
    crate::mcp_tasks::next("t1's call", &mut dev.seen.calls).await;
    build_call(&w);
    let frames = chat_send(&w, &owner, tid, "and once more").await;
    assert_eq!(approval_frames(&frames).len(), 1, "{frames:?}");
    dev.complete("t1", "42 files", true);
    row_in(&w, "ended").await;
    assert_eq!(
        roles(&w, tid).await,
        ["user", "assistant", "user", "assistant"]
    );

    let mut ws = text_voice(&w, tid).await;
    w.asr.push(Asr::Text("Wie geht's?"));
    w.chat.push(Turn::text(&["Gut."]));
    say(&mut ws).await;
    let events = until(&mut ws, |e| {
        e["type"] == "response.done" || e["type"] == "error"
    })
    .await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:#?}"
    );
    assert_eq!(
        roles(&w, tid).await,
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
    let msgs = crate::mcp_tasks::thread::messages(&w, tid).await;
    assert_eq!(msgs[5]["content"], "Wie geht's?");
    let n = w.chat.seen.chat_count();
    let wire = sent(&w, n - 1);
    assert_strict(&wire);
    let last = &wire[wire.len() - 1];
    assert_eq!(last["role"], "user", "{wire:#?}");
    assert!(
        last["content"].to_string().contains("Wie geht's?"),
        "{wire:#?}"
    );
    let before = &wire[wire.len() - 2];
    assert_eq!(before["role"], "tool", "{wire:#?}");
    assert!(
        before["content"].to_string().contains("42 files"),
        "{wire:#?}"
    );
    // The session says the result that entered.
    let mut seen = events;
    while of_type(&seen, "lmgw.task.done").is_empty() {
        seen.push(next(&mut ws).await);
    }
}
