//! Sampling an audio container while a request runs on it (WP7 live gate).
//!
//! **Why.** The reading after an answer misses what audio.cpp frees once it
//! has answered: on the 4090, pocket-tts read 0.921 GB after its answer and
//! 0.981 GB at most while it worked — the compute buffers, released per
//! request the way sd.cpp releases its own ([`crate::vram::peak`]). A figure
//! learned after the answer alone leaves that transient charged nowhere.
//!
//! **What.** From the moment an inference request is sent on an audio hold
//! ([`crate::vram::LocalHold::note_sending`]) until nothing is in flight on
//! that container any more, its processes are read every [`SAMPLE_INTERVAL`]
//! — per process, on a blocking thread, never on the request path. The
//! largest sample is folded into the reading after each answer
//! ([`super::learn`]) and stored once more when the stretch ends. Only a
//! stretch in which a request was answered counts: a request that failed
//! teaches nothing, as before.
//!
//! **Bounded.** At most one sampler per container generation, only for a
//! container started with the row's current configuration, and only until
//! the figure has settled: once [`SETTLE_AFTER`] sampled stretches in a row
//! did not raise it, that configuration's requests are not sampled any more
//! (the reading after each answer goes on). The count is in memory and
//! starts again after a restart, a configuration change or the owner's
//! reset; the models page says where it stands.

use std::time::Duration;

use crate::hf::fmt_bytes;
use crate::runtime::registry::RuntimeState;
use crate::state::SharedState;

use super::super::VramScheduler;
use super::learn::Stored;
use super::{resident_key, SETTLE_AFTER};

/// How often a container is read while a request runs on it: the image
/// sampler's in-flight rate ([`crate::vram::peak::IN_FLIGHT_INTERVAL`]),
/// which the WP7 live gate's independent 20 Hz sampler is compared with.
pub const SAMPLE_INTERVAL: Duration = crate::vram::peak::IN_FLIGHT_INTERVAL;

/// Ends a sampler's stretch however its task ends, so the generation can be
/// sampled again.
struct Watching<'a> {
    vram: &'a VramScheduler,
    generation: u64,
    epoch: u64,
    done: bool,
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.vram
                .residency
                .end_sampling(self.generation, self.epoch);
        }
        self.vram
            .residency
            .stretches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl VramScheduler {
    /// An inference request is about to be sent on an audio hold: watch the
    /// container while it runs, unless a sampler already does, the row's
    /// figure has settled, the container runs a previous configuration, or
    /// this host has no per-process figures.
    pub(crate) fn audio_sending(&self, state: &SharedState, generation: u64, model_id: &str) {
        if self.probe().process_support().is_err() {
            return;
        }
        let snap = state.snapshot();
        let Some(row) = snap.audio_models.iter().find(|m| m.model_id == model_id) else {
            return;
        };
        let key = resident_key(row, &snap.settings.audio);
        if self.residency.settled(model_id, &key) {
            return;
        }
        // Not a container on the CPU: it holds nothing on the card to sample.
        let runs_key = state.runtime().list().iter().any(|e| {
            e.generation == generation
                && e.state == RuntimeState::Ready
                && e.placement.is_gpu()
                && e.resident_key.as_deref() == Some(key.as_str())
        });
        if !runs_key || !self.residency.begin_sampling(generation) {
            return;
        }
        let epoch = self.residency.epoch(model_id);
        let st = state.clone();
        let model_id = model_id.to_string();
        tokio::spawn(async move {
            st.vram
                .sample_stretch(&st, generation, &model_id, &key, epoch)
                .await
        });
    }

    async fn sample_stretch(
        &self,
        state: &SharedState,
        generation: u64,
        model_id: &str,
        key: &str,
        epoch: u64,
    ) {
        let mut watching = Watching {
            vram: self,
            generation,
            epoch,
            done: false,
        };
        let Ok(pids) = self.container_pids(state, generation, model_id).await else {
            // The reading after the answer says why, and tries again.
            return;
        };
        loop {
            if let Ok(Some(bytes)) = self.read_processes(&pids, false).await {
                if bytes > 0 {
                    self.residency.sample(generation, epoch, bytes);
                }
            }
            self.residency
                .samples
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let in_flight = state.runtime().list().iter().any(|e| {
                e.generation == generation && e.state == RuntimeState::Ready && e.in_flight > 0
            });
            if !in_flight {
                break;
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
        watching.done = true;
        let stretch = self.residency.end_sampling(generation, epoch);
        let (true, Some(max)) = (stretch.answered, stretch.max) else {
            return;
        };
        let stored = self
            .store_resident(state, generation, model_id, max, epoch)
            .await;
        if stored == Stored::Dropped {
            return;
        }
        // Another's store since the last stretch checked — the reading after
        // an answer folds these samples in and may have stored them first —
        // or its own. Asked first, so the check is always recorded; its own
        // store is still asked after it, for a generation forgotten
        // meanwhile (its container left), whose raise nothing counts.
        let raised = self
            .residency
            .raised_since(generation, stretch.raises_at_begin);
        self.residency
            .stretch_done(model_id, key, raised || stored == Stored::Raised);
        if self.residency.calm(model_id, key) == SETTLE_AFTER {
            tracing::info!(
                "audio/{model_id}: {SETTLE_AFTER} sampled requests in a row did not raise its \
                 residency (largest sample {}) — its requests are not sampled any more",
                fmt_bytes(max)
            );
        }
    }
}
