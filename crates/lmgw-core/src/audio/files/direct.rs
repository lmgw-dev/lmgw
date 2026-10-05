//! The one GGUF a row's directory is handed to audio.cpp as.
//!
//! audio.cpp refuses a model directory that holds more than one GGUF at its
//! top level and no `model.gguf` ("model directory contains 2 GGUF files …
//! pass one of them directly"), and the row's `weight` does not choose
//! between them (ASR eval 2, 2026-10-02, audio.cpp 94bd465 and 3b68e0e:
//! Cohere Transcribe q4_0 and q8_0 side by side, CrisperWhisper q4_k and
//! q8_0). The catalog downloads every quantization of a family into one
//! directory, so a second download is enough to break a row that served
//! before it.
//!
//! So lmgw hands audio.cpp one file exactly where audio.cpp would refuse the
//! directory and the row's `weight_id` names the file it means
//! ([`direct_gguf`]):
//! - several GGUFs at the top of the directory, counted as audio.cpp counts
//!   them (`directory_gguf_files`: a regular file after following a
//!   symlink, extension `.gguf` in any case) — inside its container, where
//!   a link is followed against `/models` ([`super::container_view`]);
//! - none of them `model.gguf`, which audio.cpp takes whenever it is there
//!   (`find_directory_gguf`);
//! - one single-GGUF package's quantizations side by side: `weight_id` is
//!   a precision or quantization label ([`is_precision_label`]: `q8_0`,
//!   `q4_k_m`, `f16`, `bf16`, `orig`, …) that — matched the way
//!   [`super::row_ggufs`] narrows, name contains it, case and `-`/`_`
//!   alike — picks exactly one of them, and every top-level GGUF shares
//!   that file's name up to the label and goes on with a label of its own
//!   (`cohere-transcribe-q4_0.gguf` beside `cohere-transcribe-q8_0.gguf`,
//!   `voxcpm2-q8_0.gguf` beside `voxcpm2-orig.gguf`). What follows the
//!   label is not compared: the catalog ships
//!   `qwen3-tts-12hz-1.7b-base-q8_0_v2.gguf` beside `…-bf16.gguf` and
//!   `sortformer-v2.1-f16-mixed.gguf` beside `…-f32.gguf`, and audio.cpp
//!   refuses those directories just the same. A package of several GGUFs
//!   names its components (`text_encoder_q4_k`, `dit`, `video_vae`;
//!   liveavatar's `…-Support-Q4_K_S-F16`, `…-VAE-F16`) and is loaded as a
//!   directory by those names, so it stays one: a sibling that does not
//!   share the picked name up to the label (`yue2-vae-f16` beside
//!   `yue2-3b-q8_0`), or goes on with a component's name past it
//!   (`vocoder.gguf` beside `q8_0.gguf`), is no other quantization, and a
//!   component's name the owner typed as the weight id (`vae`, `support`,
//!   `vocoder`) is no label;
//! - a family that does not choose its weights by `weight` itself
//!   ([`crate::audio::families::reads_weight`]).
//!
//! Everything else stays a directory, as before, and audio.cpp's own
//! refusal names what is wrong rather than lmgw guessing.

use std::path::{Path, PathBuf};

/// The top-level GGUF of `root` that a `family` row's `weight_id` picks,
/// in the one case audio.cpp would refuse the directory (module doc).
/// `None`: hand audio.cpp the directory.
/// `models_dir`: the models dir `root` is under — what the container
/// mounts.
pub fn direct_gguf(
    models_dir: &Path,
    root: &Path,
    weight_id: Option<&str>,
    family: &str,
) -> Option<PathBuf> {
    if crate::audio::families::reads_weight(family) {
        return None;
    }
    let wanted = weight_id
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(super::normalise)?;
    if is_file(models_dir, &root.join("model.gguf")) {
        return None;
    }
    let top: Vec<(PathBuf, String)> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| {
            let name = super::normalise(&p.file_name()?.to_string_lossy());
            (name.ends_with(".gguf") && is_file(models_dir, &p)).then_some((p, name))
        })
        .collect();
    if top.len() < 2 {
        return None;
    }
    if !is_precision_label(&wanted) {
        return None;
    }
    let mut picked = top.iter().filter(|(_, n)| n.contains(&wanted));
    let (one, name) = picked.next()?;
    if picked.next().is_some() {
        return None;
    }
    let stem = &name[..name.find(&wanted)?];
    top.iter()
        .all(|(_, n)| {
            n.strip_prefix(stem)
                .and_then(|rest| rest.strip_suffix(".gguf"))
                .is_some_and(starts_with_label)
        })
        .then(|| one.clone())
}

