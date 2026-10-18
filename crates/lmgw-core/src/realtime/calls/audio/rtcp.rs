//! The phone's receiver reports (calls design §5.2, §5.3, §8.1): the round
//! trip, the phone's jitter and the loss it saw.
//!
//! A call sends no pings; the round trip comes from the RTCP receiver
//! reports the phone sends about lmgw's stream, by their LSR and DLSR (RFC
//! 3550 §6.4.1). str0m computes it as each report arrives and hands it
//! over in its stats event (`Event::MediaEgressStats`, every stats
//! interval, 1 s on a call), beside the report's jitter and the loss
//! fraction averaged over the reports since the last event ([`EgressStats`]
//! names the fields). Each **new** report gives:
//!
//! - a round trip for the session's `RoundTrip` (`RoundTrip::reported`): its
//!   least-of-five rule and interval bound unchanged, it feeds the barge-in
//!   margin and the playout allowance (§5.3, §8.1);
//! - the phone's interarrival jitter J for the formula
//!   (`playout::Playout::jitter`);
//! - the loss for the encoder's FEC (`encode::Encoder::loss`).
//!
//! **Which reports are new.** str0m repeats the last report's round trip and
//! jitter in every stats event until the next report arrives, so a report is
//! new when its highest sequence number moved: lmgw's stream never stops
//! (§5.1; under DTX a comfort-noise frame every 400 ms), so each report
//! names a later one.

use std::time::Duration;

/// What str0m's `MediaEgressStats` says of the phone's receiver reports:
/// `rtt`, `loss`, `remote.jitter` and `remote.maximum_sequence_number`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EgressStats {
    pub rtt: Option<Duration>,
    pub loss: Option<f32>,
    pub jitter: Option<u32>,
    pub max_seq: Option<u64>,
}

/// A new receiver report (module doc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Report {
    /// Its round trip; `None` while the phone has had no sender report of
    /// lmgw's to answer (its LSR is 0).
    pub rtt: Option<Duration>,
    /// The phone's interarrival jitter, in RTP ticks.
    pub jitter: u32,
    /// The fraction lost since the last event, when str0m had one.
    pub loss: Option<f32>,
}

/// The phone's receiver reports, as the stats events bring them.
#[derive(Debug, Default)]
pub(crate) struct PhoneReports {
    /// The highest sequence number the last report named.
    seen: Option<u64>,
}

impl PhoneReports {
    /// One stats event: the report it carries, when that is a new one.
    pub fn stats(&mut self, s: &EgressStats) -> Option<Report> {
        let (Some(seq), Some(jitter)) = (s.max_seq, s.jitter) else {
            return None;
        };
        if self.seen == Some(seq) {
            return None;
        }
        self.seen = Some(seq);
        Some(Report {
            rtt: s.rtt,
            jitter,
            loss: s.loss,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_new_receiver_report_brings_its_round_trip_jitter_and_loss() {
        let mut r = PhoneReports::default();
        let stats = |rtt: u64, seq: u64| EgressStats {
            rtt: Some(MS(rtt)),
            loss: Some(0.04),
            jitter: Some(480),
            max_seq: Some(seq),
        };
        // No report yet: str0m's event has no remote part.
        let none = EgressStats {
            rtt: None,
            loss: None,
            jitter: None,
            max_seq: None,
        };
        assert_eq!(r.stats(&none), None);
        assert_eq!(
            r.stats(&stats(80, 1_000)),
            Some(Report {
                rtt: Some(MS(80)),
                jitter: 480,
                loss: Some(0.04)
            })
        );
        // The next event repeats it: no second sample.
        assert_eq!(r.stats(&stats(80, 1_000)), None);
        assert_eq!(
            r.stats(&stats(65, 1_250)).map(|x| x.rtt),
            Some(Some(MS(65)))
        );
    }
}
