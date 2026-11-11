//! The WebRTC calls' UDP listener (calls design §3.1, §9): its switch and
//! the plain and the TLS socket group's ports, three keys of the `realtime`
//! section (`realtime.webrtc_enabled`, `realtime.webrtc_port`,
//! `realtime.webrtc_tls_port`), kept here and flattened into
//! [`super::RealtimeSettings`].
//!
//! All three apply at the next start, like `bind_addr`: the plain group binds
//! beside the HTTP listener and the TLS group beside the TLS listener
//! (headless server design D22), each released with its listener
//! (`realtime::calls`). The device group follows the device listener live,
//! on its addresses and port, and takes neither key's port; the switch, as
//! the start read it, holds for it too.

use std::net::IpAddr;

use serde::{Deserialize, Deserializer, Serialize};

/// When `realtime.webrtc_enabled` and `realtime.webrtc_port` apply, as a
/// save says it.
pub const WEBRTC_APPLIES: &str = "applies at the next start of lmgw";

/// The keys a save names, with their section.
pub const WEBRTC_KEYS: [&str; 3] = [
    "realtime.webrtc_enabled",
    "realtime.webrtc_port",
    "realtime.webrtc_tls_port",
];

/// `realtime.webrtc_enabled` and `realtime.webrtc_port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WebRtcSettings {
    /// The UDP listener for `POST /v1/realtime/calls`, both socket groups
    /// (§3.1). Off: no UDP socket, and a call is refused 503 naming this
    /// setting. On by default (the owner's Q3).
    pub webrtc_enabled: bool,
    /// The plain group's UDP port (§3.1): `None` = the HTTP port's number
    /// (the owner's Q2), so one number is opened for both and a dev instance
    /// on another HTTP port never collides; `0` = chosen by the OS. The
    /// sockets bind the HTTP listener's IP.
    #[serde(deserialize_with = "port_lenient")]
    pub webrtc_port: Option<u16>,
    /// The TLS group's UDP port (headless server design D22): `None` = the
    /// TLS listener's port number, `0` = chosen by the OS. The sockets bind
    /// `tls.listen`'s IP. A key of its own because on the desktop the plain
    /// and the TLS group can both be bound, and one port for both would
    /// collide.
    #[serde(deserialize_with = "port_lenient")]
    pub webrtc_tls_port: Option<u16>,
}

impl Default for WebRtcSettings {
    fn default() -> Self {
        Self {
            webrtc_enabled: true,
            webrtc_port: None,
            webrtc_tls_port: None,
        }
    }
}

impl WebRtcSettings {
    /// The plain group's port beside an HTTP listener on `http_port`.
    pub fn plain_port(&self, http_port: u16) -> u16 {
        self.webrtc_port.unwrap_or(http_port)
    }

    /// The TLS group's port beside a TLS listener on `tls_port`.
    pub fn tls_port(&self, tls_port: u16) -> u16 {
        self.webrtc_tls_port.unwrap_or(tls_port)
    }

    /// Set `realtime.webrtc_port` from a save's value: `None` is the HTTP
    /// port's number, else a port from 0 (chosen by the OS) to 65535.
    pub fn set_port(&mut self, v: Option<i64>) -> Result<(), String> {
        self.webrtc_port = port_of("realtime.webrtc_port", "the bind address's port", v)?;
        Ok(())
    }

    /// Set `realtime.webrtc_tls_port` from a save's value: `None` is the TLS
    /// port's number, else a port from 0 (chosen by the OS) to 65535.
    pub fn set_tls_port(&mut self, v: Option<i64>) -> Result<(), String> {
        self.webrtc_tls_port = port_of("realtime.webrtc_tls_port", "the TLS address's port", v)?;
        Ok(())
    }
}

/// A save's port for `key`: `None` stays `None` (`unset` is what that
/// means), a number from 0 to 65535 is the port. The refusal names the row.
fn port_of(key: &str, unset: &str, v: Option<i64>) -> Result<Option<u16>, String> {
    match v {
        None => Ok(None),
        Some(n) => u16::try_from(n).map(Some).map_err(|_| {
            format!(
                "{} is a UDP port from 0 (chosen by the OS) to 65535, or unset for {unset}; got \
                 {n}",
                webrtc_name(key)
            )
        }),
    }
}

