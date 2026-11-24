//! The gateway-wide tool pool (document-conversion design §7.7, D19): every
//! CPU-bound conversion step takes a slot — a tool run ([`super::run`]) or
//! a native reader on a blocking thread ([`blocking`]) — and gives it back
//! when the step has ended.
//!
//! **Size and memory.** Derived from the machine ([`super::pool_size`]): as
//! many steps at once as there are logical cores, while the memory charged
//! to the steps that run fits the usable memory. Each step is charged the
//! most it can use ([`charge`]: its tools' address-space limits and the
//! outputs it holds, each a bound), so steps side by side never need more
//! memory than the machine has, whatever their files are, and light steps
//! (a page of text recognition, 2.2 GiB) run more at once than heavy ones
//! (a PDF's runs, 6 GiB). Before the performance round (2026-11-19) every
//! step was charged the heaviest step's 6 GiB, which gave a 16 GiB server
//! two slots. Where memory is not read yet (Windows, macOS) the cores
//! alone bound the pool.
//!
//! **By hand.** The owner can set the number of steps at once under
//! Settings › Documents ("Parallel tool runs", `documents.tool_slots`; 0 is
//! derived): that many run at once and memory is not charged, as a size set
//! by hand always did (the owner's figure replaces both inputs). The page
//! shows the size, the memory considered, each kind of step's charge and how
//! many of each fit ([`ToolPool::facts`]). A new size applies to the next
//! slot handed out; steps already running finish.
//!
//! **Fairness.** Waiting steps queue per caller (a key's conversions, or
//! lmgw's own Chat and knowledge-base reads), and a slot that comes free goes
//! round robin over the callers that wait, first come first served within
//! each, so a 300-file batch does not hold back a one-page caller. A step
//! learns its caller from [`as_caller`]; one outside it is lmgw's own. A
//! request without a key is a caller of its own ([`request_caller`]), so two
//! keyless requests do not share one queue.
//!
//! **One caller's share** ([`share`], security round A; only where a second
//! caller can exist): a key (all keyless
//! requests together, lmgw's own reads) never holds the pool's last slot,
//! nor so much memory that a step like its next one would not fit beside
//! it; it may always start one step while it holds none. A caller at its
//! share does not keep its turn: the next caller that waits gets the slot.
//!
//! **Several slots at once.** A step that needs more than one core (the
//! layout model's inference, `layout_model::session`) asks for them with
//! [`ToolPool::acquire_many`]: it waits for its caller's turn like any
//! step, holding nothing, and at its turn takes them all at once or waits
//! on, keeping its turn: slots (and memory) that come free meanwhile stay
//! free for it,
//! so it is never passed for ever, and no step holds some slots while it
//! waits for more. How many it takes is clamped to the size in force each
//! time it is looked at, so a size set by hand while it waits applies.
//!
//! Waiting for a slot does not count against a file's own clock
//! ([`clock`], docling's `document_timeout`).
//!
//! The pool is one per process ([`global`]), as the tools it bounds share
//! one machine.

pub mod charge;
pub mod clock;
mod facts;
mod fit;
pub mod share;
pub mod stop;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::oneshot;

use super::pool_size::{self, Machine};

/// The caller of steps that are not inside [`as_caller`]: lmgw's own reads
/// (Chat attachments, knowledge bases).
const OWN: &str = "lmgw";

tokio::task_local! {
    static CALLER: Caller;
}

/// Whose steps these are: the queue they wait in (round robin over the
/// queues) and the share they count against ([`share`]). A key's requests
/// are one queue and one share; a request without a key is a queue of its
/// own in the one keyless share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub queue: Arc<str>,
    pub share: Arc<str>,
}

impl From<Arc<str>> for Caller {
    /// A caller whose queue is its share.
    fn from(queue: Arc<str>) -> Self {
        Self {
            share: queue.clone(),
            queue,
        }
    }
}

impl From<&str> for Caller {
    fn from(queue: &str) -> Self {
        Self::from(Arc::<str>::from(queue))
    }
}

/// Run `fut` with its steps queued as `caller`'s.
pub async fn as_caller<F: std::future::Future>(caller: impl Into<Caller>, fut: F) -> F::Output {
    CALLER.scope(caller.into(), fut).await
}

