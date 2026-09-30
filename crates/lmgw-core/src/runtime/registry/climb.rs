//! The climb primitive and the send counter (ladder design §3.4, §5; §12
//! entries 7, 10 and 19–23).
//!
//! A ladder model climbs **in place**: the registry entry stays, the container
//! under it is replaced by one started at a higher rung. That is what makes
//! the rest of the gateway need no second idea of "the model":
//!
//! - **The entry is the lineage, the generation is the container.** The
//!   entry's `phase` channel is what claims are counted against
//!   ([`AcquireGuard`]'s identity), and a climb keeps it, so an idle claim —
//!   a tool loop between turns — stays counted and the reaper never sees the
//!   model idle under it. [`Entry::generation`] is the container; a climb
//!   gives the entry a new one, and a claim on the old one learns it moved
//!   ([`ClaimStatus::Moved`]) and follows on its next send.
//! - **Drain waits for sends, not claims** (entry 10). [`SendGuard`]s count
//!   the sends in flight on one container; a climb's mark makes
//!   [`Registry::begin_send`] refuse under the same lock that increments, so
//!   once the count is zero it stays zero.
//! - **The key is never absent during a climb.** Mark (`ready`, climbing) →
//!   start ([`ClimbTicket::start`]: `starting`, new generation, charged at the
//!   new rung) → settle (`ready`). Acquirers park on the entry all the way
//!   through instead of starting the model a second time next to it.
//!
//! The VRAM half of a climb — whether the new rung fits, and what makes room —
//! is [`crate::vram::climb`]'s; this module is the machinery it drives, the
//! same split as `acquire` and admission.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;

use super::{
    next_generation, AcquireGuard, Entry, Key, Phase, Registry, RuntimeError, RuntimeState,
    StartSpec, Started,
};
use crate::runtime::container_name;
use crate::runtime::descriptor::{file_name, RungCharge, RungPos};

// ---------------------------------------------------------------------------
// What the entry and the views carry
// ---------------------------------------------------------------------------

/// A climb in progress on one entry ([`Entry::climb`]).
pub(super) struct ClimbMark {
    /// The rung it climbs to. Raised by a second trigger that needs more,
    /// until the start is claimed ([`Registry::mark_climb`]).
    pub(super) to: RungPos,
    reason: String,
    stage: String,
    since: Instant,
    /// The start is claimed: the entry is `starting` on a new generation, and
    /// the target can no longer change.
    claimed: bool,
    /// The climb's admission is queued for the VRAM admission gate right now
    /// ([`ClimbTicket::set_at_gate`]) — what another admission holding the
    /// gate yields to (§12 entries 53 and 71). Not during the drain: the
    /// climb cannot use the gate then.
    at_gate: bool,
}

impl ClimbMark {
    pub(super) fn status(&self, now: Instant) -> ClimbStatus {
        ClimbStatus {
            to: self.to.index + 1,
            of: self.to.of,
            reason: self.reason.clone(),
            stage: self.stage.clone(),
            seconds: now.saturating_duration_since(self.since).as_secs(),
            at_gate: self.at_gate,
        }
    }
}

/// The rung a container runs, as the status surfaces show it: `rung k/n`,
/// 1-based (ladder design §6, §12 entry 11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RungStatus {
    /// 1-based: the base is rung 1.
    pub rung: usize,
    pub of: usize,
    /// The weights' file name.
    pub gguf: String,
}

impl RungStatus {
    pub(super) fn from_charge(c: &RungCharge) -> Self {
        Self {
            rung: c.index + 1,
            of: c.of,
            gguf: c.gguf_file().to_string(),
        }
    }
}

/// A climb in progress, as the status surfaces show it (ladder design §6):
/// "climbing to 3/3 — prompt 41,210 + 8,192 > 30,000 — draining 2 sends".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClimbStatus {
    /// The rung it climbs to, 1-based.
    pub to: usize,
    pub of: usize,
    /// Why — the numbers of the request that did not fit.
    pub reason: String,
    /// What it is doing right now, in words.
    pub stage: String,
    /// Since the mark, in whole seconds (a frame changes once a second).
    pub seconds: u64,
    /// Its admission is queued for the admission gate right now
    /// ([`ClimbTicket::set_at_gate`]). Not published: it is the scheduler's
    /// signal, and `stage` says the same in words.
    #[serde(skip)]
    pub at_gate: bool,
}

