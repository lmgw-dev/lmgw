//! Where this lmgw runs, and the settings a container start takes from its
//! environment (headless server design §1, §2, D2, D3, D20).
//!
//! **The deployment** is explicit: `LMGW_DEPLOYMENT=container`, which the
//! server image sets, read once by the entry point into the gateway's state
//! ([`crate::state::AppState::deployment`]). Nothing infers it from
//! `/run/.containerenv`. The desktop (the tray app, the dev runner) is the
//! default and behaves as it always has: nothing in this module applies to
//! it.
//!
//! **Pinned** settings ([`Pinned`]) are the ones the unit file and the outer
//! container's port mapping depend on: a dashboard edit of the bind address
//! would leave the published port pointing at nothing. Each container start
//! writes its variable's value over the stored one, so a later start without
//! the variable keeps the last value rather than falling back to the desktop
//! default; the dashboard shows them read-only, and every settings save
//! refuses a change of one ([`refuse_pinned`]).
//!
//! **Seeded** settings ([`Seeded`]) fill a data dir once, from their
//! variable or the image's default, on the first container start that finds
//! [`Settings::container_seeded`] unset — written in the same settings write
//! as the seeds, so a start that dies before it leaves the next one to
//! seed; then the stored setting rules, and the dashboard edits it as on
//! the desktop.
//!
//! The TLS listener's pair and address, the public origin and the TLS
//! WebRTC group's port are pinned as well (§2.1, §4): the TLS listener is
//! the published one, and the unit names its files. So are proxy mode's
//! trusted peers (§4.5, `LMGW_TRUSTED_PROXIES`): which peer may say who the
//! client is is the unit's to decide, beside the port it publishes.
//!
//! **The stop budget** (D20): the unit mirrors its `StopTimeout` into
//! `LMGW_STOP_TIMEOUT`, and [`StopBudget`] compares it with what a quit can
//! take under the stored unload timeout.

use std::net::SocketAddr;
use std::path::PathBuf;

use lmgw_api_types::settings_labels::{self as labels, SettingPath};
use lmgw_api_types::StopBudget as Budget;
use lmgw_api_types::RESTART_COMMAND;
pub use lmgw_api_types::{Deployment, PinnedSetting, PinnedSource, StopBudget};

use super::{DevicesListen, Settings};
use crate::principal::Refusal;

/// [`apply`] and what a start logs of it.
mod apply;
pub use apply::{apply, apply_at_start, loopback_warning, Applied};

/// The variable that names the deployment.
pub const DEPLOYMENT_VAR: &str = "LMGW_DEPLOYMENT";

/// The variable a container start reads its stop budget from (D20): the
/// unit's `StopTimeout`, in seconds.
pub const STOP_TIMEOUT_VAR: &str = "LMGW_STOP_TIMEOUT";

/// The plain listener a fresh data dir in the image gets when
/// `LMGW_BIND_ADDR` is not set (§2.1): inside the container a published
/// port reaches only a non-loopback bind.
pub const IMAGE_BIND_ADDR: &str = "0.0.0.0:8787";

/// [`DEPLOYMENT_VAR`]'s value read: unset or empty is the desktop,
/// `container` the server image; anything else is refused, naming the
/// value, since a guessed deployment is worse than a start that stops.
pub fn parse_deployment(v: Option<&str>) -> Result<Deployment, String> {
    match v.map(str::trim) {
        None | Some("") => Ok(Deployment::Desktop),
        Some("container") => Ok(Deployment::Container),
        Some(other) => Err(format!(
            "{DEPLOYMENT_VAR}={other:?} is not a deployment lmgw knows: `container` (the server \
             image) or unset (the desktop)"
        )),
    }
}

/// A setting a container start pins (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pinned {
    /// `bind_addr`, the plain listener.
    BindAddr,
    /// `realtime.webrtc_port`, the plain WebRTC group's UDP port, which must
    /// match the unit's UDP `PublishPort` (D22).
    WebrtcPort,
    /// `tls.cert_path`, the TLS listener's certificate (§4, D8).
    TlsCert,
    /// `tls.key_path`, its private key.
    TlsKey,
    /// `tls.listen`, the TLS listener's address: the published one.
    TlsListen,
    /// `public_url`, the one public origin (D9).
    PublicUrl,
    /// `realtime.webrtc_tls_port`, the TLS WebRTC group's UDP port (D22).
    WebrtcTlsPort,
    /// `trusted_proxies`, proxy mode's trusted peers (§4.5).
    TrustedProxies,
}

