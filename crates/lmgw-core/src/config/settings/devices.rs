//! The `devices` settings class (device link design §9.1): the device
//! listener's switch, scope and port, and the bounds of its trust — a
//! pairing token's lifetime, the certificates' validity, the handshake's, a
//! request head's and an unauthenticated connection's time, and the
//! failed-enrolment rate. Each bound is visible and stated
//! (`lmgw_api_types::device_link`); none is compiled into the code that
//! uses it.

use std::net::IpAddr;

use lmgw_api_types::device_link as wire;
use serde::{Deserialize, Serialize};

/// Where the device listener binds (device link design §4.1, D24).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DevicesListen {
    /// `127.0.0.1` and `[::1]`: while every paired device is this computer.
    #[default]
    Loopback,
    /// `[::]`: every address, dual-stack.
    All,
    /// One concrete address.
    Addr(IpAddr),
}

impl DevicesListen {
    /// `loopback`, `all`, or the address; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "loopback" => Some(Self::Loopback),
            "all" => Some(Self::All),
            other => other.parse::<IpAddr>().ok().map(Self::Addr),
        }
    }

    /// The setting's text: `loopback`, `all` or the address.
    pub fn as_text(&self) -> String {
        match self {
            Self::Loopback => "loopback".into(),
            Self::All => "all".into(),
            Self::Addr(ip) => ip.to_string(),
        }
    }
}

impl Serialize for DevicesListen {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_text())
    }
}

/// Read tolerantly: a stored value that is none of the three reads as
/// `loopback`, the narrowest scope, with a warning naming it — one odd value
/// never fails the whole settings blob, and never widens what listens.
impl<'de> Deserialize<'de> for DevicesListen {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        let parsed = raw.as_str().and_then(Self::parse);
        Ok(parsed.unwrap_or_else(|| {
            tracing::warn!(
                "stored setting devices.listen = {raw} is not loopback, all or an IP address; \
                 using loopback until it is saved again"
            );
            Self::Loopback
        }))
    }
}

/// Settings → Network & access → Devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicesSettings {
    /// The device listener: off until the first pairing turns it on
    /// (§4.1).
    pub listener: bool,
    /// Where it binds (D24).
    pub listen: DevicesListen,
    /// Its port; `None` = the HTTP port's number plus one (Q1), written as
    /// a number at the listener's first start so that a later change of
    /// `bind_addr` never moves it.
    pub port: Option<u16>,
    /// A pairing token's lifetime, in minutes (§3.3).
    pub token_minutes: u32,
    /// A device certificate's validity, in days; its notice from two thirds
    /// of it (§2.3).
    pub client_cert_days: u32,
    /// A server certificate's validity, in days; re-issued from half of it
    /// (§2.2).
    pub server_cert_days: u32,
    /// A TLS handshake's bound, in seconds (§4.3).
    pub handshake_timeout_s: u32,
    /// A request head's bound and an idle kept-alive connection's, in
    /// seconds (§4.3).
    pub header_timeout_s: u32,
    /// A whole connection without a client certificate, in seconds (§4.3).
    pub unauthenticated_timeout_s: u32,
    /// Failed enrolments per address per minute before a 429 (§3.3).
    pub enrol_refuse_per_min: u32,
    /// Connections without a certificate yet (handshakes included), at
    /// most, from every peer together (§4.3).
    pub unauthenticated_max: u32,
    /// The same, from one peer (an IPv4 address, an IPv6 /64).
    pub unauthenticated_per_peer: u32,
}

impl Default for DevicesSettings {
    fn default() -> Self {
        Self {
            listener: false,
            listen: DevicesListen::Loopback,
            port: None,
            // A QR code a camera can copy lives ten minutes (D8).
            token_minutes: 10,
            // A year, with the notice from month eight (D4).
            client_cert_days: 365,
            // A month, re-issued after two weeks; no device notices (D3).
            server_cert_days: 30,
            handshake_timeout_s: 10,
            header_timeout_s: 30,
            unauthenticated_timeout_s: 10,
            // Far above any device's retries: many phones behind one
            // masquerading WireGuard peer are never refused (D8).
            enrol_refuse_per_min: 120,
            // Each takes a descriptor while it waits, and lmgw's soft limit
            // is 1024: a LAN host idling at the handshake must never starve
            // the process. Far above what devices open (a phone keeps a feed
            // and a request or two).
            unauthenticated_max: 64,
            // A household's phones behind one masquerading WireGuard peer
            // still pair; one host cannot take every slot.
            unauthenticated_per_peer: 8,
        }
    }
}

