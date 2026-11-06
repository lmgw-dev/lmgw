//! The plan's one input (find-a-model design §2.3, WP2): the weights'
//! header, every part's bytes, the companions the plan picks beside them
//! with their headers, and where it all came from, read from the disk or
//! from Hugging Face before anything is downloaded ([`hub`]).
//!
//! Before WP2 the chat and aux plans read the disk themselves at several
//! places: the sibling scan and its headers, the fit's sizing of the files
//! and the drafter's header, the split parts' check, and the aux location
//! rule's look into the aux dir. They all happen here now, once, and
//! nothing after [`PlanFiles`] touches the disk, so a plan from the hub and
//! a plan from the disk of the same bytes are the same plan (§10).
//!
//! **The companions.** Beside the weights, in the same directory, the plan
//! takes the projector the one rule picks (D121) and the first drafter. On
//! the disk a sibling's role is its header's ([`super::role_of`]), so every
//! sibling's header is read, as before. On the hub a role is the name's
//! (`classify_repo_file`), and only the picked files' headers are read:
//! reading every sibling's would cost 1 to 16 MiB a file (§2.2). The two
//! agree for every repo the design's spike listed; where a name lies about
//! a file, the hub plan says so in its warnings rather than plan it as the
//! name says (draft call D52).
//!
//! **The whole model** (D7, D131). A split file's parts each hold some of
//! its tensors, so the parameter count and the repeating layers' width are
//! the parts' added up: every later part's header is read too (16 KB each
//! on the hub, §2.2), from the disk or the hub alike.

use crate::gguf::{self, params, ModelSummary};
use crate::runtime::Class;

pub mod hub;

/// A companion of the weights: the projector or the drafter.
#[derive(Debug, Clone)]
pub struct Companion {
    /// Its path as a row names it: relative to the class's models dir, which
    /// for a hub plan is where the download puts it ([`crate::hf::dest_rel_path`]).
    pub path: String,
    /// Every part's bytes, as admission charges them.
    pub bytes: u64,
    /// The sentence for split parts that aren't there, when some aren't.
    pub missing: Option<String>,
    pub summary: ModelSummary,
}

/// Where a plan's files are.
#[derive(Debug, Clone)]
pub enum Source {
    /// In a class's models dir on this machine ([`PlanFiles::found_in`]).
    Disk,
    /// On Hugging Face, read before any download.
    Hub {
        repo: String,
        /// The weights' path in the repo.
        file: String,
        oid: Option<String>,
        commit: Option<String>,
        /// The header bytes this plan fetched, every file's; a header the
        /// process had read before (cached by its hash, D4) adds nothing
        /// (D123).
        header_bytes_read: u64,
        /// What this plan's header reads took, every file's: seconds from
        /// the first byte to the last one read (D23's header-read rate).
        read_secs: f64,
    },
}

/// The plan's one input (module doc).
#[derive(Debug, Clone)]
pub struct PlanFiles {
    /// The weights' path as a row names it (the first part of a split file).
    pub rel: String,
    /// The weights' header (the first part's, of a split file).
    pub summary: ModelSummary,
    /// Every part's bytes ([`crate::vram::plan::split`]).
    pub weights_bytes: u64,
    /// The sentence for split parts that aren't there, when some aren't.
    pub weights_missing: Option<String>,
    /// The projector the one rule picks among the siblings (D121).
    pub projector: Option<Companion>,
    /// The first sibling drafter (D41).
    pub drafter: Option<Companion>,
    /// The model's parameters, every part's (D7, D131); `None` when a part
    /// couldn't be read.
    pub parameters: Option<u64>,
    /// Bits per weight over the repeating layers, every part's (D7, D131).
    pub layer_bits: Option<f64>,
    /// `layer_bits` is the first part's alone: a later part couldn't be read,
    /// and for a quant that mixes widths by layer the first part's width
    /// need not be the model's (D131), so the fit says it is an estimate.
    pub layer_bits_estimated: bool,
    /// The elements `layer_bits` is taken over, every part's: the repeating
    /// layers' quantized matrices, the same in every quant of the model. A
    /// file list bounds another file's width from them (D62); `None` when
    /// `layer_bits` is the first part's alone, or there is none.
    pub layer_elements: Option<u64>,
    /// The elements of the input embeddings llama.cpp keeps on the host,
    /// every part's ([`crate::gguf::InputTensors::host_elements`]): the same
    /// in every quant of the model, so a file list bounds another file's
    /// host-kept bytes from them (D61). `None` when a count isn't known; the
    /// bound is then the file's bytes.
    pub host_input_elements: Option<u64>,
    /// Every part's header bytes and the most alignment padding their
    /// tensors carry, as far as the parts were read: what a file's bytes
    /// hold beside its tensors (D62's cap side).
    pub overhead_bytes: u64,
    /// What the plan's warnings add: a companion whose header couldn't be
    /// read from the hub, a split file whose parts disagree with its name.
    pub notes: Vec<String>,
    /// Why the fit is by the file's size alone, when it is (D8): a split
    /// file whose parts can't all be read as the set its first part
    /// declares (one missing, or their count not the name's), which
    /// llama-server can't load as it is. `weights_bytes` is then the bytes
    /// known, and `None` in [`Self::listed_bytes`] when a part's are not.
    pub size_only: Option<String>,
    /// Every part's bytes when all of them are known; `None` when a part is
    /// missing or has no size, so the bytes are unknown, never a fit.
    pub listed_bytes: Option<u64>,
    /// The class whose models dir holds the file: where it was found on
    /// disk, or where `hf_add` puts it for a hub plan (the class asked for,
    /// else the header's, D51).
    pub found_in: Class,
    /// For a file outside the aux dir: whether the same path is in the aux
    /// dir as well (the aux plan's location rule).
    pub in_aux_dir: bool,
    pub source: Source,
}

