//! Audio out, end to end (realtime design §2.3, §4.3, §8, §9.1, §11, §16
//! "golden sequences"): the streaming chat fake's text cut into clauses,
//! each spoken by the TTS fake with LJSpeech-derived audio, and paced back.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use lmgw_core::config::{PriceScope, PriceUnit};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::realtime::audio::pcm::fade_edges;
use lmgw_core::state::SharedState;
use lmgw_core::store;

use crate::support::realtime_fakes::{
    captured_client_frames, events_until, next_event, send, types, user_text, Step, Turn, Ws,
};
use crate::support::realtime_tts::{
    add_cloud_tts_alias, audio_of, speech, speech_22k, speech_gateway, spoken_session, tts_fake,
    tts_rows, wav, Tts, TtsFake, TTS_ALIAS, TTS_MODEL,
};

/// A long lead: nothing waits, for the tests that check what, not when.
const NO_WAIT: u32 = 60_000;

/// A user turn and a response to it, up to `response.done`.
async fn turn(ws: &mut Ws, text: &str) -> Vec<Value> {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    events_until(ws, "response.done").await
}

fn assert_part_ref(ev: &Value) {
    for k in ["response_id", "item_id"] {
        assert!(ev[k].as_str().is_some_and(|s| !s.is_empty()), "{k} in {ev}");
    }
    for k in ["output_index", "content_index"] {
        assert!(ev[k].is_u64(), "{k} in {ev}");
    }
}

#[tokio::test]
async fn an_audio_response_is_the_golden_sequence() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    chat.push(Turn::text(&["Hello there. ", "How are ", "you today?"]));
    let (mut ws, updated) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let resolved = &updated["session"]["lmgw"]["resolved"];
    assert_eq!(resolved["tts"], "speak", "{updated}");
    // No voice named: `marin`, an OpenAI name the model lacks → the setting.
    assert_eq!(resolved["voice"], "alba");
    assert_eq!(updated["session"]["lmgw"]["output_lead_ms"], NO_WAIT);

    let events = turn(&mut ws, "hi").await;
    let mut want = vec![
        "response.created",
        "response.output_item.added",
        "conversation.item.added",
        "response.content_part.added",
    ];
    for _ in 0..2 {
        // Each clause: its transcript, then 300 ms of audio in 100 ms deltas.
        want.push("response.output_audio_transcript.delta");
        want.extend(["response.output_audio.delta"; 3]);
    }
    want.extend([
        "response.output_audio.done",
        "response.output_audio_transcript.done",
        "response.content_part.done",
        "response.output_item.done",
        "conversation.item.done",
        "response.done",
    ]);
    assert_eq!(types(&events), want);

    let rid = events[0]["response"]["id"].as_str().unwrap();
    let item = &events[1]["item"];
    let iid = item["id"].as_str().unwrap();
    assert_eq!(
        (item["type"].as_str(), item["role"].as_str()),
        (Some("message"), Some("assistant"))
    );
    assert_eq!(item["status"], "in_progress");
    assert_eq!(
        events[3]["part"],
        json!({"type": "audio", "transcript": ""})
    );
    for ev in &events[3..events.len() - 3] {
        assert_part_ref(ev);
        assert_eq!(
            (ev["response_id"].as_str(), ev["item_id"].as_str()),
            (Some(rid), Some(iid))
        );
        assert_eq!(
            (ev["output_index"].as_u64(), ev["content_index"].as_u64()),
            (Some(0), Some(0))
        );
    }
    for ev in &events {
        assert!(
            ev["event_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("event_")),
            "{ev}"
        );
    }
    let transcript = "Hello there. How are you today?";
    let deltas: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_audio_transcript.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, transcript, "the deltas add up to the transcript");
    let n = events.len();
    assert_eq!(events[n - 6]["type"], "response.output_audio.done");
    assert_eq!(events[n - 5]["transcript"], transcript);
    assert_eq!(
        events[n - 4]["part"],
        json!({"type": "audio", "transcript": transcript})
    );
    let done_item = &events[n - 3]["item"];
    assert_eq!(done_item["status"], "completed");
    assert_eq!(
        done_item["content"],
        json!([{"type": "output_audio", "transcript": transcript}])
    );
    assert_eq!(events[n - 2]["item"], *done_item);

    // The response object: audio out, with the voice as a string — the
    // stock client's schema wants one.
    for r in [&events[0]["response"], &events[n - 1]["response"]] {
        assert_eq!(r["output_modalities"], json!(["audio"]));
        assert_eq!(
            r["audio"],
            json!({"output": {"format": {"type": "audio/pcm", "rate": 24000}, "voice": "marin"}})
        );
    }
    let done = &events[n - 1]["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["usage"]["total_tokens"], 20);
    assert_eq!(done["output"][0], *done_item);

    // The audio is the TTS answer at 24 kHz, each clause faded in and out
    // over 5 ms (§23 L11): two clauses' worth, not a sample more or less.
    let mut clip = speech(300);
    fade_edges(&mut clip, 120);
    assert_eq!(audio_of(&events), [clip.clone(), clip].concat());
    // What the engine was asked: the upstream model, each clause, the voice
    // and a WAV.
    assert_eq!(tts.seen.count(), 2);
    for (n, text) in ["Hello there.", "How are you today?"].iter().enumerate() {
        assert_eq!(
            tts.seen.body(n),
            json!({"model": TTS_MODEL, "input": text, "voice": "alba", "response_format": "wav"})
        );
    }
    // One TTS row for the response, not one per clause — carrying the
    // characters the clauses were sent, on a local row too, at a real 0.
    assert_eq!(tts_rows(&state).await, [(200, None)]);
    assert_eq!(chars_sent(&tts, 2), 30);
    let row = tts_row(&state, TTS_ALIAS).await;
    assert_eq!(
        row,
        (Some(30), Some(0), Some("free_local".into())),
        "(chars_in, cost_micro, price_source)"
    );
}

