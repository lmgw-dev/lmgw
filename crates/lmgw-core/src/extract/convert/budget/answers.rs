//! The answers in flight, gateway-wide (security round A, H1): a
//! conversion's upload and answer live in the gateway's memory from its
//! first byte read until its body has been written to the client, and one
//! request's output may be [`Answers::output_budget`]. Every conversion
//! therefore holds a [`Share`] of one gateway-wide bound:
//!
//! - **The bound** is the memory lmgw may use, read as the tool pool reads
//!   it (`pool_size::read`: a cgroup limit below the machine's memory, else
//!   the machine's memory). Where the memory is not read yet (Windows,
//!   macOS) nothing is bounded, and Settings › Documents says so.
//! - **A share** is the request's upload and its output budget
//!   (`super::upload::request_share`: the declared body length, or the most
//!   `max_body_mb` allows for a body sent without one), taken *before* the
//!   body is read, so a request waiting for one holds no upload, no slot and
//!   no output. Once the answer is made, the share becomes the body's real
//!   size ([`Share::settle`], never a wait) and is held until the body has
//!   been written or the client went away ([`crate::server::documents::send`],
//!   which writes a body too large to sit beside its answer as it goes).
//! - **The output budget** is [`MAX_UNCOMPRESSED_BYTES`], or half the bound
//!   where that is less (review S4), so a machine below 2 GiB still
//!   converts; `output_too_large` names the budget in force.
//! - **One caller's share** (review B2, [`holders`]): where a second caller
//!   can exist (`super::super::callers`), a caller (a key, every keyless
//!   request together, an account's owner keys) holding shares takes another
//!   only while the bound keeps one like it free beside them, and a caller
//!   holding none is served first, the one served longest ago first. A job
//!   counts against the key that sent it. With one caller, shares are
//!   handed out first come, first served, as before.
//! - A request whose share is larger than the whole bound can never get one
//!   and is refused by name ([`NeverFits`]).
//! - Waiting for a share is not the file's time (`document_timeout` starts
//!   at a file's first tool slot, after this).
//!
//! **Order** (no wait while holding what another waiter needs): a request
//! reads nothing before its share; then its body is read under its read
//! deadline (`super::upload`), then its files' steps wait for tool slots
//! and their caller's share of the pool (`pool::share`) while holding the
//! answer share. Nothing that holds a tool slot ever waits for an answer
//! share, and [`Share::settle`] never waits, so the waits form no cycle. An
//! async job takes its own share when its task starts; the request that
//! accepted it gives its own back with the 202.

mod holders;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use lmgw_api_types::documents::bounds::AnswerFacts;
use tokio::sync::oneshot;

use crate::extract::convert::pool_size;
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

/// An answer's body is written within this much time ...
pub const ANSWER_WRITE_GRACE_S: u64 = 60;

/// ... plus one second for every this many bytes of it (1 MiB/s): a client
/// that reads slower has its connection ended, named in the log, and the
/// share goes back.
pub const ANSWER_MIN_RATE_BYTES_PER_S: u64 = 1 << 20;

/// The write deadline of a body of `len` bytes.
pub fn write_deadline(len: u64) -> std::time::Duration {
    std::time::Duration::from_secs(
        ANSWER_WRITE_GRACE_S.saturating_add(len / ANSWER_MIN_RATE_BYTES_PER_S),
    )
}

/// One share waiting: the bytes it wants and whose it is.
struct Waiter {
    tx: oneshot::Sender<()>,
    bytes: u64,
    who: Arc<str>,
}

struct Inner {
    /// `None`: not bounded (module doc).
    limit: Option<u64>,
    held: u64,
    holding: u32,
    /// Whether one caller's share applies (module doc).
    shared: bool,
    holders: holders::Holders,
    /// Waiting shares in the order they came.
    waiting: VecDeque<Waiter>,
}

impl Inner {
    fn fits(&self, bytes: u64) -> bool {
        self.limit
            .is_none_or(|l| self.holding == 0 || self.held.saturating_add(bytes) <= l)
    }

    /// Whether `who` may take `bytes` more by its caller's share.
    fn allows(&self, who: &str, bytes: u64) -> bool {
        !self.shared || self.holders.allows(who, bytes, self.limit)
    }

    fn take(&mut self, who: &Arc<str>, bytes: u64) {
        self.held = self.held.saturating_add(bytes);
        self.holding += 1;
        self.holders.add(who, bytes);
    }

    fn give_back(&mut self, who: &str, bytes: u64) {
        self.held = self.held.saturating_sub(bytes);
        self.holding = self.holding.saturating_sub(1);
        self.holders.sub(who, bytes);
    }

