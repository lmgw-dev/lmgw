//! Responses that wait on turns (realtime design §4.2 step 4, §4.3): the
//! automatic response owed to committed turns is never stranded — not by a
//! noise turn after it, a failed one, one the client cleared, nor by a
//! client response that was rendered before it — a response whose turns all
//! failed to transcribe fails rather than answers nothing, and a response
//! keeps the config it was created with while it waits.

use std::sync::Arc;

use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{
    add_asr_alias, asr_fake, fixture, silence, stream, Asr, AsrFake, ASR_ALIAS,
};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, types, user_text, ChatFake, Step,
    Turn, Ws,
};

const QUESTION: &str = "Where is the nearest station?";

/// The chat fake (`chatty`), the ASR fake (`hear`) and
/// `realtime.asr_alias` set to it.
async fn voice_gateway() -> (SharedState, String, ChatFake, AsrFake) {
    let chat = chat_fake().await;
    let asr = asr_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
    })
    .await;
    add_asr_alias(&state, &asr).await;
    (state, addr, chat, asr)
}

/// A text-output session on `chatty` with `session` merged in, past its
/// `session.updated`.
async fn voice_session(addr: &str, session: Value) -> Ws {
    let mut ws = open(addr, "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    let mut update = json!({"type": "realtime", "output_modalities": ["text"],
                            "audio": {"input": {"transcription": {}}}});
    for (k, v) in session.as_object().unwrap() {
        update[k] = v.clone();
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": update}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    ws
}

/// The last user message of chat request `n`.
fn last_user(chat: &ChatFake, n: usize) -> Value {
    chat.seen.chat(n)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"]
        .clone()
}

/// Two turns 700 ms apart, the first held at its transcript until the second
/// has committed, whose transcript is `second` — the cough after a question.
async fn question_then(second: Asr) {
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), "I am tired."));
    asr.push(second);
    chat.push(Turn::text(&["Rest."]));
    let mut ws = voice_session(&addr, json!({})).await;
    stream(&mut ws, &fixture("en_two_sentences_pause.wav")).await;
    events_until(&mut ws, "input_audio_buffer.committed").await;
    events_until(&mut ws, "input_audio_buffer.committed").await;
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    let t = types(&events);
    assert_eq!(t.iter().filter(|t| **t == "response.created").count(), 1);
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(chat.seen.chat_count(), 1);
    assert_eq!(last_user(&chat, 0), "I am tired.");
}

#[tokio::test]
async fn a_question_is_answered_though_the_turn_after_it_was_noise() {
    question_then(Asr::Text("")).await;
}

#[tokio::test]
async fn a_question_is_answered_though_the_turn_after_it_failed_to_transcribe() {
    question_then(Asr::Status(
        500,
        json!({"error": {"message": "engine fell over"}}),
    ))
    .await;
}

#[tokio::test]
async fn a_question_is_answered_though_the_turn_after_it_was_cleared() {
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), "I am tired."));
    chat.push(Turn::text(&["Rest."]));
    let mut ws = voice_session(&addr, json!({})).await;
    // The first sentence, and the start of the second (1798 ms on).
    let pcm = fixture("en_two_sentences_pause.wav");
    stream(&mut ws, &pcm[..2400 * 24]).await;
    events_until(&mut ws, "input_audio_buffer.committed").await;
    events_until(&mut ws, "input_audio_buffer.speech_started").await;
    // The client drops the second turn before it ends.
    send(&mut ws, json!({"type": "input_audio_buffer.clear"})).await;
    events_until(&mut ws, "input_audio_buffer.cleared").await;
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert!(types(&events).contains(&"response.created"), "{events:?}");
    assert_eq!(last_user(&chat, 0), "I am tired.");
}

/// Turns that do not cut the response they start in (§6.4,
/// `interrupt_response: false`): what keeps a client response running while
/// the user speaks, so a turn can commit under it.
fn beside() -> Value {
    json!({"audio": {"input": {"transcription": {},
           "turn_detection": {"type": "server_vad", "interrupt_response": false}}}})
}

