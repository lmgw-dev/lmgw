//! Turn detection and the input buffer over a session (realtime design §6.2,
//! §6.3, §6.6, §16 "turn detection"): the committed synthetic fixtures
//! through the real detector, silence and noise synthesized in the test.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{
    add_asr_alias, append, asr_fake, fixture, silence, stream, Asr, AsrFake, ASR_ALIAS,
};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, types, ChatFake, Turn, Ws,
};

async fn setup() -> (String, ChatFake, AsrFake) {
    let chat = chat_fake().await;
    let asr = asr_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
    })
    .await;
    add_asr_alias(&state, &asr).await;
    (addr, chat, asr)
}

/// A text-output session with `turn_detection` as given; returns the socket
/// and the echoed session.
async fn session(addr: &str, turn_detection: Value) -> (Ws, Value) {
    let mut ws = open(addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "output_modalities": ["text"],
               "audio": {"input": {"transcription": {}, "turn_detection": turn_detection}}}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    (ws, updated["session"].clone())
}

/// Every event before the answer to a retrieve of a missing item.
async fn until_sentinel(ws: &mut Ws) -> Vec<Value> {
    send(
        ws,
        json!({"type": "conversation.item.retrieve", "item_id": "nope", "event_id": "sentinel"}),
    )
    .await;
    let mut out = Vec::new();
    loop {
        let ev = next_event(ws).await;
        if ev["error"]["event_id"] == "sentinel" {
            return out;
        }
        out.push(ev);
    }
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

/// The two-sentence fixture plus a second of silence, so the longest window
/// closes too.
fn two_sentences() -> Vec<i16> {
    let mut pcm = fixture("en_two_sentences_pause.wav");
    pcm.extend(silence(1000));
    pcm
}

#[tokio::test]
async fn the_silence_window_decides_whether_a_pause_ends_the_turn() {
    let (addr, _chat, _asr) = setup().await;
    // No automatic responses: only the turns are under test.
    for (window, turns) in [(500, 2), (800, 1)] {
        let (mut ws, _) = session(
            &addr,
            json!({"type": "server_vad", "silence_duration_ms": window,
                   "create_response": false}),
        )
        .await;
        stream(&mut ws, &two_sentences()).await;
        let events = until_sentinel(&mut ws).await;
        assert_eq!(
            count(&events, "input_audio_buffer.speech_started"),
            turns,
            "{window} ms: {:?}",
            types(&events)
        );
        assert_eq!(count(&events, "input_audio_buffer.committed"), turns);
        // Starts and stops pair up on the same item.
        let started: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "input_audio_buffer.speech_started")
            .map(|e| e["item_id"].clone())
            .collect();
        let committed: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "input_audio_buffer.committed")
            .map(|e| e["item_id"].clone())
            .collect();
        assert_eq!(started, committed);
        assert_eq!(count(&events, "response.created"), 0);
    }
}

#[tokio::test]
async fn noise_and_silence_are_not_a_turn() {
    let (addr, _chat, asr) = setup().await;
    let (mut ws, _) = session(&addr, json!({"type": "server_vad"})).await;
    let mut pcm = fixture("noise_only.wav");
    pcm.extend(silence(1000));
    stream(&mut ws, &pcm).await;
    assert_eq!(until_sentinel(&mut ws).await, Vec::<Value>::new());
    assert_eq!(asr.seen.count(), 0);
}

#[tokio::test]
async fn semantic_vad_s_escape_hatch_is_server_vad_and_says_so() {
    // `realtime.semantic_vad_engine: server_vad` (§6.3): semantic_vad as it
    // was before Smart Turn. Smart Turn itself: `realtime_semantic`.
    let chat = chat_fake().await;
    let asr = asr_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
        s.realtime.semantic_vad_engine = lmgw_core::config::SemanticVadEngine::ServerVad;
    })
    .await;
    add_asr_alias(&state, &asr).await;
    // The stock `@openai/agents` default: `semantic_vad`, no eagerness.
    let (_ws, s) = session(&addr, json!({"type": "semantic_vad"})).await;
    let td = &s["audio"]["input"]["turn_detection"];
    assert_eq!(td["type"], "semantic_vad");
    assert_eq!(td["eagerness"], "auto");
    assert_eq!(td["create_response"], true);
    assert_eq!(td["interrupt_response"], true);
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], "server_vad");
    assert_eq!(s["lmgw"]["resolved"]["semantic_vad"], Value::Null);

    // Eagerness picks the window: high (300 ms) splits the 700 ms pause,
    // low (800 ms) does not.
    for (eagerness, turns) in [("high", 2), ("low", 1)] {
        let (mut ws, _) = session(
            &addr,
            json!({"type": "semantic_vad", "eagerness": eagerness, "create_response": false}),
        )
        .await;
        stream(&mut ws, &two_sentences()).await;
        let events = until_sentinel(&mut ws).await;
        assert_eq!(
            count(&events, "input_audio_buffer.committed"),
            turns,
            "{eagerness}: {:?}",
            types(&events)
        );
    }
}