// --- Billable units (billable-units design §4.3, §4.5) ------------------------

/// The characters of the first `n` clauses `tts` was sent, as sent.
fn chars_sent(tts: &TtsFake, n: usize) -> i64 {
    (0..n)
        .map(|i| tts.seen.body(i)["input"].as_str().unwrap().chars().count() as i64)
        .sum()
}

/// The one TTS row of `alias`, once written: `(chars_in, cost_micro,
/// price_source)`.
async fn tts_row(state: &SharedState, alias: &str) -> (Option<i64>, Option<i64>, Option<String>) {
    for _ in 0..200 {
        let rows: Vec<(Option<i64>, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT chars_in, cost_micro, price_source FROM request_logs WHERE class = 'audio' \
             AND requested_alias = ?1 ORDER BY id",
        )
        .bind(alias)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if !rows.is_empty() {
            assert_eq!(rows.len(), 1, "one row per response: {rows:?}");
            return rows.into_iter().next().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no TTS row for {alias}");
}

/// A cloud TTS alias `cloud-tts` priced at 15 per 1M characters and 0.001
/// per request, as `tts-1` would be with a request fee.
async fn priced_cloud_tts(state: &SharedState) -> TtsFake {
    let cloud = tts_fake(&[]).await;
    add_cloud_tts_alias(state, &cloud, "cloud-tts").await;
    for (unit, rate) in [(PriceUnit::PerMchar, 15.0), (PriceUnit::PerRequest, 0.001)] {
        let sheet = Prices {
            source: PriceSource::Manual,
            ..Default::default()
        };
        store::upsert_price(
            &state.db,
            PriceScope::Alias,
            "cloud-tts",
            unit,
            &sheet,
            Some(rate),
            None,
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    cloud
}

async fn cloud_session(addr: &str) -> Ws {
    let lmgw = json!({"lmgw": {"output_lead_ms": NO_WAIT, "tts_model": "cloud-tts"}});
    spoken_session(addr, &[], NO_WAIT, lmgw).await.0
}

/// A binary WAV per clause carries no usage, and the row is still priced:
/// the characters as sent, 15 per 1M, plus one fee per answered clause.
#[tokio::test]
async fn a_spoken_answer_pays_for_its_characters_and_its_clauses() {
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    let cloud = priced_cloud_tts(&state).await;
    chat.push(Turn::text(&["Hello there. ", "How are ", "you today?"]));
    let mut ws = cloud_session(&addr).await;
    let events = turn(&mut ws, "hi").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    assert_eq!(cloud.seen.count(), 2);
    let chars = chars_sent(&cloud, 2);
    assert_eq!(chars, 30);
    let (chars_in, cost, source) = tts_row(&state, "cloud-tts").await;
    assert_eq!(chars_in, Some(chars));
    assert_eq!(cost, Some(chars * 15 + 2 * 1_000));
    assert_eq!(source.as_deref(), Some("manual"));
}

/// A clause the upstream refused was not billed, and leaves what was
/// answered known: the first clause's characters and one fee.
#[tokio::test]
async fn a_refused_clause_leaves_the_answered_ones_priced() {
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    let cloud = priced_cloud_tts(&state).await;
    cloud.push(Tts::Wav(wav(&speech(300), 24_000)));
    cloud.push(Tts::Status(
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    chat.push(Turn::text(&["First. ", "Second."]));
    let mut ws = cloud_session(&addr).await;
    let events = turn(&mut ws, "hi").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");

    let first = chars_sent(&cloud, 1);
    let (chars_in, cost, _) = tts_row(&state, "cloud-tts").await;
    assert_eq!(chars_in, Some(first));
    assert_eq!(cost, Some(first * 15 + 1_000));
}

/// A cancel while a clause is with the upstream leaves it unknown whether
/// the provider took it: the row's characters and requests are unknown,
/// and the row unpriced — never the answered clauses alone.
#[tokio::test]
async fn a_cancel_with_a_clause_in_flight_leaves_the_row_unpriced() {
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    let cloud = priced_cloud_tts(&state).await;
    let held = Arc::new(Notify::new());
    cloud.push(Tts::Wav(wav(&speech(300), 24_000)));
    cloud.push(Tts::Held(held.clone(), wav(&speech(300), 24_000)));
    chat.push(Turn::text(&["First.\n", "Second.\n", "Third."]));
    let mut ws = cloud_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_audio.delta").await;
    for _ in 0..100 {
        if cloud.seen.count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send(&mut ws, json!({"type": "response.cancel"})).await;
    events_until(&mut ws, "response.done").await;

    let (chars_in, cost, _) = tts_row(&state, "cloud-tts").await;
    assert_eq!(chars_in, None, "the second clause was in flight");
    assert_eq!(cost, None, "unknown is NULL, never the answered part");
    held.notify_one();
}

#[test]
fn a_fade_takes_the_click_off_a_clause_and_keeps_its_length() {
    // §23 L11: German Pocket clauses start at full amplitude at sample 0.
    let mut loud = vec![20_000i16; 1000];
    fade_edges(&mut loud, 120);
    assert_eq!(loud.len(), 1000);
    assert_eq!((loud[0], loud[999]), (0, 0));
    // Rising (and falling) monotonically to the untouched middle.
    assert!(loud[..120].windows(2).all(|w| w[0] <= w[1]), "{loud:?}");
    assert!(loud[880..].windows(2).all(|w| w[0] >= w[1]), "{loud:?}");
    assert!(loud[120..880].iter().all(|&s| s == 20_000));
    assert_eq!(loud[60], 10_000, "half way up at half the fade");
    // A clip shorter than two fades is faded over half of it each way.
    let mut short = vec![-8_000i16; 10];
    fade_edges(&mut short, 120);
    assert_eq!((short[0], short[9]), (0, 0));
    assert!(short[4] < 0 && short[5] < 0);
    let mut none: Vec<i16> = Vec::new();
    fade_edges(&mut none, 120);
    let mut one = vec![5i16];
    fade_edges(&mut one, 120);
    assert_eq!(one, [5], "nothing to fade");
}

#[tokio::test]
async fn a_voice_at_another_rate_is_resampled_to_24k() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let native = speech_22k(300);
    assert_eq!(native.len(), 6615);
    tts.set_default(wav(&native, 22_050));
    chat.push(Turn::text(&["One. ", "Two."]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let events = turn(&mut ws, "count").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // Exactly ceil(6615 · 24000 / 22050) = 7200 samples per clause.
    assert_eq!(audio_of(&events).len(), 2 * 7200);
}

/// Every event until `response.done`, each with the time it arrived — read
/// on its own task so the arrival times are true while the test does other
/// things.
fn timed_reader(mut ws: Ws) -> tokio::sync::mpsc::UnboundedReceiver<(Instant, Value)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                let done = v["type"] == "response.done";
                let _ = tx.send((Instant::now(), v));
                if done {
                    break;
                }
            }
        }
    });
    rx
}

#[tokio::test]
async fn audio_leaves_paced_and_the_voice_is_free_while_it_plays() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    // Two clauses of 600 ms: 1.2 s of audio, 200 ms of lead.
    tts.set_default(wav(&speech(600), 24_000));
    chat.push(Turn::text(&["Hello there. ", "How are you?"]));
    let (mut ws, _) = spoken_session(&addr, &[], 200, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let mut rx = timed_reader(ws);

    let mut events = Vec::new();
    let first = loop {
        let (at, ev) = rx.recv().await.unwrap();
        let audio = ev["type"] == "response.output_audio.delta";
        events.push((at, ev));
        if audio {
            break at;
        }
    };
    // Both clauses are synthesized in milliseconds, and the voice's one row
    // — written when its hold is dropped — is there long before the audio
    // has played (§9.1).
    let mut rows = Vec::new();
    for _ in 0..40 {
        rows = tts_rows(&state).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rows, [(200, None)]);
    let row_at = first.elapsed();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    let audio: Vec<Duration> = events
        .iter()
        .filter(|(_, e)| e["type"] == "response.output_audio.delta")
        .map(|(at, _)| at.duration_since(first))
        .collect();
    assert_eq!(audio.len(), 12);
    // The lead at once; the last chunk once the rest has played (1000 ms
    // after the first, give or take the scheduler).
    assert!(audio[1] < Duration::from_millis(150), "{audio:?}");
    assert!(audio[11] >= Duration::from_millis(900), "{audio:?}");
    assert!(audio[11] < Duration::from_millis(3000), "{audio:?}");
    assert!(
        row_at < audio[11],
        "the row ({row_at:?}) came after playback ({audio:?})"
    );
    // The closing events wait for the paced end.
    let (done_at, done) = events.last().unwrap();
    assert_eq!(done["type"], "response.done");
    assert!(done_at.duration_since(first) >= audio[11], "{audio:?}");
    let types: Vec<&str> = events
        .iter()
        .map(|(_, e)| e["type"].as_str().unwrap())
        .collect();
    let audio_done = types
        .iter()
        .position(|t| *t == "response.output_audio.done")
        .unwrap();
    let last_audio = types
        .iter()
        .rposition(|t| *t == "response.output_audio.delta")
        .unwrap();
    assert!(audio_done > last_audio, "{types:?}");
}

#[tokio::test]
async fn longest_pause_ms_is_echoed_at_the_setting_and_a_session_may_set_its_own() {
    let (_s, addr, _chat, _tts) = speech_gateway(false, None, |_| {}).await;
    let mut ws =
        crate::support::realtime_fakes::open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["longest_pause_ms"], 400);
    drop(ws);
    let (_ws, updated) = spoken_session(
        &addr,
        &[],
        0,
        json!({"lmgw": {"output_lead_ms": 0, "longest_pause_ms": 0}}),
    )
    .await;
    assert_eq!(updated["session"]["lmgw"]["longest_pause_ms"], 0);
}

#[tokio::test]
async fn synthesis_stays_within_synthesis_ahead_s_of_the_paced_send() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    // The setting's default is echoed, and a session may set its own.
    let mut ws =
        crate::support::realtime_fakes::open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["synthesis_ahead_s"], 30);
    drop(ws);
    let (mut ws, updated) = spoken_session(
        &addr,
        &[],
        0,
        json!({"lmgw": {"output_lead_ms": 0, "synthesis_ahead_s": 1}}),
    )
    .await;
    assert_eq!(updated["session"]["lmgw"]["synthesis_ahead_s"], 1);
    // Six clauses of 300 ms, synthesized in milliseconds each: with a
    // second allowed ahead and no lead, the fifth and sixth wait for the
    // paced send to catch up rather than going to the engine at once. One
    // sentence to a line: each clause is its own TTS request, none joins
    // another across a line (`speech/batch.rs`).
    chat.push(Turn::text(&[
        "One.\n", "Two.\n", "Three.\n", "Four.\n", "Five.\n", "Six.",
    ]));
    let events = turn(&mut ws, "count").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(audio_of(&events).len(), 6 * 7200, "nothing dropped");
    let times = tts.seen.times.lock().unwrap().clone();
    assert_eq!(times.len(), 6);
    let spread = times[5].duration_since(times[0]);
    assert!(spread >= Duration::from_millis(250), "{spread:?}");
}

