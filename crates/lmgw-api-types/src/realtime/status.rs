//! The status block's `webrtc` (realtime calls design §9): the UDP
//! listener for `POST /v1/realtime/calls` as it serves now, one socket
//! group per HTTP listener — the plain listener's, bound at start beside
//! it, and the device listener's, which follows that listener live — and
//! the live calls, each with its link's figures and its playout allowance.

use serde::{Deserialize, Serialize};

/// `GET /api/status`'s `webrtc`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebRtcStatus {
    /// `realtime.webrtc_enabled` as the gateway's last start read it: the
    /// setting applies at the next start, so a saved value may differ until
    /// then.
    pub enabled: bool,
    /// The socket groups: the plain listener's, then the device listener's.
    pub groups: Vec<WebRtcGroupStatus>,
    /// Datagrams no call accepted, dropped unanswered since lmgw started
    /// (the count outlives the call driver, which a panic starts again).
    pub dropped: u64,
    /// The live calls, oldest first, those still delivering their last
    /// events (`closing`) included.
    pub calls: Vec<WebRtcCallStatus>,
}

/// One live call (calls design §3.4, §9).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebRtcCallStatus {
    /// Its id, `rtc_…`: what `POST /v1/realtime/calls/{call_id}/hangup`
    /// takes.
    pub id: String,
    /// The socket group it lives on: the listener its POST came in on.
    pub group: WebRtcGroup,
    pub state: WebRtcCallState,
    /// Who holds its thread while it is `pending`, as `voice.bound.by`
    /// names a client.
    pub held_by: Option<String>,
    /// Its realtime session's id, once the session runs.
    pub session: Option<String>,
    /// The key that made it (`device:phone`, `owner:…`); `null` for an
    /// anonymous caller.
    pub key: Option<String>,
    /// Its one host candidate: the local address its POST arrived on, on
    /// its group's UDP port.
    pub candidate: String,
    /// Where the client's last authenticated traffic came from.
    pub remote: Option<String>,
    /// When ICE and DTLS connected (RFC 3339 UTC); `null` before.
    pub connected_at: Option<String>,
    /// The session's round trip, in ms: the least of the last five the
    /// client's receiver reports measured, as the playout allowance and the
    /// barge-in margin take it. `null` before the first report reached the
    /// call's audio out, and while `closing`.
    pub round_trip_ms: Option<u64>,
    /// The share of lmgw's packets the client lost, 0 to 1, by its latest
    /// receiver report (to 0.001); `null` while `closing`.
    pub egress_loss: Option<f64>,
    /// The share of the client's packets lmgw lost over the last second, 0
    /// to 1 (to 0.001); `null` for a second in which none arrived, and while
    /// `closing`.
    pub ingress_loss: Option<f64>,
    /// The client's interarrival jitter on lmgw's stream, by its latest
    /// receiver report, in ms (to 0.1); `null` while `closing`.
    pub client_jitter_ms: Option<f64>,
    /// How long ago the client's latest receiver report arrived, in ms: the
    /// age of `egress_loss`, `client_jitter_ms` and the round trip it fed.
    /// A client sends one every few seconds; `null` before the first, and
    /// while `closing`.
    pub receiver_report_age_ms: Option<u64>,
    /// The playout allowance in ms (calls design §5.3): NetEQ's part of the
    /// delay, which the window's end, drained and a barge-in's heard take.
    /// `null` before the call's audio out runs.
    pub allowance_ms: Option<u64>,
    /// Where the allowance comes from.
    pub allowance_source: Option<PlayoutSource>,
    /// The latest playout report's `jitter_buffer_ms` (to 0.1): NetEQ's own
    /// delay as the client's `getStats` gives it, the allowance seen from
    /// the phone.
    pub jitter_buffer_ms: Option<f64>,
    /// How long ago the latest playout report arrived, whatever it carried,
    /// in ms: the age of `jitter_buffer_ms`. `null` before the first.
    pub playout_report_age_ms: Option<u64>,
    /// The Chat thread its session is bound to.
    pub thread: Option<i64>,
}

