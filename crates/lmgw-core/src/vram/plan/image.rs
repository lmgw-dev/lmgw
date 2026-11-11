//! An image pipeline's footprint (image-generation design §9): the files it
//! loads, and — for a row that can run with less than all of it on the card —
//! what it needs there when the whole does not fit.
//!
//! **The whole first.** Every pipeline is charged the files it loads, as it
//! always was (`total_bytes`): that is what admission makes room for, evicts
//! for and reserves whenever it fits, the learned peak kept free on top once
//! the pipeline is up. Only when the whole does not fit in what can be made
//! free does admission fall back to the partial figures below
//! ([`crate::vram::partial`]).
//!
//! **What lets it run with less.** sd-server places weights off the card in
//! three ways ([`Adapts`]), all of which lmgw reads from the row's args:
//!
//! - `offload_to_cpu` "places the weights in RAM to save VRAM, and
//!   automatically loads them into VRAM when needed", one module (diffusion
//!   model, text encoder, VAE) at a time;
//! - a `params_backend` that names `cpu` or `disk`, for every module or for
//!   some of them (`diffusion=disk,clip=cpu`);
//! - sd.cpp's `--auto-fit`, `on` by default in the builds lmgw pins and
//!   turned off by `auto_fit off` or by any explicit `params_backend`: it
//!   places the weights "on the compute GPU, RAM, another GPU, or disk in that
//!   order, according to available memory (`--max-vram` limits GPU budgets)".
//!
//! A row with none of these — `auto_fit off`, or a `params_backend` that
//! names only GPU devices — pins its weights on the card ([`Pinned`]), and
//! one larger than the card is refused.
//!
//! **The on-card part** ([`PartialFit::on_card_bytes`]): what such a pipeline
//! is given on the card when the whole does not fit and enough is free.
//! - A row whose weights all wait off the card (`offload_to_cpu`, a whole
//!   `params_backend cpu|disk`): its context plus its learned peak — the peak
//!   is what one generation took *above* the idle figure
//!   ([`crate::vram::peak`]). Before the peak, its largest file plus its
//!   context: the most one module puts on the card when sd.cpp loads it
//!   whole. That is not an upper bound — measured with `offload_to_cpu` on a
//!   24 GiB card, Z-Image-Turbo's largest file is 3.6 GiB and its peak
//!   7.7 GiB (`image_recipes`) — which is why the whole is charged wherever
//!   it fits, and why sd-server is held to what admission gave it
//!   (`--max-vram`, [`crate::vram::partial`]).
//! - Every other adapting row (auto-fit, a per-module `params_backend`): all
//!   of it. Less than the whole is then whatever is free.
//!
//! **The context** ([`PartialFit::context_bytes`]): sd-server's own memory
//! outside the budget `--max-vram` sets — the row's learned idle residency
//! (`ImageModel::idle_bytes`), read per process while a container of a row
//! whose weights all wait off the card was up and idle. No other row has it
//! measured yet: its idle residency holds whatever weights auto-fit put on
//! the card. Where it is not measured, [`CONTEXT_STAND_IN`] is set aside for
//! it, and every sentence that uses it says so.
//!
//! **The working budget** ([`PartialFit::work_bytes`]): the smallest
//! `--max-vram` a generation of this row has completed under, raised where
//! one failed (`ImageModel::work_bytes`, migration 0104, learned by
//! [`crate::vram::partial::work`]). The context is no working budget: on the
//! Windows test card's sd-server's Vulkan context is 5 MiB at rest, and Z-Image-Turbo
//! fails its first segment at `--max-vram 0.5` (it asks for 942 MiB) and
//! renders at 1 (measured 2026-11-11, per-model containers §4).

use serde_json::{Map, Value};

use super::{dir_size, file_size, Footprint};

/// How a row can run with less than all of it on the card (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Adapts {
    /// `offload_to_cpu` set.
    ToCpu,
    /// A `params_backend` naming `cpu` or `disk` for at least one module, as
    /// the row wrote it.
    Params(String),
    /// sd.cpp's `--auto-fit`, on by default and not turned off.
    AutoFit,
}