    /// The waiter whose turn it is: the first in line, or with the shares
    /// on, a caller holding none first (the one served longest ago first),
    /// else the first whose caller's share allows it.
    fn next(&self) -> Option<usize> {
        let live = self
            .waiting
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.tx.is_closed());
        if !self.shared {
            return live.map(|(i, _)| i).next();
        }
        let allowed: Vec<(usize, &Waiter)> =
            live.filter(|(_, w)| self.allows(&w.who, w.bytes)).collect();
        allowed
            .iter()
            .filter_map(|(i, w)| self.holders.idle_rank(&w.who).map(|r| (r, *i)))
            .min()
            .map(|(_, i)| i)
            .or_else(|| allowed.first().map(|(i, _)| *i))
    }

    /// Hand the bytes given back to the shares that wait (module doc); the
    /// one whose turn it is but that does not fit keeps its turn, and
    /// nothing is handed out past it.
    fn hand_out(&mut self) {
        self.waiting.retain(|w| !w.tx.is_closed());
        while let Some(i) = self.next() {
            if !self.fits(self.waiting[i].bytes) {
                return;
            }
            let Some(w) = self.waiting.remove(i) else {
                return;
            };
            if w.tx.send(()).is_ok() {
                self.take(&w.who, w.bytes);
            }
        }
    }
}

/// The gateway's answers in flight ([`global`]; a test makes its own).
#[derive(Clone)]
pub struct Answers {
    inner: Arc<Mutex<Inner>>,
}

/// A conversion's share: given back when dropped.
#[must_use = "the share is given back when this is dropped"]
pub struct Share {
    inner: Arc<Mutex<Inner>>,
    bytes: u64,
    who: Arc<str>,
}

/// A share larger than the whole bound: no conversion of this size can run
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeverFits {
    pub share: u64,
    pub limit: u64,
    /// The output budget in force, part of the share.
    pub output: u64,
}

impl NeverFits {
    /// The refusal's code.
    pub const CODE: &'static str = "answer_memory";

    /// The refusal's words.
    pub fn message(&self) -> String {
        use lmgw_api_types::units::fmt_bytes;
        format!(
            "this conversion holds up to {} of the gateway's memory while it converts and \
             writes its answer (its upload, and its output of at most {}: \
             MAX_UNCOMPRESSED_BYTES or half the memory for answers), more than the {} lmgw may \
             use on this machine; send fewer or smaller files, or declare the body's length \
             ({})",
            fmt_bytes(self.share),
            fmt_bytes(self.output),
            fmt_bytes(self.limit),
            Self::CODE
        )
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.give_back(&self.who, self.bytes);
        g.hand_out();
    }
}

impl Share {
    /// The share set to `bytes`, the answer's body now that it is made: the
    /// rest goes back; a body larger than the share (JSON escapes) is
    /// counted as it is, without waiting (module doc).
    pub fn settle(mut self, bytes: u64) -> Share {
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.held = g.held.saturating_sub(self.bytes).saturating_add(bytes);
            g.holders.resize(&self.who, self.bytes, bytes);
            self.bytes = bytes;
            g.hand_out();
        }
        self
    }

    /// The share grown by `bytes` the request holds beyond its upload (a
    /// docling request's downloaded sources): counted, never waited for.
    pub fn grow(self, bytes: u64) -> Share {
        let to = self.bytes.saturating_add(bytes);
        self.settle(to)
    }

    /// What it holds now.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Dropped while still waiting: the shares behind it get their turn.
struct Wait {
    rx: Option<oneshot::Receiver<()>>,
    inner: Arc<Mutex<Inner>>,
    bytes: u64,
    who: Arc<str>,
}

impl Drop for Wait {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if rx.try_recv().is_ok() {
                // Handed over just now: given back.
                g.give_back(&self.who, self.bytes);
            }
            g.hand_out();
        }
    }
}

/// A wait that passes a second is logged (review N6), once.
const SLOW_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

impl Answers {
    /// Answers bounded by `limit` bytes (`None`: not bounded), one caller's
    /// share on.
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                limit,
                held: 0,
                holding: 0,
                shared: true,
                holders: holders::Holders::default(),
                waiting: VecDeque::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether one caller's share applies (`callers::configure`).
    pub fn set_shared(&self, on: bool) {
        let mut g = self.lock();
        if g.shared != on {
            g.shared = on;
            g.hand_out();
        }
    }