impl Pinned {
    /// Every pinned setting, in the order the start applies them.
    pub const ALL: [Pinned; 8] = [
        Pinned::BindAddr,
        Pinned::WebrtcPort,
        Pinned::TlsCert,
        Pinned::TlsKey,
        Pinned::TlsListen,
        Pinned::PublicUrl,
        Pinned::WebrtcTlsPort,
        Pinned::TrustedProxies,
    ];

    /// The setting's dotted save path, as the dashboard's patch names it.
    pub fn setting(self) -> &'static str {
        match self {
            Self::BindAddr => "bind_addr",
            Self::WebrtcPort => "realtime.webrtc_port",
            Self::TlsCert => "tls.cert_path",
            Self::TlsKey => "tls.key_path",
            Self::TlsListen => "tls.listen",
            Self::PublicUrl => "public_url",
            Self::WebrtcTlsPort => "realtime.webrtc_tls_port",
            Self::TrustedProxies => "trusted_proxies",
        }
    }

    /// The variable that sets it.
    pub fn variable(self) -> &'static str {
        match self {
            Self::BindAddr => "LMGW_BIND_ADDR",
            Self::WebrtcPort => "LMGW_WEBRTC_PORT",
            Self::TlsCert => "LMGW_TLS_CERT",
            Self::TlsKey => "LMGW_TLS_KEY",
            Self::TlsListen => "LMGW_TLS_LISTEN",
            Self::PublicUrl => "LMGW_PUBLIC_URL",
            Self::WebrtcTlsPort => "LMGW_WEBRTC_TLS_PORT",
            Self::TrustedProxies => "LMGW_TRUSTED_PROXIES",
        }
    }

    /// Where the dashboard shows it.
    pub fn place(self) -> SettingPath {
        match self {
            Self::BindAddr => labels::BIND_ADDRESS_AT,
            Self::WebrtcPort => labels::WEBRTC_PORT_AT,
            Self::TlsCert => labels::TLS_CERT_AT,
            Self::TlsKey => labels::TLS_KEY_AT,
            Self::TlsListen => labels::TLS_LISTEN_AT,
            Self::PublicUrl => labels::PUBLIC_URL_AT,
            Self::WebrtcTlsPort => labels::WEBRTC_TLS_PORT_AT,
            Self::TrustedProxies => labels::TRUSTED_PROXIES_AT,
        }
    }

    /// The stored value, for comparing a save against.
    fn value(self, s: &Settings) -> String {
        match self {
            Self::BindAddr => s.bind_addr.clone(),
            Self::WebrtcPort => format!("{:?}", s.realtime.webrtc.webrtc_port),
            Self::TlsCert => s.tls.cert_path.clone(),
            Self::TlsKey => s.tls.key_path.clone(),
            Self::TlsListen => s.tls.listen.clone(),
            Self::PublicUrl => s.public_url.clone(),
            Self::WebrtcTlsPort => format!("{:?}", s.realtime.webrtc.webrtc_tls_port),
            Self::TrustedProxies => s.trusted_proxies.join(", "),
        }
    }

    /// What the row says, and what a save of it is refused with, for a value
    /// that came from `source` on this start.
    pub fn why(self, source: PinnedSource) -> String {
        let var = self.variable();
        match source {
            PinnedSource::Variable => format!(
                "Set by this server's container ({var}). Change the unit file and restart the \
                 service ({RESTART_COMMAND})."
            ),
            PinnedSource::Kept => format!(
                "Kept as stored; {var} is not set on this start. Set it in the unit file and \
                 restart the service ({RESTART_COMMAND}) to change it."
            ),
            PinnedSource::ImageDefault => format!(
                "The server image's default; {var} is not set. Set it in the unit file and \
                 restart the service ({RESTART_COMMAND}) to change it."
            ),
        }
    }

    /// The answer's entry for it.
    pub fn dto(self, source: PinnedSource) -> PinnedSetting {
        PinnedSetting {
            setting: self.setting().into(),
            variable: self.variable().into(),
            source,
            why: self.why(source),
        }
    }

    /// Its place in [`Pinned::ALL`] and [`StartFacts::sources`].
    fn index(self) -> usize {
        self as usize
    }
}

/// The settings `deployment` pins: every [`Pinned`] in container mode, none
/// on the desktop.
pub fn pinned(deployment: Deployment) -> &'static [Pinned] {
    if deployment.is_container() {
        &Pinned::ALL
    } else {
        &[]
    }
}

