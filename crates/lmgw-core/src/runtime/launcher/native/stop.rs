//! Stopping and removing a native child.

use std::time::Duration;

use super::Native;
use crate::runtime::registry::STOP_GRACE_SECONDS;

impl Native {
    /// Ask `name` to stop and wait until it has exited, within the stop
    /// grace plus `unload_timeout` (the supervisor kills it after the grace).
    /// Its log ring stays.
    pub(super) async fn stop_and_wait(
        &self,
        name: &str,
        unload_timeout: Duration,
    ) -> Option<String> {
        let p = self.child(name)?;
        self.note_stop(name, &p);
        p.ask_to_stop();
        let bound = Duration::from_secs(STOP_GRACE_SECONDS).saturating_add(unload_timeout);
        if p.wait_ended(bound).await {
            return None;
        }
        Some(format!(
            "the engine process {name} (pid {}) was still running {bound:?} after it was asked to \
             stop (the stop grace plus the unload timeout, {})",
            p.pid,
            crate::config::UNLOAD_TIMEOUT_LABEL
        ))
    }

    /// Kill `name` and forget it, log ring included, once it has exited —
    /// within the stop grace. One that has not exited by then stays in the
    /// table, so its PID still counts, and the answer says so.
    pub(super) async fn kill_and_forget(&self, name: &str) -> Result<(), String> {
        let Some(p) = self.child(name) else {
            return Ok(());
        };
        self.note_stop(name, &p);
        p.kill();
        let bound = Duration::from_secs(STOP_GRACE_SECONDS);
        if !p.wait_ended(bound).await {
            return Err(format!(
                "the engine process {name} (pid {}) was still running {bound:?} after it was \
                 killed",
                p.pid
            ));
        }
        let mut table = self.table();
        if table
            .get(name)
            .is_some_and(|now| std::sync::Arc::ptr_eq(now, &p))
        {
            table.remove(name);
        }
        Ok(())
    }
}
