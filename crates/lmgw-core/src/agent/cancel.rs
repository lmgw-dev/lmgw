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
/// turn's stop is stopped and awaited, so it still writes its row; anything
/// else [`Self::guard`] drops — for a streaming model call that closes the
/// upstream connection, so the generation stops on the server too and not
/// just here (`agent/relay.rs`).
///
/// The default is [`Self::none`]: a run nothing can cancel, which is what
/// `/v1/responses` wants — its client going away is reported by the
/// [`EventSink`] returning `false`, and that path is untouched.
#[derive(Clone, Default)]
pub struct Cancel(Option<CancelBy>);

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
        Self(None)
    }

    /// Race against `flag` — a job's [`JobCtx::canceled`](crate::jobs::JobCtx)
    /// bit, shared as-is rather than copied, so raising it is seen here.
    pub fn flag(flag: Arc<AtomicBool>) -> Self {
        Self(Some(CancelBy::Flag(flag)))
    }

    /// Race against `stop`: the run sees it the moment it is raised, with no
    /// sampling interval — what a person waiting on a voice barge-in needs
    /// (chat-voice design §7.2). Dropping its handle cancels too.
    pub fn signal(stop: crate::proxy::StopSignal) -> Self {
        Self(Some(CancelBy::Signal(stop)))
    }

    /// Whether the cancel is raised right now (never, without one).
    pub fn is_raised(&self) -> bool {
        match &self.0 {
            None => false,
            Some(CancelBy::Flag(f)) => f.load(Ordering::Relaxed),
            Some(CancelBy::Signal(s)) => s.is_raised(),
        }
    }

    /// Resolves once the cancel is raised; pends forever when there is none,
    /// so a `select!` arm over it simply never fires.
    pub async fn raised(&self) {
        match &self.0 {
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
        if self.0.is_none() {
            return Some(fut.await);
        }
        tokio::select! {
            biased;
            () = self.raised() => None,
            v = fut => Some(v),
        }
    }
}
