//! What a model costs on the GPU, derived from the files it is made of
//! (quickdoc §9b).
//!
//! The router publishes no memory figures, so the sizing truth is the same one
//! `lmgw__local_model_plan` already uses: GGUF metadata plus the section's own
//! flags. These terms are derivable and are read, never assumed:
//!
//! * **weights** — the byte size of the GGUF, plus its projector and its
//!   speculative drafter, because llama-server loads all three into the same
//!   device memory; for a chat row less the input embeddings llama.cpp keeps
//!   in host memory ([`chat`]).
//! * **KV cache** — over the context the *section* actually asks for (not the
//!   model's trained maximum, unless that is what the section says) at the
//!   cache precision it actually asks for. A chat row's is laid out as
//!   llama.cpp lays it out for its slots, window and speculative decoding,
//!   block scales included ([`chat`]); an aux row's is
//!   [`crate::gguf::kv_cache_bytes`].
//! * **compute buffer** — [`super::compute`] from the header and the row's
//!   ubatch, batch and context, fitted to llama.cpp's own reserved sizes: an
//!   aux row's ([`aux`]), charged wherever its container gets the device,
//!   `--n-gpu-layers 0` on a GPU image included; a chat row's per context,
//!   its own and the MTP or draft context a speculative row runs
//!   ([`super::compute::chat`]). It is a fit, not a bound: within a few
//!   percent where the masks and a quantized cache's f16 copy dominate, a
//!   chat context's activations within −5 % to +42 % (at most 80 MiB), up to
//!   30 % high for an encoder whose layers differ in width, and an encoder
//!   read from its pooling alone says what a decoder would add.
//! * An aux row whose header declares it non-causal (`attention.causal =
//!   false`) is charged no KV cache: llama.cpp keeps none for an encoder.
//!
//! Everything else llama.cpp allocates — the CUDA context (~450 MiB per
//! container as measured), allocator slack, a projector's own compute
//! buffer — has no metadata to derive it from. It is not folded in here as a
//! multiplier; it is the `headroom_mb` setting, one visible number the owner
//! can raise when a load still OOMs, and the total leaves it out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::{AudioModel, ImageModel};
use crate::gguf::{self, ModelSummary};
use crate::runtime::argv::flag_value;

pub(crate) mod aux;
pub(crate) mod chat;

/// One model's GPU footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Footprint {
    pub weights_bytes: u64,
    pub kv_cache_bytes: u64,
    /// The compute buffers llama.cpp reserves on the card
    /// ([`super::compute`]), every context of the row summed; 0 on the CPU
    /// and where they could not be sized, which the note then says.
    pub compute_bytes: u64,
    /// `weights + kv + compute`. It leaves out what the headroom covers —
    /// see the module docs.
    pub total_bytes: u64,
    /// Context the KV figure was computed for; `None` when it could not be
    /// derived, which is also why `kv_cache_bytes` would be 0.
    pub ctx_tokens: Option<u64>,
    /// Why a term is missing or approximate, when it is. Surfaced verbatim.
    pub note: Option<String>,
}

impl From<Footprint> for lmgw_api_types::RungFootprint {
    /// The wire form of a footprint. Every field is named, so a field added
    /// to [`Footprint`] stops compiling here until the wire type carries it.
    fn from(f: Footprint) -> Self {
        let Footprint {
            weights_bytes,
            kv_cache_bytes,
            compute_bytes,
            total_bytes,
            ctx_tokens,
            note,
        } = f;
        Self {
            weights_bytes,
            kv_cache_bytes,
            compute_bytes,
            total_bytes,
            ctx_tokens,
            note,
        }
    }
}

impl Footprint {
    pub(crate) fn new(weights: u64, kv: u64, ctx: Option<u64>, note: Option<String>) -> Self {
        Self {
            weights_bytes: weights,
            kv_cache_bytes: kv,
            compute_bytes: 0,
            total_bytes: weights.saturating_add(kv),
            ctx_tokens: ctx,
            note,
        }
    }