/// Send a retrieve of `item` and collect everything up to its answer: what
/// the writer still sent after the events before it.
async fn until_retrieved(ws: &mut Ws, item: &str) -> (Vec<Value>, Value) {
    send(
        ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    let mut before = Vec::new();
    loop {
        let ev = next_event(ws).await;
        if ev["type"] == "conversation.item.retrieved" {
            return (before, ev);
        }
        before.push(ev);
    }
}

#[tokio::test]
async fn a_cancel_purges_the_queued_audio_and_the_item_keeps_what_left() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.set_default(wav(&speech(600), 24_000));
    chat.push(Turn::text(&["Hello there. ", "How are you?"]));
    // No lead: one 100 ms chunk leaves, the next only 100 ms later.
    let (mut ws, _) = spoken_session(&addr, &[], 0, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let mut events = events_until(&mut ws, "response.output_audio.delta").await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    events.extend(events_until(&mut ws, "response.done").await);

    let iid = events[1]["item"]["id"].as_str().unwrap().to_string();
    // The audio part closes before the item — `@openai/agents` resets its
    // audio counters only on `output_audio.done` (WP3 review m4).
    let tail: Vec<&str> = types(&events).into_iter().rev().take(6).collect();
    assert_eq!(
        tail,
        [
            "response.done",
            "conversation.item.done",
            "response.output_item.done",
            "response.content_part.done",
            "response.output_audio_transcript.done",
            "response.output_audio.done",
        ]
    );
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "client_cancelled");
    // The item closes incomplete with what left for the listener: one or
    // two chunks of the first clause, cut by character — at most four of
    // "Hello there."'s twelve, inside its first word, so no word of it
    // (§7.3: the heard part ends at the last word heard whole).
    let item = &events[events.len() - 3]["item"];
    assert_eq!(item["status"], "incomplete");
    let heard = item["content"][0]["transcript"]
        .as_str()
        .unwrap()
        .to_string();
    let sent = audio_of(&events).len() as u64;
    assert!(sent > 0 && sent <= 2 * 2400, "{sent}");
    let chars = (12 * sent / 14_400) as usize;
    assert!(chars < "Hello".len(), "{chars}");
    assert_eq!(heard, "");
    // The part's done events carry the heard transcript too.
    let n = events.len();
    assert_eq!(events[n - 5]["transcript"], heard);
    assert_eq!(
        events[n - 4]["part"],
        json!({"type": "audio", "transcript": heard})
    );
    for e in &events[n - 6..n - 3] {
        assert_eq!(e["item_id"], iid.as_str(), "{e}");
    }

    // Nothing more of it leaves, and the stored item says the same.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (after, retrieved) = until_retrieved(&mut ws, &iid).await;
    assert!(after.is_empty(), "{after:?}");
    assert_eq!(retrieved["item"]["content"][0]["transcript"], heard);
    // One row for the response — `canceled` if the cancel caught the second
    // clause still with the engine; a 200 either way.
    let rows = tts_rows(&state).await;
    assert_eq!(rows, [(200, None)]);
}