// ---------------------------------------------------------------------------
// Claim status and waiting
// ---------------------------------------------------------------------------

/// Where a claim stands against the container its entry runs now
/// ([`Registry::claim_status`]).
#[derive(Debug)]
pub enum ClaimStatus {
    /// The claim's container is the one running, and nothing is replacing it.
    Current,
    /// A climb is replacing the container (or bringing up its new rung):
    /// wait on this, then ask again.
    Climbing(PhaseWait),
    /// The entry carried the claim onto a newer container — a climb's new
    /// rung. Move the claim there ([`AcquireGuard::retarget`]); it was never
    /// released.
    Moved {
        port: u16,
        gate: Option<Arc<crate::gate::facts::GateFacts>>,
        generation: u64,
    },
    /// The entry the claim was taken on is gone (stopped, or a climb failed):
    /// the claim protects nothing any more, and a new admission is the next
    /// move — at the base rung, like every start.
    Gone,
}

/// A wait for a start or a climb to settle, either way.
#[derive(Debug)]
pub struct PhaseWait(watch::Receiver<Phase>);

/// How the start or climb a [`PhaseWait`] waited on ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// It is up (or a climb was abandoned before its start, and the running
    /// rung serves on).
    Ready,
    /// The entry is gone: stopped, or its claim abandoned.
    Gone,
    /// A climb's new rung would not start ([`Phase::ClimbFailed`]): which
    /// rung, and the start's own error.
    ClimbFailed {
        rung: Option<RungPos>,
        cause: String,
    },
}

impl PhaseWait {
    /// Returns once the entry is no longer starting or climbing — ready,
    /// gone, or failed. The caller asks again; this says nothing about which.
    pub async fn settled(self) {
        self.outcome().await;
    }

    /// [`Self::settled`], saying how it ended.
    pub async fn outcome(mut self) -> Settled {
        loop {
            let now = match &*self.0.borrow_and_update() {
                Phase::Starting | Phase::Climbing => None,
                Phase::Ready => Some(Settled::Ready),
                Phase::Gone(_) => Some(Settled::Gone),
                Phase::ClimbFailed { rung, cause } => Some(Settled::ClimbFailed {
                    rung: *rung,
                    cause: cause.clone(),
                }),
            };
            if let Some(settled) = now {
                return settled;
            }
            if self.0.changed().await.is_err() {
                // Every sender is gone without a verdict: the entry went.
                return Settled::Gone;
            }
        }
    }
}

fn status_of(e: Option<&Entry>, g: &AcquireGuard) -> ClaimStatus {
    let Some(e) = e.filter(|e| Arc::ptr_eq(&e.phase, &g.phase)) else {
        return ClaimStatus::Gone;
    };
    // `starting` under a live claim is only ever a climb's new rung: a claim
    // is taken on a `ready` entry, and only a climb takes one back.
    if e.climb.is_some() || e.state == RuntimeState::Starting {
        return ClaimStatus::Climbing(PhaseWait(e.phase.subscribe()));
    }
    if e.state == RuntimeState::Stopping {
        return ClaimStatus::Gone;
    }
    if e.generation == g.generation {
        ClaimStatus::Current
    } else {
        ClaimStatus::Moved {
            port: e.host_port,
            gate: e.gate.clone(),
            generation: e.generation,
        }
    }
}

// ---------------------------------------------------------------------------
// Sends
// ---------------------------------------------------------------------------

/// One send in flight on one container, counted for the climb's drain
/// (ladder design §12 entry 10). Released on drop — keep it until the send's
/// response has ended, next to the turn's lease.
#[must_use = "the guard is the send's place in the drain count — keep it until the response ends"]
pub struct SendGuard {
    reg: Arc<Registry>,
    key: Key,
    phase: Arc<watch::Sender<Phase>>,
    generation: u64,
}

