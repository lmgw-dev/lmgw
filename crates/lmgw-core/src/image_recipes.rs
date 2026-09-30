//! The shipped image-pipeline recipes (image-generation design §7.2).
//!
//! A stable-diffusion.cpp pipeline is not one file with a header lmgw can
//! read: it is a diffusion model (or an all-in-one checkpoint) plus a VAE plus
//! one to three text encoders, and **the components live in different repos
//! under different owners** (§2.7). `lmgw__hf_add`'s "download a weights file
//! with its companions from one repo" therefore does not describe this class,
//! and neither does an upstream catalog — there is none. So "add from recipe"
//! reads from a curated list compiled into the binary: the audio catalog flow
//! (`web/audio.rs`) with a static source instead of audio.cpp's
//! `model_specs/*.json`.
//!
//! # What a recipe is allowed to claim
//!
//! `scripts/hf-recipe-check.py <owner/repo>` is that read — it prints every
//! weight file with the byte size and the `gated` flag, and `--rust` prints
//! the `ImageRecipeAlternative` literals, so the numbers below are
//! transcribed by a machine rather than by hand.
//!
//! Every `(repo, file, size_bytes, gated)` below was read from the hub's
//! public metadata API on **2026-09-21** —
//! `GET /api/models/<repo>` for `gated`, `GET /api/models/<repo>/tree/main`
//! for the names and sizes — and the three Z-Image-Turbo files additionally
//! match the bytes on this box from the WP0 spike. Nothing here is from
//! memory: a file whose name could not be confirmed is not shipped. Sizes are
//! the hub's, so they are what the downloader's progress bar totals to, not a
//! rounded figure from a README.
//!
//! `gated: true` means the hub answers 401/403 without a token that has
//! accepted the licence. Where an un-gated mirror of a gated file exists it is
//! named instead — `black-forest-labs/FLUX.1-schnell`'s `ae.safetensors` is
//! byte-identical to `Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors`,
//! `black-forest-labs/FLUX.1-dev`'s weights are re-quantized un-gated by
//! `city96` and `QuantStack`, FLUX.2's autoencoder rides along in
//! `Comfy-Org/flux2-klein-4B`, and SD 3.5 Large is re-published as scaled fp8
//! by `Comfy-Org` — which is why **no shipped recipe is gated today**. The
//! flag stays because the next family may not be so lucky, and
//! `image_recipe_add` refuses a gated component with no token rather than
//! queueing a download that will fail at the first byte.
//!
//! # What goes in `args`, and what deliberately does not
//!
//! Only the values where the family **departs from sd-server's own defaults**,
//! which the shipped `--help` states outright: `--steps` 20, `--cfg-scale`
//! 7.0, `-W`/`-H` 512, `--guidance` 3.5, `--sampling-method` "euler for
//! Flux/SD3/Wan, euler_a otherwise", `--scheduler` "model-specific". A recipe
//! that set `sampling_method = euler` on a FLUX row would be writing down what
//! the server already does, and an invented `cfg_scale` for a family nobody
//! measured would be worse than silence. So SDXL carries nothing but its
//! 1024² size, and the guidance-distilled families carry `cfg_scale = 1.0`
//! because for them the server default is actively wrong.

use serde_json::{Map, Value};

/// One alternative file for a component — the quantizations a repo actually
/// ships, so the "which quant?" question is answered from the repo's own tree
/// rather than by trying names against the hub.
#[derive(Debug, Clone, Copy)]
pub struct ImageRecipeAlternative {
    pub file: &'static str,
    pub size_bytes: u64,
    /// Short label for a picker (`Q4_K`, `fp8 (scaled)`).
    pub label: &'static str,
}

/// One file of a pipeline, under the `files` key it fills.
#[derive(Debug, Clone, Copy)]
pub struct ImageRecipeComponent {
    /// The `files` key this file belongs under — `diffusion_model`, `model`,
    /// `vae`, `clip_l`, `clip_g`, `t5xxl`, `llm`, `llm_vision`.
    pub role: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub size_bytes: u64,
    /// The hub refuses this repo without an accepted licence.
    pub gated: bool,
    /// Why this file and not another — the mirror choice, what the component
    /// does in the pipeline.
    pub note: &'static str,
    pub alternatives: &'static [ImageRecipeAlternative],
}

impl ImageRecipeComponent {
    /// Where the downloader puts this file, relative to the image models dir.
    pub fn dest_rel_path(&self) -> String {
        crate::hf::dest_rel_path(self.repo, self.file)
            .unwrap_or_else(|_| format!("{}/{}", self.repo, self.file))
    }

    /// Whether `file` (or one of the alternatives) is this component by name.
    pub fn names_file(&self, file: &str) -> bool {
        self.file == file || self.alternatives.iter().any(|a| a.file == file)
    }
}

/// A value in a recipe's `args`, typed so it renders as the argv sd-server
/// expects: `--cfg-scale 1.0` and `--cfg-scale 1` are not the same argument,
/// and a switch is a bare flag rather than `--diffusion-fa true`.
#[derive(Debug, Clone, Copy)]
pub enum RecipeArg {
    Switch,
    Int(i64),
    Float(f64),
    Text(&'static str),
}

impl RecipeArg {
    fn to_json(self) -> Value {
        match self {
            Self::Switch => Value::Bool(true),
            Self::Int(i) => Value::from(i),
            Self::Float(f) => Value::from(f),
            Self::Text(t) => Value::from(t),
        }
    }
}

/// One family: what to download, under which `files` keys, with which `args`.
#[derive(Debug, Clone, Copy)]
pub struct ImageRecipe {
    /// Stable id — what `image_recipe_add` and the UI address it by, and the
    /// seed of the suggested `model_id`. Never renamed. Lowercase, dashes, and
    /// a `.` where the family's own version number has one
    /// (`qwen-image-2.1`).
    pub key: &'static str,
    pub display_name: &'static str,
    /// One sentence on what this family is good at.
    pub description: &'static str,
    pub components: &'static [ImageRecipeComponent],
    pub args: &'static [(&'static str, RecipeArg)],
    pub modes: &'static [&'static str],
    /// Takes reference images — may serve `/v1/images/edits` (§4).
    pub edit: bool,
    /// Measured VRAM, or the word that says nobody measured it. Never a
    /// number derived from file sizes: §12.4 found the file-size estimate 12 %
    /// under the idle figure and 2.2× under the 1024² peak.
    pub vram_note: &'static str,
}

