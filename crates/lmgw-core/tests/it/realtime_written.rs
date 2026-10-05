//! The model's history is what it wrote, up to what was heard (realtime
//! design §7.2, §7.3; B2 review 2): fenced code the voice leaves out is not
//! in the spoken transcript, but the next response's request still carries
//! it — "run the second command again" needs it — while a truncate cuts
//! that text where the listener stopped hearing.

use serde_json::{json, Value};

use crate::support::realtime_fakes::{
    events_until, next_event, send, user_text, ChatFake, Turn, Ws,
};
use crate::support::realtime_tts::{speech, speech_gateway, spoken_session, wav};

/// A spoken answer of two clauses, one second of audio each, with a code
/// block between them and one that ends it.
const ANSWER: &[&str] = &[
    "Hier der Befehl:\n```sh\nls -la\n```\n",
    "Führ ihn aus.\n```sh\nrm -rf build\n```",
];

/// Ask for the spoken answer; returns its item id.
async fn spoken_item(ws: &mut Ws) -> String {
    send(ws, user_text("Wie räume ich auf?")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    let events = events_until(ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    events[1]["item"]["id"].as_str().unwrap().to_string()
}

async fn transcript(ws: &mut Ws, item: &str) -> Value {
    send(
        ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    let got = next_event(ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved");
    got["item"]["content"][0]["transcript"].clone()
}

/// A text response after `next`: the assistant message the model was sent
/// for the spoken item.
async fn rendered(ws: &mut Ws, chat: &ChatFake) -> Value {
    chat.push(Turn::text(&["Gut."]));
    send(ws, user_text("Nochmal den zweiten Befehl")).await;
    events_until(ws, "conversation.item.done").await;
    send(
        ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    events_until(ws, "response.done").await;
    let messages = chat.seen.chat(1)["messages"].clone();
    assert_eq!(messages[2]["role"], "assistant", "{messages}");
    messages[2]["content"].clone()
}

#[tokio::test]
async fn the_model_is_sent_the_code_it_wrote_and_the_client_what_was_said() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(ANSWER));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    let item = spoken_item(&mut ws).await;

    assert_eq!(
        transcript(&mut ws, &item).await,
        "Hier der Befehl: Führ ihn aus."
    );
    assert_eq!(
        rendered(&mut ws, &chat).await,
        "Hier der Befehl:\n```sh\nls -la\n```\nFühr ihn aus.\n```sh\nrm -rf build\n```"
    );
}

#[tokio::test]
async fn a_truncate_cuts_the_written_text_where_the_listener_stopped() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(ANSWER));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    let item = spoken_item(&mut ws).await;

    // Half of the second clause heard ("Führ i"): the code before it is
    // the model's, the clause to the last word heard whole, the closing
    // block is not.
    send(
        &mut ws,
        json!({"type": "conversation.item.truncate", "item_id": item,
               "content_index": 0, "audio_end_ms": 1500}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.truncated"
    );
    assert_eq!(transcript(&mut ws, &item).await, "Hier der Befehl: Führ");
    assert_eq!(
        rendered(&mut ws, &chat).await,
        "Hier der Befehl:\n```sh\nls -la\n```\nFühr"
    );
}

/// Tables are not said (chat-voice design §6.2, end to end): a stock session
/// leaves the rows out of its transcript and announces nothing, the model is
/// sent them, and prose that opens with a pipe is said (review m1).
#[tokio::test]
async fn a_table_is_not_said_and_the_model_keeps_it() {
    const TABLE: &[&str] = &[
        "Vergleich:\n| a | b |\n|---|---|\n",
        "| 1 | 2 |\nAlso:\n|x| ist der Betrag.",
    ];
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(TABLE));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    let item = spoken_item(&mut ws).await;

    assert_eq!(
        transcript(&mut ws, &item).await,
        "Vergleich: Also: |x| ist der Betrag."
    );
    assert_eq!(rendered(&mut ws, &chat).await, TABLE.concat());
}
