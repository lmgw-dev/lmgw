//! The unified-KV pool ledger (unified-KV design §3.3 steps 3–6): one token
//! ledger per guarded chat model, so that the requests lmgw forwards to a
//! shared KV pool never add up to more than the pool holds.
//!
//! **Why it exists.** With unified KV every slot of a llama-server draws its
//! cells from one pool, and when a decode finds no free cell the server
//! first clears idle slots, then halves the batch, and at batch size 1 sends
//! "Context size has been exceeded." to **every processing slot** and clears
//! them (design §2.1 fact 3, `server-context.cpp:3688-3730`). Measured on
//! this machine's image (2 slots, pool 4096): both in-flight requests die
//! together after about five seconds — a streaming client gets its 200 and
//! then one SSE error event, a unary one a 500. The owner's conversation is
//! killed by somebody else's long prompt. The ledger makes that arithmetic
//! lmgw's own: a request is only forwarded when its worst case fits beside
//! the worst cases already in flight.
//!
//! **What a reservation is.** `prompt + max output` (§3.3 step 3), counted by
//! the gate's per-send half ([`super::fit`]) on the running server, with the
//! max output already clamped to the row's `n_predict` so the number is a real
//! bound. A slot's KV use peaks at exactly that: llama-server bounds even a
//! speculative draft by the tokens left to predict (`get_n_draft_max`:
//! `min(n_ctx - n_tokens - 2, n_remaining() - 1)`, checked at 171e884), so
//! prompt + generated + 1 + draft never passes prompt + `n_predict`. A
//! request asking for `n` completions (`n_cmpl`) reserves its prompt once and
//! its output `n` times: the children share the parent's prompt cells
//! (`copy_state_to` → `seq_cp`) and each generates its own.
//!
//! **Capacity** is the *running container's* pool
//! ([`super::facts::GateFacts::pool_tokens`]): the numbers it was started
//! with, passed in at every reservation — never the row as it has been
//! edited since (second review, finding 1). A pool whose size cannot be derived is a
//! visible refusal in the caller, never a guess here.
//!
//! **Strictly first in, first out** (§3.3 step 5). A request is granted only
//! when nobody is waiting ahead of it *and* it fits beside what is reserved;
//! a small request never overtakes a waiting large one, so a large request
//! cannot starve. Every release grants from the head for as long as the head
//! fits, and stops at the first that does not.
//!
//! **The wait is bounded** by `vram.queue_timeout_seconds`, the same budget
//! the VRAM admission queue uses (0 keeps its meaning: no limit), and — for
//! an in-process caller with a deadline of its own (MCP sampling, the agent
//! loops' wall clock) — by whatever is left of that deadline, whichever ends
//! first. Either ends in [`GatewayError::KvPoolTimeout`] — 503, code
//! `vram_queue_timeout` — naming the pool's numbers and the queue ahead.
//!
//! **Cancellation and exactly-once release.** A reservation — waiting or
//! granted — is a [`PoolTicket`], and the only way out of the ledger is the
//! ticket's `Drop` (directly, or through the deferred release it hands its
//! entry to). The ticket is created before the first `.await`, so a waiter
//! whose future is dropped (the client hung up) leaves the queue, and if it
//! was the head the next head is re-evaluated at once. A grant that races
//! with that drop is harmless: the grant moves the entry from the queue to
//! the granted set under the one lock, and the ticket's drop removes it from
//! whichever of the two it is in, under the same lock. The oneshot a waiter
//! parks on is only a wake-up call; the ledger's own state is the truth.
//!
//! **Release only once llama-server has let go** (second review, finding 6). A send
//! whose response was read to its normal end ([`PoolTicket::mark_complete`])
//! is released at once: the server finished the task before it closed the
//! response. Every other end — the client disconnected, an lmgw-side timeout
//! or stall, an upstream error mid-stream, any drop that was not a fully read
//! completion — leaves the slot running inside llama-server until it notices:
//! the server only checks a closed connection on its next `should_stop` poll
//! (`HTTP_POLLING_SECONDS = 1` in upstream `server-context.cpp`) plus one
//! ubatch, so for up to a second or more the cells are still in use. Such a
//! reservation is marked **releasing** (still reserved, visible in
//! `kv_pools`) and a spawned task polls the container's own `/slots` — the
//! cheap form, ~2.5 KB (design §12, WP0) — until no more slots are processing
//! than the reservations still in flight on that model account for, then
//! releases it. The container gone or `/slots` unreachable releases at once
//! (there are no cells left to protect, or no way to see them). The wait is
//! bounded by `vram.queue_timeout_seconds` (0 = no limit from it; the route's
//! own request timeout bounds it then, when it has one), and a bound that is
//! hit is logged as a warning, never silent.
//!
//! **No deadlock with VRAM admission** — reasoned, since a test cannot prove
//! an absence:
//! - A request only ever reaches the pool *after* its admission: it holds its
//!   model's registry claim ([`crate::vram::LocalHold`]) while it waits. That
//!   is intended — the model counts as busy while requests queue for its pool,
//!   which is the fair queueing the spec asks for (§4.5), and a model with a
//!   queue is not an eviction victim.
//! - The pool never waits on anything but its own model's releases (and a
//!   waiter ahead leaving). VRAM admission never waits on the pool.
//! - Every granted reservation belongs to a send that is already forwarded —
//!   it waits on llama-server, which answers or times out on its own, never on
//!   lmgw. A releasing one waits on the same server's own slots, bounded as
//!   above. A turn takes at most one lease, and never takes a second while it
//!   holds the first; the in-process loops (`/v1/responses`, agent chat,
//!   batch runs) hold one admission across many turns, but each turn reserves
//!   and releases its own tokens (the per-send half), so a reservation never
//!   lives as long as the hold.
//!
//! So every wait here ends: in a grant once something in flight finishes, in
//! the timeout, or in the client going away. The worst a queued request can
//! do elsewhere is keep its model busy, which delays *another model's* VRAM
//! admission — bounded by that admission's own timeout, and named in its
//! refusal.
//!
//! **Every change is on the live frame** (second review, finding 11): a grant, a
//! release, a deferral and a queue move each push the `vram` frame
//! ([`crate::vram::broadcast`], spawned — never under this ledger's lock), so
//! `kv_pools` never lags what the ledger holds.
//!
//! **What the ledger does not track** (§3.3):
//! - **Idle slots.** They are not reservations: when any task starts,
//!   llama-server saves every idle slot's prompt to its RAM cache and, with
//!   unified KV, clears it from the pool (design §2.1 fact 5).
//! - **Requests sent straight to the container's published port.** They
//!   bypass lmgw and therefore the ledger — a documented limit. (They do show
//!   in `/slots`, so a releasing reservation waits for them too: the
//!   conservative direction.)
//! - **Which slot is whose.** `/slots` has no cheap per-request form (WP0), so
//!   the deferred release compares counts: a granted request that has not
//!   reached the server yet counts as in flight without a processing slot,
//!   and can let a releasing one go a moment early.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::oneshot;

