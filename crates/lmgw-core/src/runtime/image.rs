//! What an `sd-server` container says about itself, and the two directories
//! it refuses to start healthy without (image-generation design §3, §12.2).
//!
//! The image class has no config file to render — the engine sibling of
//! [`super::audio`] is therefore not a writer but a *reader*: after a start
//! answers its health probe, lmgw asks the container one question
//! (`GET /sdcpp/v1/capabilities`) and keeps the answer on the registry entry.
//! It is the only source for "is this an edit-capable / video-capable
//! pipeline, which samplers does it have, how large may an image be" that does
//! not require lmgw to know every model family's rules. Not persisted:
//! re-read on every start, because it describes the process, not the row.
//!
//! The directory part is the other half of §12.2. The capabilities route
//! *throws* — 500, `EXCEPTION_WHAT: filesystem error` — when
//! `--lora-model-dir` / `--hires-upscalers-dir` are unset, because the scan
//! walks the empty setting as the cwd and hits `/proc`. lmgw therefore always
//! renders both flags, which means both directories have to exist before the
//! container starts: [`ensure_dirs`] is what makes that true, called from the
//! same `render_spec` that writes audio's `server.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ImageModel;

/// The `files` keys that name a **directory** rather than a file.
///
/// Read off the key itself (`…_dir`) rather than from a list, because the
/// vocabulary is the image's and sd.cpp adds flags between lmgw releases — a
/// hardcoded list would silently mis-classify the next one. Every directory
/// flag sd-server has today ends this way (`--lora-model-dir`,
/// `--hires-upscalers-dir`, `--embd-dir`, `--pm-id-images-dir`).
pub fn is_dir_key(key: &str) -> bool {
    key.ends_with("_dir")
}

/// The flags lmgw spells itself on every image start (§3), and which a row
/// therefore may not set under any of their spellings: the address and port
/// the container is reached on, and the eager load the spike measured.
///
/// Canonical keys, because that is what a stored key resolves to
/// ([`crate::sdcpp_caps::SdcppCaps::resolve_key`]) — `l`, `-l`, `listen-ip`
/// and `listen_ip` are all this one entry.
pub const CLAIMED_FLAGS: &[&str] = &["listen_ip", "listen_port", "eager_load"];

/// The two directory flags lmgw renders unconditionally (§3), paired with the
/// path each falls back to under the class models dir when the row does not
/// name one.
pub const DEFAULT_DIRS: &[(&str, &str)] = &[
    ("lora_model_dir", "loras"),
    ("hires_upscalers_dir", "upscalers"),
];

/// The models-dir-relative path one of the two unconditional directory flags
/// resolves to for this row: its own `files` entry, or the class default.
pub fn dir_path(m: &ImageModel, key: &str, default: &str) -> String {
    dir_path_from(&m.files, key, default)
}

/// [`dir_path`] over a bare `files` map — what the argv renderer has in hand,
/// which carries the two maps rather than the whole row.
pub fn dir_path_from(
    files: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: &str,
) -> String {
    files
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        // A leading slash is the in-container spelling pasted back out of a
        // rendered command line; every class drops it on the way in, and this
        // is the same normalization one step later.
        .map(|s| s.trim_start_matches('/'))
        .filter(|s| !s.is_empty())
        // A value that climbs out of the models dir falls back to the class
        // default rather than being joined: this one is used *on the host*
        // (`ensure_dirs` creates it), so `../../tmp/x` would create a
        // directory outside the tree lmgw owns. Saving such a row is already
        // refused (`ops::check_rel_path`); this is the second half of that
        // rule, for a row that got in some other way.
        .filter(|s| is_plain_rel(s))
        .unwrap_or(default)
        .to_string()
}

