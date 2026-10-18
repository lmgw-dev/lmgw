//! Settings → Network & access → Devices (device link design §9.1): the
//! device listener — its switch, scope and port, which act at once under a
//! confirmation (§4.1) — and the bounds of the device link's trust: a
//! pairing token's lifetime, the two certificates' validity, the device
//! listener's three timeouts and the failed-enrolment rate, each in its
//! stated range (`lmgw_api_types::device_link`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::device_link::{self as wire, DeviceListenerState, DeviceListenerStatus};
use lmgw_api_types::{GatewayStatus, MessageAck};
use serde_json::{json, Value};

use super::{f, Ctl, Def, Page};
use crate::widgets::{use_toasts, Modal, Select};

const fn range(b: wire::Bound) -> Ctl {
    Ctl::IntRange(b.min as i64, b.max as i64)
}

/// The group's rows, in page order.
pub(super) const DEVICES: &[Def] = &[
    f(
        "network",
        "devices",
        "devices.listener",
        "Device listener",
        Ctl::DeviceListener,
    )
    .terms("device listener tls https mtls port scope loopback all bind lan wireguard phone"),
    f(
        "network",
        "devices",
        "devices.token_minutes",
        "Pairing token lasts",
        range(wire::TOKEN_MINUTES),
    )
    .s()
    .unit("minutes")
    .hint("how long a pairing QR code works; applies to the next token; 1 to 1440")
    .terms("device pair pairing token qr enrol lifetime expiry"),
    f(
        "network",
        "devices",
        "devices.client_cert_days",
        "Device certificate lasts",
        range(wire::CLIENT_CERT_DAYS),
    )
    .s()
    .unit("days")
    .hint("a notice from two thirds of it on; applies to the next pairing; 1 to 3650")
    .terms("device certificate tls mtls validity expiry renew"),
    f(
        "network",
        "devices",
        "devices.server_cert_days",
        "Server certificate lasts",
        range(wire::SERVER_CERT_DAYS),
    )
    .s()
    .unit("days")
    .hint("renewed from half of it, unnoticed by devices; 1 to 3650")
    .terms("device server certificate tls leaf validity"),
    f(
        "network",
        "devices",
        "devices.handshake_timeout_s",
        "Handshake timeout",
        range(wire::HANDSHAKE_TIMEOUT_S),
    )
    .s()
    .unit("s")
    .hint("a TLS handshake's bound; applies to the next connection; 1 to 300")
    .terms("device tls handshake timeout"),
    f(
        "network",
        "devices",
        "devices.header_timeout_s",
        "Request head timeout",
        range(wire::HEADER_TIMEOUT_S),
    )
    .s()
    .unit("s")
    .hint("a request's headers, and an idle connection's wait for the next; 1 to 3600")
    .terms("device header idle keep-alive timeout"),
    f(
        "network",
        "devices",
        "devices.unauthenticated_timeout_s",
        "Without a certificate, at most",
        range(wire::UNAUTHENTICATED_TIMEOUT_S),
    )
    .s()
    .unit("s")
    .hint("a whole connection that enrols without a certificate; 1 to 300")
    .terms("device enrol unauthenticated timeout"),
    f(
        "network",
        "devices",
        "devices.enrol_refuse_per_min",
        "Failed pairings before refusing",
        range(wire::ENROL_REFUSE_PER_MIN),
    )
    .s()
    .unit("per minute")
    .hint(
        "per address (IPv6 by /64), answered 429 past it; far above any honest retry; 1 to 100000",
    )
    .terms("device enrol pairing failed rate limit 429"),
    f(
        "network",
        "devices",
        "devices.unauthenticated_max",
        "Connections before authenticating, at most",
        range(wire::UNAUTHENTICATED_MAX),
    )
    .s()
    .hint(
        "handshakes in progress and connections without a working device certificate, from \
         every address; a new one past it closes the oldest; 1 to 4096",
    )
    .terms("device connection limit handshake unauthenticated flood descriptor"),
    f(
        "network",
        "devices",
        "devices.unauthenticated_per_peer",
        "… from one address",
        range(wire::UNAUTHENTICATED_PER_PEER),
    )
    .s()
    .hint("the same, from one address (IPv6 by /64), whose oldest goes; 1 to 4096")
    .terms("device connection limit per peer address handshake unauthenticated"),
];

