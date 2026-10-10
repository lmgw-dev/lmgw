//! The Chat API's event streams against their documented frame types
//! (`lmgw-api-types::chat_frames`): every frame of a send's, a stored
//! reply's read-aloud and a voice warm-up's stream is read into the type of
//! its event name and written back, and must be the frame again, key by key.
//! A field the gateway adds to a frame without adding it to the type is
//! dropped by the read and fails here; so does an event name the document
//! does not list.

use lmgw_api_types::chat_frames as frames;
use lmgw_api_types::chat_turn::{SendRequest, TurnRequest, WarmRequest};
use lmgw_api_types::chat_voice::ModelState;
use lmgw_api_types::mcp_apps::{ToolReadyFrame, ToolResultFrame};
use lmgw_core::config::{Protocol, UpstreamKind};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, openai_sse, sse_events};
use crate::chat_golden::{
    mcp_stub, mount_in_order, openai_call, register_stub, send, sse_reply, thread, tool_thread,
    FULL_SSE,
};
use crate::chat_voice_speak::{post, stored_reply, thread as voice_thread, world, Reader};
use crate::support::realtime_fakes::Turn;

fn round_trip<T: DeserializeOwned + Serialize>(event: &str, live: &Value) -> T {
    let typed: T =
        serde_json::from_value(live.clone()).unwrap_or_else(|e| panic!("{event}: {e}\n{live}"));
    assert_eq!(
        &serde_json::to_value(&typed).unwrap(),
        live,
        "{event}: the type drops or invents a field"
    );
    typed
}

/// Every frame in `events` read into its type and written back; the event
/// names seen.
///
/// Each frame is also validated against the schema the document lists for its
/// event on the send route (`x-lmgw-sse-events`; the turn routes share one
/// frame set), so a frame the types read but the document does not describe
/// fails here.
pub(crate) fn typed_frames(events: &[(String, Value)]) -> Vec<String> {
    let mut seen = Vec::new();
    for (event, data) in events {
        frame_validates("/chat/api/threads/{id}/send", event, data);
        match event.as_str() {
            "turn" => drop(round_trip::<frames::TurnStarted>(event, data)),
            "retrieval" => drop(round_trip::<frames::RetrievalFrame>(event, data)),
            "delta" | "reasoning" => drop(round_trip::<frames::TextFrame>(event, data)),
            "tool" => {
                let tool = round_trip::<frames::ToolFrame>(event, data);
                // The shared MCP Apps frame types read the same frames.
                match tool {
                    frames::ToolFrame::Ready(_) => {
                        let mut bare = data.clone();
                        bare.as_object_mut().unwrap().remove("event");
                        drop(round_trip::<ToolReadyFrame>("tool ready", &bare));
                    }
                    frames::ToolFrame::Result(_) => {
                        let mut bare = data.clone();
                        bare.as_object_mut().unwrap().remove("event");
                        drop(round_trip::<ToolResultFrame>("tool result", &bare));
                    }
                    _ => {}
                }
            }
            "usage" => drop(round_trip::<frames::UsageFrame>(event, data)),
            "stop" => drop(round_trip::<frames::StopFrame>(event, data)),
            "stats" => drop(round_trip::<frames::Timings>(event, data)),
            "error" => drop(round_trip::<frames::ErrorFrame>(event, data)),
            "done" => drop(round_trip::<frames::DoneFrame>(event, data)),
            "state" => drop(round_trip::<ModelState>(event, data)),
            "voice" => drop(round_trip::<frames::VoiceFrame>(event, data)),
            "speech" => drop(round_trip::<frames::SpeechFrame>(event, data)),
            "speech_done" => drop(round_trip::<frames::SpeechDone>(event, data)),
            "speech_error" => drop(round_trip::<frames::SpeechError>(event, data)),
            other => panic!("an event the document does not list: {other} {data}"),
        }
        if !seen.contains(event) {
            seen.push(event.clone());
        }
    }
    seen
}

/// `data` validated against the `event` frame schema `route`'s POST lists.
fn frame_validates(route: &str, event: &str, data: &Value) {
    let doc = lmgw_core::openapi::admin_doc();
    let schema = doc
        .pointer(&format!(
            "/paths/{}/post/responses/200/x-lmgw-sse-events/{event}",
            route.replace('~', "~0").replace('/', "~1")
        ))
        .unwrap_or_else(|| panic!("{route} lists no {event} frame"));
    crate::common::validates_against(&format!("{route} {event} frame"), doc, schema, data);
}

fn has(seen: &[String], names: &[&str]) {
    for n in names {
        assert!(seen.iter().any(|s| s == n), "no {n} frame in {seen:?}");
    }
}

