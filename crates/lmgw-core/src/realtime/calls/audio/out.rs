//! A call's audio out, on its frame clock (calls design §5.1–§5.3): the
//! task that ticks every 20 ms for the call's whole life.
//!
//! Each tick takes the next 20 ms from the writer's paced queue
//! (`writer::Pulled::tick`: the frame, the record, the playing window,
//! under the queue's lock), sends a response's `output_audio_buffer.started`
//! with its first frame, the transcript deltas it released and, on a bound
//! call, `lmgw.output_audio.starved` on the data channel (from
//! inside the tick; the writer stops the clock before the session's last
//! events, and its tick then ends the task), encodes
//! the frame ([`encode`](super::encode)) and writes it to the driver with
//! its slot's due time as str0m's wallclock and its RTP time — so a late
//! tick sends late with the right timestamp, and every sender report lmgw
//! sends lies on the clock's line (§5.3). Its events are queued with the
//! driver without a wait: the clock never waits on the data channel (§6.3).
//!
//! **A late tick** takes every slot it missed, but skips those due longer
//! ago than the playout allowance (`Pulled::skip_stale`, §5.5): the phone's
//! NetEQ would discard them, so they are never sent and never count as
//! played; the audio due in them leaves in the frames after, the first of
//! which carries the marker bit, and a DEBUG line counts them.
//!
//! **What it hears** (`handle::Feedback`): each new receiver report of the
//! phone's (`rtcp::PhoneReports`) gives the session's round trip
//! (`RoundTrip::reported`, the barge-in margin and the allowance), the
//! phone's jitter J (the formula) and the loss the encoder's FEC follows;
//! each sender report lmgw sent gives the line playout positions map
//! through (`line::SentLine`).

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::clock::{FrameClock, Slot};
use super::encode::{Encoded, Encoder, OutputBitrate};
use super::line::SentLine;
use super::playout::Note;
use super::rtcp::PhoneReports;
use super::FRAME_TICKS;
use crate::realtime::calls::handle::{AudioLink, Feedback, RtpOut};
use crate::realtime::writer::Pulled;

/// What a call's audio out takes from the settings and the offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutParams {
    /// `realtime.webrtc_output_kbps`, capped by the phone's
    /// `maxaveragebitrate` (§5.2).
    pub bitrate: OutputBitrate,
    /// The phone's offer asked for DTX (`usedtx=1`, §5.2).
    pub dtx: bool,
    /// The call is bound to a Chat thread: it says `starved` (§6.4).
    pub bound: bool,
    /// `realtime.ping_interval_s`: playout reports stop counting after one
    /// interval of playback without one (§5.3); `None` at 0.
    pub interval: Option<Duration>,
}

/// Start the call's frame clock on `queue` (module doc): its first frame
/// is due now, at a random RTP time (RFC 3550 §5.1).
pub(crate) fn spawn(link: AudioLink, queue: Pulled, p: OutParams) -> JoinHandle<()> {
    let start = Instant::now();
    let rtp0 = u64::from(rand::random::<u32>());
    queue.begin(start, rtp0, p.interval);
    tokio::spawn(run(link, queue, p, FrameClock::new(start, rtp0)))
}

