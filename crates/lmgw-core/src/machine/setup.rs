//! What a model start learns from the machine check (machine-check design
//! §2.5, §2.6, D23, WP5).
//!
//! - **After a start on the graphics card failed** ([`after_failed_start`],
//!   D96): when the machine is as the last check read it (lmgw-machine's
//!   [`Current`], and how the last write of lmgw's spec came out), that
//!   check's verdict stands; otherwise §1.1 (the card and its driver) and
//!   §1.2 (what lets models use an NVIDIA card) are read again alone — one
//!   read at a time, joined by every start that fails meanwhile, NVML left
//!   out when the start's own write ran out of time on the card — and the
//!   full check follows in the background, so the start's answer never
//!   waits for Podman, the package metadata or a registry. When an item
//!   that this row uses is a problem ([`super::blame`], D64), that is the
//!   start's answer (`gpu_setup`). A row on the processor, a test state's
//!   blank machine and a failure that is not the container's start are not
//!   checked.
//! - **At resolve** ([`problem_at_resolve`], D66, D95): the last check's
//!   verdict for a GPU row that is not up, taken only for a cause a change
//!   to [`Current`] would clear and only while the machine is as that check
//!   read it (the driver's two halves, the NVML library file,
//!   `nvidia-ctk`, the rpm database's last change, the `/dev/nvidia*` nodes
//!   and the boot — file reads, no program). A machine that changed since
//!   (a driver or the toolkit installed in a terminal, a restart), or a
//!   cause it cannot tell is still so, gets its start tried, and a start
//!   that fails is blamed again.
//!   Such a start is stamped on the check ([`super::MachineChecks::failed_start`],
//!   D182): the last result and every later one say which model's start
//!   failed and when, and the live feed hears of it.
//! - **After a start on the graphics card succeeded** ([`after_start`])
//!   while the last check said this row could not start: that check is out
//!   of date, and a fresh one runs in the background, so the status, the
//!   tray and the next resolve read the machine as it is.

use lmgw_machine::cdi::own::Current;

use super::blame::Blamed;
use super::fit::{device, Device};
use crate::config::Snapshot;
use crate::runtime::descriptor::{model_runtime, ModelRuntime};
use crate::runtime::registry::RuntimeError;
use crate::state::SharedState;
use crate::vram::Target;

/// What `runtime`'s run arguments give its container: the NVIDIA card, AMD's
/// compute node, the render nodes alone, another CDI kind, or nothing.
fn row_device(runtime: &ModelRuntime) -> Device {
    device(&runtime.extra_run_args)
}

/// A start of `runtime`'s container failed with `e` (module doc): what on
/// the computer this row uses is a problem, if anything.
pub async fn after_failed_start(
    state: &SharedState,
    runtime: &ModelRuntime,
    e: &RuntimeError,
) -> Option<Blamed> {
    let m = &state.machine;
    if !matches!(e, RuntimeError::Start { .. }) || !runtime.placement().is_gpu() || !m.stated() {
        return None;
    }
    let (class, model) = (runtime.class.as_str(), runtime.model_id.as_str());
    let as_read = m
        .described()
        .filter(|d| d.spec_err == super::spec_err(state) && *d.read == Current::read(&*m.facts()));
    let blame = match as_read {
        Some(d) => {
            tracing::info!(
                "{class} model '{model}' did not start on the graphics card; the machine is as \
                 the last check read it, so its verdict stands"
            );
            d.blame
        }
        None => {
            tracing::info!(
                "{class} model '{model}' did not start on the graphics card, and the machine \
                 changed since the last check: reading the graphics card and its driver again"
            );
            m.blames.read(state).await?
        }
    };
    let blamed = blame.problem(&row_device(runtime));
    match &blamed {
        Some(b) => {
            tracing::warn!(
                "{class} model '{model}': the machine check says why its start failed: '{model}' \
                 {}",
                b.words()
            );
            // Whoever asked for the start, the check carries it now, so a
            // "Not now" said before it ends (D182).
            if let Some(summary) = m.failed_start(class, model) {
                state.telemetry.machine(summary);
            }
        }
        None => tracing::info!(
            "{class} model '{model}': the machine check found nothing on this computer that \
             keeps it from starting; its start's own error stands"
        ),
    }
    blamed
}

/// The problem the last check names for a start of `target` (module doc),
/// as `gpu_setup` words it after the model's name; `None` for a row on the
/// processor, one that is up, no check yet, a test state's blank machine, a
/// cause the swap cannot tell is still so, and a machine that changed since.
pub fn problem_at_resolve(state: &SharedState, snap: &Snapshot, target: &Target) -> Option<String> {
    let m = &state.machine;
    if !m.stated() {
        return None;
    }
    let d = m.described()?;
    // Nothing wrong with the card for any row: the common case, decided
    // before anything else is looked at.
    if !d.blame.any() {
        return None;
    }
    let runtime = model_runtime(snap, target.class, &target.model_id)?;
    if !runtime.placement().is_gpu() || state.vram.is_up(state, target) {
        return None;
    }
    let blamed = d.blame.problem(&row_device(&runtime))?;
    let (class, id) = (target.class.as_str(), target.model_id.as_str());
    if !blamed.swaps() {
        tracing::debug!(
            "{class}/{id}: the last machine check says it cannot start ({}), for a cause the \
             swap cannot tell is still so — trying the start",
            blamed.words()
        );
        return None;
    }
    if Current::read(&*m.facts()) != *d.read {
        tracing::info!(
            "{class}/{id}: the last machine check said it {}, and the machine has changed since \
             — trying the start, which is blamed again if it fails",
            blamed.words()
        );
        return None;
    }
    Some(blamed.words())
}

/// `runtime`'s container came up on the graphics card (module doc): when
/// the last check said it could not, a fresh check, in the background.
pub fn after_start(state: &SharedState, runtime: &ModelRuntime) {
    let m = &state.machine;
    if !m.stated() || !runtime.placement().is_gpu() {
        return;
    }
    let Some(d) = m.described() else {
        return;
    };
    let Some(blamed) = d.blame.problem(&row_device(runtime)) else {
        return;
    };
    tracing::info!(
        "{} model '{}' started on the graphics card although the last machine check said it {}: \
         checking the machine again",
        runtime.class.as_str(),
        runtime.model_id,
        blamed.words()
    );
    super::check_in_background(state);
}