/// Run `f` with the steps it takes now (`try_acquire_charged`) as
/// `caller`'s.
#[cfg(test)]
pub fn as_caller_now<T>(caller: impl Into<Caller>, f: impl FnOnce() -> T) -> T {
    CALLER.sync_scope(caller.into(), f)
}

fn granted(slot: Slot) -> Slot {
    clock::slot_granted();
    slot
}

fn caller() -> Caller {
    CALLER
        .try_with(Clone::clone)
        .unwrap_or_else(|_| Caller::from(Arc::<str>::from(OWN)))
}

/// A queue of its own for one request that has no key (auth off):
/// numbered per request, so a one-page request is not queued behind another
/// keyless caller's 300-file batch; all of them count against the one
/// keyless share ([`share::KEYLESS`]).
pub fn request_caller() -> Caller {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    Caller {
        queue: Arc::from(format!("request:{}", NEXT.fetch_add(1, Ordering::Relaxed))),
        share: Arc::from(share::KEYLESS),
    }
}

/// The pool every step shares (a test's own inside [`test_pool::within`]).
pub fn global() -> &'static ToolPool {
    #[cfg(test)]
    if let Ok(p) = test_pool::POOL.try_with(|p| *p) {
        return p;
    }
    static POOL: OnceLock<ToolPool> = OnceLock::new();
    POOL.get_or_init(|| ToolPool::new(pool_size::read()))
}

/// `documents.tool_slots`, as the published settings have it: applied to
/// [`global`] and to the web-source fetch pool, which has the same size
/// ([`super::source_fetch::slots`]), by every snapshot publish.
pub fn configure(tool_slots: u32) {
    global().set_by_hand(tool_slots);
    super::source_fetch::slots::global().set_by_hand(tool_slots);
}

/// Run the CPU-bound `f` on a blocking thread once it has a slot; the slot
/// is held by the thread until `f` returns, so a caller that goes away does
/// not free a slot whose work still runs.
pub async fn blocking<T, F>(f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    blocking_in(global(), f).await
}

/// [`blocking`] on `pool`. A panic in `f` is the `Err`; its slot is given
/// back as the thread unwinds.
pub async fn blocking_in<T, F>(pool: &ToolPool, f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let slot = pool.acquire().await;
    blocking_on(slot, f).await
}

/// [`blocking`] charged `bytes` ([`charge`]) instead of the heaviest step.
pub async fn blocking_charged<T, F>(bytes: u64, f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let slot = global().acquire_charged(1, bytes).await;
    blocking_on(slot, f).await
}

/// [`blocking`] on a slot the caller already holds (its tool runs had it):
/// the slot goes to the blocking thread and is given back when `f` returns
/// or panics. Dropped before `f` has returned, this tells `f` to stop
/// ([`stop`]).
pub async fn blocking_on<T, F>(slot: Slot, f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (_armed, flag) = stop::arm();
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        stop::run(flag, f)
    })
    .await
}

/// [`blocking_on`] that hands the slot back with what `f` returned, for a
/// caller that goes on using it (a page's next tool run). The slot is the
/// thread's while `f` runs: a caller that goes away meanwhile does not free
/// it, and it is given back when `f` has returned or panicked.
pub async fn blocking_keep<T, F>(slot: Slot, f: F) -> Result<(Slot, T), tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (_armed, flag) = stop::arm();
    tokio::task::spawn_blocking(move || {
        let out = stop::run(flag, f);
        (slot, out)
    })
    .await
}

/// One waiting step: how many slots it wants (clamped to the size when it
/// is looked at) and the memory it is charged, told how many it got.
struct Want {
    tx: oneshot::Sender<usize>,
    slots: usize,
    bytes: u64,
}

struct Waiting {
    caller: Caller,
    queue: VecDeque<Want>,
}

struct Inner {
    machine: Machine,
    derived: u32,
    /// `documents.tool_slots`; 0 is derived.
    by_hand: u32,
    running: usize,
    /// The memory charged to the steps running, in bytes.
    charged: u64,
    /// Callers with steps waiting, in round-robin order.
    waiting: VecDeque<Waiting>,
    /// What each caller holds ([`share`]).
    shares: share::Shares,
    /// Whether the callers' shares apply: the tool pool's where a second
    /// caller can exist ([`ToolPool::set_shared`]), never its twin's (the
    /// web-source fetches', whose downloads `max_body_mb` and
    /// `SOURCE_FETCH_TIMEOUT` bound each; a share there is not yet).
    shared: bool,
}