/// Whether two bind addresses cover a common address: equal, or one a
/// wildcard of a family the other is in. `[::]` counts as dual-stack, as the
/// device group sets it. The TCP listeners' check
/// ([`super::tcp_port_collision`]) asks the same.
pub(super) fn addresses_overlap(a: IpAddr, b: IpAddr) -> bool {
    let (a, b) = (a.to_canonical(), b.to_canonical());
    let covers = |w: IpAddr, x: IpAddr| w.is_unspecified() && (w.is_ipv6() || x.is_ipv4());
    a == b || covers(a, b) || covers(b, a)
}

/// One socket group as the collision check sees it: what to call it, the
/// addresses it binds, and its port.
struct GroupPorts {
    what: String,
    addrs: Vec<IpAddr>,
    port: u16,
}

/// The socket groups `s` binds at a start, each with its addresses and port
/// (calls design §3.1, headless server design D22): the plain group beside
/// `bind_addr`, the TLS group beside `tls.listen` while a TLS listener is
/// configured, the device group beside the device listener while it is on.
fn groups(s: &super::Settings) -> Vec<GroupPorts> {
    use lmgw_api_types::settings_labels::{DEVICE_LISTENER_AT, WEBRTC_PORT_AT, WEBRTC_TLS_PORT_AT};
    let w = &s.realtime.webrtc;
    let mut out = Vec::new();
    if let Ok(http) = s.bind_addr.parse::<std::net::SocketAddr>() {
        let port = w.plain_port(http.port());
        out.push(GroupPorts {
            what: format!(
                "the WebRTC calls UDP port {port} ({}{WEBRTC_PORT_AT})",
                if w.webrtc_port.is_none() {
                    "unset: the bind address's port; "
                } else {
                    ""
                }
            ),
            addrs: vec![http.ip()],
            port,
        });
    }
    if let Some(tls) = s.tls.listener() {
        let port = w.tls_port(tls.addr.port());
        out.push(GroupPorts {
            what: format!(
                "the WebRTC calls TLS UDP port {port} ({}{WEBRTC_TLS_PORT_AT})",
                if w.webrtc_tls_port.is_none() {
                    "unset: the TLS address's port; "
                } else {
                    ""
                }
            ),
            addrs: vec![tls.addr.ip()],
            port,
        });
    }
    if s.devices.listener {
        if let Ok(port) = s.devices.effective_port(&s.bind_addr) {
            out.push(GroupPorts {
                what: format!(
                    "the device listener's port {port} ({}{DEVICE_LISTENER_AT})",
                    if s.devices.port.is_none() {
                        "unset: the bind address's port plus one; "
                    } else {
                        ""
                    }
                ),
                addrs: device_addrs(&s.devices.listen),
                port,
            });
        }
    }
    out
}

/// The addresses the device listener (and its group) binds under `listen`:
/// `[::]` dual-stack for every address, both loopbacks for this computer.
pub(super) fn device_addrs(listen: &super::DevicesListen) -> Vec<IpAddr> {
    use super::DevicesListen::{Addr, All, Loopback};
    match listen {
        All => vec![IpAddr::from([0u16; 8])],
        Loopback => vec![
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]),
        ],
        Addr(ip) => vec![*ip],
    }
}

/// The refusal of a save that would make two socket groups (the plain
/// group, the TLS group and the device group, [`groups`]) want one UDP port
/// on overlapping addresses: the second bind would fail at the start, and
/// that group would never come up. `Ok` when calls are off, a port is chosen
/// by the OS (0), or the addresses do not overlap.
pub fn webrtc_port_collision(s: &super::Settings) -> Result<(), String> {
    if !s.realtime.webrtc.webrtc_enabled {
        return Ok(());
    }
    let groups = groups(s);
    for (i, a) in groups.iter().enumerate() {
        for b in &groups[i + 1..] {
            if a.port == 0 || a.port != b.port {
                continue;
            }
            let overlap = a
                .addrs
                .iter()
                .any(|x| b.addrs.iter().any(|y| addresses_overlap(*x, *y)));
            if overlap {
                return Err(format!(
                    "{} and {} are one UDP port on overlapping addresses (the bind address {}, \
                     the TLS address {}, the device listener on {}): two listeners' WebRTC \
                     sockets cannot both bind it; change one of the two ports",
                    a.what,
                    b.what,
                    s.bind_addr,
                    if s.tls.listen.trim().is_empty() {
                        "unset"
                    } else {
                        s.tls.listen.trim()
                    },
                    match s.devices.listen {
                        super::DevicesListen::All => "every address".to_string(),
                        super::DevicesListen::Loopback => "this computer only".to_string(),
                        super::DevicesListen::Addr(ip) => ip.to_string(),
                    }
                ));
            }
        }
    }
    Ok(())
}

