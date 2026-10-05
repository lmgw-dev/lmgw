//! Answering questions *about* model files (§20).
//!
//! [`crate::ops`] changes configuration; this module works out what the
//! configuration should be. The split matters because the hard part of adding
//! a local model is not writing the row — it is knowing that the repo's three
//! similar-looking GGUFs are weights, a vision projector and a speculative
//! drafter, that the drafter's architecture dictates which `--spec-type` value
//! is correct, and that the llama.cpp build in the container may not know the
//! architecture at all.
//!
//! All of that is derivable: from the GGUF headers ([`crate::gguf`]), from the
//! files sitting next to each other on disk, and from asking the running
//! llama-server. Deriving it here means a caller needs no llama.cpp expertise
//! — which is the difference between a tool surface an arbitrary agent can
//! drive and one only its author can.

use serde_json::{json, Value};

use crate::capabilities::{self, ModelCapabilities};
use crate::config::{AuxKind, LlamaParams, LocalModel, Snapshot};
use crate::gguf::{self, ModelSummary};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Resolve a caller-supplied path against the models dir.
///
/// Reachable from a remote MCP client, so `..` is refused outright rather than
/// normalized: the models dir is the boundary, and there is no legitimate
/// reason to leave it. A leading `/models/` (the in-container spelling) or `/`
/// is tolerated because that is how the path appears in the rendered argv, and
/// a caller copying one back is making an understandable mistake.
pub(crate) fn resolve(
    models_dir: &str,
    rel: &str,
    class: Class,
) -> Result<(String, std::path::PathBuf), String> {
    let rel = rel
        .trim()
        .trim_start_matches("/models/")
        .trim_start_matches('/');
    if rel.is_empty() {
        return Err("gguf_path must not be empty".into());
    }
    if rel.split('/').any(|c| c == ".." || c == ".") {
        return Err(format!("'{rel}' must be a plain path under the models dir"));
    }
    if models_dir.trim().is_empty() {
        return Err(format!(
            "the {} models directory is not configured (Settings → {})",
            class.as_str(),
            settings_page(class)
        ));
    }
    let full = std::path::Path::new(models_dir).join(rel);
    if !full.is_file() {
        let t = class.as_str();
        return Err(format!(
            "'{rel}' is not in the {t} models directory ({models_dir}) — call lmgw__gguf_files \
             target={t} to see what is, or lmgw__hf_add target={t} to download it. The four \
             classes each have their own directory and a file in another class's directory \
             is not visible here: embedding / rerank models live in the aux dir (target=aux), \
             chat models in the chat dir (target=chat), audio.cpp models in the audio dir \
             (target=audio), stable-diffusion.cpp pipelines in the image dir (target=image)."
        ));
    }
    Ok((rel.to_string(), full))
}

/// The class a `target` argument names, defaulting to chat — the spelling
/// every file-reading tool takes so an embedding model in the aux dir is
/// addressable at all.
fn class_of(target: Option<&str>) -> Result<Class, String> {
    Ok(crate::ops::parse_class_target(target)?.unwrap_or(Class::Chat))
}

/// The models dir a class's files are relative to.
fn models_dir_of(snap: &Snapshot, class: Class) -> String {
    match class {
        Class::Chat => snap.settings.router.models_dir.clone(),
        Class::Aux => snap.settings.aux_router.models_dir.clone(),
        Class::Audio => snap.settings.audio.models_dir.clone(),
        Class::Image => snap.settings.image.models_dir.clone(),
    }
}

/// Where the dashboard configures a class's models dir.
/// Where the dashboard sets a class's directory (Settings → Runtimes → …).
fn settings_page(class: Class) -> &'static str {
    match class {
        Class::Chat => "Runtimes → Chat",
        Class::Aux => "Runtimes → Aux",
        Class::Audio => "Runtimes → Audio",
        Class::Image => "Runtimes → Image",
    }
}

/// `gguf::summarize` does blocking file I/O; keep it off the async runtime.
async fn summarize(full: std::path::PathBuf) -> Result<ModelSummary, String> {
    tokio::task::spawn_blocking(move || gguf::summarize(&full))
        .await
        .map_err(|e| format!("gguf read panicked: {e}"))?
        .map_err(|e| format!("not a readable GGUF: {e}"))
}

// ---------------------------------------------------------------------------
// Classification — what a file is, and how to configure it
// ---------------------------------------------------------------------------

/// What a GGUF is for, decided from its own metadata rather than its name.
///
/// The order matters. A projector is unambiguous (`general.type = mmproj`, or
/// a `clip.*` architecture). A drafter is identified by architecture first —
/// `dflash`, `dspark`, `eagle3` are draft-module architectures that exist for
/// no other purpose — and only then by the weaker signal of carrying MTP
/// tensors while being far too small to be the model itself. Full models
/// commonly carry MTP tensors too (that is the whole point of an
/// `-MTP-GGUF` repo), so tensor presence alone would misclassify them.
pub(crate) fn role_of(s: &ModelSummary) -> &'static str {
    if s.is_mmproj {
        return "mmproj";
    }
    let arch = s.architecture.as_deref().unwrap_or("");
    if matches!(arch, "dflash" | "dspark" | "eagle3" | "eagle" | "medusa") {
        return "drafter";
    }
    // A standalone MTP module: MTP tensors, and few enough layers that it
    // cannot be a model in its own right.
    if s.has_mtp_layers && s.block_count.is_some_and(|b| b <= 8) {
        return "drafter";
    }
    if s.architecture.is_some() {
        return "weights";
    }
    "unknown"
}

/// Architectures that only ever ship as encoders: an embedding model whose
/// converter predates `pooling_type`, or one written by hand, is still
/// recognisable by these. Everything else is decided by the header's own
/// `pooling_type` / classifier-head signals ([`gguf::ModelSummary`]).
const ENCODER_ARCHES: &[&str] = &[
    "bert",
    "nomic-bert",
    "nomic-bert-moe",
    "jina-bert-v2",
    "gemma-embedding",
    "modern-bert",
    "neo-bert",
    "t5encoder",
];

/// Which class serves a GGUF, and — for the aux class — as which kind.
///
/// Decided from the header, never from the filename: `Qwen3-Embedding-4B` is
/// a `qwen3`-architecture file that differs from the chat model only in the
/// `pooling_type` its converter wrote, and that key is what says "serve me
/// with `--embeddings`, I have no LM head to generate with". A reranker is
/// the same encoder with a classifier head (`pooling_type = rank`, or the
/// `cls.output` tensors on older conversions). Projectors and drafters are
/// companions of a chat model and classify as chat.
pub(crate) fn class_for(s: &ModelSummary) -> (&'static str, Option<AuxKind>) {
    if role_of(s) != "weights" {
        return ("chat", None);
    }
    let pooling = s.pooling_type.and_then(gguf::pooling_name);
    if pooling == Some("rank") || s.has_classifier_head {
        return ("aux", Some(AuxKind::Rerank));
    }
    let arch = s.architecture.as_deref().unwrap_or("");
    if s.pooling_type.is_some() || ENCODER_ARCHES.contains(&arch) {
        return ("aux", Some(AuxKind::Embed));
    }
    ("chat", None)
}

/// The `--spec-type` value that matches a drafter, or `None` if the file is
/// not one.
///
/// This mapping is the single most valuable thing in this module. A published
/// model card for Muse-Glimmer said to pass `--spec-type draft`; the value the
/// runtime actually accepts is `draft-dflash`, and the wrong one produces a
/// config that renders perfectly and dies at load.
fn spec_type_for(s: &ModelSummary) -> Option<&'static str> {
    if s.is_mmproj {
        return None;
    }
    match s.architecture.as_deref() {
        Some("dflash") => Some("draft-dflash"),
        Some("dspark") => Some("draft-dspark"),
        Some("eagle3") | Some("eagle") => Some("draft-eagle3"),
        _ if s.has_mtp_layers => Some("draft-mtp"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Batch sizes a projector needs
// ---------------------------------------------------------------------------

/// llama-server's own `--batch-size` default: what a row that leaves
/// `batch_size` unset runs with.
pub(crate) const LLAMA_DEFAULT_BATCH: i64 = 2048;

/// llama-server's own `--ubatch-size` default: what a row that leaves
/// `ubatch_size` unset runs with.
pub(crate) const LLAMA_DEFAULT_UBATCH: i64 = 512;

/// The physical batch (`--ubatch-size`) a Gemma 4 projector needs, when
/// llama.cpp decodes its images with non-causal attention (see
/// [`image_attention`]).
///
/// Non-causal decoding has to put a whole image through one physical batch.
/// mtmd hands the text model an image in pieces of at most `n_batch` tokens,
/// and llama.cpp clamps `n_ubatch` to `n_batch`. A piece larger than
/// `n_ubatch` does not fail the request: it aborts llama-server with
/// `GGML_ASSERT(... "non-causal attention requires n_ubatch >= n_tokens")`,
/// and every request in flight on that container dies with it. Causal
/// decoding splits an image across physical batches like any prompt, so it
/// needs nothing from this.
///
/// The right number is the projector's largest image in tokens, and for
/// Gemma 4 the mmproj header does not say. llama.cpp reads a
/// `clip.vision.image_max_pixels` key when one is there, and none of the
/// sixteen projectors checked on 2026-09-24 carries it (Gemma 4, Qwen2-VL,
/// Qwen3-VL/3.5/3.6/3.8, Pixtral, Muse-Glimmer). The `clip.vision.image_size`
/// they do carry is only the nominal input size: 224 on Gemma 4, whose images
/// come out far larger, because its preprocessing sizes each image to its own
/// aspect ratio. The real ceiling is a per-projector default in llama.cpp's
/// clip code, which `--image-max-tokens` can override.
///
/// So this value is measured, on Gemma 4, where the crash was found. Its image
/// tokens stop growing at about 1100: an A4 page came to about 1053 at 150 dpi
/// and about 1094 at 300 dpi (3508x2482), and Gemma 4's own largest
/// image-token budget is 1120. 1280 clears that with a margin of 160, because
/// the final count comes from llama.cpp's resize code, not Gemma's reference.
/// It is kept that small on purpose: every ubatch token costs compute buffer,
/// and 2048 ran gemma4-26b-a4b (256k context plus its MTP drafter) out of a
/// 24 GB card by about 1.4 GB. It is also below llama.cpp's default batch
/// size of 2048, so a row that leaves `batch_size` unset needs no second
/// change.
pub(crate) const GEMMA4_IMAGE_UBATCH: i64 = 1280;

/// Gemma 3's default pooling factor per side, used when its header carries no
/// `clip.vision.projector.scale_factor`. Read out of llama.cpp's
/// `clip_model_loader::load_hparams` (the gemma3 case stores 4 before the
/// optional key is read).
const GEMMA3_DEFAULT_SCALE_FACTOR: u64 = 4;

/// The value of a llama-server flag in a row's freeform args, under any of
/// its spellings (`--ubatch-size 2048`, `--ubatch-size=2048`, `-ub 2048`).
/// The last one wins, because llama-server's own parser lets a later
/// occurrence overwrite an earlier one.
///
/// `pub(crate)` so `gate::count::image_token_bound` can read a row's own
/// `--image-max-tokens` the same way this module reads every other freeform
/// flag, rather than duplicating the argv scan.
pub(crate) fn arg_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    let mut found = None;
    let mut it = args.iter().peekable();
    while let Some(tok) = it.next() {
        if !crate::runtime::argv::is_opt(tok) {
            continue;
        }
        let key = tok.trim_start_matches('-');
        match key.split_once('=') {
            Some((k, v)) if names.contains(&k) => found = Some(v),
            None if names.contains(&key) => {
                if let Some(v) = it.next_if(|v| !crate::runtime::argv::is_opt(v)) {
                    found = Some(v.as_str());
                }
            }
            _ => {}
        }
    }
    found
}

/// Whether a chat row hands llama-server a projector. `mmproj_path` wins over
/// `no_mmproj` in the renderer; with neither field set, a `--mmproj` in the
/// freeform args passes through and loads one just the same.
pub(crate) fn row_loads_projector(params: &LlamaParams, args: &[String]) -> bool {
    if params.mmproj_path.as_deref().is_some_and(|p| !p.is_empty()) {
        return true;
    }
    !params.no_mmproj && arg_value(args, &["mmproj", "mm"]).is_some_and(|p| !p.is_empty())
}

/// The value a chat row hands llama-server for `batch_size` or `ubatch_size`:
/// the dedicated field when set, else the same flag in the freeform args
/// (which the renderer passes through only while the field is unset). `None`
/// when neither is set and llama-server's own default applies.
pub(crate) fn row_batch_field(params: &LlamaParams, args: &[String], field: &str) -> Option<i64> {
    let (stored, names): (Option<i64>, &[&str]) = match field {
        "batch_size" => (params.batch_size, &["batch-size", "b"]),
        "ubatch_size" => (params.ubatch_size, &["ubatch-size", "ub"]),
        _ => return None,
    };
    stored.or_else(|| arg_value(args, names).and_then(|v| v.parse::<i64>().ok()))
}

/// The `(batch, ubatch)` a chat row runs with, llama-server's defaults
/// filling in whatever the row leaves unset.
pub(crate) fn row_batches(params: &LlamaParams, args: &[String]) -> (i64, i64) {
    (
        row_batch_field(params, args, "batch_size").unwrap_or(LLAMA_DEFAULT_BATCH),
        row_batch_field(params, args, "ubatch_size").unwrap_or(LLAMA_DEFAULT_UBATCH),
    )
}

/// Projector types llama.cpp always decodes with non-causal attention: what
/// `mtmd_decode_use_non_causal` returns true for, named as the mmproj headers
/// spell them. Read out of `libmtmd` in both llama.cpp images on this box
/// (localhost/llama-server-cuda:official-latest and
/// ghcr.io/ggml-org/llama.cpp:server, 2026-09-24): enum values 9, 14 and 41,
/// mapped to names through the build's own projector-name table.
const NON_CAUSAL_PROJECTORS: &[&str] = &["gemma3", "gemma4uv", "deepseek4v"];

/// `gemma4v` is the one conditional case. The same function decodes it
/// causally when the text model's input embedding width is one of these
/// (Gemma 4 E2B and E4B), and non-causally otherwise (26B-A4B at 2816, 31B at
/// 5376).
const GEMMA4V_CAUSAL_TEXT_WIDTHS: &[u64] = &[1536, 2560];

/// Every other projector type in that build's name table, all decoded
/// causally. Kept so a type newer than this list reads as "unknown" rather
/// than as "causal": a type lmgw has never seen gets the cautious answer.
const CAUSAL_PROJECTORS: &[&str] = &[
    "mlp",
    "ldp",
    "ldpv2",
    "resampler",
    "adapter",
    "qwen2vl_merger",
    "qwen3vl_merger",
    "step3vl",
    "gemma3nv",
    "gemma3na",
    "gemma4a",
    "gemma4ua",
    "phi4",
    "idefics3",
    "pixtral",
    "qwen2.5vl_merger",
    "ultravox",
    "internvl",
    "llama4",
    "qwen2a",
    "qwen3a",
    "glma",
    "qwen2.5o",
    "voxtral",
    "meralion",
    "musicflamingo",
    "lfm2",
    "kimivl",
    "paddleocr",
    "lightonocr",
    "cogvlm",
    "janus_pro",
    "dots_ocr",
    "dots3note_v",
    "dots3note_a",
    "deepseekocr",
    "deepseekocr2",
    "lfm2a",
    "glm4v",
    "youtuvl",
    "yasa2",
    "kimik25",
    "nemotron_v2_vl",
    "hunyuanvl",
    "parakeet",
    "exaone4_5",
    "minicpmv4_6",
    "granite_speech",
    "mimovl",
    "minimax_m3",
    "granite4_vision",
    "mimo_audio",
    "qwen3tts_spkenc",
    "qwen3tts_gen",
    "pockettts_spkenc",
    "pockettts_gen",
    "muse-glimmer",
];

/// How many tokens a non-causal projector's largest image comes to, and how
/// lmgw knows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ImageTokens {
    /// Every image is resized to one fixed square, so every image is exactly
    /// this many tokens, computed from the header (Gemma 3).
    Fixed(i64),
    /// The count varies with the image; this is its measured ceiling.
    Measured(i64),
    /// The count varies with the image, and no ceiling has been measured for
    /// this type.
    Unmeasured,
}

/// How llama.cpp decodes a projector's images, as far as lmgw can tell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ImageAttention {
    /// A whole image has to fit one physical batch.
    NonCausal { ty: String, tokens: ImageTokens },
    /// Images split across physical batches like any prompt.
    Causal,
    /// Could not tell; the text says why.
    Unknown(String),
}

/// The image-token count of a non-causal projector type.
///
/// Gemma 3 goes through llama.cpp's fixed-size preprocessor, which resizes
/// every image to `image_size` x `image_size`; its token count is then
/// `(image_size / patch_size)^2 / scale_factor^2`. Both read out of
/// `libmtmd` (`mtmd_context::init_vision` picks
/// `mtmd_image_preprocessor_fixed_size` for gemma3, and `clip_n_output_tokens`
/// divides the patch grid by the squared scale factor for it). Gemma 3's own
/// header (896 px, 14 px patches, factor 4) gives 256.
fn image_tokens(ty: &str, p: &ModelSummary) -> ImageTokens {
    match ty {
        "gemma3" => match (p.vision_image_size, p.vision_patch_size) {
            (Some(size), Some(patch)) if patch > 0 => {
                let side = size / patch;
                let scale = p
                    .vision_scale_factor
                    .unwrap_or(GEMMA3_DEFAULT_SCALE_FACTOR)
                    .max(1);
                ImageTokens::Fixed(i64::try_from(side * side / (scale * scale)).unwrap_or(i64::MAX))
            }
            // llama.cpp cannot load a projector without these either, so
            // this is a broken file; say what is known rather than invent a
            // size.
            _ => ImageTokens::Unmeasured,
        },
        "gemma4v" | "gemma4uv" => ImageTokens::Measured(GEMMA4_IMAGE_UBATCH),
        _ => ImageTokens::Unmeasured,
    }
}

/// Whether llama.cpp decodes this projector's images with non-causal
/// attention. `projector` is the mmproj header (`None` when it could not be
/// read). `text_width` is the weights' `embedding_length`, which only the
/// conditional `gemma4v` case needs; when it is unknown, the projector's own
/// `projection_dim` stands in, because llama.cpp refuses to pair the two when
/// they differ.
pub(crate) fn image_attention(
    projector: Option<&ModelSummary>,
    text_width: Option<u64>,
) -> ImageAttention {
    let Some(p) = projector else {
        return ImageAttention::Unknown("its projector's header could not be read".into());
    };
    // No vision encoder means no images, and no audio type is on the
    // non-causal list.
    if p.has_vision_encoder == Some(false) {
        return ImageAttention::Causal;
    }
    let Some(ty) = p.projector_type.as_deref() else {
        return ImageAttention::Unknown("its projector's header names no projector type".into());
    };
    let non_causal = || ImageAttention::NonCausal {
        ty: ty.to_string(),
        tokens: image_tokens(ty, p),
    };
    if NON_CAUSAL_PROJECTORS.contains(&ty) {
        return non_causal();
    }
    if ty == "gemma4v" {
        return match text_width.or(p.vision_projection_dim) {
            Some(w) if GEMMA4V_CAUSAL_TEXT_WIDTHS.contains(&w) => ImageAttention::Causal,
            Some(_) => non_causal(),
            None => ImageAttention::Unknown(
                "its projector is gemma4v, which llama.cpp decodes non-causally unless the \
                 text model is 1536 or 2560 wide, and neither header gives the width"
                    .into(),
            ),
        };
    }
    if CAUSAL_PROJECTORS.contains(&ty) {
        return ImageAttention::Causal;
    }
    ImageAttention::Unknown(format!("its projector type '{ty}' is not one lmgw knows"))
}

