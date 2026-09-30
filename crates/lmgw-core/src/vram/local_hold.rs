//! `LocalHold`: a request's claim on a local container — its acquisition,
//! recovery and send policy. Unrelated to the GPU-hold switch
//! (`settings.hold`), which pauses local models crate-wide.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::AdmissionPolicy;
use crate::runtime::descriptor::{model_runtime, RungPos};
use crate::runtime::registry::{AcquireGuard, ClaimStatus, Origin, RuntimeError, SendGuard};
use crate::runtime::Class;
use crate::state::SharedState;

use super::background::Restart;
use super::Target;
/// Held for the life of a request against a local model: the registry's
/// in-flight claim, plus where that model answers.
///
/// Dropping it releases the claim, which is what makes the model evictable and
/// reapable again.
///
/// **Loops hold one of these for the whole loop, not one per turn.** The
/// in-process turn runners — `/v1/responses`, the dashboard's agent chat,
/// quickdoc's extraction and golden-query generation — drive many turns against
/// one model, with tool calls in between. Admitting per turn would let another
/// admission evict the model in one of those gaps and make every later turn pay
/// for a reload, and it would leave the model's in-flight count at zero (and its
/// LRU stamp unset) for most of the run, which is exactly the shape `evict_one`
/// ranks as the coldest victim on the GPU. So the runner takes the hold once,
/// keeps it in its own struct, and drops it when the loop ends.
///
/// A hold is a counter, not the serializing gate: holding one never blocks
/// another admission from running. An ingest that holds its chat model while its
/// embedding batches admit the aux model is the normal case. If the two really
/// cannot coexist on the GPU, the second admission finds nothing evictable and
/// ends at the named [`GatewayError::VramQueueTimeout`] — a visible refusal that
/// says what is holding the memory, never a deadlock.
pub struct LocalHold {
    /// Behind a lock because a hold can outlive the container it was taken
    /// on: the dead-endpoint recovery below swaps in a fresh claim on a
    /// freshly started container, and every holder of a `&LocalHold` — a
    /// `&self` turn runner, a struct field carried across a whole tool loop —
    /// has to see the new port from that moment on. Locked only for the
    /// microseconds it takes to read or replace the claim, never across an
    /// await.
    pub(super) guard: std::sync::Mutex<AcquireGuard>,
    /// What a re-acquire needs, so recovery goes back through
    /// [`VramScheduler::admit_local`](crate::vram::VramScheduler::admit_local) — the same admit path the first acquire
    /// took, arbitration included — instead of a private shortcut into the
    /// registry.
    pub(super) state: SharedState,
    pub(super) target: Target,
    pub(super) alias: String,
    /// One recovery at a time per hold. A runner can have several requests in
    /// flight against one hold (quickdoc's batches, a client streaming while
    /// a tool call runs); they must not each stop and restart the container.
    pub(super) recovering: tokio::sync::Mutex<()>,
    /// What admission knew about the request this hold serves — set once by
    /// the gate for a request that may fall back ([`AdmissionPolicy`]),
    /// `None` for pinned and direct callers. A climb in the middle of the
    /// request reads it (ladder design §12 entries 8–9).
    pub(super) policy: Option<AdmissionPolicy>,
    /// Whose claim this is (candidate-aliases design §4.4): a background
    /// candidate alias's is [`Origin::Background`] — a guest, which never
    /// climbs the owner's model ([`climb`](crate::vram::climb)) — every other one the owner's.
    /// Set at the claim, never changed: the one place the "background" fact
    /// lives for the life of the request.
    pub(super) origin: Origin,
    /// How the model is brought back when its container goes away under
    /// this hold ([`Restart`]): admission for every hold [`admit`](crate::vram::admit) gives, the
    /// guest's rule or never for the holds [`join`](crate::vram::join) and [`start_background`](crate::vram::start_background)
    /// give.
    pub(super) restart: Restart,
}

