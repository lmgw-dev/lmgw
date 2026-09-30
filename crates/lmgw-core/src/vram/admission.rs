//! Admission

use serde::Serialize;

use crate::config::{Route, Snapshot};
use crate::error::GatewayError;
use crate::hf::fmt_bytes;
use crate::runtime::descriptor::model_runtime;
use crate::runtime::Class;
use crate::state::SharedState;

use super::ledger::{trigger_off, Shares};
use super::scheduler::Reserved;
use super::{classify, LocalHold, Target};
/// The verdict of [`VramScheduler::check_background_start`](crate::vram::VramScheduler::check_background_start).
pub enum Fit {
    /// Admission is not arbitrating this start — proceed unarbitrated.
    Unchecked,
    /// It fits. The permit holds the ledger reservation until the start
    /// settles; drop it after the `acquire` returns, success or failure.
    Go(StartPermit),
    /// It does not fit, and this says why — a sentence for a log line or an
    /// op response, naming the model and what is in the way.
    Full(String),
    /// The GPU hold is on (gpu-hold design §4), so nothing starts at all — not
    /// a warm start, not an operator start. Carried as its own variant rather
    /// than folded into [`Self::Full`] because the two are opposite actions
    /// for the owner: `Full` means "make room", and this means "release the
    /// hold". Reporting a hold as "not enough GPU memory" would send them
    /// hunting for a model to evict on an empty card.
    Held(String),
}

/// A background start admission has measured and claimed memory for.
pub struct StartPermit {
    pub(super) _reservation: Option<Reserved>,
}

/// §4.7's verdict for a local model that is not resident: is the VRAM it
/// would need VRAM lmgw can make room in itself?
/// ([`VramScheduler::external_verdict`](crate::vram::VramScheduler::external_verdict).)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalVerdict {
    /// Today's path: the model is up (or coming up), or `free + lmgw_share ≥
    /// needed` — lmgw can make room itself by evicting idle models and
    /// waiting for busy ones. Contention between lmgw's own models queues,
    /// as it always has.
    Fits,
    /// The shortfall is VRAM lmgw cannot free: even with every lmgw model
    /// gone, `needed` would not fit next to what is outside its control. A
    /// caller with a fallback answers with it at once — waiting would not
    /// help.
    External(ExternalShortfall),
    /// `needed` exceeds the capacity (`vram.budget_mb` or the devices'
    /// total): today's `vram_too_large`. A configuration error, never a
    /// fallback.
    TooLarge,
    /// The trigger cannot be evaluated — switched off, admission off, the
    /// hold on, no telemetry or process list, an lmgw model that cannot be
    /// attributed — and this is why. Hold-only behaviour: today's path.
    Unavailable(String),
}

/// The numbers behind an [`ExternalVerdict::External`], pooled over every
/// device the way the ledger pools them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalShortfall {
    pub class: Class,
    pub model: String,
    /// Footprint + `vram.headroom_mb`, exactly what admission would wait for.
    pub needed_bytes: u64,
    /// Free as measured.
    pub free_bytes: u64,
    /// lmgw's own share, measured per process: what it could free.
    pub lmgw_share_bytes: u64,
    /// Used memory outside lmgw's control.
    pub outside_bytes: u64,
}

impl ExternalShortfall {
    /// One sentence, for a log line or a response note.
    pub fn describe(&self) -> String {
        format!(
            "{}/{} needs {}; {} is free, lmgw's own models hold {} and {} is used outside \
             lmgw's control",
            self.class.as_str(),
            self.model,
            fmt_bytes(self.needed_bytes),
            fmt_bytes(self.free_bytes),
            fmt_bytes(self.lmgw_share_bytes),
            fmt_bytes(self.outside_bytes)
        )
    }
}

/// §4.7's table on measured numbers, in its order: larger than the capacity
/// is `TooLarge` first; room lmgw can make itself is `Fits`; anything else
/// is short by memory lmgw cannot free.
pub(super) fn external_table(
    target: &Target,
    needed: u64,
    capacity: u64,
    free: u64,
    shares: Shares,
) -> ExternalVerdict {
    if needed > capacity {
        return ExternalVerdict::TooLarge;
    }
    if free.saturating_add(shares.lmgw) >= needed {
        return ExternalVerdict::Fits;
    }
    ExternalVerdict::External(ExternalShortfall {
        class: target.class,
        model: target.model_id.clone(),
        needed_bytes: needed,
        free_bytes: free,
        lmgw_share_bytes: shares.lmgw,
        outside_bytes: shares.outside,
    })
}