/// The listener's three settings as saved: on, scope, port (the effective
/// one when unset).
#[derive(Clone, PartialEq)]
struct Saved {
    on: bool,
    listen: String,
    port: Option<u16>,
}

fn saved(data: &Option<Value>) -> Option<Saved> {
    let d = &data.as_ref()?["devices"];
    let port = d["port"].as_u64().or_else(|| d["port_effective"].as_u64());
    Some(Saved {
        on: d["listener"].as_bool().unwrap_or(false),
        listen: d["listen"].as_str().unwrap_or("loopback").to_string(),
        port: port.and_then(|p| u16::try_from(p).ok()),
    })
}

/// What the edits ask for, checked: the `devices` patch's three keys.
#[derive(Debug, Clone, PartialEq)]
struct Wanted {
    on: bool,
    listen: String,
    port: u16,
}

fn wanted(on: bool, scope: &str, addr: &str, port: &str) -> Result<Wanted, String> {
    let listen = match scope {
        "address" => {
            let a = addr.trim();
            a.parse::<std::net::IpAddr>()
                .map_err(|_| format!("'{a}' is no IP address"))?;
            a.to_string()
        }
        other => other.to_string(),
    };
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p >= 1)
        .ok_or_else(|| "the port is a number from 1 to 65535".to_string())?;
    Ok(Wanted { on, listen, port })
}

/// Where a scope listens, in words.
fn scope_words(listen: &str) -> String {
    match listen {
        "loopback" => "127.0.0.1 and [::1] (this computer only)".into(),
        "all" => "every network address".into(),
        a if is_loopback(a) => format!("{a} (this computer only)"),
        a => a.to_string(),
    }
}

