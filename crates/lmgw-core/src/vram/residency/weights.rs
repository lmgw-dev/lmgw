//! The weights file an eager audio row has on the card once it is ready
//! (WP7 review, low): what [`super::pending`] subtracts for an eager row
//! whose container could not be read at rest.
//!
//! The model directory's size is the plan's figure, and it is the wrong one
//! here: a directory may hold several weight variants (a q8_0 and an f16 of
//! the same model), of which audio.cpp loads one — so the directory's size
//! overstates what is on the card, and an overstated figure understates what
//! is still pending. lmgw cannot ask audio.cpp which file it picked, so the
//! rule is: the GGUF files under the model root; with a `weight_id`, the
//! ones whose name contains it (`q8_0` → `…-q8_0.gguf`, compared without
//! case and with `-` and `_` alike); and of those, the **smallest** — a
//! smaller figure keeps more pending, the safe direction. No GGUF at all
//! (or no match) is 0: the whole expected residency is pending, as for a
//! lazy row.

use std::path::Path;

/// The size of the weights file an eager row is taken to have loaded — the
/// file [`crate::audio::files::row_gguf`] picks, which the speech profile
/// reads its facts from too.
pub(in crate::vram) fn selected_weights_bytes(
    models_dir: &Path,
    root: &Path,
    weight_id: Option<&str>,
    family: &str,
) -> u64 {
    crate::audio::files::row_gguf(models_dir, root, weight_id, family).map_or(0, |(_, size)| size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str, len: u64) {
        std::fs::File::create(dir.join(name))
            .unwrap()
            .set_len(len)
            .unwrap();
    }

    #[test]
    fn the_selected_variant_not_the_directory() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "model-q8_0.gguf", 800);
        file(d.path(), "model-f16.gguf", 1600);
        file(d.path(), "tokenizer.json", 5);
        assert_eq!(
            selected_weights_bytes(d.path(), d.path(), Some("F16"), "pocket_tts"),
            1600
        );
        assert_eq!(
            selected_weights_bytes(d.path(), d.path(), Some("q8-0"), "pocket_tts"),
            800
        );
        assert_eq!(
            selected_weights_bytes(d.path(), d.path(), None, "pocket_tts"),
            800,
            "no weight id: the smallest, so more stays pending"
        );
        assert_eq!(
            selected_weights_bytes(d.path(), d.path(), Some("q4_k"), "pocket_tts"),
            0
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            selected_weights_bytes(empty.path(), empty.path(), None, "pocket_tts"),
            0
        );
    }
}