/// The fallback a caller of [`admit_or_external`] could answer with: the
/// caller's word that one exists and resolves to a non-local route — and,
/// optionally, the check that decides whether it may really take *this*
/// request, deferred until a verdict says `External` (review finding 6).
/// The site's route check can cost a catalog read (image routes), which a
/// request that fits must never pay; it runs at most once, and a fallback it
/// refuses is none for the rest of the admission, which is then today's.
pub struct ExternalFallback<'a> {
    confirm: Option<futures::future::BoxFuture<'a, bool>>,
    usable: bool,
}

impl<'a> ExternalFallback<'a> {
    /// A fallback already known to be usable for this request.
    pub fn usable() -> Self {
        Self {
            confirm: None,
            usable: true,
        }
    }

    /// A fallback whose usability `confirm` decides, the first time a verdict
    /// says it could answer.
    pub fn confirmed_by(confirm: impl std::future::Future<Output = bool> + Send + 'a) -> Self {
        Self {
            confirm: Some(Box::pin(confirm)),
            usable: true,
        }
    }

    /// Not refused by its check yet: a verdict is still worth taking.
    pub(super) fn live(&self) -> bool {
        self.usable
    }

    /// May it answer? Runs the deferred check once.
    pub(super) async fn confirm(&mut self) -> bool {
        if let Some(check) = self.confirm.take() {
            self.usable = check.await;
        }
        self.usable
    }
}

impl std::fmt::Debug for ExternalFallback<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalFallback")
            .field("confirmed", &self.confirm.is_none())
            .field("usable", &self.usable)
            .finish()
    }
}

/// What [`VramScheduler::take_gate`](crate::vram::VramScheduler::take_gate) came to.
pub(crate) enum Gated<'g> {
    Held(tokio::sync::MutexGuard<'g, ()>),
    /// While it waited, the shortfall became VRAM outside lmgw's control.
    External(ExternalShortfall),
    /// The budget ran out first.
    Expired,
}

/// What [`VramScheduler::decide`](crate::vram::VramScheduler::decide) settled on.
pub(super) enum Decided {
    /// Start (or join) the container. The reservation, when there is one, is
    /// held until the `acquire` returns; `None` = nothing to arbitrate, or
    /// somebody else's start already accounts for the model.
    Go(Option<Reserved>),
    /// While the request waited, the shortfall became VRAM outside lmgw's
    /// control, and its fallback answers (review finding 8).
    External(ExternalShortfall),
}

/// What [`admit_or_external`] decided.
#[derive(Debug)]
pub enum Admission {
    /// Exactly what [`admit`] returns: `None` for a route with no local
    /// container in its future, otherwise the claim on the container that is
    /// now up.
    Admitted(Option<LocalHold>),
    /// VRAM outside lmgw's control is short (§4.7). Nothing was queued,
    /// evicted or started; the caller answers with its fallback.
    External(ExternalShortfall),
}

/// Gate a resolved route on GPU memory, and make its model reachable.
///
/// `Ok(None)` means there is no local container in this route's future — a
/// cloud upstream, or a local id this gateway has no row for. The caller
/// forwards exactly as it would have.
///
/// `Ok(Some(hold))` means the model's container is up and claimed. **The
/// caller must overwrite `route.upstream.base_url` with
/// [`LocalHold::endpoint`] before building the egress URL** (§5): a local
/// route's configured base URL is the class-wide router port, which per-model
/// containers do not have. Reading a local route's `base()` without a hold is
/// a bug by definition.
///
/// Arbitration (the ledger, the queue, eviction) runs *before* the acquire and
/// only when active. When it is inactive the acquire still happens — that is
/// §3.2's whole point, and the degraded mode's "the whole gateway runs
/// unchanged" only stays true because of it.
pub async fn admit(
    state: &SharedState,
    route: &Route,
    alias: &str,
) -> Result<Option<LocalHold>, GatewayError> {
    let snap = state.snapshot();
    let Some(target) = classify(route) else {
        return Ok(None);
    };
    // No row describes this model: there is nothing to start and nothing to
    // size. Nothing unknown is ever blocked on a guess, and nothing unknown is
    // ever started on one either.
    let Some(runtime) = model_runtime(&snap, target.class, &target.model_id) else {
        return Ok(None);
    };
    state
        .vram
        .admit_plain(state, &snap, &target, &runtime, alias)
        .await
}