/// Whether a scope reaches this computer alone: `loopback`, or one loopback
/// address (`127.0.0.0/8`, `::1`).
fn is_loopback(listen: &str) -> bool {
    listen == "loopback"
        || listen
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// The confirmation's sentences for going from `from` to `to`, with what
/// the listener holds open now.
fn confirmation(from: &Saved, to: &Wanted, now: &DeviceListenerStatus) -> Vec<String> {
    let mut out = Vec::new();
    let running = now.state == DeviceListenerState::Listening;
    let moves = from.on && (!to.on || from.listen != to.listen || from.port != Some(to.port));
    if to.on {
        out.push(format!(
            "lmgw will listen for devices on {}, port {} (TLS; a device needs a pairing token \
             and your confirmation, or its certificate). The first start makes lmgw's device CA.",
            scope_words(&to.listen),
            to.port
        ));
        // Exposed to the LAN when it listens beyond this computer and was
        // not listening there already: turned on, or moved to this scope.
        if !is_loopback(&to.listen) && (!from.on || from.listen != to.listen) {
            out.push(
                "On Fedora's default desktop zone, TCP and UDP ports 1025–65535 are open, so \
                 devices on your LAN can reach it at once; they get nothing without a pairing \
                 token and your confirmation, or a certificate."
                    .into(),
            );
        }
    } else {
        out.push(
            "The device listener turns off: no device reaches lmgw until it is on again.".into(),
        );
    }
    if moves && running {
        out.push(format!(
            "This ends the device listener's {} open connection(s), {} request(s) in flight \
             among them (feeds end), and its {} realtime session(s) or turn(s); device calls \
             close as at a stop, and their UDP sockets move with it. The plain listener's \
             connections are untouched.",
            now.connections, now.requests, now.sessions
        ));
    }
    out
}

/// The listener's state, in a line.
fn state_line(s: &Option<DeviceListenerStatus>) -> (String, &'static str) {
    match s {
        None => ("reading…".into(), "off"),
        Some(s) => match s.state {
            DeviceListenerState::Off => ("off".into(), "off"),
            DeviceListenerState::Listening => {
                let mut line = format!(
                    "listening on {} · {} connection(s)",
                    s.urls.join(" and "),
                    s.connections
                );
                // Where the bound on connections that have not authenticated
                // bites, it shows.
                if s.unauthenticated > 0 {
                    line.push_str(&format!(" · {} not authenticated", s.unauthenticated));
                }
                if s.evicted > 0 {
                    line.push_str(&format!(" · {} evicted", s.evicted));
                }
                let tone = if s.evicted > 0 { "warn" } else { "ok" };
                (line, tone)
            }
            DeviceListenerState::Error => (
                format!("not serving: {}", s.error.clone().unwrap_or_default()),
                "warn",
            ),
        },
    }
}

/// The device listener's switch, scope and port: acting at once, after a
/// confirmation that names what turns on, widens or ends.
#[component]
pub(super) fn ListenerPanel(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let status = RwSignal::new(None::<DeviceListenerStatus>);
    let refresh = move || {
        spawn_local(async move {
            if let Ok(s) = crate::api::get::<GatewayStatus>("/api/status").await {
                status.set(Some(s.devices.listener));
            }
        })
    };
    refresh();
    let stored = Memo::new(move |_| page.data.with(saved));
    let on = RwSignal::new(false);
    let scope = RwSignal::new("loopback".to_string());
    let addr = RwSignal::new(String::new());
    let port = RwSignal::new(String::new());
    // The boxes follow what is saved whenever the page reads it again.
    Effect::new(move |_| {
        let Some(s) = stored.get() else { return };
        on.set(s.on);
        match s.listen.as_str() {
            "loopback" | "all" => scope.set(s.listen.clone()),
            other => {
                scope.set("address".into());
                addr.set(other.to_string());
            }
        }
        port.set(s.port.map(|p| p.to_string()).unwrap_or_default());
    });
    let edit = Memo::new(move |_| wanted(on.get(), &scope.get(), &addr.get(), &port.get()));
    let changed = Memo::new(move |_| match (edit.get(), stored.get()) {
        (Ok(w), Some(s)) => w.on != s.on || w.listen != s.listen || Some(w.port) != s.port,
        _ => false,
    });
    let confirm = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let text = Memo::new(move |_| {
        let (Ok(to), Some(from)) = (edit.get(), stored.get()) else {
            return Vec::new();
        };
        let now = status.get().unwrap_or_default();
        confirmation(&from, &to, &now)
    });
    let apply = move |_| {
        let Ok(w) = edit.get_untracked() else { return };
        confirm.set(false);
        busy.set(true);
        spawn_local(async move {
            let patch = json!({
                "devices": { "listener": w.on, "listen": w.listen, "port": w.port }
            });
            let res = crate::api::post::<MessageAck, _>("/api/op/settings_set_full", &patch).await;
            busy.set(false);
            match res {
                Ok(a) => {
                    toasts.ok(a.message);
                    // What is saved now, as the page's other rows read it.
                    page.data.update(|d| {
                        if let Some(d) = d.as_mut() {
                            d["devices"]["listener"] = json!(w.on);
                            d["devices"]["listen"] = json!(w.listen);
                            d["devices"]["port"] = json!(w.port);
                        }
                    });
                }
                Err(e) => toasts.err(e.to_string()),
            }
            refresh();
        });
    };
    let scopes = Signal::derive(|| {
        [
            ("loopback", "this computer only (loopback)"),
            ("all", "every address"),
            ("address", "one address"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });
    view! {
        <div class="set-line" id=id hidden=move || hidden.get()>
            <span class="set-line-label">"Device listener"</span>
            {move || {
                let (line, tone) = state_line(&status.get());
                view! {
                    <span class="chip" class:warn=tone == "warn" class:off=tone == "off">
                        <span class="dot"></span>
                        {line}
                    </span>
                }
            }}
        </div>
        <div class="set-line" hidden=move || hidden.get()>
            <label class="check">
                <input
                    type="checkbox"
                    prop:checked=move || on.get()
                    on:change=move |ev| on.set(event_target_checked(&ev))
                />
                " on"
            </label>
            <Select value=scope options=scopes/>
            <input
                class="input mono"
                placeholder="192.0.2.1"
                hidden=move || scope.get() != "address"
                prop:value=move || addr.get()
                on:input=move |ev| addr.set(event_target_value(&ev))
            />
            <span class="dim">"port"</span>
            <input
                class="input mono"
                inputmode="numeric"
                prop:value=move || port.get()
                on:input=move |ev| port.set(event_target_value(&ev))
            />
            <button
                class="btn sm"
                disabled=move || busy.get() || !changed.get()
                title=move || match edit.get() {
                    Err(e) => e,
                    Ok(_) => "Asks first, then turns on, moves or stops the device listener".into(),
                }
                on:click=move |_| confirm.set(true)
            >
                {move || if busy.get() { "Working…" } else { "Apply…" }}
            </button>
            <span class="applies-now">"applies now"</span>
        </div>
        <Modal open=confirm title="Device listener">
            {move || {
                text.get().into_iter().map(|p| view! { <p>{p}</p> }).collect::<Vec<_>>()
            }}
            <div class="row" style="justify-content:flex-end">
                <button class="btn ghost" on:click=move |_| confirm.set(false)>
                    "Keep it as it is"
                </button>
                <button class="btn" on:click=apply>
                    "Apply"
                </button>
            </div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(connections: u64) -> DeviceListenerStatus {
        DeviceListenerStatus {
            state: DeviceListenerState::Listening,
            connections,
            requests: 1,
            sessions: 2,
            ..Default::default()
        }
    }

    #[test]
    fn the_confirmation_names_the_exposure_and_what_ends() {
        let off = Saved {
            on: false,
            listen: "loopback".into(),
            port: Some(8788),
        };
        let on_lo = wanted(true, "loopback", "", "8788").unwrap();
        let text = confirmation(&off, &on_lo, &DeviceListenerStatus::default()).join(" ");
        assert!(text.contains("127.0.0.1 and [::1]"), "{text}");
        assert!(!text.contains("1025–65535"), "{text}");
        assert!(!text.contains("ends"), "nothing runs: {text}");

        let running = Saved {
            on: true,
            ..off.clone()
        };
        let widened = wanted(true, "all", "", "8788").unwrap();
        let text = confirmation(&running, &widened, &now(3)).join(" ");
        assert!(text.contains("every network address"), "{text}");
        assert!(text.contains("1025–65535"), "the exposure is named: {text}");
        assert!(text.contains("3 open connection(s)"), "{text}");
        assert!(text.contains("2 realtime session(s)"), "{text}");
        assert!(text.contains("plain listener's"), "{text}");

        // Turned on where it was saved wide but off: the exposure is named.
        let off_wide = Saved {
            on: false,
            listen: "all".into(),
            port: Some(8788),
        };
        let text = confirmation(&off_wide, &widened, &DeviceListenerStatus::default()).join(" ");
        assert!(text.contains("1025–65535"), "{text}");
        // One loopback address is this computer only, and exposes nothing.
        for lo in ["127.0.0.2", "::1"] {
            let to = wanted(true, "address", lo, "8788").unwrap();
            let text = confirmation(&off, &to, &DeviceListenerStatus::default()).join(" ");
            assert!(!text.contains("1025–65535"), "{lo}: {text}");
            assert!(text.contains("this computer only"), "{lo}: {text}");
        }
        let lan = wanted(true, "address", "192.0.2.1", "8788").unwrap();
        let text = confirmation(&off, &lan, &DeviceListenerStatus::default()).join(" ");
        assert!(text.contains("1025–65535"), "{text}");

        let stop = wanted(false, "loopback", "", "8788").unwrap();
        let text = confirmation(&running, &stop, &now(1)).join(" ");
        assert!(text.contains("turns off"), "{text}");
        assert!(text.contains("1 open connection(s)"), "{text}");
    }

    #[test]
    fn the_state_line_shows_where_the_bound_bites() {
        let quiet = state_line(&Some(now(2)));
        assert_eq!(quiet.1, "ok");
        assert!(!quiet.0.contains("evicted"), "{}", quiet.0);
        let busy = state_line(&Some(DeviceListenerStatus {
            unauthenticated: 5,
            evicted: 3,
            ..now(2)
        }));
        assert!(busy.0.contains("5 not authenticated"), "{}", busy.0);
        assert!(busy.0.contains("3 evicted"), "{}", busy.0);
        assert_eq!(busy.1, "warn");
    }

    #[test]
    fn the_edits_are_checked_before_anything_is_sent() {
        assert!(wanted(true, "address", "192.0.2.1", "8788").is_ok());
        assert!(wanted(true, "address", "the moon", "8788")
            .unwrap_err()
            .contains("no IP address"));
        for bad in ["0", "65536", "", "x"] {
            assert!(wanted(true, "loopback", "", bad).is_err(), "{bad}");
        }
    }
}