/// The projector a chat row loads, relative to the models dir: `mmproj_path`,
/// else a `--mmproj /models/…` still in the extra args. `None` for a path
/// outside the models mount, whose host location lmgw cannot know.
fn row_projector_rel(params: &LlamaParams, args: &[String]) -> Option<String> {
    if let Some(p) = params.mmproj_path.as_deref().filter(|p| !p.is_empty()) {
        return Some(p.to_string());
    }
    arg_value(args, &["mmproj", "mm"])
        .and_then(|v| v.strip_prefix("/models/"))
        .map(str::to_string)
}

/// [`image_attention`] for a configured row: reads the projector it loads
/// out of `models_dir`. `text_width` as for [`image_attention`].
///
/// `None` when the projector file is not there: the missing file is the
/// problem to report, and a batch-size advisory about a projector that
/// cannot load would only bury it.
pub(crate) async fn row_image_attention(
    models_dir: &str,
    params: &LlamaParams,
    args: &[String],
    text_width: Option<u64>,
) -> Option<ImageAttention> {
    let Some(rel) = row_projector_rel(params, args) else {
        return Some(ImageAttention::Unknown(
            "its projector is outside the models dir, so lmgw cannot read its header".into(),
        ));
    };
    if models_dir.trim().is_empty() {
        return Some(ImageAttention::Unknown(
            "no models dir is configured to read its projector from".into(),
        ));
    }
    let path = std::path::Path::new(models_dir).join(rel);
    if !path.is_file() {
        return None;
    }
    let summary = tokio::task::spawn_blocking(move || gguf::summarize(&path))
        .await
        .ok()
        .and_then(Result::ok);
    Some(image_attention(summary.as_ref(), text_width))
}

/// The physical batch a row's projector needs, and a sentence saying where
/// the number comes from. `None` when it needs nothing (causal decoding).
///
/// A variable-size projector's ceiling is raised by the row's own
/// `--image-max-tokens` when that is larger: an image budget the owner raised
/// is a stated ceiling, and a measured constant must not undercut it. A
/// fixed-size projector ignores that flag, so it does not count there.
pub(crate) fn projector_ubatch_floor(
    attention: &ImageAttention,
    args: &[String],
) -> Option<(i64, String)> {
    let budget = arg_value(args, &["image-max-tokens"]).and_then(|v| v.parse::<i64>().ok());
    let (tokens, basis) = match attention {
        ImageAttention::Causal => return None,
        ImageAttention::NonCausal {
            ty,
            tokens: ImageTokens::Fixed(n),
        } => {
            return Some((
                *n,
                format!(
                    "every {ty} image is resized to one fixed size and comes to exactly {n} \
                     tokens"
                ),
            ))
        }
        ImageAttention::NonCausal {
            tokens: ImageTokens::Measured(n),
            ..
        } => (
            *n,
            "Gemma 4 images stop at about 1100 tokens (its largest budget is 1120), and this \
             adds a margin"
                .to_string(),
        ),
        ImageAttention::NonCausal {
            ty,
            tokens: ImageTokens::Unmeasured,
        } => (
            GEMMA4_IMAGE_UBATCH,
            format!(
                "no image size has been measured for {ty}, so this is Gemma 4's measured \
                 ceiling plus a margin, until one is"
            ),
        ),
        ImageAttention::Unknown(_) => (
            GEMMA4_IMAGE_UBATCH,
            "with the projector unknown, this is Gemma 4's measured ceiling plus a margin"
                .to_string(),
        ),
    };
    Some(match budget {
        Some(b) if b > tokens => (b, format!("the row's own --image-max-tokens is {b}")),
        _ => (tokens, basis),
    })
}

/// The advisory for a row whose projector can take down llama-server, or
/// quietly degrade its images: it loads a projector whose images are decoded
/// non-causally (or lmgw could not tell), and the batch sizes in force are
/// below [`projector_ubatch_floor`]. `None` when there is nothing to say.
///
/// An advisory, not a problem. The model loads and serves text and small
/// images fine; only an image larger than the batch is affected.
///
/// mtmd hands the text model an image in pieces of at most `n_batch` tokens,
/// and `n_ubatch` is clamped to `n_batch`. So there are two ways to be short:
/// a ubatch below the batch aborts on any piece larger than the ubatch; a
/// batch too small (with the ubatch at or above it) never aborts, but splits
/// a large image across separate decodes, so its tokens stop attending to
/// each other across the split.
pub(crate) fn projector_ubatch_advisory(
    params: &LlamaParams,
    args: &[String],
    attention: &ImageAttention,
) -> Option<String> {
    if !row_loads_projector(params, args) {
        return None;
    }
    let (need, basis) = projector_ubatch_floor(attention, args)?;
    let (batch, ubatch) = row_batches(params, args);
    if ubatch.min(batch) >= need {
        return None;
    }
    let lead = match attention {
        ImageAttention::NonCausal { ty, .. } => format!(
            "llama.cpp decodes this model's images ({ty} projector) with non-causal attention, \
             so a whole image has to fit in one physical batch"
        ),
        ImageAttention::Unknown(why) => format!(
            "this model loads a projector, and lmgw could not tell whether llama.cpp decodes \
             its images with non-causal attention: {why}. If it does, a whole image has to \
             fit in one physical batch"
        ),
        ImageAttention::Causal => return None,
    };
    let ubatch_set = row_batch_field(params, args, "ubatch_size").is_some();
    let effect = if ubatch < batch {
        let now = if ubatch_set {
            format!("ubatch_size is {ubatch}")
        } else {
            format!("ubatch_size is unset, so llama.cpp's default of {ubatch} applies")
        };
        format!(
            "{now}: an image of more than {ubatch} tokens aborts llama-server, and every \
             request in flight on it, with GGML_ASSERT \"non-causal attention requires \
             n_ubatch >= n_tokens\""
        )
    } else {
        format!(
            "batch_size is {batch}: an image of more than {batch} tokens is split across \
             separate decodes. Nothing aborts, but the image's tokens stop attending to each \
             other across the split, which quietly degrades what the model sees"
        )
    };
    let short: Vec<&str> = [("ubatch_size", ubatch), ("batch_size", batch)]
        .into_iter()
        .filter(|(_, v)| *v < need)
        .map(|(f, _)| f)
        .collect();
    let fix = format!("Set {} to at least {need} ({basis}).", short.join(" and "));
    Some(format!(
        "{lead}. {effect}. {fix} The model still starts; this only affects a large image."
    ))
}

/// A local model id from a GGUF filename: the base name with the split-part
/// suffix and the quantization label removed, lowercased.
///
/// `Muse-Glimmer-30B-UD-Q4_K_XL.gguf` → `muse-glimmer-30b`. The quant is
/// dropped because it is a property of the file, not of the model an alias
/// points at — and because re-quantizing later should not force every client
/// to change the name it requests.
fn suggest_model_id(file: &str) -> String {
    let stem = crate::hf::suggest_model_id(file);
    let up = stem.to_ascii_uppercase();
    let cut = [
        "-UD-", "-Q4", "-Q5", "-Q6", "-Q8", "-Q2", "-Q3", "-IQ", "-BF16", "-F16", "-F32",
    ]
    .iter()
    .filter_map(|m| up.rfind(m))
    .min()
    .unwrap_or(stem.len());
    stem[..cut].to_ascii_lowercase()
}

/// A companion GGUF found beside the weights: its path relative to the models
/// dir, and what its header says it is.
type Companion = (String, ModelSummary);

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every weights file in one class's models dir, with whether a model row
/// already uses it.
///
/// An agent driving this gateway has no shell. Without this it cannot discover
/// what is on disk, and the paths it would have to guess are exactly the ones
/// `gguf_path` / `mmproj_path` / `draft_gguf_path` expect. `target` picks the
/// directory: the four classes keep separate trees, and an embedding model
/// downloaded with `target=aux` is invisible from the chat listing — which is
/// how one agent ended up hard-linking GGUFs across the two.
///
/// It also picks the **file kinds**: GGUF for the three llama.cpp-shaped
/// classes, and every accepted image kind (`.gguf`, `.safetensors`, `.ckpt`,
/// `.pt`, `.pth`) for `target=image`, whose VAEs and text encoders are exactly
/// the files a GGUF-only scan cannot see (§7.1). The tool keeps its name: an
/// agent that already knows `lmgw__gguf_files` should not have to learn a
/// second one, and the description says what the image target returns.
pub async fn gguf_files(
    state: &SharedState,
    search: Option<&str>,
    target: Option<&str>,
) -> Result<Value, String> {
    let class = class_of(target)?;
    let snap = state.snapshot();
    let dir = models_dir_of(&snap, class);
    if dir.trim().is_empty() {
        return Err(format!(
            "the {} models directory is not configured (Settings → {})",
            class.as_str(),
            settings_page(class)
        ));
    }
    let needle = search.map(str::to_ascii_lowercase);
    let scan = {
        let dir = dir.clone();
        let t = class.as_str().to_string();
        tokio::task::spawn_blocking(move || crate::hf::scan_model_files(&dir, &t))
            .await
            .map_err(|e| format!("scan panicked: {e}"))?
    };

    // Which configured rows name a file, per class: chat rows through any of
    // their three path fields, aux rows through the weights path or a
    // `--mmproj` in their extra args, audio rows through their model root.
    let used_by = |p: &str| -> Vec<String> {
        match class {
            Class::Chat => snap
                .local_models
                .iter()
                .filter(|m| {
                    m.gguf_path == p
                        || m.params.mmproj_path.as_deref() == Some(p)
                        || m.params.draft_gguf_path.as_deref() == Some(p)
                })
                .map(|m| m.model_id.clone())
                .collect(),
            Class::Aux => snap
                .aux_models
                .iter()
                .filter(|m| {
                    m.gguf_path == p
                        || m.args
                            .iter()
                            .any(|a| a.trim_start_matches("/models/").trim_start_matches('/') == p)
                })
                .map(|m| m.model_id.clone())
                .collect(),
            Class::Audio => snap
                .audio_models
                .iter()
                .filter(|m| p.starts_with(m.path.trim_end_matches('/')))
                .map(|m| m.model_id.clone())
                .collect(),
            // An image row names its files in a map rather than in columns,
            // so this is a lookup across that map's values — a shared VAE or
            // text encoder legitimately appears in several rows — plus a
            // prefix match for the keys that name a directory.
            Class::Image => snap
                .image_models
                .iter()
                .filter(|m| {
                    m.files.iter().any(|(key, v)| {
                        let Some(v) = v.as_str().map(|v| v.trim_start_matches('/')) else {
                            return false;
                        };
                        v == p
                            || (crate::runtime::image::is_dir_key(key)
                                && p.starts_with(v.trim_end_matches('/')))
                    })
                })
                .map(|m| m.model_id.clone())
                .collect(),
        }
    };

    let files: Vec<Value> = scan
        .into_iter()
        .filter(|p| {
            needle
                .as_deref()
                .is_none_or(|n| p.to_ascii_lowercase().contains(n))
        })
        .map(|p| {
            let size = std::fs::metadata(std::path::Path::new(&dir).join(&p))
                .map(|m| m.len())
                .unwrap_or(0);
            json!({
                "path": p,
                "size_bytes": size,
                "size": crate::hf::fmt_bytes(size),
                "used_by": used_by(&p),
                // A filename-based guess, so the caller can pick the weights
                // without inspecting every file. lmgw__model_inspect reads the
                // GGUF header and is authoritative where this disagrees.
                "role_guess": crate::ops::classify_repo_file(&p, class.as_str()),
            })
        })
        .collect();

    let t = class.as_str();
    Ok(json!({
        "target": t,
        "models_dir": dir,
        "count": files.len(),
        "files": files,
        "kinds": crate::hf::accepted_extensions(t),
        "note": format!(
            "Paths are relative to the {t} models_dir and are exactly what gguf_path / \
             mmproj_path / draft_gguf_path (chat) or gguf_path (aux) take, or an image row's \
             files values (image), together with target={t}. role_guess is from the \
             filename; for chat and aux lmgw__model_inspect reads the header and is \
             authoritative, and for image there is no header to read — the image roles \
             (diffusion, checkpoint, vae, text_encoder, lora, upscaler) are heuristics the \
             editor lets you override. target=image lists every kind sd-server loads \
             ({}), not only GGUF, because a pipeline's VAE and text encoders are \
             .safetensors. Each class has its own directory: pass target=chat, target=aux \
             (embedding / rerank models), target=audio or target=image to list the others. \
             lmgw cannot see files outside these directories — download into the right one \
             with lmgw__hf_add target=…, or change the directory in the dashboard \
             (Settings → {}).",
            crate::hf::accepted_extensions_phrase(t),
            settings_page(class)
        ),
    }))
}

/// Ask llama-server whether it recognises a GGUF's architecture, in a
/// throwaway container (§3.6, "probe / help").
///
/// The probe loads the model with a minimal context and watches for the
/// architecture check. The classification rests on llama.cpp's ordering:
/// `llama_model_load` validates `general.architecture` against its built-in
/// table *before* reading any tensor data, and rejects an unknown one within
/// about a second. So "the timeout elapsed without an architecture error"
/// reliably means the architecture was accepted — we do not have to wait for a
/// 16GB file to finish loading to learn what we came for.
///
/// It runs `podman run --rm` against the *image* rather than `podman exec`
/// into a shared container, because there is no shared container any more:
/// with per-model containers the normal state is that nothing is running, and
/// a question about what an image supports must not depend on a model being
/// up. The image is the one this GGUF's model would run (its per-model
/// override, else the chat class default), so the answer describes the build
/// that will actually load it.
///
/// `CommandRunner::run` waits for the child to exit, so the timeout below does
/// not by itself stop anything: `tokio::time::timeout` abandons the future but
/// the container keeps running. That is why the probe container is named and
/// removed by name afterwards.
async fn probe_runtime(state: &SharedState, rel: &str, class: Class) -> Value {
    /// How long to wait before concluding the architecture was accepted.
    ///
    /// Note which path this bounds. A *rejected* architecture returns as soon
    /// as llama-server exits — measured at ~0.6s against a 16GB file, since
    /// the check happens on the header. A *accepted* one never exits (it goes
    /// on to serve), so success always costs the full budget. That makes this
    /// the latency of the common case, not of the error case, and it has to
    /// stay well inside the 30–60s timeout MCP clients typically impose.
    /// Ten seconds is an order of magnitude above the observed check time.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let snap = state.snapshot();
    // Reading a file's metadata is a read; spawning a process that loads that
    // file into memory is not. `lmgw__local_model_test` is gated as a
    // mutation for exactly this reason, and the probe does a smaller version
    // of the same thing — so at read_only it declines rather than doing it.
    if !snap.settings.self_admin.allows_write() {
        return json!({
            "checked": false,
            "reason": "the runtime probe loads the model into a container, so it \
                       needs self-admin 'full'; the GGUF metadata above needs no probe",
        });
    }
    // The owner switched the GPU hold on: the card is in use elsewhere, and
    // nothing lmgw starts should compete with it — deferred, and said so. A
    // benchmark run is the same (benchmark design §3.2): a probe container
    // beside it would skew what it measures.
    match snap.gpu_block() {
        None => {}
        Some(crate::bench::lease::GpuBlock::Hold) => {
            return json!({
                "checked": false,
                "reason": "the GPU hold is on, so the runtime probe (a container of the model's \
                           image) is deferred until it is released; the GGUF metadata above \
                           needs no probe",
            })
        }
        Some(crate::bench::lease::GpuBlock::Benchmark(l)) => {
            return json!({
                "checked": false,
                "reason": format!(
                    "benchmark run {} has the GPU to itself, so the runtime probe (a container \
                     of the model's image) is deferred until it ends; the GGUF metadata above \
                     needs no probe",
                    l.run_id
                ),
            })
        }
    }
    let models_dir = models_dir_of(&snap, class);
    if models_dir.trim().is_empty() {
        return json!({
            "checked": false,
            "reason": format!(
                "the {} models dir is not configured, so there is nothing to mount into the \
                 probe container (Settings → {})",
                class.as_str(),
                settings_page(class)
            ),
        });
    }
    let (image, class_run_args) = runtime_for_gguf(&snap, rel, class);
    let name = format!(
        "{}-probe-{}",
        snap.settings.container_prefix,
        crate::runtime::hash6(rel)
    );
    let run_args = probe_run_args(&class_run_args, &models_dir);
    let args = probe_args(rel);
    let registry = state.runtime();

    let run = async {
        // Where the binary is, the way the `--help` read finds it
        // (`Registry::llama_server_candidates`): the image's own entrypoint
        // first when it *is* llama-server — ik_llama.cpp's `/llama-server`,
        // on no `PATH` — then the known places. An image that is not on the
        // box has no facts; it gets the known places, and `--pull=never`
        // fails fast on the first.
        let entrypoint = registry
            .image_facts(&image)
            .await
            .ok()
            .and_then(|f| f.entrypoint);
        for bin in crate::runtime::registry::llama_server_candidates(entrypoint.as_deref()) {
            match registry
                .run_throwaway(&name, &image, &bin, &run_args, &args)
                .await
            {
                Ok(out) => {
                    let text = format!("{}\n{}", out.stdout, out.stderr);
                    // An immediate "no such binary" is worth retrying at the
                    // next path; anything else is a real answer.
                    if text.contains("executable file") && text.contains("not found") {
                        continue;
                    }
                    return Some((text, out.status));
                }
                // podman itself could not be run: no other path does better.
                Err(_) => return None,
            }
        }
        None
    };

    let verdict = match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        // Timed out: the arch check would have fired by now, so it passed.
        Err(_) => json!({
            "checked": true,
            "supported": true,
            "detail": "architecture accepted (loading was still in progress when \
                       the probe was cut short, which is past the check)",
        }),
        Ok(None) => json!({
            "checked": false,
            "reason": format!(
                "could not run llama-server from image '{image}' — is that image pulled? \
                 the probe never pulls one, since a read call must not turn into a \
                 multi-gigabyte download"
            ),
        }),
        Ok(Some((text, status))) => classify_exit(&text, status),
    };

    // Best-effort cleanup of a probe that outlived the timeout. `--rm` covers
    // every run that exits on its own; this covers the one that did not.
    if let Err(e) = registry.rm_force(&name).await {
        tracing::warn!("could not clean up the arch probe container '{name}': {e}");
    }
    verdict
}

/// The image a probe of `rel` should run and the `podman run` args it runs
/// with: the configured model that serves this GGUF decides (its per-model
/// overrides, §3.1), and anything not yet configured falls back to the class
/// defaults — which is what it would get the moment it is added.
fn runtime_for_gguf(snap: &Snapshot, rel: &str, class: Class) -> (String, Vec<String>) {
    let pick = |image: Option<&String>,
                args: Option<&Vec<String>>,
                class_image: &String,
                class_args: &Vec<String>| {
        (
            image.unwrap_or(class_image).clone(),
            args.unwrap_or(class_args).clone(),
        )
    };
    match class {
        Class::Aux => {
            let m = snap.aux_models.iter().find(|m| m.gguf_path == rel);
            let c = &snap.settings.aux_router;
            pick(
                m.and_then(|m| m.image.as_ref()),
                m.and_then(|m| m.extra_run_args.as_ref()),
                &c.image,
                &c.extra_run_args,
            )
        }
        Class::Image => {
            let m = snap.image_models.iter().find(|m| {
                m.files
                    .values()
                    .filter_map(|v| v.as_str())
                    .any(|v| v.trim_start_matches('/') == rel)
            });
            let c = &snap.settings.image;
            pick(
                m.and_then(|m| m.image.as_ref()),
                m.and_then(|m| m.extra_run_args.as_ref()),
                &c.image,
                &c.extra_run_args,
            )
        }
        Class::Chat | Class::Audio => {
            let m = snap.local_models.iter().find(|m| m.gguf_path == rel);
            let c = &snap.settings.router;
            pick(
                m.and_then(|m| m.image.as_ref()),
                m.and_then(|m| m.extra_run_args.as_ref()),
                &c.image,
                &c.extra_run_args,
            )
        }
    }
}

