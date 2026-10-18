//! The WebRTC calls' UDP listener (calls design §3.1, §9): its switch and
//! the plain socket group's port, two keys of the `realtime` section
//! (`realtime.webrtc_enabled`, `realtime.webrtc_port`), kept here and
//! flattened into [`super::RealtimeSettings`].
//!
//! Both apply at the next start, like `bind_addr`: the plain group binds
//! beside the HTTP listener and is released with it
//! (`realtime::calls`). The device group follows the device listener live,
//! on its addresses and port, and takes neither key's port; the switch, as
//! the start read it, holds for it too.

use std::net::IpAddr;

use serde::{Deserialize, Deserializer, Serialize};

/// When `realtime.webrtc_enabled` and `realtime.webrtc_port` apply, as a
/// save says it.
pub const WEBRTC_APPLIES: &str = "applies at the next start of lmgw";

/// The keys a save names, with their section.
pub const WEBRTC_KEYS: [&str; 2] = ["realtime.webrtc_enabled", "realtime.webrtc_port"];

/// `realtime.webrtc_enabled` and `realtime.webrtc_port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Default for WebRtcSettings {
    fn default() -> Self {
        Self {
            webrtc_enabled: true,
            webrtc_port: None,
        }
    }
}

impl WebRtcSettings {
    /// The plain group's port beside an HTTP listener on `http_port`.
    pub fn plain_port(&self, http_port: u16) -> u16 {
        self.webrtc_port.unwrap_or(http_port)
    }

    /// Set `realtime.webrtc_port` from a save's value: `None` is the HTTP
    /// port's number, else a port from 0 (chosen by the OS) to 65535.
    pub fn set_port(&mut self, v: Option<i64>) -> Result<(), String> {
        self.webrtc_port = match v {
            None => None,
            Some(n) => Some(u16::try_from(n).map_err(|_| {
                format!(
                    "realtime.webrtc_port is a UDP port from 0 (chosen by the OS) to 65535, or \
                     null for the HTTP port's number; got {n}"
                )
            })?),
        };
        Ok(())
    }
}

/// Whether two bind addresses cover a common address: equal, or one a
/// wildcard of a family the other is in. `[::]` counts as dual-stack, as the
/// device group sets it.
fn addresses_overlap(a: IpAddr, b: IpAddr) -> bool {
    let (a, b) = (a.to_canonical(), b.to_canonical());
    let covers = |w: IpAddr, x: IpAddr| w.is_unspecified() && (w.is_ipv6() || x.is_ipv4());
    a == b || covers(a, b) || covers(b, a)
}

/// The refusal of a save that would make the plain group (`bind_addr`'s
/// address on `realtime.webrtc_port`) and the device group (the device
/// listener's addresses on `devices.port`) want one UDP port on overlapping
/// addresses: the second bind would fail at the start, and the device group
/// would never come up. `Ok` when either group is off, a port is chosen by
/// the OS (0), or the addresses do not overlap.
pub fn webrtc_port_collision(s: &super::Settings) -> Result<(), String> {
    if !s.realtime.webrtc.webrtc_enabled || !s.devices.listener {
        return Ok(());
    }
    let Ok(http) = s.bind_addr.parse::<std::net::SocketAddr>() else {
        return Ok(());
    };
    let Ok(device_port) = s.devices.effective_port(&s.bind_addr) else {
        return Ok(());
    };
    let plain_port = s.realtime.webrtc.plain_port(http.port());
    if plain_port == 0 || plain_port != device_port {
        return Ok(());
    }
    use super::DevicesListen::{Addr, All, Loopback};
    let overlap = match s.devices.listen {
        All => true,
        Loopback => [
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]),
        ]
        .into_iter()
        .any(|lo| addresses_overlap(http.ip(), lo)),
        Addr(ip) => addresses_overlap(http.ip(), ip),
    };
    if !overlap {
        return Ok(());
    }
    let plain = match s.realtime.webrtc.webrtc_port {
        Some(_) => format!("realtime.webrtc_port ({plain_port})"),
        None => format!("realtime.webrtc_port (unset: the HTTP port's number, {plain_port})"),
    };
    let device = match s.devices.port {
        Some(_) => format!("devices.port ({device_port})"),
        None => format!("devices.port (unset: the HTTP port's number plus one, {device_port})"),
    };
    Err(format!(
        "{plain} and {device} are one UDP port on overlapping addresses (bind_addr {}, \
         devices.listen {}): the plain listener's and the device listener's WebRTC sockets \
         cannot both bind it; change one of the two ports",
        s.bind_addr,
        s.devices.listen.as_text()
    ))
}

/// The save's note for each WebRTC key it touched: when it applies.
pub fn webrtc_applies<S: AsRef<str>>(touched: &[S]) -> Vec<String> {
    touched
        .iter()
        .map(AsRef::as_ref)
        .filter(|k| WEBRTC_KEYS.contains(k))
        .map(|k| format!("{k} {WEBRTC_APPLIES}"))
        .collect()
}

/// A save's `webrtc_port`: absent leaves it, `null` is the HTTP port's
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
                "stored setting realtime.webrtc_port = {raw} is no UDP port; using the HTTP \
                 port's number until it is saved again"
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
                e.contains("realtime.webrtc_port") && e.contains(&bad.to_string()),
                "{e}"
            );
        }
        assert_eq!(
            webrtc_applies(&["realtime.webrtc_port", "realtime.threshold"]),
            ["realtime.webrtc_port applies at the next start of lmgw"]
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
        assert!(e.contains("realtime.webrtc_port"), "{e}");
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
                e.contains("realtime.webrtc_port") && e.contains("devices.port"),
                "{bind} {listen}: {e}"
            );
        }
        // The plain port set onto the device listener's derived one.
        let e = webrtc_port_collision(&ports("127.0.0.1:8787", Some(8788), "loopback", None))
            .unwrap_err();
        assert!(
            e.contains("realtime.webrtc_port (8788)") && e.contains("devices.port (unset"),
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
