//! What a barge-in does to the responses around it (realtime design §4.3,
//! §6.4, owner's decisions Q3): a `response.create` queued behind the cut
//! answer, or sent while the user still talks, is held and starts once,
//! after the turn; a tool call the client already ran is never marked
//! cancelled; `interrupt_response: false` leaves the cut to the client
//! (`@openai/agents` sends `response.cancel` itself); `create_response:
//! false` commits the turn and answers nothing by itself; a create sent
//! while a cut nobody heard is carried joins it, tools or none. Real time,
//! as in `realtime_barge`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{fixture, Asr};
use crate::support::realtime_fakes::{
    captured_client_frames, next_event, open, send, types, Step, Turn,
};
use crate::support::realtime_mic::{
    barge_gateway, barge_session, knobs, live_mic, Ear, Mic, Player,
};
use crate::support::realtime_tts::{wav, Tts};

const QUESTION: &str = "Where is the nearest station?";

fn long_answer() -> Tts {
    Tts::Wav(wav(&fixture("en_two_sentences_pause.wav"), 24_000))
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

/// Until the first audio delta, playing the client.
async fn until_audio(ear: &mut Ear, player: &mut Player) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        let audio = ev["type"] == "response.output_audio.delta";
        out.push(ev);
        if audio {
            return out;
        }
    }
}

/// Half a second into the answer, the user asks the question; every event
/// up to `speech_started`.
async fn barge_in(mic: &Mic, ear: &mut Ear) -> Vec<Value> {
    tokio::time::sleep(Duration::from_millis(500)).await;
    mic.say(fixture("en_complete_short.wav")).await;
    ear.until("input_audio_buffer.speech_started").await
}

/// A user text item.
fn hallo() -> Value {
    json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user",
           "content": [{"type": "input_text", "text": "Hallo"}]}})
}

/// A create queued while the answer plays, and one sent right after the
/// cancel's `response.done` (the SDK's follow-up), both wait for the turn.
async fn held_create(queued: bool) {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Let me look that up for you."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, _) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    if queued {
        // Generation is long over: the answer only plays. Queued, not
        // refused.
        tokio::time::sleep(Duration::from_millis(200)).await;
        mic.send(json!({"type": "response.create", "event_id": "follow"}));
    }
    barge_in(&mic, &mut ear).await;
    let cut = ear.until("response.done").await;
    assert_eq!(
        cut.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    if !queued {
        mic.send(json!({"type": "response.create", "event_id": "follow"}));
    }
    // Nothing starts before the turn is committed and transcribed — then
    // exactly one response, answering it.
    let rest = ear.until("response.done").await;
    let t = types(&rest);
    assert!(!t.contains(&"error"), "{t:?}");
    let created = t.iter().position(|t| *t == "response.created").unwrap();
    let transcribed = t
        .iter()
        .position(|t| *t == "conversation.item.input_audio_transcription.completed")
        .unwrap();
    assert!(transcribed < created, "{t:?}");
    assert_eq!(count(&rest, "response.created"), 1);
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    let later = ear.quiet_for(Duration::from_millis(600)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(chat.seen.chat_count(), 2);
    let last = chat.seen.chat(1)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], QUESTION);
}

#[tokio::test]
async fn a_create_queued_behind_the_cut_answer_waits_for_the_turn() {
    held_create(true).await;
}

#[tokio::test]
async fn a_create_sent_after_the_cut_while_the_user_talks_waits_for_the_turn() {
    held_create(false).await;
}

#[tokio::test]
async fn a_barge_in_on_a_tool_preamble_keeps_the_call_and_answers_once_after_the_turn() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::Stream(vec![
        Step::Text("Let me check, "),
        Step::CallStart {
            index: 0,
            id: Some("call_t1"),
            name: "get_time",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"tz":"Europe/Berlin"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, _) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    // The call closes `completed` at the end of generation; the client runs
    // its tool and answers at once, while the preamble still plays.
    let mut player = Player::default();
    let mut events = Vec::new();
    loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        let call_done =
            ev["type"] == "response.output_item.done" && ev["item"]["type"] == "function_call";
        events.push(ev);
        if call_done {
            break;
        }
    }
    assert!(player.first.is_some(), "the preamble is playing");
    mic.send(
        json!({"type": "conversation.item.create", "item": {"type": "function_call_output",
                    "call_id": "call_t1", "output": "{\"time\":\"12:00\"}"}}),
    );
    mic.send(json!({"type": "response.create", "event_id": "after_tool"}));
    barge_in(&mic, &mut ear).await;
    let cut = ear.until("response.done").await;
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    assert_eq!(done["output"][0]["type"], "message");
    assert_eq!(done["output"][0]["status"], "incomplete");
    assert_eq!(done["output"][1]["call_id"], "call_t1");
    assert_eq!(
        done["output"][1]["status"], "completed",
        "the client ran it"
    );
    // One response after the turn: the tool's result and the question.
    let rest = ear.until("response.done").await;
    assert_eq!(count(&rest, "response.created"), 1, "{:?}", types(&rest));
    assert!(!types(&rest).contains(&"error"));
    let later = ear.quiet_for(Duration::from_millis(600)).await;
    assert_eq!(count(&later, "response.created"), 0);
    let messages = chat.seen.chat(1)["messages"].as_array().unwrap().clone();
    assert!(messages.iter().any(|m| m["role"] == "tool"), "{messages:?}");
    assert_eq!(messages.last().unwrap()["content"], QUESTION);
    assert_eq!(chat.seen.chat_count(), 2);
}

