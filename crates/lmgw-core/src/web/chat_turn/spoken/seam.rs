//! The seams for the suite (`tests/it`): one fresh turn of a thread as a
//! bound session's heard response starts it (§3.4) — no `turn` frame,
//! `spoken` after the history — with its user row settled at once, or by
//! the test. What lets the voice-audio-input suite drive the gate's late
//! swaps (`gpu_world`) with audio in the request, and a heard turn's tools
//! against its row (WP3 review #1). `pub` (unconditional) to match the
//! other `*_for_tests` seams.

use tokio::sync::{mpsc, watch};

use super::UserRow;
use crate::ir::ContentPart;
use crate::state::SharedState;

/// A heard turn started by [`spoken_turn_held_for_tests`]: its frames as
/// they come, as `(event, data)`, and its user row, which the test settles
/// as the journal would.
#[doc(hidden)]
pub struct HeldTurnForTests {
    pub frames: mpsc::Receiver<(String, serde_json::Value)>,
    row: watch::Sender<Option<UserRow>>,
}

impl HeldTurnForTests {
    /// The journal wrote the turn's user row `id`.
    pub fn written(&self, id: i64) {
        self.row.send_replace(Some(UserRow::Written(id)));
    }

    /// The transcription failed and no turn had words: the row is written
    /// "not transcribed".
    pub fn failed(&self) {
        self.row.send_replace(Some(UserRow::Failed));
    }

    /// Every turn it answers came back without words: the veto.
    pub fn veto(&self) {
        self.row.send_replace(Some(UserRow::Veto));
    }

    /// The journal could not write the row (the store refused it).
    pub fn unwritten(&self) {
        self.row.send_replace(Some(UserRow::Unwritten));
    }

    /// Every frame until the turn ends.
    pub async fn rest(mut self) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        while let Some(f) = self.frames.recv().await {
            out.push(f);
        }
        out
    }
}

/// Test-only (`tests/it`): one fresh heard turn of thread `thread_id`, its
/// user row already settled with no row to write, and its frames, in
/// order, as `(event, data)` (module doc).
#[doc(hidden)]
pub async fn spoken_turn_for_tests(
    state: &SharedState,
    thread_id: i64,
    spoken: Vec<ContentPart>,
) -> Vec<(String, serde_json::Value)> {
    match spoken_turn_held_for_tests(state, thread_id, spoken).await {
        Some(held) => {
            held.row.send_replace(Some(UserRow::NoRow));
            held.rest().await
        }
        None => Vec::new(),
    }
}

/// Test-only (`tests/it`): one fresh heard turn of thread `thread_id`,
/// whose user row the test settles (module doc); `None` when it could not
/// start.
#[doc(hidden)]
pub async fn spoken_turn_held_for_tests(
    state: &SharedState,
    thread_id: i64,
    spoken: Vec<ContentPart>,
) -> Option<HeldTurnForTests> {
    let repo = super::super::ChatRepo::of(thread_id);
    let thread = repo.thread(state, thread_id).await.ok().flatten()?;
    let caps = super::super::super::chat_attach_gate::thread_caps(state, repo, &thread).await;
    let (tx, mut rx) = mpsc::channel(64);
    let (row, user_row) = watch::channel(None);
    let opts = super::super::TurnOpts {
        spoken: Some(spoken),
        user_row: Some(user_row),
        ..Default::default()
    };
    let mode = super::super::TurnMode::Fresh {
        user_message_id: None,
    };
    super::super::start_turn_into(state, repo, &thread, mode, caps, tx, opts)
        .await
        .ok()?;
    let (out, frames) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            let data = serde_json::from_str(&f.data).unwrap_or(serde_json::Value::Null);
            if out.send((f.event.to_string(), data)).await.is_err() {
                break;
            }
        }
    });
    Some(HeldTurnForTests { frames, row })
}