impl LocalHold {
    /// The base URL of the container this hold is on — what the route's
    /// `upstream.base_url` is overwritten with before the egress adapter
    /// builds its URL (§5).
    ///
    /// `/v1`-suffixed, unlike [`AcquireGuard::endpoint`]: `Upstream::base()`
    /// is the API root everywhere in this crate (`{base}/chat/completions`,
    /// `{base}/embeddings`, …), and the registry's endpoint is the container
    /// root. One suffix, applied in one place.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port())
    }

    /// The container's host port, for callers that need the origin rather than
    /// the API root.
    pub fn port(&self) -> u16 {
        self.guard().port()
    }

    /// The container this hold's claim is on ([`AcquireGuard::generation`]).
    pub fn generation(&self) -> u64 {
        self.guard().generation()
    }

    /// Which container an attempt goes to: its port and its generation, read
    /// together. What a send remembers so a failure can be recovered for
    /// exactly the container that failed ([`Self::recover`]).
    pub fn attempt(&self) -> (u16, u64) {
        let g = self.guard();
        (g.port(), g.generation())
    }

    /// [`Self::attempt`] together with that container's start facts, all
    /// three read under one lock of the claim (review finding 6). A send
    /// judged on facts read apart from its attempt could be judged on one
    /// container's rung and sent to another's port, when a concurrent
    /// recovery moves a shared hold in between.
    pub fn attempt_with_facts(&self) -> ((u16, u64), Option<Arc<crate::gate::facts::GateFacts>>) {
        let g = self.guard();
        ((g.port(), g.generation()), g.gate_facts().cloned())
    }

    /// The ladder rung the container this hold is on runs (its start facts'
    /// [`GateFacts::rung`](crate::gate::facts::GateFacts::rung)); `None`
    /// without a ladder.
    pub fn rung(&self) -> Option<RungPos> {
        self.guard().gate_facts().and_then(|f| f.rung)
    }

    /// What admission knew about the request this hold serves, when it may
    /// fall back ([`AdmissionPolicy`]).
    pub fn policy(&self) -> Option<&AdmissionPolicy> {
        self.policy.as_ref()
    }

    /// Whose claim this is ([`Self::origin`]'s field doc).
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// How this hold's model comes back if its container goes away
    /// ([`Restart`]).
    pub fn restart(&self) -> Restart {
        self.restart
    }

    /// The name the client asked for — the alias the request's fallback is
    /// looked up by ([`Snapshot::request_fallback`](crate::config::Snapshot::request_fallback)).
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Record the request's policy — the gate's admission, once, before the
    /// hold is handed to anything that could climb.
    pub(crate) fn set_policy(&mut self, policy: AdmissionPolicy) {
        self.policy = Some(policy);
    }

    /// Count one send on the container this hold is on, for a climb's drain
    /// (ladder design §12 entry 10): hold the returned guard until the send's
    /// response has ended. Refused — with what to do instead — while a climb
    /// is marked or the claim is not on the running container; [`Self::sync`]
    /// and try again.
    pub fn begin_send(&self) -> Result<SendGuard, ClaimStatus> {
        let g = self.guard();
        self.state.runtime().begin_send(&g)
    }

    /// Bring the claim up to date with the container its model runs now
    /// (ladder design §12 entry 10) — before each send on a ladder row:
    ///
    /// - the claim's container is running: nothing to do;
    /// - a climb is replacing it: wait for it to settle, like an acquirer;
    /// - a climb replaced it: the entry carried the claim, so it is moved to
    ///   the new container **in place** — the new port, the new rung's facts
    ///   — with no count change and no new acquire, and a tool loop's idle
    ///   claim is never seen idle by the reaper in between;
    /// - the model is gone (stopped, or a climb failed to start): the claim
    ///   is re-admitted at the **base** rung ([`model_runtime`]), through the
    ///   same admission as any start, and swapped in place like a
    ///   dead-container recovery. Every start is the base (§3.5).
    ///
    /// Waits are bounded by what they wait for (a climb's own budget and load
    /// timeout); a caller with a deadline of its own bounds this call with it.
    pub async fn sync(&self) -> Result<(), GatewayError> {
        loop {
            match self.claim_status() {
                ClaimStatus::Current => return Ok(()),
                ClaimStatus::Climbing(wait) => wait.settled().await,
                ClaimStatus::Moved {
                    port,
                    gate,
                    generation,
                } => self.guard().retarget(port, gate, generation),
                ClaimStatus::Gone => {
                    let _one_at_a_time = self.recovering.lock().await;
                    // A concurrent sync (or recovery) may have re-admitted
                    // while this one waited for the lock.
                    if !matches!(self.claim_status(), ClaimStatus::Gone) {
                        continue;
                    }
                    return self.readmit_base().await;
                }
            }
        }
    }

    fn claim_status(&self) -> ClaimStatus {
        let g = self.guard();
        self.state.runtime().claim_status(&g)
    }

    /// Take a fresh claim at the base rung through [`VramScheduler::admit_local`](crate::vram::VramScheduler::admit_local)
    /// and swap it in for the one this hold carries. Callers hold
    /// [`Self::recovering`].
    ///
    /// Only a [`Restart::Admit`] hold is admitted; a candidate's hold goes by
    /// its own rule ([`Self::restart_candidate`]), and when that says no the
    /// answer is `CandidateLost`, never an admission (candidate-aliases §12
    /// entry 46: the alias never starts an alternate).
    async fn readmit_base(&self) -> Result<(), GatewayError> {
        let fresh = match self.restart {
            Restart::Admit => self.readmit_admission().await?,
            Restart::Background | Restart::No => self.restart_candidate().await?,
        };
        let fresh = fresh.into_guard();
        let mut slot = self.guard();
        let old = std::mem::replace(&mut *slot, fresh);
        drop(slot);
        // Explicit, after the lock is released: the drop releases the *old*
        // entry's claim, which takes the registry map lock.
        drop(old);
        Ok(())
    }

    /// [`Self::readmit_base`] for a [`Restart::Admit`] hold: the same
    /// admission as any start, arbitration included.
    async fn readmit_admission(&self) -> Result<LocalHold, GatewayError> {
        let (class, model_id) = (self.target.class, self.target.model_id.as_str());
        let snap = self.state.snapshot();
        let runtime =
            model_runtime(&snap, class, model_id).ok_or_else(|| GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: format!(
                    "{class} model '{model_id}' is no longer configured, so its container could \
                     not be started again"
                ),
            })?;
        self.state
            .vram
            .admit_plain(&self.state, &snap, &self.target, &runtime, &self.alias)
            .await?
            .ok_or_else(|| GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: format!("no container could be acquired for {class} model '{model_id}'"),
            })
    }

    pub fn class(&self) -> Class {
        self.target.class
    }

    pub fn model_id(&self) -> &str {
        &self.target.model_id
    }

    /// What the request gate reads about the container this hold is on — the
    /// row it was **started** with ([`crate::gate::facts::GateFacts`]), not the
    /// row as it is now. Read through the claim on every call: after a
    /// dead-container [`Self::recover`] the hold is on a new start, and its
    /// facts are that start's.
    pub fn gate_facts(&self) -> Option<Arc<crate::gate::facts::GateFacts>> {
        self.guard().gate_facts().cloned()
    }

    pub(super) fn guard(&self) -> std::sync::MutexGuard<'_, AcquireGuard> {
        // Poisoning is recovered from rather than propagated, for the same
        // reason `Registry::map` does it: the only way to poison this is a
        // panicking `Drop`, and wedging a model until restart over it is the
        // worse failure.
        self.guard.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record that the container did not serve the request this hold was taken
    /// for — see [`AcquireGuard::mark_failed`]. Without it the release stamps
    /// `last_used`, and a dead container looks freshly used to the idle reaper
    /// and the LRU for a full idle period after every retry against it.
    pub fn mark_failed(&self) {
        self.guard().mark_failed();
    }

    /// The dead-container recovery (§3.2's "a connect-refused on forward
    /// retries once through `acquire`").
    ///
    /// The window is real and cannot be closed by the registry lock: lmgw's
    /// map says `ready` because the container *was* up when the claim was
    /// taken, and between then and the forward the container can die — an OOM
    /// kill, a `podman stop` from a shell, a crash of llama-server itself.
    /// The claim that was supposed to protect the endpoint is then a claim on
    /// a corpse, and every request that inherits it fails the same way, for
    /// as long as the entry is not reaped (and its `in_flight` count is
    /// exactly what stops the reaper from taking it).
    ///
    /// So: mark the claim failed (its `last_used` stamp would be a lie),
    /// **force** the entry down — force is honest here, the container is dead
    /// and the only in-flight claims left are the ones failing against it —
    /// and take a fresh one through the same admit path. The new claim
    /// replaces the old one *in place*, so callers holding `&LocalHold` see
    /// the new endpoint; the old guard's release is identity-checked against
    /// the entry it was taken on, so dropping it cannot decrement the
    /// successor.
    ///
    /// `attempt` is the claim the failed attempt used — its port and its
    /// container's generation ([`Self::attempt`]). If the hold is already on
    /// another container, a concurrent request recovered first and this
    /// returns without stopping anything — one death, one restart.
    ///
    /// **Only the container that failed is stopped** (ladder design §12
    /// entry 20): the stop is generation-checked, so when the model was
    /// restarted since — an override stop and a fresh request, a climb — the
    /// newer container that runs under the key now is left alone, and the
    /// claim simply joins it. Before this, a stale hold
    /// force-stopped whatever ran under the key, killing the requests of the
    /// container that had already replaced its dead one.
    ///
    /// The restart is [`Self::sync`]'s: the claim follows whatever the model
    /// runs by then — a climb's new rung, a newer container, or a fresh
    /// admission at the base when it is gone (a recovery is judged on the base,
    /// never on the rung that died, §3.5).
    pub(crate) async fn recover(&self, attempt: (u16, u64)) -> Result<(), GatewayError> {
        {
            let _one_at_a_time = self.recovering.lock().await;
            if self.attempt() != attempt {
                return Ok(());
            }
            let (class, model_id) = (self.target.class, self.target.model_id.as_str());
            let (stale_port, generation) = attempt;
            // Only a claim still on the running container, with no climb
            // replacing it, has a container of its own to stop: otherwise the
            // model already runs (or is bringing up) a newer one, and the
            // sync below follows it there.
            if matches!(self.claim_status(), ClaimStatus::Current) {
                self.mark_failed();
                // A guest never stops the owner's model: `candidate_lost`,
                // and the gate picks again (candidate-aliases §12 entry 90).
                match self.stop_dead(stale_port, generation).await? {
                    Ok(()) => {}
                    Err(RuntimeError::Moved { why, .. }) => {
                        tracing::info!(
                            "not stopping {class} model '{model_id}': it {why}; following the \
                             container that runs now"
                        );
                    }
                    // Not fatal: `stop` drops the entry whatever podman
                    // answered, so the sync below still starts fresh
                    // (`--replace` collects whatever is left under the name).
                    Err(e) => tracing::warn!(
                        "stopping the dead container for {class} model '{model_id}': {e}"
                    ),
                }
            }
        }
        self.sync().await
    }

    /// Take the claim out of a hold that is only a carrier for it — the
    /// recovery path's way of moving a freshly admitted claim into an existing
    /// hold instead of handing the caller a second hold it has nowhere to put.
    fn into_guard(self) -> AcquireGuard {
        self.guard.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// Send one request that may be served by a local container, with the §3.2
/// dead-container retry.
///
/// **The one place that policy lives.** Every held route's egress send goes
/// through here — the chat/completions paths, the in-process turn helpers, the
/// embeddings/rerank/tokenize calls, the audio routes — so "what happens when
/// the container we hold a claim on is dead" is answered once instead of
/// thirteen times, and a new send site inherits it by construction. (The
/// policy itself is [`retry_dead_container`], which the request gate's prompt
/// count shares for its own calls to the same container.)
///
/// `build` is handed the route to send against, which for a held route is the
/// caller's route with `upstream.base_url` refreshed from the hold. That
/// refresh is what makes the retry work at all (a restarted container is on a
/// new ephemeral port, so the request has to be rebuilt, not replayed) — and
/// it also means a long-lived runner that cached its route before a recovery
/// still sends to the right place.
///
/// **What is retried, and what deliberately is not.** Only a transport failure
/// of the *send* — the connect, the request write, the wait for response
/// headers. That is the shape a dead endpoint produces. An HTTP error is the
/// container answering and is passed straight back; and a stream that dies
/// mid-body has already had part of its answer relayed to the client, so it
/// cannot be replayed and is not attempted: the initial connect of a streaming
/// send is covered here, the body is not (§9's "the connect-refused retry
/// path").
///
/// `timeout` bounds the wait for headers, as the call sites' own
/// `tokio::time::timeout` did; `None` leaves the bound to whatever the builder
/// carries.
pub async fn send_local<F>(
    hold: Option<&LocalHold>,
    route: &Route,
    timeout: Option<Duration>,
    build: F,
) -> Result<reqwest::Response, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    let Some(hold) = hold else {
        // No local container in this route's future: one send, no recovery,
        // exactly what the call site did before.
        return send_built(build(route)?, timeout).await;
    };

    let build = &build;
    retry_dead_container(hold, move || async move {
        send_built(build(&on_hold(route, hold))?, timeout).await
    })
    .await
}