use crate::error::GatewayError;
use crate::runtime::Class;
use crate::state::SharedState;

/// How often a releasing reservation looks at its container's `/slots`.
///
/// A sampling rate, not a bound on anything: the release waits for as long as
/// the server keeps the slots busy (up to the visible bound in the module
/// doc), this only says how often it looks. The same 250 ms the VRAM
/// scheduler polls at, and a quarter of llama-server's own one-second
/// disconnect poll, so a release trails the server by a fraction of what the
/// server itself takes to notice.
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// One request's reservation, split so a refusal can say what it was made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    /// Every prompt token this request puts in the pool (its images at their
    /// per-image bound included).
    pub prompt: u64,
    /// Every output token it may generate: the clamped `max_tokens`, times
    /// the completions it asked for.
    pub max_output: u64,
    /// How many slots it runs in: one per prompt per completion. What a
    /// deferred release compares `/slots` against — a request with `n = 3`
    /// keeps three slots processing, not one.
    pub slots: u64,
}

impl Need {
    pub fn total(&self) -> u64 {
        self.prompt.saturating_add(self.max_output)
    }
}

/// Every guarded model's pool, keyed by chat model id. Lives on
/// [`crate::state::AppState::kv_pools`]; an entry exists only while its model
/// has a reservation in flight, releasing or waiting, which is also exactly
/// what the status surfaces list.
#[derive(Default)]
pub struct PoolLedger {
    pools: Mutex<HashMap<String, Pool>>,
    next_id: AtomicU64,
}

