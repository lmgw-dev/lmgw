//! What crosses the [`Launcher`](super::Launcher) seam.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::runtime::argv::RenderSpec;
use crate::runtime::{Class, Placement};

/// What one start asks [`Launcher::prepare`](super::Launcher::prepare) to
/// ready, once, before a port is taken: the model's class (which engine it
/// runs), its resolved run args and where it computes.
#[derive(Debug, Clone, Copy)]
pub struct Prepare<'a> {
    /// The `container_prefix` setting.
    pub container_prefix: &'a str,
    /// The model's class: which engine serves it ([`Class::engine`]).
    pub class: Class,
    /// The model's resolved `extra_run_args`: Podman's flags, and the source
    /// of the card a native process may see ([`super::native`]'s GPU
    /// selection).
    pub run_args: &'a [String],
    /// Where the model computes.
    pub placement: Placement,
    /// What the start may spend readying (`vram.load_timeout_seconds`).
    pub budget: Duration,
    /// The start's line back to its entry while this prepares ([`StartWatch`]).
    pub watch: &'a dyn StartWatch,
}

/// A start's line back to the registry while its
/// [`Launcher::prepare`](super::Launcher::prepare) works, which on a native
/// engine's first start is a download of minutes (Windows design §4.2):
/// whether the start is still wanted, and a note for its entry.
pub trait StartWatch: Send + Sync {
    /// The start's entry is still this start's: `false` once a stop, the
    /// GPU hold or an eviction has taken it. A prepare stops what it can
    /// stop cleanly (a wait, a download) when it is not.
    fn wanted(&self) -> bool;
    /// Put `note` on the starting entry's warnings, where the dashboard
    /// shows it while the start goes on (a wait for another lmgw's install).
    fn note(&self, note: String);
}

impl std::fmt::Debug for dyn StartWatch + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartWatch")
    }
}

/// A prepare with no registry entry behind it: always wanted, its notes go
/// to lmgw's log.
pub struct Unwatched;

impl StartWatch for Unwatched {
    fn wanted(&self) -> bool {
        true
    }

    fn note(&self, note: String) {
        tracing::info!("{note}");
    }
}

/// What [`Launcher::prepare`](super::Launcher::prepare) readied for one
/// start, handed to every [`Launch`] attempt of it. Opaque to the registry:
/// its contents are the launcher's.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    /// Podman's global options for this start (`--cdi-spec-dir …` for a GPU
    /// container, machine-check design §3.6.4); empty where none is needed.
    pub(super) podman_globals: Vec<String>,
    /// The native launcher's resolved engine install and process environment
    /// ([`super::native`]); `None` from every other launcher.
    pub(super) native: Option<Arc<super::native::Resolved>>,
}

impl Prepared {
    /// The engine build the native launcher resolved for this start
    /// (`b12244-cuda12`); `None` where the image names it (Podman).
    pub fn build(&self) -> Option<&str> {
        self.native.as_ref().map(|r| r.build())
    }
}

/// One start attempt: the model's render plus what the start readied.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    /// The model's render at this attempt's port — engine, files, run args,
    /// instance name and labels' inputs ([`RenderSpec`]).
    pub render: &'a RenderSpec,
    pub prepared: &'a Prepared,
    /// The lmgw instance that starts it, stamped on what is started so the
    /// pass after boot can tell its own from another instance's
    /// ([`crate::runtime::argv::OWNER_LABEL`]); `None` stamps nothing.
    pub owner: Option<&'a str>,
    /// Where the model computes. Podman reads the GPU from the run args the
    /// render carries; a launcher without run args selects the card (or
    /// none) from this.
    pub placement: Placement,
}

/// A launch that went through.
#[derive(Debug, Clone, Default)]
pub struct Launched {
    /// What the start says without failing (Podman: the restart policy a GPU
    /// container lost, D27), for the entry's warnings.
    pub warnings: Vec<String>,
}

/// A launch that did not go through.
#[derive(Debug, Clone)]
pub struct LaunchFailure {
    /// The sentence, naming what failed (`podman run failed (exit 125): …`).
    pub message: String,
    /// The published port was taken between its probe and the bind (§10.4):
    /// worth one more attempt on a fresh port.
    pub port_in_use: bool,
}

/// A launched instance that is no longer running, as
/// [`Launcher::exited`](super::Launcher::exited) reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exited {
    /// The sentence, naming what ended (`the container exited with code 1 …`).
    pub message: String,
    /// It ended because its port was taken by something else: the window
    /// between the port's probe and the bind (§10.4), which a native process
    /// only reports by exiting. The registry gives such a start its one more
    /// attempt on a fresh port, as it does a [`LaunchFailure::port_in_use`].
    pub port_in_use: bool,
    /// The build that ran cannot serve on this machine, the launcher has
    /// marked it and has another build of the engine for the next
    /// [`Launcher::prepare`](super::Launcher::prepare): the sentence that
    /// says which and why (the native launcher's Vulkan fallback, Windows
    /// design §4.2). The registry gives such a start one more go: it
    /// prepares again (where the next build is installed, outside the load
    /// timeout) and launches with a fresh load budget.
    pub fallback: Option<String>,
}

impl Exited {
    /// An exit that says nothing about the port or a fallback.
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            port_in_use: false,
            fallback: None,
        }
    }
}

/// Which instances [`Launcher::list`](super::Launcher::list) names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// Running or not (`podman ps -a`): boot reconciliation, which also
    /// collects the stopped leftovers.
    All,
    /// Running only (`podman ps`): the pass after boot, for which a stopped
    /// instance holds no memory.
    Running,
}

/// One listed instance (`podman ps --format json`'s fields that adoption
/// reads).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Listed {
    #[serde(default, rename = "Names")]
    pub names: Vec<String>,
    #[serde(default, rename = "Labels")]
    pub labels: HashMap<String, String>,
    /// `running`, `exited`, `created`, … as the launcher spells it.
    #[serde(default, rename = "State")]
    pub state: String,
    /// When it was created, in unix seconds. `0` when unknown, which reads
    /// as "long ago" everywhere it is compared — the safe direction for a
    /// reconciliation that removes things.
    #[serde(default, rename = "Created")]
    pub created: i64,
}

impl Listed {
    /// The instance's name: its first.
    pub fn name(&self) -> Option<&str> {
        self.names.first().map(String::as_str)
    }
}

/// What [`Launcher::inspect`](super::Launcher::inspect) read of one instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// `None` when the answer named no state at all.
    pub running: Option<bool>,
    pub labels: HashMap<String, String>,
    /// The command it runs after the image (Podman's `Config.Cmd`): for a
    /// managed container, exactly the engine argv
    /// ([`crate::runtime::argv::render_engine_args`]).
    pub cmd: Vec<String>,
    /// The host port its server is published on, when one is.
    pub host_port: Option<u16>,
}

/// What [`Launcher::exists`](super::Launcher::exists) could establish about
/// one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// The launcher answered, and the instance is there.
    Present,
    /// The launcher answered that there is no such instance.
    Absent,
    /// The launcher did not answer — carries the rendered reason.
    Unknown(String),
}
