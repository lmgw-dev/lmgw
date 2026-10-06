//! Server-side MCP calls in a spoken response (realtime-server-tools design
//! §2.2, §2.4; WP3): the round trip — the preamble's audio item first, then
//! the call's item and its run, the message closing when its audio has
//! played, then `response.done` — and the order rule: a call runs at once,
//! but is reported behind its own item, however long the preamble before it
//! takes to synthesize.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::mcp_stub::{echo_stub, register};
use crate::support::realtime_fakes::{events_until, next_event, send, user_text, Step, Turn, Ws};
use crate::support::realtime_mcp::{assert_every_mcp_call, at, listed, shape};
use crate::support::realtime_tts::{speech, speech_gateway, spoken_session, wav, Tts};

/// A long lead: nothing waits for playback.
const NO_WAIT: u32 = 60_000;

/// The label `a` as `@openai/agents` declares it.
fn tools() -> Value {
    json!({"tools": [{"type": "mcp", "server_label": "a", "require_approval": "never"}]})
}

/// The first event of `kind` on an item of type `item_type`.
fn first(events: &[Value], kind: &str, item_type: &str) -> usize {
    events
        .iter()
        .position(|e| e["type"] == kind && e["item"]["type"] == item_type)
        .unwrap_or_else(|| panic!("no {kind} of a {item_type} in {events:#?}"))
}

/// Events up to the `conversation.item.done` of the response's `mcp_call`.
async fn until_call_done(ws: &mut Ws) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(ws).await;
        let done = ev["type"] == "conversation.item.done" && ev["item"]["type"] == "mcp_call";
        events.push(ev);
        if done {
            return events;
        }
    }
}

/// The spoken round trip: the preamble's audio, then the call's item, its
/// arguments, its run and its done events; the message closes once its
/// audio has played, and `response.done` carries both. The SDK's follow-up,
/// sent on the call's `conversation.item.done`, renders the preamble, the
/// call and its result.
#[tokio::test]
async fn a_spoken_round_trip() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let stub = echo_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    chat.push(Turn::Stream(vec![
        Step::Text("Let me look. "),
        Step::CallStart {
            index: 0,
            id: Some("call_s1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"spoken"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]));
    chat.push(Turn::text(&["It says spoken."]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, tools()).await;
    listed(&mut ws).await;
    send(&mut ws, user_text("look it up")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;

    let mut events = until_call_done(&mut ws).await;
    let message = first(&events, "response.output_item.added", "message");
    let audio = events
        .iter()
        .position(|e| e["type"] == "response.output_audio.delta")
        .unwrap();
    let call = first(&events, "response.output_item.added", "mcp_call");
    let call_id = events[call]["item"]["id"].as_str().unwrap().to_string();
    assert_eq!(events[call]["output_index"], 1);
    let args_done = at(&events, "response.mcp_call_arguments.done", &call_id);
    let running = at(&events, "response.mcp_call.in_progress", &call_id);
    let completed = at(&events, "response.mcp_call.completed", &call_id);
    assert!(
        message < audio && audio < call && call < args_done,
        "{events:#?}"
    );
    assert!(args_done < running && running < completed, "{events:#?}");
    assert_eq!(events.last().unwrap()["item"]["output"], "echo: spoken");

    send(&mut ws, json!({"type": "response.create"})).await;
    let rest = events_until(&mut ws, "response.done").await;
    let message_done = first(&rest, "response.output_item.done", "message");
    assert!(
        rest[..message_done]
            .iter()
            .any(|e| e["type"] == "response.output_audio.done"),
        "{rest:#?}"
    );
    events.extend(rest);
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(
        done["output"][0]["content"][0]["transcript"],
        "Let me look."
    );
    assert_eq!(done["output"][1]["output"], "echo: spoken");
    events.extend(events_until(&mut ws, "response.done").await);
    assert!(!events.iter().any(|e| e["type"] == "error"), "{events:#?}");
    assert_every_mcp_call(&events);

    let sent = shape(&chat.seen.chat(1));
    let assistant = &sent[sent.len() - 2];
    assert_eq!(assistant.0, "assistant");
    assert!(
        assistant.1.starts_with("Let me look.")
            && assistant
                .1
                .ends_with(r#"call:call_s1:a__echo:{"text":"spoken"}"#),
        "{sent:?}"
    );
    assert_eq!(
        sent.last().unwrap(),
        &(
            "tool".to_string(),
            "result:call_s1:echo: spoken".to_string()
        )
    );
    assert_eq!(tts.seen.count(), 2, "one clause each");
}

/// §2.4's order rule: the preamble's first clause is still being
/// synthesized when the stream ends, and the call runs then — the server
/// has it before any audio exists — yet its `in_progress` and `completed`
/// come only after its item, which follows the preamble's audio.
#[tokio::test]
async fn a_fast_call_reports_after_its_item_behind_a_long_preamble() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let stub = echo_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    chat.push(Turn::Stream(vec![
        Step::Text("Let me look that up for you. "),
        Step::Text("It might take a moment. "),
        Step::Text("Bear with me please. "),
        Step::CallStart {
            index: 0,
            id: Some("call_q1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"quick"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]));
    let held = Arc::new(Notify::new());
    tts.push(Tts::Held(held.clone(), wav(&speech(300), 24_000)));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, tools()).await;
    listed(&mut ws).await;
    send(&mut ws, user_text("quick one")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;

    // The call ran while the first clause is still held.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stub.calls().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the call did not run while the preamble was synthesized"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(tts.seen.count(), 1, "still on the first clause");
    held.notify_one();

    let events = events_until(&mut ws, "response.done").await;
    let audio = events
        .iter()
        .position(|e| e["type"] == "response.output_audio.delta")
        .unwrap();
    let call = first(&events, "response.output_item.added", "mcp_call");
    let call_id = events[call]["item"]["id"].as_str().unwrap().to_string();
    let running = at(&events, "response.mcp_call.in_progress", &call_id);
    let completed = at(&events, "response.mcp_call.completed", &call_id);
    assert!(
        audio < call && call < running && running < completed,
        "{events:#?}"
    );
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"][1]["output"], "echo: quick");
}
