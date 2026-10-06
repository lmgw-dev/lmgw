//! Speech while a spoken response's server-side calls run
//! (realtime-server-tools design §2.5; WP4), in real time as in
//! `realtime_barge`: speech while only a call runs and nothing plays is a
//! turn of its own — committed, the response finishing with the call's
//! result, the client's follow-up held until the turn ends and answering
//! both — also under half duplex once the preamble played out; a barge-in
//! while the preamble still plays cancels as before and abandons the call;
//! the follow-up after a barge-in on a response carried again is absorbed
//! into it; a call that went to its server is abandoned by a cancel even
//! while its report still waits behind a clause; and a follow-up after the
//! turn's own response started is refused in words that say that response
//! answers with the tool results.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::mcp_stub::{held_call_stub, register};
use crate::support::realtime_audio::{fixture, Asr};
use crate::support::realtime_fakes::{events_until, send, types, user_text, Step, Turn, Ws};
use crate::support::realtime_mcp::{
    assert_every_mcp_call, calls, listed, rows_at_least, set_tools, shape,
};
use crate::support::realtime_mic::{barge_gateway, barge_session, live_mic, Ear, Player};
use crate::support::realtime_tts::{speech, speech_gateway, spoken_session, wav, Tts};

const QUESTION: &str = "Where is the nearest station?";
/// `agent::ABANDONED_CALL`.
const ABANDONED: &str =
    "abandoned when the run was cancelled; it had already been sent, so it may still have run";
/// `agent::UNMADE_CALL`.
const UNMADE: &str = "not run: the turn ended before this call was made";

fn label_a() -> Value {
    json!([{"type": "mcp", "server_label": "a", "require_approval": "never"}])
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

/// A user text item.
fn hallo() -> Value {
    json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user",
           "content": [{"type": "input_text", "text": "Hallo"}]}})
}

/// A barge-in session (`realtime_mic`'s knobs, `lmgw` merged in) with the
/// label `a` listed.
async fn barge_tools_session(addr: &str, lmgw: Value) -> Ws {
    let (mut ws, _) = barge_session(addr, json!({"type": "server_vad"}), lmgw).await;
    set_tools(&mut ws, label_a()).await;
    listed(&mut ws).await;
    ws
}

/// The `mcp_call` item `events` closed last.
fn closed_call(events: &[Value]) -> Value {
    events
        .iter()
        .rev()
        .find(|e| e["type"] == "conversation.item.done" && e["item"]["type"] == "mcp_call")
        .map(|e| e["item"].clone())
        .unwrap_or_else(|| panic!("no mcp_call closed in {events:#?}"))
}

/// Read events, playing the client, until the first output audio delta.
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

