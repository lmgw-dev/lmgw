//! GGUF header reader — model metadata without touching the weights.
//!
//! Local models are 5–20 GB files, but everything the gateway wants to know
//! about one (architecture, context length, quantization, chat template,
//! whether it carries MTP/draft layers, whether it is an mmproj projector)
//! lives in the few megabytes of header at the front. This module reads
//! *only* that header, incrementally through a [`BufReader`], and never the
//! tensor data — so summarizing a 16 GB GGUF costs a handful of megabytes of
//! I/O rather than a full read.
//!
//! It is deliberately dependency-free (std + serde for the output types): the
//! format is small enough that a crate dependency would cost more than it
//! saves, and vendoring the parse lets us bound every allocation ourselves.
//!
//! **Everything here treats the file as untrusted.** A GGUF can arrive from a
//! HF download that was interrupted, from a hand-edited quantization, or from
//! a user pointing the models dir at something that merely ends in `.gguf`.
//! No input may panic the gateway and no declared length may be believed: a
//! string or array length is only accepted when the bytes it claims actually
//! fit in the remaining file, so the ceiling on every allocation is the real
//! file size rather than a guessed constant.
//!
//! What is *not* here: tensor shapes are parsed (they have to be, to walk
//! past the descriptors) but only the names, each tensor's on-disk bytes
//! ([`GgufMeta::tensor_bytes`], from the data offsets, [`tensors`]), its
//! element count ([`GgufMeta::tensor_elements`]), its rank and its ggml type
//! are kept, because that is all the MTP/draft-layer detection, the VRAM
//! plan's output head, the parameter count and the layers' width
//! ([`params::layer_tensors`]) need. Add fields to
//! [`GgufMeta::tensor_names`]'s neighbourhood if a caller ever needs more.
//!
//! The reader takes any forward-only [`Read`] with the file's total length
//! ([`read_header_from`]), so a file on Hugging Face is read from the front
//! of its download stream the same way, with the same guards
//! ([`crate::hf::remote`]).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a GGUF header could not be read. Typed (rather than the `String` errors
/// used elsewhere in this crate) because callers act differently on the cases:
/// [`BadMagic`](GgufError::BadMagic) means "not a GGUF, skip it quietly", while
/// [`UnexpectedEof`](GgufError::UnexpectedEof) on a `.gguf` usually means an
/// interrupted download worth surfacing to the user.
#[derive(Debug, thiserror::Error)]
pub enum GgufError {
    #[error("not a GGUF file (magic {0:02x?}, expected \"GGUF\")")]
    BadMagic([u8; 4]),
    /// Only GGUF v2/v3 use 64-bit counts and string lengths, which is what
    /// this reader assumes; v1 used 32-bit and would desynchronize silently.
    #[error("unsupported GGUF version {0} (this reader handles v2 and v3)")]
    UnsupportedVersion(u32),
    /// The header ended mid-field. Carries where, since "truncated inside the
    /// tensor descriptors" and "truncated in the first 24 bytes" mean very
    /// different things about the file.
    #[error("unexpected end of file while reading {0}")]
    UnexpectedEof(&'static str),
    #[error("unknown GGUF value type {0}")]
    BadValueType(u32),
    /// A declared length would run past the end of the file — the file is
    /// corrupt or hostile. `len` is in bytes for strings, in elements for
    /// arrays and counts (see `what`).
    #[error("{what} declares {len} which cannot fit in the {remaining} bytes left in the file")]
    TooLarge {
        what: &'static str,
        len: u64,
        remaining: u64,
    },
    /// A declared length would carry the header past the caller's bound
    /// ([`read_header_within`]): the memory this computer has available,
    /// for a file read off the network (find-a-model design D129). Nothing
    /// past `at` was read for it.
    #[error(
        "{what} at byte {at} would carry the header to byte {declared_end}, past the {bound} \
         bytes it may take"
    )]
    OverBound {
        what: &'static str,
        at: u64,
        declared_end: u64,
        bound: u64,
    },
    /// A string's bytes could not be given memory as they arrived: an
    /// error, never an abort (D129).
    #[error("no memory for {what} of {len} bytes after {got} of them arrived")]
    OutOfMemory {
        what: &'static str,
        len: u64,
        got: u64,
    },
    /// The audio.cpp embedded-file table is inconsistent (offsets that do
    /// not ascend, a names/offsets count mismatch, a file past the end of
    /// the data array) — read by [`embedded`] only.
    #[error("malformed audio.cpp embedded-file table: {0}")]
    Malformed(String),
    /// An embedded file larger than [`embedded::MAX_EMBEDDED_FILE`] — the
    /// bound on what lmgw reads out of a GGUF header for one fact. Said, not
    /// truncated: a half-read JSON file would be worse than none.
    #[error(
        "the embedded file {name} is {len} bytes, more than the {cap} bytes lmgw reads \
         out of a GGUF for one fact"
    )]
    EmbeddedFileTooLarge { name: String, len: u64, cap: u64 },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, GgufError>;

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

// GGUF metadata value type tags (little-endian u32 preceding every value).
const VT_U8: u32 = 0;
const VT_I8: u32 = 1;
const VT_U16: u32 = 2;
const VT_I16: u32 = 3;
const VT_U32: u32 = 4;
const VT_I32: u32 = 5;
const VT_F32: u32 = 6;
const VT_BOOL: u32 = 7;
const VT_STRING: u32 = 8;
const VT_ARRAY: u32 = 9;
const VT_U64: u32 = 10;
const VT_I64: u32 = 11;
const VT_F64: u32 = 12;

pub mod embedded;
pub mod licence;
pub mod params;
mod split_set;
mod tensors;

pub use split_set::HeadTensors;

#[doc(hidden)]
pub mod synth;

/// How many elements of an array are kept in [`GgufValue::Array::preview`].
///
/// Arrays in real files are dominated by the tokenizer: `tokenizer.ggml.tokens`
/// runs to ~250k strings and `tokenizer.ggml.merges` to ~440k. Retaining them
/// would cost tens of megabytes per model for data no caller of this module
/// wants — the gateway does not tokenize from the GGUF. The elements past the
/// preview are still *read* (they are variable-length, so they cannot be
/// seeked over), just discarded. `len` always reports the true element count,
/// so nothing is silently hidden. Short fixed-width arrays are exempt — see
/// [`NUMERIC_ARRAY_RETAIN`].
pub const ARRAY_PREVIEW: usize = 8;

/// Fixed-width scalar arrays up to this many elements are retained in full.
///
/// Per-layer attention metadata — `attention.head_count_kv` and
/// `attention.sliding_window_pattern` on gemma4-style models — is one array
/// element per transformer layer, and VRAM sizing needs every element, not a
/// preview. Real block counts top out around a hundred; 1024 leaves an order
/// of magnitude of slack while still refusing to materialize the
/// quarter-million-element tokenizer arrays [`ARRAY_PREVIEW`] exists to avoid
/// (`tokenizer.ggml.token_type` is fixed-width too). Past the cap an array
/// falls back to preview retention, `len` still reporting the truth.
pub const NUMERIC_ARRAY_RETAIN: u64 = 1024;

/// Preview elements are only retained down to this nesting depth. The spec
/// permits arrays of arrays; no real file uses more than one level, and
/// refusing to *retain* deeper values keeps recursion in [`Reader::read_value`]
/// shallow. Deeper arrays are still parsed correctly (skipped iteratively),
/// they just arrive with an empty preview instead of blowing the stack.
const MAX_PREVIEW_DEPTH: u32 = 4;

/// A metadata value. Arrays are summarized rather than materialized — see
/// [`ARRAY_PREVIEW`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GgufValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    String(String),
    Array {
        /// Value type tag of the elements.
        elem_type: u32,
        /// True element count, even though `preview` holds at most
        /// [`ARRAY_PREVIEW`] of them.
        len: u64,
        preview: Vec<GgufValue>,
    },
}

impl GgufValue {
    /// Any non-negative integer widened to `u64`.
    ///
    /// Writers are inconsistent about integer width — llama.cpp's converters
    /// emit `block_count` and `context_length` as `u32`, others as `u64` — so
    /// callers must never match on a specific width. Negative values yield
    /// `None`: every field this module exposes as `u64` is a count or a length.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Self::U8(v) => Some(v as u64),
            Self::U16(v) => Some(v as u64),
            Self::U32(v) => Some(v as u64),
            Self::U64(v) => Some(v),
            Self::I8(v) => u64::try_from(v).ok(),
            Self::I16(v) => u64::try_from(v).ok(),
            Self::I32(v) => u64::try_from(v).ok(),
            Self::I64(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }

    /// Any integer or float widened to `f64` (for `rope.freq_base` and
    /// friends, which are `f32` in every file seen so far but `f64` per spec).
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::F32(v) => Some(v as f64),
            Self::F64(v) => Some(v),
            Self::I8(v) => Some(v as f64),
            Self::I16(v) => Some(v as f64),
            Self::I32(v) => Some(v as f64),
            Self::I64(v) => Some(v as f64),
            Self::U8(v) => Some(v as f64),
            Self::U16(v) => Some(v as f64),
            Self::U32(v) => Some(v as f64),
            Self::U64(v) => Some(v as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// Element count if this is an array, `None` for scalars.
    pub fn array_len(&self) -> Option<u64> {
        match *self {
            Self::Array { len, .. } => Some(len),
            _ => None,
        }
    }

    /// The array's elements widened to `u64`, only when *every* element was
    /// retained (see [`NUMERIC_ARRAY_RETAIN`]) and every one converts. `None`
    /// for scalars and partially-retained arrays: a per-layer table with holes
    /// would silently describe the wrong model. Bools count as 0/1 because
    /// writers encode per-layer flags as either.
    pub fn as_u64_array(&self) -> Option<Vec<u64>> {
        match self {
            Self::Array { len, preview, .. } if preview.len() as u64 == *len => preview
                .iter()
                .map(|v| v.as_u64().or_else(|| v.as_bool().map(u64::from)))
                .collect(),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// The parsed GGUF header: metadata key/values plus tensor names.
#[derive(Debug, Clone, Serialize)]
pub struct GgufMeta {
    /// GGUF container version (3 for everything current).
    pub version: u32,
    pub tensor_count: u64,
    /// Metadata in file order would be arbitrary; a `BTreeMap` gives callers
    /// (and snapshot tests) a stable, greppable ordering. Duplicate keys —
    /// which no sane writer emits — resolve last-wins.
    pub kv: BTreeMap<String, GgufValue>,
    /// Tensor names in file order. The shapes are kept only as each
    /// tensor's element count and rank.
    pub tensor_names: Vec<String>,
    /// Each tensor's bytes on disk, parallel to `tensor_names`: from its data
    /// offset to the next one's ([`tensors::sizes`]). Not serialized; read
    /// through [`GgufMeta::tensor_size`].
    #[serde(skip)]
    pub tensor_bytes: Vec<u64>,
    /// Each tensor's element count, parallel to `tensor_names`: the product
    /// of its dimensions, `None` where that overflows `u64` (a malformed
    /// header; no real tensor comes near). Not serialized.
    #[serde(skip)]
    pub tensor_elements: Vec<Option<u64>>,
    /// Each tensor's number of dimensions, parallel to `tensor_names`. Not
    /// serialized.
    #[serde(skip)]
    pub tensor_ranks: Vec<u32>,
    /// Each tensor's ggml type id (`0` F32, `8` Q8_0, …), parallel to
    /// `tensor_names`. Not serialized.
    #[serde(skip)]
    pub tensor_types: Vec<u32>,
    /// Where the tensor data starts: the bytes a reader needs from the front
    /// of the file to have every key and descriptor, the alignment padding
    /// included (find-a-model design §2.1). Not serialized.
    #[serde(skip)]
    pub data_start: u64,
    /// Size of the whole file on disk, including the weights we did not read.
    pub file_size: u64,
}

impl GgufMeta {
    pub fn get(&self, key: &str) -> Option<&GgufValue> {
        self.kv.get(key)
    }

    /// `general.architecture` — the prefix under which a model's own keys live.
    pub fn architecture(&self) -> Option<&str> {
        self.get("general.architecture").and_then(GgufValue::as_str)
    }

    /// Look up an architecture-prefixed key by its suffix: `arch_get(
    /// "context_length")` reads `muse-glimmer.context_length` when
    /// `general.architecture` is `muse-glimmer`. `None` when the file declares
    /// no architecture — guessing a prefix would silently read the wrong model.
    pub fn arch_get(&self, suffix: &str) -> Option<&GgufValue> {
        let arch = self.architecture()?;
        self.kv.get(&format!("{arch}.{suffix}"))
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).and_then(GgufValue::as_str).map(str::to_owned)
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(GgufValue::as_u64)
    }

    pub fn arch_u64(&self, suffix: &str) -> Option<u64> {
        self.arch_get(suffix).and_then(GgufValue::as_u64)
    }

    pub fn arch_f64(&self, suffix: &str) -> Option<f64> {
        self.arch_get(suffix).and_then(GgufValue::as_f64)
    }

    /// The on-disk bytes of tensor `name`, when the header names it.
    pub fn tensor_size(&self, name: &str) -> Option<u64> {
        let i = self.tensor_names.iter().position(|n| n == name)?;
        self.tensor_bytes.get(i).copied()
    }

    /// The elements of tensor `name`: `None` when the header doesn't name
    /// it, `Some(None)` when it does but its dimensions overflow `u64`.
    pub fn tensor_elements_of(&self, name: &str) -> Option<Option<u64>> {
        let i = self.tensor_names.iter().position(|n| n == name)?;
        Some(self.tensor_elements.get(i).copied().flatten())
    }
}

/// The input tensors llama.cpp keeps in host memory, by their element
/// counts ([`ModelSummary::input_tensors`]). Each is `None` when this file
/// doesn't hold it and `Some(None)` when it does but its dimensions
/// overflow, so its count isn't known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputTensors {
    /// `token_embd.weight`.
    pub token_embd: Option<Option<u64>>,
    /// Gemma 4's `per_layer_token_embd.weight`, kept on the host whether the
    /// head is tied or not.
    pub per_layer: Option<Option<u64>>,
    /// The file holds `output.weight`: the head isn't tied, so the input
    /// embeddings stay on the host.
    pub output_head: bool,
}

impl InputTensors {
    fn of(meta: &GgufMeta) -> Self {
        Self {
            token_embd: meta.tensor_elements_of("token_embd.weight"),
            per_layer: meta.tensor_elements_of("per_layer_token_embd.weight"),
            output_head: meta.tensor_names.iter().any(|n| n == "output.weight"),
        }
    }

    /// The elements llama.cpp keeps on the host for a model whose files'
    /// headers are `parts` (one for a file that isn't split, else every
    /// part read, the first first): `token_embd.weight`'s when any part
    /// holds a separate output head, and `per_layer_token_embd.weight`'s.
    /// With `complete` false (a split file's later parts not read, so an
    /// untied head could sit in one of them) `token_embd.weight`'s count
    /// whatever the head. `None` when a count that matters isn't known: a
    /// dimension overflowed, or `token_embd.weight` isn't in the parts read
    /// while it would count; a caller bounds by the file's bytes then.
    pub fn host_elements(parts: &[&InputTensors], complete: bool) -> Option<u64> {
        let token_embd = parts.iter().find_map(|p| p.token_embd);
        let per_layer = parts.iter().find_map(|p| p.per_layer);
        let untied = !complete || parts.iter().any(|p| p.output_head);
        let embd = match (untied, token_embd) {
            (false, _) => 0,
            (true, Some(n)) => n?,
            // Counted, but in a part not read.
            (true, None) if !complete => return None,
            (true, None) => 0,
        };
        let per_layer = match per_layer {
            Some(n) => n?,
            None => 0,
        };
        embd.checked_add(per_layer)
    }
}

/// Read the header of a GGUF file: magic, version, metadata KVs and tensor
/// descriptors. Stops at the first byte of tensor data.
pub fn read_header(path: &Path) -> Result<GgufMeta> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    read_header_from(BufReader::new(file), file_size)
}

/// [`read_header`] over any forward-only reader of a GGUF's first bytes,
/// given the whole file's length `total` (find-a-model design §2.1): a file
/// on disk through a `BufReader`, or a Hugging Face file's body read as a
/// stream, its length from the CDN's `Content-Range`. `total` bounds every
/// declared length exactly as a file's size on disk does, and the last
/// tensor's bytes run to it.
///
/// Nothing is buffered here and nothing past the last descriptor is read:
/// the reader is left at the end of the header, before the alignment
/// padding, so a caller's reader counts what was taken and a stream can be
/// dropped there. A stream cut before then is
/// [`UnexpectedEof`](GgufError::UnexpectedEof), as an interrupted download
/// on disk is.
pub fn read_header_from<R: Read>(reader: R, total: u64) -> Result<GgufMeta> {
    read_header_bounded(reader, total, None)
}

/// [`read_header_from`] that stops before any declared length would carry
/// the header past byte `bound` ([`GgufError::OverBound`]), whatever the
/// total allows. For a file read off the network, whose total can be tens
/// of gigabytes of which a header needs megabytes: the caller passes a real
/// quantity and says it when the read stops (find-a-model design D129,
/// [`crate::hf::remote`]).
pub fn read_header_within<R: Read>(reader: R, total: u64, bound: u64) -> Result<GgufMeta> {
    read_header_bounded(reader, total, Some(bound))
}

fn read_header_bounded<R: Read>(reader: R, total: u64, bound: Option<u64>) -> Result<GgufMeta> {
    let file_size = total;
    let mut r = Reader::new(reader, file_size);
    r.bound = bound;

    let magic = r.read_array::<4>("the magic")?;
    if &magic != b"GGUF" {
        return Err(GgufError::BadMagic(magic));
    }
    let version = r.read_u32("the version")?;
    // v2 introduced the 64-bit counts and string lengths this reader assumes;
    // parsing a v1 file with it desynchronizes on the very first field and
    // reports a nonsense length, blaming the file rather than the reader.
    if !matches!(version, 2 | 3) {
        return Err(GgufError::UnsupportedVersion(version));
    }
    let tensor_count = r.read_u64("the tensor count")?;
    let kv_count = r.read_u64("the metadata count")?;

    // Reject absurd counts before allocating for them. Each entry has a known
    // minimum encoded size, so the file length is a hard ceiling on how many
    // can possibly follow: a KV pair is at least a 8-byte string length + a
    // 4-byte type tag + a 1-byte value, a tensor descriptor at least an 8-byte
    // name length + n_dims + ggml type + offset.
    r.check_fits("the metadata count", kv_count, 13)?;
    r.check_fits("the tensor count", tensor_count, 24)?;

    let mut kv = BTreeMap::new();
    for _ in 0..kv_count {
        let key = r.read_string("a metadata key")?;
        let ty = r.read_u32("a metadata value type")?;
        let value = r.read_value(ty, 0)?;
        kv.insert(key, value);
    }

    let mut tensor_names = Vec::with_capacity(tensor_count.min(4096) as usize);
    let mut offsets = Vec::with_capacity(tensor_names.capacity());
    let mut tensor_elements = Vec::with_capacity(tensor_names.capacity());
    let mut tensor_ranks = Vec::with_capacity(tensor_names.capacity());
    let mut tensor_types = Vec::with_capacity(tensor_names.capacity());
    for _ in 0..tensor_count {
        let name = r.read_string("a tensor name")?;
        let n_dims = r.read_u32("a tensor rank")?;
        r.check_fits("a tensor rank", n_dims as u64, 8)?;
        let mut elements = Some(1u64);
        for _ in 0..n_dims {
            let d = r.read_u64("a tensor dimension")?;
            elements = elements.and_then(|e| e.checked_mul(d));
        }
        let ggml_type = r.read_u32("a tensor type")?;
        offsets.push(r.read_u64("a tensor offset")?);
        tensor_names.push(name);
        tensor_elements.push(elements);
        tensor_ranks.push(n_dims);
        tensor_types.push(ggml_type);
    }
    let alignment = kv
        .get("general.alignment")
        .and_then(GgufValue::as_u64)
        .unwrap_or(tensors::DEFAULT_ALIGNMENT);
    let data_start = tensors::data_start(r.pos, alignment);
    let data_len = file_size.saturating_sub(data_start);
    let tensor_bytes = tensors::sizes(&offsets, data_len);

    Ok(GgufMeta {
        version,
        tensor_count,
        kv,
        tensor_names,
        tensor_bytes,
        tensor_elements,
        tensor_ranks,
        tensor_types,
        data_start,
        file_size,
    })
}

// ---------------------------------------------------------------------------
// Tokenizer identity (ladder design §4.3 rule 5)
// ---------------------------------------------------------------------------

/// Metadata keys read whole rather than through the generic preview path
/// (module doc, [`Reader::read_and_hash_array`]) — the vocabulary and merge
/// tables can hold a quarter-million entries.
const HASH_ARRAY_KEYS: [&str; 5] = [
    "tokenizer.ggml.tokens",
    "tokenizer.ggml.merges",
    "tokenizer.ggml.token_type",
    // SPM/UGM merge by score, not by a merges list (review finding 11): two
    // files with the same vocabulary and different scores tokenize
    // differently, and `merges` is absent on these tokenizers entirely.
    "tokenizer.ggml.scores",
    // The UGM normalizer's character map (second-pass review finding S4):
    // upstream (`llama-vocab.cpp`) requires this as a GGUF array of
    // INT8/UINT8, never a string — reading it with `meta.string(..)` as
    // finding 11's first pass did was always `None`, so two different
    // charsmaps compared equal.
    "tokenizer.ggml.precompiled_charsmap",
];

/// Everything that determines whether two GGUF files tokenize identically
/// (ladder design §4.3 rule 5: "every rung must tokenize identically" — the
/// fit check counts with whichever rung is running). `(len, hash)` pairs
/// stand in for the vocabulary/merges/token-type tables in full — see
/// [`Reader::read_and_hash_array`] for why they cannot be materialized twice
/// just to compare them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenizerSignature {
    pub architecture: Option<String>,
    /// `tokenizer.ggml.model`: `"gpt2"`, `"llama"`, `"bert"`, …
    pub model: Option<String>,
    /// `tokenizer.ggml.pre`: the pretokenizer family llama.cpp keys its BPE
    /// splitting rules on.
    pub pre: Option<String>,
    /// `(count, hash)` of `tokenizer.ggml.tokens` — the vocabulary.
    pub tokens: Option<(u64, u64)>,
    /// `(count, hash)` of `tokenizer.ggml.merges`, present only on a BPE
    /// tokenizer.
    pub merges: Option<(u64, u64)>,
    /// `(count, hash)` of `tokenizer.ggml.token_type` (per-token
    /// special/byte/normal classification).
    pub token_type: Option<(u64, u64)>,
    /// `(count, hash)` of `tokenizer.ggml.scores` (review finding 11): SPM/UGM
    /// tokenizers (`model == "llama"` and similar) merge by score rather than
    /// by an ordered `merges` list, which those tokenizers do not even carry.
    pub scores: Option<(u64, u64)>,
    pub bos_token_id: Option<u64>,
    pub eos_token_id: Option<u64>,
    pub add_bos_token: Option<bool>,
    pub add_eos_token: Option<bool>,
    /// `tokenizer.ggml.add_space_prefix` / `remove_extra_whitespaces` (review
    /// finding 11): SPM/UGM input normalization — two rungs that only differ
    /// here tokenize differently even with the same vocabulary and scores.
    pub add_space_prefix: Option<bool>,
    pub remove_extra_whitespaces: Option<bool>,
    /// `(count, hash)` of `tokenizer.ggml.precompiled_charsmap` (second-pass
    /// review finding S4) — the UGM normalizer's character map. Upstream
    /// (`llama-vocab.cpp`) requires this as a GGUF array of INT8/UINT8, never
    /// a string; finding 11's first pass read it with `meta.string(..)`,
    /// which is always `None` for an array key, so two different charsmaps
    /// always compared equal. Hashed like the other big arrays rather than
    /// materialized: it can run to hundreds of kilobytes.
    pub precompiled_charsmap: Option<(u64, u64)>,
    /// Every `tokenizer.chat_template*` key, keyed by the full metadata key
    /// (review finding 11): llama.cpp's `common_chat_templates_init` reads
    /// named variants too — `tokenizer.chat_template.tool_use` when tools are
    /// present — so comparing only the bare key would let two rungs that
    /// render tool calls differently pass as identical. Only meaningful to
    /// compare between two rungs when neither overrides the template with
    /// `chat_template_file` — llama-server renders with the GGUF's own
    /// template(s) then, and that caller-side condition is why this lives on
    /// the signature rather than being folded into
    /// [`Self::tokenizes_identically_to`] itself.
    pub chat_templates: std::collections::BTreeMap<String, String>,
}