#[test]
fn the_requests_write_what_the_handlers_read() {
    let send = SendRequest {
        content: "hi".into(),
        attachments: vec![1],
        kb_refs: vec![2],
        voice: Some(serde_json::from_value(json!({"via": "dictation", "asr_ms": 3})).unwrap()),
        speak: true,
    };
    assert_eq!(
        serde_json::to_value(&send).unwrap(),
        json!({"content": "hi", "attachments": [1], "kb_refs": [2],
               "voice": {"via": "dictation", "asr_ms": 3}, "speak": true})
    );
    assert_eq!(
        serde_json::to_value(TurnRequest::default()).unwrap(),
        json!({})
    );
    assert!(serde_json::from_value::<WarmRequest>(json!({"stages": ["asr"], "x": 1})).is_err());
}

/// A reasoning model's plain turn: turn, reasoning, delta, stop, stats,
/// usage, done.
#[tokio::test]
async fn a_plain_turns_frames_are_the_documented_types() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_reply(FULL_SSE))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    let events = sse_events(&send(&gw, tid, "hi there").await);
    let seen = typed_frames(&events);
    has(
        &seen,
        &[
            "turn",
            "reasoning",
            "delta",
            "stop",
            "stats",
            "usage",
            "done",
        ],
    );
    let done: frames::DoneFrame = serde_json::from_value(events.last().unwrap().1.clone()).unwrap();
    assert!(done.saved.is_some_and(|s| s.timings.is_some()));
}

/// A tool turn: start, args, ready and result frames, then done.
#[tokio::test]
async fn a_tool_turns_frames_are_the_documented_types() {
    let mock = MockServer::start().await;
    let first = format!(
        "data: {}\n\n{}",
        json!({"choices": [{"delta": {"reasoning_content": "Need the tool."}}]}),
        openai_call("Checking. ", "c1", "stub__echo", "{\"text\":\"hi\"}")
    );
    mount_in_order(&mock, &[first, openai_sse("it said hi", 20, 4)]).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;
    let events = sse_events(&send(&gw, tid, "say hi through the tool").await);
    let seen = typed_frames(&events);
    has(&seen, &["turn", "tool", "delta", "done"]);
    // A saved cloud turn: the done frame sends `null` for everything a
    // fallback or a local model would fill, and the document must allow it.
    let done = &events.last().unwrap().1;
    assert_eq!(done["saved"], json!(true), "{done}");
    assert!(
        done["answered_by"].is_null() && done["timings"].is_null(),
        "{done}"
    );
    let kinds: Vec<&str> = events
        .iter()
        .filter(|(e, _)| e == "tool")
        .map(|(_, d)| d["event"].as_str().unwrap())
        .collect();
    for k in ["start", "args", "ready", "result"] {
        assert!(kinds.contains(&k), "no {k} tool frame in {kinds:?}");
    }
}

/// An upstream that fails: error, then done {aborted}.
#[tokio::test]
async fn a_refused_turns_frames_are_the_documented_types() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    let events = sse_events(&send(&gw, tid, "hi").await);
    let seen = typed_frames(&events);
    has(&seen, &["error", "done"]);
    assert_eq!(events.last().unwrap().1, json!({"aborted": true}));
}

/// A send with `speak`, the read-aloud of a stored reply and a warm-up.
#[tokio::test]
async fn the_speech_and_warm_frames_are_the_documented_types() {
    let w = world(|_| {}).await;
    let tid = voice_thread(&w.gw, "chatty").await;
    w.chat
        .push(Turn::text(&["Erste Antwort. ", "Zweiter Satz."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "sprich", "speak": true}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    let seen = typed_frames(&events);
    has(
        &seen,
        &["turn", "delta", "voice", "speech", "speech_done", "done"],
    );

    w.chat.push(Turn::text(&["Noch eine."]));
    let mid = stored_reply(&w.gw, tid, "und noch").await;
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    has(&typed_frames(&events), &["voice", "speech", "speech_done"]);

    // Continue and regenerate stream the same frames; a bare body is fine.
    w.chat.push(Turn::text(&["Anders."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/regenerate"),
        json!({"speak": true}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    has(&typed_frames(&events), &["delta", "speech", "done"]);

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/voice/warm"),
        json!({"stages": ["tts"]}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    // Its `done` is a frame of its own: no fields.
    let (done, states) = events.split_last().unwrap();
    typed_frames(states);
    assert_eq!(done.0, "done");
    frame_validates("/chat/api/threads/{id}/voice/warm", "done", &done.1);
    round_trip::<frames::WarmDone>("done", &done.1);
    assert_eq!(done.1, json!({}));
}
