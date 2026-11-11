//! Stopping and removing a model's container (§3.6).

use std::time::Duration;

use super::{is_no_such_container, Podman};
use crate::runtime::registry::STOP_GRACE_SECONDS;

impl Podman {
    /// `podman stop` then `podman wait` on one container name; `Some` is what
    /// went wrong.
    pub(super) async fn stop_and_wait(&self, name: &str, stop_timeout: Duration) -> Option<String> {
        // Bounded too (2026-10-24): a podman that hangs on its own stop —
        // a stuck storage lock — must not hold the stop's caller for good,
        // the GPU hold's sweep under the settings lock among them. The grace
        // `-t` gives, then `vram.unload_timeout_seconds`, as
        // `remove_bounded`; the runner kills the child on drop.
        let bound = Duration::from_secs(STOP_GRACE_SECONDS).saturating_add(stop_timeout);
        let stop = tokio::time::timeout(
            bound,
            self.podman(&["stop", "-t", &STOP_GRACE_SECONDS.to_string(), name]),
        )
        .await;
        let problem = match stop {
            Err(_) => {
                return Some(format!(
                    "podman stop {name} did not return within {bound:?} (the stop grace plus the \
                     unload timeout, {})",
                    crate::config::UNLOAD_TIMEOUT_LABEL
                ))
            }
            Ok(Ok(out)) if out.ok() => None,
            // Already gone is the requested state, not a failure.
            Ok(Ok(out)) if is_no_such_container(&out.stderr) => None,
            Ok(Ok(out)) => Some(format!("podman stop: {}", out.stderr.trim())),
            Ok(Err(e)) => Some(format!("podman stop could not be run: {e}")),
        };
        // `podman stop` returns when the signal has been delivered and the
        // grace has elapsed; `wait` is what says the process is actually
        // gone and its VRAM with it — the eviction path (§4) forwards on
        // that fact. Bounded by the entry's `vram.unload_timeout_seconds`;
        // the runner kills the child on drop, so a timeout leaks nothing.
        let waited = tokio::time::timeout(stop_timeout, self.podman(&["wait", name])).await;
        if problem.is_some() {
            return problem;
        }
        match waited {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(format!("podman wait could not be run: {e}")),
            Err(_) => Some(format!(
                "container '{name}' was still running {stop_timeout:?} after podman stop (the \
                 unload timeout, {})",
                crate::config::UNLOAD_TIMEOUT_LABEL
            )),
        }
    }

    /// `podman stop -t <grace> <name>`, best effort and never fatal: the
    /// caller is already returning a failure and this is hygiene on the way
    /// out. "No such container" is the requested state, not a problem.
    pub(super) async fn stop_best_effort(&self, name: &str) {
        match self
            .podman(&["stop", "-t", &STOP_GRACE_SECONDS.to_string(), name])
            .await
        {
            Ok(out) if out.ok() || is_no_such_container(&out.stderr) => {}
            Ok(out) => tracing::warn!(container = %name, "podman stop: {}", out.stderr.trim()),
            Err(e) => tracing::warn!(container = %name, "podman stop could not be run: {e}"),
        }
    }

    /// `podman rm -f <name>`, treating "already gone" as done.
    pub(super) async fn rm_force(&self, name: &str) -> Result<(), String> {
        rm_force_outcome(name, self.podman(&["rm", "-f", name]).await)
    }
}

/// How `podman rm -f <name>` went, as a sentence when it did not: shared
/// with the registry's own `rm -f` for the containers outside the launcher.
pub(crate) fn rm_force_outcome(
    name: &str,
    out: std::io::Result<crate::runtime::registry::CmdOutput>,
) -> Result<(), String> {
    match out {
        Ok(out) if out.ok() => Ok(()),
        Ok(out) if is_no_such_container(&out.stderr) => Ok(()),
        Ok(out) => Err(format!(
            "podman rm -f {name} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => Err(format!("podman rm -f {name} could not be run: {e}")),
    }
}
