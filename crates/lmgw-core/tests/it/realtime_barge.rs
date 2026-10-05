//! Barge-in over a session, in real time (realtime design §2.3, §6.4, §6.5,
//! §7.3): a live microphone streams 20 ms appends while the TTS fake's
//! answer plays, and the test plays the client — `@openai/agents` where it
//! says so, from its captured frames.
//!
//! Every answer is the 3.6 s two-sentence fixture; the user's speech is
//! the committed `en_complete_short` question (speech 300–1738 ms of the
//! clip). The knobs: 200 ms of lead, a 300 ms guard, 200 ms of evidence, a
//! 600 ms post-interrupt window. Positions are asserted on the input
//! timeline, where they are exact to a frame; wall-clock margins are wide.

use std::time::Duration;

use serde_json::{json, Value};

use crate::support::realtime_audio::{fixture, Asr};
use crate::support::realtime_fakes::{captured_client_frames, next_event, open, send, types, Turn};
use crate::support::realtime_mic::{barge_gateway, barge_session, knobs, live_mic, Ear, Player};
use crate::support::realtime_tts::{wav, Tts};

const QUESTION: &str = "Where is the nearest station?";

/// The answer every first response speaks: 3.6 s.
fn long_answer() -> Tts {
    Tts::Wav(wav(&fixture("en_two_sentences_pause.wav"), 24_000))
}

/// The user's question, as the microphone says it.
fn question() -> Vec<i16> {
    fixture("en_complete_short.wav")
}

/// Read events, playing the client, until the first output audio delta;
/// what was read.
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

/// `count` of `kind` in `events`.
fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

#[tokio::test]
async fn agents_js_barges_in_truncates_what_it_heard_and_is_answered() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&[
        "Trains leave from the main station every hour.",
    ]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));

    // `@openai/agents` exactly as captured (agents_js_barge.json), on this
    // gateway's model and the suite's knobs.
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    next_event(&mut ws).await;
    let mut frames = captured_client_frames("agents_js_barge.json").into_iter();
    let mut first = frames.next().unwrap();
    first["session"]["model"] = json!("chatty");
    first["session"]["lmgw"] = knobs();
    send(&mut ws, first).await;
    let updated = next_event(&mut ws).await;
    let td = &updated["session"]["audio"]["input"]["turn_detection"];
    assert_eq!(td["interrupt_response"], true, "the SDK decides on it");
    let lmgw = &updated["session"]["lmgw"];
    assert_eq!(
        (
            &lmgw["barge_in_min_ms"],
            &lmgw["barge_in_guard_ms"],
            &lmgw["half_duplex"]
        ),
        (&json!(200), &json!(300), &json!(false))
    );
    send(&mut ws, frames.next().unwrap()).await; // {tracing}
    next_event(&mut ws).await;

    let (mic, mut ear) = live_mic(ws);
    mic.send(frames.next().unwrap()); // "Hallo"
    ear.until("conversation.item.done").await;
    mic.send(frames.next().unwrap()); // response.create
    let mut player = Player::default();
    let mut events = until_audio(&mut ear, &mut player).await;
    let rid = player.response.clone().unwrap();
    let iid = player.item.clone().unwrap();

    // Half a second into the answer the user asks — past the guard, and
    // long before the answer's end.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let spoke_at = mic.say(question()).await;
    let (at, started) = loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        let started = ev["type"] == "input_audio_buffer.speech_started";
        events.push(ev.clone());
        if started {
            break (at, ev);
        }
    };
    // As the SDK does on speech_started with interrupt_response echoed true:
    // truncate what it played, retrieve the item — and no response.cancel.
    let truncate = player
        .truncate(at)
        .expect("still playing: the item is interruptible");
    let cut_ms = truncate["audio_end_ms"].as_u64().unwrap();
    mic.send(truncate);
    mic.send(json!({"type": "conversation.item.retrieve", "item_id": iid}));

    // Back-dated to the question's onset (300 ms into the clip) less the
    // 300 ms pre-roll: the clip's start, to a frame or two.
    let start_ms = started["audio_start_ms"].as_u64().unwrap();
    assert!(
        start_ms.abs_diff(spoke_at) <= 96,
        "{start_ms} vs {spoke_at}"
    );
    // The cancel follows at once, in this order.
    let mut after = Vec::new();
    for _ in 0..6 {
        after.push(ear.next().await.1);
    }
    assert_eq!(
        types(&after),
        [
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
        ]
    );
    let done = &after[5]["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    // The truncate cuts what was sent to what was played.
    let rest = ear.until("conversation.item.retrieved").await;
    assert_eq!(
        types(&rest),
        ["conversation.item.truncated", "conversation.item.retrieved"]
    );
    let heard = rest[1]["item"]["content"][0]["transcript"]
        .as_str()
        .unwrap()
        .to_string();
    let full = "Trains leave from the main station every hour.";
    assert!(
        full.starts_with(&heard) && heard.len() < full.len(),
        "{heard:?}"
    );
    // Cut by character share, then back to the last word heard whole
    // (§7.3): it ends where a word does, at most a word short of the share.
    let expect = full.len() * cut_ms as usize / 3649;
    assert!(
        heard.len() <= expect + 2 && expect <= heard.len() + "station ".len() + 2,
        "{heard:?} at {cut_ms} ms"
    );
    assert!(
        heard.is_empty() || full[heard.len()..].starts_with(' '),
        "{heard:?} ends inside a word"
    );

    // The question ends on the post-interrupt window (600 ms) and is
    // answered; nothing of the cut answer followed speech_started.
    let mut turn = ear.until("response.done").await;
    let stopped = turn
        .iter()
        .find(|e| e["type"] == "input_audio_buffer.speech_stopped")
        .unwrap();
    let end_ms = stopped["audio_end_ms"].as_u64().unwrap();
    assert!(
        end_ms >= spoke_at + 1738 + 600 - 64,
        "{end_ms} vs {spoke_at}"
    );
    assert!(
        end_ms < spoke_at + 1738 + 600 + 400,
        "{end_ms} vs {spoke_at}"
    );
    assert_eq!(stopped["item_id"], started["item_id"]);
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    assert_eq!(count(&turn, "response.created"), 1);
    turn.extend(after);
    assert!(
        !turn
            .iter()
            .any(|e| e["type"] == "response.output_audio.delta" && e["response_id"] == rid.as_str()),
        "audio of the cut answer after speech_started"
    );
    // The second request renders the cut answer and the question.
    let messages = chat.seen.chat(1)["messages"].as_array().unwrap().clone();
    let n = messages.len();
    assert_eq!(messages[n - 2]["role"], "assistant");
    assert_eq!(messages[n - 2]["content"], heard.as_str());
    assert_eq!(messages[n - 1]["content"], QUESTION);
    assert_eq!(asr.seen.count(), 1);
}

