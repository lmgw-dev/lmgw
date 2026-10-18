//! The playing window on a call: the playout allowance, the window's end,
//! drained and the barge-in margin (calls design §5.3).
//!
//! The phone plays a frame later by the one-way transit and NetEQ's delay.
//! lmgw sees the round trip (the phone's receiver reports, `rtcp`) and the
//! phone's interarrival jitter J; no RTCP report carries NetEQ's delay, so a
//! bound client reports its playout instead. **The playout allowance** is
//! NetEQ's part, and each term is counted once whatever its source:
//!
//! - **the window's end** is the last frame's send end plus the allowance —
//!   the playback record's end on a call, what the barge-in gate judges
//!   against ([`Playout::window_end`]);
//! - **drained**, which `output_audio.done` and `response.done` wait for, is
//!   the window's end plus half the round trip: the phone has played it
//!   ([`Playout::drained`]);
//! - **the barge-in margin** is the round trip plus `echo_tail_ms`, as on a
//!   WebSocket, the round trip from receiver reports: the session's own
//!   (`Core::echo_margin`), on the round trip they feed.
//!
//! **From playout reports** (`lmgw.output_audio.playout`, C10): the
//! report's `playout_ntp_ms` names the sample playing (`line::locate`); the
//! report left the phone about half a round trip before it arrived, so the
//! **playout delay** d from that sample's send to its playout is measured:
//! d = (arrival − half the round trip) − the sample's send time. The
//! allowance is d less half the round trip, and drained the last frame's
//! send end plus d. `jitter_buffer_ms`, NetEQ's own delay, is shown beside
//! it (a gap between the two is the transit's asymmetry); a report without
//! one keeps the last. The latest position holds until the next; one that
//! maps to no frame sent is ignored, with a DEBUG line naming it.
//!
//! **The formula, without reports**: one frame plus 2 × J (RFC 3550's
//! smoothed mean deviation, the first estimate of the high percentiles
//! NetEQ aims at); before the first receiver report, one frame.
//!
//! **When reports stop** — none measured a position while a whole
//! `realtime.ping_interval_s` of a response's audio played — the formula
//! takes over, said at INFO with the last report's age, and a report that
//! comes again takes over again. With the interval at 0 the last report
//! holds.
//!
//! **What the phone says is bounded** as the round trip it stands in for
//! is (`RoundTrip`'s bound): a measured delay, or a jitter whose allowance,
//! of `realtime.ping_interval_s` or more is no playout at all, and is
//! ignored with a DEBUG line, the last value kept; at 0 there is no bound.
//! Without it a phone's numbers could hold every drained marker, and the
//! response's end with it, for as long as they say.

use std::fmt;
use std::time::Duration;

use tokio::time::Instant;

use super::line::{locate, SentLine};
use super::record::{half, Record};
use super::{ticks_to_duration, FRAME};

/// Where the allowance came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The latest playout report's measured delay.
    Report,
    /// One frame plus 2 × J.
    Formula,
    /// One frame: no receiver report yet.
    Frame,
}

impl Source {
    /// As the status block says it.
    pub fn wire(self) -> lmgw_api_types::realtime::PlayoutSource {
        use lmgw_api_types::realtime::PlayoutSource;
        match self {
            Self::Report => PlayoutSource::Report,
            Self::Formula => PlayoutSource::Formula,
            Self::Frame => PlayoutSource::Frame,
        }
    }
}

/// The playout allowance and what it rests on, for the DEBUG line a change
/// of it gets (§5.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Allowance {
    pub value: Duration,
    pub source: Source,
    /// The round trip it was taken with.
    pub rtt: Option<Duration>,
    /// The measured playout delay, for [`Source::Report`].
    pub delay: Option<Duration>,
    /// What it was measured from: how long before the report arrived its
    /// sample left, and the round trip half of which it took off.
    pub measured: Option<(Duration, Option<Duration>)>,
    /// The phone's jitter J, in RTP ticks, once a receiver report said it.
    pub jitter: Option<u32>,
    /// The latest report's `jitter_buffer_ms`.
    pub jitter_buffer_ms: Option<f64>,
}

