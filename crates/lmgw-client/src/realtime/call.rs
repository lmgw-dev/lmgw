//! What the client events a WebRTC call adds carry (realtime calls design
//! §2.2, §5.3): the session a bound client owns, which is a call's `session`
//! part and a `session.update`'s, and a playout report. The events
//! themselves are [`ClientEvent`]'s: `ResponseStop`, `TakeOver`, `Playout`,
//! `BoundSessionUpdate` and `OutputAudioBufferClear`.
//!
//! [`ClientEvent`]: super::ClientEvent

use lmgw_api_types::realtime::{OutputAudioPlayout, TurnMode};
use serde_json::{json, Map, Value};

/// What the client of a session bound to a Chat thread owns of its session
/// (realtime calls design §2.2; chat-voice design §8.1): the turn mode, half
/// duplex and the echo tail. The thread owns the rest. As a call's `session`
/// part ([`crate::requests::calls::call`]) lmgw applies it as the session's
/// first update; as [`ClientEvent::BoundSessionUpdate`] it is a
/// `session.update`. A field left `None` is not sent, and stays as it is.
///
/// [`ClientEvent::BoundSessionUpdate`]: super::ClientEvent::BoundSessionUpdate
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundSession {
    /// `session.lmgw.turn_mode`: the mode switch, or the owner's override at
    /// the bind. Absent, the session keeps the mode lmgw resolved from the
    /// thread; lmgw sets the turn detection that goes with it, so a client
    /// names no `audio.input.turn_detection` beside it.
    pub turn_mode: Option<TurnMode>,
    /// `session.lmgw.half_duplex`: a client without echo cancellation sets
    /// it.
    pub half_duplex: Option<bool>,
    /// `session.lmgw.echo_tail_ms`: how long past the playback end input
    /// still counts as heard.
    pub echo_tail_ms: Option<u32>,
}

impl BoundSession {
    /// Only the mode switch: `session.lmgw.turn_mode`, nothing else.
    pub fn turn_mode(mode: TurnMode) -> Self {
        BoundSession {
            turn_mode: Some(mode),
            ..BoundSession::default()
        }
    }

    /// The `session` object: `{"type": "realtime", "lmgw": {…}}` with the
    /// fields that are set.
    pub fn to_value(&self) -> Value {
        let mut lmgw = Map::new();
        if let Some(m) = &self.turn_mode {
            lmgw.insert("turn_mode".into(), json!(m.as_str()));
        }
        if let Some(h) = self.half_duplex {
            lmgw.insert("half_duplex".into(), json!(h));
        }
        if let Some(t) = self.echo_tail_ms {
            lmgw.insert("echo_tail_ms".into(), json!(t));
        }
        json!({"type": "realtime", "lmgw": lmgw})
    }
}

/// A playout report with something in it (§5.3): what the player says of
/// the inbound audio. [`PlayoutReport::new`] makes none of a player that
/// says neither, since such a report is not sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayoutReport {
    playout_ntp_ms: Option<f64>,
    jitter_buffer_ms: Option<f64>,
}

impl PlayoutReport {
    /// A report of `playout_ntp_ms` (`estimatedPlayoutTimestamp`) and
    /// `jitter_buffer_ms` (the growth of `jitterBufferDelay` over that of
    /// `jitterBufferEmittedCount` since the previous report, in
    /// milliseconds). A stat that is missing, or no finite number (JSON has
    /// none for NaN), is left out; `None` when both are. libwebrtc does not
    /// fill `estimatedPlayoutTimestamp` on an audio-only call (calls design
    /// Q5), so a report there carries `jitter_buffer_ms` alone.
    pub fn new(playout_ntp_ms: Option<f64>, jitter_buffer_ms: Option<f64>) -> Option<Self> {
        let finite = |v: Option<f64>| v.filter(|x| x.is_finite());
        let (playout_ntp_ms, jitter_buffer_ms) = (finite(playout_ntp_ms), finite(jitter_buffer_ms));
        (playout_ntp_ms.is_some() || jitter_buffer_ms.is_some()).then_some(PlayoutReport {
            playout_ntp_ms,
            jitter_buffer_ms,
        })
    }

    pub fn playout_ntp_ms(&self) -> Option<f64> {
        self.playout_ntp_ms
    }

    pub fn jitter_buffer_ms(&self) -> Option<f64> {
        self.jitter_buffer_ms
    }