#[tokio::test]
async fn a_cancel_mid_synthesis_writes_the_one_row_and_stops_the_rest() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let held = Arc::new(Notify::new());
    tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    tts.push(Tts::Held(held.clone(), wav(&speech(300), 24_000)));
    // One sentence to a line: each clause is its own TTS request, none joins
    // another across a line (`speech/batch.rs`) — so "Third." is a request
    // of its own that the count below shows was never made.
    chat.push(Turn::text(&["First.\n", "Second.\n", "Third."]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_audio.delta").await;
    // The second clause is with the engine: the cancel ends the wait.
    for _ in 0..100 {
        if tts.seen.count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "cancelled");
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = tts_rows(&state).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // One row, `canceled` — a 200, like every stop's row — and the work it
    // did still counts (§11).
    assert_eq!(rows, [(200, None)]);
    let (kind,): (Option<String>,) = sqlx::query_as(
        "SELECT error_kind FROM request_logs WHERE class = 'audio' AND requested_alias = ?1",
    )
    .bind(TTS_ALIAS)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("canceled"));
    // The second clause was with the engine: whether it took it is unknown,
    // so the row's characters are too — not the first clause's alone.
    let (chars_in, _, _) = tts_row(&state, TTS_ALIAS).await;
    assert_eq!(chars_in, None);
    held.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(tts.seen.count(), 2, "the third clause was never asked for");
    assert_eq!(tts_rows(&state).await.len(), 1);
}

