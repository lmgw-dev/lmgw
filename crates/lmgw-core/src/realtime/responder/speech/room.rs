//! A speaking response's back-pressure, and what lifts it (realtime design
//! §8.2, §9.1).
//!
//! **The bound.** Synthesis runs ~30× real time, so the next clause waits
//! while more than `synthesis_ahead_s` of the response's audio is queued and
//! not yet sent (WP3 review M3): a looping model or a huge answer cannot hold
//! its whole audio in memory. Nothing is dropped.
//!
//! **Its price, and when it is not paid** (package B review 1). While the
//! speaker waits it keeps the TTS route — and with a local model, its claim
//! on the container: the model cannot be evicted, and the GPU hold's sweep
//! sees it draining. For a long answer that is most of its playback. That is
//! fine while nobody wants the card, and wrong in two cases, each of which
//! **lifts the bound** for the rest of the response ([`Unbind`]):
//! - the **GPU hold** is switched on, or a benchmark takes the GPU — the
//!   hold means lmgw uses no VRAM, so its work must end soon, not after
//!   minutes of playback;
//! - an **admission waits for room** (the registry's draining mark) — a
//!   claimed model is never evicted, so the TTS model would stand in the
//!   way of that request (the session's own ASR included) for as long as
//!   the answer plays.
//!
//! Lifted, the rest of the answer is synthesized at full speed and the hold
//! is dropped after its last clause, as for an answer shorter than the
//! bound; its audio waits in the writer as PCM (48 KB a second) — bounded
//! by the answer, the chat model's output maximum. A response on a route
//! without a local claim (a cloud TTS, a fallback) keeps the bound whatever
//! happens: it holds nothing on the GPU, and lifting it would only cost
//! memory.
//!
//! **A model on the CPU** (the audio class's per-row CPU switch) is claimed
//! like any local model, but holds nothing on the card: the hold has no
//! claim on it and no admission evicts it, so only a benchmark's lease —
//! which covers it, and whose drain waits for its claim — lifts its bound.
//! Its stall is judged like any other.
//!
//! **A writer that stalls keeps the bound** (B2 review 3): the client
//! stopped reading — the playing window ended, by more than one output delta
//! ([`DELTA_MS`]), while the answer's audio is still waiting to leave. The
//! pacer never lets a reading client run dry while audio waits (`pacing`),
//! so this is no underrun. Lifting the bound here would synthesize the rest
//! of a looping model's generation into a queue nobody reads; keeping the
//! claim would pin the model until the liveness check ends the session, and
//! forever with `ping_interval_s` 0. So the wait says [`Waited::Stalled`]
//! and goes on with the bound; the speaker lets the model go, and takes it
//! again when the writer has room (`proxy::synthesize`'s `claim`). The
//! stall reads the same facts as the window (B3 review 8): the end of what
//! left, while audio still waits.
//!
//! The hold and the draining mark are atomic reads, judged whenever the
//! writer releases audio (every delta while it keeps pace) and at the stall
//! deadline — no polling of their own.
//!
//! **No writer, no bound** (chat-voice design §6.1): the Chat's read-aloud
//! has no paced writer, so it has no playing window to judge a stall by and
//! nothing to wait for — a fake one would count as stalled at once, and the
//! claim would be let go and taken again in a loop. With no [`Progress`]
//! there is room before every clause.

use std::fmt;
use std::time::Duration;

use tokio::time::Instant;

use super::super::super::output::DELTA_MS;
use super::super::super::writer::Progress;
use crate::bench::lease::GpuBlock;
use crate::error::GatewayError;
use crate::proxy::synthesize::Synthesis;
use crate::proxy::StopSignal;
use crate::runtime::Placement;
use crate::state::SharedState;

/// Why a response's synthesis stopped waiting for its playback (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unbind {
    /// The owner's GPU hold is on.
    Hold,
    /// A benchmark run has the GPU.
    Benchmark,
    /// An admission waits for room on the GPU.
    RoomWanted,
}

impl fmt::Display for Unbind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hold => "the GPU hold is on",
            Self::Benchmark => "a benchmark has the GPU",
            Self::RoomWanted => "a request is waiting for room on the GPU",
        })
    }
}

/// How a wait for room ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waited {
    /// There is room — or the bound was lifted before (none to wait for).
    Room,
    /// The bound is lifted for the rest of the response.
    Lifted(Unbind),
    /// The client stopped reading: the bound stays, and the model should be
    /// let go before waiting on (module doc).
    Stalled,
}

/// The GPU-side reasons for a claim on a model running `on`, read now: the
/// hold or a benchmark's lease, then the draining mark of an admission
/// waiting for room. A model on the CPU answers only to the lease (module
/// doc).
pub(crate) fn gpu_pressure(state: &SharedState, on: Placement) -> Option<Unbind> {
    match state.snapshot().gpu_block_at(on) {
        Some(GpuBlock::Hold) => return Some(Unbind::Hold),
        Some(GpuBlock::Benchmark(_)) => return Some(Unbind::Benchmark),
        None => {}
    }
    (on.is_gpu() && state.runtime().draining_for_owner()).then_some(Unbind::RoomWanted)
}

/// One response's back-pressure (module doc).
pub(crate) struct Room {
    /// The writer's paced send; `None` with no writer (module doc).
    progress: Option<Progress>,
    /// The bound, in samples at 24 kHz; `None` for none — never set, or
    /// lifted.
    ahead: Option<u64>,
}

