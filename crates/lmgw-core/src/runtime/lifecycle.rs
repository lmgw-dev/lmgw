//! Where the container runtime meets the app's lifetime (design §3.4, §3.7).
//!
//! [`registry`](super::registry) owns the podman verbs and the map; this
//! module owns *when* they run. Five moments, and they are the whole of the
//! runtime's relationship with the process around it:
//!
//! - [`boot`]: reconcile against what podman is already running, sweep the
//!   router-mode containers a pre-upgrade install left behind (by the names
//!   the §6 settings migration preserved), then start the `warm_start` models.
//! - [`reap_idle`]: one pass of the idle reaper (§3.7), driven by the server's
//!   background tick — beside a reconciliation pass ([`readopt`]) that makes
//!   a running container the registry lost track of lmgw's again.
//! - [`shutdown`]: stop everything, bounded, on the way out (§3.4).
//! - [`drop_model`]: a deleted or disabled model's container goes now, not at
//!   the next boot.
//! - [`stop_for_apply`]: a configuration change to a *running* model (§3.6).
//!
//! The asymmetry between [`shutdown`] and [`boot`] is the design, not an
//! oversight: a clean quit takes the GPU with it, and a crash deliberately
//! leaves the containers up for [`boot`] to adopt — which is what makes an
//! lmgw restart cost nothing in reload time.

use std::path::Path;
use std::time::Duration;

use crate::config::Snapshot;
use crate::state::{AppState, SharedState};
use crate::vram::Fit;

use super::descriptor::{higher_rungs, model_runtimes, ModelRuntime};
use super::registry::{AcquireSpec, Presence, RuntimeError, RuntimeState};
use super::Class;

mod readopt;
pub use readopt::{readopt, readopt_in_background};

/// How often the idle reaper looks (§3.7).
///
/// A sampling rate, not a policy: the policy is each model's `idle_seconds`,
/// and this only bounds how late a reap can be relative to it. Coarse on
/// purpose — the work it guards costs a `podman stop`, and a model that has
/// been unused for ten minutes does not care about fifteen seconds.
pub const REAP_INTERVAL: Duration = Duration::from_secs(15);

/// Total wall clock [`shutdown`] gives every container together.
///
/// Not a per-container grace (that is `podman stop -t`, and
/// `vram.unload_timeout_seconds` bounds the wait after it) but a ceiling on
/// the whole concurrent sweep, because the thing on the other side of it is a
/// tray app the owner just asked to quit. Reaching it is logged with the
/// containers that were still recorded, so a machine where this is too short
/// says so rather than silently orphaning them.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Everything `acquire`/`reconcile` need for one model, assembled from the
/// snapshot the same way on every path (§3.2).
///
/// The one place the class→`models_dir` mapping is written down: chat and aux
/// keep their own directories, audio a third and image a fourth (§6,
/// image-generation §4), and a start that resolved that differently from a
/// reconciliation would compare a container against argv it was never started
/// with.
///
/// The data dir and the dev flag come from `state` itself, so a start can
/// never be handed one instance's data dir with another's rule: a dev
/// instance whose models dir lies outside its data dir starts with
/// `may_write_models_dir: false` (owner ruling 2026-10-04).
pub fn acquire_spec<'a>(
    state: &'a AppState,
    snap: &'a Snapshot,
    runtime: &'a ModelRuntime,
) -> AcquireSpec<'a> {
    let s = &snap.settings;
    let models_dir: &str = match runtime.class {
        Class::Chat => &s.router.models_dir,
        Class::Aux => &s.aux_router.models_dir,
        Class::Audio => &s.audio.models_dir,
        Class::Image => &s.image.models_dir,
    };
    AcquireSpec {
        runtime,
        container_prefix: &s.container_prefix,
        models_dir,
        data_dir: &state.data_dir,
        may_write_models_dir: models_dir.trim().is_empty()
            || state
                .refuse_shared_models_dir(Path::new(models_dir))
                .is_ok(),
        load_timeout: Duration::from_secs(s.vram.load_timeout_seconds),
        stop_timeout: Duration::from_secs(s.vram.unload_timeout_seconds),
    }
}

