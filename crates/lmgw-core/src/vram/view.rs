//! Status view

use serde::Serialize;

use crate::hf::fmt_bytes;
use crate::runtime::Class;
use crate::state::SharedState;

use super::attribution::Fill;
use super::ledger::{CardCapacity, Measured};
use super::nvml::{DeviceId, GpuMemory};
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
    /// What it is charged: [`Self::charge`]'s total, the figure the
    /// decisions read.
    pub estimated_bytes: u64,
    /// What it is charged on the cards (device-ledger design §4.1, WP4): a
    /// part per card where that is known, else one part for the cards it
    /// sees together ([`crate::vram::charge`]); empty where it is charged
    /// nowhere.
    pub charge: super::CardCharge,
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
    /// What a `ready` audio container will still load on its first request
    /// and the driver cannot show yet (realtime design §9.4,
    /// [`residency`](crate::vram::residency)) — admission keeps it free on
    /// top of the measured use. Its expected residency less what its
    /// processes hold now; where that could not be read, less what it was
    /// read holding at rest (its CUDA context, or an eager row's weights),
    /// and where neither was read, all of it for a lazy row and what comes
    /// on top of its weights file for an eager one. Again after audio.cpp
    /// unloaded it for idling. `None` while the model is loaded — or holds
    /// at least that much now, loaded by a request lmgw did not route — and
    /// for every other class.
    pub pending_bytes: Option<u64>,
    /// Why the estimate is partial, when it is.
    pub note: Option<String>,
    /// The cards its processes are on, with what they hold on each, as
    /// lmgw's measured share reads them (device-ledger design §10, WP2):
    /// a model split over two cards has both. Empty where the share is not
    /// measured, and for a reservation.
    pub cards: Vec<ResidentCardView>,
    /// The registry generation it was built from, which the measurement's
    /// containers are keyed by; `None` for a reservation. Not published.
    #[serde(skip)]
    pub(crate) generation: Option<u64>,
    /// What the ledger keeps free for it beside its charge, on the cards it
    /// is charged on: a llama-server's first-prompt margin until it has
    /// served a request (`headroom::LLAMA_MARGIN`). Freed with it, so
    /// counted in what stopping it frees ([`super::Ledger::held_on`];
    /// owner, 2026-11-18). Not published.
    #[serde(skip)]
    pub(crate) kept_bytes: u64,
}

/// One card a resident's processes are on ([`ResidentView::cards`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResidentCardView {
    /// The card's id, as [`GpuMemory::id`].
    pub id: DeviceId,
    /// What its processes hold on that card; `None` where the driver lists
    /// one of them there without a figure.
    pub bytes: Option<u64>,
}

/// One card, as the status surfaces show it (device-ledger design §10,
/// WP2): the driver's figures, lmgw's share there, the outside use there,
/// what is kept free there where that is known per card, and lmgw's models
/// on it.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    #[serde(flatten)]
    pub memory: GpuMemory,
    /// lmgw's share of this card's used memory, measured per process.
    /// `None` when it cannot be measured: [`VramView::external_trigger_reason`]
    /// says why, or [`Self::share_unknown`] where it is this card alone.
    pub lmgw_share_bytes: Option<u64>,
    /// This card's used memory less lmgw's share, never below zero. `None`
    /// exactly when the share is.
    pub outside_share_bytes: Option<u64>,
    /// Why lmgw's share here is not known while a measurement was taken:
    /// the driver lists one of lmgw's processes here without a figure. The
    /// other cards' shares stay known; the sums are unknown with it.
    pub share_unknown: Option<String>,
    /// What admission keeps free here on top of the memory in use: what
    /// the models charged on this card alone keep free
    /// ([`CardCapacity::kept_free`]). `None` where a model charged on
    /// several cards together keeps something free; [`VramView::kept_free`]
    /// is all cards' together, and on a machine of one card this is all of
    /// it.
    pub kept_free: Option<KeptFree>,
    /// lmgw's models on this card, with what each holds here, as the
    /// measurement reads them ([`ResidentView::cards`]). Empty where the
    /// share is not measured.
    pub resident: Vec<DeviceResidentView>,
}

