//! Voice turns the model hears, WP3 and its review (voice-audio-input
//! design §3.2, WP3 review #2, #3): a bound session on `gpu_world`'s `gemma`
//! with the realtime fakes (`session`), when the transcript does not come.
//!
//! - **A failed transcription** of a turn the model heard — it answered
//!   from the audio first — plays the reply all the same, the row says it
//!   was not transcribed, and the next request carries the placeholder.
//! - One that fails **before the model answered**, or a turn whose **speech
//!   recognition went away**, is said as with audio input off and ends the
//!   turn quietly: no row, no reply.
//! - **The session closing mid-hold** keeps the turn's words, and drops
//!   the reply nobody heard.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Notify;

use super::session::{hearing, quiet, rows, session, NOT_TRANSCRIBED};
use crate::realtime_chat_thread::{eventually, of_type, say, until_type};
use crate::support::gpu_world::{ANSWER, GIB};
use crate::support::realtime_audio::Asr;

#[tokio::test]
async fn a_failed_transcription_plays_the_reply_and_marks_the_row() {
    let (g, w) = hearing("local", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    // The model hears the turn and answers before the transcription fails.
    let release = Arc::new(Notify::new());
    w.asr.push(Asr::HeldStatus(
        release.clone(),
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    say(&mut ws).await;
    eventually("the model to hear the turn", || async {
        !g.world().streamed_bodies.is_empty()
    })
    .await;
    quiet(&mut ws).await;
    release.notify_one();
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );
    for t in [
        "conversation.item.input_audio_transcription.failed",
        "error",
    ] {
        assert!(of_type(&events, t).is_empty(), "{t}: {events:?}");
    }
    let user = of_type(&events, "lmgw.chat.user");
    assert_eq!(user.len(), 1, "{events:?}");
    assert_eq!(user[0]["content"], "");
    assert!(
        user[0]["voice"]["transcript_error"]
            .as_str()
            .unwrap()
            .contains("engine fell over"),
        "{user:?}"
    );
    let r = rows(&w, tid).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].2, "");
    assert!(r[0].3["transcript_error"].is_string());
    assert_eq!(r[1].2, ANSWER);

    // The next request says the turn was heard but not transcribed.
    w.asr.push(Asr::Text("Und jetzt?"));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    let body = g.world().streamed_bodies[1].clone();
    let placeholder = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "user" && m["content"] == NOT_TRANSCRIBED);
    assert!(placeholder, "{body}");
}

/// WP3 review #3: the transcription failed before the model answered from
/// the audio (its container still starting) — no model heard the turn, so
/// the failure is said as with audio input off, and the response, with no
/// words to answer, ends quietly: no row, no reply.
#[tokio::test]
async fn a_failed_transcription_before_the_model_answered_is_said_and_ends_the_turn() {
    let (g, w) = hearing("local", 24 * GIB, 30).await;
    let runs = g.gate_runs();
    let (tid, mut ws) = session(&w).await;
    w.asr.push(Asr::Status(
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let failed = of_type(
        &events,
        "conversation.item.input_audio_transcription.failed",
    );
    assert_eq!(failed.len(), 1, "said: {events:?}");
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status"], "cancelled", "{events:?}");
    assert_eq!(done["status_details"]["reason"], "transcription_failed");
    runs.send_replace(true);
    let mut all = events;
    all.extend(quiet(&mut ws).await);
    assert!(of_type(&all, "lmgw.chat.user").is_empty(), "{all:?}");
    assert!(rows(&w, tid).await.is_empty(), "nothing is written");
}

/// WP3 review #2: the thread's speech recognition went away after the
/// verdict said "hears you" (push-to-talk judges none before its first
/// turn): no transcription was attempted, so no model heard the turn — the
/// failure is said as with audio input off, and nothing is written.
#[tokio::test]
async fn a_turn_whose_speech_recognition_went_away_is_never_heard() {
    let (_g, w) = hearing("local", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    crate::realtime_chat_thread::settings(&w.state, |s| {
        s.chat_stt_alias = String::new();
        s.realtime.asr_alias = String::new();
    })
    .await;
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let failed = of_type(
        &events,
        "conversation.item.input_audio_transcription.failed",
    );
    assert_eq!(failed.len(), 1, "said: {events:?}");
    assert_eq!(failed[0]["error"]["code"], "asr_not_configured");
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status_details"]["reason"], "transcription_failed");
    let mut all = events;
    all.extend(quiet(&mut ws).await);
    assert!(of_type(&all, "lmgw.chat.user").is_empty(), "{all:?}");
    assert!(rows(&w, tid).await.is_empty(), "nothing is written");
}

/// The session ends while a heard response is still held: the turn's
/// words are written once transcribed (the session's end waits for them),
/// and the reply nobody heard is not kept.
#[tokio::test]
async fn a_session_closing_mid_hold_keeps_the_words_and_drops_the_reply() {
    let (g, w) = hearing("local", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    let release = Arc::new(Notify::new());
    w.asr
        .push(Asr::HeldText(release.clone(), "Wie spät ist es?"));
    say(&mut ws).await;
    eventually("the model to hear the turn", || async {
        !g.world().streamed_bodies.is_empty()
    })
    .await;
    quiet(&mut ws).await;
    ws.close(None).await.unwrap();
    drop(ws);
    release.notify_one();
    for _ in 0..500 {
        let r = rows(&w, tid).await;
        if r.len() == 1 && r[0].1 == "user" {
            // The reply, saved behind the row, is deleted at its finalize.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let r = rows(&w, tid).await;
    let shape: Vec<(&str, &str)> = r.iter().map(|x| (x.1.as_str(), x.2.as_str())).collect();
    assert_eq!(shape, [("user", "Wie spät ist es?")]);
    assert_eq!(r[0].3["input"], "audio");
}