/// Sets [`crate::vram::VramScheduler::set_boot_settled`] when dropped — see
/// [`boot`].
struct BootSettled<'a>(&'a SharedState);

impl Drop for BootSettled<'_> {
    fn drop(&mut self) {
        self.0.vram.set_boot_settled(true);
    }
}

/// Boot: adopt, sweep, warm up (§3.4). Runs once, before anything else
/// touches the runtime.
///
/// Nothing here is fatal. A box with no podman at all logs three warnings and
/// serves cloud upstreams exactly as before — the same posture the VRAM plane
/// takes when there is no GPU telemetry.
pub async fn boot(state: &SharedState) {
    let snap = state.snapshot();
    let runtimes = model_runtimes(&snap);
    // A ladder row's higher rungs: a container a previous lmgw had climbed is
    // adopted at the rung it runs (ladder design §3.1 — a crash is not a
    // container stop). Candidates for adoption only; a start is the base.
    let rungs = higher_rungs(&snap);
    // Only enabled models are candidates: a container for a disabled model is
    // by definition one reconciliation removes, and passing it here would
    // adopt it instead.
    let specs: Vec<AcquireSpec<'_>> = runtimes
        .iter()
        .filter(|r| r.enabled)
        .map(|r| acquire_spec(state, &snap, r))
        .collect();
    let candidates = readopt::candidates(state, &snap, &runtimes, &rungs);

    let registry = state.runtime();
    // Until adoption and the legacy sweep are over, a container a previous
    // lmgw left on the card is not in the registry, and its memory would read
    // as outside use to the outside-VRAM verdict (candidate-aliases §12,
    // review finding 1) — which is therefore unavailable until this guard
    // drops. A guard, so every way out of this block settles it: a podman
    // that cannot be run, a sweep that fails, a panic. Once boot is over,
    // what the registry does not hold is not lmgw's to free.
    let settled = BootSettled(state);
    // Leftover benchmark containers first (benchmark design §3.3): no run
    // survives a restart, so each one is a dead run's, holding VRAM nothing
    // accounts for. Reconciliation below skips them by their label.
    crate::bench::boot_sweep(state).await;
    let report = registry
        .reconcile(&snap.settings.container_prefix, &candidates)
        .await;
    for e in &report.errors {
        tracing::warn!("boot reconciliation: {e}");
    }
    tracing::info!(
        "boot reconciliation: adopted {}, removed {}",
        report.adopted.len(),
        report.removed.len()
    );
    // What podman still runs is on the card from the first moment: learn the
    // adopted containers' PIDs now rather than at the first verdict (§4.7).
    state.vram.cache_pids(state);

    if report.listed {
        legacy_sweep(state, &snap, &report.adopted).await;
    } else if !snap.settings.legacy_container_names.is_empty() {
        tracing::warn!(
            "skipping the router-mode container sweep: podman ps did not answer, so there is no \
             adoption list to check the router-mode names against"
        );
    }
    drop(settled);

    // A hold that survived the restart (gpu-hold design §2/§5): reconciliation
    // has just adopted whatever podman was still running, so this is where
    // those containers are handed back. Only the models on the CPU are then
    // warm-started — they use no VRAM, so the hold has no claim on them;
    // leaving the GPU ones out here rather than relying on `Fit::Held` to
    // skip them one by one is faster: `boot` is spawned rather than
    // awaited, and the `join_all` below would hold up the free for as long
    // as the slowest skip takes.
    let held = snap.settings.hold.active;
    if held {
        let swept = hold_sweep(state).await;
        tracing::info!(
            "GPU hold is active: stopped {} adopted container(s), {} still draining, {} \
             failed to stop, {} kept on the CPU; warm starts of models on the GPU are skipped \
             until it is released",
            swept.stopped.len(),
            swept.draining.len(),
            swept.failed.len(),
            swept.kept_on_cpu.len()
        );
    }

    // Warm starts (§3.4: "nothing auto-starts except `warm_start` models").
    // Concurrently, because that is what the per-model design bought — N cold
    // starts overlap instead of queueing (§4/§10.6).
    let warm: Vec<&AcquireSpec<'_>> = specs
        .iter()
        .filter(|s| s.runtime.warm_start)
        .filter(|s| !held || !s.runtime.placement().is_gpu())
        .filter(|s| !registry.contains(s.runtime.class, &s.runtime.model_id))
        .collect();
    if warm.is_empty() {
        return;
    }
    futures::future::join_all(warm.iter().map(|spec| {
        let registry = registry.clone();
        let snap = &snap;
        async move {
            let (class, model_id) = (spec.runtime.class, &spec.runtime.model_id);
            // Warm starts are background warmth, not requests: nobody is
            // waiting on one, so it is *skipped* when the GPU is full rather
            // than allowed to evict something (§4). With `warm_start` on more
            // models than the card holds, the alternative is a boot that
            // evicts its way down the list and ends with one model resident
            // and a log full of stops nobody asked for. When admission is
            // inactive this check is a no-op and every flagged model starts,
            // exactly as before.
            let permit = match state
                .vram
                .check_background_start(state, snap, class, model_id)
                .await
            {
                Fit::Full(why) => {
                    tracing::warn!(
                        "not warm-starting {class} model '{model_id}': {why}. It will start on \
                         demand, evicting if it has to."
                    );
                    return;
                }
                // Unreachable on the normal path — the block above returns
                // before this one when hold is active — but a hold engaged
                // while these starts were already in flight lands here, and
                // then it is exactly right: skip, and say why.
                Fit::Held(why) => {
                    tracing::info!("not warm-starting {class} model '{model_id}': {why}");
                    return;
                }
                Fit::Go(permit) => Some(permit),
                Fit::Unchecked => None,
            };
            let started = registry.acquire(spec).await;
            drop(permit);
            match started {
                // Dropped immediately and on purpose: warm start means
                // resident, not claimed. The entry stays `ready` with
                // `in_flight = 0`, which is exactly what the reaper and the
                // eviction policy expect of an idle model.
                Ok(guard) => {
                    tracing::info!(
                        "warm-started {class} model '{model_id}' on {}",
                        guard.port()
                    );
                }
                Err(e) => tracing::warn!("warm start of {class} model '{model_id}': {e}"),
            }
        }
    }))
    .await;
    // The warm-started containers' PIDs, once, for all of them (§4.7).
    state.vram.cache_pids(state);
}