impl Inner {
    fn size(&self) -> usize {
        let n = if self.by_hand > 0 {
            self.by_hand
        } else {
            self.derived
        };
        n as usize
    }

    /// The memory steps are charged against: the machine's, unless the
    /// size was set by hand (module doc) or memory is not read here.
    fn memory(&self) -> Option<u64> {
        if self.by_hand > 0 {
            return None;
        }
        self.machine.memory.map(|m| m.bytes)
    }

    /// Whether `n` slots charged `bytes` may start now: free slots, and the
    /// memory, unless nothing runs (a step heavier than all of it runs
    /// alone). Not exact yet with keeps (a first run's output charged with
    /// no slot while its step waits for a wide one, `Slot::shrink(0, kept)`;
    /// review 3 of the poppler limit): they are in `charged`, not in
    /// `running`, so a step admitted alone may pass the memory by the other
    /// waiters' keeps (each the output its run kept, under 1 GiB), and a
    /// waiter's own keep counts beside its new charge until it drops it, so
    /// it may start later than it needs (at the latest alone).
    fn fits(&self, n: usize, bytes: u64) -> bool {
        self.running + n <= self.size()
            && (self.running == 0
                || self
                    .memory()
                    .is_none_or(|m| self.charged.saturating_add(bytes) <= m))
    }

    fn waiters(&self) -> usize {
        // A step whose future was dropped has closed its receiver: it is
        // gone, whatever still sits in the queue until the next hand-out.
        self.waiting
            .iter()
            .flat_map(|w| w.queue.iter())
            .filter(|w| !w.tx.is_closed())
            .count()
    }

    /// What a step of `who` that wants `slots` takes: at most the size in
    /// force and its caller's share ([`share::Shares::cap`]), at least one;
    /// `None` while its caller is at its share.
    fn clamped(&self, who: &Caller, slots: usize) -> Option<usize> {
        if !self.shared {
            return Some(slots.min(self.size()).max(1));
        }
        self.shares
            .cap(&who.share, slots.min(self.size()), self.size())
    }

    /// Whether `who` may start `n` slots charged `bytes` by its share.
    fn share_allows(&self, who: &Caller, n: usize, bytes: u64) -> bool {
        !self.shared
            || self
                .shares
                .allows(&who.share, n, bytes, self.size(), self.memory())
    }

    /// Count `n` slots charged `bytes` as running, `who`'s.
    fn take(&mut self, who: &Caller, n: usize, bytes: u64) {
        self.running += n;
        self.charged = self.charged.saturating_add(bytes);
        self.shares.add(&who.share, n, bytes);
    }

    /// Hand free slots to waiting steps, round robin over the callers. A
    /// step whose turn it is but that wants more slots than are free keeps
    /// its turn, and nothing is handed out past it (module doc); a step
    /// whose caller is at its share passes its turn on ([`share`]).
    fn hand_out(&mut self) {
        // Callers passed over for their share since the last hand-out.
        let mut passed = 0;
        loop {
            let free = self.size().saturating_sub(self.running);
            if free == 0 || passed >= self.waiting.len() {
                return;
            }
            if self.shared {
                share::idle_first(self);
            }
            let Some(mut next) = self.waiting.pop_front() else {
                return;
            };
            // A step that went away has dropped its receiver.
            while next.queue.front().is_some_and(|w| w.tx.is_closed()) {
                next.queue.pop_front();
            }
            let Some(front) = next.queue.front() else {
                continue;
            };
            let capped = self
                .clamped(&next.caller, front.slots)
                .filter(|n| self.share_allows(&next.caller, *n, front.bytes));
            let Some(n) = capped else {
                self.waiting.push_back(next);
                passed += 1;
                continue;
            };
            if n > free || !self.fits(n, front.bytes) {
                self.waiting.push_front(next);
                return;
            }
            passed = 0;
            if let Some(w) = next.queue.pop_front() {
                if w.tx.send(n).is_ok() {
                    let who = next.caller.clone();
                    self.take(&who, n, w.bytes);
                } else if !next.queue.is_empty() {
                    // Gone at the last moment: the caller's turn goes on.
                    self.waiting.push_front(next);
                    continue;
                }
            }
            if !next.queue.is_empty() {
                self.waiting.push_back(next);
            }
        }
    }
}