/// One model's pool.
struct Pool {
    capacity: u64,
    reserved: u64,
    granted: Vec<Granted>,
    queue: VecDeque<Waiting>,
}

struct Granted {
    id: u64,
    tokens: u64,
    slots: u64,
    /// The send ended without a normal completion and the deferred release is
    /// waiting for llama-server to let go of its cells (module doc). Still
    /// reserved; no longer "in flight".
    releasing: bool,
}

struct Waiting {
    id: u64,
    tokens: u64,
    slots: u64,
    alias: String,
    since: Instant,
    /// Taken when the waiter is woken; `None` afterwards.
    wake: Option<oneshot::Sender<Wake>>,
}

/// Why a waiter was woken.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// It is in the granted set now.
    Granted,
    /// The pool shrank under it (the model was restarted with a smaller pool
    /// while it waited) and it can never fit, however long it waits — refused
    /// rather than left to run out its timeout (or forever, with the timeout
    /// at 0).
    NeverFits { capacity: u64 },
}

impl Pool {
    fn new(capacity: u64) -> Self {
        Self {
            capacity,
            reserved: 0,
            granted: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    fn grant(&mut self, id: u64, tokens: u64, slots: u64) {
        self.reserved = self.reserved.saturating_add(tokens);
        self.granted.push(Granted {
            id,
            tokens,
            slots,
            releasing: false,
        });
    }

    /// Grant from the head of the queue for as long as the head fits, and stop
    /// at the first that does not — the strict FIFO (§3.3 step 5). Returns
    /// whether anything changed.
    fn grant_from_head(&mut self) -> bool {
        let mut changed = false;
        while let Some(head) = self.queue.front() {
            if head.tokens > self.capacity {
                let mut w = self.queue.pop_front().expect("front exists");
                if let Some(tx) = w.wake.take() {
                    let _ = tx.send(Wake::NeverFits {
                        capacity: self.capacity,
                    });
                }
                changed = true;
                continue;
            }
            if self.reserved.saturating_add(head.tokens) > self.capacity {
                break;
            }
            let mut w = self.queue.pop_front().expect("front exists");
            self.grant(w.id, w.tokens, w.slots);
            // A failed send means the waiter's future is already gone; its
            // ticket's drop finds the entry in `granted` and releases it.
            if let Some(tx) = w.wake.take() {
                let _ = tx.send(Wake::Granted);
            }
            changed = true;
        }
        changed
    }

    /// Take `id` out of the pool wherever it is. Returns whether anything
    /// changed (the entry was there, or its leaving let others through).
    fn forget(&mut self, id: u64) -> bool {
        let mut changed = false;
        if let Some(i) = self.queue.iter().position(|w| w.id == id) {
            self.queue.remove(i);
            changed = true;
        } else if let Some(i) = self.granted.iter().position(|g| g.id == id) {
            let g = self.granted.swap_remove(i);
            self.reserved = self.reserved.saturating_sub(g.tokens);
            changed = true;
        }
        // A release frees room, and a waiter leaving can unblock the ones
        // behind it (a large head timing out lets smaller ones through).
        self.grant_from_head() || changed
    }

    /// Mark a granted `id` releasing. `false` when it is not granted (still
    /// waiting, or already gone) or is releasing already — nothing to defer.
    fn begin_release(&mut self, id: u64) -> bool {
        match self.granted.iter_mut().find(|g| g.id == id && !g.releasing) {
            Some(g) => {
                g.releasing = true;
                true
            }
            None => false,
        }
    }

    /// The slots the reservations still in flight (granted, not releasing)
    /// account for — what `/slots` may show processing before a releasing
    /// reservation can be let go.
    fn active_slots(&self) -> u64 {
        self.granted
            .iter()
            .filter(|g| !g.releasing)
            .fold(0u64, |acc, g| acc.saturating_add(g.slots))
    }

    fn is_idle(&self) -> bool {
        self.granted.is_empty() && self.queue.is_empty()
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.queue.iter().position(|w| w.id == id)
    }
}

/// A reservation in one model's pool — waiting or granted. Dropping it is the
/// only way out of the ledger (see the module doc's "exactly-once release"):
/// at once when [`Self::mark_complete`] was called or it was never granted,
/// through the deferred release otherwise.
#[must_use = "dropping the ticket releases the reservation"]
pub struct PoolTicket {
    state: SharedState,
    model: String,
    id: u64,
    /// The send's response was read to its normal end (module doc).
    complete: bool,
    /// The route's request timeout — the deferred release's bound when
    /// `vram.queue_timeout_seconds` is 0.
    route_timeout: Option<Duration>,
}

impl PoolTicket {
    /// The send this ticket reserved for ended normally — its response was
    /// read to the end, so llama-server is done with its cells and the drop
    /// releases at once instead of waiting for `/slots` to say so.
    pub(crate) fn mark_complete(&mut self) {
        self.complete = true;
    }
}

impl Drop for PoolTicket {
    fn drop(&mut self) {
        // No runtime (a drop during teardown) means nothing can poll: release
        // now. There is no request left to protect either.
        let rt = tokio::runtime::Handle::try_current().ok();
        let defer = !self.complete && rt.is_some();
        match self.state.kv_pools.leave(&self.model, self.id, defer) {
            Left::Nothing => {}
            Left::Released => crate::vram::broadcast(&self.state),
            Left::Deferred => {
                crate::vram::broadcast(&self.state);
                let releasing = Releasing {
                    state: self.state.clone(),
                    model: std::mem::take(&mut self.model),
                    id: self.id,
                };
                let bound = settle_bound(&self.state, self.route_timeout);
                if let Some(rt) = rt {
                    rt.spawn(settle(releasing, bound));
                }
            }
        }
    }
}

impl std::fmt::Debug for PoolTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolTicket")
            .field("model", &self.model)
            .field("id", &self.id)
            .field("complete", &self.complete)
            .finish()
    }
}

