//! Letting a response's TTS model go while its client does not read, and
//! taking it again when it does (realtime design §8.2, §9.1; B2 review 3).
//!
//! A client that stopped reading would otherwise keep the model claimed —
//! never evicted, draining under the GPU hold — for as long as the session
//! lives, and for good with `ping_interval_s` 0. Lifting the synthesis bound
//! instead, as the GPU-side reasons do, would synthesize the rest of a
//! looping model's generation into memory nobody reads. So the bound stays
//! and the claim goes ([`Synthesis::let_go`]); the next clause takes it again
//! ([`Synthesis::regain`]) through admission on the **same route**: a GPU
//! hold or a benchmark that came on meanwhile refuses it (the hold means no
//! VRAM), admission may queue it, and a model evicted meanwhile is started
//! again. No other route is tried, so the voice never changes mid-answer to
//! a fallback (§9.1). A model whose row was deleted meanwhile has nothing
//! to admit: the response fails there, cleanly, rather than sending its
//! later clauses to a container nothing keeps up any more (B4 review).
//!
//! The claim comes back **as whoever held it** (per-request background
//! design §11.13, 2026-10-22): an owner's through admission, a guest's by
//! the guest's start rule ([`crate::vram::Restart::Background`]) — joined if
//! loaded, started only into room that disturbs nobody, else the deferral.
//! No guest reaches this yet: only a realtime response lets its claim go,
//! and realtime is the owner's traffic (§12).

use super::super::stop::{canceled, stopped};
use super::{StopSignal, Synthesis};
use crate::config::Route;
use crate::error::{Deferred, GatewayError};
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::{BackgroundStart, LocalHold};

impl Synthesis {
    /// Let go of the model while the client does not read (module doc):
    /// `true` when there was a claim to let go.
    pub fn let_go(&mut self) -> bool {
        let Some(hold) = self.hold.take() else {
            return false;
        };
        self.regain = Some(hold.origin());
        true
    }

    /// Whether the claim was let go, and the next clause has to take it
    /// again.
    pub fn let_go_pending(&self) -> bool {
        self.regain.is_some()
    }

    /// Take the claim again (module doc), raced against `stop`. An error is
    /// the response's: the GPU hold, a benchmark, admission's refusal, or a
    /// guest's deferral.
    pub async fn regain(&mut self, stop: Option<&StopSignal>) -> Result<(), GatewayError> {
        let Some(origin) = self.regain else {
            return Ok(());
        };
        let admitted = tokio::select! {
            biased;
            () = stopped(stop) => return Err(canceled("stopped by the caller during admission")),
            a = take_again(&self.state, &self.route, &self.alias, origin) => a?,
        };
        // It held a claim, so a row described it: none now means it was
        // deleted while the client did not read (module doc).
        let Some(hold) = admitted else {
            return Err(GatewayError::NotFound(format!(
                "TTS '{}': its local model was let go while the client did not read, and its \
                 row is gone now (deleted meanwhile) — the rest of the answer is not spoken",
                self.alias
            )));
        };
        self.hold = Some(hold);
        self.regain = None;
        Ok(())
    }
}

/// The claim on `route` again, as `origin` held it (module doc): `Ok(None)`
/// when no row describes the model any more.
async fn take_again(
    state: &SharedState,
    route: &Route,
    alias: &str,
    origin: Origin,
) -> Result<Option<LocalHold>, GatewayError> {
    match origin {
        Origin::Owner => crate::vram::admit(state, route, alias).await,
        Origin::Background => match crate::vram::start_background(state, route, alias).await? {
            BackgroundStart::Started(hold) => Ok(Some(*hold)),
            BackgroundStart::Blocked(why) => Err(GatewayError::GpuDeferred {
                model: alias.to_string(),
                detail: format!("GPU in use by {why}"),
                on: Deferred::Model,
            }),
        },
    }
}