#[tokio::test]
async fn a_backchannel_while_the_answer_plays_is_neither_a_turn_nor_a_cut() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    let (ws, _) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(
        json!({"type": "conversation.item.create", "item": {"type": "message",
                    "role": "user", "content": [{"type": "input_text", "text": "Hallo"}]}}),
    );
    ear.until("conversation.item.done").await;
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // 150 ms of speech: "mhm".
    mic.say(question()[300 * 24..450 * 24].to_vec()).await;
    let events = ear.until("response.done").await;
    assert_eq!(
        count(&events, "input_audio_buffer.speech_started"),
        0,
        "{:?}",
        types(&events)
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // And nothing comes of it later.
    let later = ear.quiet_for(Duration::from_millis(1200)).await;
    assert!(later.is_empty(), "{:?}", types(&later));
    assert_eq!(asr.seen.count(), 0);
}

#[tokio::test]
async fn speech_after_the_answer_played_is_a_normal_turn() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(Tts::Wav(wav(&question()[..24 * 800], 24_000)));
    chat.push(Turn::text(&["Hello."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    // A long post-interrupt window, to tell the plain 500 ms one from it.
    let (ws, _) = barge_session(
        &addr,
        json!({"type": "server_vad"}),
        json!({"post_interrupt_silence_ms": 1500}),
    )
    .await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    let first = ear.until("response.done").await;
    assert_eq!(first.last().unwrap()["response"]["status"], "completed");
    mic.say(question()).await;
    let turn = ear.until("response.done").await;
    let t = types(&turn);
    assert_eq!(t[0], "input_audio_buffer.speech_started", "{t:?}");
    assert_eq!(count(&turn, "response.created"), 1);
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    // A plain turn: the 500 ms window, no turn_detected anywhere.
    let stopped = turn
        .iter()
        .find(|e| e["type"] == "input_audio_buffer.speech_stopped")
        .unwrap();
    let started = turn[0]["audio_start_ms"].as_u64().unwrap();
    let length = stopped["audio_end_ms"].as_u64().unwrap() - started;
    // Speech to 1738 ms of the clip, Silero's lag, and 500 ms — not 1500.
    assert!((2100..2700).contains(&length), "{length}");
    assert!(!turn
        .iter()
        .chain(&first)
        .any(|e| e["response"]["status_details"]["reason"] == "turn_detected"));
}

#[tokio::test]
async fn half_duplex_does_not_listen_while_the_answer_plays() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, updated) = barge_session(
        &addr,
        json!({"type": "server_vad"}),
        json!({"half_duplex": true}),
    )
    .await;
    assert_eq!(updated["session"]["lmgw"]["half_duplex"], true);
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The whole question, while the answer plays (what an echo would be).
    mic.say(question()).await;
    let events = ear.until("response.done").await;
    assert_eq!(
        count(&events, "input_audio_buffer.speech_started"),
        0,
        "{:?}",
        types(&events)
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(asr.seen.count(), 0, "nothing was committed");
    // Once it has played, the user is heard again.
    mic.say(question()).await;
    let turn = ear.until("response.done").await;
    assert_eq!(turn[0]["type"], "input_audio_buffer.speech_started");
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    assert_eq!(asr.seen.count(), 1);
}

#[tokio::test]
async fn a_barge_in_that_turns_out_to_be_noise_does_not_resume_the_answer() {
    // Owner's decision Q2: the gate passed it, the ASR heard no words — the
    // interrupted answer stays cut, and nothing answers the noise.
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    asr.push(Asr::Text(""));
    let (ws, _) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    until_audio(&mut ear, &mut player).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    mic.say(question()).await;
    let cut = ear.until("response.done").await;
    assert_eq!(
        cut.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    let turn = ear
        .until("conversation.item.input_audio_transcription.completed")
        .await;
    assert_eq!(turn.last().unwrap()["transcript"], "");
    let later = ear.quiet_for(Duration::from_millis(800)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(chat.seen.chat_count(), 1);
}

#[tokio::test]
async fn speech_as_the_answer_plays_out_is_judged_by_its_window_and_echo_tail() {
    // B3 review 12: the client does not wait for response.done to talk.
    // Made to tell whether the window's margin is there (B3 review L4: the
    // test passed without it) and to hold on a loaded runner:
    // - each 800 ms answer leaves at once (a lead longer than it), so its
    //   playback ends 800 ms after its first audio left, whatever the
    //   pacer's timers do under load;
    // - a 1.5 s echo tail leaves every timing a wide berth.
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    for _ in 0..2 {
        tts.push(Tts::Wav(wav(&question()[..24 * 800], 24_000)));
    }
    chat.push(Turn::text(&["Hello."]));
    chat.push(Turn::text(&["Hello again."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, updated) = barge_session(
        &addr,
        json!({"type": "server_vad"}),
        json!({"output_lead_ms": 2000, "echo_tail_ms": 1500}),
    )
    .await;
    assert_eq!(updated["session"]["lmgw"]["echo_tail_ms"], 1500);
    let (mic, mut ear) = live_mic(ws);

    // A "mhm" (150 ms of voice) right after the answer played out: still
    // inside the window's echo tail, so the gate judges it — too short to
    // be a turn. Without the margin it would be a normal turn (96 ms).
    mic.send(json!({"type": "response.create"}));
    let first = ear.until("response.done").await;
    assert_eq!(first.last().unwrap()["response"]["status"], "completed");
    mic.say(question()[300 * 24..450 * 24].to_vec()).await;
    let later = ear.quiet_for(Duration::from_millis(1000)).await;
    assert_eq!(
        count(&later, "input_audio_buffer.speech_started"),
        0,
        "{:?}",
        types(&later)
    );
    assert_eq!(asr.seen.count(), 0);

    // The question, its speech starting 50 ms before the next answer's end
    // (the client's clock — later still on the server's): it earns its
    // turn after the playback, and the answer is not cut.
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    let mut events = until_audio(&mut ear, &mut player).await;
    let audio = player.first.unwrap();
    tokio::time::sleep_until(audio + Duration::from_millis(800 - 50 - 300)).await;
    let spoke_at = mic.say(question()).await;
    events.extend(ear.until("response.done").await);
    let done = events.last().unwrap();
    assert_eq!(done["response"]["status"], "completed", "not cut");
    assert_eq!(count(&events, "input_audio_buffer.speech_started"), 0);
    // The turn follows, back-dated to the onset inside the window.
    let turn = ear.until("response.done").await;
    let t = types(&turn);
    assert_eq!(t[0], "input_audio_buffer.speech_started", "{t:?}");
    let start_ms = turn[0]["audio_start_ms"].as_u64().unwrap();
    assert!(
        start_ms.abs_diff(spoke_at) <= 96,
        "{start_ms} vs {spoke_at}"
    );
    assert_eq!(count(&turn, "response.created"), 1);
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    assert!(!turn
        .iter()
        .chain(&events)
        .any(|e| e["response"]["status_details"]["reason"] == "turn_detected"));
    assert_eq!(asr.seen.count(), 1);
}
