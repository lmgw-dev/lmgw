//! Half duplex at the answer's edge (realtime design §6.4, live run 2 H1),
//! in real time with 500 ms appends: a user who goes on talking just as
//! the answer starts. The turn that cancels the answer is not ended by that
//! answer's window — neither by the frames of the append it started in nor
//! by those after the cut, which still fall inside the window's margin.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{fixture, Asr};
use crate::support::realtime_fakes::{types, Turn};
use crate::support::realtime_mic::{barge_gateway, barge_session, live_mic_every};
use crate::support::realtime_tts::{wav, Tts};

const QUESTION: &str = "Where is the nearest station?";

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

#[tokio::test]
async fn a_turn_starting_just_before_the_answer_plays_is_not_cut_short_by_it() {
    let (_s, addr, chat, tts, asr) = barge_gateway().await;
    // The answer's audio is held until the test lets it go.
    let release = Arc::new(Notify::new());
    tts.push(Tts::Held(
        release.clone(),
        wav(&fixture("en_two_sentences_pause.wav"), 24_000),
    ));
    chat.push(Turn::text(&["Trains leave every hour."]));
    chat.push(Turn::text(&["Two ", "blocks."]));
    asr.push(Asr::Text(QUESTION));
    let (ws, _) = barge_session(
        &addr,
        json!({"type": "server_vad"}),
        json!({"half_duplex": true}),
    )
    .await;
    let (mic, mut ear) = live_mic_every(ws, 500);
    mic.send(json!({"type": "response.create"}));
    ear.until("response.created").await;

    // The question's speech starts 300 ms into the clip: 200 ms of silence
    // before it put its onset at the start of an append.
    let mut pcm = vec![0i16; 24 * 200];
    pcm.extend(fixture("en_complete_short.wav"));
    let clip = mic.say(pcm).await;
    let onset = clip + 500;
    // The answer's first audio leaves while that append is still being
    // recorded, 300 ms before it is sent: the turn's onset (~130 ms in) is
    // captured before the window opens, the rest of the append inside it.
    mic.ahead_of(onset, Duration::from_millis(300)).await;
    release.notify_one();

    let cut = ear.until("response.done").await;
    let done = &cut.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled", "{:?}", types(&cut));
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    let turn = ear.until("response.done").await;
    let all: Vec<Value> = cut.iter().chain(&turn).cloned().collect();
    // One turn, the whole sentence: it ends after the speech (1738 ms of
    // the clip, 200 ms of padding before it), on the post-interrupt window
    // — not at the window's first frame, with a fragment committed.
    assert_eq!(
        count(&all, "input_audio_buffer.speech_started"),
        1,
        "{:?}",
        types(&all)
    );
    let stopped = all
        .iter()
        .find(|e| e["type"] == "input_audio_buffer.speech_stopped")
        .expect("the turn ended");
    let end_ms = stopped["audio_end_ms"].as_u64().unwrap();
    assert!(
        end_ms >= clip + 200 + 1738,
        "{end_ms} vs the clip at {clip}"
    );
    assert_eq!(asr.seen.count(), 1, "one commit");
    assert_eq!(turn.last().unwrap()["response"]["status"], "completed");
    assert_eq!(count(&turn, "response.created"), 1);
}