impl PlanFiles {
    /// The bytes of the weights and the companions the row names, with the
    /// sentences of their missing parts, in the order the plan lists them:
    /// the weights, then each of `companions` (paths a row names).
    pub(crate) fn gguf_files(&self, companions: &[&str]) -> crate::vram::plan::GgufFiles {
        let mut missing: Vec<String> = self.weights_missing.iter().cloned().collect();
        let mut companion_bytes = 0u64;
        for path in companions {
            if let Some(c) = self.companion(path) {
                companion_bytes = companion_bytes.saturating_add(c.bytes);
                missing.extend(c.missing.clone());
            }
        }
        crate::vram::plan::GgufFiles {
            main_bytes: self.weights_bytes,
            companion_bytes,
            summary: Ok(self.summary.clone()),
            missing,
        }
    }

    /// The companion a row names at `path`.
    pub fn companion(&self, path: &str) -> Option<&Companion> {
        [&self.projector, &self.drafter]
            .into_iter()
            .flatten()
            .find(|c| c.path == path)
    }

    /// The missing-part sentences of the weights and of each of `paths`,
    /// in that order: what a split GGUF with parts missing adds to a plan's
    /// warnings.
    pub fn missing_parts(&self, paths: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for path in paths {
            if *path == self.rel {
                out.extend(self.weights_missing.clone());
            } else if let Some(c) = self.companion(path) {
                out.extend(c.missing.clone());
            }
        }
        out
    }

    /// What the hub's header reads for this plan moved and took, for the
    /// download estimate's fallback rate (D23); `None` for a disk plan.
    pub fn header_reads(&self) -> Option<crate::hf::rate::Timed> {
        match &self.source {
            Source::Hub {
                header_bytes_read,
                read_secs,
                ..
            } => Some(crate::hf::rate::Timed {
                bytes: *header_bytes_read,
                secs: *read_secs,
            }),
            Source::Disk => None,
        }
    }

    /// The bytes a download of this plan's files fetches: the weights' parts
    /// and the companions the plan names.
    pub fn download_bytes(&self) -> u64 {
        [&self.projector, &self.drafter]
            .into_iter()
            .flatten()
            .fold(self.weights_bytes, |sum, c| sum.saturating_add(c.bytes))
    }

    /// Another file of the same model, `bytes` long at `rel`, estimated from
    /// this plan's header (design §3.3): the same architecture, tokenizer and
    /// shape, so the same cache and buffers, its own bytes charged in full,
    /// not less the input embeddings this header keeps on the host (they
    /// differ by quant), and this plan's companions. Its width isn't known
    /// until its header is read.
    ///
    /// The companions are the projector and the drafter beside this file,
    /// so the estimate holds only for a file whose plan would name the same
    /// ones: a file in the same directory, or (both directories holding
    /// neither by name) a file planned without any. For such a file the
    /// estimate is never smaller than its real footprint, its input
    /// embeddings never being larger than its whole bytes. Which files are
    /// estimated from which, and which near the card's edge are read
    /// exactly, is the file list's call (WP3, `hub_fit::marks`).
    pub fn estimated(&self, rel: &str, bytes: u64) -> Self {
        let mut f = self.clone();
        f.rel = rel.to_string();
        f.weights_bytes = bytes;
        f.weights_missing = None;
        f.summary.host_input_bytes = 0;
        f.summary.file_size = bytes;
        f.layer_bits = None;
        f.layer_bits_estimated = false;
        f.layer_elements = self.layer_elements;
        f.size_only = None;
        f.listed_bytes = Some(bytes);
        f
    }