/// What one [`hold_sweep`] pass did, in the words the `hold_set` op response
/// and the log line use. Names are `"{class}/{model_id}"`.
#[derive(Debug, Default)]
pub struct HoldSweep {
    /// Containers this pass stopped.
    pub stopped: Vec<String>,
    /// Containers it left running because they are still working: starting,
    /// serving an lmgw request, or generating for a client that went straight
    /// to the published container port. The next pass tries again.
    pub draining: Vec<String>,
    /// Containers whose stop **failed**, as `"{class}/{model_id}: {error}"` —
    /// a `podman stop` that outlived `vram.unload_timeout_seconds`, a podman
    /// that could not be run at all.
    ///
    /// Its own bucket rather than folded into [`Self::draining`] because the
    /// two have different futures: a draining model is stopped by the next
    /// reaper tick, whereas a failed stop has already dropped the registry
    /// entry (runtime/registry/stop.rs: after a failed stop lmgw's belief about
    /// the container is worthless), so no sweep sees it again unless a
    /// reconciliation pass on a later tick ([`readopt`]) takes it up again —
    /// one this lmgw started, and with a backoff while it keeps failing. The
    /// owner engaged the hold to get their GPU back; a stop failure is the one
    /// thing here that cannot be silent, so it is named in the op response and
    /// logged at warn.
    pub failed: Vec<String>,
    /// Containers on the CPU, left running: they use no VRAM, so the hold
    /// has no claim on them (`runtime::Placement`).
    pub kept_on_cpu: Vec<String>,
}

