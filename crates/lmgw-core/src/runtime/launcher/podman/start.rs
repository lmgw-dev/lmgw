//! Starting a model's container, and the exit check its readiness poll
//! asks.

use std::time::Duration;

use serde::Deserialize;

use super::{is_no_such_container, Podman};
use crate::runtime::argv::{never_pull, podman_run_argv, stamp_owner};
use crate::runtime::launcher::types::{Launch, LaunchFailure, Launched, Prepared};
use crate::runtime::registry::is_address_in_use;

/// The part of `podman inspect <name>` [`Podman::container_exited`] reads:
/// the container's run state.
#[derive(Debug, Deserialize)]
struct InspectStateRow {
    #[serde(default, rename = "State")]
    state: Option<InspectState>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(default, rename = "Status")]
    status: String,
    #[serde(default, rename = "Running")]
    running: bool,
    #[serde(default, rename = "ExitCode")]
    exit_code: i64,
}

impl Podman {
    /// lmgw's own CDI spec, made true of this machine before a GPU start
    /// (machine-check design §3.6.2), and the option that points Podman at
    /// it — within the start's load timeout: a spec that cannot be written (a
    /// restart pending, a generation that hangs) is the start's answer.
    pub(super) async fn prepare_start(
        &self,
        container_prefix: &str,
        run_args: &[String],
        budget: Duration,
    ) -> Result<Prepared, String> {
        let podman_globals = self
            .cdi()
            .ready_globals(container_prefix, run_args, budget)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Prepared {
            podman_globals,
            native: None,
        })
    }

    /// `podman run -d --replace …` for one attempt (§3.6). Never a pull here
    /// (machine-check design §5): admission downloaded a missing image before
    /// the gate (`vram::download`).
    pub(super) async fn run_container(
        &self,
        launch: Launch<'_>,
    ) -> Result<Launched, LaunchFailure> {
        let mut render = launch.render.clone();
        // A GPU container pointed at lmgw's spec dir gets no restart policy
        // (D27); the start says so.
        let no_restart = render.set_globals(launch.prepared.podman_globals.clone());
        let warnings = if no_restart.is_empty() {
            Vec::new()
        } else {
            vec![crate::runtime::cdi::restart_note(&no_restart)]
        };
        let argv = never_pull(match launch.owner {
            Some(owner) => stamp_owner(podman_run_argv(&render), owner),
            None => podman_run_argv(&render),
        });
        let (status, stderr) = match self.podman_argv(&argv).await {
            // `podman run -d` returned; the model is still uploading.
            Ok(out) if out.ok() => return Ok(Launched { warnings }),
            Ok(out) => (out.status.to_string(), out.stderr),
            // The runner could not even spawn podman (missing binary, fork
            // failure). No container exists, so there is nothing to retry a
            // port against.
            Err(e) => (String::from("n/a"), format!("podman could not be run: {e}")),
        };
        Err(LaunchFailure {
            port_in_use: is_address_in_use(&stderr),
            message: format!("podman run failed (exit {status}): {}", stderr.trim()),
        })
    }

    /// `Some("the container exited with code N")` when podman says a starting
    /// container is no longer running — and so is "no such container", the
    /// one failing inspect with a clear answer. Anything else podman cannot
    /// answer clearly — inspect failing otherwise, JSON it cannot read, a
    /// state it does not name — is `None`: "keep waiting".
    pub(super) async fn container_exited(&self, name: &str) -> Option<String> {
        let out = self
            .podman(&["inspect", "--format", "json", name])
            .await
            .ok()?;
        // A start only asks after its own `podman run` succeeded, so a name
        // podman no longer knows is a container somebody removed while it
        // loaded — dead, not "keep waiting" until the load budget is spent.
        if !out.ok() {
            return is_no_such_container(&out.stderr)
                .then(|| "the container was removed while the model was loading".to_string());
        }
        let rows: Vec<InspectStateRow> = serde_json::from_str(&out.stdout).ok()?;
        let state = rows.into_iter().next()?.state?;
        match state.status.as_str() {
            "exited" | "stopped" | "dead" if !state.running => Some(format!(
                "the container exited with code {} ({}) while the model was loading",
                state.exit_code, state.status
            )),
            _ => None,
        }
    }
}
