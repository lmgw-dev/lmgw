//! A live microphone for the barge-in suites (realtime design §6.4, §16):
//! the session's socket split in two — a task that appends 20 ms of audio
//! every 20 ms of wall-clock time, silence unless the test gives it
//! something to say, and sends the test's other events in between; and the
//! reading half, on which the test plays the client.
//!
//! Real time matters here: the server judges input against the client's
//! playing window by when each append arrived (§6.4), so a test that
//! uploaded its speech at once would be judged at its arrival.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::time::Duration;

use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use lmgw_core::realtime::audio::pcm::encode_pcm16;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::realtime_audio::{add_asr_alias, asr_fake, AsrFake, ASR_ALIAS};
use super::realtime_fakes::{next_event, open, send, ChatFake, Ws};
use super::realtime_tts::{speech_gateway, TtsFake};

/// One append: 20 ms at 24 kHz.
const CHUNK_MS: u64 = 20;

enum Cmd {
    /// Say this next; answer where on the input timeline it starts (ms).
    Say(Vec<i16>, oneshot::Sender<u64>),
    Send(Value),
    /// Fire this long before the append that carries this point of the
    /// input timeline (ms) is sent.
    Ahead(u64, Duration, oneshot::Sender<()>),
}

/// The writing half: a microphone in real time, and the client's events.
pub struct Mic {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Mic {
    /// Queue `pcm` behind what the microphone still has to say; where on
    /// the input timeline (ms) it starts.
    pub async fn say(&self, pcm: Vec<i16>) -> u64 {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Say(pcm, tx)).unwrap();
        rx.await.unwrap()
    }

    /// Send a client event, between two appends.
    pub fn send(&self, event: Value) {
        self.tx.send(Cmd::Send(event)).unwrap();
    }

    /// Wait until `lead` before the append that carries `at_ms` of the
    /// input timeline is sent — while the microphone still records it.
    pub async fn ahead_of(&self, at_ms: u64, lead: Duration) {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Ahead(at_ms, lead, tx)).unwrap();
        rx.await.unwrap()
    }
}

/// The reading half.
pub struct Ear {
    rx: SplitStream<Ws>,
}

impl Ear {
    /// The next event and when it arrived; fails after 5 s of silence.
    pub async fn next(&mut self) -> (Instant, Value) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.rx.next())
                .await
                .expect("the server said nothing for 5 s")
                .expect("the socket closed")
                .expect("the socket failed");
            match msg {
                Message::Text(t) => {
                    return (Instant::now(), serde_json::from_str(t.as_str()).unwrap())
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("expected a text event, got {other:?}"),
            }
        }
    }

    /// Every event up to and including the first of type `until`.
    pub async fn until(&mut self, until: &str) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            let (_, ev) = self.next().await;
            let done = ev["type"] == until;
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    /// Every event that arrives within `wait`.
    pub async fn quiet_for(&mut self, wait: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        let end = Instant::now() + wait;
        while let Ok(Some(Ok(m))) = tokio::time::timeout_at(end, self.rx.next()).await {
            if let Message::Text(t) = m {
                out.push(serde_json::from_str(t.as_str()).unwrap());
            }
        }
        out
    }
}

/// Split a session into a live microphone and the client's ear.
pub fn live_mic(ws: Ws) -> (Mic, Ear) {
    live_mic_every(ws, CHUNK_MS)
}