impl fmt::Display for Allowance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |d: Duration| d.as_millis();
        write!(f, "playout allowance {} ms ", ms(self.value))?;
        match (self.source, self.delay, self.jitter) {
            (Source::Report, Some(d), _) => {
                write!(f, "from the phone's playout report (delay {} ms", ms(d))?;
                match self.measured {
                    Some((since, Some(r))) => write!(
                        f,
                        ": its sample left {} ms before the report arrived, less half the round \
                         trip then, {} ms)",
                        ms(since),
                        ms(r)
                    )?,
                    Some((since, None)) => write!(
                        f,
                        ": its sample left {} ms before the report arrived, no round trip then)",
                        ms(since)
                    )?,
                    None => write!(f, ", less half the round trip)")?,
                }
            }
            (_, _, Some(j)) => write!(
                f,
                "from the formula, one frame + 2 × the phone's jitter ({j} ticks, {} ms)",
                ms(ticks_to_duration(u64::from(j)))
            )?,
            _ => write!(f, "of one frame, no receiver report yet")?,
        }
        match self.rtt {
            Some(r) => write!(f, "; round trip {} ms", ms(r))?,
            None => write!(f, "; round trip not measured yet")?,
        }
        // The formula beside the report for the same stretch (§5.3, the
        // interop rig's comparison, §13.2).
        if let (Source::Report, Some(j)) = (self.source, self.jitter) {
            write!(
                f,
                "; the formula would give {} ms (the phone's jitter {j} ticks)",
                ms(formula(j))
            )?;
        }
        if let Some(jb) = self.jitter_buffer_ms {
            write!(f, "; the phone's jitter buffer {jb:.0} ms")?;
        }
        Ok(())
    }
}

/// What a report or the passing time changed, for the call's log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Note {
    /// A report's position was not used (DEBUG).
    Ignored(String),
    /// A receiver report's jitter J was not used: the allowance it gives is
    /// not below `realtime.ping_interval_s` (module doc; DEBUG).
    JitterIgnored {
        jitter: u32,
        allowance: Duration,
        bound: Duration,
    },
    /// Reports stopped: the formula took over (INFO).
    FellBack { age: Duration },
    /// A report came again after the formula took over (INFO).
    Again,
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignored(why) => write!(f, "a playout report's position was ignored: {why}"),
            Self::JitterIgnored {
                jitter,
                allowance,
                bound,
            } => write!(
                f,
                "the phone's jitter of {jitter} ticks was ignored: the playout allowance it gives \
                 ({} ms) is not below realtime.ping_interval_s ({} s)",
                allowance.as_millis(),
                bound.as_secs()
            ),
            Self::FellBack { age } => write!(
                f,
                "no playout report for a whole realtime.ping_interval_s of playback (the last \
                 one is {} s old): the playout allowance falls back to the formula",
                age.as_secs()
            ),
            Self::Again => write!(f, "playout reports again: the allowance follows them"),
        }
    }
}

/// One call's playout (module doc).
#[derive(Debug)]
pub(crate) struct Playout {
    /// `realtime.ping_interval_s`; `None` at 0, when the last report holds.
    interval: Option<Duration>,
    /// The phone's jitter J from its receiver reports.
    jitter: Option<u32>,
    /// The latest measured playout delay d, and what it was measured from
    /// ([`Allowance::measured`]).
    delay: Option<Duration>,
    measured: Option<(Duration, Option<Duration>)>,
    jitter_buffer_ms: Option<f64>,
    /// When the latest report arrived, whatever it carried: the age of
    /// `jitter_buffer_ms`.
    arrived: Option<Instant>,
    /// When the latest report that measured d arrived.
    last: Option<Instant>,
    /// A response's audio played since then.
    played: Duration,
    /// The formula took over from the reports.
    fell_back: bool,
}

impl Playout {
    pub fn new(interval: Option<Duration>) -> Self {
        Self {
            interval,
            jitter: None,
            delay: None,
            measured: None,
            jitter_buffer_ms: None,
            arrived: None,
            last: None,
            played: Duration::ZERO,
            fell_back: false,
        }
    }

    /// The phone's jitter J, in RTP ticks, from a receiver report
    /// (`rtcp::Report::jitter`); one whose allowance would be the interval
    /// or more is ignored (module doc), and the last J kept.
    pub fn jitter(&mut self, j: u32) -> Option<Note> {
        let allowance = formula(j);
        if let Some(bound) = self.interval.filter(|b| allowance >= *b) {
            return Some(Note::JitterIgnored {
                jitter: j,
                allowance,
                bound,
            });
        }
        self.jitter = Some(j);
        None
    }

