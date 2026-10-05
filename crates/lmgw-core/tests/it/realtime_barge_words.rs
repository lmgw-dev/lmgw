//! The barge-in word check over a session, in real time (realtime design
//! §6.4, owner's decision 2026-10-01): speech that passes the evidence gate
//! while the answer plays is transcribed before `speech_started` — a
//! backchannel or nothing keeps the answer playing, words cut it, and a
//! check with no answer leaves the duration rule. Every check is an ASR
//! call with its own usage row.
//!
//! The scripted ASR answers in order: the checks come first, the turn's
//! own transcript after its commit. As in `realtime_barge`, every answer is
//! the 3.6 s two-sentence fixture, the user's speech is the
//! `en_complete_short` question (speech 300–1738 ms of the clip), and the
//! knobs are the suite's — with `barge_in_check: "words"`.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{
    add_asr_alias, add_asr_alias_named, asr_fake, fixture, Asr, ASR_ALIAS,
};
use crate::support::realtime_fakes::{next_event, open, send, types, Turn};
use crate::support::realtime_mic::{barge_gateway, barge_session, live_mic, Ear, Mic, Player};
use crate::support::realtime_tts::{speech_gateway, wav, Tts};

const QUESTION: &str = "Where is the nearest station?";

fn long_answer() -> Tts {
    Tts::Wav(wav(&fixture("en_two_sentences_pause.wav"), 24_000))
}

fn question() -> Vec<i16> {
    fixture("en_complete_short.wav")
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

/// The rows `alias` wrote: (status, label).
async fn rows_of(state: &SharedState, alias: &str) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT status, ingress_proto FROM request_logs WHERE requested_alias = ?1 ORDER BY id",
    )
    .bind(alias)
    .fetch_all(&state.db)
    .await
    .unwrap()
}

/// The session's ASR rows: (status, label).
async fn asr_rows(state: &SharedState) -> Vec<(i64, String)> {
    rows_of(state, ASR_ALIAS).await
}