/// [`live_mic`] appending `chunk_ms` of audio every `chunk_ms` — a client
/// that sends larger appends, as openai-python's examples do (100 ms).
pub fn live_mic_every(ws: Ws, chunk_ms: u64) -> (Mic, Ear) {
    let (mut sink, rx) = ws.split();
    let (tx, mut cmds) = mpsc::unbounded_channel::<Cmd>();
    let chunk = (chunk_ms * 24) as usize;
    let period = Duration::from_millis(chunk_ms);
    tokio::spawn(async move {
        let mut queue: VecDeque<i16> = VecDeque::new();
        let mut appended: u64 = 0;
        let mut next = Instant::now() + period;
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Say(pcm, at)) => {
                        let start = appended + queue.len() as u64;
                        queue.extend(pcm);
                        let _ = at.send(start / 24);
                    }
                    Some(Cmd::Send(v)) => {
                        if sink.send(Message::text(v.to_string())).await.is_err() {
                            return;
                        }
                    }
                    Some(Cmd::Ahead(at_ms, lead, done)) => {
                        // The append carrying `at_ms` is the k-th from now.
                        let k = (at_ms * 24).saturating_sub(appended) / chunk as u64;
                        let sent = next + period * k as u32;
                        tokio::spawn(async move {
                            tokio::time::sleep_until(sent.checked_sub(lead).unwrap_or(sent)).await;
                            let _ = done.send(());
                        });
                    }
                    None => return,
                },
                () = tokio::time::sleep_until(next) => {
                    next += period;
                    let chunk: Vec<i16> =
                        (0..chunk).map(|_| queue.pop_front().unwrap_or(0)).collect();
                    appended += chunk.len() as u64;
                    let ev = json!({"type": "input_audio_buffer.append",
                                    "audio": encode_pcm16(&chunk)});
                    if sink.send(Message::text(ev.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    (Mic { tx }, Ear { rx })
}

/// The chat, TTS and ASR fakes behind one gateway: `realtime.tts_alias`,
/// `default_voice` `alba` and `asr_alias` set.
pub async fn barge_gateway() -> (SharedState, String, ChatFake, TtsFake, AsrFake) {
    let asr = asr_fake().await;
    let (state, addr, chat, tts) = speech_gateway(false, None, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
    })
    .await;
    add_asr_alias(&state, &asr).await;
    (state, addr, chat, tts, asr)
}

/// The knobs the barge-in suites run with: 200 ms of lead, a 300 ms guard
/// and a 600 ms post-interrupt window — short, so the suites are — and the
/// gate's duration rule alone: the word check (`realtime_barge_words`)
/// would take the scripted ASR answers meant for the turns.
pub fn knobs() -> Value {
    json!({"output_lead_ms": 200, "barge_in_guard_ms": 300, "post_interrupt_silence_ms": 600,
           "barge_in_check": "duration"})
}

/// A spoken session on `chatty` with transcription events, `turn_detection`
/// as given and [`knobs`] merged with `lmgw`; past its `session.updated`,
/// which is returned.
pub async fn barge_session(addr: &str, turn_detection: Value, lmgw: Value) -> (Ws, Value) {
    let mut ws = open(addr, "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    let mut ext = knobs();
    for (k, v) in lmgw.as_object().unwrap() {
        ext[k] = v.clone();
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "output_modalities": ["audio"],
               "audio": {"input": {"transcription": {}, "turn_detection": turn_detection}},
               "lmgw": ext}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    (ws, updated)
}

/// The client's side of playback, as `@openai/agents` keeps it (§2.3): when
/// the first audio delta of the current item arrived, which item, and how
/// much audio it received.
#[derive(Default)]
pub struct Player {
    pub item: Option<String>,
    pub response: Option<String>,
    pub first: Option<Instant>,
    pub received_ms: f64,
}

impl Player {
    /// Account for one event that arrived `at`.
    pub fn hear(&mut self, at: Instant, ev: &Value) {
        if ev["type"] == "response.output_audio.delta" {
            if self.first.is_none() {
                self.first = Some(at);
                self.received_ms = 0.0;
            }
            self.item = ev["item_id"].as_str().map(str::to_string);
            self.response = ev["response_id"].as_str().map(str::to_string);
            let b = lmgw_core::realtime::audio::pcm::decode_pcm16(ev["delta"].as_str().unwrap())
                .unwrap();
            self.received_ms += b.len() as f64 / 24.0;
        }
        if ev["type"] == "response.output_audio.done" {
            self.first = None;
        }
    }

    /// The SDK's `truncate` on `speech_started`: the wall-clock time since
    /// the first delta, capped at the audio received, floored — `None` once
    /// `output_audio.done` reset its state (its interrupt is a no-op then).
    pub fn truncate(&self, now: Instant) -> Option<Value> {
        let first = self.first?;
        let elapsed = now.duration_since(first).as_secs_f64() * 1000.0;
        let audio_end_ms = elapsed.min(self.received_ms).floor() as u64;
        Some(json!({"type": "conversation.item.truncate",
                    "item_id": self.item.clone()?, "content_index": 0,
                    "audio_end_ms": audio_end_ms}))
    }
}