#[tokio::test]
async fn a_failed_voice_fails_the_response_and_stops_the_stream() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let release = Arc::new(Notify::new());
    tts.push(Tts::Status(
        500,
        json!({"error": {"message": "requires a session voice"}}),
    ));
    chat.push(Turn::Stream(vec![
        Step::Text("Hello there. "),
        Step::Wait(release.clone()),
        Step::Text("more."),
        Step::Finish("stop"),
    ]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let events = turn(&mut ws, "hi").await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requires a session voice"),
        "{error}"
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    // The stream was stopped rather than waited for.
    for _ in 0..100 {
        if chat
            .seen
            .closed_early
            .load(std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the chat stream was not stopped");
}

#[tokio::test]
async fn a_voice_failing_mid_answer_closes_the_audio_part_before_the_item() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    tts.push(Tts::Status(
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    chat.push(Turn::text(&["First. ", "Second."]));
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    let events = turn(&mut ws, "hi").await;
    let t = types(&events);
    assert_eq!(
        t[t.len() - 7..],
        [
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "error",
            "response.done",
        ],
        "{t:?}"
    );
    let n = events.len();
    // What was said before the failure is what the part and the item keep.
    assert_eq!(events[n - 6]["transcript"], "First.");
    assert_eq!(events[n - 4]["item"]["status"], "incomplete");
    assert_eq!(events[n - 4]["item"]["content"][0]["transcript"], "First.");
    assert_eq!(events[n - 1]["response"]["status"], "failed");
}

#[tokio::test]
async fn a_spoken_function_call_round_trip_as_agents_js_sends_it() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let release = Arc::new(Notify::new());
    chat.push(Turn::Stream(vec![
        Step::Text("Let me check. "),
        Step::CallStart {
            index: 0,
            id: Some("call_x1"),
            name: "get_time",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"tz":"Europe/Berlin"}"#,
        },
        Step::Finish("tool_calls"),
        // Generation is over; the stream's end is not — and the preamble is
        // still playing when the follow-up lands.
        Step::Wait(release.clone()),
        Step::Usage(6, 4),
    ]));
    chat.push(Turn::text(&["Es ist ", "12 Uhr."]));

    // `@openai/agents` with an explicit URL, exactly as captured: audio out,
    // no voice, its tool.
    let mut ws = crate::support::realtime_fakes::open(&addr, "/v1/realtime", &[]).await;
    next_event(&mut ws).await;
    let mut frames = captured_client_frames("agents_js_fc.json").into_iter();
    let mut first = frames.next().unwrap();
    first["session"]["model"] = json!("chatty");
    first["session"]["lmgw"] = json!({"output_lead_ms": NO_WAIT});
    send(&mut ws, first).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(&mut ws, frames.next().unwrap()).await; // {type, tracing}
    next_event(&mut ws).await;
    send(&mut ws, frames.next().unwrap()).await; // "Hallo"
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, frames.next().unwrap()).await; // response.create 1

    // The spoken preamble's item first, then the call — closed `completed`
    // at the end of generation, while the preamble's item is still open.
    let mut events = events_until(&mut ws, "conversation.item.done").await;
    let t = types(&events);
    assert_eq!(
        t[..4],
        [
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.content_part.added"
        ]
    );
    let call_added = t
        .iter()
        .rposition(|x| *x == "response.output_item.added")
        .unwrap();
    assert!(call_added > 4, "{t:?}");
    assert_eq!(events[call_added]["output_index"], 1);
    assert_eq!(events[call_added]["item"]["type"], "function_call");
    let call_done = &events[events.len() - 2];
    assert_eq!(call_done["type"], "response.output_item.done");
    assert_eq!(call_done["item"]["status"], "completed");
    assert_eq!(call_done["item"]["call_id"], "call_x1");

    // The client answers at once; its follow-up is queued, not refused.
    send(&mut ws, frames.next().unwrap()).await;
    send(&mut ws, frames.next().unwrap()).await;
    events.extend(events_until(&mut ws, "conversation.item.done").await);
    release.notify_one();
    events.extend(events_until(&mut ws, "response.done").await);
    let first_done = &events.last().unwrap()["response"];
    assert_eq!(first_done["status"], "completed");
    assert_eq!(
        first_done["output"][0]["content"][0]["transcript"],
        "Let me check."
    );
    assert_eq!(first_done["output"][0]["status"], "completed");
    assert_eq!(first_done["output"][1]["type"], "function_call");
    assert_eq!(first_done["audio"]["output"]["voice"], "marin");

    events.extend(events_until(&mut ws, "response.done").await);
    assert!(
        !events.iter().any(|e| e["type"] == "error"),
        "no event was refused: {events:?}"
    );
    let completed = events
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .unwrap()
                .starts_with("response.output_item")
                && e["item"]["call_id"] == "call_x1"
                && e["item"]["status"] == "completed"
        })
        .count();
    assert_eq!(completed, 1, "exactly one event runs the tool");
    let second = &events.last().unwrap()["response"];
    assert_eq!(second["status"], "completed");
    assert_eq!(
        second["output"][0]["content"][0]["transcript"],
        "Es ist 12 Uhr."
    );
    // The follow-up rendered the spoken preamble and the call's result.
    let roles: Vec<String> = chat.seen.chat(1)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    assert_eq!(chat.seen.chat(1)["messages"][2]["content"], "Let me check.");
    // One clause spoken by each response.
    assert_eq!(tts.seen.count(), 2);
    assert_eq!(tts.seen.body(1)["input"], "Es ist 12 Uhr.");
}