/// What a save's note calls a WebRTC key: its name on the Settings page
/// (Realtime → WebRTC calls), never the config key.
fn webrtc_name(key: &str) -> String {
    use lmgw_api_types::settings_labels::{TLS_UDP_PORT, UDP_PORT, WEBRTC_CALLS};
    match key {
        "realtime.webrtc_port" => format!("the {WEBRTC_CALLS} {UDP_PORT}"),
        "realtime.webrtc_tls_port" => format!("the {WEBRTC_CALLS} {TLS_UDP_PORT}"),
        _ => format!("the {WEBRTC_CALLS} switch"),
    }
}

/// The save's note for each WebRTC key it touched: when it applies. In the
/// server image the next start is the service's restart on the host, and
/// the note names its command (headless server design §6.2): the image has
/// no in-process restart yet.
pub fn webrtc_applies<S: AsRef<str>>(
    deployment: crate::config::deployment::Deployment,
    touched: &[S],
) -> Vec<String> {
    touched
        .iter()
        .map(AsRef::as_ref)
        .filter(|k| WEBRTC_KEYS.contains(k))
        .map(|k| {
            let name = webrtc_name(k);
            if deployment.is_container() {
                format!(
                    "{name} {WEBRTC_APPLIES} ({} on the host)",
                    lmgw_api_types::RESTART_COMMAND
                )
            } else {
                format!("{name} {WEBRTC_APPLIES}")
            }
        })
        .collect()
}

/// A save's `webrtc_port` or `webrtc_tls_port`: absent leaves it, `null` is the HTTP port's
/// number, a number is the port (`Option<Option<_>>`, which serde reads as
/// one level without this).
pub fn nullable_port<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