/// One of lmgw's models on a card ([`DeviceView::resident`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceResidentView {
    pub container: Class,
    pub model: String,
    /// What its processes hold on this card; `None` where the driver lists
    /// one of them here without a figure.
    pub bytes: Option<u64>,
}

impl DeviceView {
    /// The status surfaces' cards: the ledger's ([`CardCapacity`]), each
    /// with the residents whose processes are on it.
    pub(super) fn of(cards: Vec<CardCapacity>, residents: &[ResidentView]) -> Vec<Self> {
        cards
            .into_iter()
            .map(|c| {
                let resident = residents
                    .iter()
                    .flat_map(|r| {
                        r.cards.iter().filter(|on| on.id == c.memory.id).map(|on| {
                            DeviceResidentView {
                                container: r.container,
                                model: r.model.clone(),
                                bytes: on.bytes,
                            }
                        })
                    })
                    .collect();
                let (lmgw_share_bytes, share_unknown) = match c.lmgw {
                    Some(Ok(b)) => (Some(b), None),
                    Some(Err(why)) => (None, Some(why)),
                    None => (None, None),
                };
                DeviceView {
                    memory: c.memory,
                    lmgw_share_bytes,
                    outside_share_bytes: c.outside,
                    share_unknown,
                    kept_free: c.kept_free,
                    resident,
                }
            })
            .collect()
    }
}

/// What the ledger keeps free on top of the memory in use, by reason — what
/// [`VramView::free_bytes`] is lowered by, so a surface can say why the free figure
/// is less than the driver's. With no device reading (budget only) only the
/// image peaks are subtracted there: the rest is in the estimate already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KeptFree {
    /// Containers still starting, and starts admitted whose container is not
    /// up yet: weights on their way onto the card.
    pub starting_bytes: u64,
    /// What resident image pipelines have been measured to take above their
    /// idle residency while they draw (the residents' `peak_extra_bytes`),
    /// and for each whose weights wait off the card (`vram::partial`) the
    /// part of its charge — a partial start's grant (or its budget and
    /// context, where more), or its context and peak — that it does not hold
    /// now and the next generation puts there. In the budget-only branch only the former: the latter is in
    /// the estimate.
    pub image_peak_bytes: u64,
    /// What ready audio containers will still load on their first request
    /// (the residents' `pending_bytes`).
    pub pending_load_bytes: u64,
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
    /// The same, typed (chat-media-tools design §5.3); absent for a stage
    /// only words say.
    pub phase: Option<super::stage_watch::WaitPhase>,
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
    /// What `free_bytes` was lowered by on top of the memory in use, by
    /// reason: memory the driver cannot show yet but admission keeps free.
    /// With a measured free figure, `free_bytes` is the capacity's measured
    /// free memory — the devices' free memory, or with `vram.budget_mb` the
    /// budget less the devices' used memory — less these three (never below
    /// zero). The devices' total less their used and free memory is in
    /// neither: memory the probe reports as not available (NVML: the
    /// driver's own reserve; an AMD APU: GTT the host's RAM cannot back).
    pub kept_free: KeptFree,
    /// What the ledger believes is resident — the reconciliation counterpart to
    /// the devices' `used_bytes`.
    pub estimated_resident_bytes: u64,
    /// Every card, in the driver's order, with its own figures and lmgw's
    /// models on it (device-ledger design §10). The fields above are the
    /// cards' sums.
    pub devices: Vec<DeviceView>,
    pub resident: Vec<ResidentView>,
    pub queue: Vec<WaiterView>,
    /// Starts whose model's engine image is still downloading, before
    /// admission (machine-check design §5): no place in [`Self::queue`] and
    /// nothing on the card; position 0, needs 0, phase `downloading`.
    pub downloading: Vec<WaiterView>,
    pub note: String,
    /// The `hold.active` setting (gpu-hold design §6) — orthogonal to
    /// [`Self::enabled`]/[`Self::active`] above: hold is a switch on the
    /// resolve/start paths, not a capacity policy, so it is reported
    /// alongside them rather than folded in.
    pub hold_active: bool,
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
    /// lmgw's own share of the used memory of all cards together, **measured
    /// per process** (candidate-aliases design §4.7): every process of every
    /// lmgw container, plus those of just-stopped containers the driver still
    /// lists. Everything lmgw could free by evicting and waiting. `None` when
    /// it cannot be measured, [`Self::external_trigger_reason`] says why; a
    /// card whose share is unknown makes the sum unknown
    /// ([`DeviceView::share_unknown`]).
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

