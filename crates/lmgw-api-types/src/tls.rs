//! The main listener's TLS and the public origin as `GET /api/settings-full`
//! reports them (headless server design §4, D8, D27): the `tls` section, and
//! the TLS listener's state with the certificate it serves.

use serde::{Deserialize, Serialize};

/// The `tls` section (Settings → Network & access → TLS).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TlsSettings {
    /// The PEM certificate chain, the leaf first: an absolute path; empty =
    /// no TLS listener.
    pub cert_path: String,
    /// The PEM private key of the leaf: an absolute path; empty = no TLS
    /// listener.
    pub key_path: String,
    /// Where the TLS listener binds, `host:port`; empty = no TLS listener.
    pub listen: String,
    /// The seconds a client gets from its connection to the end of its TLS
    /// handshake.
    pub handshake_timeout_s: u32,
}

/// Where the TLS listener stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TlsListenerState {
    /// No pair or no address is configured.
    #[default]
    Off,
    /// Bound, serving the loaded pair.
    Serving,
    /// Configured, but not started: its pair did not load or its address
    /// did not bind at the start (`error` says which), or the settings were
    /// saved since (a restart applies them).
    NotServing,
}

/// The certificate the TLS listener serves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TlsCertificate {
    /// The names it covers: its DNS and IP subject alternative names, as
    /// written (`*.lmgw-apps.example` for a wildcard).
    pub names: Vec<String>,
    /// Its subject and its issuer, as distinguished names.
    pub subject: String,
    pub issuer: String,
    /// Its serial, lowercase hex.
    pub serial: String,
    /// The end of its validity: UTC, `YYYY-MM-DD HH:MM:SS`.
    pub not_after: String,
    /// When lmgw loaded it: UTC, `YYYY-MM-DD HH:MM:SS`.
    pub loaded_at: String,
}

/// The TLS listener and its certificate (D8): what Settings → Network &
/// access → TLS shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TlsStatus {
    pub state: TlsListenerState,
    /// The address it is bound to, while it serves.
    pub listening: Option<String>,
    /// The pair it serves, while it serves one.
    pub certificate: Option<TlsCertificate>,
    /// Why it does not serve, or why the last reload of a changed pair
    /// failed (the old pair then still serves), with when.
    pub error: Option<String>,
    /// What the certificate does not cover that it should (the public
    /// URL's host, the agents' addresses), each a sentence.
    pub warnings: Vec<String>,
    /// The TLS settings were saved since the listener started: a restart
    /// applies them.
    pub restart_pending: bool,
}

/// Proxy mode as it runs (headless server design §4.5): what Settings →
/// Network & access → TLS shows beside `trusted_proxies`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProxyStatus {
    /// Some trusted proxy is set: forwarded headers are read from it.
    pub on: bool,
    /// The peer of the first request on each main listener since the
    /// start or the last save of the trusted proxies, the first from
    /// outside loopback after a loopback one, and the first from a trusted
    /// peer, each a sentence: the address a proxy reaches lmgw from. Empty
    /// on a desktop with proxy mode off.
    pub first_peers: Vec<String>,
    /// What makes proxy mode unsafe or links wrong, each a sentence: a
    /// trusted peer that is this machine's own address (the outer
    /// container's pasta), a list that trusts everyone, no public URL, an
    /// entry that does not parse.
    pub warnings: Vec<String>,
    /// The requests refused for their forwarded headers since the start:
    /// how many, and the last one's time, peer and reason.
    pub refused: Option<String>,
}
