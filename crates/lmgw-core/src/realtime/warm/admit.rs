//! The Admit warm (chat-voice design §4.2): an explicit press warms through
//! the request admission, as one group.
//!
//! - **One stage** is resolved and admitted exactly as the request it
//!   announces would be (`gate::resolve(…).admit()`): the GPU hold's swap
//!   (a fallback answers — a warm never starts it — or `gpu_hold`), the
//!   outside-VRAM verdict, eviction of idle models, the queue, the start.
//!   The benchmark lease and `gpu_block` on the placement of the descriptor
//!   started are admission's own checks; a CPU audio row claims no VRAM and
//!   is admitted under the hold. No container is started any other way.
//!   Once it is up, a lazy audio row is loaded on the admission's own claim
//!   ([`super::load::send`]).
//! - **A group** — two or more distinct local GPU models — is checked first
//!   ([`does_not_fit`]): the footprints admission charges
//!   (`VramScheduler::footprint`, `realtime_budget`'s sizing) plus the
//!   headroom, against what lmgw may use (`vram.budget_mb`, or the devices'
//!   total) less what is held outside lmgw on the card now
//!   (`VramScheduler::group_capacity`). If it fits, every stage is admitted
//!   side by side and **every claim is kept until all of them are up**: a
//!   sibling's admission cannot evict a stage that has just loaded, since a
//!   claimed model is never a victim. If it does not, each of those stages
//!   says `skipped: does_not_fit` with the sizes, and the group is warmed in
//!   Background instead — into room there is, evicting nothing.
//! - **A stage never waits on its own group** ([`super::kept`]). The check
//!   works on estimates, which are lower bounds, and on the card as it was:
//!   a stage still waiting for admission that cannot get room beside the
//!   claims its siblings keep gives up as `skipped: does_not_fit`, so it
//!   never holds the admission gate on them.
//! - **The press can end first** (the caller's [`Reporter`] is gone: key
//!   released, Esc, thread left; a bound session's socket closed). A stage
//!   still waiting for admission drops its wait — its place in the queue,
//!   its turn at the gate. A start already in flight is not abandoned (the
//!   registry's start is not dropped half-way): it finishes, and its claim is
//!   let go without a load.

use crate::config::Snapshot;
use crate::gate::RouteCheck;
use crate::hf::fmt_bytes;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::{classify, LocalHold, Target};

use super::kept::{let_go, Kept};
use super::{
    background, load, settle, GroupSizes, Loading, Reporter, SkipReason, Warm, WarmOutcome,
};

/// Admit `models` as one group (module doc); the outcomes in their order.
pub(super) async fn group(
    state: &SharedState,
    label: &str,
    models: &[Warm],
    report: &Reporter,
) -> Vec<WarmOutcome> {
    if let Some((sizes, message)) = does_not_fit(state, models).await {
        let snap = state.snapshot();
        for m in models
            .iter()
            .filter(|m| gpu_target(&snap, m.alias()).is_some())
        {
            settle(
                label,
                m,
                report,
                &WarmOutcome::Skipped {
                    reason: SkipReason::DoesNotFit,
                    message: message.clone(),
                    sizes: Some(sizes),
                },
                false,
            );
        }
        let each = models
            .iter()
            .map(|m| background::one(state, label, m, report));
        return futures::future::join_all(each).await;
    }
    let kept = Kept::new();
    let each = models.iter().map(|m| one(state, label, m, report, &kept));
    let (outcomes, claims): (Vec<_>, Vec<_>) =
        futures::future::join_all(each).await.into_iter().unzip();
    // Every stage is up (or settled otherwise): only now are the claims let
    // go, so none could be evicted by a sibling's admission.
    drop(claims);
    outcomes
}

/// Admit one stage, say and log what it came to; its claim, when it holds
/// one, for the group to keep.
async fn one(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    report: &Reporter,
    kept: &Kept,
) -> (WarmOutcome, Option<LocalHold>) {
    let (out, claim) = run(state, label, warm, report, kept).await;
    if let Some(hold) = &claim {
        kept.keep(Target {
            class: hold.class(),
            model_id: hold.model_id().to_string(),
        });
    }
    settle(label, warm, report, &out, false);
    (out, claim)
}

