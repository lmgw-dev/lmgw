//! A WebRTC call's request ([`CallRequest`], the multipart POST's parts),
//! and a bound call's own events on its `oai-events` channel (realtime
//! calls design §5.1, §5.3, §6.2, §6.3, §6.4) beside the bind and close
//! events of `binding`: the stop, the playout reports, the underrun and
//! the parts an oversized event travels in. lmgw's own, for its own clients:
//! an unbound call sends none of them and answers a client's with `error`
//! `invalid_event`. And OpenAI's `output_audio_buffer.*` events
//! ([`OutputAudioBuffer`]), which every call sends.
//!
//! Each is a JSON object with its `type` ([`RESPONSE_STOP`],
//! [`OUTPUT_AUDIO_PLAYOUT`], [`OUTPUT_AUDIO_STARVED`], [`EVENT_PART`],
//! [`OUTPUT_AUDIO_BUFFER_STARTED`] and its two siblings); lmgw's carry an
//! `event_id`, as all its events do.
//!
//! `*_ntp_ms` is a number of milliseconds on the NTP timescale (from 1900,
//! as RTCP carries it), in the clock of the stream it describes: for the
//! playout, lmgw's own, as its sender reports carry it. A JSON number, read
//! as `f64` because libwebrtc's `estimatedPlayoutTimestamp` is one (W3C's
//! `DOMHighResTimeStamp`); today's NTP milliseconds are far inside its exact
//! integers.

use serde::{Deserialize, Serialize};

/// `POST /v1/realtime/calls` as `multipart/form-data` (calls design §2.1),
/// OpenAI's shape: the parts. The offer may come alone instead, as an
/// `application/sdp` body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CallRequest {
    /// The SDP offer (`application/sdp`): audio with Opus, and the data
    /// channel `oai-events`, created before `createOffer`.
    pub sdp: String,
    /// The session (`application/json`): a JSON object, the session as
    /// `session.update` takes it — OpenAI's GA session object. Not checked
    /// before the answer: the session applies it as its first update, right
    /// after `session.created` and before any event of the client's, and
    /// answers it as any update, `session.updated` or an `error`. On a call
    /// bound to a Chat thread only what the client owns applies (turn
    /// detection or `lmgw.turn_mode`, `lmgw.half_duplex`, `lmgw.echo_tail_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub session: Option<serde_json::Map<String, serde_json::Value>>,
}

/// `lmgw.response.stop`'s `type`.
pub const RESPONSE_STOP: &str = "lmgw.response.stop";

/// `lmgw.output_audio.playout`'s `type`.
pub const OUTPUT_AUDIO_PLAYOUT: &str = "lmgw.output_audio.playout";

/// `lmgw.output_audio.starved`'s `type`.
pub const OUTPUT_AUDIO_STARVED: &str = "lmgw.output_audio.starved";

/// `lmgw.event.part`'s `type`.
pub const EVENT_PART: &str = "lmgw.event.part";

/// `output_audio_buffer.started`'s `type`.
pub const OUTPUT_AUDIO_BUFFER_STARTED: &str = "output_audio_buffer.started";

/// `output_audio_buffer.stopped`'s `type`.
pub const OUTPUT_AUDIO_BUFFER_STOPPED: &str = "output_audio_buffer.stopped";

/// `output_audio_buffer.cleared`'s `type`.
pub const OUTPUT_AUDIO_BUFFER_CLEARED: &str = "output_audio_buffer.cleared";

/// `output_audio_buffer.started`, `.stopped` and `.cleared` (lmgw → client,
/// on any call, bound or not; realtime calls design §6.2): OpenAI's
/// WebRTC events, in OpenAI's shape, `{response_id}`. A call's client reads
/// them instead of its own playback:
///
/// - `started`: lmgw sent the response's first audio frame, after its
///   `response.content_part.added`;
/// - `stopped`: right after the `response.done` of a response whose audio
///   played to its end — the client has played it;
/// - `cleared`: instead of `stopped`, right after the `response.done` of a
///   response whose audio was cut before its end (a confirmed barge-in, a
///   stop, a cancel, a `clear`): lmgw stopped sending it. The barge-in
///   verdict: a client mutes the remote track until the next `started`.
///
/// Each response that spoke ends with exactly one of `stopped` and
/// `cleared`; one that never sent a frame sends none of the three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputAudioBuffer {
    pub response_id: String,
}

