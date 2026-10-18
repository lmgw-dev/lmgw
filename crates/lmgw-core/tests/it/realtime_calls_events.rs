//! A call's events on its data channel (realtime calls design §6, §13.1;
//! WP4), end to end over loopback UDP with str0m as the phone
//! (`realtime_calls_net::client`), on the realtime fakes of
//! `realtime_chat_thread`'s world with the plain socket group bound
//! (`realtime_calls::calls_world`):
//!
//! - `buffer` (§6.1, §6.2): no audio on `oai-events` (no
//!   `response.output_audio.delta`, `input_audio_buffer.append` refused),
//!   `session.lmgw.output_lead_ms` at 0, `output_audio_buffer.started`
//!   with the first frame and `stopped` right after a played answer's
//!   `response.done`, and a barge-in's `cleared` right after its cancelled
//!   `response.done`;
//! - `openai` (§6.2): OpenAI's stop for a stock client — `response.cancel`
//!   then `output_audio_buffer.clear`, one `cleared` — `clear` alone while an
//!   answer plays, `clear` with nothing playing, and the WebSocket's answer to
//!   `clear`;
//! - `stop` (§6.2, §5.4): `lmgw.response.stop` while an answer plays, at the
//!   phone's playout position (heard cut at the sample it names), again with
//!   nothing open, while thinking (the owed response dropped), and the
//!   `lmgw.*` client events refused on an unbound call (§6.4);
//! - `sizes` (§6.3): on a bound call a client's event split by lmgw-client's
//!   splitter joins on lmgw and lmgw's oversized answer arrives as parts that
//!   lmgw-client's joiner joins, each within the phone's limit and back to
//!   back; a broken set and one past `realtime.max_message_mb` refused; on an
//!   unbound call an oversized event with its largest fields emptied and one
//!   `event_too_large` per field, and a part refused;
//! - `measure` (§6.3, the open question): which events of a bound
//!   self-admin session are over libwebrtc's 256 KiB.
//!
//! The phone captures in real time where it speaks (a frame each 20 ms, lmgw's
//! packets read between), and every wait is for an event or a log line, under
//! the harness's patience. Calls are made with the owner's key on the plain
//! listener (the dashboard client); a bound call binds a new thread.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{json, Value};
use str0m::rtp::rtcp::SenderInfo;
use str0m::Event;

use crate::realtime_calls::{post_sdp, Phone};
use crate::realtime_calls_net::client::Client;
use crate::realtime_chat_thread::World;

mod buffer;
mod measure;
mod openai;
mod sizes;
mod stop;

/// One frame of lmgw's audio as the phone received it.
#[derive(Debug, Clone)]
pub(crate) struct Got {
    /// Its RTP time, as the phone's str0m extended it.
    pub rtp: u64,
    /// Its Opus payload.
    pub payload: Vec<u8>,
}

/// What a phone has received of lmgw: the audio frames, lmgw's sender
/// reports, and every message on `oai-events` as it came, the ones not yet
/// looked at queued.
#[derive(Default)]
pub(crate) struct Records {
    pub frames: Vec<Got>,
    pub reports: Vec<SenderInfo>,
    /// Every message, in order, as its text (the connect warm's model states
    /// left out).
    pub messages: Vec<String>,
    /// Those not yet looked at, parsed.
    queue: VecDeque<Value>,
}

impl Records {
    /// Note `e`.
    fn note(&mut self, e: &Event) {
        match e {
            Event::MediaData(d) => self.frames.push(Got {
                rtp: d.time.numer(),
                payload: d.data.to_vec(),
            }),
            Event::SenderFeedback(f) => self.reports.push(f.sender_info),
            Event::ChannelData(d) => {
                let text = String::from_utf8(d.data.clone()).unwrap();
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["type"] != "lmgw.model.state" {
                    self.messages.push(text);
                    self.queue.push_back(v);
                }
            }
            _ => {}
        }
    }

    /// The queued messages up to the first for which `f` holds, into
    /// `seen`: whether it came.
    fn take(&mut self, seen: &mut Vec<Value>, f: &impl Fn(&Value) -> bool) -> bool {
        while let Some(v) = self.queue.pop_front() {
            let hit = f(&v);
            seen.push(v);
            if hit {
                return true;
            }
        }
        false
    }
}

/// A call's phone, lmgw's answer, and what the phone has received.
pub(crate) struct Call {
    pub p: Phone,
    /// lmgw's SDP answer.
    pub answer: String,
    pub rec: Records,
}

impl Call {
    /// Run the phone, noting all it hears, until lmgw's next message for
    /// which `f` holds: the messages before it, and it.
    pub async fn until(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        if self.rec.take(&mut seen, &f) {
            return seen;
        }
        let Call { p, rec, .. } = self;
        p.c.until(what, |_, e| {
            rec.note(&e);
            rec.take(&mut seen, &f).then_some(())
        })
        .await;
        seen
    }

    /// [`Self::until`] lmgw's next message of type `t`: the messages before
    /// it, and it.
    pub async fn until_type(&mut self, t: &str) -> Vec<Value> {
        self.until(&format!("lmgw's {t} on the call"), |v| v["type"] == t)
            .await
    }