/// The probe container's `podman run` args: the model's own resolved run
/// args (its override, else the class's), the GPUs hidden, then the models
/// dir.
///
/// The run args are not decoration. A build without `GGML_BACKEND_DL` —
/// ik_llama.cpp's, any hand-built one — links `libcuda.so.1` directly and
/// exits 127 before it reads a single flag unless the GPU device is attached,
/// which is what those args do (they mount the GPU libraries); the probe
/// would then report every model as unloadable. It is the same fix the
/// `--help` read took (container-builds §2.2, WP1).
///
/// But no device is *visible* to it ([`HIDE_GPUS`]): `--n-gpu-layers 0`
/// keeps the weights off the card, not the CUDA context loading a model
/// creates with a device in sight — 390 MiB for the probe's whole run
/// (measured 2026-09-26), unseen by the VRAM ledger and deaf to the GPU
/// hold. With the device hidden the architecture check runs the same.
/// `run_throwaway` also drops a `-d`/`--restart`/`--publish`/`--name` the
/// class args might carry.
///
/// [`HIDE_GPUS`]: crate::runtime::registry::HIDE_GPUS
fn probe_run_args(class_run_args: &[String], models_dir: &str) -> Vec<String> {
    let mut out = crate::runtime::registry::without_gpus(class_run_args);
    out.extend(probe_mounts(models_dir));
    out
}

/// The probe container's mounts: the models dir read-only at `/models`, with
/// the same SELinux posture the class defaults take (§6 — unlabeled mounts
/// plus `label=disable`, never a relabeling `:Z` on a directory this size).
fn probe_mounts(models_dir: &str) -> Vec<String> {
    vec![
        "-v".into(),
        format!("{}:/models:ro", models_dir.trim_end_matches('/')),
        "--security-opt".into(),
        "label=disable".into(),
    ]
}

/// llama-server's arguments for an architecture probe (pure, so the flags
/// below are covered by a test rather than by running a container).
///
/// `--n-gpu-layers 0` keeps it on the CPU: the architecture check happens on
/// the header, long before anything would be offloaded. (The container still
/// gets the class's GPU run args — see [`probe_run_args`] — because some
/// builds cannot even start without the device.)
///
/// `--host`/`--port` are load-bearing and easy to mistake for noise:
/// llama-server binds the HTTP socket *before* loading the model, and a failure
/// there would kill the probe in 0.2s — before the architecture check it exists
/// to perform — reporting every model as unloadable. Port 0 lets the kernel
/// choose, so there is nothing to collide with even if something else in the
/// container is listening.
fn probe_args(rel: &str) -> Vec<String> {
    [
        "--model",
        &format!("/models/{rel}"),
        "--ctx-size",
        "8",
        "--n-gpu-layers",
        "0",
        "--no-warmup",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// [`classify_probe`], for a probe that **exited** on its own with
/// `status`. A crash (killed by a signal: 128+) says nothing by itself — ik's
/// llama-server, with no GPU visible to it, segfaults after loading the model
/// (measured 2026-09-26) — so it counts only when the output shows the
/// architecture was already accepted (the model's metadata was printed, which
/// happens after the check); a crash before that is no answer.
fn classify_exit(text: &str, status: i32) -> Value {
    let verdict = classify_probe(text);
    let crashed = status > 128;
    if !crashed || verdict["supported"] == false {
        return verdict;
    }
    let past_check = text.lines().any(|l| {
        let l = l.trim_start();
        (l.contains("print_meta") || l.contains("print_info")) && l.contains("arch")
    });
    if past_check {
        return json!({
            "checked": true,
            "supported": true,
            "detail": format!(
                "architecture accepted (the probe printed the model's metadata, which comes \
                 after the check, then exited with status {status})"
            ),
        });
    }
    json!({
        "checked": false,
        "reason": format!(
            "the probe exited with status {status} before the architecture check said anything"
        ),
    })
}

/// Turn probe output into a verdict. Split out so it is testable without a
/// container.
fn classify_probe(text: &str) -> Value {
    let line_with = |needle: &str| {
        text.lines()
            .find(|l| l.to_ascii_lowercase().contains(needle))
            .map(|l| l.trim().to_string())
    };
    if let Some(l) = line_with("unknown model architecture") {
        return json!({
            "checked": true, "supported": false, "reason": l,
            "hint": "the llama.cpp build in this container predates the model's \
                     architecture — update the container image",
        });
    }
    if let Some(l) = line_with("failed to load clip model") {
        return json!({
            "checked": true, "supported": false, "reason": l,
            "hint": "this build does not recognise the projector type",
        });
    }
    for needle in ["error loading model", "failed to load model"] {
        if let Some(l) = line_with(needle) {
            return json!({ "checked": true, "supported": false, "reason": l });
        }
    }
    json!({ "checked": true, "supported": true })
}

/// What a GGUF is: architecture, shape, quantization, role, and whether the
/// runtime can load it at all.
pub async fn model_inspect(
    state: &SharedState,
    gguf_path: &str,
    probe: bool,
    target: Option<&str>,
) -> Result<Value, String> {
    let class = class_of(target)?;
    let dir = models_dir_of(&state.snapshot(), class);
    let (rel, full) = resolve(&dir, gguf_path, class)?;
    // Through the shared cache (model capabilities design §3.5) rather than
    // the bare `summarize` helper above: this is the tool-plane path other
    // callers (`/v1/models`, once it lands) will hit for the same files, so
    // a caller re-inspecting the same GGUF twice in a row does not re-walk
    // its header both times.
    let s = state.gguf_cache.summarize_cached(&full).await?;

    let role = role_of(&s);
    let (serve_as, aux_kind) = class_for(&s);
    // Cost the model's own trained context, at both cache precisions — the
    // caller's real question is "does this fit", and the honest answer needs
    // the real context, not a smaller one chosen to look comfortable.
    let kv = |bits: u32| {
        s.context_length
            .and_then(|c| gguf::kv_cache_bytes(&s, c, bits))
    };
    let runtime = if probe {
        probe_runtime(state, &rel, class).await
    } else {
        json!({ "checked": false, "reason": "probe not requested" })
    };

    Ok(json!({
        "path": rel,
        "target": class.as_str(),
        "role": role,
        // Which class serves this file, read from the header: an embedding
        // or rerank model is an aux model (`lmgw__aux_model_set`), whatever
        // directory it was found in.
        "serve_as": serve_as,
        "aux_kind": aux_kind.map(|k| k.as_str()),
        "pooling_type": s.pooling_type.and_then(gguf::pooling_name),
        "has_classifier_head": s.has_classifier_head,
        "suggested_spec_type": spec_type_for(&s),
        "architecture": s.architecture,
        "name": s.general_name,
        "size_label": s.size_label,
        "quant": s.quant,
        "context_length": s.context_length,
        "block_count": s.block_count,
        "head_count": s.head_count,
        "head_count_kv": s.head_count_kv,
        "head_count_kv_per_layer": s.head_count_kv_per_layer,
        "sliding_window": s.sliding_window,
        "sliding_window_pattern": s.sliding_window_pattern,
        "sliding_window_pattern_is_array": s.sliding_window_pattern_is_array,
        "full_attention_interval": s.full_attention_interval,
        "has_chat_template": s.has_chat_template,
        // Text heuristics over the chat template — reasoning, effort levels,
        // tool-call syntax (model capabilities design §3.1). `null` exactly
        // when there is no chat template to read.
        "signals": s.signals,
        "has_mtp_layers": s.has_mtp_layers,
        "projector_type": s.projector_type,
        "vision_block_count": s.vision_block_count,
        // `clip.has_vision_encoder` / `clip.has_audio_encoder` (design §3.3)
        // — meaningful on mmproj files; `null` when the header says nothing.
        "has_vision_encoder": s.has_vision_encoder,
        "has_audio_encoder": s.has_audio_encoder,
        "file_size_bytes": s.file_size,
        "file_size": crate::hf::fmt_bytes(s.file_size),
        "vram_estimate": {
            "weights_bytes": s.file_size,
            "kv_cache_bytes_at_full_ctx_f16": kv(16),
            "kv_cache_bytes_at_full_ctx_q8_0": kv(8),
            "note": "KV figures are for context_length tokens and exclude \
                     quantization scale overhead, so treat them as a lower \
                     bound. Sliding-window layers are counted at the window \
                     size, and on hybrid linear-attention models only every \
                     full_attention_interval-th layer holds a per-token cache \
                     (the rest charge their fixed recurrent state) — which is \
                     why either kind's KV is far below the naive layers x \
                     context product.",
        },
        "runtime": runtime,
    }))
}

/// Speculative knobs that are inert once `spec_type` is gone, and so have to
/// be cleared with it rather than left behind as confusing dead config.
const SPEC_KNOBS: &[&str] = &[
    "spec_type",
    "spec_draft_n_max",
    "spec_draft_n_min",
    "spec_draft_ngl",
];

/// The delta between a plan and a configured row that already uses the same
/// GGUF, as a patch that can be handed straight to `lmgw__local_model_set`.
///
/// Planning a file that is already configured is a *repair*, and a repair
/// needs the difference from what is running now — not a full parameter set
/// the caller has to eyeball against `lmgw__local_model_get` to find the two
/// fields that matter.
///
/// Only fields the plan actually proposes are compared. The plan forms no
/// opinion on sampling, threads or seed, so those never appear: a diff that
/// listed them would be proposing to revert deliberate tuning. The one
/// exception is speculation the file cannot support, which is unset rather
/// than changed — see below. The ubatch a projector needs is compared as a
/// minimum, so a row running a larger one is not asked to shrink it; `image`
/// is how the plan's projector decodes, which the row's own args can raise.
fn plan_diff(
    row: &crate::config::LocalModel,
    params: &serde_json::Map<String, Value>,
    why: &serde_json::Map<String, Value>,
    weights_have_mtp: bool,
    image: Option<&ImageAttention>,
) -> Value {
    let stored = serde_json::to_value(&row.params).unwrap_or(Value::Null);
    let current = |field: &str| -> Value {
        match field {
            "gguf_path" => json!(row.gguf_path),
            "idle_seconds" => json!(row.idle_seconds),
            _ => stored.get(field).cloned().unwrap_or(Value::Null),
        }
    };

    let mut changes: Vec<Value> = Vec::new();
    let mut patch = serde_json::Map::new();
    let mut change = |field: &str, cur: Value, planned: Value, why: Value| {
        changes.push(json!({
            "field": field,
            "current": cur,
            "planned": planned,
            "why": why,
        }));
        patch.insert(field.to_string(), planned);
    };
    // The value a batch size really has on this row: the field, or the same
    // flag passed through its extra args while the field is unset. `null`
    // means neither, so llama.cpp's own default applies.
    let in_force = |field: &str| -> Value {
        row_batch_field(&row.params, &row.args, field).map_or(Value::Null, |v| json!(v))
    };
    for (field, planned) in params {
        // The ubatch a projector needs is a floor, not a target. A row already
        // running a larger one (in the field, or passed through its extra
        // args) is in sync, and a patch lowering it would undo deliberate
        // tuning. The row's own `--image-max-tokens` can raise the floor. And
        // because mtmd hands an image over in pieces of at most batch_size
        // tokens, a row whose batch is below the floor gets its batch raised
        // too. A fresh plan sets batch_size only when llama.cpp's default
        // falls short, and that case is covered here as well.
        if field == "batch_size" && params.contains_key("ubatch_size") {
            continue;
        }
        if let ("ubatch_size", Some(floor)) = (field.as_str(), planned.as_i64()) {
            let row_floor = image
                .and_then(|a| projector_ubatch_floor(a, &row.args))
                .map_or(floor, |(n, _)| n);
            let floor = floor.max(row_floor);
            let (batch, ubatch) = row_batches(&row.params, &row.args);
            if ubatch < floor {
                change(
                    field,
                    in_force(field),
                    json!(floor),
                    why.get(field).cloned().unwrap_or(Value::Null),
                );
            }
            if batch < floor {
                change(
                    "batch_size",
                    in_force("batch_size"),
                    json!(floor),
                    json!(
                        "at least ubatch_size: mtmd hands an image over in pieces of at most \
                         batch_size tokens and llama.cpp clamps ubatch to batch, so this row's \
                         smaller batch would split a large image or undo the ubatch"
                    ),
                );
            }
            continue;
        }
        let cur = current(field);
        if &cur == planned {
            continue;
        }
        change(
            field,
            cur,
            planned.clone(),
            why.get(field).cloned().unwrap_or(Value::Null),
        );
    }

    // Speculation the files cannot support. This is the one place the plan
    // proposes *removing* configuration, and it is deliberately narrow: a
    // `draft-*` strategy needs either a drafter file or MTP heads in the
    // weights, and a row with neither fails to load every single time. Left
    // alone are the `ngram-*` strategies, which need no drafter at all, and
    // any row naming a drafter file — that path may point outside the repo
    // directory this plan scanned, so its absence here proves nothing.
    let mut unset: Vec<Value> = Vec::new();
    let mut clear: Vec<String> = Vec::new();
    let spec = row.params.spec_type.as_deref().unwrap_or("").trim();
    let unsupported = spec.starts_with("draft-")
        && row.params.draft_gguf_path.is_none()
        && !(spec == "draft-mtp" && weights_have_mtp);
    // If the plan found a real drafter it has already proposed a correct
    // spec_type above; changing it is the fix, not clearing it.
    if unsupported && !params.contains_key("spec_type") {
        for field in SPEC_KNOBS {
            let cur = current(field);
            if cur.is_null() {
                continue;
            }
            unset.push(json!({
                "field": field,
                "current": cur,
                "why": if *field == "spec_type" {
                    format!(
                        "'{spec}' needs MTP layers in the weights or a separate \
                         drafter, and this GGUF has neither — the model cannot load \
                         while it is set"
                    )
                } else {
                    "inert without spec_type".to_string()
                },
            }));
            clear.push((*field).to_string());
        }
    }

    let in_sync = changes.is_empty() && unset.is_empty();
    let mut out = serde_json::Map::new();
    out.insert("model_id".into(), json!(row.model_id));
    out.insert("id".into(), json!(row.id));
    out.insert("enabled".into(), json!(row.enabled));
    out.insert("in_sync".into(), json!(in_sync));
    out.insert("change".into(), json!(changes));
    out.insert("unset".into(), json!(unset));
    if !in_sync {
        patch.insert("action".into(), json!("update"));
        patch.insert("model_id".into(), json!(row.model_id));
        // gguf_path is what matched this row in the first place; repeating it
        // in an update is noise.
        patch.remove("gguf_path");
        if !clear.is_empty() {
            patch.insert("clear".into(), json!(clear.join(",")));
        }
        out.insert("apply".into(), Value::Object(patch));
    }
    Value::Object(out)
}

/// What `lmgw__local_model_plan target=image` answers with (§7.2).
///
/// There is no GGUF header to plan from here — half of a pipeline is
/// `.safetensors` with no metadata lmgw reads, and the diffusion GGUF carries
/// no `general.architecture` this crate understands — so the plan is a
/// **recipe match** instead: the file's `<owner>/<repo>/<file>` path (or, for
/// a hand-placed copy, its bare name) is looked up in the shipped list, and
/// the matching family's whole parameter set comes back with its other
/// components named. That is the same promise the chat plan makes — "here is
/// a ready-to-apply row and why each value is what it is" — served from a
/// curated list rather than from a header.
fn image_plan(models_dir: &str, rel: &str) -> Value {
    let Some((recipe, matched)) = crate::image_recipes::match_path(rel) else {
        return json!({
            "class": "image",
            "planned": false,
            "path": rel,
            "reason": format!(
                "unknown family — '{rel}' matches none of the pipelines lmgw ships a recipe \
                 for, and nothing in the file itself says which flag it belongs under. Set \
                 `files` by hand.",
            ),
            "known_recipes": crate::image_recipes::keys(),
            "next_step": "lmgw__image_recipes lists what is known (each with its components, \
                          repos and sizes); lmgw__image_model_set action=create takes a files \
                          map you write yourself — exactly one of files.model / \
                          files.diffusion_model, plus vae, clip_l, t5xxl, llm as the family \
                          needs. Every key is checked against the image's own --help on save.",
        });
    };

    let mut files = serde_json::Map::new();
    let mut why = serde_json::Map::new();
    let mut missing: Vec<Value> = Vec::new();
    for c in recipe.components {
        // The file the caller named is the file the plan uses, wherever it
        // sits: they asked about *this* copy, and a recipe-default path that
        // happens to exist too would silently plan a different one.
        if std::ptr::eq(c, matched) {
            files.insert(c.role.to_string(), Value::from(rel));
            why.insert(
                c.role.to_string(),
                Value::from(format!(
                    "the file this plan was made for — {} in {}",
                    c.role, recipe.display_name
                )),
            );
            continue;
        }
        // Otherwise: the recipe's default file if it is on disk, else any of
        // its alternatives that is.
        let candidates = std::iter::once(c.file)
            .chain(c.alternatives.iter().map(|a| a.file))
            .filter_map(|f| crate::hf::dest_rel_path(c.repo, f).ok())
            .find(|dest| {
                !models_dir.trim().is_empty()
                    && std::path::Path::new(models_dir).join(dest).is_file()
            });
        match candidates {
            Some(dest) => {
                why.insert(
                    c.role.to_string(),
                    Value::from(format!(
                        "{}'s {} component, found on disk",
                        recipe.key, c.role
                    )),
                );
                files.insert(c.role.to_string(), Value::from(dest));
            }
            None => {
                let dest = c.dest_rel_path();
                missing.push(json!({
                    "role": c.role,
                    "repo": c.repo,
                    "file": c.file,
                    "dest_path": dest,
                    "size_bytes": c.size_bytes,
                    "size": crate::hf::fmt_bytes(c.size_bytes),
                    "gated": c.gated,
                    "note": c.note,
                    "hf_add": format!(
                        "lmgw__hf_add repo={} file={} target=image companions=false",
                        c.repo, c.file
                    ),
                }));
            }
        }
    }

    let args = recipe.args_json();
    for (k, v) in &args {
        why.insert(
            format!("args.{k}"),
            Value::from(match k.as_str() {
                "cfg_scale" => format!(
                    "{v} — {} departs from sd-server's own default of 7.0",
                    recipe.display_name
                ),
                "steps" => format!("{v} — the step count this family is distilled for"),
                "width" | "height" => {
                    format!(
                        "{v} — the resolution this family was trained at (the server \
                             defaults to 512)"
                    )
                }
                "diffusion_fa" => "flash attention in the diffusion model; a switch, so a bare \
                                   flag"
                    .to_string(),
                _ => format!("{v} — from the {} recipe", recipe.key),
            }),
        );
    }
    why.insert(
        "modes".into(),
        Value::from(
            "what this pipeline does; drives task and endpoints on /v1/models before \
                     the container has ever run",
        ),
    );
    why.insert(
        "edit".into(),
        Value::from(if recipe.edit {
            "an edit pipeline: it takes reference images and may serve /v1/images/edits"
        } else {
            "not an edit pipeline — lmgw refuses /v1/images/edits for this row, because \
             sd-server segfaults on a reference-image request it cannot take (§12.8)"
        }),
    );

    let complete = missing.is_empty();
    json!({
        "class": "image",
        "planned": true,
        "path": rel,
        "recipe": recipe.key,
        "display_name": recipe.display_name,
        "description": recipe.description,
        "matched_role": matched.role,
        "complete": complete,
        "params": {
            "action": "create",
            "model_id": recipe.suggested_model_id(),
            "files": files,
            "args": args,
            "modes": recipe.modes,
            "edit": recipe.edit,
        },
        "why": why,
        "missing": missing,
        "vram_note": recipe.vram_note,
        "next_step": if complete {
            "pass params to lmgw__image_model_set action=create, then lmgw__local_model_test \
             model_id=<id> target=image to prove it loads and draws".to_string()
        } else {
            format!(
                "{} component(s) of this pipeline are not on disk — download them (each \
                 entry of 'missing' carries the exact lmgw__hf_add call, or run \
                 lmgw__image_recipe_add key={} to queue them all at once), then plan again",
                missing.len(),
                recipe.key
            )
        },
    })
}

/// A complete, ready-to-apply parameter set for a downloaded GGUF.
///
/// Reads the weights file and everything beside it, then returns the exact
/// field names [`crate::ops::LocalModelPatch`] accepts, plus why each value
/// was chosen. The rationale is not decoration: a caller that disagrees needs
/// to know which values were read out of the file (and are therefore facts)
/// and which are defaults it is free to override.
///
/// When some row already serves the same GGUF, the plan additionally carries
/// the delta from that row under `configured_as` — see [`plan_diff`].
pub async fn local_model_plan(
    state: &SharedState,
    gguf_path: &str,
    probe: bool,
    target: Option<&str>,
) -> Result<Value, String> {
    let class = class_of(target)?;
    let snap = state.snapshot();
    let dir = models_dir_of(&snap, class);
    // The image class has no header to plan from (image-generation design §7):
    // a stable-diffusion.cpp pipeline is a *set* of files — a DiT plus a VAE
    // plus one to three text encoders — spread across repos, half of them
    // `.safetensors` with no GGUF metadata at all. Which flag each one belongs
    // under is a property of the *family*, so the plan matches the file
    // against the shipped recipes instead of reading it. See [`image_plan`].
    let (rel, full) = resolve(&dir, gguf_path, class)?;
    if class == Class::Image {
        return Ok(image_plan(&dir, &rel));
    }
    let s = summarize(full).await?;

    // An embedding or rerank model is an aux model wherever it was found, and
    // anything planned against the aux dir is one by the caller's choice: both
    // get the aux plan, whose parameters are `lmgw__aux_model_set`'s.
    let (serve_as, aux_kind) = class_for(&s);
    if class == Class::Aux || serve_as == "aux" {
        return aux_plan(state, &snap, class, &dir, &rel, &s, aux_kind, probe).await;
    }

    let mut warnings: Vec<String> = Vec::new();
    if role_of(&s) != "weights" {
        warnings.push(format!(
            "{rel} looks like a '{}', not model weights — plan against the \
             weights file instead and pass this one as mmproj_path or \
             draft_gguf_path",
            role_of(&s)
        ));
    }

    // Companions live in the same directory: a repo ships one projector and
    // one drafter shared by every quantization of the weights.
    let parent = rel
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let siblings = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || crate::hf::scan_gguf_files(&dir))
            .await
            .map_err(|e| format!("scan panicked: {e}"))?
    };
    let (mut mmproj, mut drafter): (Option<Companion>, Option<Companion>) = (None, None);
    for cand in siblings {
        if cand == rel {
            continue;
        }
        let cand_parent = cand.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if cand_parent != parent {
            continue;
        }
        let Ok(cs) = summarize(std::path::Path::new(&dir).join(&cand)).await else {
            continue;
        };
        match role_of(&cs) {
            "mmproj" if mmproj.is_none() => mmproj = Some((cand, cs)),
            "drafter" if drafter.is_none() => drafter = Some((cand, cs)),
            _ => {}
        }
    }

    let mut params = serde_json::Map::new();
    let mut why = serde_json::Map::new();
    let mut put = |k: &str, v: Value, reason: String| {
        params.insert(k.to_string(), v);
        why.insert(k.to_string(), Value::String(reason));
    };

    put(
        "gguf_path",
        json!(rel),
        "the file this plan was made for".into(),
    );
    match s.context_length {
        Some(c) => put(
            "ctx_size",
            json!(c),
            format!("the model's trained context ({c}) — its real ceiling, not a reduced default"),
        ),
        None => warnings.push(
            "the GGUF declares no context length; leave ctx_size unset to take llama.cpp's default"
                .into(),
        ),
    }
    put("n_gpu_layers", json!(999), "offload every layer".into());
    put("flash_attn", json!("on"), "reduces attention memory".into());
    for k in ["cache_type_k", "cache_type_v"] {
        put(
            k,
            json!("q8_0"),
            "halves KV cache versus the f16 default, at negligible quality cost".into(),
        );
    }
    if s.has_chat_template {
        put(
            "jinja",
            json!(true),
            "the GGUF embeds a chat template; without --jinja it is ignored and \
             tool calling will not work"
                .into(),
        );
    } else {
        warnings.push(
            "no chat template in the GGUF — set chat_template_file, or the model \
             will only work as a completion endpoint"
                .into(),
        );
    }

    if let Some((path, ms)) = &mmproj {
        put(
            "mmproj_path",
            json!(path),
            format!(
                "sibling projector ({}), so the model can accept images",
                ms.projector_type.as_deref().unwrap_or("vision")
            ),
        );
    }

    // How the projector's images are decoded. It decides whether the plan
    // needs a ubatch floor at all, and it is what `plan_diff` measures
    // configured rows against.
    let image = mmproj
        .as_ref()
        .map(|(_, ms)| image_attention(Some(ms), s.embedding_length));
    let lead = match &image {
        Some(ImageAttention::NonCausal { ty, .. }) => Some(format!(
            "llama.cpp decodes this {ty} projector's images with non-causal attention, so a \
             whole image has to fit in one physical batch"
        )),
        Some(ImageAttention::Unknown(why)) => Some(format!(
            "lmgw could not tell whether llama.cpp decodes this projector's images with \
             non-causal attention ({why}), so this is a precaution: if it does, a whole image \
             has to fit in one physical batch"
        )),
        Some(ImageAttention::Causal) | None => None,
    };
    let floor = image.as_ref().and_then(|a| projector_ubatch_floor(a, &[]));
    // Only when the floor is above llama.cpp's default ubatch. A causal
    // projector splits an image across physical batches, and Gemma 3's fixed
    // 256 tokens fit the default; a larger ubatch would only cost VRAM.
    if let (Some(lead), Some((need, basis))) = (lead, floor) {
        if need > LLAMA_DEFAULT_UBATCH {
            let batch_note = if need > LLAMA_DEFAULT_BATCH {
                String::new()
            } else {
                format!(". The default batch_size ({LLAMA_DEFAULT_BATCH}) already covers it")
            };
            put(
                "ubatch_size",
                json!(need),
                format!(
                    "{lead}, and at llama.cpp's default of {LLAMA_DEFAULT_UBATCH} a larger image \
                     aborts the server with GGML_ASSERT \"non-causal attention requires n_ubatch \
                     >= n_tokens\". The projector's largest image sets the number: {basis}. No \
                     higher, because every ubatch token costs compute buffer in VRAM{batch_note}"
                ),
            );
            if need > LLAMA_DEFAULT_BATCH {
                put(
                    "batch_size",
                    json!(need),
                    format!(
                        "at least ubatch_size: mtmd hands an image over in pieces of at most \
                         batch_size tokens, and llama.cpp's default of {LLAMA_DEFAULT_BATCH} \
                         would split this projector's largest image"
                    ),
                );
            }
        }
    }

    match (&drafter, spec_type_for(&s)) {
        (Some((path, ds)), _) => {
            let st = spec_type_for(ds).unwrap_or("draft-mtp");
            put(
                "draft_gguf_path",
                json!(path),
                "sibling speculative drafter".into(),
            );
            put(
                "spec_type",
                json!(st),
                format!(
                    "dictated by the drafter's architecture ('{}') — a mismatched \
                     spec_type fails at load",
                    ds.architecture.as_deref().unwrap_or("?")
                ),
            );
            // dflash drafters state their own draft block size; 4 is
            // llama.cpp's working default for MTP heads.
            let n_max = if st == "draft-dflash" { 16 } else { 4 };
            put(
                "spec_draft_n_max",
                json!(n_max),
                format!("typical draft window for {st}"),
            );
        }
        // No separate drafter, but the weights carry MTP heads themselves.
        (None, Some("draft-mtp")) if s.has_mtp_layers => {
            put(
                "spec_type",
                json!("draft-mtp"),
                "the weights carry MTP layers, so they can self-draft with no \
                 separate drafter file"
                    .into(),
            );
            put(
                "spec_draft_n_max",
                json!(4),
                "typical MTP draft window".into(),
            );
        }
        _ => {}
    }

    // The live bug this check exists for: four rows in this very gateway set
    // spec_type=draft-mtp against GGUFs with no MTP layers, and every one of
    // them fails to load, every time.
    if params.get("spec_type").and_then(Value::as_str) == Some("draft-mtp")
        && !params.contains_key("draft_gguf_path")
        && !s.has_mtp_layers
    {
        warnings.push(
            "spec_type 'draft-mtp' needs MTP layers in the weights or a separate \
             drafter; this file has neither and would fail to load"
                .into(),
        );
    }

    let runtime = if probe {
        probe_runtime(state, &rel, class).await
    } else {
        json!({ "checked": false, "reason": "probe not requested" })
    };
    if runtime["supported"] == Value::Bool(false) {
        warnings.push(format!(
            "the running llama.cpp build cannot load this model: {}",
            runtime["reason"]
                .as_str()
                .unwrap_or("see lmgw__model_inspect")
        ));
    }
    if mmproj.is_some() && runtime["checked"] == Value::Bool(false) {
        warnings.push(
            "projector support was not verified — run lmgw__local_model_test \
             after applying"
                .into(),
        );
    }

    // Is this file already configured? If so the caller is repairing a model,
    // not adding one, and the delta is what it actually needs.
    let mut configured: Vec<Value> = Vec::new();
    match store::list_local_models(&state.db).await {
        Ok(rows) => {
            for row in rows.iter().filter(|m| m.gguf_path == rel) {
                configured.push(plan_diff(
                    row,
                    &params,
                    &why,
                    s.has_mtp_layers,
                    image.as_ref(),
                ));
            }
        }
        // The parameter set stands on its own; say the comparison is missing
        // rather than let an empty `configured_as` read as "nothing uses it".
        Err(e) => warnings.push(format!(
            "could not check whether a model already uses this GGUF, so \
             configured_as is incomplete: {e}"
        )),
    }
    let repairs: Vec<&Value> = configured
        .iter()
        .filter(|c| c["in_sync"] == Value::Bool(false))
        .collect();

    let next_step = if configured.is_empty() {
        "pass these to lmgw__local_model_set action=create (adding model_id), \
         then lmgw__container target=chat action=apply, then lmgw__local_model_test"
            .to_string()
    } else if repairs.is_empty() {
        "every model already using this GGUF matches this plan; nothing to do".to_string()
    } else {
        format!(
            "this GGUF is already configured — pass the ready-made \
             configured_as[].apply object ({} of them) to lmgw__local_model_set, \
             then lmgw__container target=chat action=apply, then \
             lmgw__local_model_test",
            repairs.len()
        )
    };

    Ok(json!({
        "class": "chat",
        "target": class.as_str(),
        "model_id": suggest_model_id(&rel),
        "params": params,
        "rationale": why,
        "warnings": warnings,
        "runtime": runtime,
        "configured_as": configured,
        "configured_note": "Models already serving this GGUF, and how they differ \
                            from the plan. Values the plan read out of the file \
                            are facts; the rest are defaults a deliberate variant \
                            (a CPU-only twin, a short-context twin) may be \
                            overriding on purpose — so this is a proposal to \
                            review, not a correction to apply blindly. The one \
                            exception is 'unset', which lists configuration the \
                            file provably cannot support.",
        "next_step": next_step,
    }))
}

