//! Moving an aux row's placement and batching flags out of its freeform
//! `args` into the five fields WP1 gave it (decision-models design §3.3,
//! WP2b; §7 Q4 as the owner refined it on 2026-10-12).
//!
//! [`AuxModel::hoist_args`] is pure. What it does with `--n-gpu-layers 0`
//! depends on the row's effective image, and the caller says what that image
//! carries ([`ImageGpu`], read by `backends::images::gpu`). The boot fold that
//! calls it, writes the rows back and logs the notice is
//! [`crate::runtime::lifecycle::hoist_aux_args`].
//!
//! The five flags, in every spelling the argv renderer's alias table knows
//! (`runtime::argv::canonical_key`), long or short, spaced or `=`:
//! - `--threads`/`-t`, `--ubatch-size`/`-ub` and `--batch-size`/`-b` move into
//!   their count fields, whatever the image.
//! - `--mmproj`/`-mm` moves into `mmproj_path`, stored as the chat hoist
//!   stores a projector (`config::hoist_promoted_args_into`): the in-container
//!   `/models/` prefix is dropped and a relative path is taken as written.
//!   Nothing here looks at the disk. A path that is not a file there (a
//!   directory, a relative path written for llama-server's working directory)
//!   moves as written, and the row's finding reports it
//!   (`mmproj_path '…' is not a file under the aux models dir`).
//! - `--n-gpu-layers 0` (`-ngl 0`, `--gpu-layers 0`) becomes `backend: cpu`
//!   only when the image has no GPU backend ([`ImageGpu::CpuOnly`]). On a GPU
//!   image, and on one lmgw could not read, the flag stays in `args` and the
//!   row stays a GPU row: a GPU build that sees the device still computes on
//!   the card at `-ngl 0` (§3.3's measurement), so turning that row into a
//!   CPU row is the owner's choice, and the notice names the trade.
//!
//! A value no field can hold stays in `args` exactly as written: a count
//! that is not a positive integer (the columns' CHECKs), a projector outside
//! `/models` or one that climbs out of it (the op refuses such a path in the
//! field, `ops::check_rel_path`), a flag without a value, `-ngl` at anything
//! but 0. A flag whose field is already set is dropped: the renderer claims
//! the key, so the copy renders nothing today, and the notice says so.
//!
//! Idempotent: a second pass finds nothing left to move and keeps what it
//! kept. A row with nothing to move keeps its `args` untouched, so its argv
//! stays byte-identical and boot adopts its container.

use std::path::{Component, Path};

use crate::runtime::argv::{canonical_key, is_opt};

use super::{AuxBackend, AuxModel};

/// What a row's effective image carries for computing on a GPU
/// (decision-models design §3.3, WP2b). Each variant says how that is known,
/// for the notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageGpu {
    /// A GPU backend, named as the image names it (`cuda`, `vulkan`, …).
    Gpu(String),
    /// No GPU backend; how that is known (its label, the libraries in it).
    CpuOnly(String),
    /// lmgw could not tell; why (not on this machine, podman failed, its
    /// files do not say).
    Unknown(String),
}

/// What [`AuxModel::hoist_args`] did to one row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuxHoist {
    /// Each flag moved into a field: as it was written in `args`
    /// (`--threads 8`), and the field it set (`threads 8`).
    pub moved: Vec<(String, String)>,
    /// Each flag dropped because its field was already set: as written, and
    /// that field with its value.
    pub dropped: Vec<(String, String)>,
    /// A `--n-gpu-layers 0` left in `args` because the image has a GPU
    /// backend or could not be read, as written.
    pub kept_cpu_flag: Option<String>,
    /// A projector that moved from a relative `--mmproj`, which llama-server
    /// resolved against its working directory, and which the field renders
    /// onto `/models` instead.
    pub reread_projector: Option<String>,
}

