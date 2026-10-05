//! `semantic_vad` with Smart Turn, in real time (realtime design §6.3): a
//! live microphone says the committed synthetic clips, the real Silero and
//! the real Smart Turn judge them, and the turns' ends are read off
//! `speech_stopped` on the input timeline — where they are exact to a
//! frame, apart from how long a score takes to come back.
//!
//! The clips: `en_complete_short` (a question, speech 300–1738 ms, scores
//! ~0.98 at its end) and `en_midsentence_pause` (speech 300–2488 ms, a
//! 400 ms pause inside the sentence that scores ~0.01, then 2888–4399 ms;
//! the end scores ~0.9). Defaults per eagerness: high 0.5 / floor 0.1 /
//! 2 s, medium and auto 0.5 / 0.2 / 4 s, low 0.95 / none / 3 s; a pause
//! between floor and threshold commits at 500 ms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::{SemanticVadEngine, Settings};
use lmgw_core::realtime::turn::smart_turn::{SmartTurn, INTRA_THREADS, SMART_TURN_ONNX};
use lmgw_core::state::SharedState;
use serde_json::{json, Value};

use crate::support::realtime_audio::{
    add_asr_alias, asr_fake, fixture, silence, stream, AsrFake, ASR_ALIAS,
};
use crate::support::realtime_fakes::{
    chat_fake, gateway, next_event, open, send, types, ChatFake, Turn, Ws,
};
use crate::support::realtime_mic::{barge_gateway, barge_session, live_mic, Ear, Mic};
use crate::support::realtime_tts::{wav, Tts};

const QUESTION_END: u64 = 1738;
const MID_PAUSE: u64 = 2488;
const MID_END: u64 = 4399;

/// A gateway with the ASR fake; the chat fake is never called (no session
/// here answers), only kept.
async fn setup(tweak: impl FnOnce(&mut Settings)) -> (SharedState, String, AsrFake, ChatFake) {
    let chat = chat_fake().await;
    let asr = asr_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
        tweak(s);
    })
    .await;
    add_asr_alias(&state, &asr).await;
    (state, addr, asr, chat)
}

/// A text session that commits turns and answers none, on `turn_detection`;
/// the socket and the echoed session.
async fn session(addr: &str, turn_detection: Value) -> (Ws, Value) {
    let mut ws = open(addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    let mut td = turn_detection;
    td["create_response"] = json!(false);
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "output_modalities": ["text"],
               "audio": {"input": {"transcription": {}, "turn_detection": td}}}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    (ws, updated["session"].clone())
}

/// [`session`] with a live microphone.
async fn live(addr: &str, turn_detection: Value) -> (Mic, Ear, Value) {
    let (ws, s) = session(addr, turn_detection).await;
    let (mic, ear) = live_mic(ws);
    (mic, ear, s)
}

/// `pcm` followed by `ms` of silence.
fn then_silence(mut pcm: Vec<i16>, ms: usize) -> Vec<i16> {
    pcm.extend(silence(ms));
    pcm
}

/// The next turn's `speech_stopped`, after its `speech_started`.
async fn next_stop(ear: &mut Ear) -> Value {
    let events = ear.until("input_audio_buffer.speech_stopped").await;
    assert_eq!(
        types(&events)
            .iter()
            .filter(|t| **t == "input_audio_buffer.speech_started")
            .count(),
        1,
        "{:?}",
        types(&events)
    );
    events.last().unwrap().clone()
}

fn end_ms(stopped: &Value) -> u64 {
    stopped["audio_end_ms"].as_u64().unwrap()
}

/// Smart Turn itself, installed as the test hook so each score is seen:
/// which part of the rule ended a turn is then read off its scores, not off
/// a commit time that a debug build's slower score stretches (fix package
/// B6).
fn recorded_scores(state: &SharedState) -> Arc<Mutex<Vec<f32>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let model = Mutex::new(SmartTurn::from_bytes(SMART_TURN_ONNX, INTRA_THREADS).unwrap());
    let out = seen.clone();
    state.set_turn_score_for_tests(Some(Arc::new(move |audio: &[f32]| {
        let p = model
            .lock()
            .unwrap()
            .score(audio)
            .map_err(|e| e.to_string())?;
        out.lock().unwrap().push(p);
        Ok(p)
    })));
    seen
}

/// The rule's own time and a score's, with room for a debug build — and
/// well under the 2 s maximum wait of high eagerness, the shortest.
const WELL_UNDER_MAX_WAIT: std::ops::Range<u64> = 200..1000;