/// Take lmgw off the GPU (gpu-hold design §5): stop every container that is
/// resident and idle, and report the ones that are still busy.
///
/// **Stop, never remove.** A stopped container is exactly what the next
/// start's `--replace` collects (per-model-containers §3.6), and removing it
/// would throw away the podman-side artifact the release path reuses. The
/// difference is invisible in `podman ps` and very visible in restart latency.
///
/// **Nothing is forced.** `stop(.., force = false)` refuses a model with an
/// in-flight claim, and that refusal is the point: hold means no *new* GPU
/// work, never killing work in progress (§2 — an hour-long agent loop keeps
/// its model, and is listed as draining for the whole hour). "Stop all models"
/// in the tray remains the axe.
///
/// **The `/slots` probe is the same one eviction runs**, for the same reason:
/// the dashboard publishes container ports, so a client can be mid-generation
/// against a model whose lmgw in-flight count is zero. An unanswerable `/slots`
/// counts as idle — exactly as it does for eviction — because audio.cpp has no
/// such endpoint at all, and reading "no answer" as "busy" would make an audio
/// container survive every sweep and hold the card indefinitely.
///
/// The engage race is closed by the registry, not here: `StartClaim::ready`
/// flips `Ready` and takes the in-flight claim under one lock, so a request
/// admitted a moment before the toggle owns a busy entry this pass skips, and
/// the reaper takes it once the request ends.
pub async fn hold_sweep(state: &SharedState) -> HoldSweep {
    let registry = state.runtime();
    let mut out = HoldSweep::default();
    for view in registry.list() {
        let name = format!("{}/{}", view.class.as_str(), view.model_id);
        // By the container's own placement: one started on the GPU before
        // its row was switched to the CPU is swept like any other.
        if !view.placement.is_gpu() {
            out.kept_on_cpu.push(name);
            continue;
        }
        if view.state != RuntimeState::Ready || view.in_flight > 0 {
            out.draining.push(name);
            continue;
        }
        if view.port != 0 {
            match crate::vram::busy_slots(&state.http, view.port, crate::vram::CONTROL_TIMEOUT)
                .await
            {
                Some(0) | None => {}
                Some(n) => {
                    tracing::debug!("hold sweep: {name} has {n} slot(s) still generating");
                    out.draining.push(name);
                    continue;
                }
            }
        }
        // The container `list()` showed idle, and no other: one restarted or
        // climbed since is newer work (ladder design §12 entry 20).
        match registry
            .stop_generation(view.class, &view.model_id, view.generation, false)
            .await
        {
            Ok(()) => {
                tracing::info!("hold sweep stopped {name}");
                out.stopped.push(name);
            }
            // Traffic arrived between `list()` and the stop, or the container
            // was replaced meanwhile. The claim is the guard that is supposed
            // to win here; the next tick asks again.
            Err(RuntimeError::Busy { .. } | RuntimeError::Moved { .. }) => out.draining.push(name),
            // Everything else is a container still on the GPU that no later
            // pass will pick up — see [`HoldSweep::failed`].
            Err(e) => {
                tracing::warn!("hold sweep: stopping {name} failed: {e}");
                out.failed.push(format!("{name}: {e}"));
            }
        }
    }
    out
}