impl Adapts {
    /// The row's setting, as the owner wrote it.
    pub(crate) fn setting(&self) -> String {
        match self {
            Adapts::ToCpu => "offload_to_cpu".into(),
            Adapts::Params(b) => format!("params_backend {b}"),
            Adapts::AutoFit => "sd.cpp's auto-fit (on unless auto_fit off)".into(),
        }
    }

    /// What the setting does with the weights, as a clause.
    pub(crate) fn does(&self) -> &'static str {
        match self {
            Adapts::ToCpu => {
                "keeps its weights in RAM and loads each module onto the card when it is needed"
            }
            Adapts::Params(_) => "keeps those modules' weights in RAM or on disk",
            Adapts::AutoFit => "puts in RAM what does not fit on the card",
        }
    }

    /// Every module's weights wait off the card between uses, so what its
    /// container holds at rest is sd-server's own context (module doc).
    pub(crate) fn all_off(&self) -> bool {
        match self {
            Adapts::ToCpu => true,
            Adapts::Params(s) => {
                let parts: Vec<(Option<&str>, &str)> = assignments(s).collect();
                !parts.is_empty()
                    && parts.iter().all(|(module, value)| {
                        module.is_none_or(|m| matches!(m, "*" | "all" | "default"))
                            && off_card(value)
                    })
            }
            Adapts::AutoFit => false,
        }
    }
}

/// How a row pins all of its weights on the card (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pinned {
    /// `auto_fit off`, and nothing else keeps a module off the card.
    AutoFitOff,
    /// A `params_backend` naming GPU devices only, which also turns sd.cpp's
    /// auto-fit off.
    Params(String),
}

impl Pinned {
    /// The setting that pins it, as the owner wrote it.
    pub(crate) fn setting(&self) -> String {
        match self {
            Pinned::AutoFitOff => "auto_fit off".into(),
            Pinned::Params(b) => format!("params_backend {b} (which turns sd.cpp's auto-fit off)"),
        }
    }
}

/// `module=value` pairs of a backend assignment, `None` for a bare value.
fn assignments(spec: &str) -> impl Iterator<Item = (Option<&str>, &str)> {
    spec.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((m, v)) => (Some(m.trim()), v.trim()),
            None => (None, p),
        })
}

fn off_card(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "cpu" | "disk")
}

/// How `args` place the pipeline's weights: what lets it run with less on
/// the card, or what pins it there. Keys are read under every spelling
/// ([`crate::sdcpp_caps::canonical_key`]); the rules are sd.cpp's own
/// (module doc): `offload_to_cpu` is a `params_backend` of `*=cpu`, and any
/// explicit `params_backend` turns auto-fit off.
pub(crate) fn placement(args: &Map<String, Value>) -> Result<Adapts, Pinned> {
    let mut to_cpu = false;
    let mut params: Option<String> = None;
    let mut auto_fit_off = false;
    for (k, v) in args {
        match crate::sdcpp_caps::canonical_key(k).as_str() {
            "offload_to_cpu" => to_cpu |= v.as_bool() == Some(true),
            "params_backend" => {
                params = v
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            }
            "auto_fit" => {
                auto_fit_off = v
                    .as_str()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("off"))
            }
            _ => {}
        }
    }
    if to_cpu {
        return Ok(Adapts::ToCpu);
    }
    match params {
        Some(p) if assignments(&p).any(|(_, v)| off_card(v)) => Ok(Adapts::Params(p)),
        Some(p) => Err(Pinned::Params(p)),
        None if auto_fit_off => Err(Pinned::AutoFitOff),
        None => Ok(Adapts::AutoFit),
    }
}

/// What sd-server is set aside for its own context where the row's is not
/// measured (module doc): the most a context has been measured holding at
/// rest — 1.0 GiB, CUDA, `offload_to_cpu`, Z-Image-Turbo on a 24 GiB card
/// (image-generation §12.4, the recipe's note). Vulkan's on a GTX 970 test card was 5 MiB (2026-11-11, sd.cpp `master-1079-a3c49b1`). A recorded
/// measurement, not a guess, and named wherever it is used.
pub const CONTEXT_STAND_IN: u64 = 1024 * 1024 * 1024;

