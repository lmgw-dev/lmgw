//! A heard voice turn's tools (voice-audio-input design §3.4, WP3 review
//! #1): the model answers the turn's audio before its transcript is in,
//! and on noise it greets or hallucinates — a tool call it makes then must
//! not run before the transcript said the turn had words.
//!
//! **The tools wait for the user row.** Before the loop's first tool call
//! — the self-admin `lmgw__*` tools, an MCP server's, the knowledge bases'
//! alike — the executor waits for the journal's answer about the turn's
//! user row, the pre-save barrier the reply waits on too
//! (`chat_turn::tools_may_run`). A row written, or none to write, lets
//! every call of the turn run. Anything else runs none, and each call's
//! result says why ([`not_run`]):
//! - a veto, a row the journal could not write, or a journal gone: the
//!   reply is not saved either, so the loop is stopped;
//! - a failed transcription: the reply plays and is saved — the user hears
//!   it — but a tool run is a side effect nobody confirmed had words, so
//!   the model goes on with its calls not run (verification review).
//!
//! The response's stop, a cut, ends the wait as it ends any tool call (the
//! loop's cancel). **The loop's GPU claim is let go for the wait** (`claim`):
//! the row waits for an ASR call that may need the room it pins.
//!
//! **A pause in mid-sentence** may still run a tool on the first half: its
//! row is written once its transcript has words, and the call runs then —
//! about when `off` would have started the model on that half anyway. A
//! known, accepted behaviour (design §3.4).
//!
//! A turn that does not hear the user's audio has no barrier, and runs its
//! tools as before.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::OnceCell;

use super::chat_turn::{tools_may_run, RowWatch, ToolsHeld};
use super::claim::LoopClaim;
use crate::agent::{ToolExecutor, ToolOutcome};
use crate::proxy::StopHandle;

/// What a call the turn did not run says, in its result: why.
fn not_run(why: ToolsHeld) -> &'static str {
    match why {
        ToolsHeld::Veto => "not run: the spoken turn this answered had no words",
        ToolsHeld::Failed => {
            "not run: the transcript failed, so lmgw could not confirm what was said"
        }
        ToolsHeld::Unwritten => {
            "not run: the spoken turn's user message could not be stored, so this reply is not \
             kept"
        }
        ToolsHeld::Lost => {
            "not run: the voice session ended before it stored the spoken turn's user message"
        }
    }
}

/// The loop's executor for a heard turn (module doc).
pub(super) struct HeardTools<'a, E: ?Sized> {
    inner: &'a E,
    /// The turn's user row; `None`: no heard turn, nothing to wait for.
    row: Option<RowWatch>,
    /// The journal's answer, once had: whether the tools may run.
    gate: OnceCell<Result<(), ToolsHeld>>,
    /// The loop's stop, raised when they may not.
    stop: &'a StopHandle,
    /// The loop's GPU claim, let go while the gate waits.
    claim: &'a LoopClaim,
    /// The gate stopped the turn.
    held_off: AtomicBool,
}

impl<'a, E: ToolExecutor + ?Sized> HeardTools<'a, E> {
    pub fn new(
        inner: &'a E,
        row: Option<RowWatch>,
        stop: &'a StopHandle,
        claim: &'a LoopClaim,
    ) -> Self {
        Self {
            inner,
            row,
            gate: OnceCell::new(),
            stop,
            claim,
            held_off: AtomicBool::new(false),
        }
    }

    /// Whether the gate stopped the loop: the turn ran none of its tools.
    pub fn held_off(&self) -> bool {
        self.held_off.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl<E: ToolExecutor + ?Sized> ToolExecutor for HeardTools<'_, E> {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        if let Some(row) = &self.row {
            let gate = self.gate.get_or_init(|| async {
                if row.borrow().is_none() {
                    self.claim.let_go().await;
                }
                tools_may_run(row.clone()).await
            });
            let gate = *gate.await;
            if let Err(why) = gate {
                if why.stops() {
                    self.held_off.store(true, Ordering::Relaxed);
                    self.stop.stop();
                }
                return ToolOutcome::error(not_run(why));
            }
        }
        self.inner.call(name, args).await
    }
}