/// [`send_local`]'s dead-container policy, for any call against the container
/// a hold is on — not only a forward. The request gate's prompt count
/// (`/apply-template`, `/tokenize` — unified-KV design §3.3 step 2) is the
/// other caller: it talks to the same container a moment before the forward
/// does, so a container that died in between has to be treated exactly the
/// same way, or the count would invent a second answer to "what happens when
/// the container is dead" (recover once, then fail, never a loop).
///
/// `call` is invoked once, and a second time only after a
/// [`GatewayError::Transport`] and a successful recovery. It must read the
/// hold's port (or endpoint) itself on every invocation: the recovery swaps
/// the claim in place, so the retry has to go to the fresh container, never
/// replay the dead port. Any other error — including an HTTP error status,
/// which is the container answering — is returned as it is.
pub(crate) async fn retry_dead_container<T, F, Fut>(
    hold: &LocalHold,
    call: F,
) -> Result<T, GatewayError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, GatewayError>>,
{
    let attempted = hold.attempt();
    let first = call().await;
    let Err(GatewayError::Transport(why)) = first else {
        return first;
    };

    if let Err(e) = hold.recover(attempted).await {
        // A candidate this request may not bring back is the gate's to answer
        // — by picking again — not a restart that failed.
        if matches!(e, GatewayError::CandidateLost { .. }) {
            return Err(e);
        }
        return Err(GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message: format!(
                "{} model '{}' stopped answering ({why}) and could not be restarted: {e}",
                hold.class(),
                hold.model_id()
            ),
        });
    }
    let retried = call().await;
    if matches!(retried, Err(GatewayError::Transport(_))) {
        // Twice in a row on two different containers: whatever is wrong is not
        // this container, so the claim must not stamp the model as freshly
        // used on its way out.
        hold.mark_failed();
    }
    retried
}

/// The route as it looks against the container this hold is on (§5).
fn on_hold(route: &Route, hold: &LocalHold) -> Route {
    let mut r = route.clone();
    r.upstream.base_url = hold.endpoint();
    r
}

/// One send, its wait for response headers bounded by `timeout` — the
/// building block of [`send_local`], and of the gate's ladder send
/// ([`crate::gate::send`]), which runs its own dead-container policy.
pub(crate) async fn send_built(
    rb: reqwest::RequestBuilder,
    timeout: Option<Duration>,
) -> Result<reqwest::Response, GatewayError> {
    match timeout {
        Some(d) => tokio::time::timeout(d, rb.send())
            .await
            .map_err(|_| GatewayError::Timeout)?
            .map_err(GatewayError::from),
        None => rb.send().await.map_err(GatewayError::from),
    }
}

impl std::fmt::Debug for LocalHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalHold")
            .field("class", &self.class())
            .field("model_id", &self.model_id())
            .field("endpoint", &self.endpoint())
            .finish()
    }
}
