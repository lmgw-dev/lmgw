//! Settings → Realtime → WebRTC calls (realtime calls design §9): the five
//! `realtime.webrtc_*` rows, the listener line — each socket group with its
//! rule, its addresses or why it is not bound —, the firewall hint, and the
//! live calls from the status block (`webrtc.calls`), read again every few
//! seconds while the group is on screen and the window is visible.
//!
//! **A poll, not a push.** `/api/events` carries no status frame, and the
//! calls' figures move with every receiver and playout report: a push would
//! be a timer on the gateway for every connected dashboard, a hidden one
//! included (the tray app hides the window on close and its stream stays
//! open). So the group reads `/api/status` itself, only while the document
//! is visible, at once when it becomes visible again, and never while its
//! last read is still out.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::realtime::{
    PlayoutSource, WebRtcCallState, WebRtcCallStatus, WebRtcGroup, WebRtcGroupState,
    WebRtcGroupStatus, WebRtcStatus,
};
use lmgw_api_types::GatewayStatus;
use serde_json::Value;

use super::{f, Ctl, Def, Page};

/// How often the live calls are read again while the group shows and the
/// window is visible.
const REFRESH: Duration = Duration::from_secs(3);

/// Whether the document is visible (`document.visibilityState` is
/// `visible`): a hidden or minimized window reads nothing.
fn document_visible() -> bool {
    !document().hidden()
}

/// The group's rows, in page order.
pub(super) const WEBRTC: &[Def] = &[
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_status",
        "UDP listener",
        Ctl::WebRtc,
    )
    .terms("webrtc udp listener socket group firewall port calls phone live call round trip"),
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_enabled",
        "WebRTC calls",
        Ctl::Bool,
    )
    .hint(
        "POST /v1/realtime/calls: the UDP sockets beside the plain and the device listener; off \
         answers 503 naming this setting. Applies at the next start",
    )
    .terms("webrtc calls udp enabled switch"),
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_port",
        "UDP port",
        Ctl::OptIntRange(0, 65535),
    )
    .s()
    .ph("HTTP port")
    .hint(
        "the plain listener's calls; empty = the HTTP port's number, 0 = chosen by the OS. A \
         paired device's calls use the device listener's port. Applies at the next start",
    )
    .terms("webrtc udp port"),
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_reorder_ms",
        "Reorder window",
        Ctl::IntRange(0, 40950),
    )
    .s()
    .unit("ms")
    .hint("how long a frame waits for a missing earlier one; 0 = no wait. The next call")
    .terms("webrtc jitter reorder loss fec"),
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_commit_window_ms",
        "Commit window",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint(
        "how far past its stamp a push-to-talk commit takes frames, and how long it waits for \
         them: the phone's capture-to-send offset. The next call",
    )
    .terms("webrtc push to talk commit window capture offset"),
    f(
        "realtime",
        "rt-webrtc",
        "realtime.webrtc_output_kbps",
        "Output bitrate",
        Ctl::IntRange(6, 510),
    )
    .s()
    .unit("kbps")
    .hint("the Opus answer, capped by the phone's maxaveragebitrate. The next call")
    .terms("webrtc opus bitrate quality"),
];

/// A socket group's line: its label, what it says, its chip's tone.
fn group_line(g: &WebRtcGroupStatus) -> (&'static str, String, &'static str) {
    let label = match g.group {
        WebRtcGroup::Plain => "Plain listener",
        WebRtcGroup::Devices => "Device listener",
    };
    let (line, tone) = match g.state {
        WebRtcGroupState::Bound => (g.addrs.join(" and "), "ok"),
        WebRtcGroupState::Failed => (
            format!("failed: {}", g.error.clone().unwrap_or_default()),
            "warn",
        ),
        WebRtcGroupState::Down => (
            match g.group {
                WebRtcGroup::Plain => "not bound".to_string(),
                WebRtcGroup::Devices => "not bound: the device listener is off or moving".into(),
            },
            "off",
        ),
        WebRtcGroupState::Off => ("off (realtime.webrtc_enabled)".into(), "off"),
    };
    (label, line, tone)
}