/// A finished question commits at the probe (~224 ms into the pause, plus
/// Silero's lag and the score's time) on the threshold: its one score is at
/// or above 0.5, which commits at once — not at the 4 s maximum wait.
#[tokio::test]
async fn a_complete_sentence_commits_on_the_threshold() {
    let (state, addr, asr, _chat) = setup(|_| {}).await;
    let scores = recorded_scores(&state);
    let (mic, mut ear, s) = live(&addr, json!({"type": "semantic_vad"})).await;
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], "semantic_vad");
    let at = mic
        .say(then_silence(fixture("en_complete_short.wav"), 1500))
        .await;
    let after = end_ms(&next_stop(&mut ear).await) - (at + QUESTION_END);
    let seen = scores.lock().unwrap().clone();
    eprintln!("committed {after} ms after the speech, scores {seen:?}");
    assert!(
        matches!(seen[..], [p] if p >= 0.5),
        "the threshold: {seen:?}"
    );
    assert!(
        WELL_UNDER_MAX_WAIT.contains(&after),
        "{after} ms after the speech"
    );
    ear.until("conversation.item.done").await;
    assert_eq!(asr.seen.count(), 1);
}

/// High eagerness: the 400 ms pause inside the sentence scores far below
/// the floor and holds — the plain 300 ms window of high eagerness would
/// have split there — and the turn goes on to the sentence's end.
#[tokio::test]
async fn a_mid_sentence_pause_stays_open() {
    let (state, addr, asr, _chat) = setup(|_| {}).await;
    let scores = recorded_scores(&state);
    let (mic, mut ear, _) = live(&addr, json!({"type": "semantic_vad", "eagerness": "high"})).await;
    let at = mic
        .say(then_silence(fixture("en_midsentence_pause.wav"), 1500))
        .await;
    let stopped = next_stop(&mut ear).await;
    let after = end_ms(&stopped) as i64 - (at + MID_END) as i64;
    let seen = scores.lock().unwrap().clone();
    eprintln!("committed {after} ms after the speech, scores {seen:?}");
    // The pause inside the sentence below high's floor, the end on the
    // threshold.
    assert!(
        matches!(seen[..], [mid, .., end] if mid < 0.1 && end >= 0.5),
        "{seen:?}"
    );
    assert!(
        u64::try_from(after).is_ok_and(|a| WELL_UNDER_MAX_WAIT.contains(&a)),
        "{after} ms after the sentence"
    );
    ear.until("conversation.item.done").await;
    // One turn: nothing else starts.
    let later = ear.quiet_for(Duration::from_millis(800)).await;
    assert!(
        !later
            .iter()
            .any(|e| e["type"] == "input_audio_buffer.speech_started"),
        "{:?}",
        types(&later)
    );
    assert_eq!(asr.seen.count(), 1);
}

/// A sentence that stops mid-way ("… this report because") scores below
/// the floor: the turn commits at the maximum wait, 2 s at high eagerness
/// (63 frames), counted from Silero's first unvoiced frame.
#[tokio::test]
async fn a_low_score_end_commits_at_the_max_wait() {
    let (_s, addr, _asr, _chat) = setup(|_| {}).await;
    let (mic, mut ear, _) = live(&addr, json!({"type": "semantic_vad", "eagerness": "high"})).await;
    let cut = fixture("en_midsentence_pause.wav")[..MID_PAUSE as usize * 24].to_vec();
    let at = mic.say(then_silence(cut, 3000)).await;
    let after = end_ms(&next_stop(&mut ear).await) - (at + MID_PAUSE);
    eprintln!("committed {after} ms after the speech");
    assert!((2000..2150).contains(&after), "{after} ms after the speech");
}

/// No score — here every one fails — and each pause commits on the plain
/// window of its eagerness (500 ms for auto), as before Smart Turn.
#[tokio::test]
async fn a_scorer_that_fails_falls_back_to_the_silence_window() {
    let (state, addr, _asr, _chat) = setup(|_| {}).await;
    state.set_turn_score_for_tests(Some(Arc::new(
        |_: &[f32]| Err("test: no model".to_string()),
    )));
    let (mic, mut ear, _) = live(&addr, json!({"type": "semantic_vad"})).await;
    let at = mic
        .say(then_silence(fixture("en_complete_short.wav"), 1500))
        .await;
    let after = end_ms(&next_stop(&mut ear).await) - (at + QUESTION_END);
    eprintln!("committed {after} ms after the speech");
    assert!((512..650).contains(&after), "{after} ms after the speech");
}