/// The aux-class plan: what `lmgw__aux_model_set action=create` takes for an
/// embedding or rerank GGUF, read from its header.
///
/// `found_in` is the directory the file was resolved against. An aux row can
/// only address the aux models dir, so a file found in the chat dir gets the
/// parameters *and* a warning that says where the file has to be — the exact
/// trap that had an agent hard-linking GGUFs between the two trees.
#[allow(clippy::too_many_arguments)]
async fn aux_plan(
    state: &SharedState,
    snap: &Snapshot,
    found_in: Class,
    dir: &str,
    rel: &str,
    s: &ModelSummary,
    kind: Option<AuxKind>,
    probe: bool,
) -> Result<Value, String> {
    let mut warnings: Vec<String> = Vec::new();
    let kind = kind.unwrap_or_else(|| {
        warnings.push(format!(
            "{rel} looks like a chat model (no pooling_type or classifier head in its \
             header{}) — served with --embeddings it pools its hidden states, which works \
             but is not what a trained embedder produces; fine only if that is the intent",
            if s.has_chat_template {
                ", and a chat template"
            } else {
                ""
            }
        ));
        AuxKind::Embed
    });

    let mut params = serde_json::Map::new();
    let mut why = serde_json::Map::new();
    let mut put = |k: &str, v: Value, reason: String| {
        params.insert(k.to_string(), v);
        why.insert(k.to_string(), Value::String(reason));
    };
    put(
        "gguf_path",
        json!(rel),
        "the file this plan was made for".into(),
    );
    let pooling = s.pooling_type.and_then(gguf::pooling_name);
    match kind {
        AuxKind::Rerank => put(
            "kind",
            json!("rerank"),
            if s.has_classifier_head {
                "the header carries a classifier head (cls.output), a cross-encoder that \
                 scores query/document pairs — served with --reranking on /v1/rerank"
                    .into()
            } else {
                "the header declares pooling_type 'rank' — served with --reranking on \
                 /v1/rerank; never set pooling on a rerank model"
                    .into()
            },
        ),
        AuxKind::Embed => {
            put(
                "kind",
                json!("embed"),
                match pooling {
                    Some(p) => format!(
                        "the header declares pooling_type '{p}': an encoder with no LM head, \
                         served with --embeddings on /v1/embeddings"
                    ),
                    None => "an encoder architecture, served with --embeddings on \
                             /v1/embeddings"
                        .into(),
                },
            );
            match pooling {
                Some(p) if p != "none" => put(
                    "pooling",
                    json!(p),
                    format!(
                        "the pooling the converter recorded ('{p}'); the model card's vectors \
                         assume it, and a different one retrieves noise while looking fine"
                    ),
                ),
                Some(_) => warnings.push(
                    "the header declares pooling 'none' — leave pooling unset and llama-server \
                     picks the model's default"
                        .into(),
                ),
                None => warnings.push(
                    "the header declares no pooling_type — leave pooling unset unless the \
                     model card names one (mean for most BERT-style encoders)"
                        .into(),
                ),
            }
        }
    }
    match s.context_length {
        Some(c) => put(
            "ctx_size",
            json!(c),
            format!(
                "the model's trained context ({c}) — its real ceiling; inputs longer than \
                 this are truncated, not rejected, so do not size it down"
            ),
        ),
        None => warnings.push(
            "the GGUF declares no context length; leave ctx_size unset to take llama.cpp's \
             default"
                .into(),
        ),
    }

    // A sibling projector (multimodal embedders such as Qwen3-VL-Embedding):
    // aux rows have no mmproj field, it rides in the extra args as the
    // absolute in-container path the argv renderer would otherwise write.
    let parent = rel
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let siblings = {
        let dir = dir.to_string();
        tokio::task::spawn_blocking(move || crate::hf::scan_gguf_files(&dir))
            .await
            .map_err(|e| format!("scan panicked: {e}"))?
    };
    for cand in siblings {
        if cand == rel || cand.rsplit_once('/').map(|(d, _)| d).unwrap_or("") != parent {
            continue;
        }
        let Ok(cs) = summarize(std::path::Path::new(dir).join(&cand)).await else {
            continue;
        };
        if role_of(&cs) == "mmproj" {
            put(
                "extra_args",
                json!(format!("--mmproj /models/{cand}")),
                format!(
                    "sibling projector ({}), so the model can embed images; aux rows carry \
                     it as an extra arg with the in-container /models/ path",
                    cs.projector_type.as_deref().unwrap_or("vision")
                ),
            );
            break;
        }
    }

    // The location rule, said before anything else the caller might act on.
    let mut apply_ready = true;
    if found_in != Class::Aux {
        apply_ready = false;
        let aux_dir = models_dir_of(snap, Class::Aux);
        let in_aux =
            !aux_dir.trim().is_empty() && std::path::Path::new(&aux_dir).join(rel).is_file();
        let downloads = store::list_hf_models(&state.db).await.unwrap_or_default();
        let origin = downloads
            .iter()
            .find(|d| d.dest_path == rel && d.target == found_in.as_str());
        let fetch = match origin {
            Some(d) => format!(
                "lmgw__hf_add repo={} file={} target=aux (tracked there and updatable by \
                 lmgw__hf_set check_updates)",
                d.repo, d.file
            ),
            None => "lmgw__hf_add … target=aux (tracked and updatable), or move the file into \
                     the aux models dir"
                .to_string(),
        };
        warnings.push(format!(
            "{rel} is a {} model, which the aux class serves — but it was found in the {} \
             models dir, and an aux row can only address files under the aux models dir \
             ({}). {} Do not hard-link or copy it across, and do not create a chat-class \
             row for it: a chat row cannot embed, and the file would fall out of the \
             updater's view.",
            kind.as_str(),
            found_in.as_str(),
            if aux_dir.trim().is_empty() {
                "not configured — Settings → Runtimes → Aux".to_string()
            } else {
                aux_dir.clone()
            },
            if in_aux {
                "The same path already exists there: plan again with target=aux and create \
                 the row from that."
                    .to_string()
            } else {
                format!("Get it there with {fetch}, then plan again with target=aux.")
            }
        ));
    }

    let runtime = if probe {
        probe_runtime(state, rel, found_in).await
    } else {
        json!({ "checked": false, "reason": "probe not requested" })
    };
    if runtime["supported"] == Value::Bool(false) {
        warnings.push(format!(
            "the running llama.cpp build cannot load this model: {}",
            runtime["reason"]
                .as_str()
                .unwrap_or("see lmgw__model_inspect")
        ));
    }

    // Already configured as an aux model? Then this is a repair, and the
    // delta is what the caller needs — same contract as the chat plan.
    let configured: Vec<Value> = if found_in == Class::Aux {
        snap.aux_models
            .iter()
            .filter(|m| m.gguf_path == rel)
            .map(|row| aux_plan_diff(row, &params, &why))
            .collect()
    } else {
        Vec::new()
    };
    let repairs = configured
        .iter()
        .filter(|c| c["in_sync"] == Value::Bool(false))
        .count();
    let model_id = suggest_model_id(rel);
    let next_step = if !apply_ready {
        "get the file into the aux models dir (see warnings), then run lmgw__local_model_plan \
         again with target=aux"
            .to_string()
    } else if configured.is_empty() {
        format!(
            "pass these to lmgw__aux_model_set action=create (adding model_id, e.g. \
             '{model_id}'), then lmgw__local_model_test model_id=<id> target=aux — the test \
             embeds (or reranks) a probe input, since this model cannot generate"
        )
    } else if repairs == 0 {
        "every aux model already using this GGUF matches this plan; nothing to do".to_string()
    } else {
        format!(
            "this GGUF is already configured — pass the ready-made configured_as[].apply \
             object ({repairs} of them) to lmgw__aux_model_set, then lmgw__local_model_test \
             with target=aux"
        )
    };

    Ok(json!({
        "class": "aux",
        "target": found_in.as_str(),
        "apply_ready": apply_ready,
        "model_id": model_id,
        "params": params,
        "rationale": why,
        "warnings": warnings,
        "runtime": runtime,
        "configured_as": configured,
        "configured_note": "Aux models already serving this GGUF, and how they differ from \
                            the plan. Placement flags (--n-gpu-layers 0 for a CPU-only \
                            twin, --threads) live in extra_args and are never proposed \
                            here, so a deliberate CPU variant is not a difference.",
        "next_step": next_step,
    }))
}

