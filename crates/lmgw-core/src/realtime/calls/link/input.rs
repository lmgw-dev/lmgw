//! A call's input on its way to the session (calls design §4): the
//! reader's side of the audio parts.
//!
//! - **Decode** (§4.1, §4.2): each frame str0m depacketized is decoded at
//!   24 kHz mono onto the one timeline (`audio::decode`), a gap filled from
//!   FEC and concealment, a jump past the wall clock's bound said at INFO,
//!   a late frame dropped and counted at DEBUG, a payload libopus cannot read
//!   treated as lost. Each frame is placed on the wall clock by its RTP time
//!   and the transit floor (`audio::stamp`, §4.3), and goes on as
//!   `Incoming::Audio { pcm, captured }`.
//! - **The commit window** (§4.4, `audio::commit`): the reader holds a
//!   commit and a clear, and every event behind them, until the audio they
//!   cut has arrived; every other event goes on at once. *(Changed
//!   2026-10-15: in the reader, not the session loop.)* The reader meets the
//!   frames and the events in the one order the driver met them, so its own
//!   lag never reorders them, and its deadline is checked against what
//!   already came off the socket ([`Input::due`]); the End takes what waits
//!   ([`Input::flush`]).
//! - **What comes first** (§4.6): the frames kept since DTLS connected go
//!   through the same decode ahead of the live ones, so the timeline begins
//!   at the first frame.
//! - **The capture offset** (§4.4, §13.2; `audio::capture`): how long after
//!   its capture each frame arrived, by the client's sender reports, said at
//!   DEBUG every few seconds beside `realtime.webrtc_commit_window_ms`.
//!
//! **Flow** (§4.5): nothing here is dropped for the session's pace. The
//! reader reads ahead what the inbox's budget allows, as for events; past
//! it the frames wait undecoded with the driver, counted against the call's
//! budget of what waits for its owner (`realtime.max_message_mb`, §3.7), and
//! the liveness verdict bounds a session stuck behind output that cannot
//! leave (§8.1).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

use super::super::audio::capture::CaptureOffsets;
use super::super::audio::commit::{Cut, Order};
use super::super::audio::decode::{Decode, Gap, InFrame, Outcome};
use super::super::handle::{Frame, SenderReport};
use crate::realtime::link::Incoming;

/// What the reader's input takes from the settings (`realtime.webrtc_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputParams {
    /// `realtime.webrtc_reorder_ms`: part of a gap's wall-clock bound
    /// (§4.2) and of a cut's wait (§4.4).
    pub reorder: Duration,
    /// `realtime.webrtc_commit_window_ms` (§4.4).
    pub commit_window: Duration,
}

impl Default for InputParams {
    fn default() -> Self {
        let s = crate::config::WebRtcAudioSettings::default();
        Self {
            reorder: s.reorder(),
            commit_window: s.commit_window(),
        }
    }
}

/// One thing for the inbox, in order: what it is, its stamp, and its size
/// against the read-ahead.
pub(super) struct Pass {
    pub what: Incoming,
    pub at: Instant,
    pub len: usize,
}

/// A call's input (module doc).
pub(super) struct Input {
    id: Arc<str>,
    /// `None`: libopus made no decoder, said once at ERROR; the frames go.
    decode: Option<Decode>,
    order: Order<Pass>,
    capture: CaptureOffsets,
    /// `realtime.webrtc_commit_window_ms`, for the capture line.
    commit_window: Duration,
}

