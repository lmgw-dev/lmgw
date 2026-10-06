//! Letting a hold's claim go while its holder waits on work that may need
//! the room the claim pins, and taking it again afterwards
//! (voice-audio-input design §3.4, verification review).
//!
//! A loop holds one claim for its whole run ([`LocalHold`]), and the claim
//! keeps its model from being evicted. A heard voice turn's tool loop waits,
//! before its first tool call, for the turn's user row, which waits for the
//! turn's transcript — an ASR call whose model may need exactly that room.
//! Holding the claim through the wait is a circular wait: with
//! `vram.queue_timeout_seconds` 0 the turn hangs for good, and with a timeout
//! the transcription fails. So the loop lets the claim go
//! ([`LocalHold::let_go`]) for the wait — no model call is in flight then —
//! and its next model call takes it again ([`LetGo::regain`]).
//!
//! **What is kept** is what the hold is, not the claim: the model, the alias,
//! what admission knew ([`AdmissionPolicy`]: a climb's or a candidate's walk
//! reads it), whose claim it is ([`Origin`]) and how it restarts
//! ([`Restart`]). A guest stays a guest, and a candidate alias's walk goes
//! on as it would have.
//!
//! **The regain is the hold's own rule**, on the same model — never another
//! route, so a loop's later calls do not move to a fallback for having
//! waited. (A GPU block that refuses it is no such move: the tool loop then
//! sends that call where the gate sends any request under the block, its
//! configured fallback — `web::agentchat::claim::under_block`, review V12.)
//! - [`Restart::Admit`]: admission as for any start — a model still up is
//!   joined at once; one evicted meanwhile queues, evicts and starts like any
//!   request, and the GPU hold or a benchmark refuses it;
//! - a candidate's hold ([`Restart::Background`], [`Restart::No`]): a model
//!   still loaded is joined as before; one gone comes back only as its rule
//!   allows, else [`GatewayError::CandidateLost`].

use super::{admit_base, restart_candidate, LocalHold, Restart, Target};
use crate::error::GatewayError;
use crate::gate::AdmissionPolicy;
use crate::runtime::registry::Origin;
use crate::state::SharedState;

/// A [`LocalHold`] whose claim was let go (module doc): everything the hold
/// is but the claim.
pub struct LetGo {
    state: SharedState,
    target: Target,
    alias: String,
    policy: Option<AdmissionPolicy>,
    origin: Origin,
    restart: Restart,
}

impl LocalHold {
    /// Let the claim go, keeping what the hold is (module doc): the model is
    /// evictable from here on, and its release stamps it used now.
    pub fn let_go(self) -> LetGo {
        let LocalHold {
            guard,
            state,
            target,
            alias,
            recovering: _,
            policy,
            origin,
            restart,
        } = self;
        drop(guard);
        LetGo {
            state,
            target,
            alias,
            policy,
            origin,
            restart,
        }
    }
}

impl LetGo {
    /// Take the claim again by the hold's own rule (module doc). An error is
    /// the next model call's: the GPU hold, a benchmark, admission's
    /// refusal or its queue timeout, a candidate lost, a row deleted.
    pub async fn regain(&self) -> Result<LocalHold, GatewayError> {
        let (state, target, alias) = (&self.state, &self.target, self.alias.as_str());
        let mut hold = match self.restart {
            Restart::Admit => admit_base(state, target, alias).await?,
            rule @ (Restart::Background | Restart::No) => {
                let snap = state.snapshot();
                let joined = state
                    .vram
                    .join_at(state, &snap, target, alias, self.origin, rule)
                    .await?;
                match joined {
                    Some(hold) => hold,
                    None => restart_candidate(state, target, alias, rule).await?,
                }
            }
        };
        hold.policy = self.policy.clone();
        hold.origin = self.origin;
        hold.restart = self.restart;
        Ok(hold)
    }
}
