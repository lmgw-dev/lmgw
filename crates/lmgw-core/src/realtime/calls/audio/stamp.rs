//! Placing a call's input on the wall clock (calls design §4.3).
//!
//! The barge-in gate judges input by when it was captured (realtime §6.4).
//! A WebSocket append is stamped with its arrival, which `AudioIn`'s
//! timeline reads as the capture of its **last** sample, late by the
//! client's transit (`audio_in::timeline`). A call's arrival carries the
//! network's jitter and the reorder window's wait as well, so a frame is
//! stamped from its RTP time instead:
//!
//! - its end's RTP offset from the stream's anchor (its first frame, or the
//!   frame after a discontinuity, §4.2),
//! - plus the **transit floor**: the least `arrival − offset` over the last
//!   [`FLOOR_HORIZON`].
//!
//! So a frame's stamp is its last sample's capture plus the least transit,
//! what a WebSocket stamp means, without the jitter; it is never later than
//! the frame's own arrival. Offsets are taken at each frame's end because
//! the timeline reads a stamp as the last sample's, so a concealed run is
//! stamped at its end too ([`Stamps::place`]).

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::ticks_to_duration;

/// How far back the transit floor looks (§4.3): long enough to ride out a
/// burst of jitter, short enough to follow a changed path and the drift
/// between the phone's and the host's crystals (tens of ppm, milliseconds a
/// minute). An estimator's horizon, as realtime §6.4's 0.8 s evidence gap
/// is: it bounds, drops and delays nothing.
pub(crate) const FLOOR_HORIZON: Duration = Duration::from_secs(10);

/// The stream's anchor: an RTP time and when its frame arrived.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    rtp: u64,
    arrived: Instant,
}

/// One call's stamps (module doc).
#[derive(Debug, Default)]
pub(crate) struct Stamps {
    anchor: Option<Anchor>,
    /// `(arrival, transit)` of the frames within the horizon whose transit
    /// no later frame undercuts, oldest first: the front is the floor (a
    /// sliding minimum). Transit is `arrival − offset` in nanoseconds from
    /// the anchor's arrival, and may be negative.
    floor: VecDeque<(Instant, i128)>,
}

impl Stamps {
    /// The stream starts at RTP time `rtp`, whose frame arrived at
    /// `arrived`: the first frame, or the one after a discontinuity. The
    /// floor starts over with it.
    pub fn anchor(&mut self, rtp: u64, arrived: Instant) {
        self.anchor = Some(Anchor { rtp, arrived });
        self.floor.clear();
    }

    /// Whether the stream has an anchor yet.
    #[cfg(test)]
    pub fn anchored(&self) -> bool {
        self.anchor.is_some()
    }

    /// A received frame whose last sample ends at RTP time `end` arrived at
    /// `arrived`: it joins the floor, and its stamp is answered. Without an
    /// anchor it anchors the stream at itself.
    pub fn received(&mut self, end: u64, arrived: Instant) -> Instant {
        let anchor = *self.anchor.get_or_insert(Anchor { rtp: end, arrived });
        let since = signed(arrived, anchor.arrived);
        let transit = since - offset(anchor, end);
        while self.floor.back().is_some_and(|&(_, t)| t >= transit) {
            self.floor.pop_back();
        }
        self.floor.push_back((arrived, transit));
        if let Some(oldest) = arrived.checked_sub(FLOOR_HORIZON) {
            while self.floor.front().is_some_and(|&(at, _)| at < oldest) {
                self.floor.pop_front();
            }
        }
        let floor = self.floor.front().map_or(transit, |&(_, t)| t);
        shift(anchor.arrived, offset(anchor, end) + floor)
    }

    /// The stamp of the sample ending at RTP time `end` — a concealed run's
    /// end, or a received frame's — from the floor as it stands. Before any
    /// frame it is `None`.
    #[cfg(test)]
    pub fn place(&self, end: u64) -> Option<Instant> {
        let anchor = self.anchor?;
        let floor = self.floor.front().map_or(0, |&(_, t)| t);
        Some(shift(anchor.arrived, offset(anchor, end) + floor))
    }
}

/// `end`'s offset from the anchor, in nanoseconds; an RTP time before the
/// anchor is at it.
fn offset(anchor: Anchor, end: u64) -> i128 {
    ticks_to_duration(end.saturating_sub(anchor.rtp)).as_nanos() as i128
}

/// `a − b` in signed nanoseconds.
fn signed(a: Instant, b: Instant) -> i128 {
    if a >= b {
        (a - b).as_nanos() as i128
    } else {
        -((b - a).as_nanos() as i128)
    }
}

/// `at` moved by `nanos`, either way; an instant before the clock's origin
/// stays at `at`.
fn shift(at: Instant, nanos: i128) -> Instant {
    let d = Duration::from_nanos(u64::try_from(nanos.unsigned_abs()).unwrap_or(u64::MAX));
    if nanos >= 0 {
        at + d
    } else {
        at.checked_sub(d).unwrap_or(at)
    }
}

#[cfg(test)]
mod tests;