    /// This footprint with a compute buffer of `bytes` charged on top.
    pub(crate) fn with_compute(mut self, bytes: u64) -> Self {
        self.compute_bytes = bytes;
        self.total_bytes = self
            .weights_bytes
            .saturating_add(self.kv_cache_bytes)
            .saturating_add(bytes);
        self
    }

    /// This footprint with `sentence` added to its note.
    pub(crate) fn noting(mut self, sentence: String) -> Self {
        self.note = Some(match self.note.take() {
            Some(n) => format!("{n}. {sentence}"),
            None => sentence,
        });
        self
    }
}

/// Bits per KV element for a llama.cpp cache type. `None` (the field unset) is
/// llama.cpp's own documented default, `f16` — reading the real default, not
/// inventing a smaller one.
pub(crate) fn cache_bits(t: Option<&str>) -> u32 {
    match t.unwrap_or("f16").trim() {
        "f32" => 32,
        "" | "f16" | "bf16" => 16,
        "q8_0" => 8,
        "q5_0" | "q5_1" => 5,
        "q4_0" | "q4_1" | "iq4_nl" => 4,
        // An unknown spelling is llama.cpp's problem to reject at load; sizing
        // it at the default is the honest fallback and the note says so.
        _ => 16,
    }
}

/// KV bytes with K and V charged at their own cache precisions.
///
/// [`gguf::kv_cache_bytes`] charges `key_length + value_length` at a single
/// width (and carries the sliding-window arithmetic, which is why it is reused
/// rather than reimplemented); this splits its result back along those two
/// lengths so `cache-type-k = q8_0` beside `cache-type-v = f16` sizes correctly.
fn kv_bytes(s: &ModelSummary, ctx: u64, bits_k: u32, bits_v: u32) -> Option<u64> {
    if bits_k == bits_v {
        return gguf::kv_cache_bytes(s, ctx, bits_k);
    }
    let (kl, vl) = (s.key_length?, s.value_length?);
    let span = kl.checked_add(vl)?;
    let k = gguf::kv_cache_bytes(s, ctx, bits_k)?;
    let v = gguf::kv_cache_bytes(s, ctx, bits_v)?;
    Some(k.checked_mul(kl)? / span + v.checked_mul(vl)? / span)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Total size of every regular file under `dir` (audio.cpp model roots are
/// directories, not single files).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => total = total.saturating_add(dir_size(&e.path())),
            Ok(_) => total = total.saturating_add(e.metadata().map(|m| m.len()).unwrap_or(0)),
            Err(_) => {}
        }
    }
    total
}

/// Whether a llama-server argument list pins every layer to the CPU.
///
/// `--n-gpu-layers 0` (and its `-ngl` / `--gpu-layers` spellings) is how an
/// owner runs a model *beside* the GPU — an embedding bake-off on CPU while a
/// chat model keeps the card. Charging such a model its full weights + KV
/// against VRAM would evict the chat model to make room it never uses.
fn cpu_only_args(args: &[String]) -> bool {
    flag_value(args, "n-gpu-layers").is_some_and(|v| v.trim() == "0")
}

/// The footprint of a model that offloads nothing: zero on the card. The
/// context is still reported, since `/v1/models` publishes it and it is a
/// property of the model, not of where it runs. The CUDA context itself
/// (a few hundred MB on a CUDA build even with nothing offloaded) has no
/// metadata to derive it from and is what `headroom_mb` is for.
fn cpu_only(fp: Footprint) -> Footprint {
    Footprint::new(
        0,
        0,
        fp.ctx_tokens,
        Some(
            "CPU-only (n-gpu-layers 0): weights and KV cache live in host RAM, so nothing \
             is charged to the GPU beyond the headroom setting"
                .into(),
        ),
    )
}