/// What [`PoolLedger::leave`] did.
enum Left {
    /// The entry was not there (already released) — nothing changed.
    Nothing,
    /// It is gone from the ledger.
    Released,
    /// It is marked releasing; the caller starts the deferred release.
    Deferred,
}

/// A releasing reservation, owned by the task that settles it. Its drop is
/// the release — so a settle task that is aborted (runtime shutdown) still
/// takes its entry out rather than leaving it reserved forever.
struct Releasing {
    state: SharedState,
    model: String,
    id: u64,
}

impl Drop for Releasing {
    fn drop(&mut self) {
        if self.state.kv_pools.forget(&self.model, self.id) {
            crate::vram::broadcast(&self.state);
        }
    }
}

/// How long a releasing reservation may wait for its container's slots:
/// `vram.queue_timeout_seconds` (read now, like the queue wait reads it), or
/// with that at 0 — "no limit" — the route's own request timeout, the bound
/// the send itself had. `None` only when both are unlimited; a guarded row's
/// slots end on their own anyway (`n_predict` bounds every generation).
fn settle_bound(state: &SharedState, route_timeout: Option<Duration>) -> Option<Duration> {
    let secs = state.snapshot().settings.vram.queue_timeout_seconds;
    if secs > 0 {
        Some(Duration::from_secs(secs))
    } else {
        route_timeout
    }
}

/// The deferred release (module doc): poll the model's container until no
/// more slots are processing than the reservations still in flight account
/// for, then drop `releasing`.
async fn settle(releasing: Releasing, bound: Option<Duration>) {
    let (state, model) = (&releasing.state, releasing.model.as_str());
    let started = Instant::now();
    // "In flight" is read *before* the probe: a request granted while the
    // probe is out can only add processing slots, never excuse ours. (No pool
    // at all cannot happen while this entry is in it; it would end the loop.)
    while let Some(active) = state.kv_pools.active_slots(model) {
        let Some(port) = state.runtime().ready_port(Class::Chat, model) else {
            // Stopped, restarting or gone: its cells went with it.
            break;
        };
        let busy =
            match crate::vram::busy_slots(&state.http, port, crate::vram::CONTROL_TIMEOUT).await {
                Some(busy) => busy as u64,
                // No `/slots` to read: nothing to wait on that lmgw could see.
                None => break,
            };
        if busy <= active {
            break;
        }
        let waited = started.elapsed();
        let pause = match bound {
            Some(b) if waited >= b => {
                tracing::warn!(
                    "'{model}': a request that ended without a normal completion kept its \
                     shared-KV reservation for {waited:?} while llama-server still reported \
                     {busy} processing slot(s) against {active} slot(s) lmgw has in flight — \
                     releasing it at the bound (vram.queue_timeout_seconds, or the route's \
                     request timeout when that is 0); a request admitted from here on may \
                     find those cells still in use"
                );
                break;
            }
            Some(b) => SETTLE_POLL.min(b - waited),
            None => SETTLE_POLL,
        };
        tokio::time::sleep(pause).await;
    }
    drop(releasing);
}

