//! The GPU lease (benchmark design §3.2): while a benchmark runs, the card is
//! the benchmark's, and no local model of any class is admitted.
//!
//! **Where it lives: on the [`Snapshot`].** The lease is runtime state, not a
//! setting — nothing persists it, and a restart ends it (the run is
//! `interrupted`, §3.3) — but it is carried on the snapshot, in
//! [`Snapshot::gpu_lease`], because every site that has to honour it already
//! holds a snapshot and already asks it one question: "is the GPU held?"
//! (`snap.settings.hold.active`). [`Snapshot::gpu_block`] is that question
//! with both answers, so each site changes one expression and nothing about
//! how it reaches its state:
//!
//! * the request-level reroute, [`Snapshot::resolve_for_request`] — a pure
//!   method of the snapshot, which a state-level flag could not reach without
//!   changing its signature and its four callers;
//! * the gate's candidate-alias pick and walk (`gate::candidate::walk`);
//! * the admission net: `vram/queue.rs` (`admit_local`, and the warm/operator
//!   pre-flight `check_background_start`), `vram/climb.rs`,
//!   `vram/background.rs` (`join_at`);
//! * the refusals that name the hold themselves: `local_model_test` (all
//!   classes), the runtime probe of `local_model_plan`, quickdoc's batch
//!   runners and its pinned embedder, and a build run's GPU verify.
//!
//! **And the registry, as the net under all of them** (§13 decision 44).
//! A site asks before its awaits and starts after them, so a start decided
//! just before the lease could reach `podman run` after the run's drain had
//! looked. The registry keeps a copy of the lease and refuses to create an
//! entry under it, deciding under the map lock that inserts entries
//! (`runtime/registry/lease.rs`); the sites that hold the admission gate ask
//! again after their last await, and the drain takes the gate once before it
//! looks.
//!
//! A snapshot is immutable, so the lease is set and cleared by publishing a
//! new one ([`crate::state::AppState::set_gpu_lease`]), and every publish
//! from the store carries the current lease over
//! ([`crate::state::AppState`]'s `publish_snapshot`). Both go through
//! `ArcSwap::rcu`, so a config reload racing a lease change can neither drop
//! the lease nor resurrect one that was just released.
//!
//! **The hold wins.** Under both, a site answers as the hold does: the hold
//! is the owner's own switch and ends the run anyway (§3.3).
//!
//! **Not the outside-VRAM trigger.** Unlike the hold, the lease leaves
//! §4.7's measurement on: the run's container is attributed as lmgw's
//! (`vram/ledger.rs`, from [`crate::bench::state::OnCard`]), so Overview shows
//! it as lmgw's share and outside use stays measured.

use std::sync::Arc;

use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::gate::FallbackReason;

/// "A benchmark holds the card": the run that took it, and its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuLease {
    pub run_id: i64,
    pub model_id: String,
}

/// Why no local model may be admitted right now. Owns its lease (an `Arc`),
/// so a site can keep it past the snapshot it read it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuBlock {
    /// The owner's GPU hold (gpu-hold design).
    Hold,
    /// A benchmark run's lease (benchmark design §3.2).
    Benchmark(Arc<GpuLease>),
}

impl Snapshot {
    /// The one predicate every admission site asks (module doc): `Some` when
    /// a local model may not be started or given new work right now.
    pub fn gpu_block(&self) -> Option<GpuBlock> {
        if self.settings.hold.active {
            return Some(GpuBlock::Hold);
        }
        self.gpu_lease.clone().map(GpuBlock::Benchmark)
    }
}

impl GpuBlock {
    /// The refusal a request for local model `model` gets: `gpu_hold` or
    /// `gpu_benchmark`, both 503. `detail` is `""` or names why a configured
    /// fallback did not help, e.g. `" (fallback 'x' is itself a local
    /// model)"`.
    pub fn refusal(&self, model: impl Into<String>, detail: impl Into<String>) -> GatewayError {
        let (model, detail) = (model.into(), detail.into());
        match self {
            Self::Hold => GatewayError::GpuHold { model, detail },
            Self::Benchmark(l) => GatewayError::GpuBenchmark {
                model,
                run_id: l.run_id,
                detail,
            },
        }
    }

    /// `x-lmgw-fallback-reason` (and `request_logs.fallback_reason`) when the
    /// fallback answers instead.
    pub fn fallback_reason(&self) -> FallbackReason {
        match self {
            Self::Hold => FallbackReason::Hold,
            Self::Benchmark(_) => FallbackReason::Benchmark,
        }
    }

    /// What holds the card, as the subject of a sentence ("… is not started
    /// while {who}").
    pub fn who(&self) -> String {
        match self {
            Self::Hold => "lmgw is holding the GPU".to_string(),
            Self::Benchmark(l) => format!(
                "benchmark run {} ('{}') has the GPU to itself",
                l.run_id, l.model_id
            ),
        }
    }

    /// How it ends, for the owner reading the refusal.
    pub fn how_it_ends(&self) -> &'static str {
        match self {
            Self::Hold => {
                "release the hold (tray, dashboard titlebar, or lmgw__hold_set active=false)"
            }
            Self::Benchmark(_) => {
                "wait for the run to finish, or cancel it (Benchmarks page, or lmgw__bench_cancel)"
            }
        }
    }
}

/// [`Snapshot::gpu_lease`] for a new lease.
pub fn lease(run_id: i64, model_id: &str) -> Arc<GpuLease> {
    Arc::new(GpuLease {
        run_id,
        model_id: model_id.to_string(),
    })
}

/// A run's hold on the lease: taken by [`LeaseGuard::take`], given back by
/// [`LeaseGuard::release`] — and by `Drop` on any way out that skipped it (a
/// panic in the run), because a lease nobody releases refuses every local
/// request until the next restart.
pub struct LeaseGuard {
    state: crate::state::SharedState,
    run_id: i64,
}

impl LeaseGuard {
    pub fn take(state: &crate::state::SharedState, run_id: i64, model_id: &str) -> Self {
        state.set_gpu_lease(Some(lease(run_id, model_id)));
        Self {
            state: state.clone(),
            run_id,
        }
    }

    /// Give the card back — only if the lease is still this run's.
    pub fn release(&self) {
        let ours = self
            .state
            .snapshot()
            .gpu_lease
            .as_ref()
            .is_some_and(|l| l.run_id == self.run_id);
        if ours {
            self.state.set_gpu_lease(None);
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.release();
        self.state.bench.clear(self.run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hold_wins_over_a_lease() {
        let mut snap = Snapshot::default();
        assert_eq!(snap.gpu_block(), None);
        snap.gpu_lease = Some(lease(7, "qwen"));
        let b = snap.gpu_block().unwrap();
        assert!(matches!(&b, GpuBlock::Benchmark(l) if l.run_id == 7));
        let e = b.refusal("other", "");
        assert_eq!(e.code(), "gpu_benchmark");
        assert_eq!(e.http_status().as_u16(), 503);
        assert!(e.to_string().contains("run 7"), "{e}");
        assert_eq!(b.fallback_reason().as_str(), "benchmark");

        snap.settings.hold.active = true;
        let b = snap.gpu_block().unwrap();
        assert_eq!(b, GpuBlock::Hold);
        assert_eq!(b.refusal("other", "").code(), "gpu_hold");
    }
}
