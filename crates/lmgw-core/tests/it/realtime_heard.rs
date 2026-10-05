//! Heard, not generated (realtime design §4.3, §7.3): truncating a spoken
//! item cuts its transcript to what the listener heard — by character,
//! inside the clause the cut falls in — and the next response renders that.

use std::time::Duration;

use serde_json::{json, Value};

use crate::support::realtime_fakes::{events_until, next_event, send, types, user_text, Turn, Ws};
use crate::support::realtime_tts::{speech, speech_gateway, spoken_session, wav};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn truncate(item: &str, ms: u64, event_id: &str) -> Value {
    json!({"type": "conversation.item.truncate", "event_id": event_id, "item_id": item,
           "content_index": 0, "audio_end_ms": ms})
}

/// "Hello there." then "How are you today?", one second of audio each, as
/// one spoken item; returns its id.
async fn spoken_item(ws: &mut Ws) -> String {
    send(ws, user_text("hi")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    let events = events_until(ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    events[1]["item"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_truncate_keeps_what_was_heard_and_the_next_response_renders_it() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(&["Hello there. ", "How are you today?"]));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    let item = spoken_item(&mut ws).await;

    // 1.5 s in: the first clause whole, and half of the second's 18
    // characters ("How are y"), back to the last word heard whole (§7.3).
    send(&mut ws, truncate(&item, 1500, "t1")).await;
    let ev = next_event(&mut ws).await;
    assert_eq!(
        ev,
        json!({"type": "conversation.item.truncated", "event_id": ev["event_id"],
               "item_id": item, "content_index": 0, "audio_end_ms": 1500})
    );
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved");
    assert_eq!(
        got["item"]["content"][0]["transcript"],
        "Hello there. How are"
    );

    // The model is told what was heard, not what it generated.
    chat.push(Turn::text(&["Fine."]));
    send(&mut ws, user_text("next")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    events_until(&mut ws, "response.done").await;
    let messages = chat.seen.chat(1)["messages"].clone();
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(messages[2]["content"], "Hello there. How are");

    // A second truncate cuts further, never back.
    send(&mut ws, truncate(&item, 500, "t2")).await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.truncated"
    );
    send(&mut ws, truncate(&item, 1000, "t3")).await;
    let e = next_event(&mut ws).await;
    assert_eq!(code(&e), "invalid_value", "{e}");
}

#[tokio::test]
async fn a_truncate_beyond_the_audio_is_an_error_and_changes_nothing() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(&["Hello there. ", "How are you today?"]));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    let item = spoken_item(&mut ws).await;

    send(&mut ws, truncate(&item, 2001, "late")).await;
    let e = next_event(&mut ws).await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["param"], "audio_end_ms");
    assert_eq!(e["error"]["event_id"], "late");
    assert!(
        e["error"]["message"].as_str().unwrap().contains("2000 ms"),
        "{e}"
    );
    // The whole item is still there, and its exact length is accepted.
    send(&mut ws, truncate(&item, 2000, "whole")).await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.truncated"
    );
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["item"]["content"][0]["transcript"],
        "Hello there. How are you today?"
    );
    // A content index the item has no audio at.
    let mut wrong = truncate(&item, 0, "idx");
    wrong["content_index"] = json!(1);
    send(&mut ws, wrong).await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["param"], "content_index", "{e}");
}

#[tokio::test]
async fn truncating_the_item_still_playing_stops_the_rest_of_its_response() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(600), 24_000));
    chat.push(Turn::text(&["Hello there. ", "How are you?"]));
    // No lead: one chunk out, the rest still queued when the truncate lands.
    let (mut ws, _) = spoken_session(&addr, &[], 0, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let mut events = events_until(&mut ws, "response.output_audio.delta").await;
    let item = events[1]["item"]["id"].as_str().unwrap().to_string();
    // The client stopped playback 50 ms in: one character of twelve,
    // inside the first word, so not a word of it was heard (§7.3).
    send(&mut ws, truncate(&item, 50, "stop")).await;
    events.extend(events_until(&mut ws, "response.done").await);

    let t = types(&events);
    let truncated = t
        .iter()
        .position(|x| *x == "conversation.item.truncated")
        .unwrap_or_else(|| panic!("{t:?}"));
    // The audio part closes first, saying what was heard (WP3 review m4).
    assert_eq!(
        t[truncated..],
        [
            "conversation.item.truncated",
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done"
        ]
    );
    assert_eq!(events[truncated + 2]["transcript"], "");
    assert_eq!(events[truncated + 3]["part"]["transcript"], "");
    let closed = &events[truncated + 4]["item"];
    assert_eq!(closed["status"], "incomplete");
    assert_eq!(closed["content"][0]["transcript"], "");
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "client_cancelled");

    // Nothing more of it leaves.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    let next = next_event(&mut ws).await;
    assert_eq!(next["type"], "conversation.item.retrieved", "{next}");
    assert_eq!(next["item"]["content"][0]["transcript"], "");
}
