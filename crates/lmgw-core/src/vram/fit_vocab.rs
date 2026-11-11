//! Whether a chat row's llama-server has `--fit` (`vram::partial::llama`).
//!
//! ik_llama.cpp and llama.cpp builds older than `--fit` size nothing to the
//! card: they load the row as written, at its trained context with every
//! layer on the card where `--n-gpu-layers` is on auto. So admission fits a
//! row in `--fit`'s place only where its image's `--help` lists `--fit`
//! (`llama_caps`); anywhere else the row is charged as written and nothing
//! is rendered over it.
//!
//! **Read.** The flag vocabulary is read per image (`Registry::help_text`,
//! cached by image ID), by every admission decision of a row `--fit` would
//! shrink, and by `local_model_get`/`local_model_set`'s checks; what it said
//! is kept here per image reference for the reads that cannot ask podman
//! (the ledger, status polls). Until one was read, a row is charged as
//! written, and its footprint says why.
//!
//! **Native.** The native launcher (Windows) runs the llama.cpp build lmgw
//! installs, whose `--help` is not read yet; it is official llama.cpp at a
//! build with `--fit` (b12244 on a Windows test VM, 2026-11-11), so it counts as
//! having it.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::Snapshot;
use crate::runtime::Class;
use crate::state::SharedState;

use super::{Footprint, VramScheduler};

/// What each image reference's llama-server said about `--fit`.
#[derive(Debug, Default)]
pub(super) struct FitVocab(Mutex<HashMap<String, bool>>);

/// Whether `--fit` can be relied on for a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Knows {
    Yes,
    No,
    NotRead,
}

impl VramScheduler {
    /// Remember what image `image`'s llama-server `--help` said about
    /// `--fit` (module doc).
    pub fn note_llama_vocab(&self, image: &str, caps: &crate::llama_caps::LlamaCaps) {
        self.fit_vocab
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(image.to_string(), caps.supports("fit"));
    }

    /// Whether chat row `model_id`'s llama-server has `--fit`, as last read.
    pub(crate) fn knows_fit(&self, snap: &Snapshot, model_id: &str) -> Knows {
        if self.engine_build(Class::Chat).is_some() {
            // The native launcher's own build (module doc).
            return Knows::Yes;
        }
        let Some(image) = chat_image(snap, model_id) else {
            return Knows::NotRead;
        };
        match self
            .fit_vocab
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&image)
        {
            Some(true) => Knows::Yes,
            Some(false) => Knows::No,
            None => Knows::NotRead,
        }
    }

    /// Read chat row `model_id`'s image vocabulary for a decision (module
    /// doc): only for a row `--fit` would shrink, and only where it runs in
    /// a container. A failed read keeps what was read before.
    pub(crate) async fn read_fit_vocab(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &super::Target,
    ) {
        if target.class != Class::Chat || self.engine_build(Class::Chat).is_some() {
            return;
        }
        let model_id = target.model_id.as_str();
        let Some(row) = snap.local_models.iter().find(|m| m.model_id == model_id) else {
            return;
        };
        let fp = self
            .plans
            .local(&snap.settings.router.models_dir, row)
            .await;
        if fp.llama.is_none() {
            return;
        }
        let Some(rt) = crate::runtime::descriptor::model_runtime(snap, Class::Chat, model_id)
        else {
            return;
        };
        match state
            .runtime()
            .help_text(
                &snap.settings.container_prefix,
                &rt.image,
                &rt.extra_run_args,
            )
            .await
        {
            Ok(help) => self.note_llama_vocab(&rt.image, &crate::llama_caps::parse_help(&help)),
            Err(e) => tracing::debug!(
                "VRAM admission: whether '{}' has --fit is not known: {e}",
                rt.image
            ),
        }
    }

    /// `fp` for chat row `model_id`, without its sizer where its
    /// llama-server is not known to have `--fit` (module doc): charged as
    /// written, with the reason on its note.
    pub(super) fn gate_fit(&self, snap: &Snapshot, model_id: &str, mut fp: Footprint) -> Footprint {
        if fp.llama.is_none() {
            return fp;
        }
        let why = match self.knows_fit(snap, model_id) {
            Knows::Yes => return fp,
            Knows::No => {
                "its image's llama-server has no --fit (ik_llama.cpp, or a build older \
                          than it), so it loads as written and is charged so"
            }
            Knows::NotRead => {
                "whether its image's llama-server has --fit was not read yet, so \
                               it is charged as written until a start reads it"
            }
        };
        fp.llama = None;
        fp.note = Some(match fp.note.take() {
            Some(n) => format!("{n}. {why}"),
            None => why[..1].to_uppercase() + &why[1..],
        });
        fp
    }
}

/// The image chat row `model_id` runs: its own, else the class's.
fn chat_image(snap: &Snapshot, model_id: &str) -> Option<String> {
    crate::runtime::descriptor::model_runtime(snap, Class::Chat, model_id).map(|rt| rt.image)
}
