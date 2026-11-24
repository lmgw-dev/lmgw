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
//!
//! **A split** (device-ledger design §7.2, D20, WP6) is asked of the same
//! `--help`: lmgw splits a chat model over several cards only where its
//! llama-server lists `--split-mode` and `--tensor-split`, and one with a
//! draft model only where it also lists the drafter's device (`-devd`),
//! which confines the drafter to the first card. Read beside `--fit` for a
//! row that sets its context on a machine of several cards; a native start
//! is never split (lmgw hands it no card).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::Snapshot;
use crate::runtime::Class;
use crate::state::SharedState;

use super::{Footprint, VramScheduler};

/// What each image reference's llama-server said about `--fit` and a
/// split.
#[derive(Debug, Default)]
pub(super) struct FitVocab(Mutex<HashMap<String, Vocab>>);

/// What one image's llama-server `--help` lists of what lmgw renders over a
/// row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Vocab {
    fit: bool,
    /// `--split-mode` and `--tensor-split`.
    split: bool,
    /// The drafter's device (`-devd`).
    devd: bool,
}

/// What a split may be told, as an image's `--help` lists it (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SplitVocab {
    /// `--fit` is there to be turned off.
    pub fit: bool,
    /// A drafter can be confined to the first card.
    pub devd: bool,
}

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
            .insert(
                image.to_string(),
                Vocab {
                    fit: caps.supports("fit"),
                    split: caps.supports("split-mode") && caps.supports("tensor-split"),
                    devd: caps.supports("devd") || caps.supports("spec-draft-device"),
                },
            );
    }

    /// What chat row `model_id`'s llama-server lists of a split's flags, as
    /// last read; `Err` says why it cannot be split.
    pub(crate) fn split_vocab(
        &self,
        snap: &Snapshot,
        model_id: &str,
    ) -> Result<SplitVocab, String> {
        let image = chat_image(snap, model_id).ok_or("it has no image")?;
        match self
            .fit_vocab
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&image)
        {
            Some(v) if v.split => Ok(SplitVocab {
                fit: v.fit,
                devd: v.devd,
            }),
            Some(_) => Err(format!(
                "its image's llama-server ({image}) has no --tensor-split"
            )),
            None => Err(format!(
                "what its image's llama-server ({image}) can be told was not read yet"
            )),
        }
    }

    /// What image `image`'s llama-server lists of a split's flags, as last
    /// read: what a new chat model of the class could be told (device-ledger
    /// design WP12's fit marks). `Err` says why it cannot be split.
    pub(crate) fn class_split_vocab(&self, image: &str) -> Result<SplitVocab, String> {
        match self
            .fit_vocab
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(image)
        {
            Some(v) if v.split => Ok(SplitVocab {
                fit: v.fit,
                devd: v.devd,
            }),
            Some(_) => Err(format!(
                "the chat class's llama-server ({image}) has no --tensor-split"
            )),
            None => Err(format!(
                "what the chat class's llama-server ({image}) can be told was not read yet: \
                 the image is not on this computer, or podman could not run it"
            )),
        }
    }

    /// Read the chat class's image vocabulary where it was not read yet, for
    /// a fit mark over several cards (WP12). A failed read leaves it unread.
    pub(crate) async fn read_class_vocab(&self, state: &SharedState, snap: &Snapshot) {
        let r = &snap.settings.router;
        let known = self
            .fit_vocab
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&r.image);
        if known || self.engine_build(Class::Chat).is_some() {
            return;
        }
        match state
            .runtime()
            .help_text(&snap.settings.container_prefix, &r.image, &r.extra_run_args)
            .await
        {
            Ok(help) => self.note_llama_vocab(&r.image, &crate::llama_caps::parse_help(&help)),
            Err(e) => tracing::debug!(
                "fit marks: what '{}' can be told of a split is not known: {e}",
                r.image
            ),
        }
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
            Some(v) if v.fit => Knows::Yes,
            Some(_) => Knows::No,
            None => Knows::NotRead,
        }
    }

    /// Read chat row `model_id`'s image vocabulary for a decision (module
    /// doc): only for a row `--fit` would shrink or one lmgw may split (a
    /// set context, several cards), and only where it runs in a container.
    /// A failed read keeps what was read before.
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
        let may_split = fp.by_layer.is_some()
            && crate::vram::plan::chat::sets_context(row)
            && self.last_devices.lock().unwrap().len() > 1;
        if fp.llama.is_none() && !may_split {
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

#[cfg(test)]
mod tests {
    /// The image the fakes answer with lists a split's flags and the
    /// drafter's device (D20).
    #[test]
    fn a_split_s_flags_are_read_from_the_help() {
        let caps = crate::llama_caps::parse_help(include_str!(
            "../../tests/fixtures/llama/llama-server-help-4b1a27fa0.txt"
        ));
        assert!(caps.supports("split-mode"));
        assert!(caps.supports("tensor-split"));
        assert!(caps.supports("devd") || caps.supports("spec-draft-device"));
    }
}