/// The one-time legacy sweep (§3.4, §6): stop and remove the shared
/// router-mode containers, so an upgraded install never runs both worlds
/// against one GPU.
///
/// They carry no `lmgw.instance` label — they predate it — so they can only be
/// matched by the exact names the settings used to carry. Those fields are
/// gone with the shape migration (§6); the names survive in
/// [`Settings::legacy_container_names`](crate::config::Settings::
/// legacy_container_names), which `store::load_settings` lifted out of the
/// pre-migration JSON — the ordering the design demands (sweep from the old
/// values, then rewrite).
///
/// **The list is consumed, not just read.** It is cleared — and the clear is
/// persisted, which is also the save that drops the old keys — only when every
/// name was actually dealt with: removed, or confirmed absent. "Absent" rests
/// on the `podman ps` that reconciliation just ran successfully (the caller
/// only sweeps when it did), so a box with no podman keeps its list rather
/// than mistaking "cannot see" for "not there". A name whose
/// `podman rm` failed, or that was skipped because reconciliation had just
/// adopted a per-model container under that exact name, keeps the whole list
/// alive for the next boot. Clearing on a partial sweep would forget the one
/// name that still needs sweeping, and nothing else in the system could ever
/// rediscover it; keeping it costs one `podman inspect` per boot on an install
/// that has already been swept, which is why the clear exists at all.
///
/// The adoption guard: a name reconciliation just adopted is never swept. It
/// cannot normally happen — a per-model name is
/// `<prefix>-<class>-<slug>-<hash6>` — but `container_name` was a free-text
/// setting, and "the app deleted the model container it had just recovered" is
/// not a failure worth leaving possible.
async fn legacy_sweep(state: &SharedState, snap: &Snapshot, adopted: &[String]) {
    let names = &snap.settings.legacy_container_names;
    if names.is_empty() {
        return;
    }
    let registry = state.runtime();
    let mut all_handled = true;
    for name in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if adopted.iter().any(|a| a == name) {
            tracing::warn!(
                "not sweeping the router-mode container '{name}': reconciliation just adopted a \
                 per-model container under that exact name — check container_prefix"
            );
            all_handled = false;
            continue;
        }
        match registry.container_exists(name).await {
            Presence::Absent => continue,
            Presence::Unknown(why) => {
                // Not "it is gone": lmgw learned nothing. Dropping the name on
                // this answer would retire a sweep that never happened, and no
                // other part of the system carries the old container's name.
                tracing::warn!(
                    "cannot tell whether the router-mode container '{name}' is still \
                     there ({why}) — keeping it on the sweep list for the next boot"
                );
                all_handled = false;
                continue;
            }
            Presence::Present => {}
        }
        match registry.remove_unmanaged(name).await {
            Ok(()) => tracing::info!(
                "swept the router-mode container '{name}' — this install is now per-model \
                 containers only"
            ),
            Err(e) => {
                tracing::warn!("sweeping the router-mode container '{name}': {e}");
                all_handled = false;
            }
        }
    }
    if !all_handled {
        return;
    }
    // Persisting the empty list is what finally rewrites the settings blob
    // into the new shape: `Settings` no longer has the old per-class fields,
    // so serializing it drops them (§6).
    //
    // Held across the whole read-modify-write: see `AppState::settings_write`
    // (gpu-hold design §3.1) — this runs at boot, concurrently with nothing
    // today, but the lock is cheap and the alternative is a silent exception
    // to a rule every other settings writer follows.
    // The snapshot is published under it, and MCP reconciled once it is let
    // go (`AppState::settings_saved`), against the snapshot of then
    // (`AppState::reconcile_mcp`).
    let published = {
        let _guard = state.settings_write.lock().await;
        let mut settings = state.snapshot().settings.clone();
        settings.legacy_container_names.clear();
        match crate::store::save_settings(&state.db, &settings).await {
            Ok(()) => match state.publish_snapshot().await {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!("reloading settings after the router-mode sweep: {e}");
                    false
                }
            },
            // Not fatal: the sweep already happened, and a list that survives
            // it only costs the next boot an inspect per name.
            Err(e) => {
                tracing::warn!("clearing the swept router-mode container names: {e}");
                false
            }
        }
    };
    if published {
        state.reconcile_mcp().await;
    }
}

