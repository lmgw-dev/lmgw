//! Where an audio row's files are on the host: its model root, the weights
//! GGUF audio.cpp is taken to load, and the voices its root carries.
//!
//! Shared by the VRAM residency (the eager row's loaded weights, WP7 review)
//! and the speech profile ([`super::profile`]), so both pick the same file.

use std::path::{Path, PathBuf};

use crate::config::AudioModel;

pub mod container_view;
mod direct;
pub use direct::direct_gguf;

use container_view::Seen;

/// The row's model root on the host: `<models_dir>/<path>`, which the
/// container sees as `/models/<path>`.
pub fn row_root(models_dir: &str, m: &AudioModel) -> PathBuf {
    PathBuf::from(models_dir).join(m.path.trim_start_matches('/'))
}

/// The weights GGUF a row is taken to load, with its size. lmgw cannot ask
/// audio.cpp which file it picked, so the rule is: the file [`row_ggufs`]
/// knows audio.cpp loads, when it knows one; else the GGUF files under the
/// model root; with a `weight_id`, the ones whose name contains it (`q8_0` →
/// `…-q8_0.gguf`, compared without case and with `-` and `_` alike); and of
/// those, the **smallest**. The residency's reason for the smallest — a
/// smaller figure keeps more pending, the safe direction — does not matter
/// to the profile: every quantization of one package carries the same
/// embedded spec and files. `None`: no GGUF (or none matching). `family`:
/// the row's, for [`direct_gguf`]. `models_dir`: the models dir `root` is
/// under, which the container mounts ([`container_view`]).
pub fn row_gguf(
    models_dir: &Path,
    root: &Path,
    weight_id: Option<&str>,
    family: &str,
) -> Option<(PathBuf, u64)> {
    row_ggufs(models_dir, root, weight_id, family)
        .into_iter()
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
}

/// Every GGUF [`row_gguf`] picks from: the ones under the model root, narrowed
/// by `weight_id` when it is set. More than one means the pick is the
/// smallest-file guess, which is what the audio catalog calls a variant it
/// cannot tell apart. That file alone — it is what loads — when audio.cpp
/// is handed one file instead of the directory ([`direct_gguf`]), when the
/// directory has a top-level `model.gguf` (audio.cpp's
/// `find_directory_gguf` takes it whatever sits beside it, so the
/// `weight_id` narrows nothing), or when the row's path is a GGUF file itself
/// (audio.cpp loads a file path as it is). Every GGUF is counted as the
/// container sees it ([`container_view`]): a link out of the models dir is
/// none.
pub fn row_ggufs(
    models_dir: &Path,
    root: &Path,
    weight_id: Option<&str>,
    family: &str,
) -> Vec<(PathBuf, u64)> {
    let loads = direct_gguf(models_dir, root, weight_id, family)
        .or_else(|| model_gguf(models_dir, root, family))
        .or_else(|| gguf_file(models_dir, root));
    if let Some(file) = loads {
        return vec![(file.clone(), seen_size(models_dir, &file))];
    }
    let mut ggufs: Vec<(PathBuf, u64)> = Vec::new();
    collect_ggufs(models_dir, root, &mut ggufs);
    let wanted = weight_id
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(normalise);
    ggufs
        .into_iter()
        .filter(|(p, _)| {
            wanted.as_ref().is_none_or(|w| {
                p.file_name()
                    .is_some_and(|n| normalise(&n.to_string_lossy()).contains(w))
            })
        })
        .collect()
}

/// `root` itself, when it is a GGUF file rather than a directory.
fn gguf_file(models_dir: &Path, root: &Path) -> Option<PathBuf> {
    let gguf = root
        .file_name()
        .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().ends_with(".gguf"));
    (gguf && matches!(container_view::seen(models_dir, root), Seen::File(_)))
        .then(|| root.to_path_buf())
}

/// The directory's top-level `model.gguf`, which audio.cpp loads whenever it
/// is there — except in a family that picks its file by `weight` itself
/// ([`super::families::reads_weight`]).
fn model_gguf(models_dir: &Path, root: &Path, family: &str) -> Option<PathBuf> {
    if super::families::reads_weight(family) {
        return None;
    }
    let file = root.join("model.gguf");
    matches!(container_view::seen(models_dir, &file), Seen::File(_)).then_some(file)
}

/// The size of the file the container finds at `p`; 0 when it finds none.
fn seen_size(models_dir: &Path, p: &Path) -> u64 {
    match container_view::seen(models_dir, p) {
        Seen::File(at) => std::fs::metadata(at).map_or(0, |m| m.len()),
        Seen::NotFile | Seen::Outside(_) => 0,
    }
}

fn normalise(s: &str) -> String {
    s.to_ascii_lowercase().replace('-', "_")
}

fn collect_ggufs(models_dir: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => collect_ggufs(models_dir, &e.path(), out),
            Ok(_) => {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.to_ascii_lowercase().ends_with(".gguf") {
                    continue;
                }
                if let Seen::File(at) = container_view::seen(models_dir, &e.path()) {
                    out.push((e.path(), std::fs::metadata(at).map_or(0, |m| m.len())));
                }
            }
            Err(_) => {}
        }
    }
}