/// What the boot fold knows of the row's container that the rule does not,
/// for the notice ([`AuxHoist::notice`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoistedContainer {
    /// The command or the run args the row renders differ from what it
    /// rendered before the hoist (the two things boot adoption compares), so
    /// a container of it still running from before is replaced, not adopted.
    pub replaced: bool,
    /// The run args the row starts with now hand its container a GPU
    /// (`passes_gpu`).
    pub gets_gpu: bool,
}

impl AuxHoist {
    /// Whether the row's `args` or fields changed, so it must be written
    /// back.
    pub fn changed(&self) -> bool {
        !self.moved.is_empty() || !self.dropped.is_empty()
    }

    /// Whether the move set `backend: cpu`.
    fn moved_backend(&self) -> bool {
        self.moved.iter().any(|(_, f)| f.starts_with("backend"))
    }

    /// The start notice for one row (§3.3, WP2b): what moved, what it means
    /// for the row's placement and its container, and why a kept
    /// `--n-gpu-layers 0` stays. `None` when nothing moved and nothing was
    /// kept. `image` is the row's effective image, `gpu` what was read of it,
    /// `container` what the fold rendered for it.
    pub fn notice(
        &self,
        model_id: &str,
        image: &str,
        gpu: &ImageGpu,
        container: HoistedContainer,
    ) -> Option<String> {
        if !self.changed() && self.kept_cpu_flag.is_none() {
            return None;
        }
        let mut out = format!("aux model '{model_id}'");
        if self.changed() {
            let mut parts = Vec::new();
            if !self.moved.is_empty() {
                parts.push(format!(
                    "moved {} from its extra args into its fields ({})",
                    written(&self.moved),
                    fields(&self.moved)
                ));
            }
            if !self.dropped.is_empty() {
                parts.push(format!(
                    "dropped {} from its extra args, which rendered nothing because its fields \
                     already set them ({})",
                    written(&self.dropped),
                    fields(&self.dropped)
                ));
            }
            out.push_str(&format!(": {}.", parts.join("; ")));
            if let Some(rel) = &self.reread_projector {
                out.push_str(&format!(
                    " Its projector `{rel}` was read against llama-server's working directory \
                     and is now read as /models/{rel}."
                ));
            }
            if self.moved_backend() {
                let how = match gpu {
                    ImageGpu::CpuOnly(how) => how.as_str(),
                    _ => "",
                };
                out.push_str(&format!(
                    " Its image '{image}' has no GPU backend ({how}), so the row now runs on the \
                     CPU: it is charged no VRAM and serves under the GPU hold"
                ));
                out.push_str(if container.gets_gpu {
                    ", but its own run args still hand its container a GPU, so llama-server can \
                     still open a CUDA context there: remove those flags, or clear the override \
                     to inherit the class args without them."
                } else {
                    ", and its container gets no GPU device."
                });
                out.push_str(
                    " It stays on the CPU if its image later changes to a GPU build: set backend \
                     back to empty to return it to the GPU.",
                );
            }
            out.push_str(if container.replaced {
                " What its container runs changes, so a container of it still running from \
                 before is replaced on this start rather than adopted."
            } else {
                " What its container runs is unchanged, so a container of it still running \
                 from before is adopted as it is."
            });
        }
        if let Some(flag) = &self.kept_cpu_flag {
            let subject = if self.changed() { " It" } else { "" };
            out.push_str(&format!(
                "{subject} keeps `{flag}` in its extra args and stays a GPU row: "
            ));
            match gpu {
                ImageGpu::Gpu(backend) if container.gets_gpu => out.push_str(&format!(
                    "its image '{image}' has a GPU backend ({backend}), and a GPU build that sees \
                     the device still computes on the card at -ngl 0. {MEASURED} Setting backend: \
                     cpu is a deliberate choice: the row then frees the card and serves under the \
                     GPU hold, at CPU batch speed."
                )),
                ImageGpu::Gpu(backend) => out.push_str(&format!(
                    "its image '{image}' has a GPU backend ({backend}), but its run args pass its \
                     container no GPU device, so it computes on the CPU already, unless the host \
                     hands it one anyway (a legacy NVIDIA hook reading the image's own \
                     NVIDIA_VISIBLE_DEVICES). Set backend: cpu so the GPU hold treats it as the \
                     CPU row it is: it then serves under the hold."
                )),
                ImageGpu::Unknown(why) => out.push_str(&format!(
                    "lmgw cannot tell whether its image '{image}' has a GPU backend ({why}), and \
                     the flag only becomes backend: cpu on an image without one. The next start \
                     looks again; set backend: cpu to run it on the CPU now."
                )),
                // Unreachable: on such an image the flag moved.
                ImageGpu::CpuOnly(_) => out.push_str("its image has no GPU backend."),
            }
        }
        Some(out)
    }
}

