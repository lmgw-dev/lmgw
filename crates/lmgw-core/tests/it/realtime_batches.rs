//! One TTS request for several sentences (realtime design §8.2, TTS batches
//! 2026-10-05; `realtime/responder/speech/batch.rs`): the first clause goes
//! alone and at once, later ones join what is queued and wait for more only
//! until the listener would run dry, a line end ends a batch, and a batch's
//! audio is shared out among its clauses — each still one transcript delta,
//! one heard-table row and one `speech` frame. And the
//! longest pause (`realtime/audio/pauses.rs`): the silence a TTS pads a
//! request with is cut down to `longest_pause_ms`. The chat upstream and the
//! TTS are fakes (`support::realtime_fakes`, `support::realtime_tts`).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::chat_voice_speak::{post, spoken, stored_reply, thread, world, Reader};
use crate::support::realtime_fakes::{events_until, next_event, send, user_text, Step, Turn, Ws};
use crate::support::realtime_tts::{
    audio_of, speech, speech_gateway, spoken_session, wav, TtsFake,
};

/// A long lead: nothing waits for the paced send, for the tests that check
/// what is requested, not when it plays.
const NO_WAIT: u32 = 60_000;

/// A user turn and a response to it, up to `response.done`.
async fn turn(ws: &mut Ws, text: &str) -> Vec<Value> {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    events_until(ws, "response.done").await
}

/// What the TTS was asked to say, request by request.
fn inputs(tts: &TtsFake) -> Vec<String> {
    (0..tts.seen.count())
        .map(|n| tts.seen.body(n)["input"].as_str().unwrap().to_string())
        .collect()
}

/// The transcript delta of each clause, as the client got it.
fn deltas(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "response.output_audio_transcript.delta")
        .map(|e| e["delta"].as_str().unwrap().to_string())
        .collect()
}

/// The text of the one transcript the response ended with.
fn transcript(events: &[Value]) -> &str {
    events
        .iter()
        .find(|e| e["type"] == "response.output_audio_transcript.done")
        .unwrap_or_else(|| panic!("no transcript.done in {events:?}"))["transcript"]
        .as_str()
        .unwrap()
}

#[tokio::test]
async fn a_paragraph_streamed_quickly_is_its_first_sentence_and_one_request_for_the_rest() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    // A second of audio per request: the listener has plenty to hear while
    // the rest of the paragraph is gathered.
    tts.set_default(wav(&speech(1000), 24_000));
    chat.push(Turn::text(&["Eins. Zwei. Drei. Vier."]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let events = turn(&mut ws, "zähl").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // The first sentence alone; the three after it as one text.
    assert_eq!(inputs(&tts), ["Eins.", "Zwei. Drei. Vier."]);
    // One transcript delta per clause, the second request's second shared
    // out among its three — and the whole text at the end.
    let d = deltas(&events);
    let trimmed: Vec<&str> = d.iter().map(|s| s.trim()).collect();
    assert_eq!(trimmed, ["Eins.", "Zwei.", "Drei.", "Vier."]);
    assert_eq!(d.concat(), "Eins. Zwei. Drei. Vier.");
    assert_eq!(transcript(&events), "Eins. Zwei. Drei. Vier.");
    let audio = events
        .iter()
        .filter(|e| e["type"] == "response.output_audio.delta")
        .count();
    // In 100 ms deltas per clause: ten for "Eins.", four for each of the
    // three clauses sharing the second second (333 ms each).
    assert_eq!(
        audio, 22,
        "two clips of 1 s, the second shared by three clauses"
    );
    assert_eq!(audio_of(&events).len(), 2 * 24_000);

    // The heard table has a row per clause: "Zwei." has the second
    // second's first third, and a cut half way into it falls half way into
    // "Drei." — "Dr", back to the last word heard whole.
    let item = events[1]["item"]["id"].as_str().unwrap().to_string();
    send(
        &mut ws,
        json!({"type": "conversation.item.truncate", "item_id": item,
               "content_index": 0, "audio_end_ms": 1500}),
    )
    .await;
    let truncated = next_event(&mut ws).await;
    assert_eq!(
        truncated["type"], "conversation.item.truncated",
        "{truncated}"
    );
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved", "{got}");
    assert_eq!(got["item"]["content"][0]["transcript"], "Eins. Zwei.");
}

#[tokio::test]
async fn paragraphs_and_list_items_stay_apart() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    chat.push(Turn::text(&[
        "Erst das. Dann das.\n\nNeuer Absatz. Und mehr.\n- Punkt eins.\n- Punkt zwei.",
    ]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let events = turn(&mut ws, "liste").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // The first sentence alone; "Dann das." begins a batch, which the
    // paragraph's next clause ends; the paragraph's two sentences are one
    // text (a batch is one paragraph's at most); every list item is a
    // batch of its own — no request spans a line end.
    assert_eq!(
        inputs(&tts),
        [
            "Erst das.",
            "Dann das.",
            "Neuer Absatz. Und mehr.",
            "Punkt eins.",
            "Punkt zwei."
        ]
    );
    // One transcript delta per clause, as said (the list markers are no
    // text).
    let d: Vec<String> = deltas(&events).iter().map(|s| s.trim().into()).collect();
    assert_eq!(
        d,
        [
            "Erst das.",
            "Dann das.",
            "Neuer Absatz.",
            "Und mehr.",
            "Punkt eins.",
            "Punkt zwei."
        ],
        "{d:?}"
    );
    // 300 ms of audio each.
    assert_eq!(audio_of(&events).len(), 5 * 7200);
}

#[tokio::test]
async fn the_speaker_does_not_wait_past_the_listener_s_audio_for_more_text() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    // 600 ms of audio per request: the listener runs dry 600 ms after the
    // first one went out, less twice what a request takes.
    tts.set_default(wav(&speech(600), 24_000));
    let release = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::Text("Eins. Zwei. "),
        // The model stalls; the stream has not ended.
        Step::Wait(release.clone()),
        Step::Text("Drei."),
        Step::Finish("stop"),
    ]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    send(&mut ws, user_text("zähl")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;

    // "Eins." goes at once; "Zwei." is gathered for more — and, with the
    // model still silent, goes when the listener's audio is about to run
    // out. The stream is held the whole time: only the deadline can have
    // sent it.
    for _ in 0..400 {
        if tts.seen.count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        inputs(&tts),
        ["Eins.", "Zwei."],
        "the second sentence was held back past the first one's audio"
    );
    let times = tts.seen.times.lock().unwrap().clone();
    let waited = times[1].duration_since(times[0]);
    assert!(
        waited >= Duration::from_millis(250),
        "it waited for more text before sending it ({waited:?})"
    );

    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(inputs(&tts), ["Eins.", "Zwei.", "Drei."]);
    assert_eq!(transcript(&events), "Eins. Zwei. Drei.");
    assert_eq!(audio_of(&events).len(), 3 * 14_400);
}

#[tokio::test]
async fn a_stored_reply_is_read_aloud_as_its_first_sentence_and_the_rest_at_once() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Eins. Zwei. Drei."]));
    let mid = stored_reply(&w.gw, tid, "zähl").await;

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    // The first alone and at once; the rest is all there already, and the
    // text has ended, so it joins and nothing is waited for — one frame per
    // clause still.
    assert_eq!(spoken(&events), ["Eins.", "Zwei.", "Drei."]);
    assert_eq!(
        (0..w.tts.seen.count())
            .map(|n| w.tts.seen.body(n)["input"].clone())
            .collect::<Vec<_>>(),
        ["Eins.", "Zwei. Drei."]
    );
    let done = &events.last().unwrap().1;
    assert_eq!(done["stopped"], false, "{done}");
    // 300 ms of audio per request, not per sentence.
    assert_eq!(done["audio_ms"], 600, "{done}");
}

