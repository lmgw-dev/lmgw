//! Stopping and removing containers (§3.6).

use std::time::Duration;

use super::*;
use crate::runtime::{container_name, Class};

impl Registry {
    /// Stop one model's container (§3.6).
    ///
    /// Absent is success — "make sure this is not running" is the caller's
    /// actual intent everywhere this is called from. `in_flight > 0` without
    /// `force` is a typed refusal, not a silent kill: this is the guard the
    /// eviction path and the idle reaper share, and the one `ops::container`
    /// stop never had.
    ///
    /// Whatever is there is stopped, a climb in progress included — this is
    /// the stop an owner, a delete or a shutdown asks for, and it wins against
    /// a climb (ladder design §12 races). A caller that judged one particular
    /// container (the reaper's idle age, eviction's LRU, a dead endpoint)
    /// uses [`Self::stop_generation`] instead.
    pub async fn stop(
        &self,
        class: Class,
        model_id: &str,
        force: bool,
    ) -> Result<(), RuntimeError> {
        self.stop_where(class, model_id, force, None, false).await
    }

    /// [`Self::stop`], only if the entry is still the container `generation`
    /// names and no climb is replacing it (ladder design §12 entry 10).
    ///
    /// For every stop that acts on an earlier judgement of one container: the
    /// idle reaper, eviction and the hold sweep decide from a [`Self::list`]
    /// that can predate a climb's claim, and a dead-container recovery acts on
    /// the port that failed. Without the check each of them stops whatever
    /// runs under the key *now* — which, when the model was restarted or
    /// climbed since, is a newer container nobody judged. On a row without a
    /// ladder that only differs from [`Self::stop`] when a newer container
    /// already replaced the judged one, and that difference was a bug: a stale
    /// hold's recovery force-stopped the container that had replaced its dead
    /// one (§12 entry 20).
    ///
    /// An entry that is gone is success, as for [`Self::stop`]; one that is
    /// another container, or is being climbed, is [`RuntimeError::Moved`]
    /// and nothing is stopped.
    pub async fn stop_generation(
        &self,
        class: Class,
        model_id: &str,
        generation: u64,
        force: bool,
    ) -> Result<(), RuntimeError> {
        self.stop_where(class, model_id, force, Some(generation), false)
            .await
    }

    /// The one stop: the refusal and the identity check are decided in the
    /// same lock hold that marks the entry `stopping`, so nothing can slip
    /// between "this is the container I judged" and "it is stopping".
    /// `only_background`: the judged container must still be
    /// `Background`-owned too ([`Self::stop_idle_background`]).
    pub(super) async fn stop_where(
        &self,
        class: Class,
        model_id: &str,
        force: bool,
        judged: Option<u64>,
        only_background: bool,
    ) -> Result<(), RuntimeError> {
        let key: Key = (class, model_id.to_string());
        let (name, stop_timeout, phase, verdict) = {
            let mut map = self.map();
            let Some(e) = map.get_mut(&key) else {
                return Ok(());
            };
            if let Some(generation) = judged {
                let why = if e.generation != generation {
                    Some("has been restarted since it was judged")
                } else if e.climb.is_some() {
                    Some("is being climbed to another rung")
                } else if only_background && e.owner != Origin::Background {
                    Some(ownership::CLAIMED_BY_OWNER)
                } else {
                    None
                };
                if let Some(why) = why {
                    return Err(RuntimeError::Moved {
                        class,
                        model_id: model_id.to_string(),
                        why,
                    });
                }
            }
            if e.in_flight > 0 && !force {
                return Err(RuntimeError::Busy {
                    class,
                    model_id: model_id.to_string(),
                    in_flight: e.in_flight,
                });
            }
            // What the waiters are told once the entry is gone. A climb is a
            // start too: whoever waits on it wanted the model brought up, and
            // this stop means it will not be.
            let verdict = match (&e.climb, e.state) {
                (Some(m), _) => Phase::Gone(Some(format!(
                    "{class} model '{model_id}' was stopped while it was climbing to rung {}/{}",
                    m.to.index + 1,
                    m.to.of
                ))),
                (None, RuntimeState::Starting) => Phase::Gone(Some(format!(
                    "{class} model '{model_id}' was stopped while it was starting"
                ))),
                (None, _) => Phase::Gone(None),
            };
            e.state = RuntimeState::Stopping;
            (
                e.container_name.clone(),
                e.stop_timeout,
                e.phase.clone(),
                verdict,
            )
        };

        let problem = self.stop_container(&name, stop_timeout).await;

        // Drop the entry either way: after a failed stop lmgw's belief about
        // this container is worthless, and the next start replaces it by name.
        //
        // The verdict goes out *after* the removal, never before: a terminal
        // phase on an entry that is still in the map would spin every waiter
        // (it would re-park on the same entry and re-read the same final
        // value). Waiters that wanted this container's start get told it was
        // stopped, so nobody silently brings back up what somebody just asked
        // to be taken down; the claiming task learns the same fact its own
        // way, from `ready()` finding the entry no longer `starting`.
        self.forget(&key, &phase, false);
        phase.send_replace(verdict);

        match problem {
            None => Ok(()),
            Some(message) => Err(RuntimeError::Stop {
                class,
                model_id: model_id.to_string(),
                message,
            }),
        }
    }

