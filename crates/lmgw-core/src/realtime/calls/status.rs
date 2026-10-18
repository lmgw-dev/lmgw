//! The status block's `webrtc` (calls design §9): the switch as the start
//! read it, each socket group with the rule that binds it, its bound
//! addresses or why it is not bound, and the live calls — each with its
//! state, who made it, its candidate and remote, the link's figures from
//! the client's receiver reports, and the playout allowance with its source
//! and the latest playout report.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use lmgw_api_types::realtime as wire;
use tokio::time::Instant;

use super::audio::RTP_RATE;
use super::{CallInfo, CallState, Calls, Group, GroupState};
use crate::principal::Principal;

impl Group {
    /// The rule that binds the group, naming its settings (§3.1).
    fn rule(self) -> &'static str {
        match self {
            Self::Plain => {
                "bind_addr's address, on realtime.webrtc_port (null: the HTTP port's \
                 number), bound at the start beside the plain listener"
            }
            Self::Devices => {
                "the device listener's addresses and port (devices.listen, devices.port), \
                 bound, moved and closed with it"
            }
        }
    }
}

impl Group {
    fn wire(self) -> wire::WebRtcGroup {
        match self {
            Group::Plain => wire::WebRtcGroup::Plain,
            Group::Devices => wire::WebRtcGroup::Devices,
        }
    }
}

