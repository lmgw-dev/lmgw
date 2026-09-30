//! Async work that belongs to a page (review code:C4).
//!
//! A `spawn_local` task outlives the page that started it. When the fetch it
//! awaits answers after the page was left, the continuation reads signals the
//! router has already disposed — and a *read* of a disposed signal panics,
//! which traps the whole wasm module (a write is a silent no-op). Fast in-app
//! navigation hits this every time a loader is still in flight.
//!
//! A [`Scope`] is made next to the signals a task reads — at the top of a
//! page component, or of the component that owns them — and is disposed with
//! them. Its tasks stop at their next `.await` once it is gone:
//!
//! ```ignore
//! let scope = Scope::new();
//! scope.spawn(async move {
//!     let v = crate::api::get::<Value>("/api/settings-full").await;
//!     form.rebase(flatten(&v?));   // not reached once the page is left
//! });
//! ```
//!
//! Why not `leptos::task::spawn_local_scoped_with_cancellation`: it binds to
//! whatever owner is current when it is called. In an event handler that is
//! the enclosing `move ||` view's, which a re-render disposes — a Save whose
//! button flips to "Saving…" would cancel its own continuation — and it holds
//! that owner strongly while in flight, so a left page's effects keep running
//! until the fetch returns.
//!
//! A mutation whose outcome should still be reported after the page is gone
//! (an error toast is app-level) stays a plain `spawn_local` that toasts
//! first and then checks [`Scope::alive`] before touching the page.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use leptos::prelude::*;

/// The lifetime of the reactive owner it was made under. `Copy`.
#[derive(Clone, Copy)]
pub struct Scope(StoredValue<()>);

impl Scope {
    #[allow(clippy::new_without_default)] // made where its owner is; not a value
    pub fn new() -> Self {
        Self(StoredValue::new(()))
    }

    /// Is the owner — and every signal made beside the scope — still there?
    pub fn alive(self) -> bool {
        !self.0.is_disposed()
    }

    /// `spawn_local`, dropped at its next `.await` once the scope is gone.
    pub fn spawn(self, fut: impl Future<Output = ()> + 'static) {
        leptos::task::spawn_local(Scoped {
            scope: self,
            fut: Box::pin(fut),
        });
    }
}

/// Which of several reads of one list is the newest (review MED 1): each
/// read takes a ticket before it goes out, and only the holder of the last
/// ticket applies its answer — an older read that lands late would put back
/// what a newer one already replaced. Made beside the list's signals; once
/// they are gone no ticket is current. `Copy`.
#[derive(Clone, Copy)]
pub struct Latest(StoredValue<u64>);

impl Latest {
    #[allow(clippy::new_without_default)] // made where its owner is; not a value
    pub fn new() -> Self {
        Self(StoredValue::new(0))
    }

    /// A ticket for a read about to go out; `None` once the owner is gone.
    pub fn next(self) -> Option<u64> {
        self.0.try_update_value(|n| {
            *n += 1;
            *n
        })
    }

    /// Is `ticket` the newest read's? Never once the owner is gone.
    pub fn is(self, ticket: u64) -> bool {
        self.0.try_get_value() == Some(ticket)
    }
}

struct Scoped<F> {
    scope: Scope,
    fut: Pin<Box<F>>,
}

impl<F: Future<Output = ()>> Future for Scoped<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Every resume after an await passes here first: a page that went
        // away while the task waited ends it instead of running it on.
        if !self.scope.alive() {
            return Poll::Ready(());
        }
        self.fut.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Waker;

    /// Pending on its first poll (the fetch), then records that it ran on.
    struct AfterAwait {
        polled: bool,
        ran_on: Rc<Cell<bool>>,
    }

    impl Future for AfterAwait {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if !self.polled {
                self.polled = true;
                return Poll::Pending;
            }
            self.ran_on.set(true);
            Poll::Ready(())
        }
    }

    fn scoped(scope: Scope, ran_on: Rc<Cell<bool>>) -> Scoped<AfterAwait> {
        Scoped {
            scope,
            fut: Box::pin(AfterAwait {
                polled: false,
                ran_on,
            }),
        }
    }

    #[test]
    fn a_task_whose_page_is_gone_stops_at_its_next_await() {
        let mut cx = Context::from_waker(Waker::noop());

        let owner = Owner::new();
        let (scope, ran_on) = owner.with(|| (Scope::new(), Rc::new(Cell::new(false))));
        let mut task = scoped(scope, ran_on.clone());
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        // The page is left while the fetch is out.
        owner.cleanup();
        assert!(!scope.alive());
        assert!(Pin::new(&mut task).poll(&mut cx).is_ready());
        assert!(!ran_on.get(), "the continuation must not run");
    }

    #[test]
    fn only_the_newest_read_is_applied_and_none_once_the_page_is_gone() {
        let owner = Owner::new();
        let latest = owner.with(Latest::new);
        let first = latest.next().unwrap();
        let second = latest.next().unwrap();
        // The first read lands after the second went out: dropped.
        assert!(!latest.is(first));
        assert!(latest.is(second));
        owner.cleanup();
        assert!(!latest.is(second));
        assert_eq!(latest.next(), None);
    }

    #[test]
    fn a_task_whose_page_stays_runs_to_the_end() {
        let mut cx = Context::from_waker(Waker::noop());

        let owner = Owner::new();
        let scope = owner.with(Scope::new);
        let ran_on = Rc::new(Cell::new(false));
        let mut task = scoped(scope, ran_on.clone());
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        assert!(scope.alive());
        assert!(Pin::new(&mut task).poll(&mut cx).is_ready());
        assert!(ran_on.get());
    }
}