impl ImageRecipe {
    /// `args` as an [`crate::ops::ImageModelPatch`]-shaped JSON object.
    pub fn args_json(&self) -> Map<String, Value> {
        self.args
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_json()))
            .collect()
    }

    /// `files` as the row stores them: role → path relative to the image
    /// models dir, in the downloader's `<owner>/<repo>/<file>` layout.
    ///
    /// `diffusion_file` overrides the diffusion component's file with one of
    /// its alternatives (the quant picker); an unknown name is an error rather
    /// than a silent fall back to the default, because the caller asked for a
    /// specific file and would otherwise download a different one.
    pub fn files_json(&self, diffusion_file: Option<&str>) -> Result<Map<String, Value>, String> {
        let chosen = self.choose_files(diffusion_file)?;
        Ok(chosen
            .into_iter()
            .map(|(c, file)| {
                let rel = crate::hf::dest_rel_path(c.repo, file)
                    .unwrap_or_else(|_| format!("{}/{}", c.repo, file));
                (c.role.to_string(), Value::from(rel))
            })
            .collect())
    }

    /// Every component paired with the file actually wanted from it — the
    /// default, or the picked alternative for the primary component.
    pub fn choose_files(
        &self,
        diffusion_file: Option<&str>,
    ) -> Result<Vec<(&'static ImageRecipeComponent, &'static str)>, String> {
        let wanted = diffusion_file.map(str::trim).filter(|s| !s.is_empty());
        let mut out = Vec::new();
        let mut used = false;
        for c in self.components {
            let mut file = c.file;
            if let Some(w) = wanted {
                if c.role == self.primary_role() {
                    file = if c.file == w {
                        c.file
                    } else {
                        c.alternatives
                            .iter()
                            .find(|a| a.file == w)
                            .map(|a| a.file)
                            .ok_or_else(|| {
                                format!(
                                    "'{w}' is not a file of {}'s {} component — it ships {}",
                                    self.key,
                                    c.role,
                                    std::iter::once(c.file)
                                        .chain(c.alternatives.iter().map(|a| a.file))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            })?
                    };
                    used = true;
                }
            }
            out.push((c, file));
        }
        if wanted.is_some() && !used {
            return Err(format!("{} has no diffusion component", self.key));
        }
        Ok(out)
    }

    /// `diffusion_model` or `model` — which of the two ways to load a pipeline
    /// this family uses, and therefore which component the quant picker acts
    /// on.
    pub fn primary_role(&self) -> &'static str {
        self.components
            .iter()
            .find(|c| matches!(c.role, "model" | "diffusion_model"))
            .map(|c| c.role)
            .unwrap_or("diffusion_model")
    }

    /// The component that loads the pipeline.
    pub fn primary(&self) -> Option<&'static ImageRecipeComponent> {
        let role = self.primary_role();
        self.components.iter().find(|c| c.role == role)
    }

    /// The refusal §7.2 asks for: a component whose repo is licence-gated
    /// cannot be downloaded without a token that accepted it, so say which one
    /// rather than queueing a transfer that dies on its first byte.
    ///
    /// Dormant today — every shipped recipe names an un-gated mirror, which
    /// `no_shipped_recipe_needs_a_token` pins — and kept because the next
    /// family added here may have none.
    pub fn gated_refusal(&self, diffusion_file: Option<&str>, has_token: bool) -> Option<String> {
        if has_token {
            return None;
        }
        let chosen = self.choose_files(diffusion_file).ok()?;
        let (c, _) = chosen.iter().find(|(c, _)| c.gated)?;
        Some(format!(
            "{}'s {} component ({}/{}) is a gated repo and no Hugging Face token is \
             configured — set one under Settings → Tokens & updates and accept the licence on \
             the hub, then add the recipe again",
            self.key, c.role, c.repo, c.file
        ))
    }

    /// Suggested `model_id` for the prefilled row: the key is already a
    /// client-facing, lowercase, dash-separated name, so it is the id.
    pub fn suggested_model_id(&self) -> String {
        self.key.to_string()
    }

    /// Total bytes of the default file set — what "add from recipe" is about
    /// to pull. On disk, not in VRAM (§12.4 says the two differ by more than
    /// a rounding).
    pub fn total_bytes(&self) -> u64 {
        self.components.iter().map(|c| c.size_bytes).sum()
    }
}

/// The shipped list. Adding a family is a literal here, not a schema change
/// (§7.2).
pub fn all() -> &'static [ImageRecipe] {
    RECIPES
}

/// One recipe by key.
pub fn find(key: &str) -> Option<&'static ImageRecipe> {
    let key = key.trim();
    RECIPES.iter().find(|r| r.key == key)
}

/// Every shipped key, for a refusal that lists what *is* known.
pub fn keys() -> Vec<&'static str> {
    RECIPES.iter().map(|r| r.key).collect()
}

/// The other recipes that name this exact `(repo, file)`, by display name.
///
/// Components are deliberately shared — six rows point at the same FLUX
/// autoencoder, two at the same Qwen encoder — and the `<owner>/<repo>/<file>`
/// layout means the second recipe to want one finds it already on disk. The
/// cost of that is a download queued for **one** recipe showing up as progress
/// under every other recipe that shares the file, which reads as "everything
/// started downloading". Naming the neighbours is what turns that from a bug
/// report into a sentence: *shared with Z-Image-Turbo*.
///
/// `key` is the recipe asking, and is never in the answer.
pub fn shared_with(key: &str, repo: &str, file: &str) -> Vec<&'static str> {
    RECIPES
        .iter()
        .filter(|r| r.key != key)
        .filter(|r| {
            r.components
                .iter()
                .any(|c| c.repo == repo && c.file == file)
        })
        .map(|r| r.display_name)
        .collect()
}