/// Every component of a relative path is a plain name — no `..`, no root, no
/// prefix. The same rule `ops::check_rel_path` refuses a save with, applied
/// where a stored value is turned into a host path.
pub fn is_plain_rel(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Create the LoRA and upscaler directories this row's argv will point at.
///
/// Called before every start (from
/// [`super::descriptor::ModelRuntime::render_spec`], where audio writes its
/// `server.json`) rather than once when the class is configured: a directory
/// an owner deleted between two starts would otherwise take the capabilities
/// route down with it, and `create_dir_all` on a directory that is already
/// there costs one syscall.
///
/// An unconfigured `models_dir` is left alone — joining onto `""` would create
/// `./loras` in whatever directory lmgw happens to have been started from, and
/// the start is about to fail with "models dir is not configured" anyway.
pub fn ensure_dirs(models_dir: &str, m: &ImageModel) -> std::io::Result<()> {
    if models_dir.trim().is_empty() {
        return Ok(());
    }
    let root = PathBuf::from(models_dir);
    for (key, default) in DEFAULT_DIRS {
        std::fs::create_dir_all(root.join(dir_path(m, key, default)))?;
    }
    Ok(())
}

/// Sizes of every file a row names, for the footprint estimate and the
/// pre-flight — a directory key counts as its tree, a missing path as nothing
/// and is named by the caller.
pub fn file_target(models_dir: &str, rel: &str) -> PathBuf {
    let rel = rel.trim_start_matches('/');
    if !is_plain_rel(rel) {
        // Not joined: the caller asked where a stored value points, and a
        // value with a `..` in it points outside the models dir. Handing back
        // the models dir itself makes every caller's own check (is_file /
        // is_dir / size) fail, which is the answer they all want.
        return PathBuf::from(models_dir);
    }
    Path::new(models_dir).join(rel)
}

// ---------------------------------------------------------------------------
// GET /sdcpp/v1/capabilities
// ---------------------------------------------------------------------------

/// What one loaded pipeline reports about itself (§3's capabilities probe).
///
/// Deliberately typed rather than a `serde_json::Value`: this ends up on a
/// registry view that the status tick compares for equality, and a `Value`
/// would cost that its `Eq`. Unknown keys are ignored by serde, which is the
/// tolerance a moving `master-cuda` tag needs — a newer server that adds a
/// field keeps parsing, it just does not surface the new field until lmgw
/// names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageCapabilities {
    /// `["img_gen"]`, `["img_gen","vid_gen"]` — what the loaded pipeline can
    /// actually do, as opposed to what the row claims.
    pub supported_modes: Vec<String>,
    /// The mode the server is currently configured for.
    pub current_mode: String,
    pub limits: ImageLimits,
    pub features: ImageFeatures,
    pub samplers: Vec<String>,
    pub schedulers: Vec<String>,
    pub output_formats: Vec<String>,
    /// LoRAs found under `--lora-model-dir`.
    pub loras: Vec<ImageAsset>,
    /// Upscalers: the built-in resize modes plus anything under
    /// `--hires-upscalers-dir`.
    pub upscalers: Vec<ImageAsset>,
}

/// Bounds the server enforces on a request. Every field is optional because a
/// build that stops reporting one must not fail the whole parse — and because
/// **these are the server's real numbers**: lmgw never substitutes one of its
/// own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageLimits {
    pub min_width: Option<i64>,
    pub max_width: Option<i64>,
    pub min_height: Option<i64>,
    pub max_height: Option<i64>,
    pub max_batch_count: Option<i64>,
    /// Depth of the native async job queue; a `429` is what a full one
    /// answers. Irrelevant to the synchronous OpenAI routes WP2 serves, kept
    /// because it is the only published number about queueing at all.
    pub max_queue_size: Option<i64>,
}

/// Which optional inputs the loaded pipeline accepts.
///
/// **Not an edit gate.** Measured (§12.8): Z-Image-Turbo reports
/// `ref_images: true` and then segfaults on a reference-image request. The row's
/// `edit` column is what decides whether `/v1/images/edits` may be served; this
/// is only good enough to warn when a row claims *more* than the server admits
/// to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageFeatures {
    pub init_image: bool,
    pub mask_image: bool,
    pub ref_images: bool,
    pub control_image: bool,
    pub ip_adapter_image: bool,
    pub lora: bool,
    pub hires: bool,
    pub vae_tiling: bool,
    pub cache: bool,
    pub cancel_queued: bool,
    pub cancel_generating: bool,
}

