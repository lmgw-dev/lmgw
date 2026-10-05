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

use super::super::stop::{canceled, stopped};
use super::{StopSignal, Synthesis};
use crate::error::GatewayError;

impl Synthesis {
    /// Let go of the model while the client does not read (module doc):
    /// `true` when there was a claim to let go.
    pub fn let_go(&mut self) -> bool {
        let had = self.hold.take().is_some();
        self.regain |= had;
        had
    }

    /// Whether the claim was let go, and the next clause has to take it
    /// again.
    pub fn let_go_pending(&self) -> bool {
        self.regain
    }

    /// Take the claim again (module doc), raced against `stop`. An error is
    /// the response's: the GPU hold, a benchmark, or admission's refusal.
    pub async fn regain(&mut self, stop: Option<&StopSignal>) -> Result<(), GatewayError> {
        if !self.regain {
            return Ok(());
        }
        let admitted = tokio::select! {
            biased;
            () = stopped(stop) => return Err(canceled("stopped by the caller during admission")),
            a = crate::vram::admit(&self.state, &self.route, &self.alias) => a?,
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
        self.regain = false;
        Ok(())
    }
}
