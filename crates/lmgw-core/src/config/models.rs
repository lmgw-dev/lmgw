//! Local/aux/audio/image model rows, and the shared GPU-hold fallback mode
//! they all carry.

use serde::{Deserialize, Serialize};

use super::*;

/// Per-model override of the GPU-hold fallback lookup (`2026-09-04-gpu-hold-
/// design.md` §2/§3.2): whether a held request against this row falls back to
/// the global chat alias, refuses outright, or names its own alias.
///
/// Two columns rather than a `""` sentinel on `hold_fallback` alone: every
/// patch struct in this crate already treats an empty string as "not
/// supplied" (`ops::opt`), so a single `Option<String>` could not distinguish
/// "inherit" from "explicitly none" from "not touched by this patch" at the
/// same time. `Default` is `Inherit` so a row that has never touched this
/// keeps behaving exactly as the global setting says.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HoldFallbackMode {
    /// Chat: fall through to `settings.hold.fallback_alias`. Aux/audio: no
    /// fallback — they never inherit the global (§2: a different embedding
    /// model silently corrupts a vector index).
    #[default]
    Inherit,
    /// Refuse a held request against this row even if a global fallback is
    /// configured.
    None,
    /// Use this row's own `hold_fallback` alias.
    Alias,
}

impl HoldFallbackMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::None => "none",
            Self::Alias => "alias",
        }
    }
}

impl std::str::FromStr for HoldFallbackMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "inherit" => Ok(Self::Inherit),
            "none" => Ok(Self::None),
            "alias" => Ok(Self::Alias),
            _ => Err(()),
        }
    }
}

/// A local llama.cpp model, served by llama-server in its own container
/// (per-model-containers §3.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModel {
    pub id: i64,
    /// Client-facing model id: what `--alias` makes llama-server answer to,
    /// what the container name is derived from, and what aliases point at.
    pub model_id: String,
    /// GGUF path relative to the models dir (mounted at /models in-container).
    pub gguf_path: String,
    /// Common llama-server params with dedicated form fields.
    #[serde(default)]
    pub params: LlamaParams,
    /// Extra llama-server flags without a dedicated field, e.g.
    /// ["--spec-type","draft-mtp"]; spliced into the rendered argv.
    pub args: Vec<String>,
    /// Idle seconds before lmgw stops this model's container and frees the
    /// GPU (§3.7; 0 = never).
    pub idle_seconds: i64,
    pub enabled: bool,
    /// Routable through the gateway without a manual alias: the model id
    /// (optionally under `RouterSettings::public_prefix`) resolves to the
    /// built-in router upstream and shows up in `GET /v1/models`.
    pub public: bool,
    /// Per-model container image override (per-model-containers design §3.1);
    /// `None` inherits [`RouterSettings::image`].
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits
    /// [`RouterSettings::extra_run_args`].
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2). `Inherit` reads
    /// `settings.hold.fallback_alias` (this is the chat class, so it does
    /// inherit); `Alias` reads [`Self::hold_fallback`] instead.
    #[serde(default)]
    pub hold_fallback_mode: HoldFallbackMode,
    /// The alias to route to while the GPU is held, when
    /// [`Self::hold_fallback_mode`] is `Alias`. Meaningless under any other
    /// mode — [`Snapshot::hold_fallback_for`] never reads it then.
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object with optional keys
    /// `capabilities` (deep-merged over what the GGUF/config derives),
    /// `max_output_tokens`, `notes` (appended). Applied by
    /// `capabilities::apply_owner_override`, which sets `source: "owner"` on
    /// the result. `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
    /// Ladder rungs above the base (ladder design §4.1): the row's own
    /// `gguf_path` + `params.ctx_size` are rung 1, unchanged; each entry here
    /// is one higher rung. Empty (the default) means "not a ladder" — see
    /// [`Self::is_ladder`]. `#[serde(default)]` so a frame or a row written
    /// before this field existed reads as "not a ladder" rather than failing
    /// to parse.
    #[serde(default)]
    pub ladder: Vec<crate::ladder::Rung>,
}