/// What [`plan_gguf`] reads off the disk: the weights file's bytes, its
/// companions' bytes, and its header (or why it could not be read).
pub(crate) struct GgufFiles {
    pub main_bytes: u64,
    pub companion_bytes: u64,
    pub summary: Result<ModelSummary, String>,
}

impl GgufFiles {
    /// Blocking: read `gguf_path` and `companions` under `models_dir`.
    pub(crate) fn read(models_dir: &str, gguf_path: &str, companions: &[String]) -> Self {
        let root = PathBuf::from(models_dir);
        let main = root.join(gguf_path.trim_start_matches('/'));
        let companion_bytes = companions.iter().fold(0u64, |sum, c| {
            sum.saturating_add(file_size(&root.join(c.trim_start_matches('/'))))
        });
        Self {
            main_bytes: file_size(&main),
            companion_bytes,
            summary: gguf::summarize(&main).map_err(|e| e.to_string()),
        }
    }
}

/// Weights + KV cache of a GGUF whose files were read (`files`): the
/// weights and companions at their size on disk, the KV cache at the
/// section's context (`ctx_override`, else the trained one) and cache
/// widths.
pub(crate) fn kv_footprint(
    files: &GgufFiles,
    gguf_path: &str,
    ctx_override: Option<i64>,
    bits_k: u32,
    bits_v: u32,
) -> Footprint {
    let weights = files.main_bytes.saturating_add(files.companion_bytes);
    let summary = match &files.summary {
        Ok(s) => s,
        Err(e) => {
            return Footprint::new(
                weights,
                0,
                None,
                Some(format!(
                    "KV cache not sized: '{gguf_path}' could not be read ({e}) — the estimate \
                     is weights only"
                )),
            )
        }
    };
    let ctx = ctx_override
        .filter(|c| *c > 0)
        .map(|c| c as u64)
        .or(summary.context_length);
    let Some(ctx) = ctx else {
        return Footprint::new(
            weights,
            0,
            None,
            Some(
                "KV cache not sized: the section sets no ctx-size and the GGUF declares no \
                 context length"
                    .into(),
            ),
        );
    };
    match kv_bytes(summary, ctx, bits_k, bits_v) {
        Some(kv) => Footprint::new(weights, kv, Some(ctx), None),
        None => Footprint::new(
            weights,
            0,
            Some(ctx),
            Some(
                "KV cache not sized: the GGUF states no usable block_count / head_count_kv / \
                 key_length / value_length, and none of them is guessable"
                    .into(),
            ),
        ),
    }
}

/// Memoized footprints. GGUF headers do not change under a running gateway, and
/// re-reading one on every request would put a file read on the hot path.
#[derive(Default)]
pub struct PlanCache {
    entries: Mutex<HashMap<String, Footprint>>,
}

impl PlanCache {
    /// Drop everything — the model rows or the models dir changed.
    pub async fn clear(&self) {
        self.entries.lock().await.clear();
    }

    async fn get_or_insert<F>(&self, key: String, compute: F) -> Footprint
    where
        F: FnOnce() -> Footprint + Send + 'static,
    {
        if let Some(hit) = self.entries.lock().await.get(&key) {
            return hit.clone();
        }
        let fp = tokio::task::spawn_blocking(compute)
            .await
            .unwrap_or_else(|e| {
                Footprint::new(0, 0, None, Some(format!("sizing thread failed: {e}")))
            });
        self.entries.lock().await.insert(key, fp.clone());
        fp
    }

    /// audio.cpp models are directory trees with no header lmgw can parse, so
    /// the on-disk size of the model root is the whole estimate and the note
    /// says as much — there is no KV term to derive and no telemetry to check
    /// it against.
    pub async fn audio(&self, models_dir: &str, m: &AudioModel) -> Footprint {
        let key = format!("audio|{models_dir}|{}", m.path);
        let root = PathBuf::from(models_dir).join(m.path.trim_start_matches('/'));
        self.get_or_insert(key, move || {
            Footprint::new(
                dir_size(&root),
                0,
                None,
                Some(
                    "audio.cpp exposes no model metadata and no per-model memory figures — \
                     this is the on-disk size of the model directory"
                        .into(),
                ),
            )
        })
        .await
    }