/// Whether `ip` (a bind address's host) reaches this computer alone.
fn loopback(ip: &str) -> bool {
    ip.trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// The firewall hint (§3.1, §9): the device port, TCP and UDP, for phones;
/// the plain group's UDP port only for a `bind_addr` that is not loopback.
/// `saved` is `GET /api/settings-full`.
fn firewall(saved: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let d = &saved["devices"];
    match (d["listener"].as_bool(), d["port_effective"].as_u64()) {
        (Some(true), Some(port)) => out.push(format!(
            "Phones call through the device listener: allow port {port} for TCP and UDP in the \
             firewall zone they come from (Fedora's Workstation zone opens 1025–65535 already)."
        )),
        _ => out.push(
            "Phones call through the device listener (Network & access → Devices), which is off."
                .into(),
        ),
    }
    let bind = saved["bind_addr"].as_str().unwrap_or_default();
    if let Some((host, http)) = bind.rsplit_once(':') {
        if !loopback(host) {
            let udp = saved["realtime"]["webrtc_port"]
                .as_u64()
                .map_or_else(|| http.to_string(), |p| p.to_string());
            let udp = if udp == "0" {
                "the UDP port the OS chose (the line above names it)".to_string()
            } else {
                format!("UDP port {udp}")
            };
            out.push(format!(
                "Stock clients on other machines reach the plain listener at {bind}: allow {udp} \
                 beside TCP port {http}."
            ));
        }
    }
    out
}

/// A call's state in a word.
fn state_word(s: WebRtcCallState) -> &'static str {
    match s {
        WebRtcCallState::Answered => "answered",
        WebRtcCallState::Connected => "connected",
        WebRtcCallState::Open => "binding",
        WebRtcCallState::Pending => "asking to take over",
        WebRtcCallState::Running => "running",
        WebRtcCallState::Closing => "closing",
    }
}

/// A live call's cells: call, state, by, remote, round trip, loss,
/// allowance, the phone's jitter buffer.
fn call_cells(c: &WebRtcCallStatus) -> [String; 8] {
    let short = c.id.get(..12).unwrap_or(&c.id).to_string();
    let mut state = state_word(c.state).to_string();
    if let Some(h) = &c.held_by {
        state.push_str(&format!(" (held by {h})"));
    }
    if let Some(t) = c.thread {
        state.push_str(&format!(" · thread {t}"));
    }
    let by = match c.group {
        WebRtcGroup::Devices => format!("{} · device link", c.key.as_deref().unwrap_or("?")),
        WebRtcGroup::Plain => c.key.clone().unwrap_or_else(|| "anonymous".into()),
    };
    // A figure and how old the report it comes from is.
    let aged = |v: Option<String>, age: Option<u64>| match (v, age) {
        (Some(v), Some(a)) => format!("{v} · {:.1} s ago", a as f64 / 1000.0),
        (Some(v), None) => v,
        (None, _) => "—".to_string(),
    };
    let pct = |v: Option<f64>| v.map_or("—".to_string(), |v| format!("{:.1} %", v * 100.0));
    let loss = format!("{} / {}", pct(c.egress_loss), pct(c.ingress_loss));
    let allowance = match (c.allowance_ms, c.allowance_source) {
        (Some(a), Some(src)) => {
            let from = match src {
                PlayoutSource::Report => "report",
                PlayoutSource::Formula => "formula",
                PlayoutSource::Frame => "one frame",
            };
            format!("{a} ms · {from}")
        }
        _ => "—".into(),
    };
    [
        short,
        state,
        by,
        c.remote.clone().unwrap_or_else(|| "—".into()),
        aged(
            c.round_trip_ms.map(|r| format!("{r} ms")),
            c.receiver_report_age_ms,
        ),
        loss,
        allowance,
        aged(
            c.jitter_buffer_ms.map(|j| format!("{j:.0} ms")),
            c.playout_report_age_ms,
        ),
    ]
}