    /// The event's fields as the wire type has them.
    pub(super) fn wire(&self) -> OutputAudioPlayout {
        OutputAudioPlayout {
            playout_ntp_ms: self.playout_ntp_ms,
            jitter_buffer_ms: self.jitter_buffer_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::ClientEvent;
    use super::*;

    fn sent(e: &ClientEvent) -> Value {
        serde_json::from_str(&e.to_json()).unwrap()
    }

    #[test]
    fn each_event_is_built_as_the_design_spells_it() {
        assert_eq!(
            sent(&ClientEvent::ResponseStop {
                playout_ntp_ms: Some(3_970_000_000_123.0)
            }),
            json!({"type": "lmgw.response.stop", "playout_ntp_ms": 3_970_000_000_123.0})
        );
        // Without a position, and with one that is no number: none sent.
        for p in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(
                sent(&ClientEvent::ResponseStop { playout_ntp_ms: p }),
                json!({"type": "lmgw.response.stop"}),
                "{p:?}"
            );
        }
        assert_eq!(
            sent(&ClientEvent::TakeOver),
            json!({"type": "lmgw.session.take_over"})
        );
        assert_eq!(
            sent(&ClientEvent::OutputAudioBufferClear),
            json!({"type": "output_audio_buffer.clear"})
        );
        assert_eq!(
            sent(&ClientEvent::BoundSessionUpdate(BoundSession::turn_mode(
                TurnMode::PushToTalk
            ))),
            json!({"type": "session.update",
                   "session": {"type": "realtime", "lmgw": {"turn_mode": "push_to_talk"}}})
        );
        let all = BoundSession {
            turn_mode: Some(TurnMode::Automatic),
            half_duplex: Some(true),
            echo_tail_ms: Some(250),
        };
        assert_eq!(
            all.to_value(),
            json!({"type": "realtime", "lmgw": {"turn_mode": "automatic",
                   "half_duplex": true, "echo_tail_ms": 250}})
        );
        assert_eq!(
            BoundSession::default().to_value(),
            json!({"type": "realtime", "lmgw": {}})
        );
    }

    #[test]
    fn each_event_names_its_type() {
        let report = ClientEvent::playout(Some(1.0), None).unwrap();
        for e in [
            ClientEvent::SessionUpdate {
                turn_detection: None,
                half_duplex: false,
            },
            ClientEvent::append_pcm16(&[0, 1]),
            ClientEvent::Commit,
            ClientEvent::Clear,
            ClientEvent::ResponseCreate,
            ClientEvent::ResponseCancel { response_id: None },
            ClientEvent::Truncate {
                item_id: "i".into(),
                content_index: 0,
                audio_end_ms: 0,
            },
            ClientEvent::ApprovalResponse {
                approval_request_id: "a".into(),
                approve: true,
                reason: None,
            },
            ClientEvent::ResponseStop {
                playout_ntp_ms: None,
            },
            ClientEvent::TakeOver,
            report,
            ClientEvent::BoundSessionUpdate(BoundSession::default()),
            ClientEvent::OutputAudioBufferClear,
        ] {
            assert_eq!(sent(&e)["type"], json!(e.kind()), "{e:?}");
        }
    }

    #[test]
    fn a_playout_report_carries_what_the_player_said_and_none_is_built_of_nothing() {
        let e = ClientEvent::playout(Some(3_970_000_000_000.0), Some(62.5)).unwrap();
        assert_eq!(
            sent(&e),
            json!({"type": "lmgw.output_audio.playout",
                   "playout_ntp_ms": 3_970_000_000_000.0, "jitter_buffer_ms": 62.5})
        );
        // A missing stat leaves its field out.
        let e = ClientEvent::playout(None, Some(40.0)).unwrap();
        assert_eq!(
            sent(&e),
            json!({"type": "lmgw.output_audio.playout", "jitter_buffer_ms": 40.0})
        );
        let e = ClientEvent::playout(Some(1.0), Some(f64::NAN)).unwrap();
        assert_eq!(
            sent(&e),
            json!({"type": "lmgw.output_audio.playout", "playout_ntp_ms": 1.0})
        );
        // Neither: no report.
        assert_eq!(ClientEvent::playout(None, None), None);
        assert_eq!(
            ClientEvent::playout(Some(f64::NAN), Some(f64::NEG_INFINITY)),
            None
        );
        let r = PlayoutReport::new(Some(7.0), None).unwrap();
        assert_eq!(
            (r.playout_ntp_ms(), r.jitter_buffer_ms()),
            (Some(7.0), None)
        );
    }
}