/// What a pipeline that can run with less needs on the card when the whole
/// does not fit (module doc). The whole is the footprint's `total_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialFit {
    pub(crate) how: Adapts,
    /// What it is given on the card when the whole does not fit and this
    /// much is free: its context (or [`CONTEXT_STAND_IN`]) included for a row
    /// whose weights all wait off the card.
    pub on_card_bytes: u64,
    /// What `on_card_bytes` stands for, as a parenthesis: "its largest file
    /// and its context", "its context and learned peak", "all of it".
    pub basis: String,
    /// sd-server's own context, outside its `--max-vram` budget: the row's
    /// learned idle residency, for a row whose weights all wait off the card.
    /// `None`: not measured yet ([`Self::context`] sets the stand-in aside).
    pub context_bytes: Option<u64>,
    /// The smallest `--max-vram` budget a generation of this row has
    /// completed under. `None`: not measured yet.
    pub work_bytes: Option<u64>,
    /// The row's own `max_vram`, which wins over any grant at render
    /// ([`crate::vram::partial::own_budget`]): `Some(Some(bytes))` a budget,
    /// `Some(None)` none at all (0 or less follows the driver's free memory),
    /// `None` where the row sets none.
    pub own_budget: Option<Option<u64>>,
}

impl PartialFit {
    /// What is set aside for sd-server's context: the measured one, else
    /// [`CONTEXT_STAND_IN`].
    pub fn context(&self) -> u64 {
        self.context_bytes.unwrap_or(CONTEXT_STAND_IN)
    }
}

/// What one file of a pipeline is: its key, its bytes.
pub(super) struct Part {
    pub key: String,
    pub bytes: u64,
}

/// Blocking: the bytes of each of `files` under `dir` (a directory key as its
/// tree), and the ones not there.
pub(super) fn read(dir: &str, files: &[(String, String)]) -> (Vec<Part>, Vec<String>) {
    let mut parts = Vec::new();
    let mut missing = Vec::new();
    for (key, rel) in files {
        let path = crate::runtime::image::file_target(dir, rel);
        let size = if crate::runtime::image::is_dir_key(key) {
            dir_size(&path)
        } else {
            file_size(&path)
        };
        if size == 0 && !path.exists() {
            missing.push(format!("{key} '{rel}'"));
        }
        parts.push(Part {
            key: key.clone(),
            bytes: size,
        });
    }
    (parts, missing)
}

/// The learned figures of a row: its peak above idle, its idle residency
/// and the smallest budget a generation completed under — as they count for
/// one engine ([`learned_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Learned {
    pub peak: Option<u64>,
    pub idle: Option<u64>,
    pub work: Option<u64>,
}

/// The engine an image row's learned figures belong to (migration 0105): the
/// first 12 hex digits of the SHA-256 of the resolved engine image, run args
/// and native build (`build`, the native launcher's choice; `None` where the
/// image names the build). A CUDA build's context is not a Vulkan build's
/// (1.0 GiB against 5 MiB at rest), and compute buffers move with the engine
/// and the card the run args pass, so a figure belongs to all three.
pub fn engine_key(image: &str, run_args: &[String], build: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let doc = format!(
        "{image}\n{}\n{}",
        serde_json::to_string(run_args).unwrap_or_default(),
        build.unwrap_or("")
    );
    hex::encode(&Sha256::digest(doc.as_bytes())[..6])
}

/// `m`'s learned figures as they count under engine `key`: all of them where
/// they were learned under it, none where they were learned under another
/// engine or before the key existed — not measured, so admission charges
/// conservatively until they are learned again.
pub(crate) fn learned_for(m: &crate::config::ImageModel, key: &str) -> Learned {
    if m.learned_key.as_deref() != Some(key) {
        return Learned::default();
    }
    Learned {
        peak: m.peak_extra_bytes,
        idle: m.idle_bytes,
        work: m.work_bytes,
    }
}