/// The code every settings save path refuses a pinned field with.
pub const PINNED_CODE: &str = "setting_pinned";

/// The shared settings write's check (§2.2): a save that would change a
/// setting this start pins is refused with `400 setting_pinned`, naming
/// where the setting is and its variable. Every save path asks it with the
/// settings before and after its changes, before it writes; a save that
/// carries a pinned field at its stored value goes through, as the
/// dashboard's full save may.
pub fn refuse_pinned(
    facts: StartFacts,
    before: &Settings,
    after: &Settings,
) -> Result<(), Refusal> {
    for p in pinned(facts.deployment) {
        if p.value(before) != p.value(after) {
            return Err(Refusal {
                status: axum::http::StatusCode::BAD_REQUEST,
                code: PINNED_CODE,
                message: format!("{}: {}", p.place(), p.why(facts.source(*p))),
            });
        }
    }
    Ok(())
}

/// A setting a container start seeds into a fresh data dir (§2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seeded {
    /// `auth_enabled` (ruling 9): on in the image.
    RequireApiKey,
    /// `devices.listen`: every address in the image, since the desktop's
    /// loopback default is unreachable through a published port.
    DevicesListen,
}

impl Seeded {
    /// Every seeded setting, in the order the start applies them.
    pub const ALL: [Seeded; 2] = [Seeded::RequireApiKey, Seeded::DevicesListen];

    /// The setting's dotted save path.
    pub fn setting(self) -> &'static str {
        match self {
            Self::RequireApiKey => "auth_enabled",
            Self::DevicesListen => "devices.listen",
        }
    }

    /// The variable that seeds it.
    pub fn variable(self) -> &'static str {
        match self {
            Self::RequireApiKey => "LMGW_REQUIRE_API_KEY",
            Self::DevicesListen => "LMGW_DEVICES_LISTEN",
        }
    }
}

/// What a container start's environment says, parsed (the entry point reads
/// and parses the variables, refusing one that does not parse): each pinned
/// and seeded setting's value, `None` where its variable is unset, and the
/// unit's stop budget.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerEnv {
    pub bind_addr: Option<SocketAddr>,
    pub webrtc_port: Option<u16>,
    /// `LMGW_TLS_CERT` and `LMGW_TLS_KEY`: absolute paths, both or neither
    /// (the entry point refuses half a pair).
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_listen: Option<SocketAddr>,
    /// `LMGW_PUBLIC_URL`, read and spelled as the origin it is
    /// (`crate::config::PublicUrl::base`).
    pub public_url: Option<String>,
    pub webrtc_tls_port: Option<u16>,
    /// `LMGW_TRUSTED_PROXIES`, read and written as the networks they are
    /// (`crate::config::normalize_trusted_proxies`).
    pub trusted_proxies: Option<Vec<String>>,
    pub require_api_key: Option<bool>,
    pub devices_listen: Option<DevicesListen>,
    pub stop_timeout: Option<u64>,
}

impl ContainerEnv {
    /// The variables of this environment that are set, by name.
    pub fn set_variables(&self) -> Vec<&'static str> {
        let mut set = Vec::new();
        if self.bind_addr.is_some() {
            set.push(Pinned::BindAddr.variable());
        }
        if self.webrtc_port.is_some() {
            set.push(Pinned::WebrtcPort.variable());
        }
        if self.tls_cert.is_some() {
            set.push(Pinned::TlsCert.variable());
        }
        if self.tls_key.is_some() {
            set.push(Pinned::TlsKey.variable());
        }
        if self.tls_listen.is_some() {
            set.push(Pinned::TlsListen.variable());
        }
        if self.public_url.is_some() {
            set.push(Pinned::PublicUrl.variable());
        }
        if self.webrtc_tls_port.is_some() {
            set.push(Pinned::WebrtcTlsPort.variable());
        }
        if self.trusted_proxies.is_some() {
            set.push(Pinned::TrustedProxies.variable());
        }
        if self.require_api_key.is_some() {
            set.push(Seeded::RequireApiKey.variable());
        }
        if self.devices_listen.is_some() {
            set.push(Seeded::DevicesListen.variable());
        }
        if self.stop_timeout.is_some() {
            set.push(STOP_TIMEOUT_VAR);
        }
        set
    }
}

/// What a gateway's entry point tells it about its start: the deployment
/// and, in container mode, its environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Start {
    pub deployment: Deployment,
    pub env: ContainerEnv,
}

