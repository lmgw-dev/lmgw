//! A bound session's `speech_started` warm, built only when something it
//! is built from changed (WP11 server review m3).
//!
//! `speech_started` fires on every utterance and on noise, and the warm
//! re-reads the thread's models each time (chat-voice §8.2, WP8 review
//! m5): the stages' voice reads the TTS row's profile and lists its voices
//! and the voice library. What they are built from is the thread (its
//! `updated_at` moves with every settings write and every message) and
//! the settings snapshot (a new one on every settings, alias or row
//! change), so the last stages are reused while both are what they were.
//! The thread itself is still read each time: that is one row. The clock
//! counts whole seconds, so two writes within the second of a build can
//! leave that one build in place until the next write or turn: a
//! Background warm, which the next turn's own admission corrects.

use std::sync::{Arc, Mutex};

use crate::config::Snapshot;
use crate::realtime::warm::Warm;
use crate::state::SharedState;
use crate::store::ChatThread;

/// The last stages built, and what from.
#[derive(Default)]
pub(crate) struct Stages(Mutex<Option<Built>>);

struct Built {
    updated_at: String,
    /// Held, not compared by address alone: a freed snapshot's address can
    /// come back.
    snap: Arc<Snapshot>,
    stages: Vec<Warm>,
}

impl Stages {
    /// The thread's connect stages now (`bound::connect_stages`): the last
    /// ones when the thread and the snapshot are what they were built
    /// from, else built afresh.
    pub(crate) async fn now(&self, state: &SharedState, thread: &ChatThread) -> Vec<Warm> {
        let snap = state.snapshot();
        if let Some(b) = self.lock().as_ref() {
            if b.updated_at == thread.updated_at && Arc::ptr_eq(&b.snap, &snap) {
                return b.stages.clone();
            }
        }
        let stages = crate::web::chat_voice::bound::connect_stages(state, thread).await;
        *self.lock() = Some(Built {
            updated_at: thread.updated_at.clone(),
            snap,
            stages: stages.clone(),
        });
        stages
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Built>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
