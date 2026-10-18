//! Futures erased to `dyn Future + Send`: what every async method of the
//! crate's own traits returns, and the seams below it.
//!
//! An async body that has to be `Send` — a trait method's boxed future, a
//! `tokio::spawn` — gets the auto trait proved through its whole witness:
//! each future it awaits, what that future holds across its own awaits, and
//! so on down to the leaves. rustc's global evaluation cache is keyed by the
//! param env and the predicate, so what keeps that proof cheap is bodies
//! sharing a param env. `#[async_trait]` gives every method one of its own
//! (its `'life0: 'async_trait` and `Self: 'async_trait` bounds), so each
//! method re-proved its leaves from scratch: for a type holding a
//! [`SharedState`], the `&self` capture's `Self: Sync` alone is `AppState`'s
//! ~1170 types, about 0.35 s per method.
//!
//! So no trait here uses the macro. A method is written out as
//! `fn m<'a>(&'a self, …) -> Erased<'a, T>` with the body
//! `Box::pin(async move { … })`, and runs nothing before the first poll, as
//! the macro did. The future holds every argument its body names; an argument
//! the body does not name drops at the call (the macro moved every one, `_x`
//! included), so a by-value guard parameter (a permit, a hold) needs
//! `let _guard = _guard;` inside the block to live as long as the body. The
//! method's only generic is a late-bound lifetime, so it has its impl's param
//! env, and its leaf proofs hit what the first body proved.
//! `check_coroutine_obligations` went 26 → 1.8 s with that (rustc 1.98.1, a
//! non-incremental check of the lib, 2026-10-18). Each call boxes once,
//! whatever the call rate (a sink's `emit` per streamed delta included), as
//! the macro did.
//!
//! A seam is one of the eight deliberate erasure points: a function that
//! returns [`Erased`], or a call boxed into one. Typeck lets a body that
//! defines a coroutine (every async body) use the global cache only for goals
//! that name no opaque type and no coroutine, so goals about an awaited
//! future are never shared between bodies. `dyn Future<Output = T> + Send` is
//! a leaf: its `Send` is the trait object's own bound, so a caller does not
//! walk the callee's coroutine. The seams were cut while the methods still
//! had param envs of their own, and took `check_coroutine_obligations`
//! 46 → 26 s then. Now they save little: all eight together about 0.4 s of
//! borrowck, none alone above the run-to-run noise. Some double as the heap
//! indirection a recursive call or a deep debug-build poll frame needs. Put
//! one at a request, turn, tool call or job run, not at a per-token or
//! per-frame step.
//!
//! [`SharedState`]: crate::state::SharedState

/// A future whose `Send` proof stops here (module doc). Build one with
/// `Box::pin(async move { … })` or [`futures::FutureExt::boxed`].
pub type Erased<'a, T> = futures::future::BoxFuture<'a, T>;
