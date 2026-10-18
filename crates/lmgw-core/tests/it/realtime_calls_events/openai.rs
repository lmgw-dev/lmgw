//! OpenAI's stop for a stock client (calls design §6.2): `response.cancel`
//! then `output_audio_buffer.clear` — the cancelled `response.done`, one
//! `cleared`, nothing for the `clear` after the cancel — and `clear` alone
//! while an answer plays; `clear` with nothing playing sends nothing, before
//! an answer and while one thinks. A WebSocket answers `clear` with
//! `not_implemented_yet`, worded for what it is.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Notify;

use super::{manual, say, tone, types, unbound, update, Call};
use crate::realtime_calls::calls_world;
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::{next_event, open, send, Turn};
use crate::support::realtime_tts::{wav, Tts};

/// A stock call with audio out and push-to-talk, whose next answer is three
/// seconds of tone.
async fn speaking() -> (World, Call) {
    let w = calls_world(|s| s.realtime.tts_alias = "speak".into()).await;
    w.tts.push(Tts::Wav(wav(&tone(3_000), 24_000)));
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let mut call = unbound(&w).await;
    update(&mut call, manual()).await;
    (w, call)
}

/// Its answer asked for and playing: the answer's id.
async fn playing(call: &mut Call) -> serde_json::Value {
    say(call).await;
    let started = call.next_of("output_audio_buffer.started").await;
    call.run_for(std::time::Duration::from_millis(200)).await;
    started["response_id"].clone()
}

#[tokio::test]
async fn openais_stop_form_cuts_once_and_its_clear_after_the_cancel_says_nothing() {
    let (_w, mut call) = speaking().await;
    let rid = playing(&mut call).await;
    call.send(json!({"type": "response.cancel"}));
    call.send(json!({"type": "output_audio_buffer.clear"}));
    let done = call.next_of("response.done").await;
    assert_eq!(done["response"]["id"], rid);
    assert_eq!(done["response"]["status"], "cancelled");
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "client_cancelled"
    );
    let after = call.probe().await;
    assert_eq!(
        types(&after),
        ["output_audio_buffer.cleared", "session.updated"],
        "one cleared, nothing for the clear: {after:?}"
    );
    assert_eq!(after[0]["response_id"], rid);
}

#[tokio::test]
async fn a_clear_alone_while_an_answer_plays_cancels_it_as_a_cancel_does() {
    let (_w, mut call) = speaking().await;
    let rid = playing(&mut call).await;
    call.send(json!({"type": "output_audio_buffer.clear"}));
    let done = call.next_of("response.done").await;
    assert_eq!(done["response"]["status"], "cancelled");
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "client_cancelled"
    );
    let after = call.probe().await;
    assert_eq!(
        types(&after),
        ["output_audio_buffer.cleared", "session.updated"],
        "{after:?}"
    );
    assert_eq!(after[0]["response_id"], rid);
}

/// Nothing plays before the first answer, nor while one thinks (its model
/// call held): a `clear` then sends nothing and cancels nothing, and the
/// answer that comes plays to its end.
#[tokio::test]
async fn a_clear_with_nothing_playing_sends_nothing() {
    let w = calls_world(|s| s.realtime.tts_alias = "speak".into()).await;
    let held = Arc::new(Notify::new());
    w.chat.push(Turn::Held(
        held.clone(),
        Box::new(Turn::text(&["Trains leave every hour."])),
    ));
    let mut call = unbound(&w).await;
    update(&mut call, manual()).await;
    call.send(json!({"type": "output_audio_buffer.clear"}));
    assert_eq!(types(&call.probe().await), ["session.updated"]);
    say(&mut call).await;
    call.next_of("response.created").await;
    call.send(json!({"type": "output_audio_buffer.clear"}));
    let after = call.probe().await;
    assert!(
        !types(&after)
            .iter()
            .any(|t| t.starts_with("output_audio_buffer.") || *t == "response.done"),
        "{after:?}"
    );
    held.notify_one();
    let done = call.next_of("response.done").await;
    assert_eq!(done["response"]["status"], "completed", "{done}");
    let after = call.probe().await;
    assert_eq!(types(&after)[0], "output_audio_buffer.stopped", "{after:?}");
}

/// On a WebSocket the client owns its playback buffer: `clear` is answered
/// `not_implemented_yet`, naming the calls it applies to and what a
/// WebSocket client does instead.
#[tokio::test]
async fn a_websocket_answers_clear_naming_the_calls_it_applies_to() {
    let w = calls_world(|_| {}).await;
    let mut ws = open(&w.addr(), "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "output_audio_buffer.clear", "event_id": "evt_c"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "not_implemented_yet", "{e}");
    assert_eq!(e["error"]["event_id"], "evt_c");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("applies to WebRTC calls (POST /v1/realtime/calls)")
            && message.contains("conversation.item.truncate"),
        "{message}"
    );
}