/// Where a call stands (calls design §3.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WebRtcCallState {
    /// Answered: waiting for ICE and DTLS.
    #[default]
    Answered,
    /// ICE and DTLS-SRTP are up; `oai-events` has not opened.
    Connected,
    /// `oai-events` is open; the bind runs.
    Open,
    /// `takeover=ask` found its thread held: it waits for the client's
    /// `lmgw.session.take_over` or the holder's end (`held_by`).
    Pending,
    /// Its session runs.
    Running,
    /// It let go: its slot is back, and it only delivers its last events
    /// before it closes, at most `realtime.ping_interval_s`.
    Closing,
}

/// Where a call's playout allowance comes from (calls design §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlayoutSource {
    /// The client's latest playout report (`lmgw.output_audio.playout`): the
    /// measured playout delay less half the round trip.
    Report,
    /// One frame plus twice the client's interarrival jitter: before the
    /// first report, on an unbound call, and after reports stopped.
    Formula,
    /// One frame: no receiver report yet.
    Frame,
}

/// One socket group: the sockets of the listener a call's POST came in on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebRtcGroupStatus {
    pub group: WebRtcGroup,
    /// The rule that binds it, naming its settings: the plain listener's
    /// `bind_addr` and `realtime.webrtc_port`, or the device listener's
    /// addresses and port.
    pub rule: String,
    pub state: WebRtcGroupState,
    /// Its sockets, as `udp://` addresses: one per address of its listener.
    /// Empty unless `bound`.
    pub addrs: Vec<String>,
    /// Why it is not bound, with state `failed`: the bind error naming the
    /// address, or the call driver's stop.
    pub error: Option<String>,
}

/// Which listener a socket group serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WebRtcGroup {
    /// The plain listener's: stock clients.
    #[default]
    Plain,
    /// The device listener's: paired devices.
    Devices,
}

/// Where a socket group stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WebRtcGroupState {
    /// `realtime.webrtc_enabled` was off at the gateway's start: a call on
    /// its listener is refused 503 naming the setting.
    Off,
    /// Not bound: its listener does not serve, or it is moving with it.
    #[default]
    Down,
    /// Bound on `addrs`.
    Bound,
    /// Its bind failed, or its sockets were lost; `error` says why. Its
    /// listener serves on, and a call there is refused 503 naming it.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_block_reads_as_the_status_spells_it() {
        let s = WebRtcStatus {
            enabled: true,
            groups: vec![WebRtcGroupStatus {
                group: WebRtcGroup::Devices,
                rule: "r".into(),
                state: WebRtcGroupState::Failed,
                addrs: vec![],
                error: Some("e".into()),
            }],
            dropped: 3,
            calls: vec![WebRtcCallStatus {
                id: "rtc_1".into(),
                group: WebRtcGroup::Devices,
                state: WebRtcCallState::Running,
                candidate: "192.0.2.1:8788".into(),
                allowance_ms: Some(61),
                allowance_source: Some(PlayoutSource::Report),
                ..Default::default()
            }],
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v["groups"],
            json!([
                {"group": "devices", "rule": "r", "state": "failed", "addrs": [], "error": "e"}
            ])
        );
        assert_eq!((&v["enabled"], &v["dropped"]), (&json!(true), &json!(3)));
        let call = &v["calls"][0];
        assert_eq!(
            (&call["state"], &call["group"], &call["allowance_source"]),
            (&json!("running"), &json!("devices"), &json!("report"))
        );
        assert!(call["held_by"].is_null() && call["playout_report_age_ms"].is_null());
        assert_eq!(serde_json::from_value::<WebRtcStatus>(v).unwrap(), s);
        // A status from before the block reads as nothing bound.
        assert_eq!(
            serde_json::from_value::<WebRtcStatus>(json!({})).unwrap(),
            WebRtcStatus::default()
        );
    }
}