/// One bounded `devices.*` number: its key, its stated range, what it is,
/// when a change applies, and where it lives.
pub struct DevicesBound {
    /// The key under `devices`.
    pub key: &'static str,
    pub bound: wire::Bound,
    /// What the number is, as a refusal says it.
    pub what: &'static str,
    /// When a change takes effect, as the save's answer says it.
    pub applies: &'static str,
    pub field: fn(&mut DevicesSettings) -> &mut u32,
}

/// The nine bounded numbers, in the settings' order.
pub const DEVICES_BOUNDS: [DevicesBound; 9] = [
    DevicesBound {
        key: "token_minutes",
        bound: wire::TOKEN_MINUTES,
        what: "a pairing token's lifetime in minutes",
        applies: "applies to the next pairing token",
        field: |d| &mut d.token_minutes,
    },
    DevicesBound {
        key: "client_cert_days",
        bound: wire::CLIENT_CERT_DAYS,
        what: "a device certificate's validity in days",
        applies: "applies to the next device certificate issued",
        field: |d| &mut d.client_cert_days,
    },
    DevicesBound {
        key: "server_cert_days",
        bound: wire::SERVER_CERT_DAYS,
        what: "a server certificate's validity in days",
        applies: "applies to the next server certificate issued",
        field: |d| &mut d.server_cert_days,
    },
    DevicesBound {
        key: "handshake_timeout_s",
        bound: wire::HANDSHAKE_TIMEOUT_S,
        what: "a TLS handshake's bound in seconds",
        applies: "applies to the next connection",
        field: |d| &mut d.handshake_timeout_s,
    },
    DevicesBound {
        key: "header_timeout_s",
        bound: wire::HEADER_TIMEOUT_S,
        what: "a request head's bound in seconds",
        applies: "applies to the next connection",
        field: |d| &mut d.header_timeout_s,
    },
    DevicesBound {
        key: "unauthenticated_timeout_s",
        bound: wire::UNAUTHENTICATED_TIMEOUT_S,
        what: "a connection without a client certificate's bound in seconds",
        applies: "applies to the next connection",
        field: |d| &mut d.unauthenticated_timeout_s,
    },
    DevicesBound {
        key: "enrol_refuse_per_min",
        bound: wire::ENROL_REFUSE_PER_MIN,
        what: "failed enrolments per address per minute before a 429",
        applies: "applies at once",
        field: |d| &mut d.enrol_refuse_per_min,
    },
    DevicesBound {
        key: "unauthenticated_max",
        bound: wire::UNAUTHENTICATED_MAX,
        what: "connections not authenticated yet, handshakes included, from every peer",
        applies: "applies to the next connection",
        field: |d| &mut d.unauthenticated_max,
    },
    DevicesBound {
        key: "unauthenticated_per_peer",
        bound: wire::UNAUTHENTICATED_PER_PEER,
        what: "connections not authenticated yet, handshakes included, from one address \
               (IPv6 by /64)",
        applies: "applies to the next connection",
        field: |d| &mut d.unauthenticated_per_peer,
    },
];

impl DevicesSettings {
    /// The port the device listener binds: [`Self::port`], else the HTTP
    /// port's number plus one (Q1). An error names why there is none: a
    /// `bind_addr` that does not parse, or an HTTP port of 65535, which has
    /// no port above it.
    pub fn effective_port(&self, bind_addr: &str) -> Result<u16, String> {
        if let Some(p) = self.port {
            return Ok(p);
        }
        let http = bind_addr
            .parse::<std::net::SocketAddr>()
            .map_err(|_| {
                format!(
                    "devices.port is unset and bind_addr '{bind_addr}' is not host:port, so \
                     there is no HTTP port to add one to; set devices.port"
                )
            })?
            .port();
        http.checked_add(1).ok_or_else(|| {
            "devices.port is unset and the HTTP port is 65535, which has no port above it; set \
             devices.port"
                .to_string()
        })
    }
}
