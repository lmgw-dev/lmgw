//! A plain local model's **guest** (per-request background design §4): a
//! request sent with `x-lmgw-background: on` for a model it names directly,
//! admitted the way a background candidate alias whose only candidate is that
//! model would be (candidate-aliases design §4.3, `Walk::guest` without the
//! alternates), with the model row's own fallback in place of an alias
//! fallback.
//!
//! [`admit`], in order:
//! - **Nothing to defer:** a cloud route, a row on the CPU, a local id no row
//!   describes — admitted exactly as today (§6). The GPU hold and a
//!   benchmark's lease were answered at resolve (§4 step 1).
//! - **[`vram::start_background`]:** join the model if it is loaded (ready,
//!   starting, climbing) and not draining for the owner; else start it into
//!   free VRAM or VRAM idle guests hold, never evicting the owner's models and
//!   never waiting in the admission queue. The hold is the guest's
//!   ([`crate::runtime::registry::Origin::Background`]) and restarts by the
//!   guest's rule.
//! - **Blocked:** the row's fallback along its chain (openai egress design
//!   §14.3: a local fallback is a guest there too), judged by the site's
//!   check and stamped `x-lmgw-fallback-reason: background`; else `503
//!   gpu_deferred`, saying what is in the way.
//! - **The hold that came on after resolve** (or a benchmark's lease) is
//!   answered as the owner's request meeting it is ([`held`], *changed
//!   2026-10-22*, §11.9): the row's fallback with reason `hold` /
//!   `benchmark` when it can take the request, else `gpu_hold` /
//!   `gpu_benchmark` — a configured fallback always applies.
//! - **Errors as they are:** `vram_too_large` is the configuration error it
//!   is (candidate-aliases §4.7), never a deferral and never the fallback; so
//!   is a start that fails — unless the machine check run again after it
//!   blames the computer (`gpu_setup`, machine-check design §2.5) or the
//!   engine image could not be downloaded (`engine_download`, §5): the
//!   row's fallback answers those, as for an owner's request (WP5).
//!
//! [`lost`] answers the same way once the request holds its model and the
//! send finds it cannot go on without disturbing the owner: a climb the guest
//! may not make (§4 step 6), or the model gone under the request and not
//! restarted for it. A prompt above the model's context is not among them:
//! with no alternates it is the plain `400` (§4 step 7).

use crate::config::{HopKey, Route};
use crate::error::{Deferred, GatewayError};
use crate::gate::chain::{self, Chain, Next, Walk};
use crate::gate::open::{
    admit_route, held, AdmissionPolicy, AtAdmission, FallbackReason, GateHeaders, OpenFailed,
    Opened, RouteCheck,
};
use crate::runtime::descriptor::model_runtime;
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::{self, BackgroundStart, LocalHold, Target};

/// Admission for a plain local model's guest (module doc) — the model the
/// request named, or a local hop of its chain (`alias` its name, `headers`
/// the chain so far).
pub(super) async fn admit(
    state: &SharedState,
    mut route: Route,
    headers: GateHeaders,
    alias: &str,
    check: RouteCheck,
    at: AtAdmission,
) -> Result<Opened, OpenFailed> {
    let Some(target) = on_the_gpu(state, &route) else {
        return admit_route(state, route, headers, alias, check, at, None).await;
    };
    match vram::start_background(state, &route, alias).await {
        Ok(BackgroundStart::Started(hold)) => {
            let mut hold = *hold;
            hold.point(&mut route);
            if at == AtAdmission::MayFallBack {
                hold.set_policy(AdmissionPolicy {
                    check,
                    candidate: None,
                    chain: headers.chain().clone(),
                });
            }
            Ok(Opened {
                route,
                hold: Some(hold),
                headers,
            })
        }
        // A pinned caller never falls back: the deferral is its answer.
        Ok(BackgroundStart::Blocked(why)) => {
            defer(
                state,
                &target,
                Some(route),
                alias,
                (at == AtAdmission::MayFallBack).then_some(check),
                format!("GPU in use by {why}"),
                headers.chain().clone(),
            )
            .await
        }
        Err(error @ (GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }))
            if at == AtAdmission::MayFallBack =>
        {
            held(
                state,
                &target,
                route,
                alias,
                check,
                error,
                headers.chain().clone(),
                Origin::Background,
            )
            .await
        }
        // A start this computer could not make (machine-check design
        // §2.5) or whose engine image could not be downloaded (§5): the
        // row's fallback answers, a guest along its chain (WP5).
        Err(error) if at == AtAdmission::MayFallBack && super::machine_setup::hands_on(&error) => {
            super::machine_setup::hand_on(
                state,
                super::machine_setup::Failed {
                    target: &target,
                    route,
                    headers,
                    alias,
                    check,
                    origin: Origin::Background,
                    candidate: None,
                },
                error,
            )
            .await
        }
        Err(error) => Err(OpenFailed {
            route: Some(Box::new(route)),
            headers,
            error,
        }),
    }
}

