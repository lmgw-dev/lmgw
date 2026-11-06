//! The card actions that run now, by their `act-<boot>-<n>` id (design
//! §8.3, §6.5): each is registered among the thread's running media calls
//! (`AppState::media_calls`), which the media cancel route stops, and its
//! hooks turn the running call's progress into the stream's `progress`
//! frames — the tool calls' frame (`ToolProgressFrame`), previews, what a
//! cancel would do and an upscale's `upscaling` included.
//!
//! The ids count across every gateway state of the process, so two states
//! never share one, and `<boot>` is 32 random bits drawn once per process:
//! a later process hands out an id a stored action file carries as its
//! `call_id` only if it draws the same bits, a chance of one in about four
//! billion per restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::config::Route;
use crate::erased::Erased;
use crate::proxy::image_call::{Follows, Hooks, MediaProgress};
use crate::proxy::StopHandle;
use crate::state::SharedState;

use super::super::super::chat_turn::TurnFrame;
use super::super::cancel_route::Registered;
use super::super::progress::frame;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// This process's part of every action id (module doc).
static BOOT: LazyLock<String> = LazyLock::new(|| hex::encode(rand::random::<[u8; 4]>()));

/// A running action's place among the thread's running media calls;
/// dropping it takes it out.
pub(super) struct Running {
    pub id: String,
    registered: Registered,
}

impl Running {
    /// A new id for an action on `thread_id`, stopped through `stop`.
    pub fn start(state: &SharedState, thread_id: i64, stop: StopHandle) -> Self {
        let id = format!("act-{}-{}", *BOOT, NEXT.fetch_add(1, Ordering::Relaxed));
        let registered = state
            .media_calls
            .register(state, (thread_id, &id), Arc::new(stop));
        Self { id, registered }
    }

    /// Raise its own stop (its caller's key was revoked).
    pub fn stop(&self) {
        self.registered.stop();
    }

    /// Whether its thread went away while it ran.
    pub fn thread_gone(&self) -> bool {
        self.registered.thread_gone()
    }
}

/// An action's hooks: no claim to let go (no loop runs), and its progress
/// as lossy `progress` frames — a slow reader drops frames rather than
/// stall the call, and the next frame supersedes the last.
pub(super) struct ActionHooks {
    pub running: Running,
    pub started: Instant,
    pub tx: mpsc::Sender<TurnFrame>,
}

impl ActionHooks {
    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

impl Hooks for ActionHooks {
    fn before_admit<'a>(&'a self, _route: &'a Route) -> Erased<'a, ()> {
        Box::pin(async {})
    }

    fn progress(&self, p: &MediaProgress) {
        self.running.registered.set_cancel(p.cancel());
        let f = frame(&self.running.id, p, self.elapsed_ms());
        let _ = self.tx.try_send(TurnFrame::of("progress", &f));
    }

    /// The card shows the action's previews as a tool card does.
    fn follows(&self) -> Follows {
        Follows::Previews
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ids_carry_the_process_part_and_a_gone_thread_stops_its_own_only() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let (h1, s1) = crate::proxy::stop_pair();
        let (h2, s2) = crate::proxy::stop_pair();
        let here = Running::start(&state, -4, h1);
        let other = Running::start(&state, -5, h2);
        assert_ne!(here.id, other.id);
        let prefix = format!("act-{}-", *BOOT);
        assert!(here.id.starts_with(&prefix), "{}", here.id);
        assert_eq!(BOOT.len(), 8);

        assert_eq!(state.media_calls.thread_gone(-4), 1);
        assert!(s1.is_raised() && here.thread_gone());
        assert!(!s2.is_raised() && !other.thread_gone());
    }
}
