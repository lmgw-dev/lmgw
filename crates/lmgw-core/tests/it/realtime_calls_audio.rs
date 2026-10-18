//! A call's audio end to end (realtime calls design §4, §5, §13.1; WP3):
//! real Opus both ways between lmgw and str0m as the phone
//! (`realtime_calls_net::client`), over loopback UDP, on the realtime fakes
//! of `realtime_chat_thread`'s world with the plain socket group bound
//! (`realtime_calls::calls_world`).
//!
//! - `input`: speech in to a committed turn on the timeline a WebSocket
//!   session has, a lost packet recovered from FEC and a burst concealed
//!   (the timeline as long as the RTP span), the audio kept from before the
//!   open heard first;
//! - `output`: a reply's frames every 20 ms on the RTP clock with padding
//!   and `lmgw.output_audio.starved` while synthesis is late, the round trip
//!   from the phone's receiver reports, a playout report moving the playout
//!   delay, heard on a barge-in as what left by the allowance before the
//!   cut.
//!
//! The phone captures in real time: a frame each 20 ms, lmgw's packets read
//! between. Every wait is for an event (or a log line that marks a step),
//! under the harness's patience, and no assertion leans on how the
//! scheduler ran: what is timed is bounded by what the phone itself saw
//! (no frame of lmgw's arrives before its slot is due; a measured delay is
//! no shorter than the phone held the frame). Every call is made with the
//! owner's key on the plain listener.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate, Signal};
use serde_json::Value;
use str0m::rtp::rtcp::SenderInfo;
use str0m::Event;

use crate::realtime_calls::Phone;

mod input;
mod output;

/// One 20 ms frame at 24 kHz.
pub(crate) const FRAME: usize = 480;

/// `pcm` as a phone's libwebrtc encodes it, frame by frame: VoIP at 32
/// kbps, in-band FEC on with 20 % expected loss, so every packet carries
/// the frame before it. The last frame is filled with silence.
pub(crate) fn opus(pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut enc = Encoder::new(Channels::Mono, SampleRate::Hz24000, Application::Voip).unwrap();
    enc.set_bitrate(Bitrate::Value(32_000)).unwrap();
    enc.set_inband_fec(InbandFec::Mode1).unwrap();
    enc.set_packet_loss(20).unwrap();
    enc.set_signal(Signal::Voice).unwrap();
    pcm.chunks(FRAME)
        .map(|f| {
            let mut input: Vec<u16> = f.iter().map(|&s| s as u16).collect();
            input.resize(FRAME, 0);
            let mut out = vec![0u8; 1275];
            let n = enc.encode_to_slice(&input, &mut out).unwrap();
            out.truncate(n);
            out
        })
        .collect()
}

/// One frame of lmgw's audio as the phone received it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Got {
    /// Its RTP time, as the phone's str0m extended it.
    pub rtp: u64,
    /// When its first packet came off the phone's socket (str0m's
    /// `network_time`, on the phone's clock, `client::now`).
    pub arrived: Instant,
}

/// What the phone has heard of lmgw: its audio frames, the sender reports
/// about them, and the events on `oai-events` not yet looked at (the connect
/// warm's model states left out).
#[derive(Default)]
pub(crate) struct Heard {
    pub frames: Vec<Got>,
    pub reports: Vec<SenderInfo>,
    events: VecDeque<Value>,
}

impl Heard {
    /// Note `e`; an event on `oai-events` is kept for [`until`].
    fn note(&mut self, e: &Event) {
        match e {
            Event::MediaData(d) => self.frames.push(Got {
                rtp: d.time.numer(),
                arrived: d.network_time,
            }),
            Event::SenderFeedback(f) => self.reports.push(f.sender_info),
            Event::ChannelData(d) => {
                let v: Value = serde_json::from_slice(&d.data).unwrap();
                if v["type"] != "lmgw.model.state" {
                    self.events.push_back(v);
                }
            }
            _ => {}
        }
    }
}

/// Send `frames` as the phone captures: one each 20 ms, lmgw's packets read
/// in between. `lose` says which frames the network loses.
pub(crate) async fn stream(
    p: &mut Phone,
    heard: &mut Heard,
    frames: &[Vec<u8>],
    lose: impl Fn(usize) -> bool,
) {
    for (k, f) in frames.iter().enumerate() {
        if lose(k) {
            p.c.lose_next_rtp(1);
        }
        p.c.send_frame_now(f);
        p.c.run_for(Duration::from_millis(20)).await;
        p.c.met(|e| {
            heard.note(e);
            false
        });
    }
}

/// Run the phone, noting all it hears, until lmgw's next event on
/// `oai-events` for which `f` holds: the events before it, and it.
pub(crate) async fn until(
    p: &mut Phone,
    heard: &mut Heard,
    what: &str,
    f: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut seen = Vec::new();
    while let Some(v) = heard.events.pop_front() {
        let hit = f(&v);
        seen.push(v);
        if hit {
            return seen;
        }
    }
    p.c.until(what, |_, e| {
        heard.note(&e);
        while let Some(v) = heard.events.pop_front() {
            let hit = f(&v);
            seen.push(v);
            if hit {
                return Some(());
            }
        }
        None
    })
    .await;
    seen
}

/// [`until`] lmgw's next event of type `t`: that event.
pub(crate) async fn until_type(p: &mut Phone, heard: &mut Heard, t: &str) -> Value {
    let what = format!("lmgw's {t} on the call");
    let mut seen = until(p, heard, &what, |v| v["type"] == t).await;
    seen.pop().unwrap()
}
