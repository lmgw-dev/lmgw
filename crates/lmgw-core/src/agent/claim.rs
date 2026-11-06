//! A tool loop's GPU claim (§9b), let go while the loop waits on work that
//! may need the room it pins, and taken again by its next model call.
//!
//! A loop — the Chat's, a `/v1/responses` run's, an agent run's — holds one
//! claim for its whole run (`vram::LocalHold`: no eviction between its model
//! calls). Two kinds of wait between model calls may need exactly the room
//! that claim pins:
//! - a heard Chat turn's first tool call waits for the turn's user row, and
//!   the row waits for the turn's transcript — an ASR call
//!   (voice-audio-input design §3.4, `web::agentchat::heard`);
//! - an image or audio tool call whose route lands on the GPU
//!   (chat-media-tools design §4.3, [`media`]).
//!
//! Holding the claim through either is a circular wait: with
//! `vram.queue_timeout_seconds` 0 the loop hangs for good, held and silent;
//! with a timeout the other call fails. So the claim is let go for the wait
//! ([`LoopClaim::let_go`]), and the next model call takes it again first
//! ([`LoopClaim::for_call`]) through admission on the same model
//! (`vram::LetGo::regain`). A call that fits beside the chat model evicts
//! nothing, and the regain then joins the model still up.
//!
//! A loop's own calls are sequential — a model call, then its tools — so the
//! claim is never let go under one of its model calls in flight. A loop that
//! shares one claim between concurrent model calls (an agent run's classify
//! stage) is safe too: letting go waits for every model call that holds the
//! claim to end, and a model call that starts meanwhile queues behind it.
//! A loop whose tools may not run, and is stopped, makes no further call and
//! takes nothing again.
//!
//! **A regain refused** goes where any request goes ([`LoopClaim::call_on`]),
//! through the gate's own resolve and admission ([`reroute`]):
//! - a GPU block switched on meanwhile (the GPU hold, a benchmark run)
//!   refuses the regain as it refuses any start: the call goes to the
//!   configured fallback (review V12, the owner's ruling of 2026-10-06:
//!   configured fallbacks always apply), or gets the block's refusal;
//! - a candidate alias's model that was evicted meanwhile and that the
//!   alias may not bring back (`GatewayError::CandidateLost`, a `No` or
//!   `Background` restart rule) is picked again, as the gate answers a lost
//!   candidate at a send (candidate-aliases design §12 entry 45).
//!
//! The re-routed call holds its own claim for itself alone; the loop's claim
//! stays let go, so its next call tries the regain again.
//!
//! **A regain that loads the model again** — it was evicted while the claim
//! was let go, a picture's image row took its room — is told to the loop's
//! [`RegainReport`] (chat-media-tools design §5.3): the Chat says it with the
//! `state` frames its turn's first admission sends.

use std::sync::Arc;

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::config::Route;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::gate::{FallbackReason, OpenFailed, Opened, RouteCheck};
use crate::ir::ChatRequest;
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::{LetGo, LocalHold};

pub(crate) mod media;
pub(crate) mod refused;

/// The loop's claim (module doc).
pub struct LoopClaim {
    slot: RwLock<Slot>,
    /// Told when a regain has to load the model again (module doc).
    report: Option<Arc<dyn RegainReport>>,
}

/// What a loop says about a regain that loads its model again (module doc).
pub(crate) trait RegainReport: Send + Sync {
    /// The model `alias` is being loaded again.
    fn loading<'a>(&'a self, alias: &'a str) -> Erased<'a, ()>;

    /// The load ended: `Ok` with how long it took, or the regain's refusal.
    fn settled<'a>(
        &'a self,
        alias: &'a str,
        outcome: Result<u64, &'a GatewayError>,
    ) -> Erased<'a, ()>;
}

struct Slot {
    /// The claim: `None` for a route with no local container, or while let
    /// go.
    hold: Option<LocalHold>,
    /// The claim let go, which the next model call takes again.
    let_go: Option<LetGo>,
}

/// What one model call of the loop goes out on ([`LoopClaim::call_on`]).
pub(crate) enum CallOn<'a> {
    /// The loop's own route, on its claim.
    Claim(RwLockReadGuard<'a, Option<LocalHold>>),
    /// Where the gate sent this call instead (module doc).
    Rerouted(Box<Opened>),
}

/// A call the regain and the gate's re-route both refused.
pub(crate) struct Refused {
    /// The route the gate's refusal names; `None` for the loop's own.
    pub route: Option<Box<Route>>,
    /// The refusal is the gate's re-route's ([`reroute`]), not the regain's:
    /// [`Self::fallback`] is its, not the loop's.
    pub gate: bool,
    pub fallback: Option<FallbackReason>,
    pub error: GatewayError,
}