/// `at` on the wall clock, RFC 3339 UTC: what passed since it, back from now.
fn wall(at: Instant, now: Instant) -> String {
    let then = SystemTime::now() - now.saturating_duration_since(at);
    chrono::DateTime::<chrono::Utc>::from(then).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `v` to `places` decimals: the figures are measurements, and the float
/// noise past that precision tells a reader nothing (they reach LLMs
/// through `lmgw__status`).
fn round(v: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (v * scale).round() / scale
}

/// How long ago `at` was, in whole milliseconds.
fn age(at: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(at).as_millis() as u64
}

/// One live call as the status block shows it.
fn call(c: &CallInfo, now: Instant) -> wire::WebRtcCallStatus {
    let playout = c.playout.as_ref();
    // A closing call's client may be gone, and what it last reported is no
    // longer the link: the link's figures are shown while the call lives.
    let link = (c.state != CallState::Closing).then_some(&c.stats);
    wire::WebRtcCallStatus {
        id: c.id.to_string(),
        group: c.group.wire(),
        state: match c.state {
            CallState::Answered => wire::WebRtcCallState::Answered,
            CallState::Connected => wire::WebRtcCallState::Connected,
            CallState::Open => wire::WebRtcCallState::Open,
            CallState::Pending => wire::WebRtcCallState::Pending,
            CallState::Running => wire::WebRtcCallState::Running,
            CallState::Closing => wire::WebRtcCallState::Closing,
        },
        held_by: c.held_by.clone(),
        session: c.session.clone(),
        key: match &c.creator {
            Principal::Key { name, .. } => Some(name.clone()),
            Principal::Anonymous => None,
        },
        candidate: c.candidate.to_string(),
        remote: c.remote.map(|r| r.to_string()),
        connected_at: c.connected.map(|t| wall(t, now)),
        round_trip_ms: link
            .and(playout)
            .and_then(|p| p.round_trip)
            .map(|r| r.as_millis() as u64),
        egress_loss: link
            .and_then(|l| l.egress_loss)
            .map(|v| round(f64::from(v), 3)),
        ingress_loss: link
            .and_then(|l| l.ingress_loss)
            .map(|v| round(f64::from(v), 3)),
        client_jitter_ms: link
            .and_then(|l| l.client_jitter)
            .map(|j| round(f64::from(j) * 1000.0 / f64::from(RTP_RATE), 1)),
        receiver_report_age_ms: link.and_then(|l| l.report_at).map(|t| age(t, now)),
        allowance_ms: playout.map(|p| p.allowance.as_millis() as u64),
        allowance_source: playout.map(|p| p.source),
        jitter_buffer_ms: playout
            .and_then(|p| p.jitter_buffer_ms)
            .map(|v| round(v, 1)),
        playout_report_age_ms: playout.and_then(|p| p.last_report).map(|t| age(t, now)),
        thread: c.thread,
    }
}

impl Calls {
    /// `GET /api/status`'s `webrtc`.
    pub fn status(&self) -> wire::WebRtcStatus {
        let group = |g: Group| {
            let (state, addrs, error) = match self.group(g) {
                GroupState::Off => (wire::WebRtcGroupState::Off, Vec::new(), None),
                // The gap of a move is not bound either; the wire has no
                // state for the few moments of it.
                GroupState::Down | GroupState::Moving => {
                    (wire::WebRtcGroupState::Down, Vec::new(), None)
                }
                GroupState::Bound(a) => (
                    wire::WebRtcGroupState::Bound,
                    a.iter().map(|a| format!("udp://{a}")).collect(),
                    None,
                ),
                GroupState::Failed(why) => (wire::WebRtcGroupState::Failed, Vec::new(), Some(why)),
            };
            wire::WebRtcGroupStatus {
                group: g.wire(),
                rule: g.rule().into(),
                state,
                addrs,
                error,
            }
        };
        let now = Instant::now();
        wire::WebRtcStatus {
            enabled: !self.inner.off_at_start.load(Ordering::Relaxed),
            groups: vec![group(Group::Plain), group(Group::Devices)],
            dropped: self.dropped(),
            calls: self.list().iter().map(|c| call(c, now)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::realtime::calls::{CallPlayout, CallStats};

    fn running(now: Instant) -> CallInfo {
        CallInfo {
            id: "rtc_1".into(),
            creator: Principal::Anonymous,
            group: Group::Plain,
            candidate: "127.0.0.1:8787".parse().unwrap(),
            state: CallState::Running,
            created: now,
            connected: Some(now),
            opened: Some(now),
            remote: None,
            last_heard: Some(now),
            unacked_since: None,
            stats: CallStats {
                report_at: Some(now - Duration::from_millis(2_500)),
                egress_loss: Some(0.012_345),
                // 50 ticks at 48 kHz: 1.0416… ms.
                client_jitter: Some(50),
                ingress_loss: Some(0.0),
            },
            playout: Some(CallPlayout {
                allowance: Duration::from_millis(20),
                source: wire::PlayoutSource::Formula,
                round_trip: Some(Duration::from_millis(3)),
                jitter_buffer_ms: Some(30.041_666),
                last_report: Some(now - Duration::from_millis(400)),
            }),
            held_by: None,
            session: Some("sess_1".into()),
            thread: None,
        }
    }

    /// The round trip is the session's, each figure has its age, and the
    /// floats are rounded to what they measure.
    #[test]
    fn a_live_call_shows_the_sessions_round_trip_with_its_figures_aged_and_rounded() {
        let now = Instant::now();
        let c = call(&running(now), now);
        assert_eq!(c.round_trip_ms, Some(3));
        assert_eq!(c.egress_loss, Some(0.012));
        assert_eq!(c.ingress_loss, Some(0.0));
        assert_eq!(c.client_jitter_ms, Some(1.0));
        assert_eq!(c.receiver_report_age_ms, Some(2_500));
        assert_eq!(c.jitter_buffer_ms, Some(30.0));
        assert_eq!(c.playout_report_age_ms, Some(400));
        assert_eq!(
            (c.allowance_ms, c.allowance_source),
            (Some(20), Some(wire::PlayoutSource::Formula))
        );
    }

    /// A closing call's client may be gone: no link figure stands for the
    /// link any more. The playout it last had stays, as said.
    #[test]
    fn a_closing_call_shows_no_link_figures() {
        let now = Instant::now();
        let mut info = running(now);
        info.state = CallState::Closing;
        let c = call(&info, now);
        assert_eq!(c.state, wire::WebRtcCallState::Closing);
        assert_eq!(
            (
                c.round_trip_ms,
                c.egress_loss,
                c.ingress_loss,
                c.client_jitter_ms,
                c.receiver_report_age_ms
            ),
            (None, None, None, None, None)
        );
        assert_eq!(c.allowance_ms, Some(20));
        assert_eq!(c.jitter_buffer_ms, Some(30.0));
    }
}
