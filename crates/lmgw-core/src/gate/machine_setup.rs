//! A local model this computer cannot start (machine-check design §2.5,
//! D23, WP5): what the gate does when a start fails because the graphics
//! card, its driver or what lets models use it is not ready, or because the
//! engine image the model runs in could not be downloaded (§5, D36).
//!
//! - **At admission** ([`hand_on`]): admission answered
//!   [`GatewayError::GpuSetup`] (the start failed and the machine check run
//!   again right after it names the cause, `vram::start_error`) or
//!   [`GatewayError::EngineDownload`]. Nothing has reached the client. A
//!   configured fallback always applies, as for every other reason: the
//!   hop's own, or a candidate alias's, along its chain, stamped
//!   `x-lmgw-fallback-reason: gpu_setup` or `engine_download`. Without one
//!   that can answer, the error, with a clause saying why a configured one
//!   did not.
//! - **At resolve** ([`stop`]): while the last machine check names such a
//!   cause for a hop on the graphics card that has a fallback, and the
//!   machine is still as that check read it
//!   ([`crate::machine::problem_at_resolve`]), the hop goes on to its
//!   fallback like under the hold ([`super::chain::stop_at`]), so no start
//!   that is bound to fail is tried first. Unlike the hold's, this stop lets
//!   its hop be tried when its fallback cannot answer
//!   ([`super::chain::Stop::tries_hop`]): the verdict can be out of date
//!   (fixed in a terminal a minute ago), and a start that fails checks
//!   again.

use super::candidate::CandidateCtx;
use super::chain::{self, Next, Stop, Walk};
use super::open::{FallbackReason, GateHeaders, OpenFailed, Opened, RouteCheck};
use crate::config::{FallbackRoute, HopKey, Route, Snapshot};
use crate::error::GatewayError;
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::Target;

/// The reason a fallback answers `error` for, when it is one this module
/// hands on: `None` for every other error.
pub(super) fn reason_of(error: &GatewayError) -> Option<FallbackReason> {
    match error {
        GatewayError::GpuSetup { .. } => Some(FallbackReason::GpuSetup),
        GatewayError::EngineDownload { .. } => Some(FallbackReason::EngineDownload),
        _ => None,
    }
}

/// Whether admission's `error` goes to the fallback ([`hand_on`]).
pub(super) fn hands_on(error: &GatewayError) -> bool {
    reason_of(error).is_some()
}

/// The admission that failed: whose start, on which route, for which hop.
pub(super) struct Failed<'a> {
    pub target: &'a Target,
    pub route: Route,
    /// The headers the request carried to this hop: its chain so far, and a
    /// candidate alias's `x-lmgw-candidate`, kept on the error when no
    /// fallback answers.
    pub headers: GateHeaders,
    /// The hop's name: the model asked for, a hop of its chain, or the
    /// candidate alias whose primary it is.
    pub alias: &'a str,
    pub check: RouteCheck,
    pub origin: Origin,
    /// The candidate alias the model is the primary of: its own fallback
    /// answers, never the row's (candidate-aliases §4.1).
    pub candidate: Option<&'a CandidateCtx>,
}

/// Admission said `error` ([`reason_of`]): the hop's fallback answers along
/// its chain — a cloud one at once, a local one admitted as a request
/// naming it would be — else `error`, with a clause saying why the
/// configured fallback did not answer and the chain tried.
pub(super) async fn hand_on(
    state: &SharedState,
    f: Failed<'_>,
    error: GatewayError,
) -> Result<Opened, OpenFailed> {
    let Some(reason) = reason_of(&error) else {
        return Err(OpenFailed {
            route: Some(Box::new(f.route)),
            headers: f.headers,
            error,
        });
    };
    let snap = state.snapshot();
    let (key, next) = match f.candidate {
        Some(c) => (
            HopKey::Candidate(c.row.id),
            snap.fallback_named(c.alias_row(&snap).fallback.as_deref()),
        ),
        None => {
            let key = HopKey::Local(f.target.class, f.target.model_id.clone());
            let next = snap.next_hop(&key);
            (key, next)
        }
    };
    let chain = f.headers.chain().clone();
    let mut tried = chain.clone();
    tried.leave(f.alias, key.clone());
    let walk = Walk {
        check: f.check,
        origin: f.origin,
    };
    match chain::fall_back(state, walk, chain, (f.alias, key), next, reason).await {
        Next::Opened(opened) => {
            tracing::info!(
                alias = %f.alias,
                "'{}': {error} — its fallback '{}' answers ({})",
                f.alias,
                opened.headers.fallback().unwrap_or_default(),
                reason.as_str()
            );
            Ok(opened)
        }
        Next::Failed(failed) => Err(failed),
        Next::Ended(clause) => Err(OpenFailed {
            route: Some(Box::new(f.route)),
            headers: f.headers,
            error: error.with_clause(&format!("{clause}{}", tried.tried())),
        }),
    }
}

/// The resolve-time stop of the GPU hop `target` (module doc): only with a
/// fallback to go on to — without one the start is tried, and a failure is
/// blamed again — and only while the last machine check names a cause this
/// row's start meets, one the swap can tell is still so, and the machine is
/// as that check read it (D95). Never for a pinned caller
/// ([`super::chain::stop_at`], D97).
pub(super) fn stop(state: &SharedState, snap: &Snapshot, target: &Target) -> Option<Stop> {
    let key = HopKey::Local(target.class, target.model_id.clone());
    snap.fallback_of(&key)?;
    let problem = crate::machine::problem_at_resolve(state, snap, target)?;
    Some(Stop::gpu_setup(problem, key))
}

/// The fallback that answers for the GPU row `target` when this computer
/// cannot start it, for a caller that only says so (a warm, which never
/// starts a fallback's model, D98): the row's own usable fallback, by name.
pub(crate) fn gpu_setup_fallback(snap: &Snapshot, target: &Target) -> Option<String> {
    match snap.next_hop(&HopKey::Local(target.class, target.model_id.clone())) {
        FallbackRoute::Usable { alias, .. } => Some(alias),
        FallbackRoute::None | FallbackRoute::Unusable { .. } => None,
    }
}