/// Which recipe a file in the image models dir belongs to.
///
/// Two passes, strongest evidence first: the downloader's layout makes a path
/// `<owner>/<repo>/<file>`, so `(repo, file)` is an exact match against a
/// component and its alternatives; failing that — a file someone copied in by
/// hand, or a repo that re-hosts the same weights — the **basename** alone is
/// matched. Returns the recipe and the component the file fills.
pub fn match_path(rel: &str) -> Option<(&'static ImageRecipe, &'static ImageRecipeComponent)> {
    let rel = rel
        .trim()
        .trim_start_matches("/models/")
        .trim_start_matches('/');
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() >= 3 {
        let repo = parts[..2].join("/");
        let file = parts[2..].join("/");
        for r in RECIPES {
            for c in r.components {
                if c.repo == repo && c.names_file(&file) {
                    return Some((r, c));
                }
            }
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    for r in RECIPES {
        for c in r.components {
            let c_base = |f: &str| f.rsplit('/').next().unwrap_or(f) == base;
            if c_base(c.file) || c.alternatives.iter().any(|a| c_base(a.file)) {
                return Some((r, c));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

/// The un-gated mirror of FLUX's `ae.safetensors` (§12.1): the gated
/// `black-forest-labs/FLUX.1-schnell` answers 401, this one does not, and the
/// file is the same 335 304 388 bytes. Shared by six of the shipped recipes,
/// which is exactly what the `<owner>/<repo>/<file>` layout makes free — the
/// second recipe to want it finds it already on disk.
const FLUX_AE: ImageRecipeComponent = ImageRecipeComponent {
    role: "vae",
    repo: "Comfy-Org/z_image_turbo",
    file: "split_files/vae/ae.safetensors",
    size_bytes: 335_304_388,
    gated: false,
    note: "the FLUX autoencoder, from the un-gated Comfy-Org mirror — \
           black-forest-labs/FLUX.1-schnell ships the same file behind a licence gate \
           (measured §12.1)",
    alternatives: &[],
};

const FLUX_CLIP_L: ImageRecipeComponent = ImageRecipeComponent {
    role: "clip_l",
    repo: "comfyanonymous/flux_text_encoders",
    file: "clip_l.safetensors",
    size_bytes: 246_144_152,
    gated: false,
    note: "the small CLIP encoder every FLUX pipeline needs beside T5",
    alternatives: &[],
};

const FLUX_T5XXL: ImageRecipeComponent = ImageRecipeComponent {
    role: "t5xxl",
    repo: "comfyanonymous/flux_text_encoders",
    file: "t5xxl_fp16.safetensors",
    size_bytes: 9_787_841_024,
    gated: false,
    note: "the prompt encoder, and the largest single file of a FLUX pipeline — the fp8 \
           alternatives halve it at some prompt fidelity",
    alternatives: &[
        ImageRecipeAlternative {
            file: "t5xxl_fp8_e4m3fn_scaled.safetensors",
            size_bytes: 5_157_348_688,
            label: "fp8 (scaled)",
        },
        ImageRecipeAlternative {
            file: "t5xxl_fp8_e4m3fn.safetensors",
            size_bytes: 4_893_934_904,
            label: "fp8",
        },
    ],
};

/// Qwen-Image's own autoencoder, shared by every Qwen-Image row — the family
/// cannot use the FLUX one.
const QWEN_IMAGE_VAE: ImageRecipeComponent = ImageRecipeComponent {
    role: "vae",
    repo: "QuantStack/Qwen-Image-GGUF",
    file: "VAE/Qwen_Image-VAE.safetensors",
    size_bytes: 253_806_246,
    gated: false,
    note: "Qwen-Image's own autoencoder — not interchangeable with the FLUX one",
    alternatives: &[],
};

/// The Qwen2.5-VL prompt encoder the first Qwen-Image generation loads through
/// `--llm`, shared by `qwen-image` and `qwen-image-edit-2509`.
const QWEN25_VL_LLM: ImageRecipeComponent = ImageRecipeComponent {
    role: "llm",
    repo: "unsloth/Qwen2.5-VL-7B-Instruct-GGUF",
    file: "Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf",
    size_bytes: 4_683_072_384,
    gated: false,
    note: "the prompt encoder this family needs (sd-server's --llm help names qwenvl2.5 for \
           qwen-image). The edit rows pair it with the matching mmproj under \
           files.llm_vision; a text-to-image row does not load one",
    alternatives: &[
        ImageRecipeAlternative {
            file: "Qwen2.5-VL-7B-Instruct-Q5_K_M.gguf",
            size_bytes: 5_444_830_080,
            label: "Q5_K_M",
        },
        ImageRecipeAlternative {
            file: "Qwen2.5-VL-7B-Instruct-Q6_K.gguf",
            size_bytes: 6_254_197_632,
            label: "Q6_K",
        },
        ImageRecipeAlternative {
            file: "Qwen2.5-VL-7B-Instruct-Q8_0.gguf",
            size_bytes: 8_098_524_032,
            label: "Q8_0",
        },
    ],
};

/// Z-Image's prompt encoder: an ordinary llama.cpp GGUF loaded through
/// `--llm`, shared by the Turbo and the base row.
const QWEN3_4B_LLM: ImageRecipeComponent = ImageRecipeComponent {
    role: "llm",
    repo: "unsloth/Qwen3-4B-Instruct-2507-GGUF",
    file: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    size_bytes: 2_497_281_120,
    gated: false,
    note: "the prompt encoder: an ordinary llama.cpp GGUF, loaded by sd-server through \
           --llm. It lives in the image models dir, not the chat one (§7)",
    alternatives: &[
        ImageRecipeAlternative {
            file: "Qwen3-4B-Instruct-2507-Q3_K_M.gguf",
            size_bytes: 2_075_618_400,
            label: "Q3_K_M",
        },
        ImageRecipeAlternative {
            file: "Qwen3-4B-Instruct-2507-Q5_K_M.gguf",
            size_bytes: 2_889_514_080,
            label: "Q5_K_M",
        },
        ImageRecipeAlternative {
            file: "Qwen3-4B-Instruct-2507-Q6_K.gguf",
            size_bytes: 3_306_261_600,
            label: "Q6_K",
        },
        ImageRecipeAlternative {
            file: "Qwen3-4B-Instruct-2507-Q8_0.gguf",
            size_bytes: 4_280_405_600,
            label: "Q8_0",
        },
    ],
};

/// The FLUX.2 autoencoder, shared by both FLUX.2 rows — a different file from
/// the FLUX.1 [`FLUX_AE`] above, and not interchangeable with it.
const FLUX2_VAE: ImageRecipeComponent = ImageRecipeComponent {
    role: "vae",
    repo: "Comfy-Org/flux2-klein-4B",
    file: "split_files/vae/flux2-vae.safetensors",
    size_bytes: 336_211_292,
    gated: false,
    note: "the FLUX.2 autoencoder. black-forest-labs/FLUX.2-dev's copy is the same \
           336 211 292 bytes and sits behind a licence gate (gated: auto); this one rides \
           along un-gated in Comfy-Org's klein bundle. For a card that is short on VRAM at \
           decode, upstream also publishes a small decoder — \
           black-forest-labs/FLUX.2-small-decoder/full_encoder_small_decoder.safetensors, \
           249 519 092 bytes and un-gated — which is a files.vae swap on the row, not a \
           different recipe",
    alternatives: &[],
};

static RECIPES: &[ImageRecipe] = &[
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "z-image-turbo",
        display_name: "Z-Image-Turbo",
        description: "A small, distilled text-to-image DiT that draws a 1024² image in eight \
                      steps — the fastest way to a picture on a single consumer card, and the \
                      pipeline lmgw's own spike measured.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "leejet/Z-Image-Turbo-GGUF",
                file: "z_image_turbo-Q4_K.gguf",
                size_bytes: 3_864_250_304,
                gated: false,
                note: "the quantization the spike measured end to end (§12)",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q2_K.gguf",
                        size_bytes: 2_592_442_304,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q3_K.gguf",
                        size_bytes: 3_143_559_104,
                        label: "Q3_K",
                    },
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q4_0.gguf",
                        size_bytes: 3_683_370_944,
                        label: "Q4_0",
                    },
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q5_0.gguf",
                        size_bytes: 4_542_547_904,
                        label: "Q5_0",
                    },
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q6_K.gguf",
                        size_bytes: 5_263_239_104,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "z_image_turbo-Q8_0.gguf",
                        size_bytes: 6_577_440_704,
                        label: "Q8_0",
                    },
                ],
            },
            FLUX_AE,
            QWEN3_4B_LLM,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("steps", RecipeArg::Int(8)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Measured on a 24 GiB card (§12.4): 7.1 GiB resident when idle, 13.7 GiB \
                    peak during a 1024² generation — the compute buffers are allocated per \
                    job and freed after it. 4.7 s warm for 1024²/8 steps, 0.23 s for 256²/4. \
                    With args.offload_to_cpu: 1.0 GiB idle, 7.7 GiB peak, +0.4 s per image.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "z-image",
        display_name: "Z-Image",
        description: "The un-distilled Z-Image: the same three files as Turbo, but it takes \
                      real classifier-free guidance and sd-server's own twenty steps instead \
                      of eight — slower per picture, and it follows a long prompt further.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "unsloth/Z-Image-GGUF",
                file: "z-image-Q4_K_M.gguf",
                size_bytes: 5_066_995_776,
                gated: false,
                note: "the base transformer — larger at the same quant than Turbo's, which \
                       is the distillation showing up on disk",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "z-image-Q2_K.gguf",
                        size_bytes: 4_013_115_456,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-Q3_K_M.gguf",
                        size_bytes: 4_559_946_816,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-Q4_K_S.gguf",
                        size_bytes: 4_787_443_776,
                        label: "Q4_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-Q5_K_M.gguf",
                        size_bytes: 5_578_099_776,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-Q6_K.gguf",
                        size_bytes: 6_101_921_856,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-Q8_0.gguf",
                        size_bytes: 7_224_707_136,
                        label: "Q8_0",
                    },
                    ImageRecipeAlternative {
                        file: "z-image-BF16.gguf",
                        size_bytes: 12_311_939_136,
                        label: "BF16",
                    },
                ],
            },
            FLUX_AE,
            QWEN3_4B_LLM,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(5.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. Both companions are Turbo's, so a box that \
                    already has Turbo downloads one file for this. The 5.0 cfg-scale is the \
                    upstream docs' — a base model wants real guidance, which is the whole \
                    difference from the 1.0 the distilled rows carry — and steps stay at \
                    sd-server's own default of 20.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "flux1-schnell",
        display_name: "FLUX.1 schnell",
        description: "The Apache-licensed, timestep-distilled FLUX: four steps to a \
                      photographic 1024² image, with the prompt adherence the FLUX family is \
                      known for.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "leejet/FLUX.1-schnell-gguf",
                file: "flux1-schnell-q8_0.gguf",
                size_bytes: 12_801_721_248,
                gated: false,
                note: "leejet's own GGUF quantizations of the schnell transformer — un-gated, \
                       unlike black-forest-labs/FLUX.1-schnell itself",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "flux1-schnell-q2_k.gguf",
                        size_bytes: 4_110_959_520,
                        label: "q2_k",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-schnell-q3_k.gguf",
                        size_bytes: 5_312_873_376,
                        label: "q3_k",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-schnell-q4_0.gguf",
                        size_bytes: 6_884_606_880,
                        label: "q4_0",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-schnell-q4_k.gguf",
                        size_bytes: 6_884_606_880,
                        label: "q4_k",
                    },
                ],
            },
            FLUX_AE,
            FLUX_CLIP_L,
            FLUX_T5XXL,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("steps", RecipeArg::Int(4)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. The default file set is 23 GB on disk, most of it \
                    the q8_0 transformer and the fp16 T5 — pick the smaller alternatives, or \
                    set args.offload_to_cpu, if the card is shared.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "flux1-dev",
        display_name: "FLUX.1 dev",
        description: "The full guidance-distilled FLUX: slower than schnell at 20 steps, and \
                      the better of the two at detail and text rendering.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "city96/FLUX.1-dev-gguf",
                file: "flux1-dev-Q8_0.gguf",
                size_bytes: 12_708_281_504,
                gated: false,
                note: "city96's GGUF re-quantization. black-forest-labs/FLUX.1-dev itself is \
                       gated (the hub reports gated: auto); this mirror is not, so no token \
                       is needed — the FLUX.1-dev non-commercial licence still applies to \
                       what you make with it",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q2_K.gguf",
                        size_bytes: 4_032_341_280,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q3_K_S.gguf",
                        size_bytes: 5_234_255_136,
                        label: "Q3_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q4_0.gguf",
                        size_bytes: 6_791_167_136,
                        label: "Q4_0",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q4_K_S.gguf",
                        size_bytes: 6_805_988_640,
                        label: "Q4_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q5_K_S.gguf",
                        size_bytes: 8_285_267_232,
                        label: "Q5_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-Q6_K.gguf",
                        size_bytes: 9_857_000_736,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-dev-F16.gguf",
                        size_bytes: 23_802_870_944,
                        label: "F16",
                    },
                ],
            },
            FLUX_AE,
            FLUX_CLIP_L,
            FLUX_T5XXL,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. Steps stay at sd-server's own default of 20 and \
                    the distilled guidance at 3.5 — dev wants no classifier-free CFG, which \
                    is what cfg_scale 1.0 says.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "flux1-kontext-dev",
        display_name: "FLUX.1 Kontext dev",
        description: "The FLUX edit model: it takes a reference image and a prompt describing \
                      the change, and serves /v1/images/edits.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "QuantStack/FLUX.1-Kontext-dev-GGUF",
                file: "flux1-kontext-dev-Q8_0.gguf",
                size_bytes: 12_714_452_256,
                gated: false,
                note: "QuantStack's un-gated GGUF of the Kontext transformer; \
                       black-forest-labs/FLUX.1-Kontext-dev itself is licence-gated",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q2_K.gguf",
                        size_bytes: 4_023_690_528,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q3_K_M.gguf",
                        size_bytes: 5_368_489_248,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q4_K_S.gguf",
                        size_bytes: 6_797_337_888,
                        label: "Q4_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q4_K_M.gguf",
                        size_bytes: 6_931_817_760,
                        label: "Q4_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q5_K_M.gguf",
                        size_bytes: 8_419_501_344,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux1-kontext-dev-Q6_K.gguf",
                        size_bytes: 9_848_349_984,
                        label: "Q6_K",
                    },
                ],
            },
            FLUX_AE,
            FLUX_CLIP_L,
            FLUX_T5XXL,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: true,
        vram_note: "Not measured yet. Same file set as FLUX.1 dev apart from the \
                    transformer, so a box that already ran dev downloads one file for this.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "flux2-klein-4b",
        display_name: "FLUX.2 klein 4B",
        description: "Black Forest Labs' small FLUX.2: four steps to a picture, and it takes \
                      reference images — so one 7 GB pipeline both draws and edits.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "leejet/FLUX.2-klein-4B-GGUF",
                file: "flux-2-klein-4b-Q8_0.gguf",
                size_bytes: 4_300_629_440,
                gated: false,
                note: "the distilled 4B transformer; this repo ships these two quants and \
                       nothing between them",
                alternatives: &[ImageRecipeAlternative {
                    file: "flux-2-klein-4b-Q4_0.gguf",
                    size_bytes: 2_460_378_560,
                    label: "Q4_0",
                }],
            },
            FLUX2_VAE,
            ImageRecipeComponent {
                role: "llm",
                repo: "unsloth/Qwen3-4B-GGUF",
                file: "Qwen3-4B-Q4_K_M.gguf",
                size_bytes: 2_497_281_312,
                gated: false,
                note: "klein's prompt encoder is plain Qwen3-4B, not the Instruct-2507 build \
                       Z-Image loads — near enough in size to look like the same file, and \
                       it is not",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Qwen3-4B-Q3_K_M.gguf",
                        size_bytes: 2_075_618_592,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen3-4B-Q5_K_M.gguf",
                        size_bytes: 2_889_514_272,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen3-4B-Q6_K.gguf",
                        size_bytes: 3_306_261_792,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen3-4B-Q8_0.gguf",
                        size_bytes: 4_280_405_792,
                        label: "Q8_0",
                    },
                ],
            },
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("steps", RecipeArg::Int(4)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: true,
        vram_note: "Not measured yet, and the smallest edit-capable pipeline here — \
                    7 GB of files against Kontext's 23. The distilled four steps and 1.0 \
                    cfg-scale are the upstream docs'; klein *base* is the same files with \
                    twenty steps and cfg-scale 4.0, which is an args edit on the row rather \
                    than a second download.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "flux2-dev",
        display_name: "FLUX.2 dev",
        description: "The full FLUX.2: a 32B DiT that reads its prompt through a 24B Mistral, \
                      draws and edits, and is the most capable pipeline here by a wide \
                      margin. It runs on a 24 GiB card because the two halves are never \
                      resident at once — see the note below before changing args.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "city96/FLUX.2-dev-gguf",
                file: "flux2-dev-Q4_K_S.gguf",
                size_bytes: 19_299_128_288,
                gated: false,
                note: "city96's GGUF re-quantization of BFL's gated weights, so no token is \
                       needed — the FLUX.2-dev non-commercial licence still applies to what \
                       you make with it. Q2_K exists at 12.9 GB if RAM is the binding \
                       constraint",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q2_K.gguf",
                        size_bytes: 12_858_250_208,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q3_K_S.gguf",
                        size_bytes: 15_779_648_480,
                        label: "Q3_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q3_K_M.gguf",
                        size_bytes: 15_960_134_624,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q4_K_M.gguf",
                        size_bytes: 20_082_414_560,
                        label: "Q4_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q5_K_M.gguf",
                        size_bytes: 24_057_238_496,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q6_K.gguf",
                        size_bytes: 27_396_232_160,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-Q8_0.gguf",
                        size_bytes: 35_002_602_464,
                        label: "Q8_0",
                    },
                    ImageRecipeAlternative {
                        file: "flux2-dev-BF16.gguf",
                        size_bytes: 64_446_616_544,
                        label: "BF16",
                    },
                ],
            },
            FLUX2_VAE,
            ImageRecipeComponent {
                role: "llm",
                repo: "unsloth/Mistral-Small-3.2-24B-Instruct-2506-GGUF",
                file: "Mistral-Small-3.2-24B-Instruct-2506-Q4_K_M.gguf",
                size_bytes: 14_333_922_848,
                gated: false,
                note: "FLUX.2 reads its prompt through a 24B instruct model — the largest \
                       text encoder of any row here, and the reason this pipeline is \
                       loaded in two passes rather than held whole",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q2_K.gguf",
                        size_bytes: 8_890_338_848,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q3_K_M.gguf",
                        size_bytes: 11_474_095_648,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q4_K_S.gguf",
                        size_bytes: 13_549_293_088,
                        label: "Q4_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q5_K_M.gguf",
                        size_bytes: 16_763_997_728,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q6_K.gguf",
                        size_bytes: 19_345_952_288,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "Mistral-Small-3.2-24B-Instruct-2506-Q8_0.gguf",
                        size_bytes: 25_054_793_248,
                        label: "Q8_0",
                    },
                ],
            },
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(1.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("offload_to_cpu", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: true,
        vram_note: "Not measured yet, and the only row that ships offload_to_cpu as a \
                    default rather than as advice. Its two halves — a 19 GB DiT and a \
                    14 GB Mistral encoder — never have to be resident together: \
                    --offload-to-cpu holds the weights in RAM and loads each module into \
                    VRAM when it is needed, so the card sees one at a time instead of their \
                    sum, which is what puts a 34 GB pipeline on a 24 GiB card. The cost \
                    moves to RAM — the default file set wants ~34 GB of it — and to time, \
                    since every module crosses the bus per job (§12.4 measured +0.4 s per \
                    image on Z-Image, a pipeline a tenth this size; expect more here). \
                    Clear offload_to_cpu only on a card that can hold both halves at once.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "chroma1-hd",
        display_name: "Chroma1 HD",
        description: "A FLUX.1 schnell derivative re-trained back into taking real \
                      classifier-free guidance, Apache-licensed end to end. The one FLUX \
                      pipeline here that needs no CLIP — T5 alone.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "silveroxides/Chroma-GGUF",
                file: "Chroma1-HD/Chroma1-HD-Q4_0.gguf",
                size_bytes: 5_432_053_920,
                gated: false,
                note: "the released HD weights; the repo also carries the whole \
                       chroma-unlocked-v* training history and several fine-tunes, which a \
                       recipe has no way to choose between for you",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Chroma1-HD/Chroma1-HD-Q8_0.gguf",
                        size_bytes: 9_735_409_824,
                        label: "Q8_0",
                    },
                    ImageRecipeAlternative {
                        file: "Chroma1-HD/Chroma1-HD-BF16.gguf",
                        size_bytes: 17_804_202_144,
                        label: "BF16",
                    },
                ],
            },
            FLUX_AE,
            FLUX_T5XXL,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(4.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. Two of its three files are the FLUX rows', so a \
                    box with FLUX.1 already downloads one. Chroma is de-distilled, which is \
                    why the 4.0 cfg-scale of the upstream docs replaces the 1.0 its schnell \
                    ancestor wants. The docs' `--model-args chroma_use_dit_mask=false` is \
                    deliberately not set here: it was written for the older unlocked-v* \
                    weights and nobody measured it on HD.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "sdxl-base",
        display_name: "Stable Diffusion XL base 1.0",
        description: "The classic 1024² open checkpoint: one self-contained file with its \
                      encoders bundled, a decade of LoRAs and fine-tunes built on it, and no \
                      separate text encoder to download.",
        components: &[
            ImageRecipeComponent {
                role: "model",
                repo: "stabilityai/stable-diffusion-xl-base-1.0",
                file: "sd_xl_base_1.0.safetensors",
                size_bytes: 6_938_078_334,
                gated: false,
                note: "the all-in-one checkpoint form: -m rather than --diffusion-model, with \
                       both CLIP encoders inside the file",
                alternatives: &[ImageRecipeAlternative {
                    file: "sd_xl_base_1.0_0.9vae.safetensors",
                    size_bytes: 6_938_078_334,
                    label: "0.9 VAE",
                }],
            },
            ImageRecipeComponent {
                role: "vae",
                repo: "madebyollin/sdxl-vae-fp16-fix",
                file: "sdxl_vae.safetensors",
                size_bytes: 334_641_162,
                gated: false,
                note: "the fp16-safe VAE: the one bundled in the checkpoint produces NaNs in \
                       half precision, which is what this re-scaled copy exists to fix",
                alternatives: &[],
            },
        ],
        args: &[
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. The only arg SDXL needs is its native 1024² size \
                    — sd-server's own defaults (20 steps, cfg-scale 7.0, euler_a) are this \
                    family's, unlike the distilled ones above.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "sd3.5-large",
        display_name: "Stable Diffusion 3.5 Large",
        description: "Stability's 8B MMDiT, in the fp8 form Comfy-Org publishes un-gated. \
                      The only family here that reads its prompt through three encoders at \
                      once — CLIP-L, CLIP-G and T5.",
        components: &[
            ImageRecipeComponent {
                role: "model",
                repo: "Comfy-Org/stable-diffusion-3.5-fp8",
                file: "sd3.5_large_fp8_scaled.safetensors",
                size_bytes: 14_934_922_866,
                gated: false,
                note: "loaded with -m rather than --diffusion-model, like SDXL: \
                       stabilityai/stable-diffusion-3.5-large keeps the same weights behind \
                       a licence gate, and this is the scaled-fp8 re-publication of them",
                alternatives: &[],
            },
            FLUX_CLIP_L,
            ImageRecipeComponent {
                role: "clip_g",
                repo: "Comfy-Org/stable-diffusion-3.5-fp8",
                file: "text_encoders/clip_g.safetensors",
                size_bytes: 1_389_382_176,
                gated: false,
                note: "the bigger of SD3.5's two CLIP encoders — the one component of this \
                       pipeline no other row here wants",
                alternatives: &[],
            },
            FLUX_T5XXL,
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(4.5)),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet. CLIP-L and T5 are pointed at the \
                    comfyanonymous/flux_text_encoders copies the FLUX rows already use \
                    rather than at this repo's own text_encoders/, whose files are the same \
                    size to the byte — so a box with FLUX.1 downloads the checkpoint and \
                    CLIP-G and nothing else. The 4.5 cfg-scale is the upstream docs'; SD3.5 \
                    is not distilled.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "qwen-image",
        display_name: "Qwen-Image",
        description: "The strongest open model for rendering *text* inside a picture — signs, \
                      labels, UI mockups, Chinese and English alike — at the cost of being \
                      the largest pipeline here.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "QuantStack/Qwen-Image-GGUF",
                file: "Qwen_Image-Q4_K_M.gguf",
                size_bytes: 13_065_746_976,
                gated: false,
                note: "a 20B transformer, so even Q4_K_M is 13 GB — the larger quants need a \
                       card this one will not fit beside its encoder",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q2_K.gguf",
                        size_bytes: 7_062_518_304,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q3_K_M.gguf",
                        size_bytes: 9_679_567_392,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q4_K_S.gguf",
                        size_bytes: 12_140_608_032,
                        label: "Q4_K_S",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q5_K_M.gguf",
                        size_bytes: 14_934_899_232,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q6_K.gguf",
                        size_bytes: 16_824_990_240,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen_Image-Q8_0.gguf",
                        size_bytes: 21_761_817_120,
                        label: "Q8_0",
                    },
                ],
            },
            QWEN_IMAGE_VAE,
            QWEN25_VL_LLM,
        ],
        args: &[
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Not measured yet, and one of the two largest rows here: 18 GB of \
                    files at the default quants. Qwen-Image is not guidance-distilled, so \
                    sd-server's own cfg-scale default applies rather than the 1.0 the FLUX \
                    and Z-Image rows carry.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "qwen-image-2.1",
        display_name: "Qwen-Image 2.1",
        description: "The second Qwen-Image generation: a new DiT with its own VAE and \
                      Qwen3-VL-8B reading the prompt. Draws and edits, at roughly a third of \
                      the first generation's size on disk.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "leejet/Qwen-Image-2.1-GGUF",
                file: "qwen_image_2.1-Q4_K.gguf",
                size_bytes: 4_197_494_816,
                gated: false,
                note: "the 2.1 transformer — 4 GB at Q4_K against the first generation's 13, \
                       which is what makes this the Qwen row a 24 GiB card runs comfortably",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q2_K.gguf",
                        size_bytes: 2_561_716_256,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q3_K.gguf",
                        size_bytes: 3_270_553_632,
                        label: "Q3_K",
                    },
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q4_0.gguf",
                        size_bytes: 4_197_494_816,
                        label: "Q4_0",
                    },
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q5_0.gguf",
                        size_bytes: 5_069_910_048,
                        label: "Q5_0",
                    },
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q6_K.gguf",
                        size_bytes: 5_996_851_232,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "qwen_image_2.1-Q8_0.gguf",
                        size_bytes: 7_687_155_744,
                        label: "Q8_0",
                    },
                ],
            },
            ImageRecipeComponent {
                role: "vae",
                repo: "Comfy-Org/Qwen-Image-2.1",
                file: "vae/qwen_image_2.1_vae_bf16.safetensors",
                size_bytes: 675_509_688,
                gated: false,
                note: "2.1's own autoencoder — upstream is explicit that the first \
                       Qwen-Image and Wan 2.2 VAEs are not interchangeable with it",
                alternatives: &[],
            },
            ImageRecipeComponent {
                role: "llm",
                repo: "Qwen/Qwen3-VL-8B-Instruct-GGUF",
                file: "Qwen3VL-8B-Instruct-Q4_K_M.gguf",
                size_bytes: 5_027_784_800,
                gated: false,
                note: "the prompt encoder 2.1 replaced Qwen2.5-VL with, from Qwen's own \
                       GGUF repo",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Qwen3VL-8B-Instruct-Q8_0.gguf",
                        size_bytes: 8_709_519_456,
                        label: "Q8_0",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen3VL-8B-Instruct-F16.gguf",
                        size_bytes: 16_388_044_896,
                        label: "F16",
                    },
                ],
            },
            ImageRecipeComponent {
                role: "llm_vision",
                repo: "Qwen/Qwen3-VL-8B-Instruct-GGUF",
                file: "mmproj-Qwen3VL-8B-Instruct-F16.gguf",
                size_bytes: 1_159_029_824,
                gated: false,
                note: "the vision tower that goes with the GGUF encoder — upstream requires \
                       it for editing, and it is what lets this row claim edit at all",
                alternatives: &[ImageRecipeAlternative {
                    file: "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf",
                    size_bytes: 752_289_728,
                    label: "Q8_0",
                }],
            },
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(6.0)),
            ("sampling_method", RecipeArg::Text("euler")),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: true,
        vram_note: "Not measured yet. 11 GB of files. Both the 6.0 cfg-scale and the \
                    explicit euler are the upstream docs': sd-server's sampler default is \
                    euler_a for everything outside Flux/SD3/Wan, so this family has to say \
                    so. Upstream also asks for dimensions divisible by 32.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "qwen-image-edit-2509",
        display_name: "Qwen-Image-Edit 2509",
        description: "The edit head on the first Qwen-Image: hand it reference images and \
                      describe the change. The September refresh, which takes several \
                      references at once.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "QuantStack/Qwen-Image-Edit-2509-GGUF",
                file: "Qwen-Image-Edit-2509-Q4_K_S.gguf",
                size_bytes: 12_204_309_024,
                gated: false,
                note: "the same 20B shape as Qwen-Image, so the same warning applies: the \
                       larger quants do not fit beside the encoder on a 24 GiB card",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q2_K.gguf",
                        size_bytes: 7_147_452_960,
                        label: "Q2_K",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q3_K_M.gguf",
                        size_bytes: 9_764_502_048,
                        label: "Q3_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q4_K_M.gguf",
                        size_bytes: 13_065_746_976,
                        label: "Q4_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q5_K_M.gguf",
                        size_bytes: 14_934_899_232,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q6_K.gguf",
                        size_bytes: 16_824_990_240,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "Qwen-Image-Edit-2509-Q8_0.gguf",
                        size_bytes: 21_761_817_120,
                        label: "Q8_0",
                    },
                ],
            },
            QWEN_IMAGE_VAE,
            QWEN25_VL_LLM,
            ImageRecipeComponent {
                role: "llm_vision",
                repo: "unsloth/Qwen2.5-VL-7B-Instruct-GGUF",
                file: "mmproj-F16.gguf",
                size_bytes: 1_354_163_040,
                gated: false,
                note: "the vision tower beside the encoder — the file qwen-image's note used \
                       to say you had to add by hand, and the reason this row can read a \
                       reference image at all",
                alternatives: &[ImageRecipeAlternative {
                    file: "mmproj-F32.gguf",
                    size_bytes: 2_703_221_600,
                    label: "F32",
                }],
            },
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(2.5)),
            ("sampling_method", RecipeArg::Text("euler")),
            ("flow_shift", RecipeArg::Float(3.0)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(1024)),
            ("height", RecipeArg::Int(1024)),
        ],
        modes: &["img_gen"],
        edit: true,
        vram_note: "Not measured yet, and the largest row here: 18 GB at the default \
                    quants, of which the VAE and the encoder are shared with qwen-image. All \
                    four args are the upstream docs' — 2.5 cfg-scale, euler, flow-shift 3, \
                    flash attention.",
    },
    // -----------------------------------------------------------------------
    ImageRecipe {
        key: "wan2.1-t2v-1.3b",
        display_name: "Wan 2.1 T2V 1.3B",
        description: "The one video pipeline here: a prompt to a short 832×480 clip. Wan \
                      needs sd-server's vid_gen mode even for a single frame, so this row is \
                      vid_gen only — lmgw publishes it as video_generation, and what comes \
                      back from the generations route is an encoded clip rather than a still.",
        components: &[
            ImageRecipeComponent {
                role: "diffusion_model",
                repo: "Comfy-Org/Wan_2.1_ComfyUI_repackaged",
                file: "split_files/diffusion_models/wan2.1_t2v_1.3B_fp16.safetensors",
                size_bytes: 2_838_303_560,
                gated: false,
                note: "the smallest Wan there is — the 14B variants and every Wan 2.2 are a \
                       different order of card, which is why the small one is the row that \
                       ships",
                alternatives: &[ImageRecipeAlternative {
                    file: "split_files/diffusion_models/wan2.1_t2v_1.3B_bf16.safetensors",
                    size_bytes: 2_838_104_528,
                    label: "bf16",
                }],
            },
            ImageRecipeComponent {
                role: "vae",
                repo: "Comfy-Org/Wan_2.1_ComfyUI_repackaged",
                file: "split_files/vae/wan_2.1_vae.safetensors",
                size_bytes: 253_815_318,
                gated: false,
                note: "Wan's own autoencoder, and upstream warns it is the VRAM-hungry part \
                       of the pipeline — files.taesd is the escape hatch if it does not fit",
                alternatives: &[],
            },
            ImageRecipeComponent {
                role: "t5xxl",
                repo: "city96/umt5-xxl-encoder-gguf",
                file: "umt5-xxl-encoder-Q8_0.gguf",
                size_bytes: 6_043_068_256,
                gated: false,
                note: "umT5, not the T5-XXL the FLUX rows share — a different encoder under \
                       the same flag, so nothing is reused between them",
                alternatives: &[
                    ImageRecipeAlternative {
                        file: "umt5-xxl-encoder-Q4_K_M.gguf",
                        size_bytes: 3_655_145_312,
                        label: "Q4_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "umt5-xxl-encoder-Q5_K_M.gguf",
                        size_bytes: 4_145_878_880,
                        label: "Q5_K_M",
                    },
                    ImageRecipeAlternative {
                        file: "umt5-xxl-encoder-Q6_K.gguf",
                        size_bytes: 4_667_283_296,
                        label: "Q6_K",
                    },
                    ImageRecipeAlternative {
                        file: "umt5-xxl-encoder-F16.gguf",
                        size_bytes: 11_368_687_456,
                        label: "F16",
                    },
                ],
            },
        ],
        args: &[
            ("cfg_scale", RecipeArg::Float(6.0)),
            ("flow_shift", RecipeArg::Float(3.0)),
            ("video_frames", RecipeArg::Int(33)),
            ("diffusion_fa", RecipeArg::Switch),
            ("width", RecipeArg::Int(832)),
            ("height", RecipeArg::Int(480)),
        ],
        modes: &["vid_gen"],
        edit: false,
        vram_note: "Not measured yet. 9 GB of files, none of them shared with any \
                    other row. Every arg is the upstream docs' 1.3B example — 33 frames at \
                    832×480, cfg-scale 6.0, flow-shift 3.0 — and upstream's own example also \
                    passes a long Chinese negative prompt that a row cannot carry, so set \
                    args.negative_prompt from the docs if the output disappoints.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A key is the seed of a `model_id`, so it has to read as one: lowercase,
    /// no whitespace, nothing that needs escaping anywhere it is printed.
    ///
    /// A `.` is allowed because the families themselves carry one —
    /// `qwen-image-2.1`, `sd3.5-large` — and spelling those `qwen-image-21`
    /// would invent a name upstream does not use. Version numbers are how this
    /// corner of the world names things; `gpt-4.1` is a model id too.
    #[test]
    fn keys_are_unique_and_url_safe() {
        let mut seen: Vec<&str> = Vec::new();
        for r in all() {
            assert!(!seen.contains(&r.key), "duplicate recipe key {}", r.key);
            assert!(
                r.key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'),
                "{} is not a client-facing id",
                r.key
            );
            assert!(!r.key.contains(".."), "{} would escape a path", r.key);
            seen.push(r.key);
        }
    }

    #[test]
    fn files_render_in_the_downloader_layout() {
        let r = find("z-image-turbo").unwrap();
        let files = r.files_json(None).unwrap();
        assert_eq!(
            files["diffusion_model"],
            "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf"
        );
        assert_eq!(
            files["vae"],
            "Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors"
        );
        assert_eq!(
            files["llm"],
            "unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
        );
        // …and the quant picker swaps exactly one of them.
        let picked = r.files_json(Some("z_image_turbo-Q8_0.gguf")).unwrap();
        assert_eq!(
            picked["diffusion_model"],
            "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q8_0.gguf"
        );
        assert_eq!(picked["llm"], files["llm"]);
        let err = r.files_json(Some("z_image_turbo-Q9_Z.gguf")).unwrap_err();
        assert!(err.contains("is not a file of z-image-turbo's"), "{err}");
    }

    #[test]
    fn args_keep_the_type_the_flag_needs() {
        let a = find("z-image-turbo").unwrap().args_json();
        assert_eq!(a["cfg_scale"], Value::from(1.0));
        assert!(
            a["cfg_scale"].is_f64(),
            "--cfg-scale 1 is not --cfg-scale 1.0"
        );
        assert_eq!(a["steps"], Value::from(8));
        assert_eq!(a["diffusion_fa"], Value::Bool(true));
    }

    #[test]
    fn a_path_finds_its_recipe_by_repo_then_by_name() {
        let (r, c) =
            match_path("leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf").expect("exact");
        assert_eq!(r.key, "z-image-turbo");
        assert_eq!(c.role, "diffusion_model");
        // An alternative quant of the same component.
        let (r, _) = match_path("leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q8_0.gguf").unwrap();
        assert_eq!(r.key, "z-image-turbo");
        // The in-container spelling a caller pastes out of the rendered argv.
        let (r, _) = match_path("/models/city96/FLUX.1-dev-gguf/flux1-dev-Q8_0.gguf").unwrap();
        assert_eq!(r.key, "flux1-dev");
        // A hand-placed copy under some other owner still matches by name.
        let (r, c) = match_path("mine/elsewhere/sd_xl_base_1.0.safetensors").unwrap();
        assert_eq!(r.key, "sdxl-base");
        assert_eq!(c.role, "model");
        assert!(match_path("nobody/knows/this-one.safetensors").is_none());
    }
}