/// `lmgw.response.stop {playout_ntp_ms?}` (client → lmgw): the user's stop
/// on a bound call, and the first half of the End. Idempotent: it cancels
/// whatever is open (a response in any phase a cancel changes, the response
/// owed to committed turns, a held `response.create`) and purges the unsent
/// audio. Its events are a cancel's — the cancelled `response.done`
/// (`client_cancelled`), then `output_audio_buffer.cleared` for a response
/// that spoke — and with nothing open it sends nothing, never
/// `response_cancel_not_active`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponseStop {
    /// libwebrtc's playout position at the tap, when the client muted the
    /// remote track (`estimatedPlayoutTimestamp`): what it played, which
    /// lmgw keeps as heard. Absent: heard is what lmgw sent. libwebrtc does
    /// not fill `estimatedPlayoutTimestamp` on an audio-only call (calls
    /// design Q5), so on such a call lmgw takes heard to be what was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playout_ntp_ms: Option<f64>,
}

/// `lmgw.output_audio.playout {playout_ntp_ms?, jitter_buffer_ms?}` (client
/// → lmgw): while a response's audio plays, at the client's own cadence,
/// what its player reports. lmgw measures the playout delay from it, and so
/// the playing window's end and drained. Both fields are optional; a report
/// with neither is not sent. One without `playout_ntp_ms` leaves the
/// measured position as it was, one without `jitter_buffer_ms` keeps the
/// last value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputAudioPlayout {
    /// `estimatedPlayoutTimestamp` of the inbound audio: the NTP time of the
    /// sample playing now, in lmgw's clock. libwebrtc does not fill it on an
    /// audio-only call (calls design Q5): such a report carries
    /// `jitter_buffer_ms` alone, the allowance stays on the formula, and a
    /// stop's heard is what was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playout_ntp_ms: Option<f64>,
    /// NetEQ's own delay: the change in `jitterBufferDelay` over the change
    /// in `jitterBufferEmittedCount` since the client's previous report, in
    /// milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_buffer_ms: Option<f64>,
}

/// `lmgw.output_audio.starved {response_id, starved}` (lmgw → client): on a
/// bound call, response `response_id`'s audio ran dry (`true`: the frame
/// clock started padding it with silence) or came again (`false`). It leads
/// the client's own silence by about the playout allowance, so a tool turn
/// after its spoken preamble reads as thinking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputAudioStarved {
    pub response_id: String,
    pub starved: bool,
}

/// `lmgw.event.part {id, index, count, data}` (both ways, on a bound call):
/// one slice of an event larger than the receiver's `max-message-size`. A
/// set's parts travel back to back, in index order, all with one `count`;
/// joined, their `data` is the event's JSON text, which is parsed as the one
/// event it is (never itself a part).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EventPart {
    /// The set's id: lmgw's is the event's `event_id`, a client's one of its
    /// own.
    pub id: String,
    /// From 0.
    pub index: u32,
    /// The set's number of parts, at least 2.
    pub count: u32,
    /// A slice of the event's JSON text, cut at a character boundary.
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_events_read_and_write_as_the_design_spells_them() {
        // Optional fields are left out, never written as null.
        assert_eq!(
            serde_json::to_value(ResponseStop::default()).unwrap(),
            json!({})
        );
        let stop: ResponseStop =
            serde_json::from_value(json!({"playout_ntp_ms": 3_970_000_000_123.0_f64})).unwrap();
        assert_eq!(stop.playout_ntp_ms, Some(3_970_000_000_123.0));
        // An integer reads as well as a fraction.
        let stop: ResponseStop =
            serde_json::from_value(json!({"playout_ntp_ms": 3_970_000_000_123_u64})).unwrap();
        assert_eq!(stop.playout_ntp_ms, Some(3_970_000_000_123.0));
        let report = OutputAudioPlayout {
            playout_ntp_ms: None,
            jitter_buffer_ms: Some(62.5),
        };
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"jitter_buffer_ms": 62.5})
        );
        let starved: OutputAudioStarved =
            serde_json::from_value(json!({"response_id": "resp_1", "starved": true})).unwrap();
        assert!(starved.starved);
        let part = EventPart {
            id: "evt_9".into(),
            index: 1,
            count: 3,
            data: r#"","delta":"#.into(),
        };
        assert_eq!(
            serde_json::to_value(&part).unwrap(),
            json!({"id": "evt_9", "index": 1, "count": 3, "data": "\",\"delta\":"})
        );
        let cleared: OutputAudioBuffer =
            serde_json::from_value(json!({"response_id": "resp_2"})).unwrap();
        assert_eq!(
            serde_json::to_value(&cleared).unwrap(),
            json!({"response_id": "resp_2"})
        );
    }
}