    /// The plan's source as its answer names it.
    pub fn source_dto(&self) -> lmgw_api_types::model_find::PlanSource {
        use lmgw_api_types::model_find::{PlanFrom, PlanSource};
        match &self.source {
            Source::Disk => PlanSource::default(),
            Source::Hub {
                repo,
                file,
                oid,
                commit,
                header_bytes_read,
                ..
            } => PlanSource {
                from: PlanFrom::Hub,
                repo: Some(repo.clone()),
                file: Some(file.clone()),
                oid: oid.clone(),
                commit: commit.clone(),
                header_bytes_read: *header_bytes_read,
            },
        }
    }

    /// Blocking: the plan's input for `rel` under `dir`, the models dir of
    /// `found_in` (module doc). `aux_dir` is the aux class's models dir, for
    /// the location rule. The error is the plan's own for a file that isn't
    /// a GGUF it can read.
    pub fn from_disk(dir: &str, rel: &str, found_in: Class, aux_dir: &str) -> Result<Self, String> {
        let root = std::path::Path::new(dir);
        let summary =
            gguf::summarize(&root.join(rel)).map_err(|e| format!("not a readable GGUF: {e}"))?;
        let (weights_bytes, weights_missing) = charged(root, rel);

        // Companions live in the same directory, shared by every
        // quantization of the weights: every sibling's header says its role.
        let parent = parent_of(rel);
        let mut projectors: Vec<(String, ModelSummary)> = Vec::new();
        let mut drafter: Option<(String, ModelSummary)> = None;
        for cand in crate::hf::scan_gguf_files(dir) {
            if cand == rel || parent_of(&cand) != parent {
                continue;
            }
            let Ok(cs) = gguf::summarize(&root.join(&cand)) else {
                continue;
            };
            match super::role_of(&cs) {
                "mmproj" => projectors.push((cand, cs)),
                "drafter" if drafter.is_none() => drafter = Some((cand, cs)),
                _ => {}
            }
        }
        let pick =
            crate::hf::pick_projector(projectors.iter().map(|(p, s)| (p.as_str(), s.file_size)))
                .map(str::to_string);
        let on_disk = |(path, summary): (String, ModelSummary)| {
            let (bytes, missing) = charged(root, &path);
            Companion {
                path,
                bytes,
                missing,
                summary,
            }
        };
        let projector = projectors
            .into_iter()
            .find(|(p, _)| Some(p) == pick.as_ref())
            .map(on_disk);
        let drafter = drafter.map(on_disk);

        // The later parts are read only as the set the first declares.
        let mismatch = set_mismatch(&summary, rel);
        let parts = match (&mismatch, &weights_missing) {
            (None, None) => later_parts(rel)
                .into_iter()
                .map(|p| gguf::summarize(&root.join(&p)).map_err(|e| e.to_string()))
                .collect::<Vec<_>>(),
            _ => vec![Err(String::new())],
        };
        let whole = Whole::of(&summary, &parts);
        let summary = set_summary(summary, &parts);
        let listed_bytes = weights_missing.is_none().then_some(weights_bytes);
        Ok(Self {
            rel: rel.to_string(),
            summary,
            weights_bytes,
            projector,
            drafter,
            parameters: whole.parameters,
            layer_bits: whole.layer_bits,
            layer_bits_estimated: whole.estimated,
            layer_elements: whole.layer_elements,
            host_input_elements: whole.host_input_elements,
            overhead_bytes: whole.overhead_bytes,
            notes: mismatch.iter().cloned().collect(),
            size_only: mismatch.or_else(|| weights_missing.clone()),
            weights_missing,
            listed_bytes,
            found_in,
            in_aux_dir: in_aux_dir(found_in, aux_dir, rel),
            source: Source::Disk,
        })
    }
}

/// Whether `rel` is in the aux dir `aux_dir` too, for a file whose class
/// dir isn't it: the aux plan's location rule. Blocking.
pub(crate) fn in_aux_dir(found_in: Class, aux_dir: &str, rel: &str) -> bool {
    found_in != Class::Aux
        && !aux_dir.trim().is_empty()
        && std::path::Path::new(aux_dir).join(rel).is_file()
}

