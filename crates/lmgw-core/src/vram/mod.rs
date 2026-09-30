//! VRAM admission control — the arbitration layer around `acquire`
//! (per-model-containers design §4, successor to quickdoc §9b).
//!
//! The failure this exists for: a fully loaded chat model plus an ingressing
//! embed request is an OOM crash. Nothing else on the box decides what may
//! occupy the GPU — lmgw starts every local container itself, so it is the
//! only component that can answer, before a request is forwarded, whether the
//! model it needs will fit.
//!
//! **Two planes, and they are not the same plane.** [`crate::runtime::registry`]
//! *acts*: one container per model, `acquire` starts it (or joins a start
//! already in flight) and hands back an endpoint plus an in-flight claim. This
//! module *decides*: whether that start may happen now, and what has to be
//! evicted first. Design §3.2 is explicit that the two are independent —
//! `acquire` is what makes a local model reachable at all, so when admission
//! is inactive (vram disabled, no telemetry and no declared budget) every
//! local route still acquires, unarbitrated, and the gateway runs exactly as
//! it would without this module.
//!
//! **The ledger.** Per GPU, what lmgw believes is committed: every registry
//! entry that is `starting` or `ready` (it holds VRAM, or is in the middle of
//! taking it), plus the reservations of decisions whose containers have not
//! entered the registry yet. Sizes come from GGUF metadata (see [`plan`]); the
//! measured counterpart comes from the driver — NVML on NVIDIA (see
//! [`nvml`]), amdgpu's sysfs counters on AMD (see [`amdgpu`]). Both are reported side
//! by side and neither is presented as the other — the estimate is a
//! documented lower bound, and where they disagree the *measurement* is what
//! admission decides on.
//!
//! **The gate holds the decision, not the load** (§4, and the §10.6
//! measurement that motivated it). One decision at a time — so two requests
//! cannot both be told the same free bytes — covering measure → fit → *claim*
//! → eviction picks. The claim is a reservation of the model's estimated bytes
//! against the ledger; it is what lets the gate be released **before** the
//! container start, so starts of different models overlap end to end. The
//! reservation is dropped when the start finishes, succeed or fail, by which
//! point the registry (and the driver) account for the model directly.
//!
//! **Eviction.** Candidates are registry entries with no request in flight —
//! never filtered on a nonzero estimate, because a resident whose footprint
//! reads zero would otherwise be unevictable forever — least recently used
//! first, by the registry's own `last_used`. lmgw is not the sole ingress
//! (the dashboard publishes container ports for direct use), so the victim's
//! own `GET /slots` is asked before it is stopped: any slot with
//! `is_processing` and the victim is skipped (§10.7 — direct-mode llama-server
//! serves `/slots` by default). Then `Registry::stop`, which refuses on its
//! own in-flight count, so an in-flight request that raced in still wins.
//!
//! **Degradation.** Where neither driver answers — no NVML, no amdgpu, an
//! Intel or Apple host, a CI runner — there is no measurement, so there is
//! nothing to admit against. Admission then stays *inactive* — requests acquire and
//! forward exactly as before — unless the owner declares a `budget_mb`, in
//! which case estimates are planned against that. Either way the state is
//! visible in the status surface and nothing panics: a GPU-less host (CI, this
//! crate's tests) runs the whole gateway unchanged.

pub mod amdgpu;
pub mod attribution;
mod background;
mod climb;
pub mod nvml;
pub mod peak;
pub mod plan;

pub use background::{join, start_background, BackgroundStart, Restart};
pub use climb::{climb, climb_for, Climbed};

pub use amdgpu::AmdSysfsProbe;
pub use attribution::{ProcFs, ProcTree};
pub use nvml::{GpuMemory, GpuPower, GpuProbe, NvmlProbe, ProcessMemory};
pub use plan::Footprint;

// `Fill` has no other home at the vram top level: it is attribution's own
// type, but the climb (untouched) and background (untouched) siblings reach
// it as `super::Fill`, which needs a binding here rather than only at
// `vram::attribution::Fill`.
use attribution::Fill;

mod routing_target;
pub use routing_target::*;
mod view;
pub use view::*;
mod scheduler;
pub use scheduler::*;
mod ledger;
pub(crate) use ledger::*;
mod local_hold;
pub use local_hold::*;
mod admission;
pub use admission::*;
mod queue;
mod verdict;
pub(crate) use queue::*;

#[cfg(test)]
mod tests;
