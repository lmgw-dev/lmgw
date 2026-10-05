//! Audio rows on the CPU (the per-row CPU switch) as the VRAM side sees
//! them: nothing to charge, nothing to evict, nothing to learn, and no claim
//! for the GPU hold (`runtime::Placement`).
//!
//! The ledger keeps GPU containers only ([`on_gpu`]), which is what takes a
//! CPU container out of the charge, the pending loads, the eviction
//! candidates, the holders a refusal names and the hold's draining list —
//! and out of §4.7's per-process attribution, where a running container
//! with no GPU process would otherwise make every verdict refuse ("a model
//! running without the GPU").

use crate::config::{FallbackRoute, Route, Snapshot};
use crate::error::GatewayError;
use crate::runtime::registry::RuntimeView;
use crate::runtime::Placement;
use crate::state::SharedState;

use super::{Footprint, Target};

/// The registry entry is a container on the GPU — every one but an audio
/// container started on the CPU.
pub(crate) fn on_gpu(e: &RuntimeView) -> bool {
    e.placement == Placement::Gpu
}

/// The net under a row switched to the CPU while its container still ran on
/// the GPU: under the hold, the row is not held, but that container is. It
/// is refused, by name, until the hold's sweep (or an idle stop) takes the
/// container down; the next request then starts it on the CPU. `None` when
/// the target's running container (if any) is on the CPU, or the hold is
/// off.
pub(crate) fn gpu_container_under_hold(
    state: &SharedState,
    snap: &Snapshot,
    target: &Target,
) -> Option<GatewayError> {
    if !snap.settings.hold.active
        || snap.placement(target.class, &target.model_id) != Placement::Cpu
        || state.runtime().placement_of(target.class, &target.model_id) != Some(Placement::Gpu)
    {
        return None;
    }
    Some(GatewayError::GpuHold {
        model: target.model_id.clone(),
        detail: " (its row now runs on the CPU, but its container was started on the GPU \
                 before that and is still up; it is stopped once idle, and the next request \
                 starts it on the CPU)"
            .into(),
    })
}

/// [`gpu_container_under_hold`] at resolve time, for a row with a usable
/// hold fallback: that fallback answers — as it did a moment before the
/// switch, when the row was still a GPU row under the hold — rather than a
/// bare refusal. `(fallback alias, its route)`; `None` when the net does
/// not apply or the row has no usable fallback (admission then refuses,
/// saying why).
pub(crate) fn held_container_fallback(
    state: &SharedState,
    snap: &Snapshot,
    route: &Route,
) -> Option<(String, Route)> {
    let target = super::classify(route)?;
    gpu_container_under_hold(state, snap, &target)?;
    match snap.fallback_route(&target) {
        FallbackRoute::Usable { alias, route } => Some((alias, route)),
        FallbackRoute::None | FallbackRoute::Unusable { .. } => None,
    }
}

/// What a container on the CPU is charged: nothing. The on-disk weights stay
/// in the figure for the surfaces that show them. A backstop — the ledger
/// never asks for a CPU container ([`on_gpu`]), and admission never sizes a
/// CPU start — for whatever sizes one next.
pub(crate) fn cpu_footprint(files: Footprint) -> Footprint {
    Footprint {
        total_bytes: 0,
        kv_cache_bytes: 0,
        note: Some(format!(
            "runs on the CPU: nothing is charged on the GPU ({} on disk, in host RAM once \
             loaded)",
            crate::hf::fmt_bytes(files.weights_bytes)
        )),
        ..files
    }
}
