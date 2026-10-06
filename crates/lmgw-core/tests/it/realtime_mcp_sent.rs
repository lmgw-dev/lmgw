//! A spoken response's call that went to its server before its item reached
//! the client (realtime-server-tools design §2.5; final review #1): its own
//! deltas wait behind a clause still being synthesized when the response
//! ends — a cancel, or a voice that fails with them queued. The core then
//! announces the call itself, from what the responder told it when the call
//! was sent, and closes it as abandoned: `response.done` carries it, and the
//! next response renders it, so the model does not make it again unaware.

use std::sync::Arc;

use axum::body::Bytes;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::mcp_stub::{held_call_stub, register};
use crate::support::realtime_fakes::{
    events_until, send, types, user_text, ChatFake, Step, Turn, Ws,
};
use crate::support::realtime_mcp::{assert_every_mcp_call, at, listed, rows_at_least, shape};
use crate::support::realtime_tts::{speech, speech_gateway, spoken_session, wav, Tts};
use lmgw_core::state::SharedState;

/// `agent::ABANDONED_CALL`.
const ABANDONED: &str =
    "abandoned when the run was cancelled; it had already been sent, so it may still have run";

fn label_a() -> Value {
    json!([{"type": "mcp", "server_label": "a", "require_approval": "never"}])
}

/// A preamble, then a call of `a__echo` — `call_e1`, `{"text":"early"}`.
fn preamble_and_call() -> Turn {
    Turn::Stream(vec![
        Step::Text("Let me look. "),
        Step::CallStart {
            index: 0,
            id: Some("call_e1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"early"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ])
}

/// A spoken session with the label `a` listed, its server holding every
/// call, the first clause's TTS answer held behind the returned notify and
/// then answered with `first`; a question asked, and the call already with
/// its server while the first clause is still being made.
async fn sent_behind_the_first_clause(first: Bytes) -> (SharedState, Ws, ChatFake, Arc<Notify>) {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let held = Arc::new(Notify::new());
    tts.push(Tts::Held(held.clone(), first));
    chat.push(preamble_and_call());
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({"tools": label_a()})).await;
    listed(&mut ws).await;
    send(&mut ws, user_text("look it up")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    stub.wait_calls(1).await;
    assert_eq!(tts.seen.count(), 1, "still on the first clause");
    (state, ws, chat, held)
}

/// The call the core announced and closed at the response's end: added,
/// its arguments, `.failed`, `output_item.done`, `conversation.item.done`
/// — in that order, never `in_progress` — `output: null` and the abandoned
/// error, with the arguments the model wrote.
fn announced_abandoned(events: &[Value]) -> Value {
    let added = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["item"]["type"] == "mcp_call")
        .unwrap_or_else(|| panic!("the call was never announced: {events:#?}"));
    let id = added["item"]["id"].as_str().unwrap();
    let (added, args_done, failed, output_done, item_done) = (
        at(events, "response.output_item.added", id),
        at(events, "response.mcp_call_arguments.done", id),
        at(events, "response.mcp_call.failed", id),
        at(events, "response.output_item.done", id),
        at(events, "conversation.item.done", id),
    );
    assert!(
        added < args_done && args_done < failed && failed < output_done && output_done < item_done,
        "{events:#?}"
    );
    assert!(
        !types(events).contains(&"response.mcp_call.in_progress"),
        "{events:#?}"
    );
    let call = events[item_done]["item"].clone();
    assert_eq!(call["output"], Value::Null, "{call}");
    assert_eq!(call["error"]["type"], "tool_execution_error", "{call}");
    assert_eq!(call["error"]["message"], ABANDONED, "{call}");
    assert_eq!(
        (&call["server_label"], &call["name"], &call["arguments"]),
        (&json!("a"), &json!("echo"), &json!(r#"{"text":"early"}"#))
    );
    assert_every_mcp_call(events);
    call
}

/// A cancel while the first clause is made: the call's item had not been
/// announced, yet `response.done` carries it abandoned, its row says
/// `canceled`, and the next response reads the call and what became of it.
#[tokio::test]
async fn a_cancel_before_a_sent_call_s_item_was_announced_still_closes_it_abandoned() {
    let (state, mut ws, chat, _held) =
        sent_behind_the_first_clause(wav(&speech(300), 24_000)).await;

    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert!(!types(&events).contains(&"error"), "{events:#?}");
    let call = announced_abandoned(&events);
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert!(done["output"].as_array().unwrap().contains(&call), "{done}");
    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(tool_rows[0].error_kind.as_deref(), Some("canceled"));

    chat.push(Turn::text(&["It was cut."]));
    send(&mut ws, user_text("and now?")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let next = events_until(&mut ws, "response.done").await;
    assert_eq!(next.last().unwrap()["response"]["status"], "completed");
    let sent = shape(&chat.seen.chat(1));
    for part in [
        (
            "assistant".to_string(),
            r#"call:call_e1:a__echo:{"text":"early"}"#.to_string(),
        ),
        ("tool".into(), format!("result:call_e1:{ABANDONED}")),
    ] {
        assert!(sent.contains(&part), "{part:?} not in {sent:?}");
    }
}

/// The voice fails on the first clause, with the call's deltas queued behind
/// it: the response fails, and still carries the call, abandoned — the
/// server had it.
#[tokio::test]
async fn a_voice_failing_before_a_sent_call_s_item_was_announced_still_closes_it_abandoned() {
    let (state, mut ws, chat, held) =
        sent_behind_the_first_clause(Bytes::from_static(b"not a wav")).await;

    held.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert!(types(&events).contains(&"error"), "{events:#?}");
    let call = announced_abandoned(&events);
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "failed", "{done}");
    assert!(done["output"].as_array().unwrap().contains(&call), "{done}");
    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(tool_rows[0].error_kind.as_deref(), Some("canceled"));
    assert_eq!(chat.seen.chat_count(), 1);
}
