//! The speculative type a drafter without one is started with (device-ledger
//! design ruling 18, WP10).
//!
//! llama-server's `--spec-type` defaults to `none`: a row that sets a
//! drafter and no type starts a server that loads the drafter and frees it at
//! once. lmgw renders `--spec-type draft-simple` for such a row, where the
//! image's `--help` lists that value among `--spec-type`'s. Where it cannot
//! tell (the help is unread, or the engine is native and has none to read)
//! nothing is rendered, as before ruling 18.

use super::*;
use crate::runtime::descriptor::ModelRuntime;

impl Registry {
    /// Remember what image `image`'s `--help` says about `draft-simple`.
    pub(super) fn note_draft_simple(&self, image: &str, help: &str) {
        let has = crate::llama_caps::parse_help(help)
            .values_for("spec-type")
            .is_some_and(|v| {
                v.iter()
                    .any(|v| v == crate::runtime::argv::DEFAULT_DRAFT_SPEC_TYPE)
            });
        self.draft_simple
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(image.to_string(), has);
    }

    /// `rt` with the type a drafter without one gets, for a start or an
    /// adoption: reads the image's `--help` (cached per image ID) when, and
    /// only when, the row needs the type. `None` where it needs none, where
    /// the image does not list `draft-simple`, or where the help cannot be
    /// read.
    pub async fn with_draft_type(
        &self,
        container_prefix: &str,
        rt: &ModelRuntime,
    ) -> Option<ModelRuntime> {
        let drafted = rt.with_default_spec_type()?;
        if !self.launcher.runs_containers() {
            return None;
        }
        self.help_text(container_prefix, &rt.image, &rt.extra_run_args)
            .await
            .ok()?;
        self.image_has_draft_simple(&rt.image).then_some(drafted)
    }

    /// [`Self::with_draft_type`] from what the last read of `rt`'s image said,
    /// without reading: for a surface that cannot wait on podman (the
    /// editor's command line). `None` until an image was read.
    pub fn with_draft_type_known(&self, rt: &ModelRuntime) -> Option<ModelRuntime> {
        let drafted = rt.with_default_spec_type()?;
        self.image_has_draft_simple(&rt.image).then_some(drafted)
    }

    fn image_has_draft_simple(&self, image: &str) -> bool {
        self.draft_simple
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(image)
            .copied()
            == Some(true)
    }
}
