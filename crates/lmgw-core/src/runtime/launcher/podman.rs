//! The Podman launcher: one container per model (per-model containers design
//! §3), `podman` through the registry's [`CommandRunner`].
//!
//! Every verb is one the registry shelled itself before the launcher seam
//! existed, moved here unchanged: the same argv, the same reading of
//! podman's answers, the same sentences. The `podman run` argv is
//! [`crate::runtime::argv::podman_run_argv`]'s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use super::seam::Launcher;
use super::types::{
    Exited, Found, Launch, LaunchFailure, Launched, Listed, Listing, Prepare, Prepared, Presence,
};
use crate::erased::Erased;
use crate::runtime::argv::RenderSpec;
use crate::runtime::cdi::OwnSpec;
use crate::runtime::registry::{CmdOutput, CommandRunner};

mod listing;
mod logs;
mod start;
mod stop;

pub(crate) use listing::decode_ps;
pub use listing::HOST_PID_FORMAT;
pub(crate) use stop::rm_force_outcome;

/// The Podman launcher (module doc).
pub struct Podman {
    runner: Arc<dyn CommandRunner>,
    /// lmgw's own CDI spec, shared with the registry that set it
    /// (`Registry::set_cdi`): every GPU podman call carries its
    /// `--cdi-spec-dir`, and one that would not is refused.
    cdi: Arc<ArcSwap<OwnSpec>>,
}

impl Podman {
    pub fn new(runner: Arc<dyn CommandRunner>, cdi: Arc<ArcSwap<OwnSpec>>) -> Self {
        Self { runner, cdi }
    }

    fn cdi(&self) -> Arc<OwnSpec> {
        self.cdi.load_full()
    }

    async fn podman(&self, args: &[&str]) -> std::io::Result<CmdOutput> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.podman_argv(&argv).await
    }

    async fn podman_argv(&self, argv: &[String]) -> std::io::Result<CmdOutput> {
        run(&*self.runner, &self.cdi(), argv).await
    }
}

/// `podman <argv>` through `runner` — refused when it gives a container the
/// graphics card without lmgw's CDI spec dir
/// ([`OwnSpec::refuse_unpointed`]). The one way podman is shelled for a
/// model, by this launcher and by the registry's own verbs outside it.
pub(crate) async fn run(
    runner: &dyn CommandRunner,
    cdi: &OwnSpec,
    argv: &[String],
) -> std::io::Result<CmdOutput> {
    cdi.refuse_unpointed(argv)
        .map_err(|why| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why))?;
    runner.run("podman", argv).await
}

/// podman's "this name is not a container" on stderr: the requested state
/// for a stop or a removal, and a clear answer for an existence check.
pub(crate) fn is_no_such_container(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    low.contains("no such container") || low.contains("no container with name")
}

impl Launcher for Podman {
    fn runs_containers(&self) -> bool {
        true
    }

    fn prepare<'a>(&'a self, prepare: Prepare<'a>) -> Erased<'a, Result<Prepared, String>> {
        Box::pin(async move {
            self.prepare_start(prepare.container_prefix, prepare.run_args, prepare.budget)
                .await
        })
    }

    fn launch<'a>(&'a self, launch: Launch<'a>) -> Erased<'a, Result<Launched, LaunchFailure>> {
        Box::pin(async move { self.run_container(launch).await })
    }

    fn exited<'a>(&'a self, name: &'a str) -> Erased<'a, Option<Exited>> {
        // Podman binds the published port itself, at `podman run`: a taken
        // port is a launch failure there, never an exit.
        Box::pin(async move { self.container_exited(name).await.map(Exited::plain) })
    }

    fn stop<'a>(&'a self, name: &'a str, unload_timeout: Duration) -> Erased<'a, Option<String>> {
        Box::pin(async move { self.stop_and_wait(name, unload_timeout).await })
    }

    fn stop_quietly<'a>(&'a self, name: &'a str) -> Erased<'a, ()> {
        Box::pin(async move { self.stop_best_effort(name).await })
    }

    fn remove<'a>(&'a self, name: &'a str) -> Erased<'a, Result<(), String>> {
        Box::pin(async move { self.rm_force(name).await })
    }

    fn logs_tail<'a>(&'a self, name: &'a str, lines: usize) -> Erased<'a, Result<String, String>> {
        Box::pin(async move { self.logs(name, lines).await })
    }

    fn log_excerpt<'a>(&'a self, name: &'a str) -> Erased<'a, Vec<String>> {
        Box::pin(async move { self.excerpt(name).await })
    }

    fn exists<'a>(&'a self, name: &'a str) -> Erased<'a, Presence> {
        Box::pin(async move { self.container_exists(name).await })
    }

    fn list<'a>(
        &'a self,
        container_prefix: &'a str,
        which: Listing,
    ) -> Erased<'a, Result<Vec<Listed>, String>> {
        Box::pin(async move { self.ps(container_prefix, which).await })
    }

    fn inspect<'a>(&'a self, name: &'a str) -> Erased<'a, Result<Found, String>> {
        Box::pin(async move { self.inspect_container(name).await })
    }

    fn same_start(
        &self,
        container_prefix: &str,
        render: &RenderSpec,
        found: &Found,
        labels: &HashMap<String, String>,
    ) -> Result<(), String> {
        self.same_container(container_prefix, render, found, labels)
    }

    fn host_pids<'a>(
        &'a self,
        names: &'a [String],
    ) -> Erased<'a, Result<HashMap<String, u32>, String>> {
        Box::pin(async move { self.init_pids(names).await })
    }
}
