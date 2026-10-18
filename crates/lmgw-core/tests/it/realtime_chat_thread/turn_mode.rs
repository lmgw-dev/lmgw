//! `session.lmgw.turn_mode` on the WebSocket (realtime calls design §2.2,
//! C23): resolved by lmgw at the bind from the thread's turn detection, with
//! `audio.input.turn_detection` set to match and both echoed in
//! `session.created`; switched by a `session.update`, which writes the turn
//! detection it implies; an update whose mode and turn detection disagree
//! refused; turn detection named alone moving the mode; absent on an
//! unbound session, which refuses it.

use serde_json::{json, Value};

use super::{next, world, World};
use crate::support::realtime_fakes::{next_event, send, Ws};

/// A thread whose voice ends turns by `rule`, bound with the dashboard's
/// cookie: the socket and its `session.created` session.
async fn bound_on(w: &World, rule: &str) -> (Ws, i64, Value) {
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"voice": {"language": "de", "turn_detection": rule}}),
    )
    .await;
    let (ws, created) = w.bind(tid).await;
    (ws, tid, created["session"].clone())
}

/// The session's mode and the type of its turn detection (`None`: null).
fn mode(session: &Value) -> (Value, Option<String>) {
    let td = &session["audio"]["input"]["turn_detection"];
    (
        session["lmgw"]["turn_mode"].clone(),
        td["type"].as_str().map(str::to_string),
    )
}

/// `session` as a `session.update`: the event's answer.
async fn update(ws: &mut Ws, mut session: Value) -> Value {
    session["type"] = json!("realtime");
    send(
        ws,
        json!({"type": "session.update", "event_id": "u", "session": session}),
    )
    .await;
    next(ws).await
}

#[tokio::test]
async fn a_bound_session_starts_in_its_thread_s_mode() {
    let w = world(|_| {}).await;
    let (_ws, _, s) = bound_on(&w, "push_to_talk").await;
    assert_eq!(mode(&s), (json!("push_to_talk"), None), "{s}");
    assert!(s["audio"]["input"]["turn_detection"].is_null(), "{s}");
    let (_ws, _, s) = bound_on(&w, "server_vad").await;
    assert_eq!(
        mode(&s),
        (json!("automatic"), Some("server_vad".into())),
        "{s}"
    );
    let (_ws, _, s) = bound_on(&w, "semantic_vad").await;
    assert_eq!(
        mode(&s),
        (json!("automatic"), Some("semantic_vad".into())),
        "{s}"
    );
}

#[tokio::test]
async fn a_switch_sets_the_turn_detection_and_a_disagreeing_update_is_refused() {
    let w = world(|_| {}).await;
    let (mut ws, _, _) = bound_on(&w, "push_to_talk").await;

    // To automatic on a push-to-talk thread: semantic_vad.
    let ev = update(&mut ws, json!({"lmgw": {"turn_mode": "automatic"}})).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
    assert_eq!(
        mode(&ev["session"]),
        (json!("automatic"), Some("semantic_vad".into()))
    );
    // And back.
    let ev = update(&mut ws, json!({"lmgw": {"turn_mode": "push_to_talk"}})).await;
    assert_eq!(mode(&ev["session"]), (json!("push_to_talk"), None), "{ev}");

    // A mode and a turn detection that disagree: refused, nothing changes.
    let ev = update(
        &mut ws,
        json!({"lmgw": {"turn_mode": "automatic"},
               "audio": {"input": {"turn_detection": null}}}),
    )
    .await;
    assert_eq!(ev["type"], "error", "{ev}");
    assert_eq!(ev["error"]["code"], "invalid_value");
    assert_eq!(ev["error"]["param"], "session.lmgw.turn_mode");
    assert_eq!(ev["error"]["event_id"], "u");
    // Agreeing: as named.
    let ev = update(
        &mut ws,
        json!({"lmgw": {"turn_mode": "automatic"},
               "audio": {"input": {"turn_detection": {"type": "server_vad"}}}}),
    )
    .await;
    assert_eq!(
        mode(&ev["session"]),
        (json!("automatic"), Some("server_vad".into())),
        "{ev}"
    );

    // Turn detection named alone: the mode follows it.
    let ev = update(
        &mut ws,
        json!({"audio": {"input": {"turn_detection": null}}}),
    )
    .await;
    assert_eq!(mode(&ev["session"]), (json!("push_to_talk"), None), "{ev}");

    // The whole session echoed back agrees with itself.
    let echo = ev["session"].clone();
    let ev = update(&mut ws, echo).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");

    // An unknown mode.
    let ev = update(&mut ws, json!({"lmgw": {"turn_mode": "hybrid"}})).await;
    assert_eq!(ev["error"]["code"], "invalid_value", "{ev}");
}

/// The mode is the thread's at the bind: a change of the thread's turn
/// detection applies to the next one.
#[tokio::test]
async fn the_thread_s_change_applies_at_the_next_bind() {
    let w = world(|_| {}).await;
    let (ws, tid, s) = bound_on(&w, "server_vad").await;
    assert_eq!(mode(&s).0, json!("automatic"));
    w.set(
        tid,
        json!({"voice": {"language": "de", "turn_detection": "push_to_talk"}}),
    )
    .await;
    drop(ws);
    let (_ws, created) = w.bind(tid).await;
    assert_eq!(mode(&created["session"]), (json!("push_to_talk"), None));
}

#[tokio::test]
async fn an_unbound_session_has_no_mode_and_refuses_one() {
    let w = world(|_| {}).await;
    let mut plain = w.connect("model=chatty", &[]).await.unwrap();
    let created = next_event(&mut plain).await;
    assert!(
        created["session"]["lmgw"].get("turn_mode").is_none(),
        "{created}"
    );
    let ev = update(&mut plain, json!({"lmgw": {"turn_mode": "automatic"}})).await;
    assert_eq!(ev["type"], "error", "{ev}");
    assert_eq!(ev["error"]["code"], "invalid_value");
    assert_eq!(ev["error"]["param"], "session.lmgw.turn_mode");
    // Turn detection alone works as ever, and no mode appears.
    let ev = update(
        &mut plain,
        json!({"audio": {"input": {"turn_detection": null}}}),
    )
    .await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
    assert!(ev["session"]["lmgw"].get("turn_mode").is_none(), "{ev}");
}