impl TokenizerSignature {
    /// Whether `self` and `other` tokenize identically enough to share a
    /// ladder (design §4.3 rule 5). Deliberately excludes
    /// [`Self::chat_templates`] — comparing it is conditional on whether the
    /// row overrides it, which only the caller (`ops::validate_ladder`)
    /// knows.
    pub fn tokenizes_identically_to(&self, other: &Self) -> bool {
        self.model == other.model
            && self.pre == other.pre
            && self.tokens == other.tokens
            && self.merges == other.merges
            && self.token_type == other.token_type
            && self.scores == other.scores
            && self.bos_token_id == other.bos_token_id
            && self.eos_token_id == other.eos_token_id
            && self.add_bos_token == other.add_bos_token
            && self.add_eos_token == other.add_eos_token
            && self.add_space_prefix == other.add_space_prefix
            && self.remove_extra_whitespaces == other.remove_extra_whitespaces
            && self.precompiled_charsmap == other.precompiled_charsmap
    }
}

/// Read a GGUF header down to its tokenizer identity, hashing the three big
/// tokenizer arrays in place instead of retaining them (module doc). Stops
/// before the tensor descriptors — nothing here needs them.
pub fn read_tokenizer_signature(path: &Path) -> Result<TokenizerSignature> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut r = Reader::new(BufReader::new(file), file_size);

    let magic = r.read_array::<4>("the magic")?;
    if &magic != b"GGUF" {
        return Err(GgufError::BadMagic(magic));
    }
    let version = r.read_u32("the version")?;
    if !matches!(version, 2 | 3) {
        return Err(GgufError::UnsupportedVersion(version));
    }
    let tensor_count = r.read_u64("the tensor count")?;
    let kv_count = r.read_u64("the metadata count")?;
    r.check_fits("the metadata count", kv_count, 13)?;
    r.check_fits("the tensor count", tensor_count, 24)?;

    let mut kv = BTreeMap::new();
    let mut tokens = None;
    let mut merges = None;
    let mut token_type = None;
    let mut scores = None;
    let mut precompiled_charsmap = None;
    for _ in 0..kv_count {
        let key = r.read_string("a metadata key")?;
        let ty = r.read_u32("a metadata value type")?;
        if ty == VT_ARRAY && HASH_ARRAY_KEYS.contains(&key.as_str()) {
            let hashed = r.read_and_hash_array()?;
            match key.as_str() {
                "tokenizer.ggml.tokens" => tokens = Some(hashed),
                "tokenizer.ggml.merges" => merges = Some(hashed),
                "tokenizer.ggml.token_type" => token_type = Some(hashed),
                "tokenizer.ggml.scores" => scores = Some(hashed),
                "tokenizer.ggml.precompiled_charsmap" => precompiled_charsmap = Some(hashed),
                _ => unreachable!("key already matched HASH_ARRAY_KEYS"),
            }
        } else {
            let value = r.read_value(ty, 0)?;
            kv.insert(key, value);
        }
    }
    let meta = GgufMeta {
        version,
        tensor_count,
        kv,
        tensor_names: Vec::new(),
        tensor_bytes: Vec::new(),
        tensor_elements: Vec::new(),
        tensor_ranks: Vec::new(),
        tensor_types: Vec::new(),
        data_start: 0,
        file_size,
    };
    // Every chat-template key, bare and named alike (struct doc) — plain
    // strings, so they were already retained whole in `meta.kv` above; no
    // second reader pass needed, unlike the big arrays.
    let chat_templates = meta
        .kv
        .iter()
        .filter(|(k, _)| k.starts_with("tokenizer.chat_template"))
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .filter(|(_, s)| !s.trim().is_empty())
        .collect();
    Ok(TokenizerSignature {
        architecture: meta.architecture().map(str::to_owned),
        model: meta.string("tokenizer.ggml.model"),
        pre: meta.string("tokenizer.ggml.pre"),
        tokens,
        merges,
        token_type,
        scores,
        bos_token_id: meta.u64("tokenizer.ggml.bos_token_id"),
        eos_token_id: meta.u64("tokenizer.ggml.eos_token_id"),
        add_bos_token: meta
            .get("tokenizer.ggml.add_bos_token")
            .and_then(GgufValue::as_bool),
        add_eos_token: meta
            .get("tokenizer.ggml.add_eos_token")
            .and_then(GgufValue::as_bool),
        add_space_prefix: meta
            .get("tokenizer.ggml.add_space_prefix")
            .and_then(GgufValue::as_bool),
        remove_extra_whitespaces: meta
            .get("tokenizer.ggml.remove_extra_whitespaces")
            .and_then(GgufValue::as_bool),
        precompiled_charsmap,
        chat_templates,
    })
}

mod reader;
use reader::{min_encoded_size, scalar_size, Reader};

// ---------------------------------------------------------------------------
// Quantization names
// ---------------------------------------------------------------------------

/// Human name for a `general.file_type` (the ggml `llama_ftype` enum).
///
/// Unknown ids render as `ftype<n>` rather than being dropped: a new quant
/// type should show up in the UI as an unfamiliar label, not as a blank.
/// The `--pooling` spelling of a `<arch>.pooling_type` value, or `None` for a
/// number this llama.cpp vocabulary does not know (a newer converter) — never
/// a guessed default, since the wrong pooling silently produces vectors that
/// look fine and retrieve garbage.
pub fn pooling_name(id: u64) -> Option<&'static str> {
    match id {
        0 => Some("none"),
        1 => Some("mean"),
        2 => Some("cls"),
        3 => Some("last"),
        4 => Some("rank"),
        _ => None,
    }
}

pub fn ftype_name(id: u32) -> String {
    let name = match id {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        4 => "Q4_1_SOME_F16", // legacy; 5/6 were removed from ggml entirely
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        // 33..=35 were the Q4_0_N_M repacking variants, removed upstream.
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4_MOE",
        _ => return format!("ftype{id}"),
    };
    name.to_string()
}

// ---------------------------------------------------------------------------
// Template signals — text heuristics over `tokenizer.chat_template`
// ---------------------------------------------------------------------------
//
// Model capabilities design §3.1: what a chat template *does* (reasoning,
// effort levels, tool-call syntax) has to be read from the template text
// itself — llama-server has no separate manifest of it, and the whole point
// is to answer without a running container. These are deliberately pure text
// heuristics, not a Jinja parser: a handful of `{% %}`/`{{ }}` tags is a
// tiny, well-known vocabulary (`if`, `for`, `set`, `default(...)`) and a real
// parser would cost far more than the handful of templates in the wild
// disagree about.

/// The vocabulary `effort_levels`/`effort_default` are read against, in the
/// canonical order the design (§2.1) defines: `minimal < low < medium <
/// high < xhigh < max`. `none` is deliberately absent — it means "off", not
/// a level.
const EFFORT_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The identifiers a template's reasoning-effort logic is written against —
/// Qwen3.8 computes a `resolved_reasoning_effort` from `reasoning_effort`,
/// Muse-Glimmer (gpt-oss-style) calls its own `reasoning_strength`.
const EFFORT_IDENTS: [&str; 3] = [
    "reasoning_effort",
    "resolved_reasoning_effort",
    "reasoning_strength",
];

/// Literal markers that mean "this turn carries a reasoning trace", checked
/// in order — the first one found is [`TemplateSignals::thinking_marker`].
/// Qwen/DeepSeek wrap it in `<think>...</think>`; gemma4 uses a `<|channel>`
/// tag (`'<|channel>thought\n' + thinking_text + '\n<channel|>'`).
const THINKING_MARKERS: [&str; 6] = [
    "<think>",
    "<|channel>thought",
    "<|thinking|>",
    "[THINK]",
    "<reasoning>",
    "<|inner_monologue_start|>",
];

/// The native tool-call syntax a template renders, first marker matched
/// (design §2.1). `<function=` (Qwen's XML-ish dialect) is checked before the
/// bare `<tool_call>` wrapper (Hermes-style JSON) since Qwen's own tags
/// contain `<tool_call>` too; none of the others collide.
const TOOL_CALL_MARKERS: [(&str, &str); 9] = [
    ("<function=", "qwen-xml"),
    ("[TOOL_CALLS]", "mistral"),
    ("<|python_tag|>", "llama3"),
    ("<|tool_call_start|>", "lfm2"),
    ("<|channel|>commentary to=", "gpt-oss"),
    ("<｜tool▁calls▁begin｜>", "deepseek"),
    ("<|tool_call|>", "granite"),
    ("<|tool_call>call:", "gemma"),
    ("<tool_call>", "hermes-json"),
];