/// A spoken session with the word check, `lmgw` merged in; the answer is
/// asked for and playing when this returns, 500 ms in.
async fn playing(addr: &str, lmgw: Value) -> (Mic, Ear, Value) {
    let mut ext = json!({"barge_in_check": "words"});
    for (k, v) in lmgw.as_object().unwrap() {
        ext[k] = v.clone();
    }
    let (ws, updated) = barge_session(addr, json!({"type": "server_vad"}), ext).await;
    let (mic, mut ear) = live_mic(ws);
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        if ev["type"] == "response.output_audio.delta" {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    (mic, ear, updated)
}

/// A backchannel the gate passes — 544 ms of speech, past its 200 ms —
/// that the ASR hears as `heard`: the answer plays to its end.
async fn backchannel_plays_on(heard: &'static str) {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    for _ in 0..4 {
        asr.push(Asr::Text(heard));
    }
    let (mic, mut ear, updated) = playing(&addr, json!({})).await;
    let lmgw = &updated["session"]["lmgw"];
    assert_eq!(
        (&lmgw["barge_in_check"], &lmgw["barge_in_check_timeout_ms"]),
        (&json!("words"), &json!(500))
    );
    mic.say(question()[300 * 24..844 * 24].to_vec()).await;
    let events = ear.until("response.done").await;
    assert_eq!(
        count(&events, "input_audio_buffer.speech_started"),
        0,
        "{heard:?}: {:?}",
        types(&events)
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    let later = ear.quiet_for(Duration::from_millis(1200)).await;
    assert!(later.is_empty(), "{:?}", types(&later));
    // Checked at the gate (200 ms of voice, E7), again after another
    // 200 ms, and once more at the speech's end if voice came after the
    // last check — each a row of its own, labelled realtime.
    let checks = asr.seen.count();
    assert!((1..=3).contains(&checks), "{checks} checks");
    let rows = asr_rows(&state).await;
    assert_eq!(rows.len(), checks);
    assert!(
        rows.iter().all(|r| *r == (200, "realtime".into())),
        "{rows:?}"
    );
    assert_eq!(
        chat.seen.chat_count(),
        1,
        "nothing answered the backchannel"
    );
}

#[tokio::test]
async fn a_backchannel_the_gate_passed_keeps_the_answer_playing() {
    backchannel_plays_on("Mhm.").await;
}

#[tokio::test]
async fn speech_the_asr_hears_no_words_in_keeps_the_answer_playing() {
    backchannel_plays_on("").await;
}

/// The question over the answer; the events from `speech_started` to the
/// cut's `response.done`, and where the speech began on the input.
async fn barge(mic: &Mic, ear: &mut Ear) -> (Vec<Value>, u64) {
    let spoke_at = mic.say(question()).await;
    let mut before = ear.until("input_audio_buffer.speech_started").await;
    let started = before.pop().unwrap();
    assert!(
        !before.iter().any(|e| e["type"] == "response.done"),
        "cut before the answer ended: {:?}",
        types(&before)
    );
    let mut cut = vec![started];
    cut.extend(ear.until("response.done").await);
    (cut, spoke_at)
}

/// What every cut looks like on the wire: `speech_started` first, then the
/// cancelled item's audio part — `@openai/agents` interrupts only until
/// it — and `response.done {cancelled, turn_detected}`; the start
/// back-dated to the onset.
fn assert_cut(cut: &[Value], spoke_at: u64) {
    assert_eq!(
        types(cut),
        [
            "input_audio_buffer.speech_started",
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
        ]
    );
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    let start = cut[0]["audio_start_ms"].as_u64().unwrap();
    assert!(start.abs_diff(spoke_at) <= 96, "{start} vs {spoke_at}");
}

/// After the cut: the turn ends, is transcribed and answered.
async fn answered(ear: &mut Ear) -> Vec<Value> {
    let turn = ear.until("response.done").await;
    let transcript = turn
        .iter()
        .find(|e| e["type"] == "conversation.item.input_audio_transcription.completed")
        .expect("the turn's transcript");
    assert_eq!(transcript["transcript"], QUESTION);
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    turn
}

#[tokio::test]
async fn words_cut_the_answer_in_the_order_the_client_needs() {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text("Stopp."));
    asr.push(Asr::Text(QUESTION));
    let (mic, mut ear, _) = playing(&addr, json!({})).await;
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    answered(&mut ear).await;
    // The check, then the turn: two rows.
    assert_eq!(asr.seen.count(), 2);
    assert_eq!(asr_rows(&state).await.len(), 2);
    // The check's upload was the speech so far, pre-roll included — far
    // shorter than the turn's.
    let (_, _, check) = asr.seen.wav(0);
    let (_, _, turn) = asr.seen.wav(1);
    assert!(check < turn, "{check} vs {turn}");
}

#[tokio::test]
async fn a_backchannel_that_goes_on_is_checked_again_and_cuts() {
    // "Mhm, aber warte mal": the first check hears only "Mhm".
    let (_state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text("Mhm."));
    asr.push(Asr::Text("Mhm, aber warte mal."));
    asr.push(Asr::Text(QUESTION));
    let (mic, mut ear, _) = playing(&addr, json!({})).await;
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    answered(&mut ear).await;
    assert_eq!(asr.seen.count(), 3, "two checks and the turn");
    // The re-check uploads what came since the first, with a pre-roll of
    // overlap (E2) — never the whole turn again.
    let (_, _, first) = asr.seen.wav(0);
    let (_, _, second) = asr.seen.wav(1);
    let (_, _, turn) = asr.seen.wav(2);
    assert!(first < turn && second < turn, "{first}, {second} vs {turn}");
}

#[tokio::test]
async fn a_check_that_fails_leaves_the_duration_rule_and_cuts() {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Status(
        500,
        json!({"error": {"message": "engine fell over", "type": "server_error"}}),
    ));
    asr.push(Asr::Text(QUESTION));
    let (mic, mut ear, _) = playing(&addr, json!({})).await;
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    answered(&mut ear).await;
    let rows = asr_rows(&state).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_ne!(rows[0].0, 200, "the failed check has its row");
}

