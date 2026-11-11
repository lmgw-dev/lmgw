//! The launcher: how the model runtime starts, stops, watches and finds the
//! process that serves one model (Windows design §4.1).
//!
//! The registry ([`super::registry`]) decides *when* a model runs — one
//! start per model, the in-flight claims, the reaper, eviction, adoption —
//! and the lifecycle ([`super::lifecycle`]) decides *which*. Neither knows
//! how a model's server becomes a process. That is a [`Launcher`]'s job, and
//! everything the registry asks of the machine about a model's process goes
//! through the one it was built with:
//!
//! - [`Podman`]: one container per model (per-model containers design §3),
//!   the only launcher on Linux. Its verbs are exactly the `podman` calls the
//!   registry made itself before this seam existed — the same argv, through
//!   the same [`CommandRunner`](super::registry::CommandRunner), in the same
//!   order — so a fake runner sees no difference.
//! - [`Native`]: the engine's own executable as lmgw's child process
//!   (Windows design §4.1), the launcher on Windows. On Linux it is built
//!   only through `Registry::with_launcher` (the tests); there is no setting
//!   that selects it yet.
//!
//! **A model's process is named, not held.** Every verb takes the instance
//! name the registry renders ([`super::container_name`]), which is what the
//! registry's entries record: Podman's container name, and the key a native
//! launcher keeps its children under. A start under a name that is already
//! taken replaces what runs under it (Podman's `--replace`).
//!
//! **What stays shared.** The engine's own argv is rendered once, in
//! [`super::argv`], for every launcher ([`super::argv::engine_argv`] with an
//! [`super::argv::EngineView`] saying where the engine sees its files and
//! which port it listens on); the `podman run` wrapper around it is
//! [`Podman`]'s ([`super::argv::podman_run_argv`]). Readiness is the
//! registry's: it polls the server's own HTTP route, whatever started it,
//! and only asks the launcher whether the process is still alive
//! ([`Launcher::exited`]).
//!
//! **Not behind the launcher yet**, and on Podman directly: the engine
//! vocabulary probes (`llama-server --help`, `sd-server --help`, image
//! facts — `Registry::help_text`, `Registry::sdcpp_caps`,
//! `Registry::run_throwaway`, `Registry::image_facts`), the agents' containers
//! (`agents::container`), the benchmark's container (`bench::launcher`), engine
//! builds and pulls (`backends`), MCP stdio containers (`mcp::connect`), and
//! the `rm -f` / `ps` verbs the registry lends those
//! (`Registry::rm_force`, `Registry::ps_filtered`).

mod logs;
pub mod native;
pub mod podman;
mod seam;
mod types;

pub use logs::{log_excerpt_of, LOG_EXCERPT_LINES, LOG_TAIL};
pub use native::Native;
pub use podman::Podman;
pub use seam::Launcher;
pub use types::{
    Exited, Found, Launch, LaunchFailure, Launched, Listed, Listing, Prepare, Prepared, Presence,
    StartWatch, Unwatched,
};