#[tokio::test]
async fn a_turn_that_commits_while_a_client_response_runs_is_answered_after_it() {
    let (_s, addr, chat, asr) = voice_gateway().await;
    asr.push(Asr::Text(QUESTION));
    let release = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::Text("Hallo!"),
        Step::Wait(release.clone()),
        Step::Finish("stop"),
        Step::Usage(3, 1),
    ]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    let mut ws = voice_session(&addr, beside()).await;
    send(&mut ws, user_text("Hallo")).await;
    events_until(&mut ws, "conversation.item.done").await;
    // The client starts a response of its own, rendered without the turn…
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_text.delta").await;
    // …the user starts talking, which does not cut it…
    let pcm = fixture("en_complete_short.wav");
    stream(&mut ws, &pcm[..1000 * 24]).await;
    events_until(&mut ws, "input_audio_buffer.speech_started").await;
    // …and the turn ends while it runs — on the post-interrupt window
    // (§6.5): it started while a response was in progress.
    stream(&mut ws, &pcm[1000 * 24..]).await;
    stream(&mut ws, &silence(1000)).await;
    events_until(
        &mut ws,
        "conversation.item.input_audio_transcription.completed",
    )
    .await;
    release.notify_one();
    let first = events_until(&mut ws, "response.done").await;
    assert_eq!(first.last().unwrap()["response"]["status"], "completed");
    let owed = events_until(&mut ws, "response.done").await;
    assert_eq!(owed[0]["type"], "response.created", "{owed:?}");
    assert_eq!(owed.last().unwrap()["response"]["status"], "completed");
    assert_eq!(chat.seen.chat_count(), 2);
    assert_eq!(last_user(&chat, 1), QUESTION);
}

#[tokio::test]
async fn a_client_response_sent_while_the_user_speaks_waits_for_the_turn_and_answers_it() {
    // Owner's decision Q3: held, not started — and not refused — while the
    // turn is open; one response after it, rendering it.
    let (_s, addr, chat, asr) = voice_gateway().await;
    asr.push(Asr::Text(QUESTION));
    chat.push(Turn::text(&["Two ", "blocks."]));
    let mut ws = voice_session(&addr, json!({})).await;
    send(&mut ws, user_text("Hallo")).await;
    events_until(&mut ws, "conversation.item.done").await;
    let pcm = fixture("en_complete_short.wav");
    stream(&mut ws, &pcm[..1000 * 24]).await;
    events_until(&mut ws, "input_audio_buffer.speech_started").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "mine"}),
    )
    .await;
    stream(&mut ws, &pcm[1000 * 24..]).await;
    let events = events_until(&mut ws, "response.done").await;
    let t = types(&events);
    let created = t.iter().position(|t| *t == "response.created").unwrap();
    let transcribed = t
        .iter()
        .position(|t| *t == "conversation.item.input_audio_transcription.completed")
        .unwrap();
    assert!(transcribed < created, "{t:?}");
    assert_eq!(t.iter().filter(|t| **t == "response.created").count(), 1);
    assert!(!t.contains(&"error"), "{t:?}");
    assert_eq!(chat.seen.chat_count(), 1);
    assert_eq!(last_user(&chat, 0), format!("Hallo\n{QUESTION}"));
}

#[tokio::test]
async fn a_response_whose_turn_failed_to_transcribe_fails_with_transcription_failed() {
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldStatus(
        release.clone(),
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    let mut ws = voice_session(
        &addr,
        json!({"audio": {"input": {"transcription": {}, "turn_detection": null}}}),
    )
    .await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    events_until(&mut ws, "conversation.item.added").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "r1"}),
    )
    .await;
    events_until(&mut ws, "response.created").await;
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        types(&events),
        [
            "conversation.item.input_audio_transcription.failed",
            "conversation.item.done",
            "error",
            "response.done",
        ]
    );
    assert_eq!(events[2]["error"]["code"], "transcription_failed");
    assert_eq!(events[2]["error"]["event_id"], "r1");
    let r = &events[3]["response"];
    assert_eq!(r["status"], "failed");
    assert_eq!(r["status_details"]["error"]["code"], "transcription_failed");
    assert_eq!(chat.seen.chat_count(), 0, "nothing was asked of the model");
}

