//! Connect panel

use serde::{Deserialize, Serialize};

/// `GET /api/connect` — how to reach the gateway and what it exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConnectInfo {
    /// Every URL that reaches the gateway (wildcard binds expand to
    /// loopback + LAN addresses).
    pub bases: Vec<BaseUrl>,
    pub auth_enabled: bool,
    /// Statically known client-facing model names.
    pub models: Vec<ConnectModel>,
    /// Prefixes of enabled `expose_all` upstreams whose catalogs pass
    /// through dynamically (rendered as ghost entries, not copyable names).
    #[serde(default)]
    pub catalog_prefixes: Vec<String>,
    /// The device listener's port (`devices.port`, else the HTTP port plus
    /// one): what "this computer" pairs on, `https://127.0.0.1:<port>`.
    /// `null` when none can be derived (device link design §3.1).
    #[serde(default)]
    pub device_port: Option<u16>,
    /// The URLs the pairing form proposes for another device (device link
    /// design §3.1, §3.2): the addresses a device can count on, the stable
    /// LAN and WireGuard ones first, each with its interface. Empty when
    /// the machine has none, or no port can be derived.
    #[serde(default)]
    pub device_urls: Vec<DeviceUrl>,
}

/// One URL proposed for another device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceUrl {
    /// `https://<address>:<device port>`.
    pub url: String,
    /// The interface the address is on (`eth0`, `wg0`); empty for a
    /// listener set to one address.
    pub interface: String,
    /// The interface is a point-to-point link (WireGuard, Tailscale), not
    /// a LAN's.
    pub wireguard: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BaseUrl {
    pub url: String,
    pub label: String,
}

/// `source` is `alias | local`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConnectModel {
    pub name: String,
    pub source: String,
}