/// The echo is the eagerness's row; a row the owner changed applies:
/// medium with its threshold at 0.99 takes the question's ~0.98 as unsure
/// — at or above the floor — and commits at the 500 ms floor window.
#[tokio::test]
async fn each_eagerness_runs_its_row() {
    let (_s, addr, _asr, _chat) = setup(|s| s.realtime.semantic_vad.medium.threshold = 0.99).await;
    for (eagerness, want) in [
        ("high", json!([0.5, 0.1, 2000, 300])),
        ("medium", json!([0.99, 0.2, 4000, 500])),
        ("auto", json!([0.99, 0.2, 4000, 500])),
        ("low", json!([0.95, 0.95, 3000, 800])),
    ] {
        let (_ws, s) = session(
            &addr,
            json!({"type": "semantic_vad", "eagerness": eagerness}),
        )
        .await;
        let r = &s["lmgw"]["resolved"]["semantic_vad"];
        assert_eq!(
            json!([
                r["threshold"],
                r["floor"],
                r["max_wait_ms"],
                r["silence_duration_ms"]
            ]),
            want,
            "{eagerness}: {r}"
        );
        assert_eq!(r["floor_window_ms"], 500);
    }
    let (mic, mut ear, _) = live(
        &addr,
        json!({"type": "semantic_vad", "eagerness": "medium"}),
    )
    .await;
    let at = mic
        .say(then_silence(fixture("en_complete_short.wav"), 1500))
        .await;
    let after = end_ms(&next_stop(&mut ear).await) - (at + QUESTION_END);
    eprintln!("committed {after} ms after the speech");
    assert!((512..650).contains(&after), "{after} ms after the speech");
}

/// Fix package B6 (WP6 review): a stored row that cannot run is the
/// owner's setting, not the client's error — the update is not refused, and
/// the session runs the built-in row and floor window.
#[tokio::test]
async fn a_stored_row_that_cannot_run_is_replaced_not_blamed_on_the_client() {
    let (_s, addr, _asr, _chat) = setup(|s| {
        s.realtime.semantic_vad.high.floor = 0.9;
        s.realtime.semantic_floor_window_ms = 2500;
    })
    .await;
    let resolved = |s: &Value| {
        let r = &s["lmgw"]["resolved"]["semantic_vad"];
        json!([
            r["threshold"],
            r["floor"],
            r["max_wait_ms"],
            r["silence_duration_ms"],
            r["floor_window_ms"]
        ])
    };
    let (_ws, s) = session(&addr, json!({"type": "semantic_vad", "eagerness": "high"})).await;
    assert_eq!(resolved(&s), json!([0.5, 0.1, 2000, 300, 500]));
    // Medium runs as stored: its 4 s wait holds the 2.5 s floor window.
    let (_ws, s) = session(
        &addr,
        json!({"type": "semantic_vad", "eagerness": "medium"}),
    )
    .await;
    assert_eq!(resolved(&s), json!([0.5, 0.2, 4000, 500, 2500]));
}

/// `realtime.semantic_vad_engine: server_vad`: plain `server_vad` on the
/// eagerness's window — high's 300 ms splits the mid-sentence pause that
/// Smart Turn holds — and the echo says so.
#[tokio::test]
async fn the_engine_escape_hatch_is_plain_server_vad() {
    let (_s, addr, _asr, _chat) =
        setup(|s| s.realtime.semantic_vad_engine = SemanticVadEngine::ServerVad).await;
    let (mut ws, s) = session(&addr, json!({"type": "semantic_vad", "eagerness": "high"})).await;
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], "server_vad");
    assert_eq!(s["lmgw"]["resolved"]["semantic_vad"], Value::Null);
    // Frames, not wall time, decide plain server_vad: streamed at once.
    stream(
        &mut ws,
        &then_silence(fixture("en_midsentence_pause.wav"), 1000),
    )
    .await;
    let mut committed = 0;
    while committed < 2 {
        let ev = next_event(&mut ws).await;
        committed += usize::from(ev["type"] == "input_audio_buffer.committed");
    }
}

/// After a barge-in nothing commits before `post_interrupt_silence_ms`
/// (1500 ms here), though Smart Turn calls the question complete at once.
#[tokio::test]
async fn the_post_interrupt_window_still_holds() {
    let (_s, addr, chat, tts, _asr) = barge_gateway().await;
    tts.push(Tts::Wav(wav(
        &fixture("en_two_sentences_pause.wav"),
        24_000,
    )));
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two blocks."]));
    let (ws, updated) = barge_session(
        &addr,
        json!({"type": "semantic_vad"}),
        json!({"post_interrupt_silence_ms": 1500}),
    )
    .await;
    assert_eq!(
        updated["session"]["lmgw"]["resolved"]["turn_detection"],
        "semantic_vad"
    );
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    loop {
        if ear.next().await.1["type"] == "response.output_audio.delta" {
            break;
        }
    }
    // Into the answer, past the guard: the question cuts it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let at = mic
        .say(then_silence(fixture("en_complete_short.wav"), 2500))
        .await;
    let events = ear.until("input_audio_buffer.speech_stopped").await;
    assert!(
        events
            .iter()
            .any(|e| e["response"]["status_details"]["reason"] == "turn_detected"),
        "{:?}",
        types(&events)
    );
    let after = end_ms(events.last().unwrap()) - (at + QUESTION_END);
    eprintln!("committed {after} ms after the speech");
    assert!(
        (1500 - 64..1500 + 400).contains(&after),
        "{after} ms after the speech"
    );
}