impl LocalModel {
    /// Move any [`PROMOTED_ARGS`](crate::config::llama_params) out of `args` and into `params`.
    ///
    /// Only fills a slot that is still unset: an explicitly configured field
    /// always wins over a leftover arg, and the arg is dropped either way so
    /// the renderer cannot emit the same flag twice. Paths are stored
    /// relative to the models dir, matching `gguf_path`, so an arg written as
    /// the in-container `/models/foo/bar.gguf` loses that prefix here.
    pub fn hoist_promoted_args(&mut self) {
        hoist_promoted_args_into(&mut self.params, &mut self.args);
    }
}

/// What an [`AuxModel`] is served as. Both kinds are llama.cpp GGUFs in their
/// own containers; they differ only in which flag the rendered argv carries
/// and which ingress may route to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuxKind {
    /// `--embeddings` — served through `/v1/embeddings`.
    Embed,
    /// `--reranking` — served through llama-server's `/v1/rerank`.
    ///
    /// **Never** through `/v1/embeddings`: llama-server answers an embeddings
    /// request against a reranker with HTTP 200 and an all-zero
    /// vector, which would poison whatever corpus is being built. The refusal
    /// lives in [`Snapshot::aux_model_for`]'s caller (`proxy::embeddings_inner`).
    Rerank,
}

impl AuxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Embed => "embed",
            Self::Rerank => "rerank",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "embed" => Some(Self::Embed),
            "rerank" => Some(Self::Rerank),
            _ => None,
        }
    }
}

/// A model served by the *second* llama-server router container — the **aux**
/// router (§8), so named because it hosts every small stateless model class:
/// embeddings and rerankers. Slimmer than [`LocalModel`] — neither class uses
/// chat sampling/jinja/speculative-decode params. Exposure goes through the
/// synthetic aux upstream under [`Settings::aux_router`]'s prefix, so there is
/// no `public` flag here: enabled = routable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxModel {
    pub id: i64,
    /// Client-facing id (under the aux upstream prefix); also what `--alias`
    /// makes llama-server answer to.
    pub model_id: String,
    /// GGUF path relative to the aux models dir (mounted at /models).
    pub gguf_path: String,
    /// Embedding or reranking; decides the flag the rendered argv carries.
    pub kind: AuxKind,
    /// `--pooling` strategy (`none` | `mean` | `cls` | `last` | `rank`);
    /// `None` lets llama-server pick the model's default. Always `None` for
    /// [`AuxKind::Rerank`] — `reranking = true` selects rank pooling itself and
    /// a second `pooling` key breaks it (spike-verified).
    pub pooling: Option<String>,
    /// `--ctx-size`; `None` = llama-server default.
    pub ctx_size: Option<i64>,
    /// Freeform extra llama-server flags without a dedicated field.
    pub args: Vec<String>,
    /// `--sleep-idle-seconds` (0 = never sleep).
    pub idle_seconds: i64,
    pub enabled: bool,
    /// Per-model container image override (§3.1); `None` inherits
    /// [`Settings::aux_router`]'s `image`.
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits
    /// [`Settings::aux_router`]'s `extra_run_args`.
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2). `Inherit` means "no
    /// fallback" for this class — aux models never inherit the global chat
    /// alias, because a different embedding model silently corrupts a vector
    /// index. Only `Alias` (reading [`Self::hold_fallback`]) gives this row
    /// one.
    #[serde(default)]
    pub hold_fallback_mode: HoldFallbackMode,
    /// The alias to route to while the GPU is held, when
    /// [`Self::hold_fallback_mode`] is `Alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
}