/// The pool ([`global`]; a test makes its own).
#[derive(Clone)]
pub struct ToolPool {
    inner: Arc<Mutex<Inner>>,
}

/// Held slots (one, or [`ToolPool::acquire_many`]'s), given back when
/// this and every [`Lease`] of them are dropped.
#[must_use = "the slot is given back when this is dropped"]
pub struct Slot {
    held: Arc<Held>,
}

/// Slots' place in the pool: counted as running, and their memory as
/// charged, until dropped.
struct Held {
    inner: Arc<Mutex<Inner>>,
    count: usize,
    bytes: u64,
    /// The share they count against.
    share: Arc<str>,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running = g.running.saturating_sub(self.count);
        g.charged = g.charged.saturating_sub(self.bytes);
        g.shares.sub(&self.share, self.count, self.bytes);
        g.hand_out();
    }
}

/// Slots kept in use by work that outlives their holder: a stopped tool
/// that has not ended yet ([`super::run`] holds one until the child is
/// reaped). The slots go back to the pool when the [`Slot`] and its leases
/// are all dropped, so a caller that goes away never frees a slot whose
/// work still runs.
#[must_use = "the slot stays in use while this lives"]
pub struct Lease {
    _held: Arc<Held>,
}

impl Slot {
    fn new(inner: Arc<Mutex<Inner>>, count: usize, bytes: u64, share: Arc<str>) -> Self {
        Self {
            held: Arc::new(Held {
                inner,
                count,
                bytes,
                share,
            }),
        }
    }

    /// How many slots this holds.
    pub fn count(&self) -> usize {
        self.held.count
    }

    /// These slots cut down to `count` charged `bytes` (no more than they
    /// hold), what is given up going to the steps that wait (a PDF's two
    /// geometry runs done, its layout read on one; review N3). Kept as they
    /// are while a lease of them lives (a stopped tool not reaped yet).
    pub fn shrink(mut self, count: usize, bytes: u64) -> Slot {
        if bytes > self.held.bytes {
            // A caller that needs more grows it (`Slot::fit`); never silent.
            tracing::warn!(
                "a tool-pool slot charged {} was asked to shrink to {}: kept at {}",
                self.held.bytes,
                bytes,
                self.held.bytes
            );
        }
        let Some(held) = Arc::get_mut(&mut self.held) else {
            // A keep (`shrink(0, kept)`, held while its step waits for a
            // wide slot) must hold no slot count, or it waits holding one.
            // No caller asks for it with a lease alive (review 3 of the
            // poppler limit): out of memory is decided after the run ended.
            debug_assert!(count > 0, "a leased slot was asked to shrink to 0");
            if count == 0 {
                tracing::warn!("a leased tool-pool slot was asked to shrink to 0: kept");
            }
            return self;
        };
        let (count, bytes) = (count.min(held.count), bytes.min(held.bytes));
        let mut g = held.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running = g.running.saturating_sub(held.count - count);
        g.charged = g.charged.saturating_sub(held.bytes - bytes);
        g.shares
            .sub(&held.share, held.count - count, held.bytes - bytes);
        (held.count, held.bytes) = (count, bytes);
        g.hand_out();
        drop(g);
        self
    }

    /// Keep the slots in use for as long as the lease lives.
    pub fn lease(&self) -> Lease {
        Lease {
            _held: self.held.clone(),
        }
    }
}

/// A step waiting for its slots: when it is dropped after they were
/// handed to it but before it took them, they go back; when it is dropped
/// still waiting, the steps behind it get their turn.
struct Wait {
    rx: Option<oneshot::Receiver<usize>>,
    inner: Arc<Mutex<Inner>>,
    bytes: u64,
    share: Arc<str>,
}

impl Drop for Wait {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            match rx.try_recv() {
                Ok(count) => drop(Slot::new(
                    self.inner.clone(),
                    count,
                    self.bytes,
                    self.share.clone(),
                )),
                // It may have kept its turn with slots free behind it.
                Err(_) => self
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hand_out(),
            }
        }
    }
}

