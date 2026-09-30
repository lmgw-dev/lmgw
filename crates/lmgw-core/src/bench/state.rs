//! What the gateway keeps about benchmark runs between calls
//! ([`crate::state::AppState::bench`]): the run in flight — so the GPU
//! hold can end it (§3.3) — and the two seams a test replaces, the launcher
//! and the suite's timing.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use lmgw_api_types::bench::SuiteParams;

use super::launcher::{BenchLauncher, PodmanLauncher};
use crate::state::AppState;

/// The idle window a run measures its baseline over (§3.2 step 4).
pub const BASELINE_WINDOW: Duration = Duration::from_secs(2);

/// Where the bench container is, as VRAM attribution sees it (§3.4): its
/// processes are lmgw's share of the card, like a model container's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnCard {
    /// Not started yet, or removed.
    #[default]
    Off,
    /// `podman run` is about to run or running, and the server is not
    /// healthy yet: its memory is still growing, so lmgw's share is not
    /// measured — the rule a registry entry that is `starting` follows.
    Loading,
    /// Healthy: attributed like a model container, under a generation of its
    /// own ([`crate::runtime::registry::outside_generation`]).
    Ready { generation: u64 },
}

/// The run in flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentRun {
    pub run_id: i64,
    pub job_id: i64,
    pub model_id: String,
    /// The bench container's name, once the run is about to start it.
    pub container: Option<String>,
    /// Whether that container is on the card, for VRAM attribution.
    pub on_card: OnCard,
    /// Set by whatever ended the run from outside, other than the owner's
    /// cancel: `"hold"` (§3.3) or `"shutdown"`. The run's status is then
    /// `aborted`.
    pub abort: Option<&'static str>,
    /// The run has stopped measuring and is removing its container: nothing
    /// from outside can end it any more ([`BenchState::end`]).
    pub ending: bool,
}

/// A bench container whose removal failed at its run's end, however often
/// it was tried (§13 decision 48): it may still hold GPU memory. The next
/// run tries to remove it again before it measures, `bench_plan` warns about
/// it, and the next boot's sweep collects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stranded {
    pub run_id: i64,
    pub name: String,
    /// The last removal's error.
    pub error: String,
}

/// The pause before the second attempt to remove a bench container; each
/// later one waits twice as long (§13 decision 48).
pub const REMOVE_BACKOFF: Duration = Duration::from_secs(1);

/// The suite's timing: suite v1's in production, shrunk by a test so a run
/// takes seconds (§13 decision 20: "only suite v1's values are used outside
/// tests").
#[derive(Clone)]
pub struct Tuning {
    pub baseline_window: Duration,
    /// `None`: the sampler's own 100 ms.
    pub sample_interval: Option<Duration>,
    /// Applied to a run's [`SuiteParams`] before it is stored.
    pub params: Option<fn(&mut SuiteParams)>,
    /// [`REMOVE_BACKOFF`].
    pub remove_backoff: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            baseline_window: BASELINE_WINDOW,
            sample_interval: None,
            params: None,
            remove_backoff: REMOVE_BACKOFF,
        }
    }
}

#[derive(Default)]
pub struct BenchState {
    launcher: RwLock<Option<Arc<dyn BenchLauncher>>>,
    current: Mutex<Option<CurrentRun>>,
    tuning: RwLock<Tuning>,
    stranded: Mutex<Vec<Stranded>>,
}

impl BenchState {
    /// How runs reach podman: a test's launcher when one is installed, else
    /// [`PodmanLauncher`] over the registry's runner — the real `podman` in
    /// production, and in a test gateway the runner that refuses every verb.
    pub fn launcher(&self, state: &AppState) -> Arc<dyn BenchLauncher> {
        if let Some(l) = self.launcher.read().unwrap().clone() {
            return l;
        }
        Arc::new(PodmanLauncher::new(state.runtime().runner()))
    }