/// A model served by the audio.cpp container (`audiocpp_server`). One entry
/// per `models[]` item in the generated `server.json`. Exposure and aliasing
/// go through a managed upstream row (see [`Settings::audio`]), the same
/// pattern as the aux router: enabled = in the config = in the upstream's
/// catalog = routable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioModel {
    pub id: i64,
    /// Server id / client-facing id (under the audio upstream prefix).
    pub model_id: String,
    /// audio.cpp model-loader family (`qwen3_tts`, `pocket_tts`, …).
    pub family: String,
    /// Model root directory, relative to the audio models dir (mounted at
    /// /models in-container); rendered as `/models/<path>` in server.json.
    pub path: String,
    /// `--task`: tts | asr | gen | clon | vc | svc | s2s | sep | vad | diar |
    /// align | vdes | spk | midi.
    pub task: String,
    /// `offline` (default) or `streaming` (families with streaming support).
    pub mode: String,
    /// Per-model override of [`AudioSettings::lazy_load`]: `Some(false)` loads
    /// this model at container start even though the class defers loads,
    /// `Some(true)` the other way round. `None` inherits the class.
    #[serde(default)]
    pub lazy: Option<bool>,
    /// Per-model `busy_timeout_ms`, and the **ceiling** a request's own
    /// `busy_timeout_ms` is clamped to. `None` inherits
    /// [`AudioSettings::busy_timeout_ms`]. Model runtimes differ by orders of
    /// magnitude — a TTS clip is seconds, a music generation is minutes — so
    /// one class-wide bound is either too tight for the slow rows or useless
    /// for the fast ones.
    #[serde(default)]
    pub busy_timeout_ms: Option<i64>,
    /// `default_request_options`: request options applied to every request for
    /// this model, each overridable by the request that names it. Where
    /// `load_options` and `session_options` configure the *model*, these are
    /// defaults for the *call* — a language, a speaking rate, a step count.
    #[serde(default)]
    pub default_request_options: serde_json::Map<String, serde_json::Value>,
    /// `model_spec_override`: a `<family>.json` spec file (or a directory of
    /// them) that replaces the catalog lookup for this row, as a path relative
    /// to the audio models dir. This is what serves a family whose spec the
    /// container image does not carry yet — the engine binary's `model_specs`
    /// are baked in at build time, and the family list moves faster than the
    /// image does.
    #[serde(default)]
    pub model_spec_override: Option<String>,
    /// `config` / `weight`: which named asset to load when the model directory
    /// holds more than one — a spec's discovered configs and weights are
    /// addressed by id. `None` lets the loader pick, which is right whenever
    /// the directory is unambiguous.
    #[serde(default)]
    pub config_id: Option<String>,
    #[serde(default)]
    pub weight_id: Option<String>,
    /// Model-load options (`load_options` in server.json), e.g. language.
    pub load_options: serde_json::Map<String, serde_json::Value>,
    /// Session/runtime options (`session_options` in server.json), e.g.
    /// quantization weight types.
    pub session_options: serde_json::Map<String, serde_json::Value>,
    /// Named voice presets (`voice_presets` in server.json): name -> either
    /// `{"voice_id": …}` (a voice the model ships) or `{"voice_ref": …,
    /// "reference_text": …}` (a clip to clone from). A request's `voice` picks
    /// one by name. Without presets a cloning TTS model draws a new random
    /// speaker per request, so this is what makes a voice repeatable.
    pub voice_presets: serde_json::Map<String, serde_json::Value>,
    /// `default_voice_preset`: used when a request names no voice. Either a
    /// preset name from the map above or an inline preset object; `None`
    /// leaves the key out of server.json entirely.
    pub default_voice_preset: Option<serde_json::Value>,
    pub enabled: bool,
    /// Per-model container image override (§3.1); `None` inherits
    /// [`AudioSettings::image`].
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits
    /// [`AudioSettings::extra_run_args`].
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2). Same rule as
    /// [`AuxModel::hold_fallback_mode`]: `Inherit` means no fallback for this
    /// class, only `Alias` gives this row one.
    #[serde(default)]
    pub hold_fallback_mode: HoldFallbackMode,
    /// The alias to route to while the GPU is held, when
    /// [`Self::hold_fallback_mode`] is `Alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
}