impl std::fmt::Debug for SendGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendGuard")
            .field("class", &self.key.0)
            .field("model_id", &self.key.1)
            .field("generation", &self.generation)
            .finish()
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let map = self.reg.map();
        if let Some(e) = map.get(&self.key) {
            // Only on the container it was counted on: a guard can outlive it
            // (a forced stop, a recovery), and the successor never counted it.
            if Arc::ptr_eq(&e.phase, &self.phase) && e.generation == self.generation {
                e.sends.send_modify(|n| *n = n.saturating_sub(1));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The climb
// ---------------------------------------------------------------------------

/// What [`Registry::mark_climb`] found.
#[derive(Debug)]
pub enum Marked {
    /// This caller climbs: drain, admit, start ([`ClimbTicket`]).
    Ticket(ClimbTicket),
    /// Another climb of this model is running — marked (and `raised` to this
    /// caller's rung when it needed more; one reload serves both), or already
    /// starting its rung. Wait, then judge the request again on whatever runs.
    Joined { wait: PhaseWait, raised: bool },
    /// The claim is not on the running container any more (it moved, or the
    /// entry is gone): sync the claim, then judge again.
    Stale,
    /// Nothing was marked or joined, and why — a guest's climb of a model
    /// that is the owner's, or while the owner waits for room
    /// ([`Registry::mark_climb_for_guest`], candidate-aliases §12 entry 91).
    Refused(&'static str),
}

/// Why a drain ended without the running rung going quiet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainEnd {
    /// The deadline passed with `sends` still in flight.
    Timeout { sends: u32 },
    /// The entry went away under the drain — a stop that wins against the
    /// climb (ladder design §12 races).
    Gone,
}

/// The right to climb one model, held by exactly one task (ladder design
/// §3.4): the mark is set, new claims and new sends wait, and this ticket
/// drains, then starts the new rung.
///
/// **Dropped before [`Self::start`]** — an admission refused, a drain timed
/// out, the triggering client hung up — it clears the mark and wakes whoever
/// parked on it: the running rung keeps serving, untouched. After `start`
/// nothing is left to undo: the start runs in a task of its own.
pub struct ClimbTicket {
    reg: Arc<Registry>,
    key: Key,
    phase: Arc<watch::Sender<Phase>>,
    /// The container being replaced.
    generation: u64,
    container_name: String,
    port: u16,
    stop_timeout: Duration,
    running: Option<RungCharge>,
    /// The target as marked, for when the entry can no longer be asked.
    to: RungPos,
    sends: watch::Receiver<u32>,
    started: bool,
}

impl std::fmt::Debug for ClimbTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClimbTicket")
            .field("class", &self.key.0)
            .field("model_id", &self.key.1)
            .field("generation", &self.generation)
            .field("port", &self.port)
            .finish()
    }
}

impl Registry {
    /// Where `g`'s claim stands against the container its entry runs now.
    pub fn claim_status(&self, g: &AcquireGuard) -> ClaimStatus {
        status_of(self.map().get(&g.key), g)
    }

    /// Count one send on `g`'s container — only while it is the running
    /// container and no climb is marked on it (ladder design §12 entry 10).
    ///
    /// The count goes up under the same lock that reads the mark, so a drain
    /// that has seen zero after the mark can never see a send begin behind it:
    /// the send is refused instead, with the status that says what to do
    /// (wait for the climb, follow the claim, or re-admit).
    pub fn begin_send(self: &Arc<Self>, g: &AcquireGuard) -> Result<SendGuard, ClaimStatus> {
        let map = self.map();
        let entry = map.get(&g.key);
        let status = status_of(entry, g);
        if !matches!(status, ClaimStatus::Current) {
            return Err(status);
        }
        if let Some(e) = entry {
            e.sends.send_modify(|n| *n += 1);
        }
        Ok(SendGuard {
            reg: Arc::clone(self),
            key: g.key.clone(),
            phase: Arc::clone(&g.phase),
            generation: g.generation,
        })
    }

