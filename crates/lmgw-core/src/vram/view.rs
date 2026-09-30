//! Status view

use serde::Serialize;

use crate::hf::fmt_bytes;
use crate::runtime::Class;
use crate::state::SharedState;

use super::attribution::Fill;
use super::nvml::GpuMemory;
use super::scheduler::MIB;
use super::{trigger_off, VramScheduler};

/// One model lmgw believes is holding GPU memory.
#[derive(Debug, Clone, Serialize)]
pub struct ResidentView {
    /// The class string (`chat` | `aux` | `audio` | `image`). Kept under this name
    /// because it is a published field of `GET /api/vram`, the `vram` SSE
    /// frame and `lmgw__status`; with one container per model the class *is*
    /// what a "container" column can still meaningfully say.
    pub container: Class,
    pub model: String,
    /// `starting` | `ready` from the registry, or `reserved` for a start this
    /// scheduler has admitted and whose registry entry does not exist yet.
    pub state: String,
    pub estimated_bytes: u64,
    /// Requests lmgw currently has in flight against this model.
    pub in_flight: usize,
    /// Seconds since the registry's `last_used` — the LRU key eviction uses.
    /// `None` for a reservation, which has no entry to stamp yet.
    pub idle_seconds: Option<u64>,
    /// What one generation of this image pipeline has been measured to need
    /// *above* its idle residency, and what admission keeps free for it while
    /// it is resident (image-generation §9, [`peak`](crate::vram::peak)).
    ///
    /// Only ever set for a `ready` image model, and `None` until one
    /// generation has taught it — which [`Self::note`] says in words, because
    /// an unlearned peak is a real hole in what admission can promise and a
    /// silent `null` is not a warning.
    pub peak_extra_bytes: Option<u64>,
    /// Why the estimate is partial, when it is.
    pub note: Option<String>,
}

/// One request waiting for room.
#[derive(Debug, Clone, Serialize)]
pub struct WaiterView {
    pub position: usize,
    pub alias: String,
    pub model: String,
    pub container: Class,
    pub needs_bytes: u64,
    pub waiting_ms: u64,
    /// What it is waiting on right now, in words.
    pub stage: String,
}

/// The whole plane in one object: what is measured, what is believed, what is
/// waiting. Backs `GET /api/vram`, the `vram` SSE frame, `ops::status` and
/// therefore `lmgw__status`.
#[derive(Debug, Clone, Serialize)]
pub struct VramView {
    /// The `vram.enabled` setting.
    pub enabled: bool,
    /// Whether admission is actually arbitrating. False when disabled, or when
    /// there is nothing to measure against — see `inactive_reason`.
    pub active: bool,
    pub inactive_reason: Option<String>,
    /// Where the measured numbers come from, verbatim.
    pub telemetry: String,
    pub telemetry_ok: bool,
    pub headroom_bytes: u64,
    /// Memory admission plans against: the devices' real total, or the declared
    /// `budget_mb` when one is set.
    pub capacity_bytes: u64,
    /// Free memory as measured, or derived from the estimate when there is no
    /// measurement. `None` when neither exists.
    pub free_bytes: Option<u64>,
    /// Whether `free_bytes` came from the driver. False means it was derived by
    /// subtracting the ledger's estimate from the declared budget — usable, but
    /// only as good as the estimate, and the surface must not imply otherwise.
    pub free_measured: bool,
    /// What the ledger believes is resident — the reconciliation counterpart to
    /// the devices' `used_bytes`.
    pub estimated_resident_bytes: u64,
    pub devices: Vec<GpuMemory>,
    pub resident: Vec<ResidentView>,
    pub queue: Vec<WaiterView>,
    pub note: String,
    /// The `hold.active` setting (gpu-hold design §6) — orthogonal to
    /// [`Self::enabled`]/[`Self::active`] above: hold is a switch on the
    /// resolve/start paths, not a capacity policy, so it is reported
    /// alongside them rather than folded in.
    pub hold_active: bool,
    /// `settings.hold.fallback_alias`, for surfaces that show what a held
    /// chat request without its own row override falls back to.
    pub hold_fallback_alias: Option<String>,
    /// Resident models that are busy or still starting while hold is active
    /// (§5/§6) — continuously visible here, not only in the one-shot
    /// `hold_set` response. Empty whenever hold is not active: residency is
    /// not "draining" on its own, only hold makes it so.
    ///
    /// `"{class}/{model_id}"`, the same spelling `HoldSweep` and the
    /// `hold_set` response use. One name for one thing: a model id alone is
    /// not unique across the three classes, and two shapes for the same list
    /// is how a surface ends up unable to match the op response against the
    /// live frame.
    pub draining: Vec<String>,
    /// The token ledger of every guarded unified-KV chat model with a request
    /// in flight or waiting (unified-KV design §3.3): the pool's size, what is
    /// reserved, and who is queued for room — the in-container counterpart of
    /// [`Self::queue`], shown next to it for the same reason. A wait for KV
    /// cells is invisible otherwise: the model is resident and "busy", and
    /// nothing else on this frame says why a request is not moving.
    pub kv_pools: Vec<crate::gate::pool::KvPoolView>,
    /// lmgw's own share of the pooled devices' used memory, **measured per
    /// process** (candidate-aliases design §4.7): every process of every lmgw
    /// container, plus those of just-stopped containers the driver still
    /// lists. Everything lmgw could free by evicting and waiting. `None` when
    /// it cannot be measured; [`Self::external_trigger_reason`] says why.
    pub lmgw_share_bytes: Option<u64>,
    /// Used memory that is not lmgw's: the pooled devices' used bytes minus
    /// [`Self::lmgw_share_bytes`] — games, the desktop, other apps, and what
    /// the driver keeps for itself. Memory lmgw cannot free. `None` exactly
    /// when the share is.
    pub outside_share_bytes: Option<u64>,
    /// Whether the outside-VRAM fallback (§4.7) can fire right now: the
    /// `vram.fallback_on_external` switch is on, admission is enabled, the
    /// hold is off and lmgw's share is measurable. A local model that is not
    /// resident and has a fallback is then answered by it at once when the
    /// VRAM that is short is VRAM outside lmgw's control.
    pub external_trigger_active: bool,
    /// Why it cannot, when it cannot: the switch, admission, the hold, no
    /// telemetry or process list, or the lmgw model that cannot be
    /// attributed. While it cannot, only the hold sends a local request to a
    /// fallback.
    pub external_trigger_reason: Option<String>,
    /// The benchmark run holding the card (benchmark design §3.2), if one
    /// does: like the hold, no local model is admitted while it runs.
    pub benchmark: Option<VramBenchmarkView>,
}