/// The voices the model root carries as files: `<root>/embeddings/
/// <name>.safetensors`, what audio.cpp's own voice list reads (Pocket TTS
/// keeps its built-in voices there). Sorted.
pub fn embedding_voices(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("embeddings")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".safetensors").map(str::to_string)
        })
        .collect();
    out.sort();
    out
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
    fn the_selected_variant_and_the_embedded_voices() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "model-q8_0.gguf", 800);
        file(d.path(), "model-f16.gguf", 1600);
        file(d.path(), "tokenizer.json", 5);
        let pick = |w| {
            row_gguf(d.path(), d.path(), w, "pocket_tts")
                .map(|(p, s)| (p.file_name().unwrap().to_owned(), s))
        };
        assert_eq!(pick(Some("F16")), Some(("model-f16.gguf".into(), 1600)));
        assert_eq!(pick(Some("q8-0")), Some(("model-q8_0.gguf".into(), 800)));
        assert_eq!(pick(None), Some(("model-q8_0.gguf".into(), 800)));
        assert_eq!(pick(Some("q4_k")), None);

        // The candidates the pick is made from: all of them with no
        // weight_id, the matching ones with one, none for a mismatch.
        std::fs::create_dir(d.path().join("sub")).unwrap();
        file(&d.path().join("sub"), "codec-q8_0.gguf", 10);
        let names = |w| {
            let mut v: Vec<String> = row_ggufs(d.path(), d.path(), w, "pocket_tts")
                .into_iter()
                .map(|(p, _)| p.strip_prefix(d.path()).unwrap().display().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            names(None),
            ["model-f16.gguf", "model-q8_0.gguf", "sub/codec-q8_0.gguf"]
        );
        assert_eq!(names(Some(" ")), names(None), "a blank weight_id is none");
        // Two GGUFs at the top and a weight_id that picks one of them:
        // audio.cpp is handed that file, so it alone is what the row loads
        // — not the codec under it that also matches.
        assert_eq!(names(Some("Q8-0")), ["model-q8_0.gguf"]);
        assert!(names(Some("q4_k")).is_empty());
        std::fs::remove_dir_all(d.path().join("sub")).unwrap();

        assert!(embedding_voices(d.path()).is_empty());
        std::fs::create_dir(d.path().join("embeddings")).unwrap();
        file(&d.path().join("embeddings"), "alba.safetensors", 1);
        file(&d.path().join("embeddings"), "notes.txt", 1);
        assert_eq!(embedding_voices(d.path()), ["alba"]);
    }

    /// A directory with a top-level `model.gguf`: audio.cpp loads it
    /// whatever sits beside it, so it alone is what the row loads — the
    /// served chip, the residency's figure and the speech profile read it,
    /// not the file the `weight_id` names. A family that picks by `weight`
    /// itself is the exception.
    #[test]
    fn a_model_gguf_is_what_the_directory_loads() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "model.gguf", 900);
        file(d.path(), "x-f16.gguf", 1600);
        assert_eq!(
            row_ggufs(d.path(), d.path(), Some("f16"), "cohere_asr"),
            [(d.path().join("model.gguf"), 900)]
        );
        assert_eq!(
            row_gguf(d.path(), d.path(), None, "cohere_asr"),
            Some((d.path().join("model.gguf"), 900))
        );
        assert_eq!(
            row_ggufs(d.path(), d.path(), Some("f16"), "silero_vad"),
            [(d.path().join("x-f16.gguf"), 1600)]
        );
        // A model.gguf linked out of the models dir is none to the container.
        let store = tempfile::tempdir().unwrap();
        file(store.path(), "blob", 50);
        std::fs::remove_file(d.path().join("model.gguf")).unwrap();
        std::os::unix::fs::symlink(store.path().join("blob"), d.path().join("model.gguf")).unwrap();
        assert_eq!(
            row_ggufs(d.path(), d.path(), Some("f16"), "cohere_asr"),
            [(d.path().join("x-f16.gguf"), 1600)]
        );
    }

    /// A row whose path is the GGUF file itself: audio.cpp loads a file
    /// path as it is, so that file is what the row loads.
    #[test]
    fn a_row_path_that_is_a_gguf_file_is_that_file() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "parakeet-q8_0.gguf", 700);
        let path = d.path().join("parakeet-q8_0.gguf");
        assert_eq!(
            row_ggufs(d.path(), &path, None, "parakeet_tdt"),
            [(path.clone(), 700)]
        );
        assert_eq!(
            row_gguf(d.path(), &path, Some("f16"), "parakeet_tdt"),
            Some((path, 700))
        );
        file(d.path(), "notes.txt", 1);
        assert!(row_ggufs(d.path(), &d.path().join("notes.txt"), None, "parakeet_tdt").is_empty());
    }
}