/// A named LoRA or upscaler. The server spells these as objects
/// (`{"name": "Latent"}`); a bare string is accepted too, so a build that
/// simplifies the shape does not break the probe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImageAsset {
    pub name: String,
}

impl<'de> Deserialize<'de> for ImageAsset {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Name(String),
            Object {
                #[serde(default)]
                name: String,
            },
        }
        Ok(match Shape::deserialize(d)? {
            Shape::Name(name) | Shape::Object { name } => ImageAsset { name },
        })
    }
}

impl ImageCapabilities {
    /// Parse a `GET /sdcpp/v1/capabilities` body.
    pub fn parse(body: &str) -> Result<Self, String> {
        serde_json::from_str(body).map_err(|e| format!("unreadable capabilities body: {e}"))
    }

    /// Where the row and the running pipeline disagree, in the words the
    /// registry entry's warnings carry (§4: "operator says, probe warns").
    ///
    /// Three disagreements, and each is a real misconfiguration:
    /// * the row claims a mode the pipeline does not support — requests for it
    ///   reach a server that cannot serve them;
    /// * the pipeline supports a mode the row does not claim — `/v1/models`
    ///   under-advertises it, which is only a waste, but a silent one;
    /// * the row is marked `edit` while the pipeline reports no
    ///   reference-image support — the one direction of the `edit` flag this
    ///   probe can speak to (see [`ImageFeatures`]).
    pub fn warnings(&self, m: &ImageModel) -> Vec<String> {
        let mut out = Vec::new();
        let declared = m.modes();
        for mode in &declared {
            if !self.supported_modes.iter().any(|s| s == mode) {
                out.push(format!(
                    "the row declares mode '{mode}', but this pipeline reports [{}]",
                    self.supported_modes.join(", ")
                ));
            }
        }
        for mode in &self.supported_modes {
            if !declared.iter().any(|d| d == mode) {
                out.push(format!(
                    "this pipeline supports mode '{mode}', which the row does not declare — it \
                     is not advertised"
                ));
            }
        }
        if m.edit && !self.features.ref_images {
            out.push(
                "the row is marked as an edit model, but this pipeline reports no \
                 reference-image support"
                    .to_string(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: &str =
        include_str!("../../tests/fixtures/sdcpp/capabilities-z-image-turbo-c678dfe.json");

    fn row(modes: &[&str], edit: bool) -> ImageModel {
        ImageModel {
            id: 1,
            model_id: "z-image-turbo".into(),
            files: Default::default(),
            args: Default::default(),
            modes: modes.iter().map(|s| s.to_string()).collect(),
            edit,
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
        }
    }

    #[test]
    fn the_real_capabilities_body_parses_into_the_fields_wp2_publishes() {
        let caps = ImageCapabilities::parse(CAPS).unwrap();
        assert_eq!(caps.supported_modes, vec!["img_gen"]);
        assert_eq!(caps.current_mode, "img_gen");
        assert_eq!(caps.limits.min_width, Some(64));
        assert_eq!(caps.limits.max_width, Some(4096));
        assert_eq!(caps.limits.max_height, Some(4096));
        assert_eq!(caps.limits.max_batch_count, Some(8));
        assert_eq!(caps.limits.max_queue_size, Some(64));
        assert_eq!(caps.samplers.len(), 21);
        assert_eq!(caps.schedulers.len(), 18);
        assert_eq!(caps.output_formats, vec!["png", "jpeg", "webp"]);
        assert!(caps.loras.is_empty());
        assert_eq!(caps.upscalers.len(), 9);
        assert_eq!(caps.upscalers[0].name, "None");
        assert!(caps.features.ref_images);
        assert!(!caps.features.cancel_generating);
    }

    /// The whole `defaults` / `defaults_by_mode` tree, and anything a newer
    /// build adds, is ignored rather than fatal.
    #[test]
    fn unknown_fields_are_ignored_and_a_bare_string_asset_parses() {
        let caps = ImageCapabilities::parse(
            r#"{"supported_modes":["img_gen"],"upscalers":["Lanczos"],
                "brand_new_field":{"nested":[1,2,3]}}"#,
        )
        .unwrap();
        assert_eq!(caps.upscalers[0].name, "Lanczos");
        assert_eq!(caps.limits.max_width, None);
    }

    #[test]
    fn a_row_that_matches_the_probe_warns_about_nothing() {
        let caps = ImageCapabilities::parse(CAPS).unwrap();
        assert!(caps.warnings(&row(&["img_gen"], false)).is_empty());
        // An empty `modes` column means the default, not "no modes".
        assert!(caps.warnings(&row(&[], false)).is_empty());
    }

    #[test]
    fn a_mode_the_pipeline_does_not_support_is_named() {
        let caps = ImageCapabilities::parse(CAPS).unwrap();
        let w = caps.warnings(&row(&["img_gen", "vid_gen"], false));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("vid_gen") && w[0].contains("img_gen"),
            "{w:?}"
        );
    }

    /// The edit claim is only contradicted when the pipeline reports no
    /// reference-image support at all. Z-Image-Turbo reports `ref_images:
    /// true` and still segfaults on an edit (§12.8), so this warning can
    /// never be the gate — only the row's own `edit` column is.
    #[test]
    fn an_edit_claim_is_named_only_when_the_pipeline_denies_reference_images() {
        let says_yes = ImageCapabilities::parse(CAPS).unwrap();
        assert!(says_yes.warnings(&row(&["img_gen"], true)).is_empty());

        let says_no = ImageCapabilities::parse(
            r#"{"supported_modes":["img_gen"],"features":{"ref_images":false}}"#,
        )
        .unwrap();
        let w = says_no.warnings(&row(&["img_gen"], true));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("edit model"), "{w:?}");
    }

    #[test]
    fn a_mode_the_row_never_declared_is_reported_as_unadvertised() {
        let caps = ImageCapabilities::parse(
            r#"{"supported_modes":["img_gen","vid_gen"],"features":{"ref_images":true}}"#,
        )
        .unwrap();
        let w = caps.warnings(&row(&["img_gen"], false));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("vid_gen") && w[0].contains("not advertised"),
            "{w:?}"
        );
    }

