//! The context of a partial start ([`super`]): a pipeline's learned idle
//! residency, `ImageModel::idle_bytes` (migration 0103) — sd-server's own
//! memory outside its `--max-vram` budget. No working budget: that is
//! [`super::work`]'s.
//!
//! **What is read.** One per-process reading of an image container that is
//! up and has nothing in flight — what the driver lists for its init and
//! every process under it ([`crate::vram::attribution`]) — for a row whose
//! weights all wait off the card (`offload_to_cpu`, a whole `params_backend
//! cpu|disk`). What such a container holds at rest is sd-server's own
//! context: 1.0 GiB with `offload_to_cpu` on a 24 GiB card under CUDA (the
//! Z-Image-Turbo recipe's note), 5 MiB under Vulkan on a GTX 970 test card (2026-11-11). Any other row's container holds at rest whatever weights
//! auto-fit put on the card, which is no floor, so nothing is read for it.
//!
//! **When.** Once per container, wherever one becomes ready (beside the
//! audio reading at rest, [`crate::vram::VramScheduler::cache_pids`]), while
//! the row has no figure. Nothing is read without per-process figures (no
//! NVML process list, a probe that cannot attribute), and nothing is stored
//! as 0: a container the driver lists nothing for has not made its context
//! yet. A reading during which the container took a claim (its claims
//! counter moved, whether or not the request is still in flight) is no
//! figure at rest, and one taken while the row's files, args, engine image
//! or run args changed belongs to no configuration: neither is written. The
//! figure is learned under the engine the container runs (its entry's
//! engine key, migration 0105) and counts only while the row resolves to
//! that engine; it is reset when the row's files or args change.

use crate::hf::fmt_bytes;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::plan::image::{learned_for, placement};
use super::super::{broadcast, VramScheduler};

impl VramScheduler {
    /// Read every ready image container of a row whose weights all wait off
    /// the card and that has no idle figure yet, with nothing in flight —
    /// once per container (module doc).
    pub(in crate::vram) fn read_image_at_rest(&self, state: &SharedState) {
        if self.probe().process_support().is_err() {
            return;
        }
        let snap = state.snapshot();
        let entries = state.runtime().list();
        let mut read = self.image_at_rest.lock().unwrap();
        read.retain(|g| entries.iter().any(|e| e.generation == *g));
        for e in entries {
            if e.class != Class::Image
                || e.state != RuntimeState::Ready
                || e.in_flight > 0
                || !e.placement.is_gpu()
            {
                continue;
            }
            let Some(row) = snap.image_models.iter().find(|m| m.model_id == e.model_id) else {
                continue;
            };
            // Learned under the engine the container runs, or not at all.
            let Some(key) = e.engine_key.as_deref() else {
                continue;
            };
            if learned_for(row, key).idle.is_some()
                || !placement(&row.args).is_ok_and(|a| a.all_off())
            {
                continue;
            }
            if !read.insert(e.generation) {
                continue;
            }
            let st = state.clone();
            tokio::spawn(async move { st.vram.image_reading_at_rest(&st, e).await });
        }
    }

    async fn image_reading_at_rest(&self, state: &SharedState, e: RuntimeView) {
        let label = format!("image/{}", e.model_id);
        let config = pipeline_config(state, &e.model_id);
        let read = match self
            .container_pids_labeled(state, e.generation, label.clone())
            .await
        {
            Ok(pids) => self.read_processes(&pids, true).await,
            Err(why) => Err(why),
        };
        // A request served meanwhile may have been drawing: what was read is
        // no figure of the container at rest, and the next container is
        // read. Its claims counter says so even when it is over by now.
        let busy = state
            .runtime()
            .list()
            .iter()
            .any(|x| x.generation == e.generation && (x.in_flight > 0 || x.claims != e.claims));
        let bytes = match read {
            _ if busy => return,
            Ok(Some(b)) if b > 0 => b,
            Ok(_) => {
                tracing::info!(
                    "{label}: the driver lists nothing for its processes at rest yet, so its \
                     idle residency is not learned from this container"
                );
                return;
            }
            Err(why) => {
                tracing::info!(
                    "{label}: what its container holds at rest could not be read — {why}; its \
                     context stays not measured"
                );
                return;
            }
        };
        let snap = state.snapshot();
        let Some(row) = snap.image_models.iter().find(|m| m.model_id == e.model_id) else {
            return;
        };
        let Some(key) = e.engine_key.as_deref() else {
            return;
        };
        if learned_for(row, key).idle.is_some() {
            return;
        }
        // The row changed while it was read: the reading describes the
        // configuration the container was started with, not this one.
        if config.is_none() || pipeline_config(state, &e.model_id) != config {
            return;
        }
        if let Err(err) = crate::store::set_image_model_learned(
            &state.db,
            row.id,
            key,
            crate::store::LearnedFigure::Idle,
            Some(bytes),
        )
        .await
        {
            tracing::warn!("could not record the idle residency of {label}: {err}");
            return;
        }
        if let Err(err) = state.reload_snapshot().await {
            tracing::warn!("idle residency of {label} recorded but not reloaded: {err}");
            return;
        }
        tracing::info!(
            "{label}: its container holds {} at rest, its weights off the card — VRAM admission \
             now takes that as its context, outside the budget it holds sd-server to",
            fmt_bytes(bytes)
        );
        broadcast(state);
    }
}

/// What a learned figure of image row `model_id` belongs to: its files, its
/// args, its engine image and its run args, as the row says now.
fn pipeline_config(state: &SharedState, model_id: &str) -> Option<String> {
    let snap = state.snapshot();
    let row = snap.image_models.iter().find(|m| m.model_id == model_id)?;
    serde_json::to_string(&(&row.files, &row.args, &row.image, &row.extra_run_args)).ok()
}