impl Start {
    /// The desktop's start: nothing pinned, nothing seeded.
    pub fn desktop() -> Self {
        Self::default()
    }
}

/// What a running gateway keeps of its start ([`crate::state::AppState`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartFacts {
    pub deployment: Deployment,
    /// `LMGW_STOP_TIMEOUT`, in container mode.
    pub stop_timeout: Option<u64>,
    /// Where each [`Pinned`] setting's value came from on this start, in
    /// [`Pinned::ALL`]'s order ([`apply`]'s answer).
    pub sources: [PinnedSource; Pinned::ALL.len()],
}

impl StartFacts {
    /// Where `p`'s value came from on this start.
    pub fn source(&self, p: Pinned) -> PinnedSource {
        self.sources[p.index()]
    }
}

impl From<&Start> for StartFacts {
    /// What `start` says before [`apply`] has run: a pinned variable that is
    /// set is the value's source; an unset one keeps the stored value, but
    /// the TLS pair, the public URL and the trusted proxies, which an unset
    /// variable clears to the image's default of none.
    fn from(start: &Start) -> Self {
        let container = start.deployment.is_container();
        let set = |is_set: bool, unset: PinnedSource| {
            if is_set {
                PinnedSource::Variable
            } else {
                unset
            }
        };
        let (kept, cleared) = (PinnedSource::Kept, PinnedSource::ImageDefault);
        let e = &start.env;
        Self {
            deployment: start.deployment,
            stop_timeout: container.then_some(e.stop_timeout).flatten(),
            sources: [
                set(e.bind_addr.is_some(), kept),
                set(e.webrtc_port.is_some(), kept),
                set(e.tls_cert.is_some(), cleared),
                set(e.tls_key.is_some(), cleared),
                set(e.tls_listen.is_some(), kept),
                set(e.public_url.is_some(), cleared),
                set(e.webrtc_tls_port.is_some(), kept),
                set(e.trusted_proxies.is_some(), cleared),
            ],
        }
    }
}

/// The seconds a quit can take at most besides the unload timeout (Today,
/// "The quit's bounds"): the server's stop ([`crate::server::STOP_WITHIN`]),
/// a running benchmark's abort before its unload timeout (the registry's
/// stop grace) and the model containers' stop
/// ([`crate::runtime::lifecycle::SHUTDOWN_TIMEOUT`]).
pub fn quit_constant_seconds() -> u64 {
    crate::server::STOP_WITHIN.as_secs()
        + crate::runtime::registry::STOP_GRACE_SECONDS
        + crate::runtime::lifecycle::SHUTDOWN_TIMEOUT.as_secs()
}

/// The stop budget of a container start told `unit_seconds` by its unit,
/// with the stored unload timeout `unload_timeout_seconds` (D20).
pub fn stop_budget(unit_seconds: Option<u64>, unload_timeout_seconds: u64) -> Budget {
    let constant = quit_constant_seconds();
    let needed = constant.saturating_add(unload_timeout_seconds);
    let parts = format!(
        "{} s for the server's own streams, {} s plus the unload timeout of \
         {unload_timeout_seconds} s for a running benchmark, {} s for the model containers",
        crate::server::STOP_WITHIN.as_secs(),
        crate::runtime::registry::STOP_GRACE_SECONDS,
        crate::runtime::lifecycle::SHUTDOWN_TIMEOUT.as_secs(),
    );
    let (over, sentence) = match unit_seconds {
        None => (
            false,
            format!(
                "{STOP_TIMEOUT_VAR} is not set, so lmgw cannot compare its quit with the unit's \
                 StopTimeout; a quit can take {needed} s ({parts})"
            ),
        ),
        Some(unit) if needed > unit => (
            true,
            format!(
                "this server's unit gives lmgw {unit} s to stop; with an unload timeout of \
                 {unload_timeout_seconds} s a quit can take {needed} s. Raise StopTimeout, \
                 {STOP_TIMEOUT_VAR} and TimeoutStopSec in the unit, or lower the unload timeout \
                 ({})",
                labels::UNLOAD_TIMEOUT_AT
            ),
        ),
        Some(unit) => (
            false,
            format!(
                "this server's unit gives lmgw {unit} s to stop; a quit takes at most {needed} s \
                 ({parts})"
            ),
        ),
    };
    Budget {
        unit_seconds,
        needed_seconds: needed,
        unload_timeout_seconds,
        over,
        sentence,
    }
}

#[cfg(test)]
mod tests;