    #[test]
    fn the_two_unconditional_directories_are_created_under_the_models_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        ensure_dirs(&root, &row(&[], false)).unwrap();
        assert!(dir.path().join("loras").is_dir());
        assert!(dir.path().join("upscalers").is_dir());

        // A row that points the flag somewhere else gets *that* directory.
        let mut m = row(&[], false);
        m.files
            .insert("lora_model_dir".into(), "shared/my-loras".into());
        ensure_dirs(&root, &m).unwrap();
        assert!(dir.path().join("shared/my-loras").is_dir());
        assert_eq!(dir_path(&m, "lora_model_dir", "loras"), "shared/my-loras");
    }

    /// A `..` in a directory value is not joined onto the models dir: this is
    /// the one `files` path lmgw resolves **on the host** and then creates, so
    /// a row that got past the save-time check (an older row, a hand-edited
    /// database) would otherwise have `ensure_dirs` make a directory outside
    /// the tree lmgw owns. It falls back to the class default instead.
    #[test]
    fn a_directory_value_that_escapes_the_models_dir_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let mut m = row(&[], false);
        m.files
            .insert("lora_model_dir".into(), "../../tmp/x".into());
        assert_eq!(dir_path(&m, "lora_model_dir", "loras"), "loras");
        ensure_dirs(&root, &m).unwrap();
        assert!(dir.path().join("loras").is_dir());
        assert!(!dir.path().parent().unwrap().join("../tmp/x").exists());

        // And a file value with the same shape resolves to nothing usable, so
        // every existence check the callers make fails.
        assert!(!file_target(&root, "../../etc/passwd").is_file());
        assert_eq!(file_target(&root, "../../etc/passwd"), Path::new(&root));
    }

    /// An unconfigured class must not leave `./loras` behind in whatever
    /// directory lmgw was started from.
    #[test]
    fn an_unset_models_dir_creates_nothing() {
        ensure_dirs("   ", &row(&[], false)).unwrap();
        assert!(!Path::new("loras").exists());
        assert!(!Path::new("upscalers").exists());
    }
}