    /// Mark `g`'s model climbing to `to`, because of `reason` (ladder design
    /// §3.4 step 1). Only the claim on the running, ready, unmarked container
    /// gets the [`ClimbTicket`]; see [`Marked`] for everyone else.
    pub fn mark_climb(self: &Arc<Self>, g: &AcquireGuard, to: RungPos, reason: &str) -> Marked {
        self.mark_climb_unless(g, to, reason, |_, _| None)
    }

    /// [`Self::mark_climb`], unless `refuse` — asked in the lock hold that
    /// marks or joins, so nothing can change what it judged first — names a
    /// reason not to ([`Marked::Refused`]): a guest's climb
    /// (`ownership`, candidate-aliases §12 entry 91).
    pub(super) fn mark_climb_unless(
        self: &Arc<Self>,
        g: &AcquireGuard,
        to: RungPos,
        reason: &str,
        refuse: impl FnOnce(&Registry, &Entry) -> Option<&'static str>,
    ) -> Marked {
        let mut map = self.map();
        let Some(e) = map
            .get_mut(&g.key)
            .filter(|e| Arc::ptr_eq(&e.phase, &g.phase))
        else {
            return Marked::Stale;
        };
        if let Some(why) = refuse(self, e) {
            return Marked::Refused(why);
        }
        if let Some(m) = e.climb.as_mut() {
            let raised = !m.claimed && to.index > m.to.index;
            if raised {
                m.to = to;
                m.reason = reason.to_string();
            }
            return Marked::Joined {
                wait: PhaseWait(e.phase.subscribe()),
                raised,
            };
        }
        if e.generation != g.generation || e.state != RuntimeState::Ready {
            return Marked::Stale;
        }
        e.climb = Some(ClimbMark {
            to,
            reason: reason.to_string(),
            stage: "draining the running rung".into(),
            since: Instant::now(),
            claimed: false,
            at_gate: false,
        });
        e.phase.send_replace(Phase::Climbing);
        Marked::Ticket(ClimbTicket {
            reg: Arc::clone(self),
            key: g.key.clone(),
            phase: Arc::clone(&e.phase),
            generation: e.generation,
            container_name: e.container_name.clone(),
            port: e.host_port,
            stop_timeout: e.stop_timeout,
            running: e.charge.clone(),
            to,
            sends: e.sends.subscribe(),
            started: false,
        })
    }

