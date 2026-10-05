//! A backchannel the gate passed near the answer's end is the user's reply
//! (realtime design §6.4, B3 review E1): "Ja" after "Soll ich das so
//! machen?" — whether its words come back after the answer played out, or
//! its speech goes on past the window's end while they are checked — is a
//! normal turn, committed and answered, and the answer is not cancelled.
//! Real time, as in `realtime_barge_words`.

use std::time::Duration;

use serde_json::{json, Value};

use crate::support::realtime_audio::{fixture, Asr};
use crate::support::realtime_fakes::{types, Turn};
use crate::support::realtime_mic::{barge_gateway, barge_session, live_mic, Player};
use crate::support::realtime_tts::{speech, wav, Tts};

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

#[tokio::test]
async fn a_backchannel_that_outlives_the_answer_is_a_reply_and_is_answered() {
    let (_state, addr, chat, tts, asr) = barge_gateway().await;
    // A 1.2 s answer; the user starts about 0.7 s in and talks for 1.4 s,
    // well past its end — and every check, and the turn, hear "Ja.".
    tts.push(Tts::Wav(wav(&speech(1200), 24_000)));
    chat.push(Turn::text(&["Soll ich das so machen?"]));
    chat.push(Turn::text(&["Gut, ", "mache ich."]));
    for _ in 0..6 {
        asr.push(Asr::Text("Ja."));
    }
    let (ws, _) = barge_session(
        &addr,
        json!({"type": "server_vad"}),
        json!({"barge_in_check": "words"}),
    )
    .await;
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
    tokio::time::sleep(Duration::from_millis(400)).await;
    mic.say(fixture("en_complete_short.wav")).await;

    // The answer finishes as generated; the speech is a turn of its own.
    let first = ear.until("response.done").await;
    assert_eq!(
        first.last().unwrap()["response"]["status"],
        "completed",
        "{:?}",
        types(&first)
    );
    let mut rest = ear.until("response.done").await;
    rest.splice(0..0, first.iter().cloned());
    assert_eq!(
        count(&rest, "input_audio_buffer.speech_started"),
        1,
        "{:?}",
        types(&rest)
    );
    let transcript = rest
        .iter()
        .find(|e| e["type"] == "conversation.item.input_audio_transcription.completed")
        .expect("the reply was committed and transcribed");
    assert_eq!(transcript["transcript"], "Ja.");
    assert_eq!(rest.last().unwrap()["response"]["status"], "completed");
    assert_eq!(chat.seen.chat_count(), 2, "the reply was answered");
    assert!(
        !rest
            .iter()
            .any(|e| e["response"]["status_details"]["reason"] == "turn_detected"),
        "nothing was cut"
    );
}