#[tokio::test]
async fn a_session_update_while_a_response_waits_applies_to_the_next_response() {
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), QUESTION));
    let tool = json!({"type": "function", "name": "g",
                      "parameters": {"type": "object", "properties": {}}});
    let mut ws = voice_session(
        &addr,
        json!({"audio": {"input": {"transcription": {}, "turn_detection": null}},
               "tools": [tool], "parallel_tool_calls": true}),
    )
    .await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    events_until(&mut ws, "conversation.item.added").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.created").await;
    // While it waits for the transcript, the session changes.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "reasoning": {"effort": "high"}, "parallel_tool_calls": false}}),
    )
    .await;
    events_until(&mut ws, "session.updated").await;
    release.notify_one();
    events_until(&mut ws, "response.done").await;
    let body = chat.seen.chat(0);
    assert_eq!(body["reasoning_effort"], "none", "{body}");
    assert_eq!(body["parallel_tool_calls"], true, "{body}");

    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.done").await;
    let body = chat.seen.chat(1);
    assert_eq!(body["reasoning_effort"], "high", "{body}");
    assert_eq!(body["parallel_tool_calls"], false, "{body}");
}

#[tokio::test]
async fn a_client_response_waiting_on_a_failed_turn_answers_the_owed_turn_after_it() {
    // WP1c review #1: turn A commits and is still being transcribed; the
    // client asks for a response — it waits for A — the user goes on (B
    // starts) and B commits while it waits. A's transcription fails, B's has
    // words: the waiting response answers B, rather than failing on A and
    // leaving B unanswered.
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldStatus(
        release.clone(),
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    asr.push(Asr::Text(QUESTION));
    chat.push(Turn::text(&["Two ", "blocks."]));
    // B must not cut the waiting response (§6.4), and the client asks
    // before B starts — a create while the user speaks waits for the turn
    // (owner's decision Q3).
    let mut ws = voice_session(&addr, beside()).await;
    let pcm = fixture("en_two_sentences_pause.wav");
    stream(&mut ws, &pcm[..1700 * 24]).await;
    events_until(&mut ws, "input_audio_buffer.committed").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "r1"}),
    )
    .await;
    events_until(&mut ws, "response.created").await;
    stream(&mut ws, &pcm[1700 * 24..]).await;
    // B started while a response was in progress: it ends on the
    // post-interrupt window (§6.5).
    stream(&mut ws, &silence(1000)).await;
    events_until(&mut ws, "input_audio_buffer.speech_started").await;
    events_until(&mut ws, "input_audio_buffer.committed").await;
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert!(
        !events.iter().any(|e| e["type"] == "error"),
        "nothing failed: {events:?}"
    );
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(chat.seen.chat_count(), 1);
    assert_eq!(last_user(&chat, 0), QUESTION);
    // The owed response was settled by that one: nothing else starts.
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": "nope", "event_id": "s"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["event_id"], "s", "{e}");
    assert_eq!(chat.seen.chat_count(), 1);
}

#[tokio::test]
async fn a_response_to_a_tool_output_does_not_fail_on_an_older_failed_turn() {
    let (_s, addr, chat, asr) = voice_gateway().await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldStatus(
        release.clone(),
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    chat.push(Turn::text(&["It is sunny."]));
    let mut ws = voice_session(
        &addr,
        json!({"audio": {"input": {"transcription": {}, "turn_detection": null}}}),
    )
    .await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    events_until(&mut ws, "conversation.item.added").await;
    // While that turn is with the engine, the client's tool finishes.
    send(
        &mut ws,
        json!({"type": "conversation.item.create",
               "item": {"type": "function_call", "call_id": "call_w", "name": "weather",
                        "arguments": "{}"}}),
    )
    .await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "conversation.item.create",
               "item": {"type": "function_call_output", "call_id": "call_w",
                        "output": "{\"sky\": \"clear\"}"}}),
    )
    .await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "after_tool"}),
    )
    .await;
    events_until(&mut ws, "response.created").await;
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(chat.seen.chat_count(), 1, "the tool's output was answered");
}
