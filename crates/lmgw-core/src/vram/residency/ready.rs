//! The reading at rest (WP7 review M4): one per-process reading of an audio
//! container that has become ready — started, warmed, or adopted from a
//! previous lmgw — while it has answered nothing and has nothing in flight.
//!
//! **Why.** A ready container that has not loaded its model already holds
//! its CUDA context, and the driver's `used` shows it. Charging the whole
//! expected residency as pending on top counted that context twice — about
//! 0.5–1 GB on a realtime warm-up, and the whole 5.7 GB of the three voice
//! models after a restart, whose adopted containers had long loaded theirs.
//! So what such a container is still to take is its expected residency
//! minus what it holds at rest ([`super::pending`]). Nothing is stored: the
//! figure belongs to this container, not to the row.

use std::time::Instant;

use crate::hf::fmt_bytes;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::{broadcast, VramScheduler};
use super::tracker::AtRest;
use super::{is_eager, looks_loaded};

impl VramScheduler {
    /// Read every ready audio container that has not been read at rest, has
    /// answered nothing and has nothing in flight — once per generation.
    /// Called wherever a container becomes ready ([`Self::cache_pids`]), boot's
    /// adoption included, and not gated by the outside-VRAM trigger: it learns
    /// about a container, whatever those switches say.
    pub(in crate::vram) fn read_audio_at_rest(&self, state: &SharedState) {
        if self.probe().process_support().is_err() {
            // Every note says why instead, and the whole figure is charged.
            return;
        }
        for e in state.runtime().list() {
            if e.class != Class::Audio || e.state != RuntimeState::Ready || e.in_flight > 0 {
                continue;
            }
            // A container on the CPU holds nothing on the card to read.
            if !e.placement.is_gpu() {
                continue;
            }
            if !self.residency.begin_at_rest(e.generation) {
                continue;
            }
            let st = state.clone();
            tokio::spawn(async move { st.vram.at_rest_reading(&st, e).await });
        }
    }

    async fn at_rest_reading(&self, state: &SharedState, e: RuntimeView) {
        let snap = state.snapshot();
        let row = snap.audio_models.iter().find(|m| m.model_id == e.model_id);
        let Some(row) = row else {
            self.residency
                .at_rest(e.generation, AtRest::Unread, Instant::now());
            return;
        };
        let on_disk = self
            .plans
            .audio(&snap.settings.audio.models_dir, row)
            .await
            .weights_bytes;
        let eager = is_eager(row, &snap.settings.audio);
        let read = match self.container_pids(state, e.generation, &e.model_id).await {
            Ok(pids) => self.read_processes(&pids, true).await,
            Err(why) => Err(why),
        };
        // A request that arrived meanwhile may be loading the model: what
        // was read is no figure of the container at rest.
        let busy = state
            .runtime()
            .list()
            .iter()
            .any(|x| x.generation == e.generation && x.in_flight > 0);
        let result = match read {
            _ if busy => AtRest::Unread,
            // None listed: it holds nothing on the card yet.
            Ok(bytes) => {
                let bytes = bytes.unwrap_or(0);
                match (looks_loaded(bytes, on_disk), eager) {
                    (false, _) => AtRest::Bare(bytes),
                    (true, true) => AtRest::Eager(bytes),
                    (true, false) => {
                        tracing::info!(
                            "audio/{}: its processes hold {} at rest, more than a bare context \
                             — taken as loaded",
                            e.model_id,
                            fmt_bytes(bytes)
                        );
                        AtRest::Loaded(bytes)
                    }
                }
            }
            Err(why) => {
                tracing::info!(
                    "audio/{}: what its container holds at rest could not be read — {why}; its \
                     whole expected residency stays pending",
                    e.model_id
                );
                AtRest::Failed(why)
            }
        };
        self.residency.at_rest(e.generation, result, Instant::now());
        // The free figure changes with it, and no request tells the
        // dashboard.
        broadcast(state);
    }
}