    /// The weights file an eager audio row has on the card once it is ready
    /// ([`super::residency::selected_weights_bytes`]), memoized like
    /// [`Self::audio`]. In `weights_bytes`; the rest of the footprint is
    /// empty.
    pub async fn audio_weights(&self, models_dir: &str, m: &AudioModel) -> u64 {
        let (weight_id, family) = (m.weight_id.clone(), m.family.clone());
        let key = format!(
            "audio-weights|{models_dir}|{}|{}|{family}",
            m.path,
            weight_id.as_deref().unwrap_or_default()
        );
        let models = PathBuf::from(models_dir);
        let root = models.join(m.path.trim_start_matches('/'));
        self.get_or_insert(key, move || {
            let bytes = super::residency::selected_weights_bytes(
                &models,
                &root,
                weight_id.as_deref(),
                &family,
            );
            Footprint::new(bytes, 0, None, None)
        })
        .await
        .weights_bytes
    }

    /// An sd-server pipeline is a *list* of files — a diffusion model, a VAE,
    /// one to three text encoders — so the estimate is their sum
    /// (image-generation design §9). A `files` key naming a directory
    /// (`lora_model_dir`) counts as its tree, the same rule
    /// [`Self::audio`] applies to a model root.
    ///
    /// **The note is not decoration.** Measured (§12.4): a pipeline whose
    /// files are 6.2 GB on disk is 7.1 GiB resident when idle and needs
    /// 13.7 GiB transiently to render one 1024² image, because the compute
    /// buffers are allocated per job and freed after it. So this figure is
    /// neither an upper nor a lower bound for what the card must have free —
    /// it is what the weights cost, stated as exactly that.
    ///
    /// The answer to the gap is the row's **learned `peak_extra_bytes`**
    /// ([`crate::vram::peak`]): the largest `used − idle` delta measured on
    /// the device while this model had a request in flight, which the ledger
    /// then keeps free for as long as the pipeline is resident. It is not
    /// folded into this estimate and never will be — this function answers
    /// "what do these files cost", which is a property of the files and is
    /// memoized as one, while the peak is a measurement of a *running*
    /// pipeline that the row carries and the ledger charges. The note below
    /// says which of the two the row currently has.
    pub async fn image(&self, models_dir: &str, m: &ImageModel) -> Footprint {
        let files: Vec<(String, String)> = m
            .files
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.trim().to_string())))
            .filter(|(_, p)| !p.is_empty())
            .collect();
        // A `files` value that is not a non-empty string names nothing to
        // measure — but it is still a key the row carries, and a pipeline
        // silently charged 0 for a component reads as a pipeline that has
        // none. Named in the note, and part of the cache key, so two rows that
        // differ only here are two estimates.
        let unusable: Vec<String> = m
            .files
            .iter()
            .filter(|(_, v)| v.as_str().is_none_or(|s| s.trim().is_empty()))
            .map(|(k, v)| format!("{k} ({v})"))
            .collect();
        // The learned peak is in the key because it is in the note: the same
        // file set with and without a measured peak are two different things
        // to say, and a memo that outlived the measurement would go on saying
        // "not learned yet" for the rest of the process.
        let key = format!(
            "image|{models_dir}|{}|{}|{}",
            files
                .iter()
                .map(|(k, p)| format!("{k}={p}"))
                .collect::<Vec<_>>()
                .join(","),
            unusable.join(","),
            m.peak_extra_bytes
                .map(|b| b.to_string())
                .unwrap_or_else(|| "unlearned".into())
        );
        let dir = models_dir.to_string();
        let peak = m.peak_extra_bytes;
        self.get_or_insert(key, move || {
            let mut weights = 0u64;
            let mut missing: Vec<String> = Vec::new();
            for (key, rel) in &files {
                let path = crate::runtime::image::file_target(&dir, rel);
                let size = if crate::runtime::image::is_dir_key(key) {
                    dir_size(&path)
                } else {
                    file_size(&path)
                };
                if size == 0 && !path.exists() {
                    missing.push(format!("{key} '{rel}'"));
                }
                weights = weights.saturating_add(size);
            }
            let mut note = String::from(
                "the on-disk size of the files this pipeline loads: --offload-to-cpu \
                 and --params-backend make it an upper bound for the idle figure, \
                 and no file size accounts for the compute buffers one generation \
                 allocates (measured: 7.1 GiB idle vs 13.7 GiB peak at 1024² for a \
                 6.2 GB pipeline)",
            );
            // The transient is learned rather than guessed (§9): what the
            // sampler measured while this row was generating, or nothing at
            // all — never a multiple of the figure above.
            match peak {
                Some(p) => note.push_str(&format!(
                    ". One generation was measured to need {} above this pipeline's idle \
                     residency, and admission keeps that much free while it is resident",
                    crate::hf::fmt_bytes(p)
                )),
                None => note.push_str(
                    ". The transient peak has not been learned yet — it is measured while a \
                     generation runs, so run one at the largest size you use and admission \
                     will account for it",
                ),
            }
            if !missing.is_empty() {
                note.push_str(&format!(
                    ". Counted as 0: {} (missing from the image models dir)",
                    missing.join(", ")
                ));
            }
            if !unusable.is_empty() {
                note.push_str(&format!(
                    ". Counted as 0: {} (not a path — the value is not a non-empty string, so \
                     there is nothing to measure)",
                    unusable.join(", ")
                ));
            }
            Footprint::new(weights, 0, None, Some(note))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuxKind, ImageModel, LlamaParams, LocalModel};

    /// An image pipeline is a list of files, so the estimate is their sum —
    /// and a file that is not there is charged nothing and **named**, because
    /// a silent 0 would read as "this model is free" (image-generation §9).
    #[tokio::test]
    async fn an_image_footprint_sums_its_files_and_names_the_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("z")).unwrap();
        std::fs::write(dir.path().join("z/diffusion.gguf"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.path().join("z/ae.safetensors"), vec![7u8; 1024]).unwrap();
        // A directory key counts as its tree.
        std::fs::create_dir_all(dir.path().join("loras")).unwrap();
        std::fs::write(dir.path().join("loras/a.safetensors"), vec![7u8; 512]).unwrap();
        let models_dir = dir.path().display().to_string();

        let row = |files: serde_json::Value| ImageModel {
            id: 1,
            model_id: "z-image".into(),
            files: files.as_object().cloned().unwrap(),
            args: Default::default(),
            modes: vec![],
            edit: false,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 300,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            peak_extra_bytes: None,
            peak_learned_at: None,
        };

        let plans = PlanCache::default();
        let fp = plans
            .image(
                &models_dir,
                &row(serde_json::json!({
                    "diffusion_model": "z/diffusion.gguf",
                    "vae": "z/ae.safetensors",
                    "lora_model_dir": "loras"
                })),
            )
            .await;
        assert_eq!(fp.weights_bytes, 4096 + 1024 + 512);
        assert_eq!(fp.total_bytes, fp.weights_bytes);
        assert_eq!(fp.kv_cache_bytes, 0, "there is no KV cache to size");
        assert_eq!(fp.ctx_tokens, None);
        let note = fp.note.as_deref().unwrap_or_default();
        assert!(note.contains("compute buffers"), "{note}");
        assert!(
            note.contains("13.7 GiB"),
            "the measured peak is stated: {note}"
        );
        assert!(!note.contains("missing"), "nothing was missing: {note}");

        let missing = plans
            .image(
                &models_dir,
                &row(serde_json::json!({
                    "diffusion_model": "z/diffusion.gguf",
                    "llm": "encoders/qwen3-4b.gguf"
                })),
            )
            .await;
        assert_eq!(missing.weights_bytes, 4096, "the absent file is charged 0");
        let note = missing.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("llm 'encoders/qwen3-4b.gguf'") && note.contains("missing"),
            "{note}"
        );

        // A value that is not a path at all (a `true` the editor's raw JSON box
        // let through, an empty string) used to be dropped before the missing
        // check and charged 0 in silence — a component of the pipeline costing
        // nothing, with nothing to read that said so.
        let odd = plans
            .image(
                &models_dir,
                &row(serde_json::json!({
                    "diffusion_model": "z/diffusion.gguf",
                    "vae": "",
                    "clip_l": true
                })),
            )
            .await;
        assert_eq!(odd.weights_bytes, 4096);
        let note = odd.note.as_deref().unwrap_or_default();
        assert!(note.contains("vae (\"\")"), "{note}");
        assert!(note.contains("clip_l (true)"), "{note}");
        assert!(note.contains("not a path"), "{note}");
    }

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cpu_only_is_read_from_every_spelling_of_the_flag() {
        assert!(cpu_only_args(&args(&["--n-gpu-layers", "0"])));
        assert!(cpu_only_args(&args(&["-ngl", "0"])));
        assert!(cpu_only_args(&args(&["--gpu-layers=0"])));
        assert!(!cpu_only_args(&args(&["--n-gpu-layers", "999"])));
        assert!(!cpu_only_args(&args(&["--threads", "0"])));
        assert!(!cpu_only_args(&[]));
    }

    /// The bug this guards: an embedding bake-off on the CPU was charged the
    /// full weights + KV of every model against the GPU, evicting the chat
    /// model that was actually using it.
    #[tokio::test]
    async fn a_cpu_only_model_is_charged_nothing_but_keeps_its_context() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "org/embedder/embedder-q8.gguf";
        crate::gguf::synth::embedding("qwen3", 3, 32768).write_to(&dir.path().join(rel));
        let models_dir = dir.path().display().to_string();
        // Run args that pass no GPU: `--n-gpu-layers 0` then leaves nothing
        // on the card (`aux::Placement::HostNoDevice`).
        let class = crate::config::RouterSettings {
            models_dir: models_dir.clone(),
            extra_run_args: vec!["--security-opt".into(), "label=disable".into()],
            ..crate::config::RouterSettings::default_aux()
        };
        let row = |args: Vec<String>| crate::config::AuxModel {
            id: 1,
            model_id: "embedder".into(),
            gguf_path: rel.into(),
            kind: AuxKind::Embed,
            pooling: Some("last".into()),
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args,
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        };

        let plans = PlanCache::default();
        let gpu = plans.aux(&class, &row(vec![])).await;
        assert!(gpu.weights_bytes > 0, "the file itself is charged: {gpu:?}");
        assert_eq!(gpu.ctx_tokens, Some(32768));

        let cpu = plans
            .aux(&class, &row(args(&["--n-gpu-layers", "0"])))
            .await;
        assert_eq!(cpu.total_bytes, 0, "{cpu:?}");
        assert_eq!(cpu.ctx_tokens, Some(32768), "context is still reported");
        assert!(cpu.note.as_deref().unwrap_or("").contains("CPU-only"));

        // The chat class reads its structured field for the same rule.
        let chat = LocalModel {
            id: 2,
            model_id: "embedder-chat".into(),
            gguf_path: rel.into(),
            params: LlamaParams {
                n_gpu_layers: Some(0),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        };
        assert_eq!(plans.local(&models_dir, &chat).await.total_bytes, 0);
    }
}