/// A send on a plain model's guest `hold` found the model cannot take the
/// request without disturbing the owner (`cause`, module doc): its row's
/// fallback answers instead, or the deferral. Nothing was sent anywhere yet.
/// Any other `cause` is the answer as it is.
pub(crate) async fn lost(
    state: &SharedState,
    hold: &LocalHold,
    check: RouteCheck,
    cause: GatewayError,
) -> Result<Opened, OpenFailed> {
    let detail = match &cause {
        GatewayError::GpuDeferred { detail, .. } => detail.clone(),
        GatewayError::CandidateLost { detail, .. } => {
            format!(
                "'{}' went away under this request: {detail}",
                hold.model_id()
            )
        }
        _ => {
            return Err(OpenFailed {
                route: None,
                headers: GateHeaders::default(),
                error: cause,
            })
        }
    };
    let target = Target {
        class: hold.class(),
        model_id: hold.model_id().to_string(),
    };
    let chain = hold.policy().map(|p| p.chain.clone()).unwrap_or_default();
    defer(
        state,
        &target,
        None,
        hold.alias(),
        Some(check),
        detail,
        chain,
    )
    .await
}

/// The model `route` names, when a guest's admission applies to it: a local
/// model with a row, placed on the GPU. Anything else has no owner to
/// disturb (§6).
fn on_the_gpu(state: &SharedState, route: &Route) -> Option<Target> {
    let target = vram::classify(route)?;
    let snap = state.snapshot();
    let gpu = snap.placement(target.class, &target.model_id).is_gpu();
    (gpu && model_runtime(&snap, target.class, &target.model_id).is_some()).then_some(target)
}

/// Nothing local takes the guest's request now (`detail`, to read after
/// "deferred: "): the row's fallback along its chain when `check` is given
/// (a caller that may fall back) and it can take the request — a local
/// fallback is a guest there in turn — else `503 gpu_deferred`, with a
/// clause saying why a configured fallback did not answer.
#[allow(clippy::too_many_arguments)]
async fn defer(
    state: &SharedState,
    target: &Target,
    route: Option<Route>,
    alias: &str,
    check: Option<RouteCheck>,
    detail: String,
    chain: Chain,
) -> Result<Opened, OpenFailed> {
    let key = HopKey::Local(target.class, target.model_id.clone());
    let mut tried = chain.clone();
    tried.leave(alias, key.clone());
    let not_used = match check {
        None => String::new(),
        Some(check) => {
            let walk = Walk {
                check,
                origin: Origin::Background,
            };
            let next = state.snapshot().next_hop(&key);
            match chain::fall_back(
                state,
                walk,
                chain,
                (alias, key),
                next,
                FallbackReason::Background,
            )
            .await
            {
                Next::Opened(opened) => {
                    tracing::info!(
                        alias = %alias,
                        "background traffic for '{alias}': deferred ({detail}) — its fallback \
                         '{}' answers",
                        opened.headers.fallback().unwrap_or_default()
                    );
                    return Ok(opened);
                }
                Next::Failed(f) => return Err(f),
                Next::Ended(clause) => clause,
            }
        }
    };
    Err(OpenFailed {
        route: route.map(Box::new),
        headers: GateHeaders::along(tried.clone()),
        error: GatewayError::GpuDeferred {
            model: alias.to_string(),
            detail: format!("{detail}{not_used}{}", tried.tried()),
            on: Deferred::Model,
        },
    })
}