impl Input {
    pub fn new(id: Arc<str>, p: InputParams) -> Self {
        let decode = match Decode::new(p.reorder) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::error!("WebRTC call {id}: {e}; its audio is not heard");
                None
            }
        };
        Self {
            id,
            decode,
            order: Order::new(p.commit_window, p.reorder),
            capture: CaptureOffsets::default(),
            commit_window: p.commit_window,
        }
    }

    /// The client's RTCP sender report: what the capture line maps through.
    pub fn sender_report(&mut self, sr: &SenderReport) {
        self.capture.report(sr);
    }

    /// An event's message (`Incoming::Event` or `Binary`) that came off the
    /// socket at `at`: what goes on now, in order — it, or nothing while it
    /// or an event before it waits for audio (module doc).
    pub fn event(&mut self, what: Incoming, at: Instant, len: usize) -> Vec<Pass> {
        let cut = match &what {
            Incoming::Event(text) => cut_of(text.as_str()),
            _ => Cut::Other,
        };
        self.order.event(Pass { what, at, len }, cut, at)
    }

    /// A frame: the events it releases, then its audio (module doc).
    pub fn frame(&mut self, f: &Frame) -> Vec<Pass> {
        let Some(decode) = self.decode.as_mut() else {
            return Vec::new();
        };
        let outcome = decode.frame(InFrame {
            rtp: f.rtp_time,
            arrived: f.arrived,
            contiguous: f.contiguous,
            seq: f.seq.0,
            payload: &f.payload,
        });
        let d = match outcome {
            Outcome::Audio(d) => d,
            Outcome::Late(late) => {
                tracing::debug!("WebRTC call {}: {late}", self.id);
                return Vec::new();
            }
            Outcome::Invalid(why) => {
                tracing::debug!(
                    "WebRTC call {}: a frame libopus cannot read ({why}): treated as lost",
                    self.id
                );
                return Vec::new();
            }
        };
        match &d.gap {
            Some(g @ Gap::Discontinuity { .. }) => {
                tracing::info!("WebRTC call {}: {g}", self.id);
            }
            Some(g @ Gap::Filled { .. }) => tracing::debug!("WebRTC call {}: {g}", self.id),
            None => {}
        }
        // The frame's own samples, at 24 kHz, are what follows the gap's.
        let filled = match d.gap {
            Some(Gap::Filled { concealed, fec, .. }) => concealed + fec,
            Some(Gap::Discontinuity { concealed, .. }) => concealed,
            None => 0,
        };
        let own = (d.pcm.len() as u64).saturating_sub(filled);
        let ticks = own * u64::from(super::super::audio::RTP_RATE)
            / u64::from(super::super::audio::PCM_RATE);
        if let Some(s) = self.capture.frame(f.rtp_time, ticks, f.arrived) {
            tracing::debug!(
                "WebRTC call {}: the client's frames arrived {:.0}/{:.0}/{:.0} ms (least/median/\
                 most of {}) after their last sample's capture, by its sender reports; \
                 realtime.webrtc_commit_window_ms is {} ms (meaningful where the client's clock \
                 is lmgw's, as on one host)",
                self.id,
                s.least,
                s.median,
                s.most,
                s.frames,
                self.commit_window.as_millis()
            );
        }
        // The frame's place in the order is judged on the socket's clock
        // (module doc): it came off the socket at its arrival.
        let (mut out, late) = self.order.frame(d.captured, f.arrived);
        if let Some(late) = late {
            tracing::debug!("WebRTC call {}: {late}", self.id);
        }
        let len = d.pcm.len() * 2;
        out.push(Pass {
            what: Incoming::Audio {
                pcm: d.pcm,
                captured: d.captured,
            },
            at: f.arrived,
            len,
        });
        out
    }

    /// When the first waiting event stops waiting, for the reader's timer.
    pub fn deadline(&self) -> Option<Instant> {
        self.order.deadline()
    }

    /// The time is `now`, and nothing more came off the socket before it:
    /// the events whose wait is over.
    pub fn due(&mut self, now: Instant) -> Vec<Pass> {
        self.order.due(now)
    }

    /// The End (§8.1): everything waiting, at once.
    pub fn flush(&mut self) -> Vec<Pass> {
        self.order.flush()
    }
}

/// What an event is to the commit window, by its `type`.
fn cut_of(text: &str) -> Cut {
    #[derive(Deserialize)]
    struct Typed<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    match serde_json::from_str::<Typed<'_>>(text)
        .ok()
        .and_then(|t| t.kind)
        .as_deref()
    {
        Some("input_audio_buffer.commit") => Cut::Commit,
        Some("input_audio_buffer.clear") => Cut::Clear,
        _ => Cut::Other,
    }
}

#[cfg(test)]
mod tests;