#[tokio::test]
async fn with_interrupt_response_off_agents_js_cancels_itself() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    // agents_js_barge_interrupt_false.json: the stub echoed
    // interrupt_response false; here the client asks for it.
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    next_event(&mut ws).await;
    let mut frames = captured_client_frames("agents_js_barge_interrupt_false.json").into_iter();
    let mut first = frames.next().unwrap();
    first["session"]["model"] = json!("chatty");
    first["session"]["lmgw"] = knobs();
    first["session"]["audio"]["input"]["turn_detection"] =
        json!({"type": "semantic_vad", "interrupt_response": false});
    send(&mut ws, first).await;
    let updated = next_event(&mut ws).await;
    assert_eq!(
        updated["session"]["audio"]["input"]["turn_detection"]["interrupt_response"],
        false
    );
    send(&mut ws, frames.next().unwrap()).await;
    next_event(&mut ws).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(frames.next().unwrap());
    ear.until("conversation.item.done").await;
    mic.send(frames.next().unwrap());
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    let iid = player.item.clone().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    mic.say(fixture("en_complete_short.wav")).await;
    let (at, _) = loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        if ev["type"] == "input_audio_buffer.speech_started" {
            break (at, ev);
        }
    };
    // The server does not cut it; the SDK does — cancel, truncate, retrieve.
    let truncate = player.truncate(at).expect("still playing");
    mic.send(json!({"type": "response.cancel"}));
    mic.send(truncate);
    mic.send(json!({"type": "conversation.item.retrieve", "item_id": iid}));
    let cut = ear.until("response.done").await;
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "client_cancelled");
    let rest = ear.until("response.done").await;
    let t = types(&rest);
    assert!(t.contains(&"conversation.item.truncated"), "{t:?}");
    assert!(!t.contains(&"error"), "{t:?}");
    // The turn, on the post-interrupt window, is answered after it.
    assert_eq!(count(&rest, "response.created"), 1);
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    assert_eq!(asr.seen.count(), 1);
}

#[tokio::test]
async fn with_create_response_off_the_turn_commits_and_waits_for_the_client() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, _) = barge_session(
        &addr,
        json!({"type": "server_vad", "create_response": false}),
        json!({}),
    )
    .await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    barge_in(&mic, &mut ear).await;
    let cut = ear.until("response.done").await;
    assert_eq!(
        cut.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    let turn = ear.until("conversation.item.done").await;
    let t = types(&turn);
    assert!(t.contains(&"input_audio_buffer.committed"), "{t:?}");
    let later = ear.quiet_for(Duration::from_millis(800)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    // The client answers when it wants to.
    mic.send(json!({"type": "response.create"}));
    let answer = ear.until("response.done").await;
    assert_eq!(answer.last().unwrap()["response"]["status"], "completed");
    let last = chat.seen.chat(1)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], QUESTION);
}

/// The absorb rule (realtime-server-tools §2.5) belongs to the re-carry, not
/// to the tools (final review #7): a client's response cut before anything
/// of it was heard has its `response.create` held again for after the turn,
/// and a bare create the client sends meanwhile joins it — no tool, no
/// `error`, one response after the turn. The held create has not rendered
/// yet, so it answers the newer request too.
#[tokio::test]
async fn a_create_sent_while_an_unheard_cut_s_create_is_carried_joins_it() {
    let (_s, addr, chat, _tts, asr) = barge_gateway().await;
    let never = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::Wait(never.clone()),
        Step::Text("Too late."),
        Step::Finish("stop"),
    ]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, _) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create", "event_id": "first"}));
    ear.until("response.created").await;

    mic.say(fixture("en_complete_short.wav")).await;
    ear.until("input_audio_buffer.speech_started").await;
    let cut = ear.until("response.done").await;
    assert_eq!(
        cut.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    mic.send(json!({"type": "response.create", "event_id": "second"}));
    let rest = ear.until("response.done").await;
    let t = types(&rest);
    assert!(!t.contains(&"error"), "{t:?}");
    assert_eq!(count(&rest, "response.created"), 1, "{t:?}");
    let created = t.iter().position(|t| *t == "response.created").unwrap();
    let transcribed = t
        .iter()
        .position(|t| *t == "conversation.item.input_audio_transcription.completed")
        .unwrap();
    assert!(transcribed < created, "{t:?}");
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    let later = ear.quiet_for(Duration::from_millis(600)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(count(&later, "error"), 0, "{:?}", types(&later));
    assert_eq!(chat.seen.chat_count(), 2);
    // The cut said nothing: the two user turns are adjacent, rendered as one
    // (§7.2), and the one response answers both.
    let last = chat.seen.chat(1)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], format!("Hallo\n{QUESTION}"));
}