/// What a chat template's own text says about reasoning, effort selection and
/// tool-call rendering — the GGUF-derived half of a local model's
/// capabilities (design §3.1). Every field is a pure heuristic over the
/// template text: no Jinja evaluation, so a template that computes its
/// effort vocabulary indirectly (through an `{% include %}` or a macro this
/// module cannot see) is read as "nothing found", never guessed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TemplateSignals {
    /// The template renders a reasoning trace somewhere — one of
    /// [`THINKING_MARKERS`], or `message.reasoning_content` actually output
    /// (not merely read) through a `{{ ... }}` expression.
    pub thinking_markers: bool,
    /// The first marker that matched, verbatim — `"reasoning_content"` when
    /// [`thinking_markers`](Self::thinking_markers) came from the
    /// rendered-`reasoning_content` fallback rather than a literal tag.
    pub thinking_marker: Option<String>,
    /// The template reads an `enable_thinking` variable at all.
    pub enable_thinking_var: bool,
    /// `enable_thinking | default(false|true)`, or an
    /// `enable_thinking is undefined or … is true`-style guard (→ `true`).
    pub enable_thinking_default: Option<bool>,
    /// The template reads at least one of [`EFFORT_IDENTS`] — the any-of form
    /// of [`effort_var_names`](Self::effort_var_names).
    pub reasoning_effort_var: bool,
    /// *Which* of [`EFFORT_IDENTS`] the template actually reads, in canonical
    /// order. The distinction matters to a caller deciding what a request can
    /// change: llama-server maps a request's `reasoning_effort` onto the
    /// template variables named `reasoning_effort` /
    /// `resolved_reasoning_effort` only, so a template that reads its own
    /// `reasoning_strength` (gpt-oss-style) has an effort knob **nothing lmgw
    /// sends can reach**, and advertising one would be a lie.
    pub effort_var_names: Vec<String>,
    /// Every quoted literal compared against `reasoning_effort` /
    /// `resolved_reasoning_effort` / `reasoning_strength`, filtered to
    /// [`EFFORT_ORDER`] and sorted canonically. Empty when the template
    /// reads the variable only as a toggle (Cohere North: `== "none"`).
    pub effort_levels: Vec<String>,
    /// The literal in `reasoning_effort | default('…')`, verbatim (not
    /// filtered against the canonical vocabulary — this is what the
    /// template itself falls back to, whatever it is spelled).
    pub effort_default: Option<String>,
    /// The template reads a `preserve_thinking` variable — whether replayed
    /// reasoning from earlier turns is fed back to the model.
    pub preserve_thinking_var: bool,
    /// `tools` is used as a Jinja variable (`{% if tools %}`, `{% for tool in
    /// tools %}`, …), not merely mentioned in rendered prose (a system-prompt
    /// sentence like "a set of tools" must not count).
    pub tools_var: bool,
    /// The template loops over something named `tool_calls` (`{% for tc in
    /// message.tool_calls %}`) — it can render more than one call per turn.
    pub parallel_tool_calls: bool,
    /// The native tool-call syntax family the template renders (design
    /// §2.1) — `None` when tools are rendered with none of the known
    /// markers, or not rendered at all.
    pub tool_call_format: Option<String>,
}

/// The API shape of the signals. Destructured without `..`: a field added to
/// [`TemplateSignals`] stops compiling here until the API type has it too.
impl From<&TemplateSignals> for lmgw_api_types::TemplateSignals {
    fn from(s: &TemplateSignals) -> Self {
        let TemplateSignals {
            thinking_markers,
            thinking_marker,
            enable_thinking_var,
            enable_thinking_default,
            reasoning_effort_var,
            effort_var_names,
            effort_levels,
            effort_default,
            preserve_thinking_var,
            tools_var,
            parallel_tool_calls,
            tool_call_format,
        } = s.clone();
        Self {
            thinking_markers,
            thinking_marker,
            enable_thinking_var,
            enable_thinking_default,
            reasoning_effort_var,
            effort_var_names,
            effort_levels,
            effort_default,
            preserve_thinking_var,
            tools_var,
            parallel_tool_calls,
            tool_call_format,
        }
    }
}

impl TemplateSignals {
    /// Derive the signals from a template's raw Jinja source. Pure text
    /// heuristics (see the module section header) — no Jinja parsing, so a
    /// template that expresses one of these through indirection this module
    /// cannot see is read as absent, never guessed.
    pub fn from_template(text: &str) -> Self {
        let marker = THINKING_MARKERS
            .iter()
            .find(|m| text.contains(*m))
            .map(|m| m.to_string());
        let thinking_marker =
            marker.or_else(|| renders_reasoning_content(text).then(|| "reasoning_content".into()));

        let effort_var_names: Vec<String> = EFFORT_IDENTS
            .iter()
            .filter(|id| has_ident(text, id))
            .map(|id| (*id).to_string())
            .collect();

        Self {
            thinking_markers: thinking_marker.is_some(),
            thinking_marker,
            enable_thinking_var: has_ident(text, "enable_thinking"),
            enable_thinking_default: enable_thinking_default(text),
            reasoning_effort_var: !effort_var_names.is_empty(),
            effort_var_names,
            effort_levels: effort_levels(text),
            effort_default: effort_default(text),
            preserve_thinking_var: has_ident(text, "preserve_thinking"),
            tools_var: references_tools(text),
            parallel_tool_calls: parallel_tool_calls(text),
            tool_call_format: tool_call_format(text),
        }
    }
}

/// Which kind of tag [`jinja_tag_iter`] found — statement tags (`{% %}`,
/// control flow and assignment) are where `if`/`for`/`set` live; output tags
/// (`{{ }}`) are where a value actually reaches the rendered text, which is
/// the distinction [`renders_reasoning_content`] needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JinjaTagKind {
    Statement,
    Output,
}

/// Walks `text` yielding the trimmed body of every `{% ... %}` and `{{ ...
/// }}` tag, whitespace-control markers (`{%-`, `-%}`, `{{-`, `-}}`) stripped.
/// This is the unit every heuristic below reasons about, so a quoted string
/// sitting in the *rendered* prose between tags (gemma4's `"# Tools\n..."`,
/// lfm2's `"List of tools: ["`) never leaks into a check meant for the code.
fn jinja_tag_iter(text: &str) -> impl Iterator<Item = (JinjaTagKind, &str)> + '_ {
    let mut rest = text;
    std::iter::from_fn(move || loop {
        let pct = rest.find("{%");
        let brace = rest.find("{{");
        let (start, kind, close) = match (pct, brace) {
            (None, None) => return None,
            (Some(p), None) => (p, JinjaTagKind::Statement, "%}"),
            (None, Some(b)) => (b, JinjaTagKind::Output, "}}"),
            (Some(p), Some(b)) if p <= b => (p, JinjaTagKind::Statement, "%}"),
            (Some(_), Some(b)) => (b, JinjaTagKind::Output, "}}"),
        };
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find(close) else {
            rest = "";
            return None;
        };
        let body = after_open[..end].trim();
        rest = &after_open[end + 2..];
        let body = body.strip_prefix(['-', '+']).unwrap_or(body).trim();
        let body = body.strip_suffix(['-', '+']).unwrap_or(body).trim();
        if body.is_empty() {
            continue;
        }
        return Some((kind, body));
    })
}

fn jinja_tags(text: &str) -> impl Iterator<Item = &str> + '_ {
    jinja_tag_iter(text).map(|(_, body)| body)
}

fn jinja_output_tags(text: &str) -> impl Iterator<Item = &str> + '_ {
    jinja_tag_iter(text).filter_map(|(kind, body)| (kind == JinjaTagKind::Output).then_some(body))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The byte offset of the next standalone occurrence of `ident` in `text`