/// The delta between an aux plan and a configured aux row on the same GGUF,
/// as a patch for `lmgw__aux_model_set`. Only the fields the plan proposes
/// are compared; `extra_args` only when the plan found a projector the row
/// does not name, since the row's args legitimately carry placement flags
/// the plan knows nothing about.
fn aux_plan_diff(
    row: &crate::config::AuxModel,
    params: &serde_json::Map<String, Value>,
    why: &serde_json::Map<String, Value>,
) -> Value {
    let current = |field: &str| -> Value {
        match field {
            "gguf_path" => json!(row.gguf_path),
            "kind" => json!(row.kind.as_str()),
            "pooling" => json!(row.pooling),
            "ctx_size" => json!(row.ctx_size),
            "extra_args" => json!(crate::runtime::argv::args_to_lines(&row.args)),
            _ => Value::Null,
        }
    };
    let mut changes: Vec<Value> = Vec::new();
    let mut patch = serde_json::Map::new();
    for (field, planned) in params {
        let cur = current(field);
        let differs = match field.as_str() {
            "extra_args" => !row.args.iter().any(|a| a.contains("mmproj")),
            _ => &cur != planned,
        };
        if !differs {
            continue;
        }
        changes.push(json!({
            "field": field,
            "current": cur,
            "planned": planned,
            "why": why.get(field).cloned().unwrap_or(Value::Null),
        }));
        patch.insert(field.clone(), planned.clone());
    }
    let in_sync = changes.is_empty();
    let mut out = serde_json::Map::new();
    out.insert("model_id".into(), json!(row.model_id));
    out.insert("id".into(), json!(row.id));
    out.insert("enabled".into(), json!(row.enabled));
    out.insert("in_sync".into(), json!(in_sync));
    out.insert("change".into(), json!(changes));
    if !in_sync {
        patch.insert("action".into(), json!("update"));
        patch.insert("model_id".into(), json!(row.model_id));
        patch.remove("gguf_path");
        out.insert("apply".into(), Value::Object(patch));
    }
    Value::Object(out)
}

/// What a load test asks the model to do — the one thing its class can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// One token from `/chat/completions`.
    Generate,
    /// One vector from `/embeddings`, checked to be non-empty and non-zero.
    Embed,
    /// Scores for two documents from `/rerank`.
    Rerank,
}

impl Probe {
    fn as_str(self) -> &'static str {
        match self {
            Self::Generate => "generate",
            Self::Embed => "embed",
            Self::Rerank => "rerank",
        }
    }
}

/// Whether a chat row's freeform args turn its server into an embedder — the
/// shape the old workaround took (an `embed/…` chat row with `--embedding`),
/// and one a load test must not then judge by token generation.
fn args_request_embeddings(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(
            a.trim_start_matches('-').split('=').next(),
            Some("embedding") | Some("embeddings")
        )
    })
}

/// Lowercased substrings that mean "the backend ran out of device memory",
/// across every backend a container image here might be built with.
///
/// One table rather than a chain of `contains` calls because the list is
/// backend-specific knowledge, and a CUDA-only list is exactly how an AMD host
/// ends up being told "see container_log below" for the one failure this tool
/// has a real answer for. `out of memory` catches ROCm's `hipMalloc failed:
/// out of memory` and llama.cpp's own CPU-side messages; the rest are the
/// spellings that do *not* contain that phrase.
const OOM_MARKERS: [&str; 5] = [
    "out of memory",
    // CUDA
    "cudamalloc",
    // ROCm/HIP
    "hiperroroutofmemory",
    // Vulkan: `vk::Device::allocateMemory: ErrorOutOfDeviceMemory`, and
    // ggml-vulkan's own `Device memory allocation of size N failed`.
    "erroroutofdevicememory",
    "device memory allocation of size",
];

/// `s` with its first letter upper-cased — a clause made a sentence.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The load-failure classifier (`local_model_test`'s, and the benchmark
/// launcher's, benchmark design §2.4): what a container's error and log tail
/// say went wrong, for the failures that have one known cause. `haystack` is
/// the error and the log lines, lowercased. `None` when nothing matches.
pub(crate) fn load_failure_hint(haystack: &str) -> Option<&'static str> {
    Some(if haystack.contains("unknown model architecture") {
        "the llama.cpp build in this container predates the model's architecture \
         — update the container image; the model config itself is fine"
    } else if haystack.contains("mtp layers") {
        "spec_type is draft-mtp but this GGUF has no MTP layers — clear spec_type, \
         or point draft_gguf_path at a real drafter"
    } else if haystack.contains("failed to load clip model") {
        "the runtime does not recognise this mmproj projector type — update the \
         container image, or clear mmproj_path to serve it text-only"
    } else if OOM_MARKERS.iter().any(|m| haystack.contains(m)) {
        "not enough VRAM — lower ctx_size, quantize the KV cache (cache_type_k/v \
         = q8_0), or reduce n_gpu_layers"
    } else if haystack.contains("no such file") {
        "a referenced GGUF is missing — check gguf_path (and, for aux rows, any --mmproj \
         in extra_args) with lmgw__local_model_get"
    } else {
        return None;
    })
}

/// Load a configured model and prove it does its job: a chat model generates
/// one token, an embedding model returns one vector, a reranker scores two
/// documents.
///
/// `lmgw__container apply` only recreates containers — it reports success for
/// a configuration that cannot load a single model, because nothing tries
/// until the next request. This is the tool that actually tries, and it
/// reports the verbatim error plus the container log lines behind it, which is
/// where the real reason lives.
///
/// It goes through the *real* path (§5): the model's own route, `vram::admit`,
/// and the endpoint the acquired container came up on — a test that skipped
/// admission would not be testing what a client's request does. The probe is
/// chosen by class and kind, never fixed to generation: an encoder has no LM
/// head, so asking it for a token reported every embedding model as broken.
pub async fn local_model_test(
    state: &SharedState,
    model_id: &str,
    target: Option<&str>,
) -> Result<Value, String> {
    let snap = state.snapshot();
    let given = crate::ops::parse_class_target(target)?;
    let class = crate::ops::resolve_model_class(&snap, model_id, given).map_err(|e| {
        let known: Vec<String> = snap
            .local_models
            .iter()
            .map(|m| m.model_id.clone())
            .chain(
                snap.aux_models
                    .iter()
                    .map(|m| format!("{} (aux)", m.model_id)),
            )
            .chain(
                snap.image_models
                    .iter()
                    .map(|m| format!("{} (image)", m.model_id)),
            )
            .collect();
        format!("{e}; configured: {}", known.join(", "))
    })?;

    // A diffusion pipeline is exercised on a different route with a different
    // body, so it gets its own function rather than a fourth arm below.
    if class == Class::Image {
        return image_model_test(state, &snap, model_id).await;
    }

    // `chat_row` is kept (cloned) past the match only for the Chat class: the
    // `/props` cross-check below needs the row to re-derive its static
    // capabilities the same way `/v1/models` would, and `snap` (which `row`
    // borrows from) does not live past this block.
    let (route, probe, enabled, chat_row) = match class {
        Class::Chat => {
            let row = snap
                .local_models
                .iter()
                .find(|m| m.model_id == model_id)
                .expect("resolved to chat");
            let probe = if args_request_embeddings(&row.args) {
                Probe::Embed
            } else {
                Probe::Generate
            };
            (
                snap.chat_local_route(model_id),
                probe,
                row.enabled,
                Some(row.clone()),
            )
        }
        Class::Aux => {
            let row = snap
                .aux_models
                .iter()
                .find(|m| m.model_id == model_id)
                .expect("resolved to aux");
            let probe = match row.kind {
                AuxKind::Embed => Probe::Embed,
                AuxKind::Rerank => Probe::Rerank,
            };
            (snap.aux_local_route(model_id), probe, row.enabled, None)
        }
        Class::Audio => {
            return Err(format!(
                "'{model_id}' is an audio model; audio.cpp models are exercised from the \
                 dashboard's Audio page, not by this load test"
            ))
        }
        Class::Image => unreachable!("the image class returned above"),
    };
    if !enabled {
        return Err(format!("'{model_id}' is disabled — enable it first"));
    }

    // Refused under a GPU hold, explicitly and before admission (gpu-hold
    // design §2). This is the one inference path that does not go through
    // `resolve_for_request`: the routes above address the row directly, so
    // they do no fallback lookup and no hold check of their own — and a test
    // is exactly the thing that would otherwise start a container on a card
    // the owner has taken back. Named rather than left to `admit_local`'s net
    // so the answer says "hold", not "admission refused". A benchmark's
    // lease refuses it the same way (benchmark design §3.2). Per model, as
    // every admission site decides (`gpu_block_for`).
    if let Some(block) = snap.gpu_block_for(class, model_id) {
        return Err(format!(
            "{}, so '{model_id}' is not started to test it — a test loads the model for real. \
             {} and run it again.",
            block.who(),
            capitalize(block.how_it_ends())
        ));
    }

    // A ladder row (design §6: "that is also WP0's instrument") tests every
    // rung in turn instead of the one probe below — its own function, because
    // nothing else here changes: same route, same enabled/hold checks, same
    // Generate probe, just climbed to and repeated per rung.
    if let Some(row) = chat_row.as_ref().filter(|m| m.is_ladder()) {
        return ladder_local_model_test(state, &route, model_id, row).await;
    }

    let started = std::time::Instant::now();
    // Errors here are lmgw's own (no room, load timed out, podman refused);
    // they are the answer, not a reason to fall through to an HTTP attempt.
    let hold = crate::vram::admit(state, &route, model_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!("'{model_id}' has no runtime descriptor — nothing to start or test")
        })?;
    // The generation probe is real traffic to the model's KV pool, so it takes
    // the gate's per-send half like any chat send: on a guarded row it is
    // counted and reserves its tokens, and a load test never overfills a pool
    // a client is using; on a ladder row it is counted beside the send, like
    // any request. The request below mirrors the probe body exactly (one user
    // "hi", max_tokens 1), so the count is of what is sent; on any other row
    // this is a no-op without a network call.
    let probe_ir = crate::ir::ChatRequest {
        model_alias: model_id.to_string(),
        messages: vec![crate::ir::Message::text(crate::ir::Role::User, "hi")],
        params: crate::ir::Params {
            max_tokens: Some(1),
            ..Default::default()
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let mut probe_params = probe_ir.params.clone();
    let mut lease = match probe {
        Probe::Generate => {
            let (lease, _) = crate::gate::fit_chat(
                state,
                Some(&hold),
                &route,
                &probe_ir,
                &mut probe_params,
                false,
                None,
            )
            .await
            .map_err(|f| {
                format!(
                    "the request gate refused the load test's probe: {}",
                    f.error
                )
            })?;
            lease
        }
        Probe::Embed | Probe::Rerank => crate::gate::TurnLease::unguarded(),
    };
    // Deliberately long: this is the one call that pays the real cost of a
    // cold load, and a 16GB model coming off a spinning disk into VRAM can
    // take minutes. A short timeout here would report "broken" for "slow".
    //
    // Through the gate's send (§3.2's dead-container retry, a ladder's count)
    // like every other forward: a smoke test that reported "broken" for a
    // container that had died since it was acquired would be reporting on
    // lmgw, not on the model. Only the generation probe's lease can carry a
    // ladder, so the embedding and rerank probes are plain sends.
    let sent = crate::gate::send_gated(
        state,
        Some(&hold),
        &route,
        &mut lease,
        crate::gate::CountInput::Chat {
            ir: &probe_ir,
            params: &probe_params,
            stream: false,
        },
        None,
        None,
        |r| {
            let base = r.upstream.base().to_string();
            let (path, body) = match probe {
                Probe::Generate => (
                    "/chat/completions",
                    json!({
                        "model": model_id,
                        "messages": [{ "role": "user", "content": "hi" }],
                        "max_tokens": 1,
                        "stream": false,
                    }),
                ),
                Probe::Embed => (
                    "/embeddings",
                    json!({ "model": model_id, "input": "lmgw load test" }),
                ),
                Probe::Rerank => (
                    "/rerank",
                    json!({
                        "model": model_id,
                        "query": "what is lmgw?",
                        "documents": [
                            "lmgw is a self-hosted LLM API gateway.",
                            "Bananas are yellow when ripe."
                        ],
                        "top_n": 2,
                    }),
                ),
            };
            Ok(state
                .http
                .post(format!("{base}{path}"))
                .json(&body)
                .timeout(std::time::Duration::from_secs(300)))
        },
    )
    .await;
    // Admitted without a fallback policy (a direct `vram::admit`), so a climb
    // never answers with one: that would be lmgw's own bug, named as such.
    let resp = match sent {
        Ok(crate::gate::Sent::Upstream(r)) => Ok(r),
        Ok(crate::gate::Sent::Rerouted(_)) => Err(crate::error::GatewayError::Internal(
            "the load test's probe was sent elsewhere by the gate, which a direct admission \
             never does"
                .into(),
        )),
        Err(e) => Err(e),
    };
    let latency_ms = started.elapsed().as_millis() as u64;

    // A 200 is necessary, not sufficient: an embedder misconfigured as a
    // reranker answers with an all-zero vector (spike-verified), and that is
    // the failure this probe exists to catch.
    let mut extra = serde_json::Map::new();
    // Whether the probe's answer was read to its end — the generation probe's
    // lease is released at once then, and only once llama-server lets go of
    // the slot otherwise (`gate::pool`, second review, finding 6).
    let mut read_whole = false;
    let (ok, detail) = match resp {
        Ok(r) if r.status().is_success() => match probe {
            Probe::Generate => {
                read_whole = r.bytes().await.is_ok();
                (true, String::new())
            }
            Probe::Embed => match r.json::<Value>().await {
                Ok(body) => {
                    let vec = body["data"][0]["embedding"].as_array().cloned();
                    match vec {
                        Some(v) if !v.is_empty() => {
                            let nonzero = v.iter().any(|x| x.as_f64().is_some_and(|f| f != 0.0));
                            extra.insert("dimensions".into(), json!(v.len()));
                            if nonzero {
                                (true, String::new())
                            } else {
                                (
                                    false,
                                    format!(
                                        "the model answered with an all-zero {}-dimensional \
                                         vector",
                                        v.len()
                                    ),
                                )
                            }
                        }
                        _ => (
                            false,
                            format!(
                                "HTTP 200 but no embedding in the response: {}",
                                body.to_string().chars().take(300).collect::<String>()
                            ),
                        ),
                    }
                }
                Err(e) => (false, format!("HTTP 200 but the body is not JSON: {e}")),
            },
            Probe::Rerank => match r.json::<Value>().await {
                Ok(body) => match body["results"].as_array() {
                    Some(results)
                        if !results.is_empty()
                            && results.iter().all(|x| x["relevance_score"].is_number()) =>
                    {
                        extra.insert("scored".into(), json!(results.len()));
                        (true, String::new())
                    }
                    _ => (
                        false,
                        format!(
                            "HTTP 200 but no relevance scores in the response: {}",
                            body.to_string().chars().take(300).collect::<String>()
                        ),
                    ),
                },
                Err(e) => (false, format!("HTTP 200 but the body is not JSON: {e}")),
            },
        },
        Ok(r) => {
            let code = r.status().as_u16();
            let body = r.text().await;
            read_whole = body.is_ok();
            let body = body.unwrap_or_default();
            (false, format!("HTTP {code}: {}", body.trim()))
        }
        Err(e) => (false, e.to_string()),
    };
    // The probe's answer is read (or failed): it no longer occupies the pool.
    lease.end(read_whole);
    if ok {
        let mut out = json!({
            "ok": true, "loaded": true, "model_id": model_id,
            "class": class.as_str(),
            "probe": probe.as_str(),
            "latency_ms": latency_ms,
        });
        out.as_object_mut().expect("object").extend(extra);
        // The running-build cross-check (design §3.1's live-verified gloss,
        // §8 item 9): only for the chat class, since `/props`'
        // `chat_template_caps` and `modalities` are compared against the same
        // template/projector derivation `/v1/models` publishes for a
        // `LocalModel` row — aux rows have no chat template to disagree over.
        if let Some(model) = &chat_row {
            let check = props_cross_check(state, &hold, model).await;
            let obj = out.as_object_mut().expect("object");
            obj.insert("props".to_string(), check.props);
            obj.insert("static".to_string(), check.static_caps);
            obj.insert("disagreements".to_string(), json!(check.disagreements));
            if let Some(note) = check.note {
                obj.insert("note".to_string(), json!(note));
            }
        }
        return Ok(out);
    }

    // The HTTP error is usually a generic "failed to load"; the reason is in
    // this model's own container log (§3.6's `logs` verb, the same tail
    // `lmgw__container action=logs` reads — there is no shared container log
    // to grep any more, and that is an improvement: these lines are about
    // this model and nothing else).
    let container =
        crate::runtime::container_name(&snap.settings.container_prefix, class, model_id);
    let log_lines: Vec<String> = match state.runtime().logs_tail(&container, 60).await {
        Ok(text) => text
            .lines()
            .filter(|l| {
                let low = l.to_ascii_lowercase();
                ["error", "failed", "unknown model", "exiting"]
                    .iter()
                    .any(|n| low.contains(n))
            })
            .map(|l| l.trim().to_string())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        Err(_) => Vec::new(),
    };

    // A chat row whose file is really an encoder: the classic misfiling, and
    // worth naming over any generic hint, because no amount of parameter
    // tuning makes an embedding model generate.
    let misfiled_encoder = if class == Class::Chat && probe == Probe::Generate {
        let dir = models_dir_of(&snap, Class::Chat);
        let rel = snap
            .local_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| m.gguf_path.clone())
            .unwrap_or_default();
        match summarize(std::path::Path::new(&dir).join(&rel)).await {
            Ok(s) => class_for(&s).0 == "aux",
            Err(_) => false,
        }
    } else {
        false
    };

    let haystack = format!("{detail}\n{}", log_lines.join("\n")).to_ascii_lowercase();
    let hint = if misfiled_encoder {
        "this GGUF is an embedding/rerank model (its header declares a pooling_type or a \
         classifier head) — it has no LM head to generate with. Serve it as an aux model \
         instead: lmgw__local_model_plan with target=aux, then lmgw__aux_model_set"
    } else if let Some(hint) = load_failure_hint(&haystack) {
        hint
    } else if probe == Probe::Embed && haystack.contains("all-zero") {
        "an all-zero vector is what llama-server returns when an embedder is served with \
         --reranking, or when pooling is wrong for the model — check kind and pooling with \
         lmgw__local_model_get, and lmgw__local_model_plan target=aux for what the header says"
    } else if probe == Probe::Embed && haystack.contains("pooling") {
        "llama-server rejected the pooling — lmgw__local_model_plan target=aux reports the \
         value the GGUF header declares"
    } else if probe != Probe::Generate
        && (haystack.contains("not supported") || haystack.contains("only supported"))
    {
        "the server refused this endpoint — the row's kind (embed / rerank) decides which \
         flag it starts with; check it with lmgw__local_model_get"
    } else {
        "see container_log below, and lmgw__local_model_get for the exact command \
         line this model renders to"
    };

    Ok(json!({
        "ok": false, "loaded": false, "model_id": model_id,
        "class": class.as_str(),
        "probe": probe.as_str(),
        "latency_ms": latency_ms,
        "error": detail,
        "container_log": log_lines,
        "hint": hint,
    }))
}