/// One pass of the idle reaper (§3.7): stop every `ready` model that has been
/// unused for longer than its own `idle_seconds`, and is not serving anything.
/// It also stops, once idle, an audio container a download or a delete left
/// on an old `server.json` ([`crate::runtime::audio::recheck_left`]).
///
/// **`idle_seconds` is read from the current snapshot, not from the entry.**
/// The registry entry captures `stop_timeout` at claim time because
/// [`Registry::stop`](super::registry::Registry::stop)'s signature cannot
/// carry settings; `idle_seconds` has no such constraint, and it is a policy
/// the owner changes in the UI expecting it to mean something *now* — a value
/// frozen at start time would leave a model that was started under
/// `idle = 0` resident forever after the owner set it to five minutes, with
/// nothing on any surface explaining why. Re-derived per tick, so the reaper
/// is always acting on what the config currently says.
///
/// A model with no descriptor at all (its row was deleted or disabled since it
/// started) is skipped rather than reaped: [`drop_model`] is what removes
/// those, immediately and by name, and a second opinion here would only race
/// it.
pub async fn reap_idle(state: &SharedState) {
    // Beside the reaping, never in its way: a container that lost its entry
    // is adopted by a pass of its own, and judged from the next tick on like
    // any other — reaped once idle, swept under the hold.
    readopt_in_background(state);
    let snap = state.snapshot();
    // The GPU hold comes first and ignores `idle_seconds` entirely (gpu-hold
    // design §5). This tick is what actually drains a held GPU: a model that
    // was busy when the hold engaged has its claim dropped when its request
    // ends, and the next pass stops it. It is also the only reason a model
    // configured `idle_seconds = 0` — "never reap" — is ever stopped by the
    // reaper, which is the intent: under hold, "never idle-unload" is not a
    // promise lmgw can keep and still be off the card.
    if snap.settings.hold.active {
        let swept = hold_sweep(state).await;
        if !swept.stopped.is_empty() {
            // Event-driven frame: nothing else would tell the dashboard the
            // model is gone, because no request ran to trigger one.
            crate::vram::broadcast(state);
        }
    }
    let registry = state.runtime();
    let runtimes = model_runtimes(&snap);
    for view in registry.list() {
        if view.state != RuntimeState::Ready || view.in_flight > 0 {
            continue;
        }
        let Some(rt) = runtimes
            .iter()
            .find(|r| r.class == view.class && r.model_id == view.model_id)
        else {
            continue;
        };
        // 0 = never, the warm-keep default. Audio always reads 0 today: the
        // `audio_models` table has no idle column, so audio.cpp containers are
        // never reaped.
        // TODO(§3.7): give audio models their own `idle_seconds` column —
        // "uniform across engines" is the design's wording, and audio is the
        // one engine that has never had idle unload at all.
        if rt.idle_seconds <= 0 || view.last_used_age_seconds < rt.idle_seconds as u64 {
            continue;
        }
        // Only the container whose idle age was just read (ladder design §12
        // entry 20): a climb or a restart since is a container nobody judged.
        match registry
            .stop_generation(view.class, &view.model_id, view.generation, false)
            .await
        {
            Ok(()) => tracing::info!(
                "idle-stopped {} model '{}' after {}s (idle_seconds = {})",
                view.class,
                view.model_id,
                view.last_used_age_seconds,
                rt.idle_seconds
            ),
            // Traffic arrived between `list()` and the stop, or the container
            // was replaced meanwhile. Nothing to do — the claim is exactly the
            // guard that is supposed to win here, and the next tick asks again.
            Err(RuntimeError::Busy { .. } | RuntimeError::Moved { .. }) => {}
            Err(e) => tracing::warn!("idle reaper: {e}"),
        }
    }
    // Audio containers a download or a delete left on an old `server.json`
    // because they were starting or serving then: stopped once idle.
    crate::runtime::audio::recheck_left(state).await;
}

/// Graceful shutdown (§3.4): stop every managed container before the process
/// goes away, so a tray app that quit does not leave the GPU full.
///
/// `force`, because by this point the owner has already decided: refusing to
/// stop a busy model would leave the container running with nothing left alive
/// to ever stop it. Bounded by [`SHUTDOWN_TIMEOUT`] as a whole; whatever is
/// still recorded when that runs out is named in the log, and boot
/// reconciliation adopts or removes it next time.
pub async fn shutdown(state: &SharedState) {
    // A benchmark run's container is not a registry entry (benchmark design
    // §3.4), so `stop_all` below would leave it on the card.
    crate::bench::runner::abort_for_shutdown(state).await;
    let registry = state.runtime();
    let live = registry.list();
    if live.is_empty() {
        return;
    }
    tracing::info!("stopping {} managed container(s) before exit", live.len());
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, registry.stop_all(true)).await {
        Ok(errors) => {
            for e in errors {
                tracing::warn!("shutdown: {e}");
            }
        }
        Err(_) => {
            let stragglers: Vec<String> = registry
                .list()
                .into_iter()
                .map(|v| v.container_name)
                .collect();
            tracing::warn!(
                "shutdown timed out after {SHUTDOWN_TIMEOUT:?} with {} container(s) still \
                 recorded ({}); boot reconciliation will collect them",
                stragglers.len(),
                stragglers.join(", ")
            );
        }
    }
}