/// §3.3's measurement (2026-10-12): Qwen3-Embedding-4B Q4_K_M, pooling
/// `last`, `-c 8192 -ub 8192 -b 8192 --threads 8 -ngl 0`, the CUDA image at
/// b11549 with and without the GPU device, a batch of 32 inputs of ~200
/// tokens each.
const MEASURED: &str = "Measured on a 4B embedder at ubatch 8192: with the device, ~5.7 GiB of \
     VRAM for compute buffers and 2.2 s per batch of 32 inputs; without it (backend: cpu), 0 VRAM \
     and 50.6 s.";

/// `` `--threads 8`, `-ub 4096` ``.
fn written(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(w, _)| format!("`{w}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `threads 8, ubatch_size 4096`.
fn fields(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(_, f)| f.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The five flags, by the long name `canonical_key` gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Threads,
    Ubatch,
    Batch,
    Mmproj,
    GpuLayers,
}

impl Slot {
    fn of(key: &str) -> Option<Self> {
        match key {
            "threads" => Some(Self::Threads),
            "ubatch-size" => Some(Self::Ubatch),
            "batch-size" => Some(Self::Batch),
            "mmproj" => Some(Self::Mmproj),
            "n-gpu-layers" => Some(Self::GpuLayers),
            _ => None,
        }
    }
}

/// What became of one flag.
enum Took {
    /// Into its field, which now reads as given.
    Moved(String),
    /// Dropped: its field was set already, and reads as given.
    Dropped(String),
    /// Left in `args` as written.
    Kept,
}

/// A count a field can hold: a positive integer (the columns' CHECKs).
fn count(value: Option<&str>) -> Option<i64> {
    value?.trim().parse::<i64>().ok().filter(|n| *n > 0)
}

/// `--n-gpu-layers 0`'s value: no layer on the GPU.
fn zero(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().parse::<i64>() == Ok(0))
}

/// The projector path a field holds for `value`, relative to the models dir
/// like `gguf_path`: the in-container `/models/` prefix dropped, as the chat
/// hoist and the op do (`ops::strip_models_prefix`). `None` for one that has
/// no such form: empty, absolute outside `/models`, or climbing out of it.
fn projector(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    if v.starts_with('/') && !v.starts_with("/models/") {
        return None;
    }
    let rel = v.trim_start_matches("/models/").trim_start_matches('/');
    let plain = Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (!rel.is_empty() && plain).then(|| rel.to_string())
}

impl AuxModel {
    /// Move the five flags of decision-models design §3.3 out of
    /// [`Self::args`] into their fields, `--n-gpu-layers 0` only when
    /// `image` has no GPU backend (WP2b; the module doc has the rules), and
    /// say what moved. Idempotent; a row with nothing to move is left
    /// exactly as it was.
    pub fn hoist_args(&mut self, image: &ImageGpu) -> AuxHoist {
        let mut report = AuxHoist::default();
        let mut kept: Vec<String> = Vec::with_capacity(self.args.len());
        let args = std::mem::take(&mut self.args);
        let mut i = 0;
        while i < args.len() {
            let raw = &args[i];
            // `--flag=value` is one token; split it so that spelling moves
            // like the spaced one.
            let (tok, inline) = match raw.split_once('=') {
                Some((f, v)) if f.starts_with('-') => (f, Some(v)),
                _ => (raw.as_str(), None),
            };
            let slot = tok
                .starts_with('-')
                .then(|| Slot::of(canonical_key(tok.trim_start_matches('-'))))
                .flatten();
            let Some(slot) = slot else {
                kept.push(raw.clone());
                i += 1;
                continue;
            };
            // A value belongs to the flag only if it is not itself a flag —
            // the rule the renderer's `push_freeform_args` reads it by.
            let (value, consumed) = match inline {
                Some(v) => (Some(v), 1),
                None => match args.get(i + 1) {
                    Some(v) if !is_opt(v) => (Some(v.as_str()), 2),
                    _ => (None, 1),
                },
            };
            let as_written = args[i..i + consumed].join(" ");
            match self.take(slot, value, image) {
                Took::Moved(field) => {
                    // A relative projector in the args was read against
                    // llama-server's working directory; the field's is
                    // read under `/models`.
                    if slot == Slot::Mmproj && value.is_some_and(|v| !v.trim().starts_with('/')) {
                        report.reread_projector = self.mmproj_path.clone();
                    }
                    report.moved.push((as_written, field));
                }
                Took::Dropped(field) => report.dropped.push((as_written, field)),
                Took::Kept => {
                    if slot == Slot::GpuLayers && zero(value) {
                        report.kept_cpu_flag = Some(as_written);
                    }
                    kept.extend(args[i..i + consumed].iter().cloned());
                }
            }
            i += consumed;
        }
        self.args = kept;
        report
    }

    /// Whether [`Self::hoist_args`] would read the image for this row: its
    /// `args` carry `--n-gpu-layers 0` and its `backend` is unset. Only those
    /// rows' images are inspected.
    pub fn hoist_reads_image(&self) -> bool {
        self.clone()
            .hoist_args(&ImageGpu::Unknown(String::new()))
            .kept_cpu_flag
            .is_some()
    }

    /// One flag's fate (see [`Self::hoist_args`]).
    fn take(&mut self, slot: Slot, value: Option<&str>, image: &ImageGpu) -> Took {
        let counted = |field: &mut Option<i64>, name: &str| match *field {
            Some(n) => Took::Dropped(format!("{name} {n}")),
            None => match count(value) {
                Some(n) => {
                    *field = Some(n);
                    Took::Moved(format!("{name} {n}"))
                }
                None => Took::Kept,
            },
        };
        match slot {
            Slot::Threads => counted(&mut self.threads, "threads"),
            Slot::Ubatch => counted(&mut self.ubatch_size, "ubatch_size"),
            Slot::Batch => counted(&mut self.batch_size, "batch_size"),
            Slot::Mmproj => match &self.mmproj_path {
                Some(p) => Took::Dropped(format!("mmproj_path {p}")),
                None => match projector(value) {
                    Some(rel) => {
                        let field = format!("mmproj_path {rel}");
                        self.mmproj_path = Some(rel);
                        Took::Moved(field)
                    }
                    None => Took::Kept,
                },
            },
            Slot::GpuLayers => match self.backend {
                Some(b) => Took::Dropped(format!("backend {}", b.as_str())),
                None if zero(value) => match image {
                    ImageGpu::CpuOnly(_) => {
                        self.backend = Some(AuxBackend::Cpu);
                        Took::Moved("backend cpu".into())
                    }
                    ImageGpu::Gpu(_) | ImageGpu::Unknown(_) => Took::Kept,
                },
                // Any other layer count is GPU placement, which an unset
                // `backend` already is; it stays as the owner wrote it.
                None => Took::Kept,
            },
        }
    }
}