    /// `podman stop` then `podman wait` on one container name; `Some` is what
    /// went wrong. Shared by [`Self::stop`] and a climb's stop of the rung it
    /// replaces, which logs a failure and starts the new rung anyway
    /// (`--replace` collects the old container by name).
    pub(super) async fn stop_container(
        &self,
        name: &str,
        stop_timeout: Duration,
    ) -> Option<String> {
        let stop = self
            .podman(&["stop", "-t", &STOP_GRACE_SECONDS.to_string(), name])
            .await;
        let problem = match stop {
            Ok(out) if out.ok() => None,
            // Already gone is the requested state, not a failure.
            Ok(out) if is_no_such_container(&out.stderr) => None,
            Ok(out) => Some(format!("podman stop: {}", out.stderr.trim())),
            Err(e) => Some(format!("podman stop could not be run: {e}")),
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
                "container '{name}' was still running {stop_timeout:?} after podman stop \
                 (vram.unload_timeout_seconds)"
            )),
        }
    }

    /// Stop every managed container (§3.4's graceful shutdown, and the ops
    /// "stop all"). Concurrent on purpose — a tray app quitting should not
    /// pay N × grace serially — and returns every refusal/failure rather than
    /// the first, so the caller can report exactly which models were busy.
    pub async fn stop_all(&self, force: bool) -> Vec<RuntimeError> {
        let keys: Vec<Key> = self.map().keys().cloned().collect();
        futures::future::join_all(
            keys.iter()
                .map(|(class, model_id)| self.stop(*class, model_id, force)),
        )
        .await
        .into_iter()
        .filter_map(Result::err)
        .collect()
    }

    /// `podman stop -t <grace> <name>`, best effort and never fatal: the
    /// caller is already returning a failure and this is hygiene on the way
    /// out. "No such container" is the requested state, not a problem.
    pub(super) async fn stop_quietly(&self, name: &str) {
        match self
            .podman(&["stop", "-t", &STOP_GRACE_SECONDS.to_string(), name])
            .await
        {
            Ok(out) if out.ok() || is_no_such_container(&out.stderr) => {}
            Ok(out) => tracing::warn!(container = %name, "podman stop: {}", out.stderr.trim()),
            Err(e) => tracing::warn!(container = %name, "podman stop could not be run: {e}"),
        }
    }

    /// `podman rm -f <name>`, treating "already gone" as done. Best-effort by
    /// contract: every caller has already decided the container must not be
    /// there, and a failure to remove it is reported, never retried into a
    /// loop.
    pub async fn rm_force(&self, name: &str) -> Result<(), String> {
        match self.podman(&["rm", "-f", name]).await {
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

    /// Stop one model's container and make sure the container object is gone
    /// too (§3.4's "deleting or disabling a model stops and removes its
    /// container immediately").
    ///
    /// [`Self::stop`] alone leaves the stopped container in place — deliberate,
    /// so a failed model keeps its logs, and `--replace` collects it on the
    /// next start. That is exactly wrong for a model that will never start
    /// again: nothing would ever collect it. So the delete/disable paths call
    /// this, which follows the stop with an `rm -f` **by rendered name**, and
    /// therefore also collects a container this lmgw never had an entry for
    /// (one orphaned by a crash, deleted before the next boot reconcile).
    pub async fn stop_and_remove(
        &self,
        container_prefix: &str,
        class: Class,
        model_id: &str,
        force: bool,
    ) -> Result<(), RuntimeError> {
        let stopped = self.stop(class, model_id, force).await;
        let name = container_name(container_prefix, class, model_id);
        if let Err(e) = self.rm_force(&name).await {
            tracing::warn!(container = %name, "{e}");
        }
        stopped
    }
}
