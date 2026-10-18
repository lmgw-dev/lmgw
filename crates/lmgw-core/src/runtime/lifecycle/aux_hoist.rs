//! The aux hoist's boot fold (decision-models design §3.3, WP2b): move the
//! placement and batching flags existing aux rows carry in their freeform
//! `args` into the five fields, once, with a start notice naming each row.
//!
//! It runs first in [`super::boot`], before reconciliation, so a row whose
//! container command or run args it changed meets its old container with the
//! new ones and replaces it rather than adopting it, once. The notice says
//! which: it renders both sides of what adoption compares, because a moved
//! flag often renders where it rendered before (`-ngl 0 --threads 16` from
//! the fields is the same argv), and a dropped copy renders nothing. It writes the hoisted
//! rows back ([`store::store_aux_hoist`]), so every later load reads them as
//! they now are, and a later start finds nothing left to move. A row that
//! keeps its `--n-gpu-layers 0` (a GPU image, or one lmgw could not read) is
//! judged again at each start, because its image may change, and its notice
//! is said again with it.
//!
//! Not a fold on every snapshot load, as the chat hoist is
//! (`config::hoist_promoted_args_into`): what becomes of `--n-gpu-layers 0`
//! depends on the image's files, a podman read the load cannot make. The
//! rule itself is [`crate::config::AuxModel::hoist_args`]; what an image
//! carries is [`crate::backends::images::image_gpu`].

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{AuxModel, HoistedContainer, ImageGpu, Snapshot};
use crate::runtime::argv::{render_engine_args, run_args_digest};
use crate::runtime::aux as aux_rt;
use crate::runtime::descriptor::aux_runtime;
use crate::state::SharedState;
use crate::store;

/// Hoist every aux row's flags (module doc), write the changed rows back,
/// publish the snapshot when one changed, and log one notice per row it
/// moved or kept a flag of. Returns the notices, in row order.
pub async fn hoist_aux_args(state: &SharedState) -> Vec<String> {
    let snap = state.snapshot();
    // The effective image: the row's own, else the class's, as the
    // descriptor starts it (`runtime::descriptor`'s `aux_runtime`).
    let image_of = |m: &AuxModel| {
        m.image
            .clone()
            .unwrap_or_else(|| snap.settings.aux_router.image.clone())
    };
    let wanted: BTreeSet<String> = snap
        .aux_models
        .iter()
        .filter(|m| m.hoist_reads_image())
        .map(image_of)
        .collect();
    let gpu: BTreeMap<String, ImageGpu> = if wanted.is_empty() {
        BTreeMap::new()
    } else {
        crate::backends::images::image_gpu(state, &wanted).await
    };
    let mut notices = Vec::new();
    let mut wrote = false;
    for was in &snap.aux_models {
        let image = image_of(was);
        // Read only for a row that asks (`hoist_reads_image`); any other row
        // never consults it.
        let facts = gpu
            .get(&image)
            .cloned()
            .unwrap_or_else(|| ImageGpu::Unknown("its image was not read".into()));
        let mut now = was.clone();
        let hoist = now.hoist_args(&facts);
        if hoist.changed() {
            match store::store_aux_hoist(&state.db, was, &now).await {
                Ok(true) => wrote = true,
                Ok(false) => {
                    tracing::info!(
                        "aux model '{}' was saved while its extra args were being hoisted into \
                         its fields; the next start hoists what that save left",
                        was.model_id
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        "aux model '{}': hoisting its extra args into its fields could not be \
                         stored ({e}); it runs as before, and the next start tries again",
                        was.model_id
                    );
                    continue;
                }
            }
        }
        let render = |m: &AuxModel| rendered(state, &snap, m);
        let container = HoistedContainer {
            // What boot adoption compares; `None` (no spec) on both sides is
            // no evidence, so a changed row then counts as replaced.
            replaced: match (render(was), render(&now)) {
                (Some(a), Some(b)) => a != b,
                _ => hoist.changed(),
            },
            gets_gpu: aux_rt::passes_gpu(&aux_rt::run_args(&now, &snap.settings.aux_router)),
        };
        if let Some(text) = hoist.notice(&was.model_id, &image, &facts, container) {
            tracing::info!("{text}");
            notices.push(text);
        }
    }
    if wrote {
        if let Err(e) = state.publish_snapshot().await {
            tracing::warn!(
                "aux rows were hoisted, but the configuration could not be reloaded ({e}); \
                 they run as hoisted from the next reload on"
            );
        }
    }
    notices
}

/// What boot adoption compares for row `m` (`Registry`'s reconcile): the
/// container command, and the digest of its run args. The port and the dirs
/// are the same on both sides of a comparison, so only the row tells.
fn rendered(state: &SharedState, snap: &Snapshot, m: &AuxModel) -> Option<(Vec<String>, String)> {
    let spec = aux_runtime(snap, m)
        .preview_spec(
            &snap.settings.container_prefix,
            0,
            &snap.settings.aux_router.models_dir,
            &state.data_dir,
        )
        .ok()?;
    Some((
        render_engine_args(&spec),
        run_args_digest(&spec.extra_run_args),
    ))
}