const ESTIMATE_NOTE: &str = "Estimates are weights + KV cache + compute buffers, read from the \
                             GGUF and the section's own flags: the KV cache as llama.cpp lays it \
                             out for the row's slots and speculative decoding, the compute \
                             buffer of every context the row runs (a chat model's MTP or draft \
                             context too) from its ubatch, batch and context, fitted to \
                             llama.cpp's own figures (within a few percent where the masks and \
                             a quantized cache's copy dominate; a chat context's activations \
                             -5 % to +42 %, at most 80 MiB; an encoder's up to 30 % high). A \
                             llama.cpp model is charged the engine's own cost on each card it \
                             is on beside them (its CUDA context and what else it keeps there: \
                             learned per card, else the backend's stated default). Not derived, \
                             and so not in them: allocator slack and a projector's compute \
                             buffer. That remainder is the headroom setting.";

/// Each resident's cards, from what the measurement read of its container's
/// processes (device-ledger design §10, WP2): a container the measurement
/// did not read, or a reservation, keeps none.
fn on_cards(residents: &mut [ResidentView], m: &Measured) {
    for r in residents.iter_mut() {
        let Some(member) = r
            .generation
            .and_then(|g| m.members.iter().find(|c| c.generation == g))
        else {
            continue;
        };
        r.cards = member
            .cards
            .iter()
            .map(|(id, bytes)| ResidentCardView {
                id: id.clone(),
                bytes: *bytes,
            })
            .collect();
    }
}

impl VramScheduler {
    /// The whole plane, for the status surfaces.
    pub async fn view(&self, state: &SharedState) -> VramView {
        let snap = state.snapshot();
        let l = self.ledger(state, &snap).await;
        let enabled = snap.settings.vram.enabled;
        let inactive_reason = if enabled {
            l.inactive_reason.clone()
        } else {
            Some(format!(
                "GPU memory arbitration is off ({}) — the ledger is reporting only",
                crate::config::ADMISSION_LABEL
            ))
        };
        // §4.7's figures, measured only while the trigger is armed: with the
        // switch off (or admission off, or the hold on) the answer is the
        // reason, and nothing is probed for it.
        //
        // A measurement with one card unknown still says the other cards'
        // shares, and who is on them; the sums are unknown with it.
        let (measured, lmgw_share_bytes, outside_share_bytes, external_trigger_reason) =
            match trigger_off(&snap).or_else(|| self.boot_unsettled()) {
                Some(why) => (None, None, None, Some(why)),
                None => match self.measure(state, &snap, &l, Fill::View).await {
                    Ok(m) => match m.shares() {
                        Ok(s) => (Some(m), Some(s.lmgw), Some(s.outside), None),
                        Err(why) => (Some(m), None, None, Some(why)),
                    },
                    Err(why) => (None, None, None, Some(why)),
                },
            };
        let mut residents = l.residents.clone();
        if let Some(m) = &measured {
            on_cards(&mut residents, m);
        }
        let devices = DeviceView::of(l.cards(measured.as_ref()), &residents);
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
            headroom_bytes: self.headroom(&snap).bytes,
            capacity_bytes: l.capacity.as_ref().map(|c| c.total).unwrap_or(0),
            free_bytes: l.capacity.as_ref().map(|c| c.free),
            free_measured: l.capacity.as_ref().is_some_and(|c| c.measured),
            kept_free: l.capacity.as_ref().map(|c| c.kept_free).unwrap_or_default(),
            estimated_resident_bytes: l.estimated_resident,
            devices,
            resident: residents,
            queue: self.queue_view(),
            downloading: self.downloads_view(),
            note: ESTIMATE_NOTE.to_string(),
            hold_active,
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