impl ToolPool {
    /// A pool sized from `machine`.
    pub fn new(machine: Machine) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                machine,
                derived: pool_size::derive(&machine),
                by_hand: 0,
                running: 0,
                charged: 0,
                waiting: VecDeque::new(),
                shares: share::Shares::default(),
                shared: true,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A pool of its own, empty, with this one's machine and size (derived,
    /// or set by hand): the web-source fetches'.
    pub fn twin(&self) -> Self {
        let (machine, by_hand) = {
            let g = self.lock();
            (g.machine, g.by_hand)
        };
        let p = Self::new(machine);
        p.lock().shared = false;
        p.set_by_hand(by_hand);
        p
    }

    /// Set the size by hand; 0 goes back to the derived size.
    pub fn set_by_hand(&self, slots: u32) {
        let mut g = self.lock();
        if g.by_hand != slots {
            g.by_hand = slots;
            g.hand_out();
        }
    }

    /// A slot charged the heaviest step ([`charge::HEAVIEST`]), once one is
    /// free, the memory fits and it is this caller's turn.
    pub async fn acquire(&self) -> Slot {
        self.acquire_charged(1, charge::HEAVIEST).await
    }

    /// `slots` slots at once (module doc), charged the heaviest step each.
    pub async fn acquire_many(&self, slots: usize) -> Slot {
        let each = charge::HEAVIEST;
        self.acquire_charged(slots, each.saturating_mul(slots as u64))
            .await
    }

    /// `slots` slots at once charged `bytes` together (module doc), the
    /// slots clamped to the size in force and the caller's share when they
    /// are handed out: [`Slot::count`] says how many it got.
    pub async fn acquire_charged(&self, slots: usize, bytes: u64) -> Slot {
        let who = caller();
        let rx = {
            let mut g = self.lock();
            // Only when nobody waits: a newcomer never passes the queue.
            if let Some(n) = g.clamped(&who, slots) {
                if g.fits(n, bytes) && g.share_allows(&who, n, bytes) && g.waiting.is_empty() {
                    g.take(&who, n, bytes);
                    return granted(Slot::new(self.inner.clone(), n, bytes, who.share));
                }
            }
            let (tx, rx) = oneshot::channel();
            let want = Want { tx, slots, bytes };
            match g.waiting.iter_mut().find(|w| w.caller.queue == who.queue) {
                Some(w) => w.queue.push_back(want),
                None => g.waiting.push_back(Waiting {
                    caller: who.clone(),
                    queue: VecDeque::from([want]),
                }),
            }
            // Free slots another caller's share left behind it go now.
            g.hand_out();
            rx
        };
        let mut wait = Wait {
            rx: Some(rx),
            inner: self.inner.clone(),
            bytes,
            share: who.share.clone(),
        };
        let got = {
            let _paused = clock::paused();
            match wait.rx.as_mut() {
                Some(rx) => rx.await.ok(),
                None => None,
            }
        };
        wait.rx = None;
        let count = match got {
            Some(n) => n,
            None => {
                // The sender is only dropped unsent when the pool itself is
                // gone, which a held `inner` rules out; take a slot anyway
                // rather than run without one.
                self.lock().take(&who, 1, bytes);
                1
            }
        };
        granted(Slot::new(self.inner.clone(), count, bytes, who.share))
    }

    /// `slots` slots charged `bytes`, now or not at all: only when they are
    /// free, the memory fits and nobody waits (a step that can use more
    /// cores when they are idle, and goes on with one otherwise).
    pub fn try_acquire_charged(&self, slots: usize, bytes: u64) -> Option<Slot> {
        let who = caller();
        let mut g = self.lock();
        let n = g.clamped(&who, slots)?;
        if n < slots
            || !g.fits(n, bytes)
            || !g.share_allows(&who, n, bytes)
            || !g.waiting.is_empty()
        {
            return None;
        }
        g.take(&who, n, bytes);
        Some(granted(Slot::new(self.inner.clone(), n, bytes, who.share)))
    }
}

#[cfg(test)]
mod peak;
#[cfg(test)]
pub mod test_pool;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_charge;
#[cfg(test)]
mod tests_many;
#[cfg(test)]
mod tests_read;