/// `rest` — a GGUF name past the stem it shares with the picked file —
/// opens with a precision label: one of its `_`-joined beginnings is one
/// (`f16` of `f16_mixed`, `q8_0` of `q8_0_v2`). The label of one package's
/// quantizations is all that tells them apart; what follows it varies.
fn starts_with_label(rest: &str) -> bool {
    rest.match_indices('_')
        .map(|(i, _)| &rest[..i])
        .chain([rest])
        .any(is_precision_label)
}

/// A precision or quantization label as GGUF file names carry them, after
/// [`super::normalise`]: a head of `q`, `iq`, `tq`, `i`, `f`, `bf`, `fp`,
/// `nvfp`, `mxfp` or `int` followed by digits, or `bfloat16`/`float16`/
/// `float32`, then any short tail parts (`q4_k_m`, `iq4_xs`, `i2_s`); or
/// `orig`, the catalog's label for a package kept in the original dtypes
/// (`voxcpm2-orig.gguf` beside `voxcpm2-q8_0.gguf`).
/// A component name (`vae`, `support`, `vocoder`, `dit`) or a size (`3b`)
/// is none — the case the direct-file pick must not fire on.
fn is_precision_label(id: &str) -> bool {
    let mut parts = id.split('_');
    let head = parts.next().unwrap_or_default();
    let numbered = |prefix: &str| {
        head.strip_prefix(prefix)
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    let head_ok = ["q", "iq", "tq", "i", "f", "bf", "fp", "nvfp", "mxfp", "int"]
        .iter()
        .any(|p| numbered(p))
        || matches!(head, "bfloat16" | "float16" | "float32" | "orig");
    head_ok
        && parts.all(|p| (1..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// A regular file once symlinks are followed — audio.cpp's
/// `is_existing_file`, in the container.
fn is_file(models_dir: &Path, p: &Path) -> bool {
    matches!(
        super::container_view::seen(models_dir, p),
        super::container_view::Seen::File(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    fn name(p: Option<PathBuf>) -> Option<String> {
        p.map(|p| p.file_name().unwrap().to_string_lossy().to_string())
    }

    #[test]
    fn a_weight_id_that_picks_one_of_several_top_level_ggufs_names_it() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "cohere-transcribe-q4_0.gguf");
        file(d.path(), "cohere-transcribe-q8_0.gguf");
        file(d.path(), "tokenizer.json");
        let pick = |w| name(direct_gguf(d.path(), d.path(), w, "cohere_asr"));
        assert_eq!(
            pick(Some("q4_0")).as_deref(),
            Some("cohere-transcribe-q4_0.gguf")
        );
        assert_eq!(
            pick(Some("Q8-0")).as_deref(),
            Some("cohere-transcribe-q8_0.gguf"),
            "case and -/_ alike, as row_ggufs matches"
        );
        // No weight_id, one that matches none, one that matches both: the
        // directory, and audio.cpp's own refusal says why.
        assert_eq!(pick(None), None);
        assert_eq!(pick(Some(" ")), None);
        assert_eq!(pick(Some("f16")), None);
        assert_eq!(pick(Some("cohere")), None);
    }

    #[test]
    fn a_directory_audio_cpp_takes_as_it_is_stays_one() {
        let d = tempfile::tempdir().unwrap();
        // One GGUF at the top: audio.cpp loads the directory.
        file(d.path(), "model-q8_0.gguf");
        std::fs::create_dir(d.path().join("codec")).unwrap();
        file(&d.path().join("codec"), "codec-q8_0.gguf");
        let pick = |root: &Path| direct_gguf(root, root, Some("q8_0"), "pocket_tts");
        assert_eq!(pick(d.path()), None, "subdirs are not counted");
        // No GGUF at all, or no directory.
        let empty = tempfile::tempdir().unwrap();
        file(empty.path(), "model.safetensors");
        assert_eq!(pick(empty.path()), None);
        assert_eq!(pick(&d.path().join("missing")), None);
        assert_eq!(
            pick(&d.path().join("model-q8_0.gguf")),
            None,
            "a row whose path is the file already"
        );
    }

    /// The cases audio.cpp would not refuse: a `model.gguf` it takes
    /// whatever sits beside it, a package of several named GGUFs it loads
    /// as a directory, and a family that chooses by `weight` itself.
    #[test]
    fn only_a_directory_audio_cpp_would_refuse_is_narrowed() {
        let d = tempfile::tempdir().unwrap();
        file(d.path(), "model.gguf");
        file(d.path(), "x-f16.gguf");
        assert_eq!(
            direct_gguf(d.path(), d.path(), Some("f16"), "cohere_asr"),
            None
        );

        let multi = tempfile::tempdir().unwrap();
        for n in [
            "text_encoder_q4_k.gguf",
            "dit.gguf",
            "audio_vae_folded_f16.gguf",
            "video_vae.gguf",
        ] {
            file(multi.path(), n);
        }
        assert_eq!(
            direct_gguf(multi.path(), multi.path(), Some("q4_k"), "minimax_h3"),
            None
        );

        let vad = tempfile::tempdir().unwrap();
        file(vad.path(), "silero-vad-q8_0.gguf");
        file(vad.path(), "silero-vad-f16.gguf");
        assert_eq!(
            direct_gguf(vad.path(), vad.path(), Some("q8_0"), "silero_vad"),
            None
        );
        assert!(direct_gguf(vad.path(), vad.path(), Some("q8_0"), "pocket_tts").is_some());
    }

    /// A weight id that is no precision label, but a component's name the
    /// owner typed — liveavatar's VAE or support file, minimax_music3's
    /// vocoder — leaves the directory, which those loaders take by names:
    /// handed one component as the model, they would load it as the
    /// denoiser. Nor does a label pick a file whose siblings are other
    /// components rather than other labels.
    #[test]
    fn a_component_name_or_a_package_of_components_keeps_the_directory() {
        let live = tempfile::tempdir().unwrap();
        for n in [
            "Wan2.2-S2V-Support-Q4_K_S-F16.gguf",
            "Wan2.2-S2V-VAE-F16.gguf",
            "Wan2.2-S2V-14B-NVFP4-LORA.gguf",
        ] {
            file(live.path(), n);
        }
        for w in ["vae", "VAE", "support", "f16", "q4_k_s", "nvfp4", "14b"] {
            assert_eq!(
                direct_gguf(live.path(), live.path(), Some(w), "liveavatar"),
                None,
                "weight id {w}"
            );
        }
        let music = tempfile::tempdir().unwrap();
        for n in [
            "language_model_q8_0.gguf",
            "rvq_depth_decoder_q8_0.gguf",
            "condition_encoder.gguf",
            "transformer_q8_0.gguf",
            "vocoder.gguf",
        ] {
            file(music.path(), n);
        }
        for w in ["vocoder", "condition_encoder", "q8_0"] {
            assert_eq!(
                direct_gguf(music.path(), music.path(), Some(w), "minimax_music3"),
                None
            );
        }
        let sized = tempfile::tempdir().unwrap();
        file(sized.path(), "yue-3b.gguf");
        file(sized.path(), "yue-vae.gguf");
        assert_eq!(
            direct_gguf(sized.path(), sized.path(), Some("3b"), "yue2"),
            None
        );
        // Bare labels as names: still one package's quantizations.
        let bare = tempfile::tempdir().unwrap();
        file(bare.path(), "q4_0.gguf");
        file(bare.path(), "Q8_0.gguf");
        assert_eq!(
            name(direct_gguf(
                bare.path(),
                bare.path(),
                Some("q8_0"),
                "cohere_asr"
            ))
            .as_deref(),
            Some("Q8_0.gguf")
        );
    }

    #[test]
    fn precision_labels_are_told_from_component_names() {
        for label in [
            "q8_0", "q4_k", "q4_k_m", "q5_k_s", "iq4_xs", "i2_s", "f16", "f32", "bf16", "fp8",
            "nvfp4", "int8", "bfloat16", "orig",
        ] {
            assert!(is_precision_label(label), "{label}");
        }
        for name in [
            "vae",
            "support",
            "vocoder",
            "dit",
            "3b",
            "14b",
            "lora",
            "original",
            "native",
            "q",
            "f",
            "",
            "q8_0_lora_adapter",
        ] {
            assert!(!is_precision_label(name), "{name}");
        }
    }

    /// A directory holding `names` at its top level.
    fn dir_of<'a>(names: impl IntoIterator<Item = &'a str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for n in names {
            file(d.path(), n);
        }
        d
    }

    /// The catalog's single-GGUF families whose quantizations share one
    /// `target_directory`, with their file names as the specs ship them
    /// (`(precision, file)` per package): an `orig` package beside its
    /// quantizations, Qwen3-TTS 1.7B Base's `q8_0_v2`, Sortformer v2.1's
    /// `f16-mixed`. audio.cpp refuses each directory once two are
    /// downloaded, so whichever two or more are, every package's precision
    /// as the weight id picks its own file.
    #[test]
    fn the_catalog_quantizations_of_one_directory_each_get_their_file() {
        let dirs: &[(&str, &[(&str, &str)])] = &[
            (
                "firered_audio",
                &[
                    ("orig", "firered-audio-orig.gguf"),
                    ("q8_0", "firered-audio-q8_0.gguf"),
                ],
            ),
            (
                "fireredtts3",
                &[
                    ("orig", "fireredtts3-instruct-orig.gguf"),
                    ("q8_0", "fireredtts3-instruct-q8_0.gguf"),
                ],
            ),
            (
                "fireredtts3",
                &[
                    ("orig", "fireredtts3-base-orig.gguf"),
                    ("q8_0", "fireredtts3-base-q8_0.gguf"),
                ],
            ),
            (
                "index_tts2",
                &[
                    ("q8_0", "index-tts2-q8_0.gguf"),
                    ("f16", "index-tts2-f16.gguf"),
                    ("orig", "index-tts2-orig.gguf"),
                ],
            ),
            (
                "index_tts2",
                &[
                    ("q8_0", "index-tts2_5-q8_0.gguf"),
                    ("f16", "index-tts2_5-f16.gguf"),
                    ("orig", "index-tts2_5-orig.gguf"),
                ],
            ),
            (
                "magpie_tts",
                &[
                    ("orig", "magpie-tts-multilingual-357m-orig.gguf"),
                    ("q8_0", "magpie-tts-multilingual-357m-q8_0.gguf"),
                ],
            ),
            (
                "miocodec",
                &[
                    ("q8_0", "miocodec-25hz-44khz-v2-q8_0.gguf"),
                    ("f16", "miocodec-25hz-44khz-v2-f16.gguf"),
                    ("orig", "miocodec-25hz-44khz-v2-orig.gguf"),
                ],
            ),
            (
                "miotts",
                &[
                    ("q8_0", "miotts-1.7b-q8_0.gguf"),
                    ("bf16", "miotts-1.7b-bf16.gguf"),
                    ("orig", "miotts-1.7b-orig.gguf"),
                ],
            ),
            (
                "seed_vc",
                &[
                    ("q8_0", "seed-vc-mlx-q8_0.gguf"),
                    ("f16", "seed-vc-mlx-f16.gguf"),
                    ("orig", "seed-vc-mlx-orig.gguf"),
                ],
            ),
            (
                "supertonic",
                &[
                    ("q8_0", "supertonic-3-q8_0.gguf"),
                    ("f16", "supertonic-3-f16.gguf"),
                    ("orig", "supertonic-3-orig.gguf"),
                ],
            ),
            (
                "vevo2",
                &[
                    ("q8_0", "vevo2-q8_0.gguf"),
                    ("f16", "vevo2-f16.gguf"),
                    ("orig", "vevo2-orig.gguf"),
                ],
            ),
            (
                "voxcpm2",
                &[
                    ("q8_0", "voxcpm2-q8_0.gguf"),
                    ("bf16", "voxcpm2-bf16.gguf"),
                    ("orig", "voxcpm2-orig.gguf"),
                ],
            ),
            (
                "zipvoice",
                &[
                    ("orig", "zipvoice-distill-orig.gguf"),
                    ("q8_0", "zipvoice-distill-q8_0.gguf"),
                ],
            ),
            (
                "qwen3_tts",
                &[
                    ("q8_0", "qwen3-tts-12hz-1.7b-base-q8_0_v2.gguf"),
                    ("bf16", "qwen3-tts-12hz-1.7b-base-bf16.gguf"),
                    ("orig", "qwen3-tts-12hz-1.7b-base-orig.gguf"),
                ],
            ),
            (
                "sortformer_diar_v2",
                &[
                    ("f32", "sortformer-v2.1-f32.gguf"),
                    ("f16", "sortformer-v2.1-f16-mixed.gguf"),
                ],
            ),
        ];
        for (family, packages) in dirs {
            // Every set of two or more of the directory's packages.
            for mask in 1u32..1 << packages.len() {
                let on: Vec<&(&str, &str)> = packages
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, p)| p)
                    .collect();
                if on.len() < 2 {
                    continue;
                }
                let d = dir_of(on.iter().map(|(_, f)| *f));
                for (precision, wanted) in &on {
                    assert_eq!(
                        name(direct_gguf(d.path(), d.path(), Some(precision), family)).as_deref(),
                        Some(*wanted),
                        "{family}: weight id {precision} among {on:?}"
                    );
                }
            }
        }
    }

    /// The catalog's packages of several GGUFs, by their spec file names:
    /// liveavatar, minimax_music3, yue2 (model and VAE packages), auk
    /// (model, Qwen and VAE packages), minimax_h3 and vibeasr. Downloaded
    /// whole, or one package of each part as a row would load them, they
    /// stay a directory for every precision the catalog names and every
    /// component's name as the weight id.
    #[test]
    fn the_catalog_packages_of_components_keep_their_directory() {
        type Part<'a> = &'a [&'a [&'a str]];
        let music_q4 = [
            "language_model_q4_0.gguf",
            "rvq_depth_decoder_q8_0.gguf",
            "condition_encoder.gguf",
            "transformer_q4_0.gguf",
            "vocoder.gguf",
        ];
        let music_q8 = [
            "language_model_q8_0.gguf",
            "rvq_depth_decoder_q8_0.gguf",
            "condition_encoder.gguf",
            "transformer_q8_0.gguf",
            "vocoder.gguf",
        ];
        let music_bf16 = [
            "language_model_bf16.gguf",
            "rvq_depth_decoder_bf16.gguf",
            "condition_encoder.gguf",
            "transformer_bf16.gguf",
            "vocoder.gguf",
        ];
        // Each family's parts, each part's packages as their files.
        let families: &[(&str, &[Part<'_>])] = &[
            (
                "liveavatar",
                &[&[&[
                    "Wan2.2-S2V-Support-Q4_K_S-F16.gguf",
                    "Wan2.2-S2V-VAE-F16.gguf",
                    "Wan2.2-S2V-14B-NVFP4-LORA.gguf",
                ]]],
            ),
            ("minimax_music3", &[&[&music_q4, &music_q8, &music_bf16]]),
            (
                "yue2",
                &[
                    &[
                        &["yue2-3b-q8_0.gguf"],
                        &["yue2-3b-bf16.gguf"],
                        &["yue2-3b-q4_0.gguf"],
                    ],
                    &[&["yue2-vae-f16.gguf"], &["yue2-vae-f32.gguf"]],
                ],
            ),
            (
                "auk",
                &[
                    &[
                        &["auk-base-f32.gguf"],
                        &["auk-base-f16.gguf"],
                        &["auk-base-q8_0.gguf"],
                        &["auk-flash-f32.gguf"],
                        &["auk-flash-f16.gguf"],
                        &["auk-flash-q8_0.gguf"],
                    ],
                    &[
                        &["qwen2.5-omni-3b-bf16.gguf"],
                        &["qwen2.5-omni-3b-q8_0.gguf"],
                    ],
                    &[&["auk-vae-f32.gguf"]],
                ],
            ),
            (
                "minimax_h3",
                &[&[
                    &[
                        "text_encoder_q4_k.gguf",
                        "dit.gguf",
                        "audio_vae_folded_f16.gguf",
                        "video_vae.gguf",
                    ],
                    &[
                        "text_encoder_q4_k.gguf",
                        "dit_int8.gguf",
                        "audio_vae_folded_f16.gguf",
                        "video_vae.gguf",
                    ],
                ]],
            ),
            (
                "vibeasr",
                &[&[&[
                    "vibeasr-vae-encoder-i8_s.gguf",
                    "vibeasr-lm-i2_s-embed-q6_k.gguf",
                ]]],
            ),
        ];
        let weight_ids = [
            // Every precision the catalog's packages name, and the labels
            // inside these files' names.
            "q8_0",
            "q4_0",
            "q4_k",
            "q4_k_s",
            "q6_k",
            "bf16",
            "f16",
            "f32",
            "orig",
            "native",
            "int8",
            "i2_s",
            "i8_s",
            "nvfp4",
            // Components' names.
            "vae",
            "VAE",
            "support",
            "vocoder",
            "condition_encoder",
            "dit",
            "text_encoder",
            "lora",
            "3b",
            "14b",
            "base",
            "flash",
            "qwen",
            "lm",
            "embed",
        ];
        for (family, parts) in families {
            // One package of each part, every way; then all of them at once.
            let mut sets: Vec<Vec<&str>> = vec![Vec::new()];
            for part in *parts {
                sets = sets
                    .iter()
                    .flat_map(|s| {
                        part.iter().map(move |pkg| {
                            let mut s = s.clone();
                            s.extend(pkg.iter().copied());
                            s
                        })
                    })
                    .collect();
            }
            sets.push(
                parts
                    .iter()
                    .flat_map(|p| p.iter().flat_map(|pkg| pkg.iter().copied()))
                    .collect(),
            );
            for set in sets {
                let d = dir_of(
                    set.iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>(),
                );
                for w in weight_ids {
                    assert_eq!(
                        direct_gguf(d.path(), d.path(), Some(w), family),
                        None,
                        "{family}: weight id {w} among {set:?}"
                    );
                }
            }
        }
    }

    /// A symlinked GGUF counts as audio.cpp counts it inside its container:
    /// one whose link stays in the models dir is a file there; one into a
    /// store outside it (a Hugging Face cache blob) dangles there, however
    /// it resolves on the host, and so does a dangling one.
    #[test]
    fn a_symlinked_gguf_is_counted_as_the_container_sees_it() {
        let models = tempfile::tempdir().unwrap();
        let m = models.path();
        file(m, "blob");
        let d = m.join("row");
        std::fs::create_dir(&d).unwrap();
        file(&d, "parakeet-q8_0.gguf");
        std::os::unix::fs::symlink("../blob", d.join("parakeet-f16.gguf")).unwrap();
        assert_eq!(
            name(direct_gguf(m, &d, Some("q8_0"), "parakeet_tdt")).as_deref(),
            Some("parakeet-q8_0.gguf")
        );

        let store = tempfile::tempdir().unwrap();
        file(store.path(), "blob");
        for (row, target) in [
            ("cached", store.path().join("blob")),
            ("dangling", m.join("gone")),
        ] {
            let d = m.join(row);
            std::fs::create_dir(&d).unwrap();
            file(&d, "parakeet-q8_0.gguf");
            std::os::unix::fs::symlink(&target, d.join("parakeet-f16.gguf")).unwrap();
            assert_eq!(
                direct_gguf(m, &d, Some("q8_0"), "parakeet_tdt"),
                None,
                "{row}: one GGUF to audio.cpp, so the directory"
            );
        }
    }
}