    /// A bound of `limit` in place of the one read from the machine (a
    /// test's gateway).
    #[doc(hidden)]
    pub fn set_limit(&self, limit: Option<u64>) {
        let mut g = self.lock();
        g.limit = limit;
        g.hand_out();
    }

    /// What one request's files may produce together:
    /// [`MAX_UNCOMPRESSED_BYTES`], or half the bound where that is less.
    pub fn output_budget(&self) -> u64 {
        output_budget_of(self.lock().limit)
    }

    /// A share of `bytes` for `who`, once it fits, its caller's share allows
    /// it and its turn has come; refused when it never can.
    pub async fn hold(&self, who: &Arc<str>, bytes: u64) -> Result<Share, NeverFits> {
        let rx = {
            let mut g = self.lock();
            if let Some(limit) = g.limit.filter(|l| bytes > *l) {
                return Err(NeverFits {
                    share: bytes,
                    limit,
                    output: output_budget_of(Some(limit)),
                });
            }
            let nobody_waits = g.waiting.iter().all(|w| w.tx.is_closed());
            if nobody_waits && g.fits(bytes) && g.allows(who, bytes) {
                g.take(who, bytes);
                return Ok(self.share(who, bytes));
            }
            let (tx, rx) = oneshot::channel();
            g.waiting.push_back(Waiter {
                tx,
                bytes,
                who: who.clone(),
            });
            // A caller holding none may go past callers at their share.
            g.hand_out();
            rx
        };
        let mut wait = Wait {
            rx: Some(rx),
            inner: self.inner.clone(),
            bytes,
            who: who.clone(),
        };
        let got = match wait.rx.as_mut() {
            Some(rx) => match tokio::time::timeout(SLOW_WAIT, &mut *rx).await {
                Ok(r) => r.is_ok(),
                Err(_) => {
                    let f = self.facts();
                    tracing::debug!(
                        "a conversion waits for its share of the memory for answers ({} wanted; \
                         {} held by {} conversions, {} waiting)",
                        lmgw_api_types::units::fmt_bytes(bytes),
                        lmgw_api_types::units::fmt_bytes(f.held_bytes),
                        f.holding,
                        f.waiting
                    );
                    rx.await.is_ok()
                }
            },
            None => false,
        };
        wait.rx = None;
        if !got {
            // Only when the bound itself is gone, which a held `inner` rules
            // out; counted anyway rather than held without a count.
            self.lock().take(who, bytes);
        }
        Ok(self.share(who, bytes))
    }

    fn share(&self, who: &Arc<str>, bytes: u64) -> Share {
        Share {
            inner: self.inner.clone(),
            bytes,
            who: who.clone(),
        }
    }

    /// What is held and waits now, and the bound.
    pub fn facts(&self) -> AnswerFacts {
        let g = self.lock();
        AnswerFacts {
            limit_bytes: g.limit,
            output_bytes: output_budget_of(g.limit),
            held_bytes: g.held,
            holding: g.holding,
            waiting: u32::try_from(g.waiting.iter().filter(|w| !w.tx.is_closed()).count())
                .unwrap_or(u32::MAX),
            write_grace_s: ANSWER_WRITE_GRACE_S,
            min_rate_bytes_per_s: ANSWER_MIN_RATE_BYTES_PER_S,
            read_grace_s: super::upload::UPLOAD_READ_GRACE_S,
            read_min_rate_bytes_per_s: super::upload::UPLOAD_MIN_RATE_BYTES_PER_S,
            callers: lmgw_api_types::documents::bounds::CallerFacts {
                shared: g.shared,
                ..crate::extract::convert::callers::facts()
            },
        }
    }
}

/// The output budget under a bound of `limit` (module doc).
fn output_budget_of(limit: Option<u64>) -> u64 {
    limit.map_or(MAX_UNCOMPRESSED_BYTES, |l| {
        (l / 2).min(MAX_UNCOMPRESSED_BYTES)
    })
}

tokio::task_local! {
    static OWN: Answers;
}

/// The gateway's answers in flight, bounded by the memory lmgw may use (a
/// test's own inside [`within`]).
pub fn global() -> Answers {
    if let Ok(a) = OWN.try_with(Clone::clone) {
        return a;
    }
    static ANSWERS: OnceLock<Answers> = OnceLock::new();
    ANSWERS
        .get_or_init(|| Answers::new(pool_size::read().memory.map(|m| m.bytes)))
        .clone()
}

/// `fut` with `answers` as its answers in flight (a test's small bound).
pub async fn within<F: std::future::Future>(answers: Answers, fut: F) -> F::Output {
    OWN.scope(answers, fut).await
}

#[cfg(test)]
mod tests;