impl Room {
    /// A bound needs a writer to measure it against: with no `progress` the
    /// response always has room, so an `ahead` there would be a bound that
    /// silently does nothing (WP4 review n1).
    pub fn new(progress: Option<Progress>, ahead: Option<u64>) -> Self {
        debug_assert!(
            progress.is_some() || ahead.is_none(),
            "a synthesis bound with no writer to measure it against"
        );
        Self { progress, ahead }
    }

    /// Wait until no more than the bound of generation `gen`'s audio is
    /// queued and not yet sent — `queued`: samples handed to the core so
    /// far — or the response stops. `pressure`: `Some` while the response
    /// holds a local model, the GPU-side reasons to lift the bound
    /// ([`gpu_pressure`]); the stall is judged then too.
    ///
    /// [`Waited::Lifted`]: the bound is lifted, for this wait and every
    /// later one. [`Waited::Stalled`]: the bound stays — let the model go,
    /// and wait again without `pressure`. A writer that is gone ends the
    /// response as stopped: the session is ending, and nothing is admitted
    /// again for it (B4 review).
    pub async fn wait(
        &mut self,
        gen: u64,
        queued: u64,
        stop: &StopSignal,
        pressure: Option<&(dyn Fn() -> Option<Unbind> + Sync)>,
    ) -> Result<Waited, GatewayError> {
        let Some(ahead) = self.ahead else {
            return Ok(Waited::Room);
        };
        let Some(progress) = self.progress.as_mut() else {
            return Ok(Waited::Room);
        };
        let since = Instant::now();
        loop {
            let sent = progress.sent(gen);
            if queued.saturating_sub(sent) <= ahead {
                return Ok(Waited::Room);
            }
            let stall_at = match pressure {
                None => None,
                Some(pressure) => {
                    // The client runs dry at its window's end — or, with
                    // nothing of this answer sent yet, holds nothing now.
                    let dry = progress.window_end(gen).map_or(since, |e| e.max(since));
                    let stall_at = dry + Duration::from_millis(DELTA_MS);
                    if let Some(why) = pressure() {
                        self.ahead = None;
                        return Ok(Waited::Lifted(why));
                    }
                    if Instant::now() >= stall_at {
                        return Ok(Waited::Stalled);
                    }
                    Some(stall_at)
                }
            };
            tokio::select! {
                biased;
                () = stop.raised() => return Err(crate::proxy::canceled("stopped by the caller")),
                alive = progress.changed() => {
                    if !alive {
                        return Err(crate::proxy::canceled("the session's writer is gone"));
                    }
                }
                () = sleep_until(stall_at), if stall_at.is_some() => {}
            }
        }
    }
}

/// What [`before_clause`] needs of the response's TTS route: where the
/// local model it holds runs, if it holds one, and letting that claim go
/// and taking it again ([`Synthesis`]; a test's fake).
pub(super) trait Claim {
    fn held_on(&self) -> Option<Placement>;
    /// `true` when there was a claim to let go.
    fn let_go(&mut self) -> bool;
    fn let_go_pending(&self) -> bool;
    async fn regain(&mut self, stop: Option<&StopSignal>) -> Result<(), GatewayError>;
}

impl Claim for Synthesis {
    fn held_on(&self) -> Option<Placement> {
        Synthesis::held_on(self)
    }
    fn let_go(&mut self) -> bool {
        Synthesis::let_go(self)
    }
    fn let_go_pending(&self) -> bool {
        Synthesis::let_go_pending(self)
    }
    async fn regain(&mut self, stop: Option<&StopSignal>) -> Result<(), GatewayError> {
        Synthesis::regain(self, stop).await
    }
}

/// Wait for room before the next clause of generation `gen`, `queued`
/// samples handed on so far (module doc): a lift is logged; a stall lets the
/// model go — logged — and the wait goes on with the bound; once there is
/// room, a model let go is admitted again on the response's route.
/// `pressure`: the GPU-side reasons for a model where it runs
/// ([`gpu_pressure`]), judged — like the stall — only while the response
/// holds a local model. `(alias, label)`: the TTS alias and what the log
/// lines start with.
pub(super) async fn before_clause<C: Claim>(
    room: &mut Room,
    mut synthesis: Option<&mut C>,
    (gen, queued): (u64, u64),
    stop: &StopSignal,
    pressure: &(dyn Fn(Placement) -> Option<Unbind> + Sync),
    (alias, label): (&str, &str),
) -> Result<(), GatewayError> {
    loop {
        let held = synthesis.as_deref().and_then(C::held_on);
        let judged = move || held.and_then(pressure);
        let judged: &(dyn Fn() -> Option<Unbind> + Sync) = &judged;
        match room
            .wait(gen, queued, stop, held.is_some().then_some(judged))
            .await?
        {
            Waited::Room => break,
            Waited::Lifted(why) => {
                tracing::info!(
                    "{label}: TTS '{alias}' no longer waits for its playback ({why}): the rest of \
                     the answer is synthesized now, and the model is let go after it"
                );
                break;
            }
            Waited::Stalled => {
                if synthesis.as_deref_mut().is_some_and(C::let_go) {
                    tracing::info!(
                        "{label}: the client is not reading its audio — TTS '{alias}' lets go of its \
                         model and waits, its synthesis bound kept; the model is admitted again \
                         when the client reads"
                    );
                }
            }
        }
    }
    if let Some(synth) = synthesis.filter(|s| s.let_go_pending()) {
        tracing::info!(
            "{label}: the client reads its audio again — TTS '{alias}' is admitted again for the \
             rest of the answer"
        );
        synth.regain(Some(stop)).await?;
    }
    Ok(())
}

async fn sleep_until(at: Option<Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}