    /// A frame of a response — audio or padding — left at `now`: 20 ms more
    /// of playback since the last report. Past one interval with none, the
    /// formula takes over.
    pub fn played(&mut self, now: Instant) -> Option<Note> {
        self.played += FRAME;
        let (Some(interval), Some(last)) = (self.interval, self.last) else {
            return None;
        };
        if self.fell_back || self.played < interval {
            return None;
        }
        self.fell_back = true;
        Some(Note::FellBack {
            age: now.saturating_duration_since(last),
        })
    }

    /// A playout report arrived at `arrived`, its fields as the client sent
    /// them; `line` and `record` locate its position, `rtt` is the round
    /// trip now.
    pub fn report(
        &mut self,
        arrived: Instant,
        playout_ntp_ms: Option<f64>,
        jitter_buffer_ms: Option<f64>,
        line: Option<&SentLine>,
        record: &Record,
        rtt: Option<Duration>,
    ) -> Vec<Note> {
        let mut notes = Vec::new();
        self.arrived = Some(arrived);
        if let Some(jb) = jitter_buffer_ms.filter(|v| v.is_finite()) {
            self.jitter_buffer_ms = Some(jb);
        }
        let Some(position) = playout_ntp_ms.filter(|v| v.is_finite()) else {
            return notes;
        };
        let at = match locate(line, record, position) {
            Ok(l) => l,
            Err(why) => {
                notes.push(Note::Ignored(why.to_string()));
                return notes;
            }
        };
        let left_phone = arrived.checked_sub(half(rtt)).unwrap_or(arrived);
        let Some(delay) = left_phone.checked_duration_since(at.sent) else {
            notes.push(Note::Ignored(format!(
                "it names a sample sent {} ms after the report left the phone",
                at.sent.saturating_duration_since(left_phone).as_millis()
            )));
            return notes;
        };
        if let Some(bound) = self.interval.filter(|b| delay >= *b) {
            notes.push(Note::Ignored(format!(
                "it measures a playout delay of {} ms, not below realtime.ping_interval_s ({} s)",
                delay.as_millis(),
                bound.as_secs()
            )));
            return notes;
        }
        self.delay = Some(delay);
        self.measured = Some((arrived.saturating_duration_since(at.sent), rtt));
        self.last = Some(arrived);
        self.played = Duration::ZERO;
        if std::mem::take(&mut self.fell_back) {
            notes.push(Note::Again);
        }
        notes
    }

    /// The allowance now, with the round trip `rtt` (module doc).
    pub fn allowance(&self, rtt: Option<Duration>) -> Allowance {
        let (value, source) = match (self.reported(), self.jitter) {
            (Some(d), _) => (d.saturating_sub(half(rtt)), Source::Report),
            (None, Some(j)) => (formula(j), Source::Formula),
            (None, None) => (FRAME, Source::Frame),
        };
        Allowance {
            value,
            source,
            rtt,
            delay: self.reported(),
            measured: self.measured.filter(|_| self.reported().is_some()),
            jitter: self.jitter,
            jitter_buffer_ms: self.jitter_buffer_ms,
        }
    }

    /// Where the playing window ends for a response whose last frame's send
    /// ended at `send_end`.
    pub fn window_end(&self, send_end: Instant, rtt: Option<Duration>) -> Instant {
        send_end + self.allowance(rtt).value
    }

    /// When the phone has played a response whose last frame's send ended
    /// at `send_end`: drained.
    pub fn drained(&self, send_end: Instant, rtt: Option<Duration>) -> Instant {
        match self.reported() {
            Some(d) => send_end + d,
            None => self.window_end(send_end, rtt) + half(rtt),
        }
    }

    /// When the latest report arrived, with a position or without one.
    pub fn last_report(&self) -> Option<Instant> {
        self.arrived
    }

    /// The measured delay, while the reports hold.
    fn reported(&self) -> Option<Duration> {
        self.delay.filter(|_| !self.fell_back)
    }
}

/// The formula's allowance for the phone's jitter `j` (RTP ticks): one
/// frame plus 2 × J (module doc).
fn formula(j: u32) -> Duration {
    FRAME + 2 * ticks_to_duration(u64::from(j))
}

#[cfg(test)]
mod tests;