    /// The next message of type `t`.
    pub async fn next_of(&mut self, t: &str) -> Value {
        self.until_type(t).await.pop().unwrap()
    }

    /// Run the phone for `d`, noting all it hears.
    pub async fn run_for(&mut self, d: Duration) {
        self.p.c.run_for(d).await;
        let rec = &mut self.rec;
        self.p.c.met(|e| {
            rec.note(e);
            false
        });
    }

    /// Send `event` on `oai-events`.
    pub fn send(&mut self, event: Value) {
        self.p.send(event);
    }

    /// A probe — an empty `session.update` — and every message up to its
    /// `session.updated`: what lmgw said after what was sent before the
    /// probe, the probe's answer last.
    pub async fn probe(&mut self) -> Vec<Value> {
        self.send(json!({"type": "session.update", "session": {"type": "realtime"}}));
        self.until_type("session.updated").await
    }

    /// The phone captures `frames` of Opus in real time: one each 20 ms,
    /// lmgw's packets read between.
    pub async fn speak(&mut self, frames: &[Vec<u8>]) {
        for f in frames {
            self.p.c.send_frame_now(f);
            self.run_for(Duration::from_millis(20)).await;
        }
    }
}

/// A call bound to a new thread on `chatty`, from the phone `c` (its offer
/// `sdp`), connected and past its `session.created`.
pub(crate) async fn bound_with(w: &World, c: Client, sdp: &str) -> Call {
    let tid = w.thread("chatty", json!({})).await;
    let a = post_sdp(w, &w.gw.client(), &format!("chat_thread={tid}"), sdp).await;
    let answer = a.body.clone();
    connected(Phone::answered(c, a), answer).await
}

/// [`bound_with`] from a plain phone.
pub(crate) async fn bound(w: &World) -> Call {
    let (c, sdp) = Phone::offer().await;
    bound_with(w, c, &sdp).await
}

/// An unbound call on `?model=chatty` from the phone `c` (its offer `sdp`),
/// connected and past its `session.created`.
pub(crate) async fn unbound_with(w: &World, c: Client, sdp: &str) -> Call {
    let a = post_sdp(w, &w.gw.client(), "model=chatty", sdp).await;
    let answer = a.body.clone();
    connected(Phone::answered(c, a), answer).await
}

/// [`unbound_with`] from a plain phone.
pub(crate) async fn unbound(w: &World) -> Call {
    let (c, sdp) = Phone::offer().await;
    unbound_with(w, c, &sdp).await
}

async fn connected(mut p: Phone, answer: String) -> Call {
    p.c.connect().await;
    let mut call = Call {
        p,
        answer,
        rec: Records::default(),
    };
    call.until_type("session.created").await;
    call
}

/// Apply `update` (a `session.update`), past its `session.updated`.
pub(crate) async fn update(call: &mut Call, update: Value) -> Value {
    call.send(update);
    let ev = call
        .until("session.updated", |v| {
            v["type"] == "session.updated" || v["type"] == "error"
        })
        .await;
    let last = ev.last().unwrap().clone();
    assert_eq!(last["type"], "session.updated", "{ev:?}");
    last
}

/// Push-to-talk with audio out: what every speaking test here starts from.
pub(crate) fn manual() -> Value {
    json!({"type": "session.update", "session": {"type": "realtime",
        "output_modalities": ["audio"], "audio": {"input": {"turn_detection": null}}}})
}

/// A push-to-talk turn: 200 ms of silence, the commit, `response.create`.
pub(crate) async fn say(call: &mut Call) {
    let silence = crate::realtime_calls_audio::opus(&crate::support::realtime_audio::silence(200));
    call.speak(&silence).await;
    call.send(json!({"type": "input_audio_buffer.commit"}));
    call.send(json!({"type": "response.create"}));
}

/// The types of `events`, in order.
pub(crate) fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// [`types`] without lmgw's own: what a bound session says of its thread
/// (`lmgw.chat.*`, the timing line) comes when it comes.
pub(crate) fn openai_types(events: &[Value]) -> Vec<&str> {
    types(events)
        .into_iter()
        .filter(|t| !t.starts_with("lmgw."))
        .collect()
}

/// `ms` of a 440 Hz tone at 24 kHz, loud: TTS audio whose every frame the
/// phone tells from silence once it decodes it.
pub(crate) fn tone(ms: usize) -> Vec<i16> {
    (0..ms * 24)
        .map(|i| {
            let t = i as f64 / 24_000.0;
            (8_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
        })
        .collect()
}

/// Whether the frame `payload`, decoded with `dec`, carries the tone: its
/// RMS well above what Opus makes of silence.
pub(crate) fn loud(dec: &mut opusic_c::Decoder, payload: &[u8]) -> bool {
    let mut buf = [0u16; 2_880];
    let n = dec.decode_to_slice(payload, &mut buf, false).unwrap();
    let sum: f64 = buf[..n].iter().map(|&s| f64::from(s as i16).powi(2)).sum();
    (sum / n.max(1) as f64).sqrt() > 1_000.0
}