async fn run(mut link: AudioLink, queue: Pulled, p: OutParams, mut clock: FrameClock) {
    let id = link.id().to_string();
    let mut encoder = match Encoder::new(p.bitrate, p.dtx) {
        Ok(e) => Some(e),
        Err(e) => {
            // The queue is still pulled, so every response ends as if
            // played; the phone hears nothing.
            tracing::error!("WebRTC call {id}: {e}; its audio out is silent");
            None
        }
    };
    tracing::info!(
        "WebRTC call {id}: audio out as Opus at {}, {}",
        p.bitrate,
        if p.dtx {
            "DTX on (the phone's usedtx=1)"
        } else {
            "no DTX (the phone did not ask for it)"
        }
    );
    let mut reports = PhoneReports::default();
    // Slots were skipped since the last frame sent: the next one carries the
    // marker bit, as after a DTX pause.
    let mut resumed = false;
    loop {
        tokio::time::sleep_until(clock.next_due()).await;
        let now = Instant::now();
        if let Some(f) = link.feedback() {
            heard(&id, f, &mut reports, &queue, encoder.as_mut(), &clock);
        }
        let slots: Vec<Slot> = std::iter::from_fn(|| clock.take(now)).collect();
        let Some(first) = slots.first() else {
            continue;
        };
        // A late tick: the slots the phone would discard are skipped (§5.5).
        let Some((stale, allowance)) = queue.skip_stale(&slots, now) else {
            return;
        };
        if stale > 0 {
            resumed = true;
            tracing::debug!(
                "WebRTC call {id}: a tick {} ms late skipped {stale} frames due longer ago than \
                 the playout allowance ({} ms), which the phone would discard: never sent",
                now.saturating_duration_since(first.due).as_millis(),
                allowance.as_millis()
            );
        }
        for slot in slots.into_iter().skip(stale) {
            // Its events go to the driver from inside the tick, so none can
            // follow the session's last ones once the writer stopped the
            // clock.
            let Some(t) = queue.tick(slot, now, p.bound, |text| {
                link.event(text);
            }) else {
                return;
            };
            if let Some(s) = &t.starved {
                // On any call, bound or not: the underrun's start and end.
                let what = if s.starved {
                    "ran dry: padded with silence"
                } else {
                    "came again"
                };
                tracing::debug!(
                    "WebRTC call {id}: response {}'s audio {what}",
                    s.response_id
                );
            }
            for n in t.notes {
                note(&format!("WebRTC call {id}"), &n);
            }
            if let Some(a) = t.allowance {
                tracing::debug!("WebRTC call {id}: {a}");
            }
            if let Some(p) = t.playout {
                link.playout(p);
            }
            let Some(enc) = encoder.as_mut() else {
                continue;
            };
            match enc.encode(&t.pcm) {
                Ok(Encoded::Send { payload, talkspurt }) => {
                    let after_skip = std::mem::take(&mut resumed);
                    let frame = RtpOut {
                        wallclock: slot.due,
                        rtp: slot.rtp,
                        payload,
                        talkspurt: talkspurt || after_skip,
                    };
                    if !link.write(frame) {
                        return;
                    }
                }
                Ok(Encoded::Skip) => {}
                Err(e) => tracing::debug!("WebRTC call {id}: {e}; the frame is not sent"),
            }
        }
    }
}

/// What the call heard on its sending side (module doc).
fn heard(
    id: &str,
    f: Feedback,
    reports: &mut PhoneReports,
    queue: &Pulled,
    encoder: Option<&mut Encoder>,
    clock: &FrameClock,
) {
    if let Some(r) = f.egress.as_ref().and_then(|e| reports.stats(e)) {
        if let Some(rtt) = r.rtt {
            queue.reported(rtt);
        }
        if let Some(n) = queue.jitter(r.jitter) {
            note(&format!("WebRTC call {id}"), &n);
        }
        if let (Some(loss), Some(enc)) = (r.loss, encoder) {
            if let Err(e) = enc.loss(loss) {
                tracing::debug!("WebRTC call {id}: {e}");
            }
        }
    }
    if let Some(s) = f.sent {
        // The last RTP time written: the report's 32 bits are matched to it.
        let near = clock.next_rtp().saturating_sub(FRAME_TICKS);
        queue.line(SentLine::new(s.ntp, s.rtp, near));
    }
}

/// A playout note, at its level (§5.3): an ignored position or jitter at
/// DEBUG, the fallback and its end at INFO.
pub(crate) fn note(who: &str, n: &Note) {
    match n {
        Note::Ignored(_) | Note::JitterIgnored { .. } => tracing::debug!("{who}: {n}"),
        Note::FellBack { .. } | Note::Again => tracing::info!("{who}: {n}"),
    }
}
