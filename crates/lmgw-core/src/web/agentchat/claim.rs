//! The tool loop's GPU claim (§9b), and letting it go while a heard voice
//! turn's tools wait for its user row (voice-audio-input design §3.4,
//! verification review).
//!
//! The loop holds one claim for its whole run (`vram::LocalHold`: no
//! eviction between its model calls). A heard turn's first tool call waits
//! for the turn's user row (`heard`), and the row waits for the turn's
//! transcript — an ASR call whose model may need the very room the claim
//! pins. Holding it through that wait was a circular wait: with
//! `vram.queue_timeout_seconds` 0 the turn hung for good, held and silent;
//! with a timeout the transcription failed. So the claim is let go for the
//! wait ([`LoopClaim::let_go`]), as the plain path drops its claim before
//! its reply waits for the row, and the next model call takes it again
//! first ([`LoopClaim::for_call`]) through admission on the same model
//! (`vram::LetGo::regain`). An ASR that fits beside the chat model evicts
//! nothing, and the regain then joins the model still up.
//!
//! The loop's calls are sequential — a model call, then its tools — so the
//! claim is never let go under a model call in flight. A turn whose tools
//! may not run, and is stopped, makes no further call and takes nothing
//! again.
//!
//! **A block switched on meanwhile** (the GPU hold, a benchmark run) refuses
//! the regain, as it refuses any start. The call then goes where any request
//! goes under that block — the configured fallback, through the gate's own
//! resolve and admission ([`under_block`]) — rather than failing the turn
//! after its tool frames (review V12, the owner's ruling of 2026-10-06:
//! configured fallbacks always apply). A fallback that cannot take the
//! turn's audio gets its transcript (`unheard`).

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::GatewayError;
use crate::gate::{OpenFailed, Opened, RouteCheck};
use crate::ir::ChatRequest;
use crate::state::SharedState;
use crate::vram::{LetGo, LocalHold};

/// The loop's claim (module doc).
pub(super) struct LoopClaim {
    slot: RwLock<Slot>,
}

struct Slot {
    /// The claim: `None` for a route with no local container, or while let
    /// go.
    hold: Option<LocalHold>,
    /// The claim let go, which the next model call takes again.
    let_go: Option<LetGo>,
}

impl LoopClaim {
    /// The claim admission gave the loop.
    pub fn new(hold: Option<LocalHold>) -> Self {
        Self {
            slot: RwLock::new(Slot { hold, let_go: None }),
        }
    }

    /// Let the claim go until the next model call (module doc). Called
    /// between model calls only.
    pub async fn let_go(&self) {
        let mut slot = self.slot.write().await;
        if let Some(hold) = slot.hold.take() {
            slot.let_go = Some(hold.let_go());
        }
    }

    /// The claim a model call sends on, taken again first when it was let
    /// go. An error is the call's: admission's refusal of the regain.
    pub async fn for_call(&self) -> Result<RwLockReadGuard<'_, Option<LocalHold>>, GatewayError> {
        let mut slot = self.slot.write().await;
        if let Some(let_go) = &slot.let_go {
            slot.hold = Some(let_go.regain().await?);
            slot.let_go = None;
        }
        Ok(RwLockReadGuard::map(
            RwLockWriteGuard::downgrade(slot),
            |s| &s.hold,
        ))
    }
}

/// Where a call of the loop for `ir` goes while a GPU block refuses its
/// claim's regain (module doc): the gate's resolve and admission, as for any
/// request — the block's fallback, or its refusal when there is none. The
/// candidate alias's facets are asked as at the loop's own admission.
pub(super) async fn under_block(
    state: &SharedState,
    ir: &ChatRequest,
) -> Result<Opened, OpenFailed> {
    let uses = crate::gate::request_facets(ir, None).insert(crate::candidates::Facet::ToolCalls);
    crate::gate::resolve(state, &ir.model_alias, RouteCheck::None)
        .await?
        .using(uses)?
        .admit(state)
        .await
}