    /// Set a climb's stage, if the entry is still the one `generation` names,
    /// still `starting` and still climbing; `false` means a stop has taken it
    /// — or is taking it: a stop marks the entry `stopping` long before it
    /// removes it, and that is a stop the climb has already lost to (§12
    /// entry 47).
    fn climb_stage(
        &self,
        key: &Key,
        phase: &Arc<watch::Sender<Phase>>,
        generation: u64,
        stage: String,
    ) -> bool {
        let mut map = self.map();
        match map.get_mut(key) {
            Some(e) if owned_by(e, phase, generation) => match e.climb.as_mut() {
                Some(m) => {
                    m.stage = stage;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The spawned half of a climb (ladder design §3.4 step 4): stop the
    /// running rung, start the new one under the same name, settle.
    ///
    /// - The old rung's stop failing is logged and the start goes ahead:
    ///   `podman run --replace` collects the old container by name, as after
    ///   any failed stop (§12 races).
    /// - A stop that won against the climb (override, delete, shutdown) ends
    ///   it: checked before the load, so a rung nobody wants any more is not
    ///   loaded, and again at settle, where a container started meanwhile is
    ///   removed — exactly [`super::raii::StartClaim::ready`]'s abort. Nothing is
    ///   restarted.
    /// - The new rung failing to start forgets the entry and tells its
    ///   waiters [`Phase::ClimbFailed`]: they go back to admission, which
    ///   starts the base (§12 entry 22).
    async fn run_climb(self: Arc<Self>, job: ClimbJob) -> Result<(), RuntimeError> {
        let (class, model_id) = (job.key.0, job.key.1.clone());
        let label = rung_label(&job.spec);
        let mut unsettled = Unsettled {
            reg: Arc::clone(&self),
            key: job.key.clone(),
            phase: Arc::clone(&job.phase),
            generation: job.generation,
            rung: job.spec.runtime.rung,
            armed: true,
        };

        if let Some(problem) = self
            .stop_container(&job.old_name, job.old_stop_timeout)
            .await
        {
            tracing::warn!(
                container = %job.old_name,
                "climbing {class} model '{model_id}' to {label}: stopping the running rung \
                 failed ({problem}) — starting the new rung anyway; `podman run --replace` \
                 collects the old container by name"
            );
        }
        if !self.climb_stage(
            &job.key,
            &job.phase,
            job.generation,
            format!("loading {label}"),
        ) {
            unsettled.armed = false;
            return Err(RuntimeError::Aborted { class, model_id });
        }

        let name = container_name(&job.spec.container_prefix, class, &model_id);
        let started = self.start_container(&job.spec.as_spec(), &name).await;
        unsettled.armed = false;
        let started = match started {
            Ok(started) => started,
            Err(e) => {
                // Only the owner of the entry speaks for it. A stop that is
                // under way (or done) has told, or will tell, the waiters that
                // the model was stopped — the start failing is then the stop's
                // doing, not a broken rung, and nobody may be sent back to
                // admission to bring the model up again (§12 entry 47).
                let ours = self.forget_if(&job.key, |x| owned_by(x, &job.phase, job.generation));
                if !ours {
                    return Err(RuntimeError::Aborted { class, model_id });
                }
                job.phase.send_replace(Phase::ClimbFailed {
                    rung: job.spec.runtime.rung,
                    cause: e.to_string(),
                });
                return Err(e);
            }
        };

        let Started {
            port,
            warnings,
            capabilities,
            gate,
        } = started;
        let ours = {
            let mut map = self.map();
            match map.get_mut(&job.key) {
                Some(e)
                    if Arc::ptr_eq(&e.phase, &job.phase)
                        && e.generation == job.generation
                        && e.state == RuntimeState::Starting =>
                {
                    let now = Instant::now();
                    e.state = RuntimeState::Ready;
                    e.host_port = port;
                    e.container_name = name.clone();
                    e.started_at = now;
                    // A climb is demand: somebody's request needed this rung a
                    // moment ago.
                    e.last_used = now;
                    e.warnings = warnings;
                    e.capabilities = capabilities;
                    e.gate = gate;
                    e.climb = None;
                    true
                }
                _ => false,
            }
        };
        if !ours {
            if let Err(e) = self.rm_force(&name).await {
                tracing::warn!(container = %name, "removing the container of an aborted climb: {e}");
            }
            return Err(RuntimeError::Aborted { class, model_id });
        }
        job.phase.send_replace(Phase::Ready);
        tracing::info!(container = %name, "{class} model '{model_id}' climbed to {label}");
        Ok(())
    }
}

/// Is `e` the climb task's entry: the lineage, the generation its start
/// claimed, and still `starting` — not `stopping`, which a stop sets as soon as
/// it takes the entry, well before it removes it.
fn owned_by(e: &Entry, phase: &Arc<watch::Sender<Phase>>, generation: u64) -> bool {
    Arc::ptr_eq(&e.phase, phase) && e.generation == generation && e.state == RuntimeState::Starting
}

/// "rung 3/3 (top.gguf)" for the descriptor a climb starts.
fn rung_label(spec: &StartSpec) -> String {
    match spec.runtime.rung_charge() {
        Some(c) => format!("rung {}/{} ({})", c.index + 1, c.of, c.gguf_file()),
        None => "the base rung".into(),
    }
}

/// What the spawned half of a climb needs, owned.
struct ClimbJob {
    key: Key,
    phase: Arc<watch::Sender<Phase>>,
    /// The new container's generation, claimed by [`ClimbTicket::start`].
    generation: u64,
    old_name: String,
    old_stop_timeout: Duration,
    spec: StartSpec,
}

/// Settles an entry the climb task left `starting` — a panic, or the task
/// dropped with the runtime — the way a failed start does, so a model is never
/// wedged in a climb nothing will finish.
struct Unsettled {
    reg: Arc<Registry>,
    key: Key,
    phase: Arc<watch::Sender<Phase>>,
    generation: u64,
    rung: Option<RungPos>,
    armed: bool,
}

impl Drop for Unsettled {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (phase, generation) = (&self.phase, self.generation);
        if self
            .reg
            .forget_if(&self.key, |e| owned_by(e, phase, generation))
        {
            self.phase.send_replace(Phase::ClimbFailed {
                rung: self.rung,
                cause: "the climb ended before its rung was started".into(),
            });
        }
    }
}

/// The spawned start of a climb ([`ClimbTicket::start`]). Awaiting it is
/// optional — the climb finishes whether or not anyone does, so a trigger
/// whose client hung up cannot abandon a climb others are waiting on.
#[derive(Debug)]
pub struct ClimbRun {
    class: crate::runtime::Class,
    model_id: String,
    task: tokio::task::JoinHandle<Result<(), RuntimeError>>,
}

impl ClimbRun {
    /// How the climb ended: `Ok` once the new rung is ready; `Aborted` when a
    /// stop won against it; the start's own error when the rung would not
    /// come up.
    pub async fn finish(self) -> Result<(), RuntimeError> {
        match self.task.await {
            Ok(outcome) => outcome,
            Err(e) => Err(RuntimeError::ClimbFailed {
                class: self.class,
                model_id: self.model_id,
                message: format!("the climb task ended abnormally: {e}"),
            }),
        }
    }
}

impl ClimbTicket {
    /// The running container's host port — where the drain asks `/slots`.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The rung the container being replaced runs ([`Entry::charge`]).
    pub fn running(&self) -> Option<&RungCharge> {
        self.running.as_ref()
    }

    /// Is `e` the entry this ticket marked, still marked and not yet started?
    fn owns(&self, e: &Entry) -> bool {
        Arc::ptr_eq(&e.phase, &self.phase)
            && e.generation == self.generation
            && e.state == RuntimeState::Ready
            && e.climb.as_ref().is_some_and(|m| !m.claimed)
    }

    /// The rung to climb to — possibly raised by another trigger since the
    /// mark ([`Marked::Joined`]).
    pub fn to(&self) -> RungPos {
        let map = self.reg.map();
        map.get(&self.key)
            .filter(|e| self.owns(e))
            .and_then(|e| e.climb.as_ref())
            .map_or(self.to, |m| m.to)
    }

    /// Say whether the climb's admission is queued for the admission gate
    /// right now — set just before it asks for the gate, cleared once it
    /// holds it. Another admission holding the gate while it waits for a
    /// busy model yields it only then (§12 entry 71).
    pub fn set_at_gate(&self, queued: bool) {
        let mut map = self.reg.map();
        if let Some(e) = map.get_mut(&self.key) {
            if self.owns(e) {
                if let Some(m) = e.climb.as_mut() {
                    m.at_gate = queued;
                }
            }
        }
    }

    /// Say what the climb is doing, for the status surfaces.
    pub fn set_stage(&self, stage: impl Into<String>) {
        let mut map = self.reg.map();
        if let Some(e) = map.get_mut(&self.key) {
            if self.owns(e) {
                if let Some(m) = e.climb.as_mut() {
                    m.stage = stage.into();
                }
            }
        }
    }

    /// Wait until no send is in flight on the running container (ladder
    /// design §3.4 step 2, §12 entry 10), or `until`.
    ///
    /// Only lmgw's own sends: a client on the container's published port, and
    /// the moment a dropped send keeps its slot after lmgw let go, are the
    /// container's `/slots` to tell — the caller asks it next, as eviction
    /// does.
    pub async fn drain(&mut self, until: Option<Instant>) -> Result<(), DrainEnd> {
        let outcome = {
            let wait = self.sends.wait_for(|n| *n == 0);
            match until {
                Some(d) => tokio::time::timeout_at(tokio::time::Instant::from_std(d), wait)
                    .await
                    .map(|outcome| outcome.map(drop)),
                None => Ok(wait.await.map(drop)),
            }
        };
        let Ok(outcome) = outcome else {
            return Err(DrainEnd::Timeout {
                sends: *self.sends.borrow(),
            });
        };
        // The counter dies with the entry: a stop took it.
        outcome.map_err(|_| DrainEnd::Gone)?;
        let map = self.reg.map();
        if map.get(&self.key).is_some_and(|e| self.owns(e)) {
            Ok(())
        } else {
            Err(DrainEnd::Gone)
        }
    }

    /// Claim the new rung's start and spawn it (ladder design §3.4 step 4).
    ///
    /// Synchronous on purpose: the caller does this while it holds the VRAM
    /// admission gate, so the flip — a new generation, `starting`, charged at
    /// `spec`'s rung — is what the next admission measures, and nothing else
    /// can take the memory the old rung is about to free (§12 entry 21). The
    /// entry is never absent: acquirers keep parking on it.
    ///
    /// `None` when the entry is no longer the one this ticket marked — a stop
    /// won against the climb; nothing was started.
    pub fn start(self, spec: StartSpec) -> Option<ClimbRun> {
        match self.start_unless(spec, |_, _| None) {
            ClimbStart::Started(run) => Some(run),
            ClimbStart::Gone | ClimbStart::Refused(_) => None,
        }
    }

    /// [`Self::start`], unless `refuse` — asked under the same map lock hold
    /// that claims the start, so nothing can change what it judged before the
    /// claim lands — names a reason not to: a guest's climb's last look at
    /// who the model belongs to (`ownership`, candidate-aliases §12 entry
    /// 89). Refused, nothing was started and the ticket is dropped, which
    /// clears the mark: the running rung serves on.
    pub(super) fn start_unless(
        mut self,
        spec: StartSpec,
        refuse: impl FnOnce(&Registry, &Entry) -> Option<&'static str>,
    ) -> ClimbStart {
        let charge = spec.runtime.rung_charge();
        let to = spec.runtime.rung;
        let stage = format!(
            "stopping {}",
            self.running
                .as_ref()
                .map_or("the running rung".to_string(), |c| {
                    format!(
                        "rung {}/{} ({})",
                        c.index + 1,
                        c.of,
                        file_name(&c.gguf_path)
                    )
                })
        );
        let generation = {
            let mut map = self.reg.map();
            let Some(e) = map.get_mut(&self.key).filter(|e| self.owns(e)) else {
                return ClimbStart::Gone;
            };
            if let Some(why) = refuse(&self.reg, e) {
                return ClimbStart::Refused(why);
            }
            let generation = next_generation();
            e.generation = generation;
            e.state = RuntimeState::Starting;
            e.host_port = 0;
            e.gate = None;
            e.warnings.clear();
            e.capabilities = None;
            e.charge = charge;
            e.started_at = Instant::now();
            e.stop_timeout = spec.stop_timeout;
            e.sends.send_replace(0);
            if let Some(m) = e.climb.as_mut() {
                m.claimed = true;
                if let Some(to) = to {
                    m.to = to;
                }
                m.stage = stage;
            }
            generation
        };
        self.started = true;
        let (class, model_id) = (self.key.0, self.key.1.clone());
        let job = ClimbJob {
            key: self.key.clone(),
            phase: Arc::clone(&self.phase),
            generation,
            old_name: self.container_name.clone(),
            old_stop_timeout: self.stop_timeout,
            spec,
        };
        let task = tokio::spawn(Arc::clone(&self.reg).run_climb(job));
        ClimbStart::Started(ClimbRun {
            class,
            model_id,
            task,
        })
    }
}

/// What [`ClimbTicket::start_unless`] came to.
#[derive(Debug)]
pub enum ClimbStart {
    /// The new rung's start is claimed and running.
    Started(ClimbRun),
    /// The entry is no longer the one the ticket marked — a stop won.
    Gone,
    /// The caller's condition said no, and why (a clause about the model).
    Refused(&'static str),
}

impl Drop for ClimbTicket {
    fn drop(&mut self) {
        if self.started {
            return;
        }
        let cleared = {
            let mut map = self.reg.map();
            match map.get_mut(&self.key) {
                Some(e) if self.owns(e) => {
                    e.climb = None;
                    true
                }
                _ => false,
            }
        };
        // The running rung keeps serving: whoever parked on the mark takes
        // its claim (or its send) there and judges again.
        if cleared {
            self.phase.send_replace(Phase::Ready);
        }
    }
}