/// One guarded model's pool as the status surfaces show it (`GET /api/vram`,
/// the `vram` frame, `lmgw__status`) — the token-denominated sibling of the
/// VRAM admission queue's [`crate::vram::WaiterView`].
#[derive(Debug, Clone, Serialize)]
pub struct KvPoolView {
    pub model: String,
    /// The pool's size in tokens (KV cells), as the running container was
    /// started with it.
    pub capacity_tokens: u64,
    /// Σ (prompt + max output) of the requests in flight and releasing.
    pub reserved_tokens: u64,
    /// Requests lmgw has forwarded to this pool and not yet seen end.
    pub in_flight: usize,
    /// Requests that ended without a normal completion and still hold their
    /// reservation until llama-server lets go of their slots (module doc).
    pub releasing: usize,
    /// Requests waiting for room, head first.
    pub queue: Vec<KvPoolWaiterView>,
}

/// One request waiting for room in a pool.
#[derive(Debug, Clone, Serialize)]
pub struct KvPoolWaiterView {
    pub position: usize,
    pub alias: String,
    /// Its reservation: prompt + max output.
    pub needs_tokens: u64,
    pub waiting_ms: u64,
}

impl PoolLedger {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pool>> {
        // Recovered rather than propagated, like `LocalHold::guard`: the only
        // way to poison this is a panic while holding it, and wedging every
        // guarded model until restart over it is the worse failure.
        self.pools.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Release `id` from `model`'s pool, wherever it is. Returns whether
    /// anything changed.
    fn forget(&self, model: &str, id: u64) -> bool {
        let mut pools = self.lock();
        let Some(pool) = pools.get_mut(model) else {
            return false;
        };
        let changed = pool.forget(id);
        if pool.is_idle() {
            pools.remove(model);
        }
        changed
    }

    /// A ticket's way out: with `defer`, a granted entry is marked releasing
    /// and stays; anything else (a waiter, or `defer` off) is released now.
    fn leave(&self, model: &str, id: u64, defer: bool) -> Left {
        let mut pools = self.lock();
        let Some(pool) = pools.get_mut(model) else {
            return Left::Nothing;
        };
        if defer && pool.begin_release(id) {
            return Left::Deferred;
        }
        let changed = pool.forget(id);
        if pool.is_idle() {
            pools.remove(model);
        }
        if changed {
            Left::Released
        } else {
            Left::Nothing
        }
    }

    /// [`Pool::active_slots`] of `model`'s pool, `None` when it has none.
    fn active_slots(&self, model: &str) -> Option<u64> {
        self.lock().get(model).map(Pool::active_slots)
    }