/// One image-generation pipeline, served by stable-diffusion.cpp's
/// `sd-server` in its own container (image-generation design §4).
///
/// The fourth local class, and the one that names its weights as a **map**
/// rather than as columns: one row is one pipeline, and which files a pipeline
/// needs depends on its family (a standalone DiT plus a VAE plus one to three
/// text encoders, or a single all-in-one checkpoint). sd.cpp adds a family —
/// and its flag — every few weeks, so [`Self::files`]' keys are validated
/// against the image's own `--help` ([`crate::sdcpp_caps`]) instead of being
/// frozen into a Rust enum here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageModel {
    pub id: i64,
    /// Client-facing id under the image prefix (`image/<model_id>`), and what
    /// the container name is derived from.
    pub model_id: String,
    /// Flag key → path **relative to the image models dir**: `model`,
    /// `diffusion_model`, `vae`, `clip_l`, `clip_g`, `t5xxl`, `llm`,
    /// `llm_vision`, `clip_vision`, `taesd`, `control_net`, `ip_adapter`,
    /// `photo_maker`, `upscale_model`, `high_noise_diffusion_model`,
    /// `tokenizer`, `lora_model_dir`, `hires_upscalers_dir`, `embd_dir`, …
    ///
    /// Keys are the long flag with `-` → `_`
    /// ([`crate::sdcpp_caps::canonical_key`]); the renderer maps each back to
    /// the image's exact spelling, which mixes separators (`--clip_l` but
    /// `--diffusion-model`). **Exactly one of `model` / `diffusion_model` is
    /// required** — an all-in-one checkpoint or a standalone diffusion model,
    /// never both and never neither.
    #[serde(default)]
    pub files: serde_json::Map<String, serde_json::Value>,
    /// Runtime + default-generation flags, same canonical-key convention:
    /// `type`, `offload_to_cpu`, `diffusion_fa`, `vae_tiling`, `cfg_scale`,
    /// `steps`, `sampling_method`, `width`, `height`, … A JSON `true` renders
    /// as a bare switch, anything else as `--flag value`.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
    /// What the operator says this pipeline does: `["img_gen"]` or
    /// `["img_gen","vid_gen"]`. Drives `task`/`endpoints` on `/v1/models`
    /// before the container has ever run, and is compared against the probed
    /// `supported_modes` at start — a mismatch is a warning, never a silent
    /// correction.
    #[serde(default)]
    pub modes: Vec<String>,
    /// The pipeline takes reference images (Kontext, Qwen-Image-Edit,
    /// Z-Image-Omni) and may serve `/v1/images/edits`.
    ///
    /// Load-bearing rather than descriptive: sd-server **segfaults** on a
    /// reference-image request against a pipeline that cannot take one
    /// (measured, §12.8), so lmgw refuses that request itself.
    #[serde(default)]
    pub edit: bool,
    pub enabled: bool,
    /// Per-model container image override; `None` inherits
    /// [`ImageSettings::image`].
    pub image: Option<String>,
    /// Per-model `podman run` args override; `None` inherits
    /// [`ImageSettings::extra_run_args`].
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch.
    pub warm_start: bool,
    /// Idle seconds before lmgw stops this model's container and frees the
    /// GPU (0 = never). A real column from day one, unlike audio's: a loaded
    /// pipeline is 7–13 GiB nothing else can use (§3, §9).
    pub idle_seconds: i64,
    /// GPU-hold fallback mode. Same rule as [`AuxModel::hold_fallback_mode`]:
    /// `Inherit` means no fallback for this class, only `Alias` gives this row
    /// one — the global chat alias is not an image model.
    #[serde(default)]
    pub hold_fallback_mode: HoldFallbackMode,
    /// The alias to route to while the GPU is held, when
    /// [`Self::hold_fallback_mode`] is `Alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7), exactly as [`LocalModel`] types it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
    /// The learned transient peak: the largest `used − idle` delta the VRAM
    /// sampler has measured on the device while this row had a request in
    /// flight (§9, and [`crate::vram::peak`]).
    ///
    /// `None` is "not learned yet" and is charged as nothing — never as a
    /// guessed multiple of the file sizes. This is the only number in the row
    /// lmgw writes by itself; everything else here is the owner's.
    #[serde(default)]
    pub peak_extra_bytes: Option<u64>,
    /// When [`Self::peak_extra_bytes`] was observed (`datetime('now')`), so a
    /// surface can say how old the figure is. `None` whenever the peak is.
    #[serde(default)]
    pub peak_learned_at: Option<String>,
}

/// The one mode every image row has unless it says otherwise.
pub const IMAGE_MODE_DEFAULT: &str = "img_gen";

impl ImageModel {
    /// The declared modes, with the default applied for a row that names
    /// none — so a caller never has to decide what an empty list means.
    pub fn modes(&self) -> Vec<String> {
        if self.modes.is_empty() {
            vec![IMAGE_MODE_DEFAULT.to_string()]
        } else {
            self.modes.clone()
        }
    }
}