/// A benchmark run's GPU lease, for the status surfaces.
#[derive(Debug, Clone, Serialize)]
pub struct VramBenchmarkView {
    pub run_id: i64,
    pub model_id: String,
}

/// The learned-peak half of a resident image model's note — the *visible*
/// form of what admission does and does not know about this pipeline.
///
/// The unlearned case is a warning, not a null: until one generation has run,
/// the compute buffers are memory lmgw is measurably blind to, and the owner
/// is the only one who can close the gap (by generating once at the size they
/// actually use). Saying nothing there would make an admission that is about
/// to OOM look exactly like one that is safe.
pub(super) fn peak_note(footprint_note: Option<&str>, peak: Option<u64>) -> String {
    let sentence = match peak {
        Some(p) => format!(
            "learned peak +{} above idle, charged at admission",
            fmt_bytes(p)
        ),
        None => "peak not learned yet — run one generation at the largest size you use; until \
                 then other models may be admitted into memory the next generation needs"
            .to_string(),
    };
    match footprint_note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{n}. {sentence}"),
        None => sentence,
    }
}

const ESTIMATE_NOTE: &str =
    "Estimates are weights + KV cache read from the GGUF and the section's \
                             own flags — a lower bound, because compute buffers, the CUDA context \
                             and allocator slack have no metadata to derive them from. That \
                             remainder is the headroom setting.";

impl VramScheduler {
    /// The whole plane, for the status surfaces.
    pub async fn view(&self, state: &SharedState) -> VramView {
        let snap = state.snapshot();
        let l = self.ledger(state, &snap).await;
        let enabled = snap.settings.vram.enabled;
        let inactive_reason = if enabled {
            l.inactive_reason.clone()
        } else {
            Some("vram.enabled is off — the ledger is reporting only".into())
        };
        // §4.7's figures, measured only while the trigger is armed: with the
        // switch off (or admission off, or the hold on) the answer is the
        // reason, and nothing is probed for it.
        let (lmgw_share_bytes, outside_share_bytes, external_trigger_reason) =
            match trigger_off(&snap).or_else(|| self.boot_unsettled()) {
                Some(why) => (None, None, Some(why)),
                None => match self.measure(state, &snap, &l, Fill::View).await {
                    Ok(m) => (Some(m.shares.lmgw), Some(m.shares.outside), None),
                    Err(why) => (None, None, Some(why)),
                },
            };
        // Reporting only — the hold itself is enforced at resolve time
        // (`Snapshot::resolve_for_request`) and in `admit_local`, and the
        // stopping is `lifecycle::hold_sweep`'s job. "Draining" while hold is
        // not active is meaningless — residency alone never means that — so
        // the list is empty then regardless of what the registry reports.
        //
        // Read off the ledger's own residents rather than re-probing `/slots`:
        // this view is built on every `vram` frame, and a probe per resident
        // per frame would put a control-plane round trip on a display path.
        // The sweep does probe, so a model this lists as draining because a
        // direct client is generating on its port is stopped by the next tick,
        // not by this view.
        let hold_active = snap.settings.hold.active;
        let draining: Vec<String> = if hold_active {
            l.residents
                .iter()
                .filter(|r| r.state == "starting" || r.in_flight > 0)
                .map(|r| format!("{}/{}", r.container.as_str(), r.model))
                .collect()
        } else {
            Vec::new()
        };
        VramView {
            enabled,
            active: enabled && l.capacity.is_some(),
            inactive_reason,
            telemetry: self.probe().source(),
            telemetry_ok: l.telemetry_error.is_none(),
            headroom_bytes: snap.settings.vram.headroom_mb.saturating_mul(MIB),
            capacity_bytes: l.capacity.as_ref().map(|c| c.total).unwrap_or(0),
            free_bytes: l.capacity.as_ref().map(|c| c.free),
            free_measured: l.capacity.as_ref().is_some_and(|c| c.measured),
            estimated_resident_bytes: l.estimated_resident,
            devices: l.devices,
            resident: l.residents,
            queue: self.queue_view(),
            note: ESTIMATE_NOTE.to_string(),
            hold_active,
            hold_fallback_alias: snap.settings.hold.fallback_alias.clone(),
            draining,
            kv_pools: state.kv_pools.view(),
            lmgw_share_bytes,
            outside_share_bytes,
            external_trigger_active: external_trigger_reason.is_none(),
            external_trigger_reason,
            benchmark: snap.gpu_lease.as_ref().map(|l| VramBenchmarkView {
                run_id: l.run_id,
                model_id: l.model_id.clone(),
            }),
        }
    }
}