    /// Every pool with activity, by model id.
    pub fn view(&self) -> Vec<KvPoolView> {
        let pools = self.lock();
        let mut out: Vec<KvPoolView> = pools
            .iter()
            .map(|(model, p)| {
                let releasing = p.granted.iter().filter(|g| g.releasing).count();
                KvPoolView {
                    model: model.clone(),
                    capacity_tokens: p.capacity,
                    reserved_tokens: p.reserved,
                    in_flight: p.granted.len() - releasing,
                    releasing,
                    queue: p
                        .queue
                        .iter()
                        .enumerate()
                        .map(|(i, w)| KvPoolWaiterView {
                            position: i + 1,
                            alias: w.alias.clone(),
                            needs_tokens: w.tokens,
                            waiting_ms: w.since.elapsed().as_millis() as u64,
                        })
                        .collect(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }
}

/// What one reservation asks the ledger for, and on what terms.
pub struct Ask<'a> {
    pub model: &'a str,
    /// Only for the queue view.
    pub alias: &'a str,
    /// The running container's pool ([`super::facts::GateFacts::pool_tokens`]).
    pub capacity: u64,
    pub need: Need,
    /// An in-process caller's own deadline (MCP sampling, the agent loops),
    /// which the wait must not outlast — second review, finding 7.
    pub deadline: Option<Instant>,
    /// The route's request timeout, kept on the ticket for the deferred
    /// release's bound.
    pub route_timeout: Option<Duration>,
}

/// Reserve `ask.need` in `ask.model`'s pool, waiting in its FIFO when it does
/// not fit now (§3.3 steps 5–6).
///
/// A request larger than the whole pool is refused at once as
/// [`GatewayError::ContextExceeded`] — it could never be granted, and a
/// single-sequence request cannot get here that way (the per-request limit is
/// never above the pool), so this is a request asking for several prompts or
/// completions at once.
pub async fn reserve(state: &SharedState, ask: Ask<'_>) -> Result<PoolTicket, GatewayError> {
    let Ask {
        model,
        alias,
        capacity,
        need,
        deadline,
        route_timeout,
    } = ask;
    let tokens = need.total();
    let too_large = |limit: u64| GatewayError::ContextExceeded {
        model: model.to_string(),
        prompt_tokens: need.prompt,
        max_output: Some(need.max_output),
        limit,
        top_rung: None,
    };
    if tokens > capacity {
        return Err(too_large(capacity));
    }

    let ledger = &state.kv_pools;
    let id = ledger.next_id.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let ticket = |complete| PoolTicket {
        state: state.clone(),
        model: model.to_string(),
        id,
        complete,
        route_timeout,
    };
    let mut rx = {
        let mut pools = ledger.lock();
        let pool = pools
            .entry(model.to_string())
            .or_insert_with(|| Pool::new(capacity));
        // The capacity is the running container's, handed in at every
        // reservation: a restart onto a larger pool lets whoever is already
        // queued through before this request is judged against the queue.
        pool.capacity = capacity;
        pool.grant_from_head();
        if pool.queue.is_empty() && pool.reserved.saturating_add(tokens) <= capacity {
            pool.grant(id, tokens, need.slots);
            drop(pools);
            // A grant moves `reserved` and `in_flight`: on the frame now, not
            // at the next queue change (second review, finding 11).
            crate::vram::broadcast(state);
            return Ok(ticket(false));
        }
        let (tx, rx) = oneshot::channel();
        pool.queue.push_back(Waiting {
            id,
            tokens,
            slots: need.slots,
            alias: alias.to_string(),
            since: started,
            wake: Some(tx),
        });
        rx
    };
    // Before the first await: from here on, however this future ends —
    // granted, refused, timed out or dropped — the ticket's drop is what
    // takes the entry out again.
    let ticket = ticket(false);
    // A queue that forms is visible while it exists, not once it is over —
    // the same rule the VRAM admission queue follows. Also announces whatever
    // the `grant_from_head` above let through.
    crate::vram::broadcast(state);

    let secs = state.snapshot().settings.vram.queue_timeout_seconds;
    let budget = (secs > 0).then(|| Duration::from_secs(secs));
    let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let wait = match (budget, left) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let woke = match wait {
        Some(d) => match tokio::time::timeout(d, &mut rx).await {
            Ok(w) => w.ok(),
            // The grant may have landed in the instant the timer fired: take
            // it rather than refuse a request the pool just made room for.
            Err(_) => rx.try_recv().ok(),
        },
        None => (&mut rx).await.ok(),
    };
    match woke {
        Some(Wake::Granted) => Ok(ticket),
        Some(Wake::NeverFits { capacity }) => Err(too_large(capacity)),
        None => {
            let pools = ledger.lock();
            let (capacity, reserved, in_flight, ahead) = match pools.get(model) {
                Some(p) => (
                    p.capacity,
                    p.reserved,
                    p.granted.len(),
                    p.position(id).unwrap_or(0),
                ),
                None => (capacity, 0, 0, 0),
            };
            drop(pools);
            // `ticket` drops on the way out: the entry leaves the queue and
            // whoever was behind it is re-evaluated.
            Err(GatewayError::KvPoolTimeout {
                model: model.to_string(),
                waited_seconds: started.elapsed().as_secs(),
                needs: tokens,
                capacity,
                reserved,
                in_flight,
                ahead,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(pool: &mut Pool, id: u64, tokens: u64) -> oneshot::Receiver<Wake> {
        let (tx, rx) = oneshot::channel();
        pool.queue.push_back(Waiting {
            id,
            tokens,
            slots: 1,
            alias: "m".into(),
            since: Instant::now(),
            wake: Some(tx),
        });
        rx
    }

    /// A small request behind a large one waits, even when it would fit now
    /// — and is granted in the same pass as the large one once both fit.
    #[test]
    fn the_head_is_never_overtaken() {
        let mut p = Pool::new(64);
        p.grant(1, 40, 1);
        let mut large = waiting(&mut p, 2, 50);
        let mut small = waiting(&mut p, 3, 20);
        assert!(!p.grant_from_head(), "40 + 50 > 64, so nothing moves");
        assert!(
            small.try_recv().is_err(),
            "20 would fit beside 40 — but not ahead of 50"
        );

        assert!(p.forget(1), "the release lets the head through");
        assert_eq!(large.try_recv().unwrap(), Wake::Granted);
        assert!(
            small.try_recv().is_err(),
            "50 + 20 > 64: the small one keeps waiting"
        );
        assert_eq!(p.reserved, 50);

        p.forget(2);
        assert_eq!(small.try_recv().unwrap(), Wake::Granted);
        assert_eq!(p.reserved, 20);
        p.forget(3);
        assert!(p.is_idle());
        assert_eq!(p.reserved, 0);
    }

    /// A waiter that leaves (timed out, client gone) takes nothing with it,
    /// and if it was the head the one behind it is judged at once.
    #[test]
    fn a_head_that_leaves_lets_the_next_through() {
        let mut p = Pool::new(64);
        p.grant(1, 40, 1);
        let _large = waiting(&mut p, 2, 50);
        let mut small = waiting(&mut p, 3, 20);
        assert!(p.forget(2));
        assert_eq!(small.try_recv().unwrap(), Wake::Granted);
        assert_eq!(p.reserved, 60);
    }

    /// A grant whose waiter is already gone is still released exactly once:
    /// by the ticket's forget, from the granted set.
    #[test]
    fn a_grant_that_races_a_drop_is_released_not_leaked() {
        let mut p = Pool::new(64);
        p.grant(1, 60, 1);
        let rx = waiting(&mut p, 2, 30);
        drop(rx); // the client hung up; its ticket drop comes next
        p.forget(1); // …but the grant lands first
        assert_eq!(p.reserved, 30, "granted to a waiter that is gone");
        p.forget(2); // the ticket's drop
        assert_eq!(p.reserved, 0);
        assert!(p.is_idle());
        p.forget(2); // and a second forget is a no-op, never a double release
        assert_eq!(p.reserved, 0);
    }

    /// The model was restarted onto a smaller pool under a waiter: it is
    /// refused instead of waiting for a grant that cannot come, and the queue
    /// moves on.
    #[test]
    fn a_waiter_larger_than_the_pool_is_refused_not_stranded() {
        let mut p = Pool::new(128);
        p.grant(1, 40, 1);
        let mut big = waiting(&mut p, 2, 100);
        let mut small = waiting(&mut p, 3, 10);
        assert!(!p.grant_from_head(), "40 + 100 > 128: the big one waits");
        p.capacity = 64;
        assert!(p.grant_from_head());
        assert_eq!(big.try_recv().unwrap(), Wake::NeverFits { capacity: 64 });
        assert_eq!(
            small.try_recv().unwrap(),
            Wake::Granted,
            "40 + 10 fits the shrunk pool, and nothing is ahead of it any more"
        );
    }

    /// A releasing reservation (second review, finding 6) still holds its tokens —
    /// the next waiter is not granted on its account — but no longer counts
    /// as in flight, which is what lets its container's `/slots` excuse it.
    #[test]
    fn a_releasing_reservation_keeps_its_tokens_but_not_its_slots() {
        let mut p = Pool::new(64);
        p.grant(1, 40, 1);
        p.grant(2, 10, 3);
        assert_eq!(p.active_slots(), 4);
        assert!(p.begin_release(1));
        assert!(!p.begin_release(1), "once releasing, never twice");
        assert_eq!(p.active_slots(), 3, "only the three slots still in flight");
        assert_eq!(p.reserved, 50, "the releasing one is still reserved");

        let mut next = waiting(&mut p, 3, 20);
        assert!(!p.grant_from_head(), "50 + 20 > 64: it waits");
        assert!(next.try_recv().is_err());
        assert!(!p.begin_release(3), "a waiter has nothing to defer");

        // The deferred release lands: the waiter goes.
        assert!(p.forget(1));
        assert_eq!(next.try_recv().unwrap(), Wake::Granted);
        assert_eq!(p.reserved, 30);
    }
}