    /// Test-only: the launcher runs use from now on.
    #[doc(hidden)]
    pub fn set_launcher_for_tests(&self, launcher: Arc<dyn BenchLauncher>) {
        *self.launcher.write().unwrap() = Some(launcher);
    }

    pub fn tuning(&self) -> Tuning {
        self.tuning.read().unwrap().clone()
    }

    /// Test-only: shrink the suite's timing.
    #[doc(hidden)]
    pub fn set_tuning_for_tests(&self, tuning: Tuning) {
        *self.tuning.write().unwrap() = tuning;
    }

    /// Bench containers whose removal failed, oldest first.
    pub fn stranded(&self) -> Vec<Stranded> {
        self.stranded.lock().unwrap().clone()
    }

    pub(super) fn strand(&self, s: Stranded) {
        let mut all = self.stranded.lock().unwrap();
        all.retain(|o| o.name != s.name);
        all.push(s);
    }

    pub(super) fn unstrand(&self, name: &str) {
        self.stranded.lock().unwrap().retain(|s| s.name != name);
    }

    /// The run in flight, if any.
    pub fn current(&self) -> Option<CurrentRun> {
        self.current.lock().unwrap().clone()
    }

    pub(super) fn begin(&self, run: CurrentRun) {
        *self.current.lock().unwrap() = Some(run);
    }

    /// Clear the run in flight, if it is still `run_id`'s.
    pub(super) fn clear(&self, run_id: i64) {
        let mut cur = self.current.lock().unwrap();
        if cur.as_ref().is_some_and(|c| c.run_id == run_id) {
            *cur = None;
        }
    }

    pub(super) fn set_container(&self, run_id: i64, name: &str) {
        if let Some(c) = self.current.lock().unwrap().as_mut() {
            if c.run_id == run_id {
                c.container = Some(name.to_string());
            }
        }
    }

    /// Move the run's container along [`OnCard`], if the run in flight is
    /// still `run_id`.
    pub(super) fn set_on_card(&self, run_id: i64, on_card: OnCard) {
        if let Some(c) = self.current.lock().unwrap().as_mut() {
            if c.run_id == run_id {
                c.on_card = on_card;
            }
        }
    }

    /// Mark the run in flight as ended by `reason`, and return it — `None`
    /// when nothing runs, or the run is already ending: it measured all it
    /// was going to, and its own end removes the container. An abort then
    /// would relabel a finished run and report one that never happened.
    pub(super) fn abort(&self, reason: &'static str) -> Option<CurrentRun> {
        let mut cur = self.current.lock().unwrap();
        let c = cur.as_mut().filter(|c| !c.ending)?;
        c.abort.get_or_insert(reason);
        Some(c.clone())
    }

    /// The run `run_id` has stopped measuring: from now on [`Self::abort`]
    /// finds nothing to end. Returns what aborted it before that, if
    /// anything did.
    pub(super) fn end(&self, run_id: i64) -> Option<&'static str> {
        let mut cur = self.current.lock().unwrap();
        let c = cur.as_mut().filter(|c| c.run_id == run_id)?;
        c.ending = true;
        c.abort
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hold switched on once the run has stopped measuring finds nothing
    /// to abort (review finding 3): the run is not relabelled, and `hold_set`
    /// does not report an abort that never happened.
    #[test]
    fn an_ending_run_cannot_be_aborted() {
        let b = BenchState::default();
        b.begin(CurrentRun {
            run_id: 3,
            ..Default::default()
        });
        assert_eq!(b.end(3), None);
        assert_eq!(b.abort("hold"), None);
        assert_eq!(b.current().unwrap().abort, None);

        // Aborted before it ended: the reason is what `end` hands back.
        b.begin(CurrentRun {
            run_id: 4,
            ..Default::default()
        });
        assert_eq!(b.abort("hold").map(|c| c.run_id), Some(4));
        assert_eq!(b.end(4), Some("hold"));
        assert_eq!(b.end(5), None, "another run's end");
    }
}