/// A second of sound with a second of silence before it and after it: what
/// a TTS that pads its answers would say.
fn padded() -> Vec<i16> {
    let quiet = vec![0i16; 24_000];
    let sound: Vec<i16> = (0..24_000)
        .map(|i| if i % 48 < 24 { 12_000 } else { -12_000 })
        .collect();
    [quiet.clone(), sound, quiet].concat()
}

/// `samples` at 24 kHz, in milliseconds.
fn ms(samples: usize) -> i64 {
    (samples / 24) as i64
}

/// Within a few 10 ms windows of `want`.
#[track_caller]
fn assert_about(got: i64, want: i64, what: &str) {
    assert!(
        (got - want).abs() <= 30,
        "{what}: {got} ms, wanted {want} ms"
    );
}

#[tokio::test]
async fn the_longest_pause_cuts_the_silence_around_a_request_s_sound() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&padded(), 24_000));
    chat.push(Turn::text(&["Hallo."]));
    chat.push(Turn::text(&["Hallo."]));

    // Two sessions at once, to wait for the paced end of both together: the
    // default, 400 ms — half of it kept before the sound and half after —
    // and 0, which keeps the engine's silences.
    let heard = |extra: Value, setting: u64| {
        let addr = addr.clone();
        async move {
            let (mut ws, updated) = spoken_session(&addr, &[], NO_WAIT, extra).await;
            assert_eq!(updated["session"]["lmgw"]["longest_pause_ms"], setting);
            let events = turn(&mut ws, "hi").await;
            assert_eq!(events.last().unwrap()["response"]["status"], "completed");
            ms(audio_of(&events).len())
        }
    };
    let (default, off) = tokio::join!(
        heard(json!({}), 400),
        heard(
            json!({"lmgw": {"output_lead_ms": NO_WAIT, "longest_pause_ms": 0}}),
            0
        )
    );
    assert_about(default, 1400, "the default");
    assert_about(off, 3000, "longest_pause_ms 0");
    assert_eq!(tts.seen.count(), 2);
}

/// The Chat has no setting of its own: its read-aloud takes the owner-wide
/// `realtime.longest_pause_ms`.
#[tokio::test]
async fn a_read_aloud_takes_the_owner_wide_longest_pause() {
    for (setting, want) in [(None, 1400), (Some(0), 3000), (Some(200), 1200)] {
        let w = world(|s| {
            if let Some(ms) = setting {
                s.realtime.longest_pause_ms = ms;
            }
        })
        .await;
        w.tts.set_default(wav(&padded(), 24_000));
        let tid = thread(&w.gw, "chatty").await;
        w.chat.push(Turn::text(&["Hallo."]));
        let mid = stored_reply(&w.gw, tid, "hi").await;
        let r = post(
            &w.gw,
            &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
            json!({}),
        )
        .await;
        let events = Reader::new(r).rest().await;
        let pcm: Vec<usize> = events
            .iter()
            .filter(|(e, _)| e == "speech")
            .map(|(_, d)| {
                base64::engine::general_purpose::STANDARD
                    .decode(d["pcm"].as_str().unwrap())
                    .unwrap()
                    .len()
                    / 2
            })
            .collect();
        assert_eq!(pcm.len(), 1, "{events:?}");
        assert_about(ms(pcm[0]), want, &format!("longest_pause_ms {setting:?}"));
    }
}