/// The local model whose admission could answer [`Admission::External`] for
/// `route`, if there is one: a local model with a row, not up (neither
/// `ready` nor `starting`), with the trigger armed — `vram.fallback_on_external`
/// on, admission enabled, the hold off.
///
/// Costs a snapshot read and the registry's in-memory list; probes nothing.
/// A caller asks this *before* it looks up and checks a fallback, so a
/// request that cannot be sent to one — the switch off, a model already
/// loaded — does no fallback work at all and is admitted exactly as before
/// (candidate-aliases design §7 item 14). `None` only means "no verdict could
/// change this admission"; [`admit_or_external`] still re-checks everything.
pub fn external_armed(state: &SharedState, snap: &Snapshot, route: &Route) -> Option<Target> {
    let target = classify(route)?;
    if trigger_off(snap).is_some()
        || model_runtime(snap, target.class, &target.model_id).is_none()
        || state.vram.is_up(state, &target)
    {
        return None;
    }
    Some(target)
}

/// [`admit`], with §4.7's outside-VRAM verdict taken **before** anything
/// queues (candidate-aliases design §5 stage 5) — and again on every pass of
/// the wait, should the request queue (review finding 8): waiting does not
/// help when the shortfall has become VRAM outside lmgw's control.
///
/// **Who calls it:** the request gate's admission (`gate::open`), for a route
/// whose model has a fallback (`gate::usable_fallback`: the row's fallback,
/// `Snapshot::fallback_route`) or, for a non-background candidate alias, the
/// alias's own — and phase 4's candidate path. `fallback` is the caller's
/// word that one exists and resolves to a non-local route, with the check
/// that decides whether it may take this request deferred until a verdict
/// says it could ([`ExternalFallback`]); without one (`None`) this is
/// [`admit`], exactly: no probe, no `podman inspect`, the same queue.
///
/// **What the caller must do with [`Admission::External`]:** swap to the
/// fallback's route and stamp `x-lmgw-fallback: <alias>` plus
/// `x-lmgw-fallback-reason: external_vram` on the response. Nothing is held:
/// no container was started or claimed for the model.
///
/// The verdict is computed only when it can matter: `vram.fallback_on_external`
/// on, admission enabled, the hold off, the model not up (not `ready` or
/// `starting`), and a fallback. `TooLarge` and `Unavailable` verdicts go on to
/// today's path, which refuses the first with `vram_too_large` and queues the
/// second; so does an `External` whose fallback the deferred check refuses.
/// A request that queues with a fallback still usable has the verdict taken
/// again every poll while it waits ([`VramScheduler::decide`](crate::vram::VramScheduler::decide)); an `External`
/// then takes it out of the queue — nothing reserved, nothing started — and
/// is answered the same way.
pub async fn admit_or_external(
    state: &SharedState,
    route: &Route,
    alias: &str,
    fallback: Option<ExternalFallback<'_>>,
) -> Result<Admission, GatewayError> {
    let snap = state.snapshot();
    let Some(target) = classify(route) else {
        return Ok(Admission::Admitted(None));
    };
    let Some(runtime) = model_runtime(&snap, target.class, &target.model_id) else {
        return Ok(Admission::Admitted(None));
    };
    let mut fallback = fallback;
    if let Some(fb) = fallback.as_mut() {
        match state.vram.external_verdict(state, &snap, &target).await {
            ExternalVerdict::External(short) => {
                if fb.confirm().await {
                    log_external(alias, &short, false);
                    return Ok(Admission::External(short));
                }
            }
            ExternalVerdict::Unavailable(why) => {
                tracing::debug!("outside-VRAM fallback not evaluated for '{alias}': {why}");
            }
            ExternalVerdict::Fits | ExternalVerdict::TooLarge => {}
        }
    }
    let fallback = fallback.as_mut().filter(|f| f.live());
    state
        .vram
        .admit_local(state, &snap, &target, &runtime, alias, fallback)
        .await
}

/// The one `info` line an outside-VRAM answer writes (§12 entry 26).
pub(super) fn log_external(alias: &str, short: &ExternalShortfall, while_waiting: bool) {
    let when = if while_waiting {
        " while it waited"
    } else {
        ""
    };
    tracing::info!(
        alias = %alias,
        needed = short.needed_bytes,
        free = short.free_bytes,
        lmgw_share = short.lmgw_share_bytes,
        outside = short.outside_bytes,
        "VRAM outside lmgw's control is short{when} — '{alias}' goes to its fallback: {}",
        short.describe()
    );
}