/// at or after `from` — bounded by a non-identifier character (or the
/// string edge) on both sides, so a search for `reasoning_effort` does not
/// fire on `resolved_reasoning_effort` and a search for `tools` does not
/// fire on `tools_or_docs_exist`.
fn find_ident(text: &str, ident: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut start = from;
    while let Some(rel) = text.get(start..).and_then(|s| s.find(ident)) {
        let pos = start + rel;
        let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let after = pos + ident.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

fn has_ident(text: &str, ident: &str) -> bool {
    find_ident(text, ident, 0).is_some()
}

/// Blanks out the contents of every quoted string, so a rendered label like
/// lfm2's `"List of tools: ["` or Qwen's `"<tools>"` heading can't be
/// mistaken for a reference to the `tools` variable itself.
fn strip_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.by_ref().next() {
        if c == '\'' || c == '"' {
            out.push(' ');
            for c2 in chars.by_ref() {
                out.push(' ');
                if c2 == c {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Every quoted (single- or double-) literal in `text`, in order. No escape
/// handling — none of the comparisons this reads (`== 'x'`, `in (...)`,
/// `default('x')`) ever need one.
fn quoted_literals(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            match text[i + 1..].find(c as char) {
                Some(rel_end) => {
                    let end = i + 1 + rel_end;
                    out.push(&text[i + 1..end]);
                    i = end + 1;
                }
                None => break,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// `tools` referenced as a Jinja variable: a bare identifier token inside a
/// `{% ... %}` or `{{ ... }}` tag, quoted string literals stripped first.
/// Matches e.g. gemma4's `{%- if tools -%}` / `{%- for tool in tools %}`,
/// qwen3.8's `{%- if tools and tools is iterable ... %}`.
fn references_tools(text: &str) -> bool {
    jinja_tags(text).any(|tag| has_ident(&strip_quoted(tag), "tools"))
}

/// A `{% for X in Y %}` tag whose iterable mentions `tool_calls` — however
/// it is spelled: dot access (qwen3.8's `message.tool_calls`), `.get(...)`
/// (gemma4's `message.get('tool_calls')`) or bracket indexing (deepseek's
/// `message['tool_calls']`). Rendering more than one call per turn is
/// exactly what looping over the list means.
fn parallel_tool_calls(text: &str) -> bool {
    jinja_tags(text).any(|tag| {
        let Some(rest) = tag.strip_prefix("for ") else {
            return false;
        };
        match rest.find(" in ") {
            Some(idx) => rest[idx + 4..].contains("tool_calls"),
            None => false,
        }
    })
}

fn tool_call_format(text: &str) -> Option<String> {
    TOOL_CALL_MARKERS
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map(|&(_, format)| format.to_string())
}

/// `message.reasoning_content` (however it is accessed) actually reaching
/// the rendered text through a `{{ ... }}` expression — as opposed to merely
/// being read into a variable, which by itself proves nothing about what the
/// template does with it. Cohere North's `print_thinking` macro and
/// Muse-Glimmer's assistant-turn renderer both do this without ever writing
/// a `<think>`-style tag.
fn renders_reasoning_content(text: &str) -> bool {
    jinja_output_tags(text).any(|tag| has_ident(tag, "reasoning_content"))
}

/// `enable_thinking | default(false)` / `default(true)` (whitespace around
/// the pipe and inside the call is optional — checked by squeezing each tag
/// before matching), or a guard that only fires when thinking has not been
/// explicitly turned off (`enable_thinking is undefined or enable_thinking
/// is true`, or the equivalent `is not defined` phrasing) — the two shapes
/// this module has seen for "on by default".
fn enable_thinking_default(text: &str) -> Option<bool> {
    for tag in jinja_tags(text) {
        let squeezed: String = tag.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.contains("enable_thinking|default(false)") {
            return Some(false);
        }
        if squeezed.contains("enable_thinking|default(true)") {
            return Some(true);
        }
    }
    const GUARD_TRUE: [&str; 2] = [
        "enable_thinking is undefined or enable_thinking is true",
        "enable_thinking is not defined or enable_thinking",
    ];
    if GUARD_TRUE.iter().any(|g| text.contains(g)) {
        return Some(true);
    }
    None
}

/// A literal from [`EFFORT_ORDER`] compared against one of [`EFFORT_IDENTS`]
/// (`==`, `!=`, `in (...)`, `not in (...)`, `| default('…')`), read per tag
/// so an unrelated string elsewhere in the template (an error message, a
/// rendered heading) can never leak in — see [`jinja_tags`].
///
/// A bare ternary default (`X if X is defined else '…'` — see
/// [`ternary_default_literal`]) is deliberately **not** a source of levels
/// on its own: falling back to `'high'` when nothing else is set says
/// nothing about what the other levels are. Its literal only counts once the
/// template proves there *are* other levels by comparing the variable
/// somewhere for real (`==`, `!=`, `in`, `not in`) — Muse-Glimmer never does,
/// so its `effort_levels` stays empty despite the `'high'` fallback.
fn effort_levels(text: &str) -> Vec<String> {
    let mut found: Vec<&'static str> = Vec::new();
    let mut has_real_comparison = false;
    for tag in jinja_tags(text) {
        if !EFFORT_IDENTS.iter().any(|id| has_ident(tag, id)) {
            continue;
        }
        let is_comparison = tag.contains("==")
            || tag.contains("!=")
            || tag.contains(" in (")
            || tag.contains("in(");
        let is_default = tag.contains("default(");
        has_real_comparison |= is_comparison;
        if !is_comparison && !is_default {
            continue;
        }
        for lit in quoted_literals(tag) {
            if let Some(canon) = EFFORT_ORDER.iter().find(|&&l| l == lit) {
                if !found.contains(canon) {
                    found.push(canon);
                }
            }
        }
    }
    if has_real_comparison {
        for tag in jinja_tags(text) {
            for ident in EFFORT_IDENTS {
                let Some(lit) = ternary_default_literal(tag, ident) else {
                    continue;
                };
                if let Some(canon) = EFFORT_ORDER.iter().find(|&&l| l == lit) {
                    if !found.contains(canon) {
                        found.push(canon);
                    }
                }
            }
        }
    }
    EFFORT_ORDER
        .iter()
        .filter(|l| found.contains(l))
        .map(|s| s.to_string())
        .collect()
}

/// The literal in `<ident> | default('…')` for whichever of [`EFFORT_IDENTS`]
/// the template uses — e.g. Qwen3.8's `resolved_reasoning_effort =
/// reasoning_effort|default('xhigh')` — falling back to the Jinja ternary
/// shape [`ternary_default_literal`] matches (Muse-Glimmer has no `|
/// default(...)` filter anywhere but falls back to `'high'` all the same).
/// Verbatim, not filtered against the canonical vocabulary: this is what the
/// template itself falls back to.
fn effort_default(text: &str) -> Option<String> {
    for ident in EFFORT_IDENTS {
        let mut from = 0;
        while let Some(pos) = find_ident(text, ident, from) {
            from = pos + ident.len();
            let rest = text[from..].trim_start();
            let Some(rest) = rest.strip_prefix('|') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix("default(") else {
                continue;
            };
            let Some(close) = rest.find(')') else {
                continue;
            };
            if let Some(lit) = quoted_literals(&rest[..close]).into_iter().next() {
                return Some(lit.to_string());
            }
        }
    }
    for tag in jinja_tags(text) {
        for ident in EFFORT_IDENTS {
            if let Some(lit) = ternary_default_literal(tag, ident) {
                return Some(lit.to_string());
            }
        }
    }
    None
}

/// The Jinja ternary "on by default" shape: `X if X is defined [and X] else
/// '…'` / `X if X is defined else "…"` — Muse-Glimmer's `render_reasoning`
/// macro: `reasoning_strength if reasoning_strength is defined and
/// reasoning_strength else 'high'`. Same intent as `| default(...)`, just
/// spelled without the filter. Order-checked rather than a single substring
/// match (`X`, then `if`, then `X` again, then `is defined`, then `else`,
/// then the literal) so an unrelated `if … else` elsewhere in the same tag
/// can't be mistaken for it; scoped to one tag (self-contained statement),
/// same as every other heuristic here.
fn ternary_default_literal<'a>(tag: &'a str, ident: &str) -> Option<&'a str> {
    let ident_pos = find_ident(tag, ident, 0)?;
    let after_ident = ident_pos + ident.len();
    let if_pos = after_ident + tag[after_ident..].find(" if ")?;
    let defined_pos = if_pos + tag[if_pos..].find("is defined")?;
    find_ident(&tag[if_pos..defined_pos], ident, 0)?;
    let else_pos = defined_pos + tag[defined_pos..].find(" else")?;
    quoted_literals(&tag[else_pos..]).into_iter().next()
}

// ---------------------------------------------------------------------------
// Summary — what callers actually want
// ---------------------------------------------------------------------------

/// The interesting parts of a GGUF header, with the architecture prefix
/// already resolved. Every field is optional because GGUF has no required
/// metadata beyond the container fields — an absent key means "the file did
/// not say", never a defaulted guess.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelSummary {
    /// `general.architecture`, and the prefix under which the fields below
    /// were looked up.
    pub architecture: Option<String>,
    pub general_name: Option<String>,
    /// `general.type`: `"model"`, `"mmproj"`, …
    pub general_type: Option<String>,
    pub size_label: Option<String>,
    /// `general.license` and its name and link, when the header states one.
    pub licence: Option<licence::Licence>,
    /// Human name for [`file_type_id`](Self::file_type_id), see [`ftype_name`].
    pub quant: Option<String>,
    pub file_type_id: Option<u32>,
    /// `<arch>.context_length` — the model's trained maximum, which is the
    /// real ceiling callers should size context against.
    pub context_length: Option<u64>,
    /// `<arch>.block_count` — transformer layers.
    pub block_count: Option<u64>,
    pub embedding_length: Option<u64>,
    /// `<arch>.feed_forward_length`; the largest layer's when the header
    /// gives one per layer (ModernBERT-style). Sizes an encoder's compute
    /// buffer ([`crate::vram::compute`]).
    pub feed_forward_length: Option<u64>,
    /// `<arch>.n_deepstack_layers` (Qwen3-VL): the text model's input
    /// embedding is this many `embedding_length`s wider than its hidden
    /// state ([`crate::vram::compute`]).
    pub deepstack_layers: Option<u64>,
    /// The length of `tokenizer.ggml.tokens`: the vocabulary a decoder's
    /// logits span ([`crate::vram::compute`]).
    pub vocab_size: Option<u64>,
    /// The output head's bytes on disk: `output.weight`, else the
    /// `token_embd.weight` a tied head reuses. llama.cpp copies it to the
    /// device for the logits when the weights stay on the host
    /// (`--n-gpu-layers 0`, [`crate::vram::compute`]). Of a split file, the
    /// whole set's once [`Self::of_set`] read its later parts.
    pub output_head_bytes: Option<u64>,
    pub head_count: Option<u64>,
    /// `<arch>.attention.head_count_kv` — KV heads (< `head_count` for GQA),
    /// the term that dominates KV-cache size.
    pub head_count_kv: Option<u64>,
    /// The per-layer *array* encoding of `attention.head_count_kv`
    /// (gemma4-style models whose global and windowed layers differ): one
    /// entry per block. Exactly one of this and `head_count_kv` is set —
    /// scalar files fill the scalar.
    pub head_count_kv_per_layer: Option<Vec<u64>>,
    /// `<arch>.attention.key_length`. Left `None` when absent rather than
    /// defaulted to the common 128: the caller can then tell "not stated"
    /// from "stated as 128", and [`kv_cache_bytes`] declines to guess.
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    /// `<arch>.attention.sliding_window` — window size of the SWA layers.
    pub sliding_window: Option<u64>,
    /// `<arch>.attention.sliding_window_pattern`. Two encodings exist in the
    /// wild: an integer N ("every Nth layer is full attention", e.g. 4) and an
    /// array of per-layer bools. For the array form this is the array *length*
    /// — see [`sliding_window_pattern_is_array`](Self::sliding_window_pattern_is_array),
    /// which callers must check before reading it as a stride.
    pub sliding_window_pattern: Option<u64>,
    /// True when `sliding_window_pattern` came from the array encoding, where
    /// the number means "layers described", not "full-attention stride".
    pub sliding_window_pattern_is_array: bool,
    /// The array encoding of `attention.sliding_window_pattern`, materialized:
    /// one entry per block, true = windowed. `None` for the integer encoding
    /// or when the array was too long to retain in full.
    pub sliding_window_layers: Option<Vec<bool>>,
    /// `<arch>.attention.key_length_swa` / `value_length_swa` — the windowed
    /// layers' head dims, on models where they differ from the full-attention
    /// ones (gemma4). Absent means the windowed layers share
    /// `key_length` / `value_length`.
    pub key_length_swa: Option<u64>,
    pub value_length_swa: Option<u64>,
    /// `<arch>.full_attention_interval` — hybrid linear-attention models (the
    /// qwen35 / qwen3next family): every Nth layer is full attention, the rest
    /// hold a constant-size recurrent state instead of a per-token KV cache.
    pub full_attention_interval: Option<u64>,
    /// `<arch>.block_size` — a block drafter's (dflash) draft block, anchor
    /// slot included: llama.cpp drafts at most one token fewer per step.
    pub block_size: Option<u64>,
    /// `<arch>.ssm.*` — the recurrent-state shape of those linear layers.
    pub ssm_conv_kernel: Option<u64>,
    pub ssm_state_size: Option<u64>,
    pub ssm_group_count: Option<u64>,
    pub ssm_inner_size: Option<u64>,
    pub rope_freq_base: Option<f64>,
    /// `<arch>.expert_used_count`: the experts a mixture-of-experts layer
    /// runs per token. With the two widths below it sizes such a layer's
    /// activations in a chat row's compute buffer
    /// ([`crate::vram::compute::chat`]).
    pub expert_used_count: Option<u64>,
    /// `<arch>.expert_feed_forward_length`, the largest layer's when the
    /// header gives one per layer.
    pub expert_feed_forward_length: Option<u64>,
    /// `<arch>.expert_shared_feed_forward_length`: the always-on shared
    /// expert's width (Qwen3.5-MoE, Ling).
    pub expert_shared_feed_forward_length: Option<u64>,
    /// `<arch>.attention.kv_lora_rank`: the latent width of multi-head latent
    /// attention.
    pub kv_lora_rank: Option<u64>,
    /// `<arch>.attention.key_length_mla` / `value_length_mla`: the per-head
    /// dims MLA decompresses to. llama.cpp runs the absorbed MLA graph, which
    /// caches K only and reads V as a view of it, exactly when both are
    /// stated (`llama_hparams::is_mla`; measured: Ling's V cache is 0
    /// bytes). A DeepSeek or Kimi GGUF converted before these keys existed
    /// keeps a real V cache although it states `kv_lora_rank`.
    pub key_length_mla: Option<u64>,
    pub value_length_mla: Option<u64>,
    /// Some `<arch>.ssm.*`, `shortconv.*`, `kda.*` or `wkv.*` key: the
    /// layers without attention keep a recurrent state (Qwen3.5's gated
    /// delta net, LFM2's short convolutions, Ling's KDA, RWKV), rather than
    /// being plain attention-free blocks that keep nothing.
    pub has_recurrent_keys: bool,
    /// `<arch>.embedding_length_per_layer_input` (Gemma 4 E-models): every
    /// layer gets a per-layer input this wide, held through the whole graph.
    pub embedding_length_per_layer_input: Option<u64>,
    /// `<arch>.attention.shared_kv_layers`: the last this many layers reuse
    /// an earlier layer's KV cache instead of keeping their own (Gemma 4
    /// E-models; a Gemma 4 MTP assistant shares all of its layers with the
    /// model it drafts for).
    pub shared_kv_layers: Option<u64>,
    /// `<arch>.nextn_predict_layers`: the multi-token-prediction layers a
    /// `draft-mtp` row runs in a context of their own.
    pub nextn_predict_layers: Option<u64>,
    /// The bytes of the input embeddings llama.cpp keeps in host memory
    /// whatever `--n-gpu-layers` says (its input layer is the CPU's):
    /// `token_embd.weight` when the file has a separate `output.weight` (a
    /// tied head is copied to the card for the logits), and Gemma 4's
    /// `per_layer_token_embd.weight`. Measured against llama.cpp's
    /// `CUDA0 model buffer size` on nine GGUF files (b11549). Of a split
    /// file, the whole set's once [`Self::of_set`] read its later parts: the
    /// untied head can sit in a later part than `token_embd.weight`.
    pub host_input_bytes: u64,
    /// The input tensors behind [`Self::host_input_bytes`] by their element
    /// counts, the same in every quant of a model while their bytes differ
    /// by quant, and whether this file holds a separate output head: what a
    /// file list estimating one file from another's header bounds the other
    /// file's host-kept bytes from, every part of a split file's together
    /// ([`InputTensors::host_elements`], find-a-model design §3.3, D61).
    #[serde(skip)]
    pub input_tensors: InputTensors,
    /// The bytes of the tensors [`Self::host_input_bytes`] and
    /// [`Self::output_head_bytes`] come from, every part's once
    /// [`Self::of_set`] took a split file's later parts in.
    #[serde(skip)]
    pub head_tensors: HeadTensors,
    /// Where this file's tensor data starts: its header's bytes, the
    /// alignment padding after it included (`GgufMeta::data_start`).
    #[serde(skip)]
    pub header_bytes: u64,
    /// The most alignment padding this file's tensors can carry between
    /// them: their count times `general.alignment`. With
    /// [`Self::header_bytes`], what a file list takes off a sibling's bytes
    /// before it bounds the sibling's layer width from below (D62).
    #[serde(skip)]
    pub padding_bound: u64,
    /// `<arch>.pooling_type` — written by the converter for embedding and
    /// rerank models (llama.cpp's `llama_pooling_type`: 0 none, 1 mean,
    /// 2 cls, 3 last, 4 rank) and absent on chat models. Its presence is the
    /// header's own statement that this file is an encoder to be served with
    /// `--embeddings`/`--reranking`, not a generator; see [`pooling_name`].
    pub pooling_type: Option<u64>,
    /// A sequence-classification head (`cls.output*` tensors) — what a
    /// cross-encoder reranker scores with. Rerankers converted before the
    /// hub wrote `pooling_type = rank` are only recognisable by this.
    pub has_classifier_head: bool,
    /// `<arch>.decision.type` — the key llama.cpp reads to serve the file as
    /// a decision model on `/v1/systemone` (decision-models design §3.2).
    /// Any value marks one, including a type this lmgw has never seen.
    pub decision_type: Option<String>,
    /// `<arch>.decision.max_head_tokens`.
    pub decision_max_head_tokens: Option<u64>,
    /// `<arch>.attention.causal`: absent on causal files, `false` on
    /// non-causal encoders (Laya, Julia-1), whose whole prompt llama.cpp
    /// evaluates in one ubatch.
    pub attention_causal: Option<bool>,
    /// `tokenizer.chat_template.systemone`: llama-server refuses to load a
    /// decision model without it.
    pub has_systemone_template: bool,
    /// `decision.*` tensors: a decision head of the file's own (Clef's),
    /// beside [`Self::has_classifier_head`]'s `cls.output*`. Either is what
    /// the decision planner reads as "answers from a head, not from label
    /// logits" (decision-models design §3.4).
    pub has_decision_tensors: bool,
    /// Whether the file can drive a chat endpoint on its own.
    pub has_chat_template: bool,
    /// Full Jinja template text (~7–10 KB typically); callers truncate for
    /// display. Kept whole because it is also the thing you want to diff when
    /// a model starts formatting tool calls wrong.
    pub chat_template: Option<String>,
    /// Text heuristics over [`chat_template`](Self::chat_template) — reasoning,
    /// effort levels and tool-call syntax (model capabilities design §3.1).
    /// `None` exactly when `chat_template` is `None`; a caller with an
    /// override file (`chat_template_file`) calls
    /// [`TemplateSignals::from_template`] on it directly rather than through
    /// this field, since that file — not this one — is what llama-server
    /// actually renders.
    pub signals: Option<TemplateSignals>,
    /// Whether the file carries multi-token-prediction / draft layers, which
    /// llama.cpp only uses when explicitly enabled.
    pub has_mtp_layers: bool,
    /// The tensor names that triggered [`has_mtp_layers`](Self::has_mtp_layers),
    /// sorted and deduplicated.
    pub mtp_tensor_names: Vec<String>,
    /// The vision projector's type on mmproj files: `clip.projector_type`,
    /// else `clip.vision.projector_type`. Files that carry a vision and an
    /// audio projector (Gemma 4) only write the per-modality keys, and
    /// llama.cpp's loader reads them in this same order, so this is the type
    /// it will load for images.
    pub projector_type: Option<String>,
    /// A vision/audio projector rather than a model — it must be passed to
    /// llama-server as `--mmproj`, never as `--model`.
    pub is_mmproj: bool,
    pub vision_block_count: Option<u64>,
    /// `clip.vision.image_size` and `clip.vision.patch_size`: the projector's
    /// nominal input size and its patch edge, in pixels. For a projector that
    /// resizes every image to one fixed square (Gemma 3), these two and the
    /// scale factor below fix its image-token count exactly.
    pub vision_image_size: Option<u64>,
    pub vision_patch_size: Option<u64>,
    /// `clip.vision.projector.scale_factor`: the pooling factor per side. Left
    /// `None` when absent, because llama.cpp's default for it depends on the
    /// projector type.
    pub vision_scale_factor: Option<u64>,
    /// `clip.vision.projection_dim`: the width of the embeddings the projector
    /// hands the text model. llama.cpp refuses to pair a projector with a text
    /// model of another width, so it equals the weights' `embedding_length`.
    pub vision_projection_dim: Option<u64>,
    /// `clip.has_vision_encoder` on mmproj files; `Some(true)` when the key
    /// is absent but a `clip.vision.*` block exists (older conversions never
    /// wrote the flag, and llama.cpp's own loader treats the block the same
    /// way). `None` — not `Some(false)` — when the header says nothing at
    /// all, so a caller can tell "not a vision projector" from "unknown".
    pub has_vision_encoder: Option<bool>,
    /// `clip.has_audio_encoder`, with the same `clip.audio.*` fallback.
    pub has_audio_encoder: Option<bool>,
    /// The model's parameters, as the descriptors declare them: every
    /// tensor's element count, summed (find-a-model design D7), so a
    /// GGUF-only upload has a count of its own. `None` for a part of a split
    /// file (`split.count` above 1, D131): one part holds some of the
    /// tensors, and the whole count is the parts' [`Self::part_parameter_count`]
    /// added up ([`params::whole_parameter_count`]). `None` too with no
    /// tensors, or when a count overflows.
    pub parameter_count: Option<u64>,
    /// The parameters of this file's own tensors, split or not: for a file
    /// that isn't split, [`Self::parameter_count`]; for a part, its share.
    pub part_parameter_count: Option<u64>,
    /// Bits per weight over the repeating layers alone: the `blk.*` tensors'
    /// bytes times 8 over their element count (D7), so a file
    /// whose token embeddings are kept wide doesn't read wider than its
    /// layers. `None` without `blk.*` tensors (a projector names its own
    /// `v.blk.*`). Of this file's tensors: a split file's first part gives
    /// the width of the layers it holds, which for a quant that mixes widths
    /// by layer need not be the whole model's (D131).
    pub layer_bits_per_weight: Option<f64>,
    /// The `blk.*` tensors' bytes and elements behind
    /// [`Self::layer_bits_per_weight`], so a split file's parts add up to
    /// the whole model's width ([`params::whole_layer_bits`]). `None` as
    /// that is.
    pub layer_tensors: Option<(u64, u64)>,
    /// `split.no` and `split.count`: which part of a split file this is,
    /// and of how many. `None` for a file that doesn't say.
    pub split_no: Option<u64>,
    pub split_count: Option<u64>,
    pub file_size: u64,
}

impl ModelSummary {
    /// Derive a summary from an already-parsed header (no I/O).
    pub fn from_meta(meta: &GgufMeta) -> Self {
        let file_type_id = meta
            .u64("general.file_type")
            .and_then(|v| u32::try_from(v).ok());

        // Both encodings of sliding_window_pattern collapse to one number; the
        // flag tells the caller which one it is looking at. The array form is
        // additionally materialized per layer when it was retained in full.
        let pattern = meta.arch_get("attention.sliding_window_pattern");
        let sliding_window_pattern_is_array = pattern.is_some_and(|v| v.array_len().is_some());
        let sliding_window_pattern = pattern.and_then(|v| v.array_len().or_else(|| v.as_u64()));
        let sliding_window_layers = pattern
            .and_then(GgufValue::as_u64_array)
            .map(|v| v.iter().map(|&x| x != 0).collect());

        let heads_kv = meta.arch_get("attention.head_count_kv");

        let chat_template = meta
            .string("tokenizer.chat_template")
            .filter(|t| !t.trim().is_empty());
        let signals = chat_template.as_deref().map(TemplateSignals::from_template);

        // MTP/draft weights are identified by tensor naming because no
        // metadata key marks them consistently: Qwen ships
        // `blk.N.nextn.*`, other conversions use `mtp` in the name.
        let mut mtp_tensor_names: Vec<String> = meta
            .tensor_names
            .iter()
            .filter(|n| {
                let lower = n.to_ascii_lowercase();
                lower.contains("nextn") || lower.contains("mtp")
            })
            .cloned()
            .collect();
        mtp_tensor_names.sort();
        mtp_tensor_names.dedup();

        let has_classifier_head = meta
            .tensor_names
            .iter()
            .any(|n| n.starts_with("cls.output"));

        let general_type = meta.string("general.type");
        let projector_type = meta
            .string("clip.projector_type")
            .or_else(|| meta.string("clip.vision.projector_type"));
        // Any one of these is enough: `general.type` is the modern marker, but
        // older mmproj conversions only carry the clip.* keys.
        let is_mmproj = general_type.as_deref() == Some("mmproj")
            || meta.get("clip.has_vision_encoder").is_some()
            || projector_type.is_some();
        let has_vision_encoder = encoder_flag(meta, "clip.has_vision_encoder", "clip.vision.");
        let has_audio_encoder = encoder_flag(meta, "clip.has_audio_encoder", "clip.audio.");

        let head_tensors = HeadTensors::of(meta);
        Self {
            architecture: meta.architecture().map(str::to_owned),
            general_name: meta.string("general.name"),
            general_type,
            size_label: meta.string("general.size_label"),
            licence: licence::Licence::from_meta(meta),
            quant: file_type_id.map(ftype_name),
            file_type_id,
            context_length: meta.arch_u64("context_length"),
            block_count: meta.arch_u64("block_count"),
            embedding_length: meta.arch_u64("embedding_length"),
            feed_forward_length: largest(meta.arch_get("feed_forward_length")),
            deepstack_layers: meta.arch_u64("n_deepstack_layers"),
            vocab_size: meta
                .get("tokenizer.ggml.tokens")
                .and_then(GgufValue::array_len),
            output_head_bytes: head_tensors.output_head_bytes(),
            head_count: meta.arch_u64("attention.head_count"),
            head_count_kv: heads_kv.and_then(GgufValue::as_u64),
            head_count_kv_per_layer: heads_kv.and_then(GgufValue::as_u64_array),
            key_length: meta.arch_u64("attention.key_length"),
            value_length: meta.arch_u64("attention.value_length"),
            key_length_swa: meta.arch_u64("attention.key_length_swa"),
            value_length_swa: meta.arch_u64("attention.value_length_swa"),
            sliding_window: meta.arch_u64("attention.sliding_window"),
            sliding_window_pattern,
            sliding_window_pattern_is_array,
            sliding_window_layers,
            full_attention_interval: meta.arch_u64("full_attention_interval"),
            block_size: meta.arch_u64("block_size"),
            ssm_conv_kernel: meta.arch_u64("ssm.conv_kernel"),
            ssm_state_size: meta.arch_u64("ssm.state_size"),
            ssm_group_count: meta.arch_u64("ssm.group_count"),
            ssm_inner_size: meta.arch_u64("ssm.inner_size"),
            rope_freq_base: meta.arch_f64("rope.freq_base"),
            expert_used_count: meta.arch_u64("expert_used_count"),
            expert_feed_forward_length: largest(meta.arch_get("expert_feed_forward_length")),
            expert_shared_feed_forward_length: largest(
                meta.arch_get("expert_shared_feed_forward_length"),
            ),
            kv_lora_rank: meta.arch_u64("attention.kv_lora_rank"),
            key_length_mla: meta.arch_u64("attention.key_length_mla"),
            value_length_mla: meta.arch_u64("attention.value_length_mla"),
            has_recurrent_keys: meta.architecture().is_some_and(|arch| {
                ["ssm.", "shortconv.", "kda.", "wkv."].iter().any(|family| {
                    let prefix = format!("{arch}.{family}");
                    meta.kv.keys().any(|k| k.starts_with(&prefix))
                })
            }),
            embedding_length_per_layer_input: meta.arch_u64("embedding_length_per_layer_input"),
            shared_kv_layers: meta.arch_u64("attention.shared_kv_layers"),
            nextn_predict_layers: meta.arch_u64("nextn_predict_layers"),
            host_input_bytes: head_tensors.host_input_bytes(),
            input_tensors: InputTensors::of(meta),
            head_tensors,
            header_bytes: meta.data_start,
            padding_bound: meta.tensor_count.saturating_mul(
                meta.get("general.alignment")
                    .and_then(GgufValue::as_u64)
                    .unwrap_or(tensors::DEFAULT_ALIGNMENT),
            ),
            pooling_type: meta.arch_u64("pooling_type"),
            has_classifier_head,
            decision_type: meta
                .arch_get("decision.type")
                .and_then(GgufValue::as_str)
                .map(str::to_owned),
            decision_max_head_tokens: meta.arch_u64("decision.max_head_tokens"),
            attention_causal: meta
                .arch_get("attention.causal")
                .and_then(GgufValue::as_bool),
            has_systemone_template: meta.get("tokenizer.chat_template.systemone").is_some(),
            has_decision_tensors: meta.tensor_names.iter().any(|n| n.starts_with("decision.")),
            has_chat_template: chat_template.is_some(),
            chat_template,
            signals,
            has_mtp_layers: !mtp_tensor_names.is_empty(),
            mtp_tensor_names,
            projector_type,
            is_mmproj,
            vision_block_count: meta.u64("clip.vision.block_count"),
            vision_image_size: meta.u64("clip.vision.image_size"),
            vision_patch_size: meta.u64("clip.vision.patch_size"),
            vision_scale_factor: meta.u64("clip.vision.projector.scale_factor"),
            vision_projection_dim: meta.u64("clip.vision.projection_dim"),
            has_vision_encoder,
            has_audio_encoder,
            parameter_count: params::parameter_count(meta),
            part_parameter_count: params::own_parameter_count(meta),
            layer_bits_per_weight: params::layer_bits_per_weight(meta),
            layer_tensors: params::layer_tensors(meta),
            split_no: meta.u64("split.no"),
            split_count: meta.u64("split.count"),
            file_size: meta.file_size,
        }
    }
}

/// A width the header states once or per layer (`feed_forward_length`,
/// `expert_feed_forward_length`): the value, or the largest layer's.
fn largest(v: Option<&GgufValue>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_u64_array().and_then(|a| a.into_iter().max()))
}

/// `<key>` when present and boolean; else `Some(true)` when any key under
/// `<prefix>` exists (older mmproj conversions never wrote the modern
/// `clip.has_*_encoder` flag, and llama.cpp's own loader treats the block's
/// mere presence the same way); else `None` — the header says nothing at
/// all, which is not the same as "no".
fn encoder_flag(meta: &GgufMeta, key: &str, prefix: &str) -> Option<bool> {
    match meta.get(key).and_then(GgufValue::as_bool) {
        Some(b) => Some(b),
        None => meta
            .kv
            .keys()
            .any(|k| k.starts_with(prefix))
            .then_some(true),
    }
}

/// Read a GGUF header and reduce it to the fields callers care about.
pub fn summarize(path: &Path) -> Result<ModelSummary> {
    Ok(ModelSummary::from_meta(&read_header(path)?))
}

// ---------------------------------------------------------------------------
// GGUF summary cache (model capabilities design §3.5)
// ---------------------------------------------------------------------------
//
// `/v1/models` must stay cheap: one header read per local row on every call
// is tens of MB of I/O for no reason when the file has not moved since the
// last read. Keyed by absolute path, each entry is validated against the
// file's current `(len, mtime)` on every hit rather than on a timer — a
// model re-downloaded or re-quantized in place is picked up on the very next
// call, and a file that never changes never pays for a re-read.

/// One cached [`ModelSummary`], stamped with the file metadata it was read
/// at so a later call can tell whether the file has changed underneath it.
struct CachedSummary {
    len: u64,
    mtime: SystemTime,
    summary: Arc<ModelSummary>,
}

/// What one blocking `stat` (+ read) decided about a path.
enum StatOutcome {
    /// The file's `(len, mtime)` still match the cached entry.
    Unchanged,
    /// Freshly read, with the stamp it was read at. Boxed because a
    /// `ModelSummary` carries the whole chat template: leaving it inline would
    /// make every `Unchanged` (the common answer) move a kilobyte of nothing.
    Read {
        len: u64,
        mtime: SystemTime,
        summary: Box<ModelSummary>,
    },
}

/// The blocking half of [`GgufSummaryCache::summarize_cached`]: `stat` first,
/// header read only when the stamp moved. Both are filesystem calls, so both
/// belong on the blocking pool — a `metadata()` on the async thread is a
/// syscall on the reactor, and on a cold NFS/spun-down disk it is not a fast
/// one.
fn stat_and_read(
    path: &Path,
    cached: Option<(u64, SystemTime)>,
) -> std::result::Result<StatOutcome, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if cached == Some((len, mtime)) {
        return Ok(StatOutcome::Unchanged);
    }
    let summary =
        summarize(path).map_err(|e| format!("{}: not a readable GGUF: {e}", path.display()))?;
    Ok(StatOutcome::Read {
        len,
        mtime,
        summary: Box::new(summary),
    })
}

/// `AppState`'s GGUF header cache (mirrors [`crate::catalog::CatalogCache`]'s
/// shape: a lock around a map, `Default`-constructed). Projectors go through
/// the same cache as weights — both are read through
/// [`Self::summarize_cached`].
#[derive(Default)]
pub struct GgufSummaryCache {
    inner: RwLock<HashMap<PathBuf, CachedSummary>>,
    /// One mutex per path, held across the stat+read so that concurrent
    /// callers for the *same* file collapse into one read instead of each
    /// paying for it (a cold `/v1/models` asks for the same projector once per
    /// row that configures it). Entries are never evicted; the key space is
    /// the set of configured model files, not user input.
    inflight: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// How many header reads actually happened — the observable the
    /// single-flight and cache-hit behaviour is asserted against (and a
    /// legitimate metric: a number that keeps climbing on a warm gateway means
    /// something is rewriting model files under it).
    reads: AtomicU64,
}

impl GgufSummaryCache {
    /// Read `path`'s GGUF header, or return the cached summary when the
    /// file's size and mtime have not changed since the last read.
    ///
    /// Everything that touches the filesystem — the `stat` as well as the
    /// header read — runs on `spawn_blocking`, one task per call, so a cold
    /// cache (23 local rows on a `/v1/models` call right after startup) fans
    /// out across the blocking pool and never stalls the reactor. Concurrent
    /// callers naming the same path serialise behind that path's own guard:
    /// the first reads, the rest find the fresh entry and only re-`stat`.
    ///
    /// Errors (file missing, not a GGUF, truncated header) are never
    /// cached — a transient problem must not stick around after it is
    /// fixed, and the cost of re-reading a file that keeps failing is the
    /// same handful of KB every time.
    pub async fn summarize_cached(
        &self,
        path: &Path,
    ) -> std::result::Result<Arc<ModelSummary>, String> {
        let guard = {
            let mut inflight = self.inflight.lock().await;
            inflight.entry(path.to_path_buf()).or_default().clone()
        };
        let _held = guard.lock().await;

        let cached = self
            .inner
            .read()
            .await
            .get(path)
            .map(|c| (c.len, c.mtime, c.summary.clone()));

        let owned = path.to_path_buf();
        let stamp = cached.as_ref().map(|(len, mtime, _)| (*len, *mtime));
        let outcome = tokio::task::spawn_blocking(move || stat_and_read(&owned, stamp))
            .await
            .map_err(|e| format!("gguf read panicked: {e}"))??;

        match outcome {
            // Only reachable when `cached` was `Some` — `stat_and_read` can
            // only report "unchanged" against a stamp it was given.
            StatOutcome::Unchanged => Ok(cached
                .map(|(_, _, summary)| summary)
                .expect("unchanged implies a cached entry")),
            StatOutcome::Read {
                len,
                mtime,
                summary,
            } => {
                self.reads.fetch_add(1, Ordering::Relaxed);
                let summary = Arc::new(*summary);
                self.inner.write().await.insert(
                    path.to_path_buf(),
                    CachedSummary {
                        len,
                        mtime,
                        summary: summary.clone(),
                    },
                );
                Ok(summary)
            }
        }
    }

    /// Header reads performed since this cache was created — everything the
    /// cache exists to avoid. A hit does not increment it.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

/// Bytes of KV cache at `ctx` tokens, accounting for sliding-window layers.
///
/// The formula, per layer per token: K holds `head_count_kv * key_length`
/// elements and V holds `head_count_kv * value_length`, so
///
/// ```text
/// elements_per_layer_token = head_count_kv * (key_length + value_length)
/// tokens(layer)            = ctx                        for full-attention layers
///                          = min(ctx, sliding_window)   for SWA layers
/// bytes                    = Σ_layers tokens(layer) * elements_per_layer_token
///                            * bits_per_element / 8
/// ```
///
/// Layer classification, in order:
///
/// * A declared `full_attention_interval` N > 1 (hybrid linear-attention
///   models — the qwen35 / qwen3next family) means only every Nth layer is
///   full attention (`block_count / N` of them). The other layers keep a
///   **constant-size recurrent state**, not a per-token cache: they are
///   charged [`recurrent_state_bytes`] each, independent of `ctx` and of the
///   cache type. Missing that formula's `ssm.*` inputs, they charge zero —
///   still a lower bound, never an overcount that blocks a model that fits.
/// * A **per-layer** `attention.head_count_kv` array (gemma4-style models,
///   whose global and windowed layers differ in heads *and* head dims) sizes
///   each layer with its own head count, the materialized
///   `sliding_window_layers` array deciding which layers are windowed and the
///   `*_swa` head dims applying to those. An array whose length does not
///   match `block_count` describes some other model — that is a malformed
///   header and the answer is `None`, not a guess.
/// * Otherwise, a `sliding_window` beside either the materialized bool array
///   or an integer `sliding_window_pattern` N > 1 (every Nth layer full)
///   splits the layers, windowed ones held at `min(ctx, sliding_window)`
///   tokens.
/// * Without any of that, every layer is treated as full attention — the
///   conservative answer. An array pattern that was too long to retain (see
///   [`NUMERIC_ARRAY_RETAIN`]) is not usable as a stride, so it is ignored
///   and all layers count as full attention.
///
/// `bits_per_element` comes from the cache type: 16 for `f16`/`bf16`, 8 for
/// `q8_0`, 5 for `q5_1`, 4 for `q4_0`. **Treat the result as a lower bound for
/// the quantized types**: `q8_0` and friends store a per-block scale (and
/// `q4_1`/`q5_1` a min) alongside the quants, which this does not count —
/// `q8_0` really costs ~8.5 bits/element. Nor does it include the compute
/// buffers llama.cpp allocates next to the cache.
///
/// Returns `None` when the header did not state `block_count`,
/// `head_count_kv`, `key_length` or `value_length` — no defaults are invented,
/// since a wrong VRAM estimate is worse than none — or if the arithmetic would
/// overflow `u64`.
pub fn kv_cache_bytes(s: &ModelSummary, ctx: u64, bits_per_element: u32) -> Option<u64> {
    let layers = s.block_count?;
    let (kl, vl) = (s.key_length?, s.value_length?);
    let span_full = kl.checked_add(vl)?;
    let span_swa = s
        .key_length_swa
        .unwrap_or(kl)
        .checked_add(s.value_length_swa.unwrap_or(vl))?;
    let swa_tokens = s.sliding_window.map_or(ctx, |w| w.min(ctx));
    let bytes_of = |elems: u64| -> Option<u64> {
        Some(elems.checked_mul(u64::from(bits_per_element))?.div_ceil(8))
    };

    if let Some(n) = s.full_attention_interval.filter(|n| *n > 1) {
        let full_layers = layers / n;
        let linear_layers = layers - full_layers;
        let attn = bytes_of(
            full_layers
                .checked_mul(ctx)?
                .checked_mul(s.head_count_kv?.checked_mul(span_full)?)?,
        )?;
        let recurrent = linear_layers.checked_mul(recurrent_state_bytes(s).unwrap_or(0))?;
        return attn.checked_add(recurrent);
    }

    // Windowed unless the file says otherwise; the "everything full attention"
    // fallbacks below express themselves as "no layer is windowed".
    let is_swa = |i: usize| match (&s.sliding_window_layers, s.sliding_window_pattern) {
        (Some(w), _) if w.len() as u64 == layers => w[i],
        (_, Some(n))
            if n > 1 && !s.sliding_window_pattern_is_array && s.sliding_window.is_some() =>
        {
            !(i as u64 + 1).is_multiple_of(n)
        }
        _ => false,
    };

    if let Some(heads) = &s.head_count_kv_per_layer {
        if heads.len() as u64 != layers {
            return None;
        }
        let mut elems = 0u64;
        for (i, &h) in heads.iter().enumerate() {
            let (tokens, span) = if is_swa(i) {
                (swa_tokens, span_swa)
            } else {
                (ctx, span_full)
            };
            elems = elems.checked_add(h.checked_mul(span)?.checked_mul(tokens)?)?;
        }
        return bytes_of(elems);
    }

    // Uniform heads: the same classification, closed-form over layer counts —
    // never a loop over `block_count`, which is attacker-controlled input.
    let heads_kv = s.head_count_kv?;
    let swa_layers = match (&s.sliding_window_layers, s.sliding_window_pattern) {
        (Some(w), _) if w.len() as u64 == layers => w.iter().filter(|&&b| b).count() as u64,
        (_, Some(n))
            if n > 1 && !s.sliding_window_pattern_is_array && s.sliding_window.is_some() =>
        {
            layers - layers / n
        }
        _ => 0,
    };
    let full_elems = (layers - swa_layers)
        .checked_mul(ctx)?
        .checked_mul(heads_kv.checked_mul(span_full)?)?;
    let swa_elems = swa_layers
        .checked_mul(swa_tokens)?
        .checked_mul(heads_kv.checked_mul(span_swa)?)?;
    bytes_of(full_elems.checked_add(swa_elems)?)
}

/// Bytes of recurrent state one linear-attention layer holds, per sequence:
/// the conv window `(conv_kernel − 1) * (inner_size + 2 * group_count *
/// state_size)` plus the state matrix `state_size * inner_size`, both kept in
/// f32 by llama.cpp regardless of the KV cache type. This is llama.cpp's
/// generic recurrent-cache shape; hybrid architectures specialize it a little,
/// but the term is single-digit MiB per layer either way and the module's
/// contract is a lower bound. `None` when `ssm.state_size` or
/// `ssm.inner_size` is absent; a missing `conv_kernel` or `group_count` just
/// zeroes its own term — shrinking the estimate, never inflating it.
pub(crate) fn recurrent_state_bytes(s: &ModelSummary) -> Option<u64> {
    let state = s.ssm_state_size?;
    let inner = s.ssm_inner_size?;
    let groups = s.ssm_group_count.unwrap_or(0);
    let conv_span = inner.checked_add(2u64.checked_mul(groups)?.checked_mul(state)?)?;
    let conv = s
        .ssm_conv_kernel
        .unwrap_or(0)
        .saturating_sub(1)
        .checked_mul(conv_span)?;
    conv.checked_add(state.checked_mul(inner)?)?.checked_mul(4)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod review_regressions {
    use super::*;
    use std::io::Write;

    /// Build a minimal valid header carrying one KV pair, then let the caller
    /// append a crafted value.
    fn header(kv_count: u64, version: u32) -> Vec<u8> {
        let mut v = b"GGUF".to_vec();
        v.extend_from_slice(&version.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        v.extend_from_slice(&kv_count.to_le_bytes());
        v
    }

    fn kv_key(v: &mut Vec<u8>, key: &str) {
        v.extend_from_slice(&(key.len() as u64).to_le_bytes());
        v.extend_from_slice(key.as_bytes());
    }

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    /// A huge fixed-width array used to be skipped one element at a time, at
    /// roughly 19 seconds per GiB of declared payload. The elements here are
    /// backed by real bytes, so this only proves the mechanism; the point is
    /// that the skip is one operation rather than `len` of them.
    #[test]
    fn a_large_scalar_array_is_skipped_in_one_operation() {
        const N: u64 = 4 * 1024 * 1024;
        let mut v = header(1, 3);
        kv_key(&mut v, "big");
        v.extend_from_slice(&9u32.to_le_bytes()); // array
        v.extend_from_slice(&0u32.to_le_bytes()); // of u8
        v.extend_from_slice(&N.to_le_bytes());
        v.extend(std::iter::repeat_n(0x41u8, N as usize));
        let f = write_tmp(&v);

        let started = std::time::Instant::now();
        let meta = read_header(f.path()).expect("should parse");
        // Generous: the old path took seconds for this size even in release,
        // and this is a debug build.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "skipping {N} bytes took {:?}",
            started.elapsed()
        );
        assert_eq!(meta.kv.get("big").and_then(GgufValue::array_len), Some(N));
    }

    /// A declared length far larger than the file must be refused before
    /// anything is allocated for it — a single flipped bit in a length field
    /// is enough to ask for gigabytes.
    #[test]
    fn an_absurd_string_length_is_refused_not_allocated() {
        let mut v = header(1, 3);
        kv_key(&mut v, "k");
        v.extend_from_slice(&8u32.to_le_bytes()); // string
        v.extend_from_slice(&0x8000_1B58u64.to_le_bytes()); // ~2.1 GB
        v.extend_from_slice(b"short");
        let f = write_tmp(&v);
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::TooLarge { .. })
        ));
    }

    /// v1 used 32-bit counts and lengths, so this reader desynchronizes on it.
    /// The error should say that rather than blame the file's contents.
    #[test]
    fn an_unsupported_version_is_named() {
        for bad in [0u32, 1, 4, u32::MAX] {
            let f = write_tmp(&header(0, bad));
            match read_header(f.path()) {
                Err(GgufError::UnsupportedVersion(v)) => assert_eq!(v, bad),
                other => panic!("version {bad} gave {other:?}"),
            }
        }
        // v2 and v3 still parse.
        for good in [2u32, 3] {
            let f = write_tmp(&header(0, good));
            assert_eq!(read_header(f.path()).unwrap().version, good);
        }
    }

    /// Invalid UTF-8 is decoded lossily rather than failing the whole model,
    /// and the raw buffer is not kept alive alongside the expansion.
    #[test]
    fn invalid_utf8_still_decodes() {
        let mut v = header(1, 3);
        kv_key(&mut v, "k");
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&3u64.to_le_bytes());
        v.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        let f = write_tmp(&v);
        let meta = read_header(f.path()).expect("lossy decode");
        assert!(meta.kv.get("k").and_then(GgufValue::as_str).is_some());
    }
}