/// Why the split set `rel` names isn't the one its first part's header
/// declares, when it isn't: llama.cpp finds a split file's parts by the
/// count part 1 declares (`split.count`), so a name saying another count
/// names parts it never loads, or misses some. `None` for a file that isn't
/// split, by its name and its header both, and for a set that agrees.
pub(crate) fn set_mismatch(first: &ModelSummary, rel: &str) -> Option<String> {
    let named = match crate::hf::split_part_name(rel) {
        Some((_, _, total)) => u64::from(total),
        None => 1,
    };
    let declared = first.split_count.unwrap_or(1).max(1);
    (named != declared).then(|| {
        format!(
            "{rel}'s header declares a GGUF split into {declared} part(s), and its name says              {named}: llama-server loads the parts the header declares, so this set doesn't              load as it is, and the plan's fit is by its size alone; the later parts' headers              weren't read"
        )
    })
}

/// A split file's summary as the whole set's (D3, owner ruling
/// 2026-11-04): `first`, part 1's header, with the tensors of `later`, every
/// later part's read, taken in ([`ModelSummary::of_set`]), so the input
/// embeddings an untied head in a later part keeps on the host are off the
/// card as they are for the single file of the same quant. Part 1's own
/// when a later part wasn't read, and for a file that isn't split.
pub(crate) fn set_summary(
    first: ModelSummary,
    later: &[Result<ModelSummary, String>],
) -> ModelSummary {
    match later
        .iter()
        .map(|r| r.as_ref().ok())
        .collect::<Option<Vec<_>>>()
    {
        Some(parts) => first.of_set(&parts),
        None => first,
    }
}

/// The directory part of a path, `""` at the top.
pub(crate) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// The bytes on disk of `rel` (every part of a split GGUF) and the sentence
/// for its missing parts, when some are.
fn charged(root: &std::path::Path, rel: &str) -> (u64, Option<String>) {
    match crate::vram::plan::split::gguf_bytes(root, rel) {
        Ok(b) => (b, None),
        Err(m) => (m.present_bytes, Some(m.message)),
    }
}

/// The parts of a split file after its first, by the naming rule; none for a
/// file that isn't split.
pub(crate) fn later_parts(rel: &str) -> Vec<String> {
    match crate::hf::split_part_name(rel) {
        Some((prefix, 1, total)) if total > 1 => (2..=total)
            .map(|i| format!("{prefix}-{i:05}-of-{total:05}.gguf"))
            .collect(),
        _ => Vec::new(),
    }
}

/// The whole model's count and width from its first part's header and the
/// later parts' reads (module doc).
pub(crate) struct Whole {
    pub parameters: Option<u64>,
    pub layer_bits: Option<f64>,
    pub estimated: bool,
    /// The elements `layer_bits` is over, every part's; `None` with it, or
    /// when it is the first part's alone.
    pub layer_elements: Option<u64>,
    /// The host-kept input embeddings' elements over every part read
    /// ([`crate::gguf::InputTensors::host_elements`]).
    pub host_input_elements: Option<u64>,
    /// Every part's header bytes and padding bound, of the parts read.
    pub overhead_bytes: u64,
}

impl Whole {
    pub(crate) fn of(first: &ModelSummary, later: &[Result<ModelSummary, String>]) -> Self {
        let overhead = |all: &[&ModelSummary]| {
            all.iter()
                .map(|p| p.header_bytes.saturating_add(p.padding_bound))
                .fold(0u64, u64::saturating_add)
        };
        let inputs = |all: &[&ModelSummary], complete: bool| {
            let parts: Vec<&gguf::InputTensors> = all.iter().map(|p| &p.input_tensors).collect();
            gguf::InputTensors::host_elements(&parts, complete)
        };
        if first.split_count.unwrap_or(1) <= 1 {
            return Self {
                parameters: first.parameter_count,
                layer_bits: first.layer_bits_per_weight,
                estimated: false,
                layer_elements: first.layer_tensors.map(|(_, e)| e),
                host_input_elements: inputs(&[first], true),
                overhead_bytes: overhead(&[first]),
            };
        }
        let read: Option<Vec<&ModelSummary>> = later.iter().map(|r| r.as_ref().ok()).collect();
        match read {
            Some(rest) => {
                let mut all = vec![first];
                all.extend(rest);
                let layer_bits = params::whole_layer_bits(&all);
                let layer_elements = layer_bits.and_then(|_| {
                    all.iter()
                        .filter_map(|p| p.layer_tensors.map(|(_, e)| e))
                        .try_fold(0u64, u64::checked_add)
                });
                Self {
                    parameters: params::whole_parameter_count_of(&all),
                    layer_bits,
                    estimated: false,
                    layer_elements,
                    host_input_elements: inputs(&all, true),
                    overhead_bytes: overhead(&all),
                }
            }
            None => Self {
                parameters: None,
                layer_bits: first.layer_bits_per_weight,
                estimated: first.layer_bits_per_weight.is_some(),
                layer_elements: None,
                host_input_elements: inputs(&[first], false),
                overhead_bytes: overhead(&[first]),
            },
        }
    }
}