#[tokio::test]
async fn manual_turns_commit_and_clear_on_the_client_s_word() {
    let (addr, chat, asr) = setup().await;
    let (mut ws, s) = session(&addr, Value::Null).await;
    assert_eq!(s["audio"]["input"]["turn_detection"], Value::Null);
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], Value::Null);

    // Nothing to commit yet.
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.commit", "event_id": "c0"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "input_audio_buffer_commit_empty");
    assert_eq!(e["error"]["event_id"], "c0");

    // Speech, no detector: no events. A clear drops it.
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    send(&mut ws, json!({"type": "input_audio_buffer.clear"})).await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "input_audio_buffer.cleared"
    );
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    assert_eq!(
        next_event(&mut ws).await["error"]["code"],
        "input_audio_buffer_commit_empty"
    );

    // Committed: a user item and its transcript, and no response of its own.
    asr.push(Asr::Text("first"));
    let pcm = fixture("en_complete_short.wav");
    stream(&mut ws, &pcm).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let mut events = events_until(&mut ws, "conversation.item.done").await;
    events.extend(until_sentinel(&mut ws).await);
    assert_eq!(
        types(&events),
        [
            "input_audio_buffer.committed",
            "conversation.item.added",
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done",
        ]
    );
    // The whole buffer went up, at 16 kHz.
    let (rate, _, samples) = asr.seen.wav(0);
    assert_eq!(rate, 16_000);
    assert_eq!(samples, (pcm.len() * 2).div_ceil(3));

    // commit + response.create back to back: the response is created at
    // once, and waits for the transcript before it asks the model (§4.1).
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), "second"));
    chat.push(Turn::text(&["ok"]));
    append(&mut ws, &pcm[..24_000]).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let mut events = events_until(&mut ws, "response.created").await;
    release.notify_one();
    events.extend(events_until(&mut ws, "response.done").await);
    assert_eq!(
        types(&events)[..6],
        [
            "input_audio_buffer.committed",
            "conversation.item.added",
            "response.created",
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done",
            "response.output_item.added",
        ]
    );
    let last = chat.seen.chat(0)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], "first\nsecond");
}

#[tokio::test]
async fn with_detection_on_a_commit_takes_the_open_turn_only() {
    let (addr, _chat, _asr) = setup().await;
    let (mut ws, _) = session(&addr, json!({"type": "server_vad"})).await;
    // Silence only: no turn is open, so there is nothing to commit.
    stream(&mut ws, &silence(600)).await;
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.commit", "event_id": "c1"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "input_audio_buffer_commit_empty");
    assert_eq!(e["error"]["event_id"], "c1");

    // Mid-utterance, the commit ends the turn now — on the item the
    // speech_started already named.
    let pcm = fixture("en_complete_short.wav");
    stream(&mut ws, &pcm[..24_000]).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let events = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&events)[..3],
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.committed",
            "conversation.item.added",
        ]
    );
    assert_eq!(events[1]["item_id"], events[0]["item_id"]);
}

#[tokio::test]
async fn audio_that_does_not_decode_is_an_error_and_the_session_goes_on() {
    let (addr, _chat, _asr) = setup().await;
    let (mut ws, _) = session(&addr, json!({"type": "server_vad"})).await;
    for (audio, id) in [("not base64!", "a1"), ("AA==", "a2")] {
        send(
            &mut ws,
            json!({"type": "input_audio_buffer.append", "event_id": id, "audio": audio}),
        )
        .await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["error"]["code"], "invalid_value", "{e}");
        assert_eq!(e["error"]["param"], "audio");
        assert_eq!(e["error"]["event_id"], id);
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
}

#[tokio::test]
async fn switching_to_manual_turns_mid_turn_closes_the_turn_and_frees_response_create() {
    // B3 review 3: the open turn used to stay the core's, and every later
    // response.create was held for a turn that could never end.
    let (addr, chat, asr) = setup().await;
    let (mut ws, _) = session(&addr, json!({"type": "server_vad"})).await;
    let pcm = fixture("en_complete_short.wav");
    stream(&mut ws, &pcm[..24_000]).await;
    let started = events_until(&mut ws, "input_audio_buffer.speech_started").await;
    let item = started.last().unwrap()["item_id"].clone();
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    let events = until_sentinel(&mut ws).await;
    assert_eq!(
        types(&events),
        ["session.updated", "input_audio_buffer.speech_stopped"]
    );
    assert_eq!(events[1]["item_id"], item);
    assert_eq!(events[1]["audio_end_ms"], 1000);
    // A response starts at once: nobody is speaking any more.
    chat.push(Turn::text(&["ok"]));
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(types(&events)[0], "response.created");
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // The turn's audio is the manual buffer now: the client commits it, as
    // an item of its own.
    asr.push(Asr::Text("hello"));
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let events = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(types(&events)[0], "input_audio_buffer.committed");
    assert_ne!(events[0]["item_id"], item);
    let (_, _, samples) = asr.seen.wav(0);
    assert!(samples > 8_000, "the turn's audio went up: {samples}");
}
