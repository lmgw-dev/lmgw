//! Cooperative cancellation for one run ([`Cancel`]).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often a pending await is re-checked against [`Cancel`].
///
/// A cancel is a person waiting for a button to do something, so the interval
/// is set by what a person notices, not by what is cheap: a quarter second of
/// a stopped clock reads as instant. It is a *sampling rate*, not a bound on
/// anything — the awaits themselves carry their own deadline.
pub const CANCEL_POLL: Duration = Duration::from_millis(250);

/// Cooperative cancellation for one run: a flag the loop races **every** await
/// against, rather than a check between them.
///
/// The difference is the whole point. A tool loop's awaits are a model call and
/// a tool call, and both can take minutes; a flag observed only at the
/// boundaries means Cancel does nothing at all for as long as one of them is in
/// flight, which is exactly when it is pressed. A model call that took the
/// turn's stop is stopped and awaited, so it still writes its row, and so is a
/// tool call that took its own (`call_stop`); anything else is dropped — for a streaming model call that closes the
/// upstream connection, so the generation stops on the server too and not
/// just here (`agent/relay.rs`).
///
/// The default is [`Self::none`]: a run nothing can cancel. A streamed
/// `/v1/responses` run takes its client's hang-up as one ([`Self::signal`],
/// raised when the stream's receiver is dropped): a guest's, so a tool call
/// in flight does not keep the run's claims after its reader is gone; the
/// owner's [`sparing calls`](Self::sparing_calls), so its model call stops
/// at once while a tool call in flight finishes with its rows. The
/// [`EventSink`] returning `false` still ends a run between awaits.
#[derive(Clone, Default)]
pub struct Cancel {
    by: Option<CancelBy>,
    /// Tool calls in flight are not raced: they finish, and the loop meets
    /// the cancel after them, starting no new call and no new turn.
    spares_calls: bool,
}

#[derive(Clone)]
enum CancelBy {
    /// A job's bit, sampled every [`CANCEL_POLL`].
    Flag(Arc<AtomicBool>),
    /// A stop that wakes its waiters the moment it is raised.
    Signal(crate::proxy::StopSignal),
}

impl Cancel {
    /// Nothing cancels this run; the awaits below are not raced at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Race against `flag` — a job's [`JobCtx::canceled`](crate::jobs::JobCtx)
    /// bit, shared as-is rather than copied, so raising it is seen here.
    pub fn flag(flag: Arc<AtomicBool>) -> Self {
        Self {
            by: Some(CancelBy::Flag(flag)),
            spares_calls: false,
        }
    }

    /// Race against `stop`: the run sees it the moment it is raised, with no
    /// sampling interval — what a person waiting on a voice barge-in needs
    /// (chat-voice design §7.2). Dropping its handle cancels too.
    pub fn signal(stop: crate::proxy::StopSignal) -> Self {
        Self {
            by: Some(CancelBy::Signal(stop)),
            spares_calls: false,
        }
    }

    /// The same cancel, which lets a tool call in flight finish: it stops
    /// the model call where it stands, and once the calls in flight have
    /// ended the run starts no other call and no other turn. What the
    /// owner's streamed `/v1/responses` run takes at its client's hang-up
    /// (per-request background design §11.16, 2026-10-24).
    pub fn sparing_calls(mut self) -> Self {
        self.spares_calls = true;
        self
    }

    /// Whether a tool call in flight runs to its end under this cancel.
    pub(super) fn spares_calls(&self) -> bool {
        self.spares_calls
    }

    /// Whether the cancel is raised right now (never, without one).
    pub fn is_raised(&self) -> bool {
        match &self.by {
            None => false,
            Some(CancelBy::Flag(f)) => f.load(Ordering::Relaxed),
            Some(CancelBy::Signal(s)) => s.is_raised(),
        }
    }

    /// Resolves once the cancel is raised; pends forever when there is none,
    /// so a `select!` arm over it simply never fires.
    pub async fn raised(&self) {
        match &self.by {
            None => std::future::pending::<()>().await,
            Some(CancelBy::Flag(flag)) => {
                while !flag.load(Ordering::Relaxed) {
                    tokio::time::sleep(CANCEL_POLL).await;
                }
            }
            Some(CancelBy::Signal(s)) => s.raised().await,
        }
    }

    /// Await `fut`, abandoning it the moment the cancel is raised. `None` means
    /// it was abandoned — and abandoned means *dropped*, which is what makes
    /// the upstream connection close rather than the run merely stopping to
    /// wait for it.
    pub async fn guard<T>(&self, fut: impl Future<Output = T>) -> Option<T> {
        if self.by.is_none() {
            return Some(fut.await);
        }
        tokio::select! {
            biased;
            () = self.raised() => None,
            v = fut => Some(v),
        }
    }
}
