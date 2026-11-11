//! `podman logs` for a model's container.

use super::Podman;
use crate::runtime::launcher::{log_excerpt_of, LOG_TAIL};

impl Podman {
    /// `podman logs --tail <n>` for one container, verbatim (§3.6/§8's
    /// `logs` verb — the `lmgw__container action=logs` surface).
    pub(super) async fn logs(&self, container: &str, tail: usize) -> Result<String, String> {
        let out = self
            .podman(&["logs", "--tail", &tail.to_string(), container])
            .await
            .map_err(|e| format!("podman logs could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman logs failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(format!("{}{}", out.stdout, out.stderr))
    }

    /// Best-effort `podman logs --tail` for a failed start, read down to
    /// [`log_excerpt_of`]'s lines. Whatever podman printed counts, its own
    /// complaint on a failed read included.
    pub(super) async fn excerpt(&self, container: &str) -> Vec<String> {
        let Ok(out) = self
            .podman(&["logs", "--tail", &LOG_TAIL.to_string(), container])
            .await
        else {
            return Vec::new();
        };
        log_excerpt_of(&format!("{}\n{}", out.stdout, out.stderr))
    }
}
