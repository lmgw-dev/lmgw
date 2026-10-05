//! The Background warm of one stage (module doc of [`super`]): started only
//! into room there is, never evicting, no claim kept.

use std::time::Instant;

use crate::config::Snapshot;
use crate::runtime::descriptor::{model_runtime, ModelRuntime};
use crate::runtime::lifecycle::acquire_spec;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::{classify, Fit, Target};

use super::load::{self, Joined};
use super::{settle, HeldCause, Loading, Reporter, SkipReason, Warm, WarmOutcome};

/// Warm `warm` in the background, say and log what it came to — at debug
/// when it found nothing to do (module doc of [`super`]).
pub(super) async fn one(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    report: &Reporter,
) -> WarmOutcome {
    let (out, quiet) = match run(state, label, warm, report).await {
        Ok(out) => (out, false),
        Err(Idle(out)) => (out, true),
    };
    settle(label, warm, report, &out, quiet);
    out
}

/// What a Background warm found when there was nothing for it to do: an
/// alias that does not resolve, a disabled row, a voice that cannot speak
/// with a container that is up already. Said like any outcome, logged at
/// debug: a session's every `speech_started` finds it again.
struct Idle(WarmOutcome);

async fn run(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    report: &Reporter,
) -> Result<WarmOutcome, Idle> {
    let alias = warm.alias();
    let snap = state.snapshot();
    // A candidate alias picks per request: nothing to warm ahead of it.
    if snap.candidate_alias(alias).is_some() {
        return Ok(WarmOutcome::Ready { ms: None });
    }
    let route = match snap.resolve(alias) {
        Ok(r) => r,
        Err(e) => {
            let out = WarmOutcome::failed(format!("'{alias}' does not resolve: {e}"));
            return Err(Idle(out));
        }
    };
    // Off this machine, or a local id no row describes: nothing to start.
    let Some(target) = classify(&route) else {
        return Ok(WarmOutcome::Ready { ms: None });
    };
    let Some(runtime) = model_runtime(&snap, target.class, &target.model_id) else {
        return Ok(WarmOutcome::Ready { ms: None });
    };
    let (class, model_id) = (target.class, target.model_id.as_str());
    if !runtime.enabled {
        let out = WarmOutcome::failed(format!("{class} model '{model_id}' is disabled"));
        return Err(Idle(out));
    }
    let up = state.runtime().contains(class, model_id);
    if let Warm::Voice {
        instructions,
        speaks,
        ..
    } = warm
    {
        let voice = speaks.as_ref().and_then(|s| s.voice.as_deref());
        if let Err(e) =
            crate::proxy::synthesize::refuse_route(state, &route, instructions.as_deref(), voice)
                .await
        {
            let out = WarmOutcome::Skipped {
                reason: SkipReason::CannotSpeak,
                message: format!("it cannot speak these answers — {e}"),
                sizes: None,
            };
            // Not up: said once, as the start it keeps from happening.
            return if up { Err(Idle(out)) } else { Ok(out) };
        }
    }
    let mut loading = Loading::default();
    if !up {
        if let Err(out) = start(
            state,
            label,
            warm,
            (&snap, &runtime),
            &target,
            &mut loading,
            report,
        )
        .await
        {
            return Ok(out);
        }
    }
    if class == Class::Audio {
        let joined = load::by_join(state, label, warm, &route, model_id, || {
            loading.mark(report, warm)
        })
        .await;
        match joined {
            Joined::NotNeeded | Joined::Loaded => {}
            Joined::Held(block) => {
                return Ok(held_or_fallback(
                    &snap,
                    alias,
                    HeldCause::of(&block),
                    block.who(),
                ))
            }
            Joined::NoClaim(None) => {
                return Ok(WarmOutcome::failed(
                    "its container went away before it loaded; its first request starts it",
                ))
            }
            Joined::NoClaim(Some(e)) => return Ok(WarmOutcome::refused(&e)),
            Joined::Failed(e) => {
                return Ok(WarmOutcome::failed(format!(
                    "loading it failed ({e}); its first request loads it"
                )))
            }
        }
    }
    Ok(WarmOutcome::Ready { ms: loading.ms() })
}

/// Start the container under the never-evict admission (module doc of
/// [`super`]); `Err` is the stage's outcome when it does not come up.
async fn start(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    (snap, runtime): (&Snapshot, &ModelRuntime),
    target: &Target,
    loading: &mut Loading,
    report: &Reporter,
) -> Result<(), WarmOutcome> {
    let (class, model_id) = (target.class, target.model_id.as_str());
    let alias = warm.alias();
    let permit = match state
        .vram
        .check_background_start(state, snap, class, model_id)
        .await
    {
        Fit::Full(why) => {
            return Err(WarmOutcome::Skipped {
                reason: SkipReason::Full,
                message: why,
                sizes: None,
            })
        }
        Fit::Held(why) => {
            let cause = snap
                .gpu_block_for(class, model_id)
                .map_or(HeldCause::GpuHold, |b| HeldCause::of(&b));
            return Err(held_or_fallback(snap, alias, cause, why));
        }
        Fit::Go(permit) => Some(permit),
        Fit::Unchecked => None,
    };
    loading.mark(report, warm);
    let started_at = Instant::now();
    let spec = acquire_spec(state, snap, runtime);
    let started = state.runtime().acquire(&spec).await;
    drop(permit);
    match started {
        // Dropped at once on purpose: resident, not claimed (module doc).
        Ok(guard) => {
            tracing::info!(
                "{label}: started '{alias}' ({class} '{model_id}') on {} in {} ms",
                guard.port(),
                started_at.elapsed().as_millis()
            );
            state.vram.cache_pids(state);
            Ok(())
        }
        Err(e) => Err(WarmOutcome::failed(format!(
            "starting {class} '{model_id}' failed: {e}"
        ))),
    }
}

/// A stage the GPU hold or a benchmark keeps from starting: `fallback` when
/// a request for `alias` would be answered by its fallback now, else
/// `held`.
fn held_or_fallback(snap: &Snapshot, alias: &str, cause: HeldCause, why: String) -> WarmOutcome {
    match snap.resolve_for_request(alias) {
        Ok(r) => match r.fallback {
            Some(answered_by) => WarmOutcome::Fallback { answered_by },
            None => WarmOutcome::Held {
                cause,
                message: why,
            },
        },
        Err(e) => WarmOutcome::refused(&e),
    }
}