// ---------------------------------------------------------------------------
// Ladder models: every rung tested in turn (ladder design §6, §8 WP6)
// ---------------------------------------------------------------------------

/// [`local_model_test`] on a ladder row: climbs through every rung in order,
/// timing each one's load and proving it generates, then stops the model so
/// the next real request starts clean at the base (§3.5 — the ladder resets
/// on every stop). This is also WP0's instrument (design §6): short of
/// watching a live climb happen under real traffic, it is the only way to
/// measure a rung's real load time and confirm it actually loads.
///
/// **Mechanics.** A running container is stopped first, so every run starts
/// from the same place regardless of what real traffic left the model on —
/// refused, without touching anything, while it is busy (a test is an
/// explicit owner action, but that does not extend to dropping somebody
/// else's in-flight request). The base is then admitted fresh
/// ([`crate::vram::admit`], rung 1) and probed, and each higher rung is
/// reached with [`crate::vram::climb`] — the same primitive a request that
/// outgrows its rung uses — and probed in turn. A climb (or its probe)
/// failing ends the run there: nothing above a broken rung can be reached
/// through it. The final stop is not forced: this test never kills a request
/// that started while it ran, which means a build-up of real traffic during
/// a long ladder can leave `reset_to_base` false — named in the result rather
/// than hidden, so the owner knows to check `lmgw__status` or stop it by hand.
async fn ladder_local_model_test(
    state: &SharedState,
    route: &crate::config::Route,
    model_id: &str,
    row: &LocalModel,
) -> Result<Value, String> {
    let was_running = state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.class == Class::Chat && e.model_id == model_id)
        .map(|e| e.rung.map_or(1, |r| r.rung));
    if let Err(e) = state.runtime().stop(Class::Chat, model_id, false).await {
        return Err(match e {
            crate::runtime::registry::RuntimeError::Busy { in_flight, .. } => format!(
                "'{model_id}' has {in_flight} claim(s) open (requests, or tool loops/agent runs \
                 sitting between turns) — a ladder test stops and restarts its container at \
                 every rung in turn, and none of that must be dropped for it. Wait for it to go \
                 idle and run the test again."
            ),
            other => other.to_string(),
        });
    }

    let mut rungs = Vec::new();

    let started = std::time::Instant::now();
    let hold = match crate::vram::admit(state, route, model_id).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Err(format!(
                "'{model_id}' has no runtime descriptor — nothing to start or test"
            ))
        }
        Err(e) => return Err(e.to_string()),
    };
    rungs.push(probe_rung(state, route, &hold, model_id, row, 0, started.elapsed()).await);
    let mut all_ok = rungs
        .last()
        .is_some_and(|r| r["ok"].as_bool().unwrap_or(false));

    for k in 1..=row.top_rung() {
        if !all_ok {
            break;
        }
        let t0 = std::time::Instant::now();
        let outcome = match crate::vram::climb(state, &hold, k, "lmgw__local_model_test").await {
            Ok(crate::vram::Climbed::Done) => match hold.sync().await {
                // A `Done` can mean "reached" or "nothing to do, judge again"
                // (a live request's own climb already got there, joined or
                // stale, review third pass T1) — `hold.rung()` after `sync`
                // is the only way to tell which, and crediting a probe that
                // ran on some *other* rung as this one's would let a broken
                // rung k pass as tested.
                Ok(()) if hold.rung().map(|r| r.index) == Some(k) => {
                    probe_rung(state, route, &hold, model_id, row, k, t0.elapsed()).await
                }
                Ok(()) => {
                    let reached = hold.rung().map_or_else(
                        || "an unknown rung".to_string(),
                        |r| format!("{}/{}", r.index + 1, r.of),
                    );
                    rung_error(
                        state,
                        row,
                        k,
                        t0.elapsed(),
                        format!(
                            "not reached: other traffic moved the model to rung {reached} \
                             while this test was climbing to rung {}",
                            k + 1
                        ),
                    )
                    .await
                }
                Err(e) => rung_error(state, row, k, t0.elapsed(), e.to_string()).await,
            },
            // Never for this test's hold, which is the owner's; matched so a
            // guest's denial could not pass as a tested rung.
            Ok(crate::vram::Climbed::Denied { why }) => {
                rung_error(
                    state,
                    row,
                    k,
                    t0.elapsed(),
                    format!("not climbed: {why} — rung {} was never reached", k + 1),
                )
                .await
            }
            Ok(crate::vram::Climbed::Fallback { alias, reason, .. }) => {
                rung_error(
                    state,
                    row,
                    k,
                    t0.elapsed(),
                    format!(
                        "answered by the fallback '{alias}' ({}) instead of climbing — rung {} \
                         was never reached",
                        reason.as_str(),
                        k + 1
                    ),
                )
                .await
            }
            Err(e) => rung_error(state, row, k, t0.elapsed(), e.to_string()).await,
        };
        all_ok = outcome["ok"].as_bool().unwrap_or(false);
        rungs.push(outcome);
    }

    // This test's own claim would otherwise count as "still busy" against the
    // stop below — drop it first, the same release point a real request's
    // gate reaches once its response has been read to the end.
    drop(hold);

    // Not forced (module doc above): a request that started mid-test keeps
    // this from force-killing it, at the cost of an occasional false
    // `reset_to_base` on a busy box — named, not hidden.
    let reset_to_base = state
        .runtime()
        .stop(Class::Chat, model_id, false)
        .await
        .is_ok();

    Ok(json!({
        "ok": all_ok,
        "model_id": model_id,
        "class": "chat",
        "ladder": true,
        "top_rung": row.top_rung() + 1,
        "rungs": rungs,
        "was_running_rung": was_running,
        "reset_to_base": reset_to_base,
    }))
}

/// One rung's probe: [`rung_facts`] plus a live [`generate_probe`] on it.
async fn probe_rung(
    state: &SharedState,
    route: &crate::config::Route,
    hold: &crate::vram::LocalHold,
    model_id: &str,
    row: &LocalModel,
    index: usize,
    load: std::time::Duration,
) -> Value {
    let (ok, detail) = generate_probe(state, route, hold, model_id).await;
    let mut v = rung_facts(state, row, index, load).await;
    let obj = v.as_object_mut().expect("object");
    obj.insert("ok".into(), json!(ok));
    if !ok {
        obj.insert("error".into(), json!(detail));
    }
    v
}

/// A rung that never got a probe at all — the climb to it, or the sync
/// after, failed first.
async fn rung_error(
    state: &SharedState,
    row: &LocalModel,
    index: usize,
    load: std::time::Duration,
    error: String,
) -> Value {
    let mut v = rung_facts(state, row, index, load).await;
    let obj = v.as_object_mut().expect("object");
    obj.insert("ok".into(), json!(false));
    obj.insert("error".into(), json!(error));
    v
}

/// The facts §6 asks a per-rung test to report, `ok`/`error` aside: `rung
/// k/n`, the GGUF, its context, the derived per-slot context and switchover
/// ([`LocalModel::per_slot_ctx`]/[`LocalModel::switchover`]), and how long
/// this rung took to become ready.
///
/// `per_slot_ctx`/`switchover` are capped at the rung's own GGUF's trained
/// context ([`crate::ladder::slot_ctx`]) — the same number the gate judges a
/// live request on ([`crate::gate::ladder::target_rung`]) — rather than the
/// configured (possibly higher) value, so this test's report agrees with
/// what a request against this rung is actually held to (review third pass,
/// T4).
async fn rung_facts(
    state: &SharedState,
    row: &LocalModel,
    index: usize,
    load: std::time::Duration,
) -> Value {
    let view = row
        .all_rungs()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.index == index);
    let models_dir = state.snapshot().settings.router.models_dir.clone();
    let trained = crate::gate::ladder::trained_contexts(state, &models_dir, row).await;
    let per_slot_ctx = row
        .per_slot_ctx(index)
        .map(|c| crate::ladder::slot_ctx(c, trained.get(index).copied().flatten()));
    let n_predict = row.params.n_predict.filter(|&n| n > 0);
    let switchover = per_slot_ctx.zip(n_predict).map(|(c, n)| c - n);
    json!({
        "rung": index + 1,
        "of": row.top_rung() + 1,
        "gguf_path": view.map(|r| r.gguf_path.to_string()),
        "ctx_size": view.map(|r| r.ctx_size),
        "per_slot_ctx": per_slot_ctx,
        "switchover": switchover,
        "load_seconds": load.as_secs_f64(),
    })
}