impl LoopClaim {
    /// The claim admission gave the loop.
    pub fn new(hold: Option<LocalHold>) -> Self {
        Self {
            slot: RwLock::new(Slot { hold, let_go: None }),
            report: None,
        }
    }

    /// Tell `report` of every regain that loads the model again.
    pub(crate) fn reporting(mut self, report: Arc<dyn RegainReport>) -> Self {
        self.report = Some(report);
        self
    }

    /// Let the claim go until the next model call (module doc). Waits for
    /// the model calls holding it to end.
    pub async fn let_go(&self) {
        let mut slot = self.slot.write().await;
        if let Some(hold) = slot.hold.take() {
            slot.let_go = Some(hold.let_go());
        }
    }

    /// The claim a model call sends on, taken again first when it was let
    /// go. An error is the call's: admission's refusal of the regain.
    ///
    /// Read first: a claim nobody let go is shared by every model call at
    /// once, as it was before it could be let go.
    pub async fn for_call(&self) -> Result<RwLockReadGuard<'_, Option<LocalHold>>, GatewayError> {
        {
            let slot = self.slot.read().await;
            if slot.let_go.is_none() {
                return Ok(RwLockReadGuard::map(slot, |s| &s.hold));
            }
        }
        let mut slot = self.slot.write().await;
        if let Some(let_go) = &slot.let_go {
            // Said only when the model has to come back: one still up is
            // joined at once.
            let report = self.report.as_deref().filter(|_| !let_go.is_up());
            if let Some(r) = report {
                r.loading(let_go.alias()).await;
            }
            let started = std::time::Instant::now();
            let regained = let_go.regain().await;
            if let Some(r) = report {
                let outcome = match &regained {
                    Ok(_) => Ok(started.elapsed().as_millis() as u64),
                    Err(e) => Err(e),
                };
                r.settled(let_go.alias(), outcome).await;
            }
            slot.hold = Some(regained?);
            slot.let_go = None;
        }
        Ok(RwLockReadGuard::map(
            RwLockWriteGuard::downgrade(slot),
            |s| &s.hold,
        ))
    }

    /// What the next model call for `ir` goes out on (module doc): the claim,
    /// regained if it was let go, or the gate's route for this call when the
    /// regain was refused by a GPU block or a lost candidate.
    pub(crate) async fn call_on(
        &self,
        state: &SharedState,
        ir: &ChatRequest,
    ) -> Result<CallOn<'_>, Refused> {
        match self.for_call().await {
            Ok(hold) => Ok(CallOn::Claim(hold)),
            Err(e) if reroutes(state, &e) => match reroute(state, ir, self.origin().await).await {
                Ok(opened) => Ok(CallOn::Rerouted(Box::new(opened))),
                Err(f) => Err(Refused {
                    fallback: f.headers.fallback_reason(),
                    route: f.route,
                    gate: true,
                    error: f.error,
                }),
            },
            Err(error) => Err(Refused {
                route: None,
                gate: false,
                fallback: None,
                error,
            }),
        }
    }

    /// Whose claim the loop's is (per-request background design §7): a
    /// guest's run stays a guest when a call of it is sent elsewhere.
    async fn origin(&self) -> Origin {
        let slot = self.slot.read().await;
        match (&slot.hold, &slot.let_go) {
            (Some(hold), _) => hold.origin(),
            (None, Some(let_go)) => let_go.origin(),
            (None, None) => Origin::Owner,
        }
    }
}

/// Whether a refused regain goes to the gate rather than failing the call
/// (module doc).
fn reroutes(state: &SharedState, e: &GatewayError) -> bool {
    state.snapshot().gpu_block().is_some() || matches!(e, GatewayError::CandidateLost { .. })
}

/// Where a call of the loop for `ir` goes when its claim's regain was
/// refused (module doc): the gate's resolve and admission, as for any
/// request — a block's fallback or its refusal, a candidate alias's next
/// pick. The candidate alias's facets are asked as at the loop's own
/// admission, and the call goes as the loop's claim did (`origin`): a
/// guest's stays a guest.
pub(crate) async fn reroute(
    state: &SharedState,
    ir: &ChatRequest,
    origin: Origin,
) -> Result<Opened, OpenFailed> {
    let uses = crate::gate::request_facets(ir, None).insert(crate::candidates::Facet::ToolCalls);
    crate::gate::resolve(state, &ir.model_alias, RouteCheck::None, origin)
        .await?
        .using(uses)?
        .admit(state)
        .await
}