#[tokio::test]
async fn a_check_that_does_not_answer_in_time_leaves_the_duration_rule() {
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), "Mhm."));
    asr.push(Asr::Text(QUESTION));
    let (mic, mut ear, updated) = playing(&addr, json!({"barge_in_check_timeout_ms": 150})).await;
    assert_eq!(updated["session"]["lmgw"]["barge_in_check_timeout_ms"], 150);
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    // The late "Mhm." changes nothing; the call still ends and is logged.
    release.notify_one();
    answered(&mut ear).await;
    assert_eq!(asr_rows(&state).await.len(), 2);
}

/// Whether a multipart upload carries the field `name` with `value`.
fn has_field(body: &[u8], name: &str, value: &str) -> bool {
    let body = String::from_utf8_lossy(body);
    let head = format!("name=\"{name}\"");
    body.split("--")
        .any(|part| part.contains(&head) && part.trim_end().ends_with(value))
}

#[tokio::test]
async fn the_check_uses_its_own_alias_and_every_call_the_session_s_language() {
    // W1 (live run 2): the word check can have an ASR alias of its own —
    // nemotron heard nothing in the first check of 7 of 7 barge-ins, qwen3
    // heard them all — while the turns keep the session's. And the session's
    // transcription language goes up with the check and the turn alike, as
    // the two-letter code it names (fix package B6).
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    let checker = asr_fake().await;
    add_asr_alias_named(&state, &checker, "checker").await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    checker.push(Asr::Text("Stopp."));
    asr.push(Asr::Text(QUESTION));
    let mut ext = json!({"barge_in_check": "words"});
    ext["barge_in_check_alias"] = json!("checker");
    let (ws, updated) = barge_session(&addr, json!({"type": "server_vad"}), ext).await;
    assert_eq!(
        updated["session"]["lmgw"]["barge_in_check_alias"],
        "checker"
    );
    let (mic, mut ear) = live_mic(ws);
    mic.send(
        json!({"type": "session.update", "session": {"type": "realtime",
        "audio": {"input": {"transcription": {"language": "de-DE"}}}}}),
    );
    ear.until("session.updated").await;
    mic.send(json!({"type": "response.create"}));
    let mut player = Player::default();
    loop {
        let (at, ev) = ear.next().await;
        player.hear(at, &ev);
        if ev["type"] == "response.output_audio.delta" {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    answered(&mut ear).await;
    assert_eq!(checker.seen.count(), 1, "the check went to its own alias");
    assert_eq!(asr.seen.count(), 1, "the turn to the session's");
    let check = checker.seen.bodies.lock().unwrap()[0].clone();
    let turn = asr.seen.bodies.lock().unwrap()[0].clone();
    assert!(has_field(&check, "language", "de"), "the check's language");
    assert!(has_field(&turn, "language", "de"), "the turn's language");
    assert!(has_field(&check, "model", "nemotron-asr"));

    // Not set: the echo says so, and the session's alias checks (as in the
    // tests above).
    let (_, updated) = barge_session(&addr, json!({"type": "server_vad"}), json!({})).await;
    assert_eq!(updated["session"]["lmgw"]["barge_in_check_alias"], "");
}

#[tokio::test]
async fn an_empty_turn_the_check_heard_words_in_is_transcribed_again_with_the_check_s_alias() {
    // N3 (live runs 3/3b): the turn's ASR (nemotron) answered "" where the
    // word check (qwen3) had heard words, and no response followed. The
    // turn's audio goes once more to the check's alias, and its transcript
    // is the turn's — not the check's own words, which heard only the start.
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    let checker = asr_fake().await;
    add_asr_alias_named(&state, &checker, "checker").await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    checker.push(Asr::Text("Stopp."));
    asr.push(Asr::Text(""));
    checker.push(Asr::Text(QUESTION));
    let (mic, mut ear, updated) = playing(&addr, json!({"barge_in_check_alias": "checker"})).await;
    assert_eq!(
        updated["session"]["lmgw"]["barge_in_check_alias"],
        "checker"
    );
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    let turn = answered(&mut ear).await;
    assert_eq!(
        count(
            &turn,
            "conversation.item.input_audio_transcription.completed"
        ),
        1,
        "one transcript for the turn: {:?}",
        types(&turn)
    );
    assert_eq!(count(&turn, "response.created"), 1);
    assert_eq!(chat.seen.chat_count(), 2, "the turn was answered");

    // The check, the turn's own call, and the second call with the turn's
    // whole audio on the check's alias — each a row of its own.
    assert_eq!(asr.seen.count(), 1);
    assert_eq!(checker.seen.count(), 2);
    let (_, _, check) = checker.seen.wav(0);
    let (_, _, own) = asr.seen.wav(0);
    let (_, _, again) = checker.seen.wav(1);
    assert_eq!(again, own, "the same audio as the turn's own call");
    assert!(check < again, "{check} vs {again}");
    assert_eq!(rows_of(&state, ASR_ALIAS).await, [(200, "realtime".into())]);
    assert_eq!(
        rows_of(&state, "checker").await,
        [(200, "realtime".into()), (200, "realtime".into())]
    );
}

#[tokio::test]
async fn an_empty_turn_is_not_transcribed_again_by_the_alias_that_heard_nothing() {
    // With no check alias of its own the check is the session's ASR alias:
    // asking it again would only repeat the empty answer.
    let (state, addr, chat, tts, asr) = barge_gateway().await;
    tts.push(long_answer());
    chat.push(Turn::text(&["Trains leave every hour."]));
    asr.push(Asr::Text("Stopp."));
    asr.push(Asr::Text(""));
    let (mic, mut ear, _) = playing(&addr, json!({})).await;
    let (cut, spoke_at) = barge(&mic, &mut ear).await;
    assert_cut(&cut, spoke_at);
    let turn = ear
        .until("conversation.item.input_audio_transcription.completed")
        .await;
    assert_eq!(turn.last().unwrap()["transcript"], "");
    let later = ear.quiet_for(Duration::from_millis(1200)).await;
    assert_eq!(count(&later, "response.created"), 0, "{:?}", types(&later));
    assert_eq!(asr.seen.count(), 2, "the check and the turn, nothing more");
    assert_eq!(asr_rows(&state).await.len(), 2);
    assert_eq!(chat.seen.chat_count(), 1, "nothing answered the empty turn");
}

#[tokio::test]
async fn a_check_alias_that_is_no_asr_alias_is_refused_or_replaced() {
    // B5 review, fix package B6: a chat alias as the word check's alias
    // failed every check, and nothing said why. The owner's is warned about
    // when the session starts, which then checks with its own ASR alias.
    let asr = asr_fake().await;
    let (state, addr, _chat, _tts) = speech_gateway(false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
        s.realtime.barge_in_check_alias = "chatty".into();
    })
    .await;
    add_asr_alias(&state, &asr).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["type"], "session.created");
    assert_eq!(created["session"]["lmgw"]["barge_in_check_alias"], "");
    // A client's is refused, on its parameter.
    send(
        &mut ws,
        json!({"type": "session.update", "event_id": "u1", "session": {"type": "realtime",
               "lmgw": {"barge_in_check_alias": "chatty"}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["code"], "invalid_value");
    assert_eq!(e["error"]["param"], "session.lmgw.barge_in_check_alias");
    assert_eq!(e["error"]["event_id"], "u1");
    // An ASR alias is taken.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "lmgw": {"barge_in_check_alias": ASR_ALIAS}}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(
        updated["session"]["lmgw"]["barge_in_check_alias"], ASR_ALIAS,
        "{updated}"
    );
}