/// A model was deleted or disabled: its container goes now (§3.4).
///
/// Forced, because the row is already gone or off — there is nothing left for
/// a refusal to protect, and leaving a container serving a model the config no
/// longer admits to having is worse than cutting a request short. Removal (not
/// just a stop) because nothing would ever collect the stopped container:
/// `--replace` only fires on a *next start*, and for this model there is none.
///
/// Never fails the caller's operation — the config change has already been
/// written, and a podman that would not cooperate is a warning plus a job for
/// the next boot reconciliation, not a reason to report the delete as failed.
pub async fn drop_model(state: &SharedState, class: Class, model_id: &str) {
    let prefix = state.snapshot().settings.container_prefix.clone();
    if let Err(e) = state
        .runtime()
        .stop_and_remove(&prefix, class, model_id, true)
        .await
    {
        tracing::warn!("removing the container for {class} model '{model_id}': {e}");
    }
}

/// What [`stop_for_apply_outcome`] did to a model whose configuration
/// changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStop {
    /// Nothing was running: the next start renders the new configuration.
    NotRunning,
    /// It was stopped: the next request starts it with the new configuration.
    Stopped,
    /// It is mid-request and keeps running the configuration it started with.
    Busy { in_flight: u32 },
    /// Stopping it failed; it may still be running the old configuration.
    Failed(String),
}

impl ApplyStop {
    /// The sentence the op response appends, or `None` when nothing was
    /// running and there is therefore nothing to say.
    pub fn note(&self, model_id: &str) -> Option<String> {
        match self {
            Self::NotRunning => None,
            Self::Stopped => Some(format!(
                "its container was stopped — the next request starts '{model_id}' with the new \
                 configuration"
            )),
            Self::Busy { in_flight } => Some(format!(
                "'{model_id}' is still serving {in_flight} request(s), so its container keeps \
                 running the previous configuration; stop it to apply the change"
            )),
            Self::Failed(e) => Some(format!("its container could not be stopped: {e}")),
        }
    }

    /// A container is still up on the configuration before the change — what
    /// a load test run right now would be testing.
    pub fn kept_previous(&self) -> bool {
        matches!(self, Self::Busy { .. } | Self::Failed(_))
    }
}

/// A running model's configuration changed (§3.6, apply = recreate *only if
/// running*). Returns the sentence the op response appends, or `None` when
/// nothing was running and there is therefore nothing to say.
///
/// The smallest honest version of apply: stop it, and let the next request pay
/// the cold start with the new argv. Not `force` — a model mid-generation
/// keeps running the configuration it was started with, and the response says
/// so instead of pretending the change took effect. (The `logs`/`apply`
/// surfaces work package formalizes the explicit, overridable version; this is
/// the part that must not wait for it, because otherwise an edited model
/// silently serves stale argv for as long as it stays warm.)
pub async fn stop_for_apply(state: &SharedState, class: Class, model_id: &str) -> Option<String> {
    stop_for_apply_outcome(state, class, model_id)
        .await
        .note(model_id)
}

/// [`stop_for_apply`], saying what happened rather than only the sentence.
pub async fn stop_for_apply_outcome(
    state: &SharedState,
    class: Class,
    model_id: &str,
) -> ApplyStop {
    let registry = state.runtime();
    if !registry.contains(class, model_id) {
        return ApplyStop::NotRunning;
    }
    match registry.stop(class, model_id, false).await {
        Ok(()) => ApplyStop::Stopped,
        Err(RuntimeError::Busy { in_flight, .. }) => ApplyStop::Busy { in_flight },
        Err(e) => ApplyStop::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod apply_stop_tests {
    use super::ApplyStop;

    #[test]
    fn only_a_container_left_on_the_old_configuration_counts_as_kept() {
        assert!(!ApplyStop::NotRunning.kept_previous());
        assert!(!ApplyStop::Stopped.kept_previous());
        assert!(ApplyStop::Busy { in_flight: 1 }.kept_previous());
        assert!(ApplyStop::Failed("podman".into()).kept_previous());
        assert_eq!(ApplyStop::NotRunning.note("m"), None);
        assert!(ApplyStop::Busy { in_flight: 2 }
            .note("m")
            .unwrap()
            .contains("previous configuration"));
    }
}