/// The listener line, the firewall hint and the live calls.
#[component]
pub(super) fn WebRtcPanel(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let status = RwSignal::new(None::<WebRtcStatus>);
    // A read is out: no other starts until it is back (module doc).
    let reading = Rc::new(Cell::new(false));
    let refresh = move || {
        if hidden.get_untracked() || !document_visible() || reading.replace(true) {
            return;
        }
        let reading = reading.clone();
        spawn_local(async move {
            if let Ok(s) = crate::api::get::<GatewayStatus>("/api/status").await {
                status.try_set(Some(s.webrtc));
            }
            reading.set(false);
        })
    };
    refresh();
    if let Ok(h) = set_interval_with_handle(refresh.clone(), REFRESH) {
        on_cleanup(move || h.clear());
    }
    // At once when the window shows again, and when the group does.
    let on_visible = refresh.clone();
    let listener = window_event_listener_untyped("visibilitychange", move |_| on_visible());
    on_cleanup(move || listener.remove());
    let on_shown = refresh.clone();
    Effect::new(move |was: Option<bool>| {
        let now = hidden.get();
        if was == Some(true) && !now {
            on_shown();
        }
        now
    });
    // The switch as saved against the switch as the start read it.
    let pending = move || {
        let saved = page
            .data
            .with(|d| d.as_ref()?["realtime"]["webrtc_enabled"].as_bool())?;
        let started = status.with(|s| s.as_ref().map(|s| s.enabled))?;
        (saved != started).then(|| {
            format!(
                "realtime.webrtc_enabled is saved {}: it applies at the next start of lmgw",
                if saved { "on" } else { "off" }
            )
        })
    };
    let hints = move || {
        page.data
            .with(|d| d.as_ref().map(firewall).unwrap_or_default())
    };
    view! {
        <div class="set-blocks webrtc-panel" id=id hidden=move || hidden.get()>
            {move || match status.get() {
                None => view! { <div class="set-line"><span class="dim">"reading…"</span></div> }
                    .into_any(),
                Some(s) => {
                    let dropped = s.dropped;
                    view! {
                        {s
                            .groups
                            .iter()
                            .map(|g| {
                                let (label, line, tone) = group_line(g);
                                let rule = g.rule.clone();
                                view! {
                                    <div class="set-line">
                                        <span class="set-line-label">{label}</span>
                                        <span class="chip" class:ok=tone == "ok" class:warn=tone == "warn"
                                            class:off=tone == "off">
                                            <span class="dot"></span>
                                            {line}
                                        </span>
                                        <span class="dim webrtc-rule">{rule}</span>
                                    </div>
                                }
                            })
                            .collect_view()}
                        {(dropped > 0)
                            .then(|| {
                                view! {
                                    <div class="field-hint">
                                        {format!("{dropped} datagram(s) no call accepted, dropped unanswered since the start")}
                                    </div>
                                }
                            })}
                    }
                    .into_any()
                }
            }}
            {move || pending().map(|p| view! { <div class="notice warn set-notice">{p}</div> })}
            <div class="field-hint webrtc-firewall">
                {move || hints().into_iter().map(|h| view! { <p>{h}</p> }).collect_view()}
            </div>
            {move || {
                let calls = status.with(|s| s.as_ref().map(|s| s.calls.clone()).unwrap_or_default());
                if calls.is_empty() {
                    return view! { <div class="field-hint">"No call is live."</div> }.into_any();
                }
                view! {
                    <div class="webrtc-calls-wrap">
                    <table class="data webrtc-calls">
                        <thead>
                            <tr>
                                <th>"Call"</th>
                                <th>"State"</th>
                                <th>"By"</th>
                                <th>"From"</th>
                                <th title="the session's round trip, as the playout allowance and the barge-in margin take it: the least of the last five the client's receiver reports measured; beside it, how old the latest report is">"RTT"</th>
                                <th title="the share of lmgw's packets the client lost, by its latest receiver report / of the client's packets lmgw lost, over the last second">
                                    "Loss out / in"
                                </th>
                                <th title="NetEQ's part of the delay: from the phone's playout reports, else one frame plus twice its jitter">
                                    "Allowance"
                                </th>
                                <th title="NetEQ's own delay, from the phone's latest playout report (jitter_buffer_ms), and how old that report is">
                                    "Jitter buffer"
                                </th>
                            </tr>
                        </thead>
                        <tbody>
                            {calls
                                .iter()
                                .map(|c| {
                                    let [call, state, by, from, rtt, loss, allowance, jb] = call_cells(c);
                                    let title = c.id.clone();
                                    view! {
                                        <tr>
                                            <td class="mono" title=title>{call}</td>
                                            <td class="wrap">{state}</td>
                                            <td class="wrap">{by}</td>
                                            <td class="mono">{from}</td>
                                            <td class="num">{rtt}</td>
                                            <td class="num">{loss}</td>
                                            <td class="wrap">{allowance}</td>
                                            <td class="num">{jb}</td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_firewall_hint_names_the_device_port_and_a_plain_port_off_loopback() {
        let lo = json!({"bind_addr": "127.0.0.1:8787", "realtime": {"webrtc_port": null},
                        "devices": {"listener": true, "port_effective": 8788}});
        let h = firewall(&lo);
        assert_eq!(h.len(), 1, "{h:?}");
        assert!(h[0].contains("port 8788 for TCP and UDP"), "{h:?}");
        let lan = json!({"bind_addr": "192.0.2.1:8787", "realtime": {"webrtc_port": null},
                         "devices": {"listener": false, "port_effective": 8788}});
        let h = firewall(&lan);
        assert!(h[0].contains("which is off"), "{h:?}");
        assert!(h[1].contains("UDP port 8787 beside TCP port 8787"), "{h:?}");
        let own = json!({"bind_addr": "[::]:8787", "realtime": {"webrtc_port": 3478},
                         "devices": {"listener": false}});
        assert!(
            firewall(&own)[1].contains("UDP port 3478"),
            "{:?}",
            firewall(&own)
        );
        let v6lo = json!({"bind_addr": "[::1]:8787", "realtime": {}, "devices": {}});
        assert_eq!(firewall(&v6lo).len(), 1);
    }

    #[test]
    fn a_group_and_a_call_read_in_words() {
        let g = WebRtcGroupStatus {
            group: WebRtcGroup::Devices,
            state: WebRtcGroupState::Failed,
            error: Some("udp://[::]:8788: address in use".into()),
            ..Default::default()
        };
        let (label, line, tone) = group_line(&g);
        assert_eq!((label, tone), ("Device listener", "warn"));
        assert!(line.contains("address in use"), "{line}");
        let c = WebRtcCallStatus {
            id: "rtc_0123456789abcdef".into(),
            group: WebRtcGroup::Devices,
            state: WebRtcCallState::Running,
            key: Some("device:phone".into()),
            thread: Some(12),
            round_trip_ms: Some(34),
            receiver_report_age_ms: Some(2_600),
            egress_loss: Some(0.012),
            allowance_ms: Some(61),
            allowance_source: Some(PlayoutSource::Report),
            jitter_buffer_ms: Some(58.4),
            playout_report_age_ms: Some(400),
            ..Default::default()
        };
        let cells = call_cells(&c);
        assert_eq!(cells[0], "rtc_01234567");
        assert_eq!(cells[1], "running · thread 12");
        assert_eq!(cells[2], "device:phone · device link");
        assert_eq!(cells[4], "34 ms · 2.6 s ago");
        assert_eq!(cells[5], "1.2 % / —");
        assert_eq!(cells[6], "61 ms · report");
        assert_eq!(cells[7], "58 ms · 0.4 s ago");
        // A closing call: no link figures.
        let closing = WebRtcCallStatus {
            state: WebRtcCallState::Closing,
            allowance_ms: Some(20),
            allowance_source: Some(PlayoutSource::Formula),
            ..Default::default()
        };
        let cells = call_cells(&closing);
        assert_eq!((cells[4].as_str(), cells[5].as_str()), ("—", "— / —"));
        assert_eq!(cells[6], "20 ms · formula");
    }
}