/// After the tool response's `response.done`: the client's follow-up (the
/// SDK sends it on the call's `conversation.item.done`) arrives while the
/// user still talks, is held, and starts once, after the turn is committed
/// and transcribed — answering the question with the call's result.
async fn follow_up_answers_once(mic_send: impl Fn(Value), ear: &mut Ear) -> Vec<Value> {
    mic_send(json!({"type": "response.create", "event_id": "follow"}));
    let rest = ear.until("response.done").await;
    let t = types(&rest);
    assert!(!t.contains(&"error"), "{t:?}");
    assert_eq!(count(&rest, "response.created"), 1, "{t:?}");
    let created = t.iter().position(|t| *t == "response.created").unwrap();
    let transcribed = t
        .iter()
        .position(|t| *t == "conversation.item.input_audio_transcription.completed")
        .expect("the turn was committed and transcribed");
    assert!(transcribed < created, "{t:?}");
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    let later = ear.quiet_for(Duration::from_millis(600)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(count(&later, "error"), 0, "{:?}", types(&later));
    rest
}

/// A model turn that calls `a__echo` with `text`, after `preamble` when
/// there is one.
fn call_turn(preamble: Option<&'static str>, id: &'static str, args: &'static str) -> Turn {
    let mut steps = Vec::new();
    if let Some(p) = preamble {
        steps.push(Step::Text(p));
    }
    steps.extend([
        Step::CallStart {
            index: 0,
            id: Some(id),
            name: "a__echo",
        },
        Step::CallArgs { index: 0, args },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]);
    Turn::Stream(steps)
}

/// §2.5: the model only calls a tool — nothing is said, nothing plays — and
/// the user speaks while it runs. That is a turn, not a barge-in: the
/// response is not cancelled and finishes with the call's result, and the
/// one response after the turn answers the question and reads the result.
#[tokio::test]
async fn speech_while_a_silent_call_runs_is_a_turn_answered_once_with_the_result() {
    let (state, addr, chat, _tts, asr) = barge_gateway().await;
    let (stub, gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    chat.push(calls(
        &[(0, "call_v1", "a__echo", r#"{"text":"slow"}"#)],
        "tool_calls",
    ));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let ws = barge_tools_session(&addr, json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    ear.until("response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;

    mic.say(fixture("en_complete_short.wav")).await;
    let started = ear.until("input_audio_buffer.speech_started").await;
    assert_eq!(count(&started, "response.done"), 0, "{:?}", types(&started));
    gate.notify_one();
    let first = ear.until("response.done").await;
    assert!(!types(&first).contains(&"error"), "{:?}", types(&first));
    let done = &first.last().unwrap()["response"];
    assert_eq!(done["status"], "completed", "not cancelled: {done}");
    assert_eq!(done["output"][0]["output"], "echo: slow");
    assert_every_mcp_call(&first);

    follow_up_answers_once(|v| mic.send(v), &mut ear).await;
    assert_eq!(chat.seen.chat_count(), 2);
    let sent = shape(&chat.seen.chat(1));
    assert!(
        sent.contains(&("tool".to_string(), "result:call_v1:echo: slow".to_string())),
        "{sent:?}"
    );
    assert_eq!(
        sent.last().unwrap(),
        &("user".to_string(), QUESTION.to_string())
    );
}

/// Half duplex does not listen while an answer plays — but a preamble that
/// has played out is not playing, however long its call runs: speech then
/// is heard, and is a turn (§2.5). It used to count as playing until the
/// call was done, and half duplex heard nothing.
#[tokio::test]
async fn half_duplex_listens_once_the_preamble_played_out_while_the_call_runs() {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    let (stub, gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    // 300 ms of preamble.
    tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    chat.push(call_turn(
        Some("One moment. "),
        "call_p1",
        r#"{"text":"later"}"#,
    ));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let ws = barge_tools_session(&addr, json!({"half_duplex": true})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    ear.until("response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;
    // Past the preamble's playback and the window's margin (the echo tail,
    // 250 ms, and a loopback round trip).
    let first = player.first.expect("the preamble played");
    tokio::time::sleep_until(first + Duration::from_millis(1200)).await;

    mic.say(fixture("en_complete_short.wav")).await;
    ear.until("input_audio_buffer.speech_started").await;
    gate.notify_one();
    let done_events = ear.until("response.done").await;
    let done = &done_events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["output"][0]["content"][0]["transcript"], "One moment.");
    assert_eq!(done["output"][1]["output"], "echo: later");

    follow_up_answers_once(|v| mic.send(v), &mut ear).await;
    let sent = shape(&chat.seen.chat(1));
    assert_eq!(
        sent.last().unwrap(),
        &("user".to_string(), QUESTION.to_string())
    );
}

/// A barge-in while the preamble still plays cancels the response as
/// before (§2.5), and the call it ran is abandoned: its item says so, and
/// its row is `canceled`. The client's follow-up waits for the turn; one
/// response answers it, and reads the abandoned call.
#[tokio::test]
async fn a_barge_in_while_the_preamble_plays_cancels_and_abandons_the_running_call() {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    let (stub, _gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    tts.push(Tts::Wav(wav(
        &fixture("en_two_sentences_pause.wav"),
        24_000,
    )));
    chat.push(call_turn(
        Some("Let me look that up for you. "),
        "call_b1",
        r#"{"text":"cut"}"#,
    ));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let ws = barge_tools_session(&addr, json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    stub.wait_calls(1).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    mic.say(fixture("en_complete_short.wav")).await;
    ear.until("input_audio_buffer.speech_started").await;
    let cut = ear.until("response.done").await;
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    let call = closed_call(&cut);
    assert_eq!(call["error"]["message"], ABANDONED, "{call}");
    assert!(types(&cut).contains(&"response.mcp_call.failed"));
    assert_eq!(done["output"][1], call);

    follow_up_answers_once(|v| mic.send(v), &mut ear).await;
    let sent = shape(&chat.seen.chat(1));
    assert!(
        sent.contains(&("tool".to_string(), format!("result:call_b1:{ABANDONED}"))),
        "{sent:?}"
    );
    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(tool_rows[0].error_kind.as_deref(), Some("canceled"));
}

/// §2.5's absorbed follow-up: a client's response is cut by speech before
/// anything of it was heard — the model was still writing its call — so its
/// `response.create` is carried again for after the turn. The call closes
/// never made, and `@openai/agents` sends its follow-up for it while the
/// user still talks: that is absorbed into the carried create, with no
/// `conversation_already_has_active_response`, and exactly one response
/// follows the turn.
#[tokio::test]
async fn the_follow_up_after_a_cut_nobody_heard_joins_the_carried_create() {
    let (state, addr, chat, _tts, asr) = barge_gateway().await;
    let (stub, _gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let never = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_r1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"x"}"#,
        },
        Step::Wait(never.clone()),
        Step::Finish("tool_calls"),
    ]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let ws = barge_tools_session(&addr, json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create", "event_id": "first"}));
    ear.until("response.mcp_call_arguments.delta").await;

    mic.say(fixture("en_complete_short.wav")).await;
    ear.until("input_audio_buffer.speech_started").await;
    let cut = ear.until("response.done").await;
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    let call = closed_call(&cut);
    assert_eq!(call["error"]["message"], UNMADE, "{call}");

    let rest = follow_up_answers_once(|v| mic.send(v), &mut ear).await;
    assert!(
        !cut.iter().chain(&rest).any(|e| e["type"] == "error"),
        "nothing refused"
    );
    assert_eq!(chat.seen.chat_count(), 2);
    let sent = shape(&chat.seen.chat(1));
    assert!(
        sent.contains(&("tool".to_string(), format!("result:call_r1:{UNMADE}"))),
        "{sent:?}"
    );
    assert_eq!(
        sent.last().unwrap(),
        &("user".to_string(), QUESTION.to_string())
    );
    assert!(stub.calls().is_empty());
}

/// A call runs at once, but in speech mode its `in_progress` waits behind
/// the clauses before it. A cancel in between still knows the server has it
/// (`ToolSent`, §2.5): the call closes abandoned — "may still have run" —
/// not never made, which would invite the model to make it again.
#[tokio::test]
async fn a_cancel_while_a_sent_call_s_report_waits_behind_a_clause_abandons_it() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let held = Arc::new(Notify::new());
    tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    tts.push(Tts::Held(held.clone(), wav(&speech(300), 24_000)));
    chat.push(Turn::Stream(vec![
        Step::Text("One moment. "),
        Step::CallStart {
            index: 0,
            id: Some("call_q1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"queued"}"#,
        },
        Step::Text("Still there? "),
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({"tools": label_a()})).await;
    listed(&mut ws).await;
    send(&mut ws, user_text("queue it")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let before = loop {
        let events = events_until(&mut ws, "response.output_item.added").await;
        if events.last().unwrap()["item"]["type"] == "mcp_call" {
            break events;
        }
    };
    stub.wait_calls(1).await;
    // The second clause is held, and the call's report behind it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tts.seen.count() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no second clause");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let all: Vec<Value> = before.iter().chain(&events).cloned().collect();
    assert_eq!(count(&all, "response.mcp_call.in_progress"), 0);
    let call = closed_call(&events);
    assert_eq!(call["error"]["message"], ABANDONED, "{call}");
    assert_eq!(events.last().unwrap()["response"]["status"], "cancelled");
    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(tool_rows[0].error_kind.as_deref(), Some("canceled"));
}

/// §2.5 (final review #10): the user's turn ends — committed and
/// transcribed — while the call still runs, so its automatic response
/// starts right at the call's `response.done` and renders the result. The
/// SDK's follow-up meets it generating and is refused once, as any second
/// create is, in words that say the running response already answers with
/// the tool results.
#[tokio::test]
async fn a_follow_up_after_the_turn_s_own_response_started_is_refused_saying_it_answers() {
    let (state, addr, chat, _tts, asr) = barge_gateway().await;
    let (stub, gate) = held_call_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    chat.push(calls(
        &[(0, "call_t1", "a__echo", r#"{"text":"slow"}"#)],
        "tool_calls",
    ));
    let answering = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::Wait(answering.clone()),
        Step::Text("Two blocks."),
        Step::Finish("stop"),
    ]));
    asr.push(Asr::Text(QUESTION));
    let ws = barge_tools_session(&addr, json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(hallo());
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    ear.until("response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;

    mic.say(fixture("en_complete_short.wav")).await;
    let turn = ear
        .until("conversation.item.input_audio_transcription.completed")
        .await;
    assert_eq!(count(&turn, "response.done"), 0, "{:?}", types(&turn));
    gate.notify_one();
    let first = ear.until("response.done").await;
    assert_eq!(first.last().unwrap()["response"]["status"], "completed");
    ear.until("response.created").await;

    mic.send(json!({"type": "response.create", "event_id": "follow"}));
    let refused = ear.until("error").await;
    let e = &refused.last().unwrap()["error"];
    assert_eq!(e["code"], "conversation_already_has_active_response", "{e}");
    assert_eq!(e["event_id"], "follow", "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("already answers with the results of this session's last MCP calls"),
        "{e}"
    );

    answering.notify_one();
    let rest = ear.until("response.done").await;
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    let later = ear.quiet_for(Duration::from_millis(600)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(chat.seen.chat_count(), 2);
    let sent = shape(&chat.seen.chat(1));
    assert!(
        sent.contains(&("tool".to_string(), "result:call_t1:echo: slow".to_string())),
        "{sent:?}"
    );
    assert_eq!(
        sent.last().unwrap(),
        &("user".to_string(), QUESTION.to_string())
    );
}
