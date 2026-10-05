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
//! **Per model** ([`Snapshot::gpu_block_for`]): an audio row on the CPU
//! (`runtime::Placement::Cpu`) is blocked by the lease and never by the
//! hold. Every site that decides for one model asks that instead.
//!
//! **Not the outside-VRAM trigger.** Unlike the hold, the lease leaves
//! §4.7's measurement on: the run's container is attributed as lmgw's
//! (`vram/ledger.rs`, from [`crate::bench::state::OnCard`]), so Overview shows
//! it as lmgw's share and outside use stays measured.

use std::sync::Arc;

use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::runtime::{Class, Placement};

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

    /// [`Self::gpu_block`] for one model, by where it runs
    /// ([`Self::placement`]): a model on the CPU uses no VRAM, so the hold
    /// has no claim on it — the owner reads the hold as "lmgw may use zero
    /// VRAM". A benchmark's lease still covers it: the run measures the
    /// whole machine, and its drain needs no exception.
    pub fn gpu_block_for(&self, class: Class, model_id: &str) -> Option<GpuBlock> {
        self.gpu_block_at(self.placement(class, model_id))
    }

    /// [`Self::gpu_block`] for a start on `placement` — the placement of the
    /// descriptor that will actually be started. A check after an await asks
    /// this rather than [`Self::gpu_block_for`]: the row may have been
    /// switched while the start waited, and its container is still the one
    /// rendered before the wait.
    pub fn gpu_block_at(&self, placement: Placement) -> Option<GpuBlock> {
        match placement {
            Placement::Gpu => self.gpu_block(),
            Placement::Cpu => self.gpu_lease.clone().map(GpuBlock::Benchmark),
        }
    }

    /// Where `model_id` of `class` computes as its row is configured now —
    /// what its next start runs on. An unknown model reads as `Gpu`, the
    /// stricter answer. A running container's own placement is its
    /// registry entry's ([`crate::runtime::registry::Registry::placement_of`]).
    pub fn placement(&self, class: Class, model_id: &str) -> Placement {
        if class != Class::Audio {
            return Placement::Gpu;
        }
        self.audio_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map_or(Placement::Gpu, |m| {
                crate::runtime::audio::placement(m, &self.settings.audio)
            })
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

    /// A model on the CPU is blocked by a benchmark's lease, never by the
    /// hold; every other model by both, the hold winning.
    #[test]
    fn the_hold_has_no_claim_on_a_model_on_the_cpu() {
        let row = |id: &str, backend: Option<&str>| {
            serde_json::from_value::<crate::config::AudioModel>(serde_json::json!({
                "id": 1, "model_id": id, "family": "parakeet_tdt", "path": "p",
                "task": "asr", "mode": "offline", "load_options": {}, "session_options": {},
                "voice_presets": {}, "default_voice_preset": null, "enabled": true,
                "image": null, "extra_run_args": null, "warm_start": false,
                "backend": backend
            }))
            .unwrap()
        };
        let mut snap = Snapshot {
            audio_models: vec![row("cpu-asr", Some("cpu")), row("gpu-tts", None)],
            ..Snapshot::default()
        };
        let block = |s: &Snapshot, class, id| s.gpu_block_for(class, id);
        assert_eq!(snap.placement(Class::Audio, "cpu-asr"), Placement::Cpu);
        assert_eq!(snap.placement(Class::Audio, "gpu-tts"), Placement::Gpu);
        assert_eq!(snap.placement(Class::Audio, "unknown"), Placement::Gpu);
        assert_eq!(snap.placement(Class::Chat, "cpu-asr"), Placement::Gpu);
        assert_eq!(block(&snap, Class::Audio, "cpu-asr"), None);

        snap.settings.hold.active = true;
        assert_eq!(block(&snap, Class::Audio, "cpu-asr"), None, "hold only");
        assert_eq!(block(&snap, Class::Audio, "gpu-tts"), Some(GpuBlock::Hold));
        assert_eq!(block(&snap, Class::Chat, "x"), Some(GpuBlock::Hold));

        snap.gpu_lease = Some(lease(3, "qwen"));
        assert!(matches!(
            block(&snap, Class::Audio, "cpu-asr"),
            Some(GpuBlock::Benchmark(l)) if l.run_id == 3
        ));
        assert_eq!(
            block(&snap, Class::Audio, "gpu-tts"),
            Some(GpuBlock::Hold),
            "the hold wins on the GPU"
        );
        snap.settings.hold.active = false;
        assert!(matches!(
            block(&snap, Class::Audio, "cpu-asr"),
            Some(GpuBlock::Benchmark(_))
        ));
        // A class whose backend is the CPU puts every row that inherits it
        // there.
        snap.gpu_lease = None;
        snap.settings.hold.active = true;
        snap.settings.audio.backend = "cpu".into();
        assert_eq!(block(&snap, Class::Audio, "gpu-tts"), None);
    }
}