/// Read tolerantly: a stored port that is no port reads as the HTTP port's
/// number, with a warning naming it — one odd value never fails the whole
/// settings blob.
fn port_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    if raw.is_null() {
        return Ok(None);
    }
    match raw.as_u64().and_then(|n| u16::try_from(n).ok()) {
        Some(p) => Ok(Some(p)),
        None => {
            tracing::warn!(
                "a stored WebRTC UDP port ({raw}) is no UDP port; using the listener's own port \
                 number until it is saved again"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_defaults_to_the_http_ports_number_and_reads_tolerantly() {
        let w = WebRtcSettings::default();
        assert!(w.webrtc_enabled);
        assert_eq!(w.plain_port(8787), 8787);
        let read = |j: &str| serde_json::from_str::<WebRtcSettings>(j).unwrap();
        assert_eq!(read(r#"{"webrtc_port": 0}"#).plain_port(8787), 0);
        assert_eq!(read(r#"{"webrtc_port": 3478}"#).plain_port(8787), 3478);
        assert_eq!(read(r#"{"webrtc_port": null}"#).webrtc_port, None);
        assert_eq!(read(r#"{"webrtc_port": 70000}"#).webrtc_port, None);
        assert_eq!(read(r#"{"webrtc_port": "x"}"#).webrtc_port, None);
        assert!(!read(r#"{"webrtc_enabled": false}"#).webrtc_enabled);
    }

    #[test]
    fn a_save_refuses_a_port_out_of_range_naming_the_setting() {
        let mut w = WebRtcSettings::default();
        w.set_port(Some(0)).unwrap();
        assert_eq!(w.webrtc_port, Some(0));
        w.set_port(None).unwrap();
        assert_eq!(w.webrtc_port, None);
        for bad in [-1, 65536] {
            let e = w.set_port(Some(bad)).unwrap_err();
            assert!(
                e.contains("the WebRTC calls UDP port") && e.contains(&bad.to_string()),
                "{e}"
            );
        }
        use crate::config::deployment::Deployment;
        assert_eq!(
            webrtc_applies(
                Deployment::Desktop,
                &["realtime.webrtc_port", "realtime.threshold"]
            ),
            ["the WebRTC calls UDP port applies at the next start of lmgw"]
        );
        // In the server image the next start is the service's restart.
        assert_eq!(
            webrtc_applies(Deployment::Container, &["realtime.webrtc_enabled"]),
            [
                "the WebRTC calls switch applies at the next start of lmgw (systemctl --user \
                 restart lmgw on the host)"
            ]
        );
    }

    /// The realtime section's save: absent leaves the port, `null` is the
    /// HTTP port's number, a number is the port, out of range is refused.
    #[test]
    fn the_realtime_patch_carries_both_keys() {
        use crate::config::RealtimeSettings;
        use crate::ops::RealtimeSettingsPatch;
        let apply = |r: &mut RealtimeSettings, j: &str| {
            serde_json::from_str::<RealtimeSettingsPatch>(j)
                .unwrap()
                .apply(r)
        };
        let mut r = RealtimeSettings::default();
        let touched = apply(&mut r, r#"{"webrtc_port": 3478, "webrtc_enabled": false}"#).unwrap();
        assert_eq!(r.webrtc.webrtc_port, Some(3478));
        assert!(!r.webrtc.webrtc_enabled);
        assert_eq!(touched, ["realtime.webrtc_enabled", "realtime.webrtc_port"]);
        assert!(apply(&mut r, r#"{"threshold": 0.4}"#).is_ok());
        assert_eq!(r.webrtc.webrtc_port, Some(3478), "left out, kept");
        apply(&mut r, r#"{"webrtc_port": null}"#).unwrap();
        assert_eq!(r.webrtc.webrtc_port, None);
        let e = apply(&mut r, r#"{"webrtc_port": 70000}"#).unwrap_err();
        assert!(
            e.starts_with("the WebRTC calls UDP port is a UDP port"),
            "{e}"
        );
        // Both keys sit in the realtime section of the stored blob, and a
        // blob from before them reads the defaults.
        let blob = serde_json::to_value(RealtimeSettings::default()).unwrap();
        assert_eq!(blob["webrtc_enabled"], true);
        assert!(blob["webrtc_port"].is_null());
        let old: RealtimeSettings = serde_json::from_str(r#"{"ping_interval_s": 7}"#).unwrap();
        assert_eq!(old.webrtc, WebRtcSettings::default());
        assert_eq!(old.ping_interval_s, 7);
    }

    fn ports(
        bind: &str,
        plain: Option<u16>,
        listen: &str,
        device: Option<u16>,
    ) -> super::super::Settings {
        let mut s = super::super::Settings {
            bind_addr: bind.into(),
            ..Default::default()
        };
        s.realtime.webrtc.webrtc_port = plain;
        s.devices.listener = true;
        s.devices.listen = super::super::DevicesListen::parse(listen).unwrap();
        s.devices.port = device;
        s
    }

    #[test]
    fn one_udp_port_on_overlapping_addresses_is_refused_naming_both_settings() {
        // The defaults (HTTP port, and one above it) do not meet.
        webrtc_port_collision(&ports("127.0.0.1:8787", None, "loopback", None)).unwrap();
        // The device port set to the plain group's number, on its address.
        for (bind, listen) in [
            ("127.0.0.1:8787", "loopback"),
            ("127.0.0.1:8787", "all"),
            ("0.0.0.0:8787", "127.0.0.1"),
            ("[::]:8787", "loopback"),
            ("[::1]:8787", "::1"),
        ] {
            let e = webrtc_port_collision(&ports(bind, None, listen, Some(8787))).unwrap_err();
            assert!(
                e.contains("the WebRTC calls UDP port 8787")
                    && e.contains("the device listener's port 8787"),
                "{bind} {listen}: {e}"
            );
            assert!(!e.contains("realtime.") && !e.contains("devices."), "{e}");
        }
        // The plain port set onto the device listener's derived one.
        let e = webrtc_port_collision(&ports("127.0.0.1:8787", Some(8788), "loopback", None))
            .unwrap_err();
        assert!(
            e.contains(
                "the WebRTC calls UDP port 8788 (Settings → Realtime → WebRTC calls → UDP port)"
            ) && e.contains(
                "the device listener's port 8788 (unset: the bind address's port plus one; \
                 Settings → Network & access → Devices → Device listener)"
            ),
            "{e}"
        );
        // Apart: another address, another port, the OS's pick, or off.
        webrtc_port_collision(&ports("127.0.0.1:8787", None, "127.0.0.2", Some(8787))).unwrap();
        webrtc_port_collision(&ports("198.51.100.5:8787", None, "loopback", Some(8787))).unwrap();
        webrtc_port_collision(&ports("127.0.0.1:8787", Some(0), "loopback", Some(8787))).unwrap();
        let mut off = ports("127.0.0.1:8787", None, "loopback", Some(8787));
        off.realtime.webrtc.webrtc_enabled = false;
        webrtc_port_collision(&off).unwrap();
        off.realtime.webrtc.webrtc_enabled = true;
        off.devices.listener = false;
        webrtc_port_collision(&off).unwrap();
    }
}
