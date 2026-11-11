//! The [`Launcher`] trait.

use std::collections::HashMap;
use std::time::Duration;

use super::types::{
    Exited, Found, Launch, LaunchFailure, Launched, Listed, Listing, Prepare, Prepared, Presence,
};
use crate::erased::Erased;
use crate::runtime::argv::RenderSpec;

/// How the model runtime reaches the processes that serve its models
/// (module doc). One per registry, shared by every start, stop and pass.
///
/// Every method is a question about one instance name, or about the
/// instances labelled for one `container_prefix`. Answers come back as text
/// a caller can quote: each launcher words its own failures, so an error a
/// user reads names what actually failed (`podman stop`, a process that
/// would not exit).
pub trait Launcher: Send + Sync {
    /// Whether this launcher's instances are Podman containers. What only a
    /// container has is asked of Podman only where this is true: the
    /// router-mode sweep (`lifecycle`), whose old containers a native
    /// launcher cannot see, and an engine's flag vocabulary, which is read
    /// from its image (`Registry::sdcpp_caps`).
    fn runs_containers(&self) -> bool;

    // -- start ---------------------------------------------------------------

    /// Ready the machine for one start ([`Prepare`]: the model's class, run
    /// args and placement, within `budget`, `vram.load_timeout_seconds`).
    /// Asked once per start, before a port is taken; every [`Self::launch`]
    /// attempt of that start carries the answer. `Err` is the start's answer
    /// as it is.
    ///
    /// Podman: lmgw's CDI spec, made true of the machine before a GPU start,
    /// and the global options pointing Podman at it (machine-check design
    /// §3.6). Native: the engine's executable, found in its install, and the
    /// environment that selects the card.
    fn prepare<'a>(&'a self, prepare: Prepare<'a>) -> Erased<'a, Result<Prepared, String>>;

    /// Start the model `launch` renders, under `launch.render.container_name`,
    /// replacing whatever runs under that name. Returns once the process is
    /// started — not once it serves, which the registry polls for itself.
    ///
    /// A failure that left anything behind under the name is the caller's to
    /// collect ([`Self::remove`]); [`LaunchFailure::port_in_use`] says the
    /// published port was taken between its probe and the bind, which the
    /// registry retries once on a fresh port.
    fn launch<'a>(&'a self, launch: Launch<'a>) -> Erased<'a, Result<Launched, LaunchFailure>>;

    /// `Some` once a launched instance is known to be no longer running — it
    /// exited, or it was removed — so a readiness poll can stop at once
    /// instead of waiting out the load timeout. `None` is "running, or not
    /// known": a doubt never fails a start that might still come up. An exit
    /// because the port was taken says so ([`Exited::port_in_use`]), and the
    /// registry retries it as it retries a [`LaunchFailure::port_in_use`].
    /// An exit that asks for the engine's next build ([`Exited::fallback`])
    /// gets one more `prepare` and launch.
    fn exited<'a>(&'a self, name: &'a str) -> Erased<'a, Option<Exited>>;

    // -- stop ----------------------------------------------------------------

    /// Stop `name` and wait until its process is gone — the moment its
    /// memory is free, which eviction forwards on — within the stop grace
    /// plus `unload_timeout` (`vram.unload_timeout_seconds`). `None` when it
    /// is gone (or was never there); `Some` is what went wrong. A stopped
    /// instance keeps its logs until the next start under its name.
    fn stop<'a>(&'a self, name: &'a str, unload_timeout: Duration) -> Erased<'a, Option<String>>;

    /// Ask `name` to stop, without waiting for it to be gone, and log
    /// rather than return what went wrong: hygiene on the way out of a start
    /// that already failed.
    fn stop_quietly<'a>(&'a self, name: &'a str) -> Erased<'a, ()>;

    /// Kill `name` if it runs and drop everything kept of it, logs included.
    /// Gone already is done.
    fn remove<'a>(&'a self, name: &'a str) -> Erased<'a, Result<(), String>>;

    // -- logs ----------------------------------------------------------------

    /// The last `lines` lines `name` printed, stdout and stderr, as they are.
    fn logs_tail<'a>(&'a self, name: &'a str, lines: usize) -> Erased<'a, Result<String, String>>;

    /// The lines of `name`'s log tail that look like a diagnosis, for a
    /// failed start's error ([`super::log_excerpt_of`]); empty when nothing
    /// could be read.
    fn log_excerpt<'a>(&'a self, name: &'a str) -> Erased<'a, Vec<String>>;

    // -- existence and adoption ---------------------------------------------

    /// Whether anything, running or not, exists under `name`.
    fn exists<'a>(&'a self, name: &'a str) -> Erased<'a, Presence>;

    /// Every instance labelled for `container_prefix` ([`Listing`] says
    /// whether stopped ones count): what boot reconciliation and the pass
    /// after it compare the registry with (per-model containers §3.4). A
    /// launcher whose instances never outlive lmgw lists none, and so has
    /// nothing to adopt.
    fn list<'a>(
        &'a self,
        container_prefix: &'a str,
        which: Listing,
    ) -> Erased<'a, Result<Vec<Listed>, String>>;

    /// What one instance runs: its state, labels, command and published
    /// port — what adoption reads before it compares.
    fn inspect<'a>(&'a self, name: &'a str) -> Erased<'a, Result<Found, String>>;

    /// Would launching `render` produce exactly the instance `found` (with
    /// the labels it was listed with)? `Err` says why not. The registry asks
    /// this for every candidate rung before it adopts; the engine-side
    /// comparisons (audio's `server.json`, the readiness probe) are its own.
    fn same_start(
        &self,
        container_prefix: &str,
        render: &RenderSpec,
        found: &Found,
        labels: &HashMap<String, String>,
    ) -> Result<(), String>;

    // -- VRAM attribution ----------------------------------------------------

    /// The host PID of each named instance's root process, in one call
    /// (candidate-aliases design §4.7). A name that is not running is absent
    /// from the map; `Err` only when nothing could be learned at all. The
    /// processes under each root are read from the host's process tree by
    /// the caller ([`crate::vram::attribution`]).
    fn host_pids<'a>(
        &'a self,
        names: &'a [String],
    ) -> Erased<'a, Result<HashMap<String, u32>, String>>;
}