/// The footprint of a pipeline whose files are `parts` and which can run
/// with less on the card by `how` (module doc), with `note` the on-disk
/// sentence every image footprint carries. Charged the whole; the partial
/// figures go beside it.
pub(super) fn adapting(
    parts: &[Part],
    how: Adapts,
    learned: Learned,
    own_budget: Option<Option<u64>>,
    note: String,
) -> Footprint {
    let full = parts.iter().fold(0u64, |s, p| s.saturating_add(p.bytes));
    let all_off = how.all_off();
    let largest = parts
        .iter()
        .filter(|p| !crate::runtime::image::is_dir_key(&p.key))
        .max_by_key(|p| p.bytes);
    let idle = learned.idle.filter(|_| all_off);
    let context = idle.unwrap_or(CONTEXT_STAND_IN);
    let context_named = match idle {
        Some(_) => "its context".to_string(),
        None => format!(
            "{} set aside for its context, not measured yet",
            crate::hf::fmt_bytes(CONTEXT_STAND_IN)
        ),
    };
    let (on_card, basis) = match (all_off, learned.peak, largest) {
        (false, _, _) => (full, "all of it".to_string()),
        (true, Some(p), _) => (
            context.saturating_add(p),
            format!("its learned peak and {context_named}"),
        ),
        (true, None, Some(l)) => (
            l.bytes.saturating_add(context),
            format!("its largest file, {}, and {context_named}", l.key),
        ),
        (true, None, None) => (
            context,
            format!("nothing but {context_named}: it names no file"),
        ),
    };
    let least = match learned.work {
        Some(w) => format!(
            "; the least it has rendered with is a {} budget (--max-vram)",
            crate::hf::fmt_bytes(w)
        ),
        None => "; how little it can run with is not measured yet".into(),
    };
    let sentence = format!(
        "{} {}: where its {} of files do not fit, it gets {} on the card ({basis}) or what is \
         free{least}",
        how.setting(),
        how.does(),
        crate::hf::fmt_bytes(full),
        crate::hf::fmt_bytes(on_card),
    );
    let mut fp = Footprint::new(full, 0, None, Some(format!("{note}. {sentence}")));
    fp.partial = Some(PartialFit {
        how,
        on_card_bytes: on_card,
        basis,
        context_bytes: idle,
        work_bytes: learned.work,
        own_budget,
    });
    fp
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MIB: u64 = 1024 * 1024;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn placement_follows_sdcpp_rules() {
        assert_eq!(
            placement(&args(json!({"offload_to_cpu": true}))),
            Ok(Adapts::ToCpu)
        );
        assert_eq!(
            placement(&args(
                json!({"offload-to-cpu": true, "params_backend": "cuda0"})
            )),
            Ok(Adapts::ToCpu),
            "offload_to_cpu is a params_backend of *=cpu in front of the row's"
        );
        assert_eq!(
            placement(&args(json!({"offload_to_cpu": false}))),
            Ok(Adapts::AutoFit),
            "auto-fit is on by default"
        );
        assert_eq!(placement(&args(json!({"steps": 8}))), Ok(Adapts::AutoFit));
        assert_eq!(
            placement(&args(json!({"auto_fit": "on"}))),
            Ok(Adapts::AutoFit)
        );
        assert_eq!(
            placement(&args(json!({"auto-fit": "OFF"}))),
            Err(Pinned::AutoFitOff)
        );
        assert_eq!(
            placement(&args(json!({"params_backend": "disk"}))),
            Ok(Adapts::Params("disk".into()))
        );
        assert_eq!(
            placement(&args(json!({"params_backend": "diffusion=disk,clip=cpu"}))),
            Ok(Adapts::Params("diffusion=disk,clip=cpu".into())),
            "a per-module assignment counts"
        );
        assert_eq!(
            placement(&args(json!({"params_backend": "cuda0"}))),
            Err(Pinned::Params("cuda0".into())),
            "an explicit params_backend on the GPU turns auto-fit off"
        );
        assert_eq!(
            placement(&args(
                json!({"params_backend": "diffusion=cuda0,vae=vulkan0"})
            )),
            Err(Pinned::Params("diffusion=cuda0,vae=vulkan0".into()))
        );
    }

    #[test]
    fn all_off_is_offload_or_a_whole_off_card_params_backend() {
        assert!(Adapts::ToCpu.all_off());
        assert!(Adapts::Params("CPU ".into()).all_off());
        assert!(Adapts::Params("*=disk".into()).all_off());
        assert!(!Adapts::Params("diffusion=disk,clip=cpu".into()).all_off());
        assert!(!Adapts::Params("diffusion=disk,vae=cuda0".into()).all_off());
        assert!(!Adapts::AutoFit.all_off());
    }

    fn parts() -> [Part; 4] {
        [
            Part {
                key: "diffusion_model".into(),
                bytes: 3685 * MIB,
            },
            Part {
                key: "llm".into(),
                bytes: 2381 * MIB,
            },
            Part {
                key: "vae".into(),
                bytes: 320 * MIB,
            },
            Part {
                key: "lora_model_dir".into(),
                bytes: 9000 * MIB,
            },
        ]
    }

    #[test]
    fn an_adapting_pipeline_is_charged_the_whole_with_its_partial_figures_beside() {
        let full = (3685 + 2381 + 320 + 9000) * MIB;
        let fp = adapting(
            &parts(),
            Adapts::ToCpu,
            Learned::default(),
            None,
            "on disk".into(),
        );
        assert_eq!(fp.total_bytes, full, "the whole, as every pipeline");
        let p = fp.partial.clone().unwrap();
        assert_eq!(
            p.on_card_bytes,
            3685 * MIB + CONTEXT_STAND_IN,
            "the largest file, not the LoRA dir, and the stand-in for its context"
        );
        assert_eq!((p.context_bytes, p.work_bytes), (None, None));
        assert_eq!(p.context(), CONTEXT_STAND_IN);
        let note = fp.note.unwrap();
        assert!(
            note.contains("offload_to_cpu keeps its weights in RAM"),
            "{note}"
        );
        assert!(
            note.contains(
                "its largest file, diffusion_model, and 1.0 GiB set aside for its context, not \
                 measured yet"
            ),
            "{note}"
        );
        assert!(
            note.contains("how little it can run with is not measured yet"),
            "{note}"
        );

        // Learned: the context, the peak above it and a working budget.
        let learned = Learned {
            peak: Some(6 * 1024 * MIB),
            idle: Some(512 * MIB),
            work: Some(1024 * MIB),
        };
        let fp = adapting(
            &parts(),
            Adapts::Params("disk".into()),
            learned,
            None,
            "x".into(),
        );
        let p = fp.partial.unwrap();
        assert_eq!(
            p.on_card_bytes,
            6 * 1024 * MIB + 512 * MIB,
            "context + peak"
        );
        assert_eq!(p.context_bytes, Some(512 * MIB));
        assert_eq!(p.work_bytes, Some(1024 * MIB));
        let note = fp.note.unwrap();
        assert!(
            note.contains("(its learned peak and its context)")
                && note.contains("the least it has rendered with is a 1.0 GiB budget"),
            "{note}"
        );

        // A peak before the context: the stand-in beside it, never less.
        let fp = adapting(
            &parts(),
            Adapts::ToCpu,
            Learned {
                peak: Some(6 * 1024 * MIB),
                ..Learned::default()
            },
            None,
            "x".into(),
        );
        assert_eq!(
            fp.partial.unwrap().on_card_bytes,
            6 * 1024 * MIB + CONTEXT_STAND_IN
        );

        // Auto-fit: all of it, and no context even with an idle reading.
        let fp = adapting(&parts(), Adapts::AutoFit, learned, None, "x".into());
        let p = fp.partial.unwrap();
        assert_eq!(p.on_card_bytes, full);
        assert_eq!(p.context_bytes, None);
        assert_eq!(
            p.work_bytes,
            Some(1024 * MIB),
            "a budget learned under a grant"
        );
    }
}