async fn run(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    report: &Reporter,
    kept: &Kept,
) -> (WarmOutcome, Option<LocalHold>) {
    let alias = warm.alias();
    // As the request it announces is checked: the Chat's own turn for the
    // chat model, the audio routes' check for a speech model.
    let check = match warm.stage() {
        "chat" => RouteCheck::None,
        _ => RouteCheck::Audio,
    };
    let routed = match crate::gate::resolve(state, alias, check).await {
        Ok(r) => r,
        Err(f) => return (WarmOutcome::refused(&f.error), None),
    };
    if let Some(answered_by) = routed.headers().fallback() {
        let answered_by = answered_by.to_string();
        return (WarmOutcome::Fallback { answered_by }, None);
    }
    if let Warm::Voice {
        instructions,
        speaks,
        ..
    } = warm
    {
        let refused = crate::proxy::synthesize::refuse_route(
            state,
            routed.resolved(),
            instructions.as_deref(),
            speaks.as_ref().and_then(|s| s.voice.as_deref()),
        )
        .await;
        if let Err(e) = refused {
            let out = WarmOutcome::Skipped {
                reason: SkipReason::CannotSpeak,
                message: format!("it cannot speak these answers — {e}"),
                sizes: None,
            };
            return (out, None);
        }
    }
    let target = classify(routed.resolved());
    let mut loading = Loading::default();
    if target
        .as_ref()
        .is_some_and(|t| state.runtime().ready_port(t.class, &t.model_id).is_none())
    {
        loading.mark(report, warm);
    }
    let admit = routed.admit(state);
    tokio::pin!(admit);
    // The press ended (`None`), or the stage cannot get room beside its
    // siblings' kept claims: the wait goes, unless that would leave
    // something half-way (`kept::let_go`).
    let ended = tokio::select! {
        biased;
        a = &mut admit => Ok(a),
        () = report.gone() => Err(None),
        out = kept.crowded_out(state, target.as_ref()) => Err(Some(out)),
    };
    let admitted = match ended {
        Ok(a) => a,
        Err(why) => match let_go(state, target.as_ref(), admit.as_mut()).await {
            Some(a) => a,
            None => return (why.unwrap_or(WarmOutcome::Aborted), None),
        },
    };
    let opened = match admitted {
        Ok(o) => o,
        Err(f) => return (WarmOutcome::refused(&f.error), None),
    };
    // The outside-VRAM verdict sent it to its fallback: nothing started.
    if let Some(answered_by) = opened.headers.fallback() {
        let answered_by = answered_by.to_string();
        return (WarmOutcome::Fallback { answered_by }, None);
    }
    let Some(hold) = opened.hold else {
        return (WarmOutcome::Ready { ms: loading.ms() }, None);
    };
    if hold.class() == Class::Audio {
        let snap = state.snapshot();
        if let Some((row, job)) = load::job(&snap, warm, hold.model_id()) {
            if !load::loaded(state, &snap, row, hold.generation()) {
                // Nobody is waiting on it any more: up, not loaded.
                if report.is_gone() {
                    let out = WarmOutcome::UpNotLoaded { ms: loading.ms() };
                    return (out, Some(hold));
                }
                loading.mark(report, warm);
                if let Err(e) = load::send(state, label, alias, &job, &opened.route, &hold).await {
                    let out = WarmOutcome::failed(format!(
                        "it is up, but loading it failed ({e}); its first request loads it"
                    ));
                    return (out, Some(hold));
                }
            }
        }
    }
    (WarmOutcome::Ready { ms: loading.ms() }, Some(hold))
}

/// The local GPU model `alias` would be started as: a candidate alias's
/// primary, or the row its route names unless that row runs on the CPU.
fn gpu_target(snap: &Snapshot, alias: &str) -> Option<Target> {
    if let Some(ca) = snap.candidate_alias(alias) {
        return ca.candidates.first().map(|primary| Target {
            class: Class::Chat,
            model_id: primary.clone(),
        });
    }
    let target = classify(&snap.resolve(alias).ok()?)?;
    snap.placement(target.class, &target.model_id)
        .is_gpu()
        .then_some(target)
}

/// The group's sizes and the sentence that says it, when its distinct local
/// GPU models do not fit in what lmgw may use together beside what is held
/// outside lmgw on the card now (module doc; `realtime_budget`'s
/// `too_large` and `tight` verdicts in one). `None` — no group decision, so
/// every stage is admitted — for fewer than two such models, under the GPU
/// hold or a benchmark (nothing local starts on the card then), with
/// admission switched off, with no capacity to measure against, or when a
/// model has no figure (nothing is guessed). The memory outside lmgw is
/// only subtracted when it can be told.
async fn does_not_fit(state: &SharedState, models: &[Warm]) -> Option<(GroupSizes, String)> {
    let snap = state.snapshot();
    if snap.gpu_block().is_some() || !snap.settings.vram.enabled {
        return None;
    }
    let mut seen: Vec<Target> = Vec::new();
    let mut parts = Vec::new();
    let mut total = 0u64;
    for m in models {
        let Some(t) = gpu_target(&snap, m.alias()) else {
            continue;
        };
        if seen.contains(&t) {
            continue;
        }
        let Some(fp) = state.vram.footprint(&snap, t.class, &t.model_id).await else {
            tracing::debug!(
                "warm group: no figure for {}/{}, so no group decision",
                t.class.as_str(),
                t.model_id
            );
            return None;
        };
        total = total.saturating_add(fp.total_bytes);
        parts.push(format!(
            "{}/{} {}",
            t.class.as_str(),
            t.model_id,
            fmt_bytes(fp.total_bytes)
        ));
        seen.push(t);
    }
    if seen.len() < 2 {
        return None;
    }
    let room = state.vram.group_capacity(state, &snap).await?;
    let outside = room.outside.unwrap_or(0);
    let usable = room.capacity.saturating_sub(outside);
    let needed = total.saturating_add(room.headroom);
    if needed <= usable {
        return None;
    }
    let beside = if outside > 0 {
        format!(", {} of it held outside lmgw now", fmt_bytes(outside))
    } else {
        String::new()
    };
    let message = format!(
        "these models do not fit on the GPU together: {} need {} with {} headroom, and lmgw \
         may use {}{beside} — each is warmed only into room there is, evicting nothing, so \
         they cannot evict each other",
        parts.join(" + "),
        fmt_bytes(needed),
        fmt_bytes(room.headroom),
        fmt_bytes(room.capacity)
    );
    let sizes = GroupSizes {
        needed_bytes: needed,
        capacity_bytes: usable,
    };
    Some((sizes, message))
}
