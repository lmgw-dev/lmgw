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