/// One `/chat/completions` probe on `hold`'s container — one user "hi",
/// `max_tokens` 1, the same request [`local_model_test`]'s `Probe::Generate`
/// arm sends — factored out so [`ladder_local_model_test`] can send exactly
/// it against whichever rung `hold` is on right now, through the same gate a
/// real request takes (a ladder row is clamped and counted like any other
/// chat send; this probe is always far under every rung's switchover, so it
/// never itself triggers a climb).
///
/// Returns `(ok, detail)`; `detail` is empty on success.
async fn generate_probe(
    state: &SharedState,
    route: &crate::config::Route,
    hold: &crate::vram::LocalHold,
    model_id: &str,
) -> (bool, String) {
    let probe_ir = crate::ir::ChatRequest {
        model_alias: model_id.to_string(),
        messages: vec![crate::ir::Message::text(crate::ir::Role::User, "hi")],
        params: crate::ir::Params {
            max_tokens: Some(1),
            ..Default::default()
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let mut probe_params = probe_ir.params.clone();
    let mut lease = match crate::gate::fit_chat(
        state,
        Some(hold),
        route,
        &probe_ir,
        &mut probe_params,
        false,
        None,
    )
    .await
    {
        Ok((lease, _)) => lease,
        Err(f) => {
            return (
                false,
                format!(
                    "the request gate refused the load test's probe: {}",
                    f.error
                ),
            )
        }
    };
    let sent = crate::gate::send_gated(
        state,
        Some(hold),
        route,
        &mut lease,
        crate::gate::CountInput::Chat {
            ir: &probe_ir,
            params: &probe_params,
            stream: false,
        },
        None,
        None,
        |r| {
            let base = r.upstream.base().to_string();
            Ok(state
                .http
                .post(format!("{base}/chat/completions"))
                .json(&json!({
                    "model": model_id,
                    "messages": [{ "role": "user", "content": "hi" }],
                    "max_tokens": 1,
                    "stream": false,
                }))
                .timeout(std::time::Duration::from_secs(300)))
        },
    )
    .await;
    // Admitted without a fallback policy (a direct `vram::admit`/`vram::climb`),
    // so a climb never answers with one: that would be lmgw's own bug, named
    // as such, the same stance the plain load test takes.
    let resp = match sent {
        Ok(crate::gate::Sent::Upstream(r)) => Ok(r),
        Ok(crate::gate::Sent::Rerouted(_)) => Err(
            "the load test's probe was sent elsewhere by the gate, which a direct admission \
             never does"
                .to_string(),
        ),
        Err(e) => Err(e.to_string()),
    };
    let mut read_whole = false;
    let (ok, detail) = match resp {
        Ok(r) if r.status().is_success() => {
            read_whole = r.bytes().await.is_ok();
            (true, String::new())
        }
        Ok(r) => {
            let code = r.status().as_u16();
            let body = r.text().await;
            read_whole = body.is_ok();
            (
                false,
                format!("HTTP {code}: {}", body.unwrap_or_default().trim()),
            )
        }
        Err(e) => (false, e),
    };
    lease.end(read_whole);
    (ok, detail)
}

// ---------------------------------------------------------------------------
// The image class's load test (image-generation design §8)
// ---------------------------------------------------------------------------

/// The smallest honest exercise of a loaded stable-diffusion.cpp pipeline,
/// measured in §12.3: 256×256, four steps, one image, a fixed seed. Every one
/// of them is reported back in the result, so the figure a caller compares
/// against its own always says what produced it.
const IMAGE_TEST_SIZE: &str = "256x256";
const IMAGE_TEST_STEPS: u32 = 4;
const IMAGE_TEST_SEED: i64 = 42;

/// Load an image model and prove it draws.
///
/// Its own function rather than a fourth arm of [`local_model_test`]'s match:
/// nothing about `/v1/chat/completions`, `/v1/embeddings` or `/v1/rerank`
/// applies to a diffusion pipeline, and the probe is a *route*, not a body.
///
/// Dispatched **in process through the real handler**
/// ([`crate::proxy::handle_image_generation`]), the way the Audio lab drives
/// its siblings: the test then takes the same admission, the same
/// `resolve_image` guards, the same error normalization and the same log row a
/// client's request takes. A hand-built POST at the acquired endpoint would
/// prove the container answers and nothing about the gateway in front of it —
/// and it would not appear in Logs, where the owner looks first.
async fn image_model_test(
    state: &SharedState,
    snap: &Snapshot,
    model_id: &str,
) -> Result<Value, String> {
    use base64::Engine as _;

    let row = snap
        .image_models
        .iter()
        .find(|m| m.model_id == model_id)
        .ok_or_else(|| format!("no image model '{model_id}' is configured"))?;
    if !row.enabled {
        return Err(format!("'{model_id}' is disabled — enable it first"));
    }
    // The same refusal shape the edits route takes for a row whose `edit`
    // column is false: what the row says it does is what lmgw will send it. A
    // pipeline that claims only `vid_gen` does not serve a still image, and a
    // test that asked anyway would be reporting on a request the row never
    // advertised.
    let modes = row.modes();
    if !modes.iter().any(|m| m == crate::config::IMAGE_MODE_DEFAULT) {
        return Err(format!(
            "'{model_id}' declares modes [{}] — this test generates one still image on \
             /v1/images/generations, which a row that does not claim '{}' does not serve. Add \
             it to `modes` if the pipeline really does generate images.",
            modes.join(", "),
            crate::config::IMAGE_MODE_DEFAULT
        ));
    }
    if let Some(block) = snap.gpu_block() {
        return Err(format!(
            "{}, so '{model_id}' is not started to test it — a test loads the model for real. \
             {} and run it again.",
            block.who(),
            capitalize(block.how_it_ends())
        ));
    }

    // Steps and seed reach the OpenAI route through sd.cpp's one extension
    // (§2.3): the route itself reads only prompt / n / size / output_format /
    // output_compression, and everything else is a JSON block inside the
    // prompt that the server strips before generating. lmgw adds no field of
    // its own — this is exactly the request a client would write.
    let prompt = format!(
        "a red cube on a white background \
         <sd_cpp_extra_args>{{\"seed\":{IMAGE_TEST_SEED},\
         \"sample_params\":{{\"sample_steps\":{IMAGE_TEST_STEPS}}}}}</sd_cpp_extra_args>"
    );
    let public = snap.image_public_name(model_id);
    let body = json!({
        "model": public,
        "prompt": prompt,
        "n": 1,
        "size": IMAGE_TEST_SIZE,
        "output_format": "png",
    });

    let started = std::time::Instant::now();
    let resp = crate::proxy::handle_image_generation(
        state.clone(),
        crate::proxy::RequestCtx::default(),
        body,
    )
    .await;
    let status = resp.status().as_u16();
    // Unbounded like the route itself: a 4096² PNG is the size it is, and a
    // read limit here would turn a working model into a failed test.
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .map_err(|e| format!("the image response body could not be read: {e}"))?;
    let latency_ms = started.elapsed().as_millis() as u64;

    let parsed: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let detail = if (200..300).contains(&status) {
        let images = parsed["data"].as_array().cloned().unwrap_or_default();
        let first = images
            .first()
            .and_then(|d| d["b64_json"].as_str())
            .unwrap_or_default();
        match base64::engine::general_purpose::STANDARD.decode(first) {
            Ok(bytes) if !bytes.is_empty() => {
                return Ok(json!({
                    "ok": true, "loaded": true, "model_id": model_id, "class": "image",
                    "public_name": public,
                    "probe": "image_generation",
                    "endpoint": "/v1/images/generations",
                    "latency_ms": latency_ms,
                    "size": IMAGE_TEST_SIZE,
                    "steps": IMAGE_TEST_STEPS,
                    "seed": IMAGE_TEST_SEED,
                    "n": images.len(),
                    // The server's own word for what it encoded; `null` when a
                    // build stops echoing it, never a guess of lmgw's.
                    "output_format": parsed["output_format"].clone(),
                    "bytes": bytes.len(),
                    "note": "One generation, never an edit: /v1/images/edits takes a \
                             reference image, and on a pipeline that cannot read one \
                             sd-server does not refuse it — it dies (design §12.8). An \
                             `edit` row is proven by a real edits request.",
                }));
            }
            Ok(_) => format!(
                "HTTP {status} but the response carried no image: {}",
                String::from_utf8_lossy(&raw)
                    .chars()
                    .take(300)
                    .collect::<String>()
            ),
            Err(e) => format!("HTTP {status} but data[0].b64_json is not base64: {e}"),
        }
    } else {
        format!("HTTP {status}: {}", String::from_utf8_lossy(&raw).trim())
    };

    // The reason is in this model's own container log, the same tail
    // `lmgw__container action=logs` reads.
    let container =
        crate::runtime::container_name(&snap.settings.container_prefix, Class::Image, model_id);
    let log_lines: Vec<String> = match state.runtime().logs_tail(&container, 60).await {
        Ok(text) => text
            .lines()
            .filter(|l| {
                let low = l.to_ascii_lowercase();
                ["error", "failed", "exception", "exiting"]
                    .iter()
                    .any(|n| low.contains(n))
            })
            .map(|l| l.trim().to_string())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        Err(_) => Vec::new(),
    };

    let haystack = format!("{detail}\n{}", log_lines.join("\n")).to_ascii_lowercase();
    let hint = if OOM_MARKERS.iter().any(|m| haystack.contains(m)) {
        "not enough VRAM — set `offload_to_cpu = true` in the row's args (measured §12.4: idle \
         1.0 GiB instead of 7.1, peak 7.7 instead of 13.7, +0.4 s per image), generate smaller, \
         or stop whatever else is resident"
    } else if haystack.contains("no such file") || haystack.contains("failed to open") {
        "a file this row names is not where it says it is — lmgw__local_model_get \
         model_id=<id> target=image reports which of them are present"
    } else if haystack.contains("prompt required") {
        "sd-server rejected the request before generating — this is a gateway bug, not a model \
         problem: the test always sends a prompt"
    } else {
        "see container_log below, and lmgw__local_model_get target=image for the exact command \
         line this row renders to"
    };

    Ok(json!({
        "ok": false, "loaded": false, "model_id": model_id, "class": "image",
        "public_name": public,
        "probe": "image_generation",
        "endpoint": "/v1/images/generations",
        "latency_ms": latency_ms,
        "error": detail,
        "container_log": log_lines,
        "hint": hint,
    }))
}

/// What [`props_cross_check`] found: the running build's own answer, the same
/// static facts `/v1/models` would publish for this row, and every place they
/// disagree.
struct PropsCheck {
    /// llama-server's `/props` body, verbatim, or `Value::Null` when it could
    /// not be read — never fails the test (design §8 item 9).
    props: Value,
    /// `{reasoning, tool_calls, input_modalities}` — the subset of this row's
    /// static [`ModelCapabilities`] that `/props` also states an opinion on.
    static_caps: Value,
    disagreements: Vec<String>,
    /// Why `props` is null, when it is.
    note: Option<String>,
}

/// Cross-checks a just-loaded chat row's static capabilities (the same
/// derivation `/v1/models` publishes, re-run here through
/// [`capabilities::exposed::derived_for_local`]) against the running
/// llama-server's own `GET /props` (design §3.1's live-verified gloss, §8
/// item 9). `/props` lives on the container root, not under `/v1`
/// (`AcquireGuard::endpoint`'s split from `LocalHold::endpoint`), so this
/// dials `hold.port()` directly rather than the route base the probe request
/// used.
async fn props_cross_check(
    state: &SharedState,
    hold: &crate::vram::LocalHold,
    model: &LocalModel,
) -> PropsCheck {
    let derived = capabilities::exposed::derived_for_local(state, model).await;
    let caps = derived.capabilities.as_ref();
    let static_caps = json!({
        "reasoning": caps.and_then(|c| c.reasoning.clone()),
        "tool_calls": caps.and_then(|c| c.tool_calls.clone()),
        "input_modalities": caps.and_then(|c| c.input_modalities.clone()),
    });

    let props_url = format!("http://127.0.0.1:{}/props", hold.port());
    let fetch = async {
        let resp = state
            .http
            .get(&props_url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        resp.json::<Value>().await.map_err(|e| e.to_string())
    };

    match fetch.await {
        Ok(props) => {
            let disagreements = caps
                .map(|c| props_disagreements(c, &props))
                .unwrap_or_default();
            PropsCheck {
                props,
                static_caps,
                disagreements,
                note: None,
            }
        }
        Err(e) => PropsCheck {
            props: Value::Null,
            static_caps,
            disagreements: Vec::new(),
            note: Some(format!(
                "GET /props on the running container could not be read ({e}); the \
                 static-vs-running comparison is skipped, everything else above is unaffected."
            )),
        },
    }
}

/// Every place the running build's `/props` disagrees with the static
/// derivation (design §8 item 9). Only compared when both sides state an
/// opinion: an absent static field is "unknown", not a mismatch, and a
/// `/props` shape this old a build does not send is the same "unknown".
fn props_disagreements(caps: &ModelCapabilities, props: &Value) -> Vec<String> {
    let ctc = &props["chat_template_caps"];
    let modalities = &props["modalities"];
    let mut out = Vec::new();

    let static_native = caps.tool_calls.as_ref().map(|t| t.kind == "native");
    if let (Some(s), Some(live)) = (static_native, ctc["supports_tools"].as_bool()) {
        if s != live {
            out.push(format!(
                "tools: the static derivation says tool_calls.kind == \"{}\" (native tool calls \
                 = {s}), but the running build's /props chat_template_caps.supports_tools is \
                 {live}",
                caps.tool_calls
                    .as_ref()
                    .map(|t| t.kind.as_str())
                    .unwrap_or("?"),
            ));
        }
    }

    let static_parallel = caps.tool_calls.as_ref().and_then(|t| t.parallel);
    if let (Some(s), Some(live)) = (
        static_parallel,
        ctc["supports_parallel_tool_calls"].as_bool(),
    ) {
        if s != live {
            out.push(format!(
                "parallel tool calls: the static derivation says tool_calls.parallel = {s}, but \
                 /props chat_template_caps.supports_parallel_tool_calls is {live}"
            ));
        }
    }

    let static_levels = caps.reasoning.as_ref().map(|r| r.kind == "levels");
    if let (Some(s), Some(live)) = (static_levels, ctc["supports_reasoning_effort"].as_bool()) {
        if s != live {
            out.push(format!(
                "reasoning effort: the static derivation says reasoning.kind == \"{}\" (effort \
                 levels = {s}), but /props chat_template_caps.supports_reasoning_effort is {live}",
                caps.reasoning
                    .as_ref()
                    .map(|r| r.kind.as_str())
                    .unwrap_or("?"),
            ));
        }
    }

    let static_preserve = caps.reasoning.as_ref().and_then(|r| r.preserve_history);
    if let (Some(s), Some(live)) = (
        static_preserve,
        ctc["supports_preserve_reasoning"].as_bool(),
    ) {
        if s != live {
            out.push(format!(
                "preserve reasoning: the static derivation says reasoning.preserve_history = \
                 {s}, but /props chat_template_caps.supports_preserve_reasoning is {live}"
            ));
        }
    }

    let static_vision = caps.vision;
    if let (Some(s), Some(live)) = (static_vision, modalities["vision"].as_bool()) {
        if s != live {
            out.push(format!(
                "vision: the static derivation says capabilities.vision = {s}, but /props \
                 modalities.vision is {live}"
            ));
        }
    }

    let static_audio = caps
        .input_modalities
        .as_ref()
        .map(|m| m.iter().any(|x| x == "audio"));
    if let (Some(s), Some(live)) = (static_audio, modalities["audio"].as_bool()) {
        if s != live {
            out.push(format!(
                "audio: the static derivation says input_modalities contains \"audio\" = {s}, \
                 but /props modalities.audio is {live}"
            ));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(arch: &str) -> ModelSummary {
        ModelSummary {
            architecture: Some(arch.to_string()),
            ..ModelSummary::default()
        }
    }

    #[test]
    fn model_ids_drop_the_quantization_label() {
        assert_eq!(
            suggest_model_id("unsloth/Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf"),
            "muse-glimmer-30b"
        );
        assert_eq!(suggest_model_id("Qwen3.6-27B-IQ4_XS.gguf"), "qwen3.6-27b");
        assert_eq!(
            suggest_model_id("translategemma-27b-it-Q4_K_M.gguf"),
            "translategemma-27b-it"
        );
        // Nothing to strip is not an error.
        assert_eq!(suggest_model_id("plainmodel.gguf"), "plainmodel");
    }

    /// The classification that keeps an embedding model out of the chat
    /// table: decided by the header's pooling_type / classifier head, never
    /// by the filename or the architecture name a chat model shares.
    #[test]
    fn encoders_classify_as_aux_by_their_header() {
        // Qwen3-Embedding is architecture `qwen3` — the same as the chat
        // model — and differs only in pooling_type.
        let mut embed = summary("qwen3");
        embed.pooling_type = Some(3);
        assert_eq!(class_for(&embed), ("aux", Some(AuxKind::Embed)));

        let mut rerank = summary("bert");
        rerank.pooling_type = Some(4);
        assert_eq!(class_for(&rerank), ("aux", Some(AuxKind::Rerank)));

        // An older reranker conversion: no pooling_type, but the head.
        let mut old_rerank = summary("bert");
        old_rerank.has_classifier_head = true;
        assert_eq!(class_for(&old_rerank), ("aux", Some(AuxKind::Rerank)));

        // A BERT-family encoder with neither is still an embedder.
        assert_eq!(
            class_for(&summary("nomic-bert")),
            ("aux", Some(AuxKind::Embed))
        );

        // A chat model, and a chat model's companions.
        let mut chat = summary("qwen35");
        chat.has_chat_template = true;
        assert_eq!(class_for(&chat), ("chat", None));
        let mut proj = summary("clip");
        proj.is_mmproj = true;
        proj.pooling_type = Some(1);
        assert_eq!(class_for(&proj), ("chat", None));
        assert_eq!(class_for(&summary("dflash")), ("chat", None));
    }

    /// A chat row carrying `--embedding` in its extra args (the shape the
    /// old workaround took) is probed as an embedder, not by generation.
    #[test]
    fn a_chat_row_flagged_as_an_embedder_is_probed_for_embeddings() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(args_request_embeddings(&args(&[
            "--embedding",
            "--pooling",
            "last"
        ])));
        assert!(args_request_embeddings(&args(&["--embeddings"])));
        assert!(args_request_embeddings(&args(&["--embedding=true"])));
        assert!(!args_request_embeddings(&args(&["--no-warmup"])));
        assert!(!args_request_embeddings(&[]));
    }

    #[test]
    fn draft_architectures_map_to_their_spec_type() {
        assert_eq!(spec_type_for(&summary("dflash")), Some("draft-dflash"));
        assert_eq!(spec_type_for(&summary("dspark")), Some("draft-dspark"));
        assert_eq!(spec_type_for(&summary("eagle3")), Some("draft-eagle3"));
        // A plain model with no MTP heads cannot self-draft.
        assert_eq!(spec_type_for(&summary("qwen35")), None);

        let mut mtp = summary("qwen35");
        mtp.has_mtp_layers = true;
        assert_eq!(spec_type_for(&mtp), Some("draft-mtp"));

        // A projector is never a drafter, whatever else it says.
        let mut proj = summary("clip");
        proj.is_mmproj = true;
        proj.has_mtp_layers = true;
        assert_eq!(spec_type_for(&proj), None);
    }

    #[test]
    fn a_full_model_carrying_mtp_tensors_is_still_weights() {
        // The trap: `-MTP-GGUF` repos ship full models *with* MTP tensors, so
        // tensor presence alone would misfile a 27B model as a drafter.
        let mut big = summary("qwen35");
        big.has_mtp_layers = true;
        big.block_count = Some(64);
        assert_eq!(role_of(&big), "weights");

        let mut module = summary("qwen35");
        module.has_mtp_layers = true;
        module.block_count = Some(1);
        assert_eq!(role_of(&module), "drafter");

        assert_eq!(role_of(&summary("dflash")), "drafter");
        let mut proj = summary("clip");
        proj.is_mmproj = true;
        assert_eq!(role_of(&proj), "mmproj");
        assert_eq!(role_of(&ModelSummary::default()), "unknown");
    }

    /// Verified against a live container: without `--port` the probe exits in
    /// 0.2s with "couldn't bind HTTP server socket" — binding happens before
    /// model loading — so every architecture would be reported unsupported.
    #[test]
    fn the_probe_binds_its_own_port_and_costs_nothing_to_load() {
        let joined = probe_args("a/b.gguf").join(" ");
        assert!(joined.contains("--port 0"), "{joined}");
        assert!(joined.contains("--host 127.0.0.1"), "{joined}");
        assert!(joined.contains("--model /models/a/b.gguf"), "{joined}");
        // Costs nothing to load: no GPU offload, minimal context, no warmup.
        assert!(joined.contains("--n-gpu-layers 0"), "{joined}");
        assert!(joined.contains("--no-warmup"), "{joined}");
    }

    /// The probe container mounts the models dir the same way a model's own
    /// container does (§6: unlabeled mount + `label=disable`), because a
    /// probe that cannot read the GGUF reports every model as unloadable on
    /// an SELinux-enforcing box.
    #[test]
    fn the_probe_mounts_the_models_dir_read_only() {
        let mounts = probe_mounts("/srv/models/");
        assert_eq!(
            mounts,
            vec![
                "-v".to_string(),
                "/srv/models:/models:ro".to_string(),
                "--security-opt".to_string(),
                "label=disable".to_string(),
            ]
        );
    }

    /// The model's run args come first, then the models dir — and a row's own
    /// override replaces the class's rather than adding to it.
    #[test]
    fn the_probe_runs_with_the_resolved_run_args_and_the_models_dir() {
        let gpu = vec!["--device".to_string(), "nvidia.com/gpu=all".to_string()];
        assert_eq!(
            probe_run_args(&gpu, "/srv/models"),
            vec![
                "--device".to_string(),
                "nvidia.com/gpu=all".to_string(),
                "-e".to_string(),
                "CUDA_VISIBLE_DEVICES=".to_string(),
                "-v".to_string(),
                "/srv/models:/models:ro".to_string(),
                "--security-opt".to_string(),
                "label=disable".to_string(),
            ]
        );
        let mut snap = Snapshot::default();
        snap.settings.router.image = "class-image".into();
        snap.settings.router.extra_run_args = gpu.clone();
        assert_eq!(
            runtime_for_gguf(&snap, "a.gguf", Class::Chat),
            ("class-image".to_string(), gpu.clone())
        );
        let mut row = row_with(json!({}), &[]);
        row.gguf_path = "a.gguf".into();
        row.image = Some("row-image".into());
        row.extra_run_args = Some(vec!["--cpus".into(), "4".into()]);
        snap.local_models.push(row);
        assert_eq!(
            runtime_for_gguf(&snap, "a.gguf", Class::Chat),
            (
                "row-image".to_string(),
                vec!["--cpus".to_string(), "4".to_string()]
            )
        );
    }

    /// ik_llama.cpp installs `/llama-server` (on no `PATH`) and links
    /// `libcuda.so.1` directly, so the probe has to find the binary through
    /// the image's entrypoint and hand the container the GPU device — the
    /// probe used to try only the two official paths with no run args, and
    /// failed for every ik image.
    #[tokio::test]
    async fn the_arch_probe_finds_ik_llama_server_and_passes_the_gpu_run_args() {
        use crate::runtime::registry::{CmdOutput, CommandRunner, Registry};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Fake {
            calls: Mutex<Vec<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl CommandRunner for Fake {
            async fn run(&self, _p: &str, args: &[String]) -> std::io::Result<CmdOutput> {
                self.calls.lock().unwrap().push(args.to_vec());
                let out = |status: i32, stdout: String, stderr: &str| CmdOutput {
                    status,
                    stdout,
                    stderr: stderr.to_string(),
                };
                Ok(match args[0].as_str() {
                    "image" => out(0, format!("{} [\"/llama-server\"]\n", "a".repeat(64)), ""),
                    "run" => out(
                        1,
                        String::new(),
                        "llama_model_load: error loading model: unknown model architecture: \
                         'muse-glimmer'",
                    ),
                    _ => out(0, String::new(), ""),
                })
            }
        }

        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let fake = Arc::new(Fake::default());
        state.set_runtime_for_tests(Arc::new(Registry::new(
            fake.clone(),
            reqwest::Client::new(),
        )));
        let mut settings = crate::config::Settings {
            self_admin: crate::config::SelfAdmin::Full,
            ..Default::default()
        };
        settings.router.models_dir = "/srv/models".into();
        settings.router.image = "localhost/llama-server-cuda:ik-latest".into();
        settings.router.extra_run_args = vec!["--device".into(), "nvidia.com/gpu=all".into()];
        crate::store::save_settings(&state.db, &settings)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();

        let verdict = probe_runtime(&state, "x/w.gguf", Class::Chat).await;
        assert_eq!(verdict["checked"], true, "{verdict}");
        assert_eq!(verdict["supported"], false, "{verdict}");

        let calls = fake.calls.lock().unwrap().clone();
        let runs: Vec<&Vec<String>> = calls.iter().filter(|a| a[0] == "run").collect();
        assert_eq!(runs.len(), 1, "the entrypoint answered first: {calls:?}");
        let run = runs[0];
        let pair = |a: &str, b: &str| run.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("--entrypoint", "/llama-server"), "{run:?}");
        assert!(pair("--device", "nvidia.com/gpu=all"), "{run:?}");
        assert!(pair("-v", "/srv/models:/models:ro"), "{run:?}");
        assert!(
            pair("-e", "CUDA_VISIBLE_DEVICES="),
            "the device libraries, but no device: {run:?}"
        );

        // Under the GPU hold the probe is deferred: nothing is started.
        let mut held = settings.clone();
        held.hold.active = true;
        crate::store::save_settings(&state.db, &held).await.unwrap();
        state.reload_snapshot().await.unwrap();
        fake.calls.lock().unwrap().clear();
        let verdict = probe_runtime(&state, "x/w.gguf", Class::Chat).await;
        assert_eq!(verdict["checked"], false, "{verdict}");
        assert!(verdict["reason"].as_str().unwrap().contains("GPU hold"));
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn probe_output_is_classified_by_the_architecture_check() {
        let unknown = classify_probe(
            "0.00.712 E llama_model_load: error loading model: unknown model architecture: 'muse-glimmer'\n\
             0.00.712 E llama_model_load_from_file_impl: failed to load model",
        );
        assert_eq!(unknown["supported"], false);
        assert!(unknown["reason"].as_str().unwrap().contains("muse-glimmer"));
        assert!(unknown["hint"]
            .as_str()
            .unwrap()
            .contains("container image"));

        let clip = classify_probe("E mtmd: Failed to load CLIP model from /models/mmproj.gguf");
        assert_eq!(clip["supported"], false);

        // Progress past the architecture check is a pass, even mid-load.
        let fine = classify_probe(
            "load_tensors: loading model tensors\nllama_context: constructing context",
        );
        assert_eq!(fine["supported"], true);

        // A crash counts only past the check (ik without a visible GPU
        // segfaults after printing the model's metadata).
        let ik = "llm_load_print_meta: arch             = qwen35\nLayer 24: 198.93 MiB";
        assert_eq!(classify_exit(ik, 139)["supported"], true);
        let early = classify_exit("ggml_cuda_init: failed to initialize CUDA", 139);
        assert_eq!(early["checked"], false, "{early}");
        assert_eq!(
            classify_exit("error loading model: unknown model architecture: 'x'", 1)["supported"],
            false
        );
        assert_eq!(classify_exit("load_tensors: loading", 0)["supported"], true);
    }

    // -- the batch a projector needs -----------------------------------------

    fn with_projector() -> LlamaParams {
        LlamaParams {
            mmproj_path: Some("gemma4/mmproj-BF16.gguf".into()),
            ..LlamaParams::default()
        }
    }

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Gemma 4 12B's projector, the live crash.
    fn gemma4uv() -> ImageAttention {
        ImageAttention::NonCausal {
            ty: "gemma4uv".into(),
            tokens: ImageTokens::Measured(GEMMA4_IMAGE_UBATCH),
        }
    }

    fn advise(params: &LlamaParams, args: &[String]) -> Option<String> {
        projector_ubatch_advisory(params, args, &gemma4uv())
    }

    fn projector(ty: Option<&str>) -> ModelSummary {
        ModelSummary {
            is_mmproj: true,
            has_vision_encoder: Some(true),
            projector_type: ty.map(str::to_string),
            ..ModelSummary::default()
        }
    }

    /// A Gemma 3 projector as its header states it: 896 px images in 14 px
    /// patches, no scale-factor key.
    fn gemma3_projector() -> ModelSummary {
        ModelSummary {
            vision_image_size: Some(896),
            vision_patch_size: Some(14),
            ..projector(Some("gemma3"))
        }
    }

    /// The types and widths of the projectors on this box (2026-09-24),
    /// against what `mtmd_decode_use_non_causal` in its llama.cpp answers.
    #[test]
    fn image_attention_follows_llama_cpps_non_causal_list() {
        let measured = |ty: &str| ImageAttention::NonCausal {
            ty: ty.to_string(),
            tokens: ImageTokens::Measured(1280),
        };
        // Gemma 4 12B: always non-causal.
        assert_eq!(
            image_attention(Some(&projector(Some("gemma4uv"))), Some(3840)),
            measured("gemma4uv")
        );
        // gemma4v depends on the text model: 26B-A4B and 31B are non-causal,
        // E2B and E4B causal.
        let g4v = projector(Some("gemma4v"));
        assert_eq!(image_attention(Some(&g4v), Some(2816)), measured("gemma4v"));
        assert_eq!(image_attention(Some(&g4v), Some(5376)), measured("gemma4v"));
        assert_eq!(
            image_attention(Some(&g4v), Some(2560)),
            ImageAttention::Causal
        );
        assert_eq!(
            image_attention(Some(&g4v), Some(1536)),
            ImageAttention::Causal
        );
        // Without the weights' width, the projector's own projection_dim is
        // the same number.
        let e4b = ModelSummary {
            vision_projection_dim: Some(2560),
            ..projector(Some("gemma4v"))
        };
        assert_eq!(image_attention(Some(&e4b), None), ImageAttention::Causal);
        assert!(matches!(
            image_attention(Some(&g4v), None),
            ImageAttention::Unknown(why) if why.contains("gemma4v")
        ));
        // A type with no measured ceiling is still non-causal, and says so.
        assert_eq!(
            image_attention(Some(&projector(Some("deepseek4v"))), None),
            ImageAttention::NonCausal {
                ty: "deepseek4v".into(),
                tokens: ImageTokens::Unmeasured
            }
        );
        // Everything else on this box decodes causally.
        for ty in [
            "qwen3vl_merger",
            "qwen2vl_merger",
            "pixtral",
            "muse-glimmer",
        ] {
            assert_eq!(
                image_attention(Some(&projector(Some(ty))), Some(4096)),
                ImageAttention::Causal,
                "{ty}"
            );
        }
        // No images at all: nothing to decode.
        let audio_only = ModelSummary {
            has_vision_encoder: Some(false),
            ..projector(Some("qwen2a"))
        };
        assert_eq!(
            image_attention(Some(&audio_only), None),
            ImageAttention::Causal
        );
    }

    /// Gemma 3 resizes every image to one square, so its token count is a
    /// property of the header: (896 / 14)^2 / 4^2 = 256. That fits llama.cpp's
    /// default ubatch of 512, so there is nothing to plan or warn about.
    #[test]
    fn gemma3_images_are_a_fixed_256_tokens_and_fit_the_default() {
        let attention = image_attention(Some(&gemma3_projector()), None);
        assert_eq!(
            attention,
            ImageAttention::NonCausal {
                ty: "gemma3".into(),
                tokens: ImageTokens::Fixed(256)
            }
        );
        let (floor, basis) = projector_ubatch_floor(&attention, &[]).unwrap();
        assert_eq!(floor, 256);
        assert!(basis.contains("exactly 256 tokens"), "{basis}");
        assert!(floor <= LLAMA_DEFAULT_UBATCH);
        assert_eq!(
            projector_ubatch_advisory(&with_projector(), &[], &attention),
            None
        );
        // A fixed-size projector ignores --image-max-tokens.
        assert_eq!(
            projector_ubatch_floor(&attention, &argv(&["--image-max-tokens", "4096"]))
                .unwrap()
                .0,
            256
        );
        // Below 256 it would still abort, and says so.
        let tiny = LlamaParams {
            ubatch_size: Some(128),
            ..with_projector()
        };
        let w = projector_ubatch_advisory(&tiny, &[], &attention).expect("must advise");
        assert!(w.contains("aborts llama-server"), "{w}");
        assert!(w.contains("at least 256"), "{w}");
        // The header's own scale factor, where it states one, is used.
        let scaled = ModelSummary {
            vision_scale_factor: Some(2),
            ..gemma3_projector()
        };
        assert!(matches!(
            image_attention(Some(&scaled), None),
            ImageAttention::NonCausal {
                tokens: ImageTokens::Fixed(1024),
                ..
            }
        ));
    }

    /// Not knowing is not the same as knowing it is safe.
    #[test]
    fn an_unreadable_untyped_or_unknown_projector_is_unknown() {
        for (p, says) in [
            (None, "could not be read"),
            (Some(projector(None)), "names no projector type"),
            (Some(projector(Some("brand-new-vl"))), "'brand-new-vl'"),
        ] {
            match image_attention(p.as_ref(), Some(4096)) {
                ImageAttention::Unknown(why) => assert!(why.contains(says), "{why}"),
                other => panic!("expected Unknown, got {other:?}"),
            }
        }
    }

    /// The live crash: a Gemma 4 row with a projector and no ubatch_size, so
    /// llama.cpp's 512 was in force and a full-page image aborted the server.
    #[test]
    fn a_projector_at_the_default_ubatch_is_advised_about_by_the_assert_it_trips() {
        let w = advise(&with_projector(), &[]).expect("must advise");
        assert!(
            w.contains("(gemma4uv projector) with non-causal attention"),
            "{w}"
        );
        assert!(
            w.contains(
                "ubatch_size is unset, so llama.cpp's default of 512 applies: an image of more \
                 than 512 tokens aborts llama-server"
            ),
            "{w}"
        );
        assert!(
            w.contains("GGML_ASSERT \"non-causal attention requires n_ubatch >= n_tokens\""),
            "{w}"
        );
        assert!(
            w.contains("Set ubatch_size to at least 1280 (Gemma 4 images stop"),
            "{w}"
        );
        // The default batch (2048) is not the problem, so it is not named.
        assert!(!w.contains("and batch_size"), "{w}");
        assert!(
            w.contains("still starts"),
            "an advisory, not a refusal: {w}"
        );
    }

    /// The four live Qwen3.5 rows: a qwen3vl_merger projector, batch_size
    /// 1024 and llama.cpp's default ubatch. Causal decoding splits an image
    /// across physical batches, so there is nothing to advise.
    #[test]
    fn a_causal_projector_gets_no_advisory_whatever_its_batch() {
        let qwen = LlamaParams {
            mmproj_path: Some("unsloth/Qwen3.5-9B-GGUF/mmproj-BF16.gguf".into()),
            batch_size: Some(1024),
            ..LlamaParams::default()
        };
        let attention = image_attention(Some(&projector(Some("qwen3vl_merger"))), Some(4096));
        assert_eq!(projector_ubatch_floor(&attention, &[]), None);
        assert_eq!(projector_ubatch_advisory(&qwen, &[], &attention), None);
    }

    /// When lmgw cannot tell, it still advises, says it could not tell, and
    /// says where the number it uses comes from.
    #[test]
    fn unknown_and_unmeasured_projectors_say_what_they_do_not_know() {
        let w = projector_ubatch_advisory(
            &with_projector(),
            &[],
            &ImageAttention::Unknown("its projector's header could not be read".into()),
        )
        .expect("must advise");
        assert!(
            w.contains(
                "lmgw could not tell whether llama.cpp decodes its images with non-causal \
                 attention: its projector's header could not be read. If it does"
            ),
            "{w}"
        );
        assert!(
            w.contains("at least 1280 (with the projector unknown"),
            "{w}"
        );

        let deepseek = ImageAttention::NonCausal {
            ty: "deepseek4v".into(),
            tokens: ImageTokens::Unmeasured,
        };
        let w = projector_ubatch_advisory(&with_projector(), &[], &deepseek).expect("must advise");
        assert!(
            w.contains("no image size has been measured for deepseek4v"),
            "{w}"
        );
    }

    #[test]
    fn a_text_only_row_or_large_enough_batches_get_no_advisory() {
        // Nothing to split without a projector.
        assert_eq!(advise(&LlamaParams::default(), &[]), None);
        // `--no-mmproj` claims the flag, so an extra-args projector never loads.
        let text_only = LlamaParams {
            no_mmproj: true,
            ..LlamaParams::default()
        };
        assert_eq!(
            advise(&text_only, &argv(&["--mmproj", "/models/x.gguf"])),
            None
        );
        // The floor exactly, with the batch left at llama.cpp's 2048.
        let at_floor = LlamaParams {
            ubatch_size: Some(GEMMA4_IMAGE_UBATCH),
            ..with_projector()
        };
        assert_eq!(advise(&at_floor, &[]), None);
        // The live gemma4 rows run 2048/2048.
        let live = LlamaParams {
            ubatch_size: Some(2048),
            batch_size: Some(2048),
            ..with_projector()
        };
        assert_eq!(advise(&live, &[]), None);
        // An extra arg reaches llama-server while the field is unset, so it
        // counts, in either spelling.
        for args in [&["-ub", "1280"][..], &["--ubatch-size=4096"][..]] {
            assert_eq!(advise(&with_projector(), &argv(args)), None, "{args:?}");
        }
    }

    /// mtmd hands an image over in pieces of at most batch_size tokens and
    /// llama.cpp clamps ubatch to batch. A ubatch at or above a too-small
    /// batch therefore never aborts: it splits the image, which degrades it.
    #[test]
    fn a_batch_too_small_for_the_image_splits_it_rather_than_aborting() {
        let clamped = LlamaParams {
            ubatch_size: Some(2048),
            batch_size: Some(1024),
            ..with_projector()
        };
        let w = advise(&clamped, &[]).expect("must advise");
        assert!(
            w.contains(
                "batch_size is 1024: an image of more than 1024 tokens is split across \
                 separate decodes. Nothing aborts"
            ),
            "{w}"
        );
        assert!(w.contains("degrades"), "{w}");
        assert!(!w.contains("aborts llama-server"), "{w}");
        // Only the field that is short is named.
        assert!(w.contains("Set batch_size to at least 1280"), "{w}");

        let equal = LlamaParams {
            ubatch_size: Some(1024),
            batch_size: Some(1024),
            ..with_projector()
        };
        let w = advise(&equal, &[]).expect("must advise");
        assert!(w.contains("split across separate decodes"), "{w}");
        assert!(
            w.contains("Set ubatch_size and batch_size to at least 1280"),
            "{w}"
        );
    }

    /// A ubatch below the batch aborts on any piece larger than the ubatch,
    /// whatever the batch.
    #[test]
    fn a_ubatch_below_the_batch_aborts_and_names_every_short_field() {
        let small_batch = LlamaParams {
            batch_size: Some(1024),
            ..with_projector()
        };
        let w = advise(&small_batch, &[]).expect("must advise");
        assert!(
            w.contains("an image of more than 512 tokens aborts llama-server"),
            "{w}"
        );
        assert!(
            w.contains("Set ubatch_size and batch_size to at least 1280"),
            "{w}"
        );

        // A value that came from the extra args is not described as unset.
        let w = advise(&with_projector(), &argv(&["-ub", "1024"])).expect("must advise");
        assert!(
            w.contains("ubatch_size is 1024: an image of more than 1024"),
            "{w}"
        );
    }

    #[test]
    fn a_raised_image_budget_raises_the_floor() {
        // The owner raised the image budget past the constant: that is the
        // ceiling now, and the constant must not hide it.
        let p = LlamaParams {
            ubatch_size: Some(2048),
            ..with_projector()
        };
        let w = advise(&p, &argv(&["--image-max-tokens", "4096"])).expect("must advise");
        assert!(
            w.contains(
                "Set ubatch_size and batch_size to at least 4096 (the row's own \
                 --image-max-tokens is 4096)"
            ),
            "{w}"
        );
        // A smaller budget does not lower the measured floor.
        assert_eq!(
            projector_ubatch_floor(&gemma4uv(), &argv(&["--image-max-tokens=256"]))
                .unwrap()
                .0,
            1280
        );
    }

    /// A projector outside the models dir loads, and its header cannot be
    /// read from here, so it is unknown. One that is missing from the models
    /// dir cannot load at all: the missing file is the problem, and there is
    /// no batch advice to give about it.
    #[tokio::test]
    async fn an_outside_projector_is_unknown_and_a_missing_one_is_skipped() {
        let outside = argv(&["-mm", "/srv/p.gguf"]);
        let attention = row_image_attention("/models-dir", &LlamaParams::default(), &outside, None)
            .await
            .expect("an outside projector is still checked");
        assert!(
            matches!(&attention, ImageAttention::Unknown(why) if why.contains("outside")),
            "{attention:?}"
        );
        let w = projector_ubatch_advisory(&LlamaParams::default(), &outside, &attention);
        assert!(w.is_some_and(|w| w.contains("could not tell")));

        let dir = tempfile::tempdir().unwrap();
        let missing = row_image_attention(
            &dir.path().display().to_string(),
            &with_projector(),
            &[],
            None,
        )
        .await;
        assert_eq!(missing, None);
    }

    fn row_with(params: Value, args: &[&str]) -> LocalModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": "gemma4", "gguf_path": "gemma4/w.gguf",
            "params": params, "args": args, "idle_seconds": 0,
            "enabled": true, "public": false, "warm_start": false,
        }))
        .unwrap()
    }

    /// What `local_model_plan` proposes for weights with a sibling projector,
    /// reduced to the fields that matter here.
    fn projector_plan() -> (
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    ) {
        let params = json!({
            "mmproj_path": "gemma4/mmproj-BF16.gguf",
            "ubatch_size": GEMMA4_IMAGE_UBATCH,
        });
        let why = json!({ "mmproj_path": "sibling", "ubatch_size": "the assert" });
        (
            params.as_object().unwrap().clone(),
            why.as_object().unwrap().clone(),
        )
    }

    fn changed_fields(d: &Value) -> Vec<&str> {
        d["change"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["field"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn the_plan_diff_raises_a_small_ubatch_and_never_lowers_a_large_one() {
        let (params, why) = projector_plan();
        let mm = "gemma4/mmproj-BF16.gguf";
        let image = gemma4uv();
        let diff = |row: &LocalModel| plan_diff(row, &params, &why, false, Some(&image));

        // The crashing row: ubatch unset, batch at llama.cpp's default. Only
        // the ubatch needs to change; the patch applies as-is.
        let d = diff(&row_with(json!({ "mmproj_path": mm }), &[]));
        assert_eq!(changed_fields(&d), ["ubatch_size"], "{d}");
        assert_eq!(d["apply"]["ubatch_size"], 1280);
        assert!(d["apply"]["batch_size"].is_null(), "{d}");
        assert_eq!(d["change"][0]["why"], "the assert");
        assert!(d["change"][0]["current"].is_null(), "{d}");

        // A deliberately larger pair is in sync, not a proposal to shrink it.
        let big = json!({ "mmproj_path": mm, "ubatch_size": 2048, "batch_size": 2048 });
        assert_eq!(diff(&row_with(big, &[]))["in_sync"], true);

        // Nor is a large enough ubatch passed through the extra args.
        let d = diff(&row_with(json!({ "mmproj_path": mm }), &["-ub", "1536"]));
        assert_eq!(d["in_sync"], true, "{d}");

        // A too-small one there is what is in force, and `current` says so
        // rather than showing the empty field.
        let d = diff(&row_with(
            json!({ "mmproj_path": mm }),
            &["-ub", "1024", "-b", "1024"],
        ));
        assert_eq!(changed_fields(&d), ["ubatch_size", "batch_size"], "{d}");
        assert_eq!(d["change"][0]["current"], 1024, "{d}");
        assert_eq!(d["change"][1]["current"], 1024, "{d}");

        // A batch below the floor would split the image or clamp the new
        // ubatch, so it is raised with it — the Qwen3.5 rows' batch_size 1024.
        let d = diff(&row_with(
            json!({ "mmproj_path": mm, "batch_size": 1024 }),
            &[],
        ));
        assert_eq!(changed_fields(&d), ["ubatch_size", "batch_size"], "{d}");
        assert_eq!(d["apply"]["batch_size"], 1280);
        assert!(d["change"][1]["why"].as_str().unwrap().contains("clamps"));

        // The row's own raised image budget is the floor it is diffed against.
        let d = diff(&row_with(
            json!({ "mmproj_path": mm }),
            &["--image-max-tokens", "4096"],
        ));
        assert_eq!(d["apply"]["ubatch_size"], 4096, "{d}");
        assert_eq!(d["apply"]["batch_size"], 4096, "{d}");
    }

    // -- against the real files, when this box has them ----------------------

    const MUSE: &str = "unsloth/Muse-Glimmer-30B-GGUF";
    const QWEN9B: &str = "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf";

    /// The models root (`LMGW_TEST_MODELS_DIR`), or `None` with a skip note.
    fn models_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("LMGW_TEST_MODELS_DIR").map(std::path::PathBuf::from);
        if root.is_none() {
            eprintln!("skipping: LMGW_TEST_MODELS_DIR is not set");
        }
        root
    }

    /// Summary of the file at `rel` below the models root; `None` (skip) when
    /// `LMGW_TEST_MODELS_DIR` is unset or the file is missing.
    fn real(rel: &str) -> Option<ModelSummary> {
        let p = models_root()?.join(rel);
        p.is_file().then(|| gguf::summarize(&p).ok()).flatten()
    }

    /// All sixteen projectors on this box (2026-09-24), typed through the
    /// header reader and classified against llama.cpp's non-causal list. The
    /// Gemma 4 files name their type only under `clip.vision.projector_type`.
    #[test]
    fn real_projectors_decode_the_way_llama_cpp_says() {
        let nc = |ty: &str| ImageAttention::NonCausal {
            ty: ty.to_string(),
            tokens: ImageTokens::Measured(GEMMA4_IMAGE_UBATCH),
        };
        let gemma = |size: &str| {
            (
                format!("unsloth/gemma-4-{size}-it-qat-GGUF/mmproj-BF16.gguf"),
                Some(format!(
                    "unsloth/gemma-4-{size}-it-qat-GGUF/gemma-4-{size}-it-qat-UD-Q4_K_XL.gguf"
                )),
            )
        };
        let causal = |path: &str| (path.to_string(), None);
        // (mmproj, the weights beside it when the width matters), type, answer.
        type Case = ((String, Option<String>), &'static str, ImageAttention);
        let cases: Vec<Case> = vec![
            (gemma("12B"), "gemma4uv", nc("gemma4uv")),
            (gemma("26B-A4B"), "gemma4v", nc("gemma4v")),
            (gemma("31B"), "gemma4v", nc("gemma4v")),
            (gemma("E4B"), "gemma4v", ImageAttention::Causal),
            (
                causal("bartowski/UI-TARS-7B-DPO-GGUF/mmproj-UI-TARS-7B-DPO-f16.gguf"),
                "qwen2vl_merger",
                ImageAttention::Causal,
            ),
            (
                causal("noctrex/Shieldstral-1.0-3B-GGUF/mmproj-BF16.gguf"),
                "pixtral",
                ImageAttention::Causal,
            ),
            (
                causal("unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"),
                "muse-glimmer",
                ImageAttention::Causal,
            ),
            (
                causal("Qwen/Qwen3-VL-4B-Instruct-GGUF/mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf"),
                "qwen3vl_merger",
                ImageAttention::Causal,
            ),
            (
                causal("example/Qwen3.6-27B-Finetune-NEO-MTP-GGUF/mmproj-BF16.gguf"),
                "qwen3vl_merger",
                ImageAttention::Causal,
            ),
        ];
        let qwen: Vec<_> = [
            "Qwen3.5-0.8B",
            "Qwen3.5-2B",
            "Qwen3.5-4B",
            "Qwen3.5-9B",
            "Qwen3.6-27B-MTP",
            "Qwen3.6-35B-A3B-MTP",
            "Qwen3.8-27B",
        ]
        .into_iter()
        .map(|m| {
            (
                causal(&format!("unsloth/{m}-GGUF/mmproj-BF16.gguf")),
                "qwen3vl_merger",
                ImageAttention::Causal,
            )
        })
        .collect();
        for ((mmproj, weights), ty, want) in cases.into_iter().chain(qwen) {
            let Some(p) = real(&mmproj) else {
                continue;
            };
            assert_eq!(p.projector_type.as_deref(), Some(ty), "{mmproj}");
            let width = weights
                .and_then(|w| real(&w))
                .and_then(|w| w.embedding_length);
            assert_eq!(image_attention(Some(&p), width), want, "{mmproj}");
        }
    }

    #[test]
    fn real_files_classify_correctly() {
        if let Some(s) = real(&format!("{MUSE}/dflash-kquant.gguf")) {
            assert_eq!(role_of(&s), "drafter");
            assert_eq!(spec_type_for(&s), Some("draft-dflash"));
        }
        if let Some(s) = real(&format!("{MUSE}/mmproj-kquant.gguf")) {
            assert_eq!(role_of(&s), "mmproj");
            assert_eq!(spec_type_for(&s), None);
        }
        if let Some(s) = real(&format!("{MUSE}/Muse-Glimmer-30B-UD-Q4_K_XL.gguf")) {
            assert_eq!(role_of(&s), "weights");
            assert_eq!(s.context_length, Some(131072));
        }
        // The model behind the live misconfiguration: no MTP layers, so
        // spec_type=draft-mtp against it can never load.
        if let Some(s) = real(QWEN9B) {
            assert!(!s.has_mtp_layers);
            assert_eq!(spec_type_for(&s), None);
        }
    }
}
