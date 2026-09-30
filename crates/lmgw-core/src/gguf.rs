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
//! What is *not* here: tensor shapes and ggml types are parsed (they have to
//! be, to walk past the descriptors) but only the names are kept, because that
//! is all the MTP/draft-layer detection needs. Add fields to
//! [`GgufMeta::tensor_names`]'s neighbourhood if a caller ever needs more.

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

/// A minimal GGUF *header* writer for tests in other crates and modules — the
/// integration suites need "a file whose header says it is an embedding
/// model" without shipping a real GGUF. Only what a header check reads is
/// written: string / u32 / u64 KVs and tensor names with zero-byte data.
/// Hidden from docs because it is not a general-purpose writer.
#[doc(hidden)]
pub mod synth {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    #[derive(Default)]
    pub struct Header {
        kv: Vec<u8>,
        kv_count: u64,
        tensors: Vec<u8>,
        tensor_count: u64,
    }

    impl Header {
        fn kv(&mut self, key: &str, ty: u32, body: &[u8]) -> &mut Self {
            put_str(&mut self.kv, key);
            self.kv.extend_from_slice(&ty.to_le_bytes());
            self.kv.extend_from_slice(body);
            self.kv_count += 1;
            self
        }
        pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
            let mut b = Vec::new();
            put_str(&mut b, v);
            self.kv(k, super::VT_STRING, &b)
        }
        pub fn u32(&mut self, k: &str, v: u32) -> &mut Self {
            self.kv(k, super::VT_U32, &v.to_le_bytes())
        }
        pub fn u64(&mut self, k: &str, v: u64) -> &mut Self {
            self.kv(k, super::VT_U64, &v.to_le_bytes())
        }
        pub fn bool_flag(&mut self, k: &str, v: bool) -> &mut Self {
            self.kv(k, super::VT_BOOL, &[v as u8])
        }
        /// A `tokenizer.ggml.tokens`/`merges`-shaped array of strings — the
        /// two tables [`super::read_tokenizer_signature`] hashes in full
        /// rather than through the generic (preview-truncated) array path.
        pub fn arr_str(&mut self, k: &str, vals: &[&str]) -> &mut Self {
            let mut b = super::VT_STRING.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for v in vals {
                put_str(&mut b, v);
            }
            self.kv(k, super::VT_ARRAY, &b)
        }
        /// A `tokenizer.ggml.token_type`-shaped array of `int32`s.
        pub fn arr_i32(&mut self, k: &str, vals: &[i32]) -> &mut Self {
            let mut b = super::VT_I32.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for v in vals {
                b.extend_from_slice(&v.to_le_bytes());
            }
            self.kv(k, super::VT_ARRAY, &b)
        }
        pub fn tensor(&mut self, name: &str) -> &mut Self {
            put_str(&mut self.tensors, name);
            self.tensors.extend_from_slice(&1u32.to_le_bytes());
            self.tensors.extend_from_slice(&1u64.to_le_bytes());
            self.tensors.extend_from_slice(&0u32.to_le_bytes());
            self.tensors.extend_from_slice(&0u64.to_le_bytes());
            self.tensor_count += 1;
            self
        }
        pub fn bytes(&self) -> Vec<u8> {
            let mut out = b"GGUF".to_vec();
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&self.tensor_count.to_le_bytes());
            out.extend_from_slice(&self.kv_count.to_le_bytes());
            out.extend_from_slice(&self.kv);
            out.extend_from_slice(&self.tensors);
            out
        }
        /// Write the header to `path`, creating parent directories.
        pub fn write_to(&self, path: &std::path::Path) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create models dir");
            }
            std::fs::write(path, self.bytes()).expect("write synthetic gguf");
        }
    }

    /// A header that reads as an embedding model of `arch` with the given
    /// `pooling_type` and trained context — the shape every hub-converted
    /// embedder carries.
    pub fn embedding(arch: &str, pooling_type: u32, context_length: u32) -> Header {
        let mut h = Header::default();
        h.str("general.architecture", arch)
            .str("general.name", "synthetic embedder")
            .u32(&format!("{arch}.context_length"), context_length)
            .u32(&format!("{arch}.block_count"), 4)
            .u32(&format!("{arch}.embedding_length"), 64)
            .u32(&format!("{arch}.pooling_type"), pooling_type);
        h
    }

    /// A header that reads as a plain chat model (no pooling, a template).
    pub fn chat(arch: &str, context_length: u32) -> Header {
        let mut h = Header::default();
        h.str("general.architecture", arch)
            .str("general.name", "synthetic chat model")
            .u32(&format!("{arch}.context_length"), context_length)
            .u32(&format!("{arch}.block_count"), 4)
            .str("tokenizer.chat_template", "{{ messages }}");
        h
    }
}

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
    /// Tensor names in file order. Shapes and ggml types are parsed but
    /// dropped; nothing needs them yet.
    pub tensor_names: Vec<String>,
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
}

/// Read the header of a GGUF file: magic, version, metadata KVs and tensor
/// descriptors. Stops at the first byte of tensor data.
pub fn read_header(path: &Path) -> Result<GgufMeta> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut r = Reader::new(BufReader::new(file), file_size);

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
    for _ in 0..tensor_count {
        let name = r.read_string("a tensor name")?;
        let n_dims = r.read_u32("a tensor rank")?;
        r.check_fits("a tensor rank", n_dims as u64, 8)?;
        for _ in 0..n_dims {
            r.read_u64("a tensor dimension")?;
        }
        let _ggml_type = r.read_u32("a tensor type")?;
        let _offset = r.read_u64("a tensor offset")?;
        tensor_names.push(name);
    }

    Ok(GgufMeta {
        version,
        tensor_count,
        kv,
        tensor_names,
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

// ---------------------------------------------------------------------------
// Reader — every read is bounded by the real remaining file length
// ---------------------------------------------------------------------------

struct Reader<R: Read> {
    inner: R,
    /// Bytes consumed so far. Tracked by hand rather than with `Seek` because
    /// the whole point is a single forward pass through a `BufReader`.
    pos: u64,
    file_size: u64,
}

impl<R: Read> Reader<R> {
    fn new(inner: R, file_size: u64) -> Self {
        Self {
            inner,
            pos: 0,
            file_size,
        }
    }

    fn remaining(&self) -> u64 {
        self.file_size.saturating_sub(self.pos)
    }

    fn read_array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        match self.inner.read_exact(&mut buf) {
            Ok(()) => {
                self.pos += N as u64;
                Ok(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(GgufError::UnexpectedEof(what))
            }
            Err(e) => Err(GgufError::Io(e)),
        }
    }

    fn read_u32(&mut self, what: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(what)?))
    }

    fn read_u64(&mut self, what: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>(what)?))
    }

    /// Refuse a declared count of `count` items that each occupy at least
    /// `min_each` bytes when they cannot fit in what is left of the file.
    /// This is the single guard that keeps a corrupt length from turning into
    /// a multi-gigabyte allocation, and its ceiling is the real file size —
    /// never a made-up maximum.
    fn check_fits(&self, what: &'static str, count: u64, min_each: u64) -> Result<()> {
        // Saturating, not checked: an overflowing product is by definition
        // larger than any file and must be rejected, not wrapped.
        if count.saturating_mul(min_each) > self.remaining() {
            return Err(GgufError::TooLarge {
                what,
                len: count,
                remaining: self.remaining(),
            });
        }
        Ok(())
    }

    /// GGUF strings are a `u64` byte length followed by unterminated UTF-8.
    /// Decoded lossily: a mojibake token name should not fail a whole model.
    fn read_string(&mut self, what: &'static str) -> Result<String> {
        let len = self.read_u64(what)?;
        self.check_fits(what, len, 1)?;
        let mut buf = vec![0u8; len as usize];
        match self.inner.read_exact(&mut buf) {
            Ok(()) => {
                self.pos += len;
                // Reuse the buffer when it is already valid UTF-8 — which it
                // essentially always is. Going through `from_utf8_lossy`
                // unconditionally kept the raw bytes and a copy alive at once,
                // and each invalid byte expands to a 3-byte U+FFFD, so a
                // corrupt length field cost ~4x the declared size in RAM.
                Ok(match String::from_utf8(buf) {
                    Ok(s) => s,
                    Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(GgufError::UnexpectedEof(what))
            }
            Err(e) => Err(GgufError::Io(e)),
        }
    }

    fn read_value(&mut self, ty: u32, depth: u32) -> Result<GgufValue> {
        Ok(match ty {
            VT_U8 => GgufValue::U8(self.read_array::<1>("a u8")?[0]),
            VT_I8 => GgufValue::I8(i8::from_le_bytes(self.read_array::<1>("an i8")?)),
            VT_U16 => GgufValue::U16(u16::from_le_bytes(self.read_array::<2>("a u16")?)),
            VT_I16 => GgufValue::I16(i16::from_le_bytes(self.read_array::<2>("an i16")?)),
            VT_U32 => GgufValue::U32(self.read_u32("a u32")?),
            VT_I32 => GgufValue::I32(i32::from_le_bytes(self.read_array::<4>("an i32")?)),
            VT_F32 => GgufValue::F32(f32::from_le_bytes(self.read_array::<4>("an f32")?)),
            // Any non-zero byte is true; the spec says 1 but writers have
            // shipped 0xff.
            VT_BOOL => GgufValue::Bool(self.read_array::<1>("a bool")?[0] != 0),
            VT_STRING => GgufValue::String(self.read_string("a string")?),
            VT_U64 => GgufValue::U64(self.read_u64("a u64")?),
            VT_I64 => GgufValue::I64(i64::from_le_bytes(self.read_array::<8>("an i64")?)),
            VT_F64 => GgufValue::F64(f64::from_le_bytes(self.read_array::<8>("an f64")?)),
            VT_ARRAY => {
                let elem_type = self.read_u32("an array element type")?;
                let len = self.read_u64("an array length")?;
                self.check_fits("an array", len, min_encoded_size(elem_type)?)?;
                let keep = if depth >= MAX_PREVIEW_DEPTH {
                    0
                } else if scalar_size(elem_type).is_some() && len <= NUMERIC_ARRAY_RETAIN {
                    len
                } else {
                    ARRAY_PREVIEW as u64
                };
                let mut preview = Vec::with_capacity(keep.min(len) as usize);
                let previewed = len.min(keep);
                for _ in 0..previewed {
                    preview.push(self.read_value(elem_type, depth + 1)?);
                }
                // Skip the remainder in one operation when the element width
                // is fixed. Element-by-element cost ~19s per GiB — each step
                // allocating and running its own `io::copy` — so a declared
                // multi-billion-element array of bytes (which passes the size
                // check, since one byte is one byte) turned a header read into
                // minutes on a blocking thread.
                let rest = len - previewed;
                match scalar_size(elem_type) {
                    Some(w) => {
                        let bytes = rest.checked_mul(w).ok_or(GgufError::TooLarge {
                            what: "an array payload",
                            len: rest,
                            remaining: self.remaining(),
                        })?;
                        self.check_fits("an array payload", bytes, 1)?;
                        self.discard(bytes, "an array payload")?;
                    }
                    // Strings and nested arrays have no fixed width.
                    None => {
                        for _ in 0..rest {
                            self.skip_value(elem_type)?;
                        }
                    }
                }
                GgufValue::Array {
                    elem_type,
                    len,
                    preview,
                }
            }
            other => return Err(GgufError::BadValueType(other)),
        })
    }

    /// Consume one value of `ty` without building a [`GgufValue`].
    ///
    /// Nested arrays are walked with an explicit stack instead of recursion:
    /// a crafted file can nest arbitrarily deep (each level costs only 12
    /// header bytes), and recursing there would be a stack overflow — i.e. a
    /// crash — rather than an error. The stack's depth is bounded by the file
    /// length, same as everything else here.
    fn skip_value(&mut self, ty: u32) -> Result<()> {
        // (element type, elements still to consume)
        let mut stack: Vec<(u32, u64)> = vec![(ty, 1)];
        while let Some(&(elem_type, todo)) = stack.last() {
            if todo == 0 {
                stack.pop();
                continue;
            }
            if let Some(top) = stack.last_mut() {
                top.1 -= 1;
            }
            match elem_type {
                VT_ARRAY => {
                    let inner_type = self.read_u32("an array element type")?;
                    let len = self.read_u64("an array length")?;
                    self.check_fits("an array", len, min_encoded_size(inner_type)?)?;
                    stack.push((inner_type, len));
                }
                VT_STRING => {
                    let len = self.read_u64("a string")?;
                    self.check_fits("a string", len, 1)?;
                    self.discard(len, "a string")?;
                }
                t => {
                    let n = scalar_size(t).ok_or(GgufError::BadValueType(t))?;
                    self.discard(n, "a value")?;
                }
            }
        }
        Ok(())
    }

    /// Read and throw away `n` bytes. Seeking is not an option: the values
    /// being skipped are variable-length, so their end is only known by
    /// walking them — and the underlying reader is a forward-only
    /// `BufReader` on purpose.
    fn discard(&mut self, n: u64, what: &'static str) -> Result<()> {
        let copied = std::io::copy(&mut self.inner.by_ref().take(n), &mut std::io::sink())?;
        self.pos += copied;
        if copied < n {
            return Err(GgufError::UnexpectedEof(what));
        }
        Ok(())
    }

    /// Read one array value, the type tag (`VT_ARRAY`) already consumed by
    /// the caller, and fold every element into a hash instead of retaining it
    /// — [`ladder`](crate::ladder)'s tokenizer-identity check (design §4.3
    /// rule 5) needs the vocabulary and merge tables *in full*, but
    /// `read_value`'s `preview` deliberately truncates exactly those arrays
    /// (`ARRAY_PREVIEW`/`NUMERIC_ARRAY_RETAIN`) because a quarter-million-entry
    /// table is too large to keep in memory just to compare two files.
    /// Streaming a hash costs the same one forward pass `read_value` already
    /// pays to skip an array it does not retain. Returns `(true length,
    /// hash)`; the hash only has to agree with itself within one run of lmgw
    /// (two files compared in the same validation call), never across
    /// versions or processes, so a plain [`DefaultHasher`](std::collections::hash_map::DefaultHasher)
    /// is enough.
    fn read_and_hash_array(&mut self) -> Result<(u64, u64)> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let elem_type = self.read_u32("an array element type")?;
        let len = self.read_u64("an array length")?;
        self.check_fits("an array", len, min_encoded_size(elem_type)?)?;
        let mut hasher = DefaultHasher::new();
        len.hash(&mut hasher);
        elem_type.hash(&mut hasher);
        for _ in 0..len {
            match elem_type {
                VT_STRING => self.read_string("an array string")?.hash(&mut hasher),
                VT_U8 => self.read_array::<1>("a u8")?[0].hash(&mut hasher),
                VT_I8 => (self.read_array::<1>("an i8")?[0] as i8).hash(&mut hasher),
                VT_U16 => u16::from_le_bytes(self.read_array::<2>("a u16")?).hash(&mut hasher),
                VT_I16 => i16::from_le_bytes(self.read_array::<2>("an i16")?).hash(&mut hasher),
                VT_U32 => self.read_u32("a u32")?.hash(&mut hasher),
                VT_I32 => i32::from_le_bytes(self.read_array::<4>("an i32")?).hash(&mut hasher),
                VT_U64 => self.read_u64("a u64")?.hash(&mut hasher),
                VT_I64 => i64::from_le_bytes(self.read_array::<8>("an i64")?).hash(&mut hasher),
                VT_BOOL => (self.read_array::<1>("a bool")?[0] != 0).hash(&mut hasher),
                VT_F32 => f32::from_le_bytes(self.read_array::<4>("an f32")?)
                    .to_bits()
                    .hash(&mut hasher),
                VT_F64 => f64::from_le_bytes(self.read_array::<8>("an f64")?)
                    .to_bits()
                    .hash(&mut hasher),
                // No known tokenizer array nests one, but folding its own
                // hash in is honest and cheap where refusing the whole file
                // would not be.
                VT_ARRAY => self.read_and_hash_array()?.hash(&mut hasher),
                other => return Err(GgufError::BadValueType(other)),
            }
        }
        Ok((len, hasher.finish()))
    }
}

/// Encoded byte size of a fixed-width value type, `None` for string/array.
fn scalar_size(ty: u32) -> Option<u64> {
    Some(match ty {
        VT_U8 | VT_I8 | VT_BOOL => 1,
        VT_U16 | VT_I16 => 2,
        VT_U32 | VT_I32 | VT_F32 => 4,
        VT_U64 | VT_I64 | VT_F64 => 8,
        _ => return None,
    })
}

/// Smallest number of bytes one value of `ty` can occupy — the multiplier that
/// turns a declared array length into a byte requirement we can bounds-check.
fn min_encoded_size(ty: u32) -> Result<u64> {
    match ty {
        VT_STRING => Ok(8), // u64 length, possibly zero bytes of text
        VT_ARRAY => Ok(12), // element type u32 + count u64
        t => scalar_size(t).ok_or(GgufError::BadValueType(t)),
    }
}

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
    /// Human name for [`file_type_id`](Self::file_type_id), see [`ftype_name`].
    pub quant: Option<String>,
    pub file_type_id: Option<u32>,
    /// `<arch>.context_length` — the model's trained maximum, which is the
    /// real ceiling callers should size context against.
    pub context_length: Option<u64>,
    /// `<arch>.block_count` — transformer layers.
    pub block_count: Option<u64>,
    pub embedding_length: Option<u64>,
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
    /// `<arch>.ssm.*` — the recurrent-state shape of those linear layers.
    pub ssm_conv_kernel: Option<u64>,
    pub ssm_state_size: Option<u64>,
    pub ssm_group_count: Option<u64>,
    pub ssm_inner_size: Option<u64>,
    pub rope_freq_base: Option<f64>,
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

        Self {
            architecture: meta.architecture().map(str::to_owned),
            general_name: meta.string("general.name"),
            general_type,
            size_label: meta.string("general.size_label"),
            quant: file_type_id.map(ftype_name),
            file_type_id,
            context_length: meta.arch_u64("context_length"),
            block_count: meta.arch_u64("block_count"),
            embedding_length: meta.arch_u64("embedding_length"),
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
            ssm_conv_kernel: meta.arch_u64("ssm.conv_kernel"),
            ssm_state_size: meta.arch_u64("ssm.state_size"),
            ssm_group_count: meta.arch_u64("ssm.group_count"),
            ssm_inner_size: meta.arch_u64("ssm.inner_size"),
            rope_freq_base: meta.arch_f64("rope.freq_base"),
            pooling_type: meta.arch_u64("pooling_type"),
            has_classifier_head,
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
            file_size: meta.file_size,
        }
    }
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
fn recurrent_state_bytes(s: &ModelSummary) -> Option<u64> {
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
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    // ---- synthetic GGUF builder -------------------------------------------

    /// Assembles a byte-exact GGUF header in memory so the parser can be
    /// tested against known-good *and* deliberately broken input.
    #[derive(Default)]
    struct Build {
        kv: Vec<u8>,
        kv_count: u64,
        tensors: Vec<u8>,
        tensor_count: u64,
    }

    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    impl Build {
        fn kv(&mut self, key: &str, ty: u32, body: &[u8]) -> &mut Self {
            put_str(&mut self.kv, key);
            self.kv.extend_from_slice(&ty.to_le_bytes());
            self.kv.extend_from_slice(body);
            self.kv_count += 1;
            self
        }
        fn u8(&mut self, k: &str, v: u8) -> &mut Self {
            self.kv(k, VT_U8, &[v])
        }
        fn i8(&mut self, k: &str, v: i8) -> &mut Self {
            self.kv(k, VT_I8, &v.to_le_bytes())
        }
        fn u16(&mut self, k: &str, v: u16) -> &mut Self {
            self.kv(k, VT_U16, &v.to_le_bytes())
        }
        fn i16(&mut self, k: &str, v: i16) -> &mut Self {
            self.kv(k, VT_I16, &v.to_le_bytes())
        }
        fn u32(&mut self, k: &str, v: u32) -> &mut Self {
            self.kv(k, VT_U32, &v.to_le_bytes())
        }
        fn i32(&mut self, k: &str, v: i32) -> &mut Self {
            self.kv(k, VT_I32, &v.to_le_bytes())
        }
        fn u64(&mut self, k: &str, v: u64) -> &mut Self {
            self.kv(k, VT_U64, &v.to_le_bytes())
        }
        fn i64(&mut self, k: &str, v: i64) -> &mut Self {
            self.kv(k, VT_I64, &v.to_le_bytes())
        }
        fn f32(&mut self, k: &str, v: f32) -> &mut Self {
            self.kv(k, VT_F32, &v.to_le_bytes())
        }
        fn f64(&mut self, k: &str, v: f64) -> &mut Self {
            self.kv(k, VT_F64, &v.to_le_bytes())
        }
        fn bool(&mut self, k: &str, v: bool) -> &mut Self {
            self.kv(k, VT_BOOL, &[v as u8])
        }
        fn str(&mut self, k: &str, v: &str) -> &mut Self {
            let mut b = Vec::new();
            put_str(&mut b, v);
            self.kv(k, VT_STRING, &b)
        }
        fn arr_str(&mut self, k: &str, vals: &[&str]) -> &mut Self {
            let mut b = VT_STRING.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for v in vals {
                put_str(&mut b, v);
            }
            self.kv(k, VT_ARRAY, &b)
        }
        fn arr_bool(&mut self, k: &str, vals: &[bool]) -> &mut Self {
            let mut b = VT_BOOL.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            b.extend(vals.iter().map(|v| *v as u8));
            self.kv(k, VT_ARRAY, &b)
        }
        fn arr_u32(&mut self, k: &str, vals: &[u32]) -> &mut Self {
            let mut b = VT_U32.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for v in vals {
                b.extend_from_slice(&v.to_le_bytes());
            }
            self.kv(k, VT_ARRAY, &b)
        }
        fn arr_i32(&mut self, k: &str, vals: &[i32]) -> &mut Self {
            let mut b = VT_I32.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for v in vals {
                b.extend_from_slice(&v.to_le_bytes());
            }
            self.kv(k, VT_ARRAY, &b)
        }
        /// `tokenizer.ggml.precompiled_charsmap`'s real on-disk shape
        /// (second-pass review finding S4): a GGUF array of `UINT8`, not a
        /// string.
        fn arr_u8(&mut self, k: &str, vals: &[u8]) -> &mut Self {
            let mut b = VT_U8.to_le_bytes().to_vec();
            b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            b.extend_from_slice(vals);
            self.kv(k, VT_ARRAY, &b)
        }
        /// `[[1u32, 2], [3]]` — the nested case the spec allows.
        fn arr_nested_u32(&mut self, k: &str, rows: &[&[u32]]) -> &mut Self {
            let mut b = VT_ARRAY.to_le_bytes().to_vec();
            b.extend_from_slice(&(rows.len() as u64).to_le_bytes());
            for row in rows {
                b.extend_from_slice(&VT_U32.to_le_bytes());
                b.extend_from_slice(&(row.len() as u64).to_le_bytes());
                for v in *row {
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
            self.kv(k, VT_ARRAY, &b)
        }
        fn tensor(&mut self, name: &str, dims: &[u64]) -> &mut Self {
            put_str(&mut self.tensors, name);
            self.tensors
                .extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for d in dims {
                self.tensors.extend_from_slice(&d.to_le_bytes());
            }
            self.tensors.extend_from_slice(&0u32.to_le_bytes()); // ggml type F32
            self.tensors.extend_from_slice(&0u64.to_le_bytes()); // offset
            self.tensor_count += 1;
            self
        }
        fn bytes(&self) -> Vec<u8> {
            let mut out = b"GGUF".to_vec();
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&self.tensor_count.to_le_bytes());
            out.extend_from_slice(&self.kv_count.to_le_bytes());
            out.extend_from_slice(&self.kv);
            out.extend_from_slice(&self.tensors);
            out
        }
        fn file(&self) -> tempfile::NamedTempFile {
            write_tmp(&self.bytes())
        }
    }

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    // ---- happy path -------------------------------------------------------

    #[test]
    fn every_scalar_type_round_trips() {
        let mut b = Build::default();
        b.u8("a.u8", 200)
            .i8("a.i8", -100)
            .u16("a.u16", 60_000)
            .i16("a.i16", -30_000)
            .u32("a.u32", 4_000_000_000)
            .i32("a.i32", -2_000_000_000)
            .u64("a.u64", u64::MAX)
            .i64("a.i64", i64::MIN)
            .f32("a.f32", 0.5)
            .f64("a.f64", -1.25)
            .bool("a.t", true)
            .bool("a.f", false)
            .str("a.str", "héllo ✓")
            .str("a.empty", "");
        let f = b.file();
        let m = read_header(f.path()).unwrap();

        assert_eq!(m.version, 3);
        assert_eq!(m.tensor_count, 0);
        assert_eq!(m.get("a.u8"), Some(&GgufValue::U8(200)));
        assert_eq!(m.get("a.i8"), Some(&GgufValue::I8(-100)));
        assert_eq!(m.get("a.u16"), Some(&GgufValue::U16(60_000)));
        assert_eq!(m.get("a.i16"), Some(&GgufValue::I16(-30_000)));
        assert_eq!(m.get("a.u32"), Some(&GgufValue::U32(4_000_000_000)));
        assert_eq!(m.get("a.i32"), Some(&GgufValue::I32(-2_000_000_000)));
        assert_eq!(m.get("a.u64"), Some(&GgufValue::U64(u64::MAX)));
        assert_eq!(m.get("a.i64"), Some(&GgufValue::I64(i64::MIN)));
        assert_eq!(m.get("a.f32"), Some(&GgufValue::F32(0.5)));
        assert_eq!(m.get("a.f64"), Some(&GgufValue::F64(-1.25)));
        assert_eq!(m.get("a.t").unwrap().as_bool(), Some(true));
        assert_eq!(m.get("a.f").unwrap().as_bool(), Some(false));
        assert_eq!(m.get("a.str").unwrap().as_str(), Some("héllo ✓"));
        assert_eq!(m.get("a.empty").unwrap().as_str(), Some(""));

        // Widening accessors: any int width reads as u64, negatives refuse.
        assert_eq!(m.u64("a.u16"), Some(60_000));
        assert_eq!(m.u64("a.i32"), None);
        assert_eq!(m.get("a.i32").unwrap().as_f64(), Some(-2_000_000_000.0));
        assert_eq!(m.u64("a.str"), None);
        assert_eq!(m.u64("a.missing"), None);
    }

    #[test]
    fn arrays_keep_a_preview_and_the_true_length() {
        let big: Vec<String> = (0..1000).map(|i| format!("tok{i}")).collect();
        let refs: Vec<&str> = big.iter().map(String::as_str).collect();
        let mut b = Build::default();
        b.arr_str("tokenizer.ggml.tokens", &refs)
            .arr_bool("a.flags", &[true, false, true])
            .arr_str("a.empty", &[])
            .arr_nested_u32("a.nested", &[&[1, 2, 3], &[4], &[]])
            .str("z.after", "still parsed"); // arrays must not desync the stream
        let f = b.file();
        let m = read_header(f.path()).unwrap();

        let GgufValue::Array {
            elem_type,
            len,
            preview,
        } = m.get("tokenizer.ggml.tokens").unwrap()
        else {
            panic!("expected an array")
        };
        assert_eq!(*elem_type, VT_STRING);
        assert_eq!(*len, 1000);
        assert_eq!(preview.len(), ARRAY_PREVIEW);
        assert_eq!(preview[0].as_str(), Some("tok0"));
        assert_eq!(preview[7].as_str(), Some("tok7"));

        assert_eq!(m.get("a.flags").unwrap().array_len(), Some(3));
        assert_eq!(m.get("a.empty").unwrap().array_len(), Some(0));
        assert_eq!(m.get("a.nested").unwrap().array_len(), Some(3));
        assert_eq!(m.get("z.after").unwrap().as_str(), Some("still parsed"));
    }

    #[test]
    fn tensor_descriptors_yield_names_in_file_order() {
        let mut b = Build::default();
        b.str("general.architecture", "foo")
            .tensor("token_embd.weight", &[4096, 32000])
            .tensor("blk.0.attn_q.weight", &[4096, 4096])
            .tensor("scalar", &[]);
        let f = b.file();
        let m = read_header(f.path()).unwrap();
        assert_eq!(m.tensor_count, 3);
        assert_eq!(
            m.tensor_names,
            ["token_embd.weight", "blk.0.attn_q.weight", "scalar"]
        );
    }

    // ---- robustness -------------------------------------------------------

    #[test]
    fn bad_magic_is_rejected() {
        let f = write_tmp(b"NOTGGUF\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::BadMagic(m)) if &m == b"NOTG"
        ));
        // An empty file cannot even supply the magic.
        let empty = write_tmp(b"");
        assert!(matches!(
            read_header(empty.path()),
            Err(GgufError::UnexpectedEof(_))
        ));
    }

    #[test]
    fn truncated_header_reports_eof_not_panic() {
        let f = write_tmp(b"GGUF\x03\x00\x00\x00");
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::UnexpectedEof(_))
        ));
    }

    #[test]
    fn absurd_lengths_are_rejected_against_the_real_file_size() {
        // A key whose declared length is the whole u64 space.
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&1u64.to_le_bytes()); // kv_count
        bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // key length
        let f = write_tmp(&bytes);
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::TooLarge { .. })
        ));

        // A KV count that could never fit in the file.
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        let f = write_tmp(&bytes);
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::TooLarge { .. })
        ));

        // An array claiming 2^60 strings.
        let mut body = VT_STRING.to_le_bytes().to_vec();
        body.extend_from_slice(&(1u64 << 60).to_le_bytes());
        let mut b = Build::default();
        b.kv("a.arr", VT_ARRAY, &body);
        let f = write_tmp(&b.bytes());
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::TooLarge { .. })
        ));
    }

    #[test]
    fn unknown_value_type_is_an_error() {
        let mut b = Build::default();
        b.kv("a.weird", 99, &[0u8; 4]);
        let f = write_tmp(&b.bytes());
        assert!(matches!(
            read_header(f.path()),
            Err(GgufError::BadValueType(99))
        ));
    }

    #[test]
    fn no_truncation_of_a_valid_file_panics_or_hangs() {
        // Every prefix of a good header must produce a value or a typed error,
        // never a panic and never an unbounded read.
        let mut b = Build::default();
        b.str("general.architecture", "foo")
            .u32("foo.block_count", 12)
            .arr_str("tokenizer.ggml.tokens", &["a", "b", "c", "d"])
            .arr_nested_u32("foo.nested", &[&[1, 2], &[3]])
            .tensor("blk.0.attn_q.weight", &[16, 16]);
        let full = b.bytes();
        for cut in 0..full.len() {
            let f = write_tmp(&full[..cut]);
            let _ = read_header(f.path()); // must not panic
        }
        // Flipping the high bit of each byte must not panic either — this is
        // where a length field turns into something absurd.
        for i in 0..full.len() {
            let mut corrupt = full.clone();
            corrupt[i] ^= 0x80;
            let f = write_tmp(&corrupt);
            let _ = read_header(f.path());
        }
    }

    #[test]
    fn deeply_nested_arrays_do_not_blow_the_stack() {
        // 20k nested single-element arrays: skipping must be iterative.
        const DEPTH: usize = 20_000;
        let mut body = Vec::new();
        for _ in 0..DEPTH {
            body.extend_from_slice(&VT_ARRAY.to_le_bytes());
            body.extend_from_slice(&1u64.to_le_bytes());
        }
        body.extend_from_slice(&VT_U32.to_le_bytes());
        body.extend_from_slice(&1u64.to_le_bytes());
        body.extend_from_slice(&7u32.to_le_bytes());
        let mut b = Build::default();
        b.kv("a.deep", VT_ARRAY, &body);
        let f = write_tmp(&b.bytes());
        // Beyond MAX_PREVIEW_DEPTH the value is parsed but not retained.
        let m = read_header(f.path()).unwrap();
        assert_eq!(m.get("a.deep").unwrap().array_len(), Some(1));
    }

    // ---- summary ----------------------------------------------------------

    #[test]
    fn embedders_declare_pooling_and_rerankers_a_classifier_head() {
        let f = write_tmp(&synth::embedding("qwen3", 3, 32768).bytes());
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.pooling_type, Some(3));
        assert_eq!(s.pooling_type.and_then(pooling_name), Some("last"));
        assert!(!s.has_classifier_head);
        assert!(!s.has_chat_template);

        let mut h = synth::embedding("bert", 4, 8192);
        h.tensor("cls.output.weight").tensor("cls.output.bias");
        let f = write_tmp(&h.bytes());
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.pooling_type.and_then(pooling_name), Some("rank"));
        assert!(s.has_classifier_head);

        // A chat model says nothing about pooling, and an unknown enum value
        // is reported as unknown rather than mapped to a guess.
        let f = write_tmp(&synth::chat("qwen35", 4096).bytes());
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.pooling_type, None);
        assert_eq!(pooling_name(99), None);
    }

    #[test]
    fn arch_prefixed_keys_resolve_through_general_architecture() {
        let mut b = Build::default();
        b.str("general.architecture", "foo")
            .str("general.name", "Foo 7B")
            .str("general.type", "model")
            .str("general.size_label", "7B")
            .u32("general.file_type", 15)
            .u32("foo.context_length", 131_072)
            .u32("foo.block_count", 32)
            .u32("foo.embedding_length", 4096)
            .u32("foo.attention.head_count", 32)
            .u32("foo.attention.head_count_kv", 8)
            .u32("foo.attention.key_length", 128)
            .f32("foo.rope.freq_base", 500_000.0)
            // Same suffix under a *different* architecture must be ignored.
            .u32("bar.context_length", 2048)
            .str("tokenizer.chat_template", "{{ messages }}");
        let f = b.file();
        let s = summarize(f.path()).unwrap();

        assert_eq!(s.architecture.as_deref(), Some("foo"));
        assert_eq!(s.general_name.as_deref(), Some("Foo 7B"));
        assert_eq!(s.size_label.as_deref(), Some("7B"));
        assert_eq!(s.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(s.file_type_id, Some(15));
        assert_eq!(s.context_length, Some(131_072));
        assert_eq!(s.block_count, Some(32));
        assert_eq!(s.embedding_length, Some(4096));
        assert_eq!(s.head_count, Some(32));
        assert_eq!(s.head_count_kv, Some(8));
        assert_eq!(s.key_length, Some(128));
        // Absent keys stay None rather than being defaulted from a sibling.
        assert_eq!(s.value_length, None);
        assert_eq!(s.sliding_window, None);
        assert_eq!(s.rope_freq_base, Some(500_000.0));
        assert!(s.has_chat_template);
        assert_eq!(s.chat_template.as_deref(), Some("{{ messages }}"));
        assert!(!s.is_mmproj);
        assert!(!s.has_mtp_layers);
        assert!(s.file_size > 0);
    }

    #[test]
    fn without_an_architecture_prefixed_keys_are_not_guessed() {
        let mut b = Build::default();
        b.u32("foo.context_length", 4096);
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.architecture, None);
        assert_eq!(s.context_length, None);
    }

    #[test]
    fn blank_chat_template_does_not_count_as_present() {
        let mut b = Build::default();
        b.str("tokenizer.chat_template", "   \n ");
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert!(!s.has_chat_template);
        assert_eq!(s.chat_template, None);
    }

    #[test]
    fn mtp_tensors_are_detected_by_name() {
        let mut b = Build::default();
        b.str("general.architecture", "qwen35")
            .tensor("blk.63.attn_q.weight", &[16, 16])
            .tensor("blk.64.nextn.eh_proj.weight", &[16, 16])
            .tensor("blk.64.nextn.enorm.weight", &[16])
            .tensor("some.MTP.head", &[16]);
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert!(s.has_mtp_layers);
        assert_eq!(
            s.mtp_tensor_names,
            [
                "blk.64.nextn.eh_proj.weight",
                "blk.64.nextn.enorm.weight",
                "some.MTP.head",
            ]
        );

        let mut plain = Build::default();
        plain.tensor("blk.0.attn_q.weight", &[16, 16]);
        let f = plain.file();
        let s = summarize(f.path()).unwrap();
        assert!(!s.has_mtp_layers);
        assert!(s.mtp_tensor_names.is_empty());
    }

    #[test]
    fn mmproj_is_detected_three_ways() {
        let mut by_type = Build::default();
        by_type
            .str("general.architecture", "clip")
            .str("general.type", "mmproj");
        let f = by_type.file();
        let s = summarize(f.path()).unwrap();
        assert!(s.is_mmproj);
        assert_eq!(s.general_type.as_deref(), Some("mmproj"));

        let mut by_encoder = Build::default();
        by_encoder
            .str("general.type", "model")
            .bool("clip.has_vision_encoder", true)
            .u32("clip.vision.block_count", 50);
        let f = by_encoder.file();
        let s = summarize(f.path()).unwrap();
        assert!(s.is_mmproj);
        assert_eq!(s.vision_block_count, Some(50));

        let mut by_projector = Build::default();
        by_projector.str("clip.projector_type", "gemma3");
        let f = by_projector.file();
        let s = summarize(f.path()).unwrap();
        assert!(s.is_mmproj);
        assert_eq!(s.projector_type.as_deref(), Some("gemma3"));
    }

    /// Gemma 4's mmproj carries a vision and an audio projector and names each
    /// under its own modality key, with no `clip.projector_type` at all. The
    /// vision one is the type images are decoded with.
    #[test]
    fn a_two_modality_projector_is_typed_by_its_vision_key() {
        let mut b = Build::default();
        b.str("general.architecture", "clip")
            .str("general.type", "mmproj")
            .str("clip.vision.projector_type", "gemma4uv")
            .str("clip.audio.projector_type", "gemma4ua");
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert!(s.is_mmproj);
        assert_eq!(s.projector_type.as_deref(), Some("gemma4uv"));

        // The shared key, where a file has it, is what llama.cpp reads first.
        let mut both = Build::default();
        both.str("clip.projector_type", "qwen3vl_merger")
            .str("clip.vision.projector_type", "gemma4v");
        let f = both.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.projector_type.as_deref(), Some("qwen3vl_merger"));
    }

    #[test]
    fn file_types_map_to_quant_names() {
        assert_eq!(ftype_name(0), "F32");
        assert_eq!(ftype_name(1), "F16");
        assert_eq!(ftype_name(7), "Q8_0");
        assert_eq!(ftype_name(14), "Q4_K_S");
        assert_eq!(ftype_name(15), "Q4_K_M");
        assert_eq!(ftype_name(18), "Q6_K");
        assert_eq!(ftype_name(30), "IQ4_XS");
        assert_eq!(ftype_name(32), "BF16");
        assert_eq!(ftype_name(999), "ftype999");

        let mut b = Build::default();
        b.u32("general.file_type", 4242);
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.quant.as_deref(), Some("ftype4242"));
        assert_eq!(s.file_type_id, Some(4242));

        // No file_type at all → no invented quant name.
        let f = Build::default().file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.quant, None);
    }

    #[test]
    fn sliding_window_pattern_reads_as_int_or_as_array() {
        let mut int_form = Build::default();
        int_form
            .str("general.architecture", "muse-glimmer")
            .u32("muse-glimmer.attention.sliding_window", 2048)
            .u32("muse-glimmer.attention.sliding_window_pattern", 4);
        let f = int_form.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.sliding_window, Some(2048));
        assert_eq!(s.sliding_window_pattern, Some(4));
        assert!(!s.sliding_window_pattern_is_array);

        let mut arr_form = Build::default();
        arr_form
            .str("general.architecture", "dflash")
            .u32("dflash.attention.sliding_window", 2048)
            .arr_bool(
                "dflash.attention.sliding_window_pattern",
                &[true, true, true, true, true],
            );
        let f = arr_form.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.sliding_window_pattern, Some(5)); // the array length
        assert!(s.sliding_window_pattern_is_array);
        // Short enough to have been retained in full, so it is also usable.
        assert_eq!(s.sliding_window_layers, Some(vec![true; 5]));
    }

    #[test]
    fn numeric_arrays_are_retained_in_full_up_to_the_cap() {
        let small: Vec<u32> = (0..48).collect();
        let big: Vec<u32> = (0..=NUMERIC_ARRAY_RETAIN as u32).collect();
        let mut b = Build::default();
        b.arr_u32("a.small", &small).arr_u32("a.big", &big);
        let f = b.file();
        let m = read_header(f.path()).unwrap();

        let got = m.get("a.small").unwrap().as_u64_array().unwrap();
        assert_eq!(got, (0..48).collect::<Vec<u64>>());

        // One past the cap: preview retention, and no partially-materialized
        // array pretending to be the whole table.
        let big_v = m.get("a.big").unwrap();
        assert_eq!(big_v.array_len(), Some(NUMERIC_ARRAY_RETAIN + 1));
        assert_eq!(big_v.as_u64_array(), None);
        let GgufValue::Array { preview, .. } = big_v else {
            panic!("expected an array")
        };
        assert_eq!(preview.len(), ARRAY_PREVIEW);
    }

    #[test]
    fn per_layer_attention_arrays_are_materialized() {
        // gemma4's encoding: head_count_kv and sliding_window_pattern as one
        // entry per block, plus separate head dims for the windowed layers.
        let full = |i: u32| (i + 1).is_multiple_of(6);
        let heads: Vec<u32> = (0..48).map(|i| if full(i) { 1 } else { 8 }).collect();
        let pattern: Vec<u32> = (0..48).map(|i| u32::from(!full(i))).collect();
        let mut b = Build::default();
        b.str("general.architecture", "gemma4")
            .u32("gemma4.block_count", 48)
            .arr_u32("gemma4.attention.head_count_kv", &heads)
            .arr_u32("gemma4.attention.sliding_window_pattern", &pattern)
            .u32("gemma4.attention.sliding_window", 1024)
            .u32("gemma4.attention.key_length", 512)
            .u32("gemma4.attention.value_length", 512)
            .u32("gemma4.attention.key_length_swa", 256)
            .u32("gemma4.attention.value_length_swa", 256);
        let f = b.file();
        let s = summarize(f.path()).unwrap();

        // The scalar is not invented from the array; the array is complete.
        assert_eq!(s.head_count_kv, None);
        let got = s.head_count_kv_per_layer.as_ref().unwrap();
        assert_eq!(got.len(), 48);
        assert_eq!((got[0], got[5]), (8, 1));
        let w = s.sliding_window_layers.as_ref().unwrap();
        assert!(w[0] && !w[5]);
        assert_eq!(s.key_length_swa, Some(256));
        assert_eq!(s.value_length_swa, Some(256));
        // The collapsed bookkeeping fields keep their meaning.
        assert!(s.sliding_window_pattern_is_array);
        assert_eq!(s.sliding_window_pattern, Some(48));
    }

    // ---- KV cache ---------------------------------------------------------

    fn kv_model(block_count: u64, heads_kv: u64, head_dim: u64) -> ModelSummary {
        ModelSummary {
            block_count: Some(block_count),
            head_count_kv: Some(heads_kv),
            key_length: Some(head_dim),
            value_length: Some(head_dim),
            ..Default::default()
        }
    }

    #[test]
    fn kv_cache_full_attention_is_the_textbook_formula() {
        let m = kv_model(32, 8, 128);
        // 32 layers * 4096 tokens * 8 kv heads * (128+128) * 16 bits / 8
        let expect = 32u64 * 4096 * 8 * 256 * 16 / 8;
        assert_eq!(kv_cache_bytes(&m, 4096, 16), Some(expect));
        assert_eq!(expect, 536_870_912); // 512 MiB at f16

        // q8_0 halves it (a lower bound — block scales are not counted).
        assert_eq!(kv_cache_bytes(&m, 4096, 8), Some(expect / 2));
        // Linear in context.
        assert_eq!(kv_cache_bytes(&m, 8192, 16), Some(expect * 2));
    }

    #[test]
    fn sliding_window_layers_shrink_the_cache_a_lot() {
        // Muse-Glimmer's shape: 52 layers, 2 KV heads, 128-dim, 2048 window,
        // every 4th layer full attention.
        let mut m = kv_model(52, 2, 128);
        m.sliding_window = Some(2048);
        m.sliding_window_pattern = Some(4);

        let ctx = 131_072;
        let swa = kv_cache_bytes(&m, ctx, 16).unwrap();

        let mut full = m.clone();
        full.sliding_window = None;
        full.sliding_window_pattern = None;
        let full = kv_cache_bytes(&full, ctx, 16).unwrap();

        // 13 full layers at 131072 tokens + 39 windowed layers at 2048.
        let per_layer_token = 2 * (128 + 128) * 16 / 8;
        assert_eq!(swa, (13 * ctx + 39 * 2048) * per_layer_token);
        assert_eq!(full, 52 * ctx * per_layer_token);
        // 6.5 GiB → 1.7 GiB, ~3.8x smaller: the whole reason a 128k context on
        // an SWA model fits in a 24 GB card at all.
        assert!(full > swa * 3, "full={full} swa={swa}");

        // Below the window size the two are identical — a windowed layer never
        // holds more than the context itself.
        assert_eq!(
            kv_cache_bytes(&m, 1024, 16),
            kv_cache_bytes(&full_of(&m), 1024, 16)
        );
    }

    #[test]
    fn hybrid_linear_attention_keys_are_read() {
        let mut b = Build::default();
        b.str("general.architecture", "qwen35")
            .u32("qwen35.full_attention_interval", 4)
            .u32("qwen35.ssm.conv_kernel", 4)
            .u32("qwen35.ssm.state_size", 128)
            .u32("qwen35.ssm.group_count", 16)
            .u32("qwen35.ssm.inner_size", 6144);
        let f = b.file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.full_attention_interval, Some(4));
        assert_eq!(s.ssm_conv_kernel, Some(4));
        assert_eq!(s.ssm_state_size, Some(128));
        assert_eq!(s.ssm_group_count, Some(16));
        assert_eq!(s.ssm_inner_size, Some(6144));
    }

    #[test]
    fn hybrid_linear_attention_charges_only_the_full_layers() {
        // Qwen3.8-27B's shape: 65 blocks, every 4th full attention, 4 KV
        // heads, 256-dim K and V, DeltaNet state 6144x128 with 16 groups.
        let mut m = kv_model(65, 4, 256);
        m.full_attention_interval = Some(4);
        m.ssm_conv_kernel = Some(4);
        m.ssm_state_size = Some(128);
        m.ssm_group_count = Some(16);
        m.ssm_inner_size = Some(6144);

        let ctx = 130_000;
        let got = kv_cache_bytes(&m, ctx, 8).unwrap();
        // 16 full layers at ctx tokens, q8_0 = 1 byte/element…
        let per_layer_token = 4 * (256 + 256);
        // …plus 49 linear layers at their fixed f32 recurrent state.
        let recurrent = (3 * (6144 + 2 * 16 * 128) + 128 * 6144) * 4;
        assert_eq!(got, 16 * ctx * per_layer_token + 49 * recurrent);
        // ~4.1 GiB — the all-full-attention formula would claim ~16 GiB and
        // wrongly refuse to start a model that fits a 24 GB card.
        assert!(got < 5 << 30, "got={got}");
        assert!(kv_cache_bytes(&full_of(&m), ctx, 8).unwrap() > 15 << 30);

        // The recurrent term does not scale with context or cache type.
        assert_eq!(
            kv_cache_bytes(&m, 2 * ctx, 8).unwrap() - got,
            16 * ctx * per_layer_token
        );

        // Without the ssm.* shape the linear layers charge nothing — still a
        // lower bound, never an overcount.
        m.ssm_state_size = None;
        assert_eq!(kv_cache_bytes(&m, ctx, 8), Some(16 * ctx * per_layer_token));
    }

    #[test]
    fn per_layer_head_counts_size_each_layer_with_its_own_shape() {
        // gemma4-12B's shape: 48 blocks, every 6th full attention with one
        // 512-dim KV head, the windowed rest with eight 256-dim KV heads and
        // a 1024-token window.
        let full = |i: usize| (i + 1).is_multiple_of(6);
        let mut m = kv_model(48, 8, 512);
        m.head_count_kv = None; // the array encoding replaces the scalar
        m.head_count_kv_per_layer = Some((0..48).map(|i| if full(i) { 1 } else { 8 }).collect());
        m.sliding_window_layers = Some((0..48).map(|i| !full(i)).collect());
        m.sliding_window = Some(1024);
        m.key_length_swa = Some(256);
        m.value_length_swa = Some(256);

        let ctx = 262_144;
        let got = kv_cache_bytes(&m, ctx, 16).unwrap();
        // 8 full layers: 1 head x (512+512) x ctx tokens; 40 windowed layers:
        // 8 heads x (256+256) x 1024 tokens; f16 = 2 bytes per element.
        assert_eq!(got, (8 * ctx * 1024 + 40 * 1024 * 4096) * 2);
        // ~4.3 GiB, where reading the header as 48 uniform full-attention
        // layers was impossible before (scalar head_count_kv absent) and left
        // this model sized as weights-only.
        assert!(got < 5 << 30, "got={got}");

        // A heads array that does not match block_count describes some other
        // model: decline, don't guess.
        m.head_count_kv_per_layer = Some(vec![8; 47]);
        assert_eq!(kv_cache_bytes(&m, ctx, 16), None);
    }

    /// Same model with the SWA and hybrid fields stripped, for comparisons.
    fn full_of(m: &ModelSummary) -> ModelSummary {
        ModelSummary {
            sliding_window: None,
            sliding_window_pattern: None,
            full_attention_interval: None,
            ..m.clone()
        }
    }

    #[test]
    fn kv_cache_declines_to_guess_missing_fields() {
        let mut m = kv_model(32, 8, 128);
        m.key_length = None;
        assert_eq!(kv_cache_bytes(&m, 4096, 16), None);

        let mut m = kv_model(32, 8, 128);
        m.head_count_kv = None;
        assert_eq!(kv_cache_bytes(&m, 4096, 16), None);

        assert_eq!(kv_cache_bytes(&ModelSummary::default(), 4096, 16), None);

        // The bool-array pattern form is not a stride, so it must not be used
        // as one; the answer falls back to all-full-attention.
        let mut m = kv_model(52, 2, 128);
        m.sliding_window = Some(2048);
        m.sliding_window_pattern = Some(5);
        m.sliding_window_pattern_is_array = true;
        assert_eq!(
            kv_cache_bytes(&m, 131_072, 16),
            kv_cache_bytes(&full_of(&m), 131_072, 16)
        );

        // Absurd inputs saturate to None instead of overflowing.
        assert_eq!(
            kv_cache_bytes(&kv_model(u64::MAX, u64::MAX, u64::MAX), u64::MAX, 16),
            None
        );
    }

    // ---- real files (skipped when not present, e.g. in CI) ----------------

    /// `Some(path)` when the file is on this machine, `None` (with a note) when
    /// it is not — these are 1–18 GB downloads that only exist locally. `rel`
    /// is below `<LMGW_TEST_MODELS_DIR>/unsloth`; without the variable the
    /// test skips.
    fn local(rel: &str) -> Option<PathBuf> {
        let Some(root) = std::env::var_os("LMGW_TEST_MODELS_DIR") else {
            eprintln!("skipping: LMGW_TEST_MODELS_DIR is not set ({rel})");
            return None;
        };
        let p = PathBuf::from(root).join("unsloth").join(rel);
        if p.is_file() {
            Some(p)
        } else {
            eprintln!(
                "skipping: {} is not present (LMGW_TEST_MODELS_DIR)",
                p.display()
            );
            None
        }
    }

    #[test]
    fn real_muse_glimmer_30b() {
        let Some(p) = local("Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert_eq!(s.architecture.as_deref(), Some("muse-glimmer"));
        assert_eq!(s.context_length, Some(131_072));
        assert_eq!(s.block_count, Some(52));
        assert_eq!(s.head_count_kv, Some(2));
        assert_eq!(s.sliding_window, Some(2048));
        assert_eq!(s.sliding_window_pattern, Some(4));
        assert!(!s.sliding_window_pattern_is_array);
        assert!(s.has_chat_template);
        assert_eq!(s.general_type.as_deref(), Some("model"));
        assert_eq!(s.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(s.key_length, Some(128));
        assert!(!s.is_mmproj);
        assert!(!s.has_mtp_layers);
        // Reading the header must not have read the 15 GB of weights.
        assert!(s.file_size > 15_000_000_000);
        assert!(kv_cache_bytes(&s, 131_072, 16).unwrap() < 5 << 30);
    }

    #[test]
    fn real_muse_glimmer_mmproj() {
        let Some(p) = local("Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert!(s.is_mmproj);
        assert_eq!(s.projector_type.as_deref(), Some("muse-glimmer"));
        assert_eq!(s.general_type.as_deref(), Some("mmproj"));
        assert_eq!(s.vision_block_count, Some(50));
        // mmproj files declare `clip` as their architecture, so the
        // arch-prefixed model fields are legitimately absent.
        assert_eq!(s.architecture.as_deref(), Some("clip"));
        assert_eq!(s.context_length, None);
    }

    #[test]
    fn real_muse_glimmer_dflash_draft() {
        let Some(p) = local("Muse-Glimmer-30B-GGUF/dflash-kquant.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert_eq!(s.architecture.as_deref(), Some("dflash"));
        assert_eq!(s.block_count, Some(5));
        assert!(!s.is_mmproj);
        // This one ships the *array* encoding of the pattern.
        assert!(s.sliding_window_pattern_is_array);
        assert_eq!(s.sliding_window_pattern, Some(5));
    }

    #[test]
    fn real_qwen38_27b_has_mtp_layers() {
        let Some(p) = local("Qwen3.8-27B-GGUF/Qwen3.8-27B-UD-Q4_K_XL.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert!(s.has_mtp_layers);
        assert!(s
            .mtp_tensor_names
            .iter()
            .any(|n| n == "blk.64.nextn.eh_proj.weight"));
        assert_eq!(s.architecture.as_deref(), Some("qwen35"));
        assert_eq!(s.context_length, Some(262_144));

        // Hybrid linear attention: 65 blocks but only every 4th holds a
        // per-token KV cache. At 130k ctx / q8_0 that is ~4.1 GiB — the
        // all-full-attention formula claimed ~16 GiB and made admission
        // refuse a model that actually fits a 24 GB card.
        assert_eq!(s.full_attention_interval, Some(4));
        assert_eq!(s.block_count, Some(65));
        let kv = kv_cache_bytes(&s, 130_000, 8).unwrap();
        assert_eq!(kv, 16 * 130_000 * 2048 + 49 * 3_268_608);
    }

    #[test]
    fn real_gemma4_12b_sizes_per_layer() {
        let Some(p) = local("gemma-4-12B-it-qat-GGUF/gemma-4-12B-it-qat-UD-Q4_K_XL.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert_eq!(s.architecture.as_deref(), Some("gemma4"));
        assert_eq!(s.block_count, Some(48));

        // head_count_kv ships as a per-layer array (8 windowed / 1 full), so
        // the scalar is legitimately absent — which used to leave this model
        // "KV cache not sized" and admitted on weights alone.
        assert_eq!(s.head_count_kv, None);
        let heads = s.head_count_kv_per_layer.as_ref().unwrap();
        assert_eq!(heads.len(), 48);
        assert_eq!(heads.iter().filter(|&&h| h == 1).count(), 8);
        let w = s.sliding_window_layers.as_ref().unwrap();
        assert_eq!(w.iter().filter(|&&b| !b).count(), 8);
        assert_eq!(s.sliding_window, Some(1024));
        assert_eq!((s.key_length, s.key_length_swa), (Some(512), Some(256)));

        // 8 full layers x 1 head x 1024 dims x 262144 tokens + 40 windowed
        // layers x 8 heads x 512 dims x 1024 tokens, at f16 — ~4.3 GiB.
        let kv = kv_cache_bytes(&s, 262_144, 16).unwrap();
        assert_eq!(kv, (8 * 262_144 * 1024 + 40 * 1024 * 4096) * 2);
    }

    #[test]
    fn real_qwen35_9b_has_no_mtp_layers() {
        let Some(p) = local("Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf") else {
            return;
        };
        let s = summarize(&p).unwrap();
        assert!(!s.has_mtp_layers);
        assert!(s.mtp_tensor_names.is_empty());
        assert_eq!(s.architecture.as_deref(), Some("qwen35"));
        assert_eq!(s.block_count, Some(32));
        assert!(s.has_chat_template);
    }

    // ---- TemplateSignals: real templates (model capabilities design §3.1) -

    const QWEN38_TPL: &str = include_str!("../tests/fixtures/chat_templates/qwen3.8.jinja");
    const QWEN36_TPL: &str = include_str!("../tests/fixtures/chat_templates/qwen3.6.jinja");
    const GEMMA4_TPL: &str = include_str!("../tests/fixtures/chat_templates/gemma4.jinja");
    const DEEPSEEK_V4_FLASH_TPL: &str =
        include_str!("../tests/fixtures/chat_templates/deepseek-v4-flash.jinja");
    const COHERE_NORTH_TPL: &str =
        include_str!("../tests/fixtures/chat_templates/cohere-north.jinja");
    const LFM2_TPL: &str = include_str!("../tests/fixtures/chat_templates/lfm2.jinja");
    const QWEN3VL_TPL: &str = include_str!("../tests/fixtures/chat_templates/qwen3vl.jinja");
    const MEDGEMMA_TPL: &str = include_str!("../tests/fixtures/chat_templates/medgemma.jinja");
    const MISTRAL3_TPL: &str = include_str!("../tests/fixtures/chat_templates/mistral3.jinja");
    const MUSE_GLIMMER_TPL: &str =
        include_str!("../tests/fixtures/chat_templates/muse-glimmer.jinja");

    #[test]
    fn qwen38_signals() {
        let s = TemplateSignals::from_template(QWEN38_TPL);
        assert!(s.thinking_markers);
        assert_eq!(s.thinking_marker.as_deref(), Some("<think>"));
        assert!(s.enable_thinking_var);
        assert_eq!(s.enable_thinking_default, Some(true));
        assert!(s.reasoning_effort_var);
        // Both spellings: the request-settable `reasoning_effort` and the
        // `resolved_reasoning_effort` the template derives from it. A caller
        // reads this to know an effort it sends actually lands (§2.1
        // `control`).
        assert_eq!(
            s.effort_var_names,
            ["reasoning_effort", "resolved_reasoning_effort"]
        );
        assert_eq!(s.effort_levels, ["low", "medium", "high", "xhigh"]);
        assert_eq!(s.effort_default.as_deref(), Some("xhigh"));
        assert!(s.preserve_thinking_var);
        assert!(s.tools_var);
        assert!(s.parallel_tool_calls);
        assert_eq!(s.tool_call_format.as_deref(), Some("qwen-xml"));
    }

    #[test]
    fn qwen36_signals() {
        let s = TemplateSignals::from_template(QWEN36_TPL);
        assert!(s.thinking_markers);
        assert_eq!(s.thinking_marker.as_deref(), Some("<think>"));
        assert!(s.enable_thinking_var);
        // Unlike Qwen3.8, this template's only `enable_thinking` reference is
        // `enable_thinking is defined and enable_thinking is false` (guarding
        // whether to print an empty `<think></think>` before generation) —
        // neither the `| default(...)` form nor the "on unless defined false"
        // guard, so there is no stated default here. (An expectation of "same as
        // Qwen3.8" would assume the guard is present; it is not.)
        assert_eq!(s.enable_thinking_default, None);
        assert!(!s.reasoning_effort_var);
        assert!(s.effort_levels.is_empty());
        assert_eq!(s.effort_default, None);
        assert!(s.preserve_thinking_var);
        assert!(s.tools_var);
        assert!(s.parallel_tool_calls);
        assert_eq!(s.tool_call_format.as_deref(), Some("qwen-xml"));
    }

    #[test]
    fn gemma4_signals() {
        let s = TemplateSignals::from_template(GEMMA4_TPL);
        assert_eq!(s.thinking_marker.as_deref(), Some("<|channel>thought"));
        assert!(s.enable_thinking_var);
        assert_eq!(s.enable_thinking_default, Some(false));
        assert!(!s.reasoning_effort_var);
        assert!(s.preserve_thinking_var);
        assert!(s.tools_var);
        assert_eq!(s.tool_call_format.as_deref(), Some("gemma"));
    }

    #[test]
    fn deepseek_v4_flash_signals() {
        let s = TemplateSignals::from_template(DEEPSEEK_V4_FLASH_TPL);
        assert!(s.enable_thinking_var);
        assert_eq!(s.thinking_marker.as_deref(), Some("<think>"));
        assert!(!s.reasoning_effort_var);
        assert!(s.tools_var);
    }

    #[test]
    fn cohere_north_signals() {
        let s = TemplateSignals::from_template(COHERE_NORTH_TPL);
        assert!(s.reasoning_effort_var);
        assert!(s.effort_levels.is_empty());
        // No `<think>`-style tag — the trace reaches the output only via
        // `{{ msg.reasoning_content }}` inside `print_thinking`.
        assert!(s.thinking_markers);
        assert_eq!(s.thinking_marker.as_deref(), Some("reasoning_content"));
    }

    #[test]
    fn lfm2_signals() {
        let s = TemplateSignals::from_template(LFM2_TPL);
        assert_eq!(s.tool_call_format.as_deref(), Some("lfm2"));
        assert!(s.preserve_thinking_var);
        assert!(s.thinking_markers);
        assert!(s.tools_var);
        assert!(s.parallel_tool_calls);
    }

    #[test]
    fn qwen3vl_signals() {
        // Confirms the fixture actually renders `<tool_call>\n{"name": ...}`
        // — the hermes-json shape, not Qwen3.8/3.6's `<function=...>` XML —
        // before asserting on it. The template source spells its own
        // `\n` literally (backslash-n, rendered as a newline by Jinja at
        // generation time, not by this raw-text read).
        assert!(QWEN3VL_TPL.contains(r#"<tool_call>\n{"name": ""#));
        assert!(!QWEN3VL_TPL.contains("<function="));

        let s = TemplateSignals::from_template(QWEN3VL_TPL);
        assert!(s.tools_var);
        assert!(!s.thinking_markers);
        assert_eq!(s.tool_call_format.as_deref(), Some("hermes-json"));
    }

    #[test]
    fn medgemma_signals() {
        let s = TemplateSignals::from_template(MEDGEMMA_TPL);
        assert_eq!(s, TemplateSignals::default());
    }

    #[test]
    fn mistral3_signals() {
        let s = TemplateSignals::from_template(MISTRAL3_TPL);
        assert!(!s.tools_var);
        assert!(!s.thinking_markers);
        assert_eq!(s.tool_call_format, None);
    }

    #[test]
    fn muse_glimmer_signals() {
        let s = TemplateSignals::from_template(MUSE_GLIMMER_TPL);
        // No `<think>`-style tag — the trace reaches the output only through
        // `message['reasoning_content']` inside a `{{ ... }}` expression, same
        // shape as Cohere North.
        assert!(s.thinking_markers);
        assert_eq!(s.thinking_marker.as_deref(), Some("reasoning_content"));
        assert!(!s.enable_thinking_var);
        assert_eq!(s.enable_thinking_default, None);
        // `render_reasoning()`'s only reference is `reasoning_strength`, read
        // via the Jinja ternary `reasoning_strength if reasoning_strength is
        // defined and reasoning_strength else 'high'` — no `==`/`!=`/`in`
        // anywhere, so there is no evidence of other levels.
        assert!(s.reasoning_effort_var);
        // …and it is *only* `reasoning_strength`, a name llama-server never
        // sets from a request's `reasoning_effort`. A caller must be able to
        // tell this apart from a template it can actually steer, which is what
        // `effort_var_names` is for (§2.1 `control`).
        assert_eq!(s.effort_var_names, ["reasoning_strength"]);
        assert!(s.effort_levels.is_empty());
        assert_eq!(s.effort_default.as_deref(), Some("high"));
        assert!(!s.preserve_thinking_var);
        assert!(s.tools_var);
        assert!(s.parallel_tool_calls);
        // Its tool-call syntax is a bespoke `<atem:function_calls>` /
        // `<atem:invoke name="...">` dialect — none of the known markers.
        assert_eq!(s.tool_call_format, None);
    }

    // ---- TemplateSignals: synthetic snippets for markers not on disk ------

    #[test]
    fn tool_call_format_synthetic_markers() {
        assert_eq!(
            TemplateSignals::from_template("...[TOOL_CALLS][{\"name\": \"x\"}]...")
                .tool_call_format
                .as_deref(),
            Some("mistral")
        );
        assert_eq!(
            TemplateSignals::from_template("...<|python_tag|>x.y(z=1)...")
                .tool_call_format
                .as_deref(),
            Some("llama3")
        );
        assert_eq!(
            TemplateSignals::from_template("...<|channel|>commentary to=functions.x...")
                .tool_call_format
                .as_deref(),
            Some("gpt-oss")
        );
        assert_eq!(
            TemplateSignals::from_template("...<|tool_call|>{\"name\": \"x\"}...")
                .tool_call_format
                .as_deref(),
            Some("granite")
        );
    }

    #[test]
    fn effort_levels_read_double_quoted_literals_too() {
        let s =
            TemplateSignals::from_template("{%- if reasoning_effort == \"high\" %}yes{%- endif %}");
        assert_eq!(s.effort_levels, ["high"]);
    }

    // ---- has_vision_encoder / has_audio_encoder ----------------------------

    #[test]
    fn encoder_flags_read_the_declared_key_or_fall_back_to_the_block() {
        // Declared explicitly, and explicitly false — must not be promoted
        // to `Some(true)` just because *some* clip.* key exists elsewhere.
        let mut declared_false = Build::default();
        declared_false
            .bool("clip.has_vision_encoder", false)
            .u32("clip.vision.block_count", 10);
        let f = declared_false.file();
        assert_eq!(summarize(f.path()).unwrap().has_vision_encoder, Some(false));

        // No `clip.has_vision_encoder` key, but a `clip.vision.*` block
        // exists — older mmproj conversions never wrote the flag.
        let mut fallback = Build::default();
        fallback.u32("clip.vision.block_count", 27);
        let f = fallback.file();
        assert_eq!(summarize(f.path()).unwrap().has_vision_encoder, Some(true));

        // Audio, same two rules.
        let mut audio_declared = Build::default();
        audio_declared.bool("clip.has_audio_encoder", true);
        let f = audio_declared.file();
        assert_eq!(summarize(f.path()).unwrap().has_audio_encoder, Some(true));

        let mut audio_fallback = Build::default();
        audio_fallback.u32("clip.audio.block_count", 6);
        let f = audio_fallback.file();
        assert_eq!(summarize(f.path()).unwrap().has_audio_encoder, Some(true));

        // Nothing at all — neither key nor block — stays `None`, not `Some(false)`.
        let f = Build::default().file();
        let s = summarize(f.path()).unwrap();
        assert_eq!(s.has_vision_encoder, None);
        assert_eq!(s.has_audio_encoder, None);
    }

    // ---- GgufSummaryCache ---------------------------------------------------

    #[tokio::test]
    async fn cache_hits_until_the_file_changes_underneath_it() {
        let mut b = Build::default();
        b.str("general.architecture", "foo")
            .u32("foo.block_count", 12);
        let f = b.file();
        let cache = GgufSummaryCache::default();

        let first = cache.summarize_cached(f.path()).await.unwrap();
        assert_eq!(first.block_count, Some(12));
        // A second call against an unchanged file must be served from cache:
        // same `Arc` pointer, not merely an equal value.
        let second = cache.summarize_cached(f.path()).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "expected a cache hit");

        // Rewriting the file changes both its length and mtime, which must
        // invalidate the entry — a model re-downloaded in place must not
        // keep serving the old header.
        let mut b2 = Build::default();
        b2.str("general.architecture", "foo")
            .u32("foo.block_count", 99);
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(f.path(), b2.bytes()).unwrap();

        let third = cache.summarize_cached(f.path()).await.unwrap();
        assert_eq!(third.block_count, Some(99));
        assert!(!Arc::ptr_eq(&first, &third), "expected a cache miss");
    }

    /// A cold path asked for by several tasks at once is read **once**: the
    /// per-path guard makes the losers wait for the winner's summary instead
    /// of each re-reading the same header (which is exactly what a cold
    /// `/v1/models` does — one projector shared by several rows).
    #[tokio::test]
    async fn concurrent_misses_on_one_path_read_it_once() {
        let mut b = Build::default();
        b.str("general.architecture", "foo")
            .u32("foo.block_count", 12);
        let f = b.file();
        let cache = Arc::new(GgufSummaryCache::default());
        assert_eq!(cache.reads(), 0);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let path = f.path().to_path_buf();
            tasks.push(tokio::spawn(async move {
                cache.summarize_cached(&path).await.unwrap()
            }));
        }
        let summaries: Vec<_> = futures::future::join_all(tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(cache.reads(), 1, "the header must be read exactly once");
        for s in &summaries {
            assert!(
                Arc::ptr_eq(s, &summaries[0]),
                "every caller must get the one summary that was read"
            );
        }

        // …and a later call still costs nothing.
        cache.summarize_cached(f.path()).await.unwrap();
        assert_eq!(cache.reads(), 1);
    }

    #[tokio::test]
    async fn cache_never_remembers_an_error() {
        let cache = GgufSummaryCache::default();
        let missing = PathBuf::from("/nonexistent/definitely-not-a-model.gguf");
        assert!(cache.summarize_cached(&missing).await.is_err());
        // No panic, no stuck error — a file that starts existing afterwards
        // (e.g. a download landing mid-run) must be read on the next call.
        assert!(cache.summarize_cached(&missing).await.is_err());
    }

    // ---- tokenizer signature (ladder design §4.3 rule 5) ------------------

    /// A minimal but complete tokenizer block: model/pre, a small vocabulary,
    /// merges, token types, BOS/EOS, and a chat template — everything
    /// [`TokenizerSignature`] reads.
    fn tokenizer_header(pre: &str, vocab: &[&str], merges: &[&str]) -> Build {
        let mut b = Build::default();
        b.str("general.architecture", "qwen3")
            .u32("qwen3.context_length", 4096)
            .u32("qwen3.block_count", 4)
            .str("tokenizer.ggml.model", "gpt2")
            .str("tokenizer.ggml.pre", pre)
            .arr_str("tokenizer.ggml.tokens", vocab)
            .arr_str("tokenizer.ggml.merges", merges)
            .arr_i32("tokenizer.ggml.token_type", &vec![1i32; vocab.len()])
            .u32("tokenizer.ggml.bos_token_id", 1)
            .u32("tokenizer.ggml.eos_token_id", 2)
            .bool("tokenizer.ggml.add_bos_token", true)
            .bool("tokenizer.ggml.add_eos_token", false)
            .str("tokenizer.chat_template", "{{ messages }}");
        b
    }

    /// The whole point of hashing instead of previewing (module doc,
    /// `Reader::read_and_hash_array`): a 1000-entry vocabulary — ten times
    /// past `ARRAY_PREVIEW` — still compares exactly equal to a byte-for-byte
    /// copy, and any single different token is caught.
    #[test]
    fn identical_files_have_identical_signatures() {
        let vocab: Vec<String> = (0..1000).map(|i| format!("tok{i}")).collect();
        let refs: Vec<&str> = vocab.iter().map(String::as_str).collect();
        let f1 = tokenizer_header("qwen2", &refs, &["a b", "c d"]).file();
        let f2 = tokenizer_header("qwen2", &refs, &["a b", "c d"]).file();
        let s1 = read_tokenizer_signature(f1.path()).unwrap();
        let s2 = read_tokenizer_signature(f2.path()).unwrap();
        assert_eq!(s1.tokens.unwrap().0, 1000, "true length, not the preview");
        assert!(s1.tokenizes_identically_to(&s2));
        assert_eq!(s1, s2);
    }

    #[test]
    fn one_different_token_past_the_preview_is_caught() {
        let mut vocab: Vec<String> = (0..1000).map(|i| format!("tok{i}")).collect();
        let refs1: Vec<&str> = vocab.iter().map(String::as_str).collect();
        let f1 = tokenizer_header("qwen2", &refs1, &["a b"]).file();
        vocab[999] = "different".into(); // well past ARRAY_PREVIEW
        let refs2: Vec<&str> = vocab.iter().map(String::as_str).collect();
        let f2 = tokenizer_header("qwen2", &refs2, &["a b"]).file();
        let s1 = read_tokenizer_signature(f1.path()).unwrap();
        let s2 = read_tokenizer_signature(f2.path()).unwrap();
        assert_ne!(s1.tokens, s2.tokens);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn different_merges_are_caught_independently_of_tokens() {
        let vocab = ["a", "b", "c"];
        let f1 = tokenizer_header("qwen2", &vocab, &["a b"]).file();
        let f2 = tokenizer_header("qwen2", &vocab, &["a b", "b c"]).file();
        let s1 = read_tokenizer_signature(f1.path()).unwrap();
        let s2 = read_tokenizer_signature(f2.path()).unwrap();
        assert_eq!(s1.tokens, s2.tokens);
        assert_ne!(s1.merges, s2.merges);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn a_different_pretokenizer_is_caught() {
        let vocab = ["a", "b"];
        let f1 = tokenizer_header("qwen2", &vocab, &[]).file();
        let f2 = tokenizer_header("llama3", &vocab, &[]).file();
        let s1 = read_tokenizer_signature(f1.path()).unwrap();
        let s2 = read_tokenizer_signature(f2.path()).unwrap();
        assert_ne!(s1.pre, s2.pre);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn different_chat_templates_do_not_affect_tokenizes_identically_to() {
        // §4.3 rule 5's template check is conditional on the row not
        // overriding it, which only the caller (`ops::validate_ladder`)
        // knows — so `TokenizerSignature::tokenizes_identically_to` itself
        // must ignore it, and callers compare `chat_templates` separately.
        let vocab = ["a", "b"];
        let f1 = tokenizer_header("qwen2", &vocab, &[]).file();
        let mut b2 = tokenizer_header("qwen2", &vocab, &[]);
        b2.str("tokenizer.chat_template", "a different template entirely");
        let f2 = b2.file();
        let s1 = read_tokenizer_signature(f1.path()).unwrap();
        let s2 = read_tokenizer_signature(f2.path()).unwrap();
        assert_ne!(s1.chat_templates, s2.chat_templates);
        assert!(s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn different_scores_are_caught_even_with_no_merges_list() {
        // SPM/UGM tokenizers (review finding 11) merge by score and carry no
        // `merges` array at all — a `merges`-only comparison would miss this
        // entirely. `scores` is written here as a `u32` array (bit patterns);
        // `read_and_hash_array` hashes any scalar array element-wise
        // regardless of its declared type, so this exercises the same path
        // real `f32` scores take.
        let vocab = ["a", "b", "c"];
        let mut b1 = Build::default();
        b1.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "llama")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .arr_u32("tokenizer.ggml.scores", &[1, 2, 3])
            .u32("tokenizer.ggml.bos_token_id", 1);
        let mut b2 = Build::default();
        b2.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "llama")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .arr_u32("tokenizer.ggml.scores", &[1, 2, 4])
            .u32("tokenizer.ggml.bos_token_id", 1);
        let s1 = read_tokenizer_signature(b1.file().path()).unwrap();
        let s2 = read_tokenizer_signature(b2.file().path()).unwrap();
        assert_eq!(s1.tokens, s2.tokens);
        assert_ne!(s1.scores, s2.scores);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn different_normalization_flags_are_caught() {
        let vocab = ["a", "b"];
        let mut b1 = Build::default();
        b1.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "llama")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .bool("tokenizer.ggml.add_space_prefix", true);
        let mut b2 = Build::default();
        b2.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "llama")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .bool("tokenizer.ggml.add_space_prefix", false);
        let s1 = read_tokenizer_signature(b1.file().path()).unwrap();
        let s2 = read_tokenizer_signature(b2.file().path()).unwrap();
        assert_ne!(s1.add_space_prefix, s2.add_space_prefix);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    /// Second-pass review finding S4: `precompiled_charsmap` is a GGUF array
    /// of `UINT8` upstream, never a string — reading it with `meta.string`
    /// (finding 11's first pass) was always `None`, so two different
    /// charsmaps compared equal. It is hashed now, like the other big
    /// tokenizer arrays.
    #[test]
    fn different_precompiled_charsmaps_are_caught() {
        let vocab = ["a", "b"];
        let mut b1 = Build::default();
        b1.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "t5")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .arr_u8("tokenizer.ggml.precompiled_charsmap", &[1, 2, 3, 4]);
        let mut b2 = Build::default();
        b2.str("general.architecture", "qwen3")
            .str("tokenizer.ggml.model", "t5")
            .arr_str("tokenizer.ggml.tokens", &vocab)
            .arr_u8("tokenizer.ggml.precompiled_charsmap", &[1, 2, 3, 5]); // one byte differs
        let s1 = read_tokenizer_signature(b1.file().path()).unwrap();
        let s2 = read_tokenizer_signature(b2.file().path()).unwrap();
        assert!(s1.precompiled_charsmap.is_some(), "must not read as None");
        assert_ne!(s1.precompiled_charsmap, s2.precompiled_charsmap);
        assert!(!s1.tokenizes_identically_to(&s2));
    }

    #[test]
    fn a_named_chat_template_is_captured_next_to_the_bare_one() {
        let mut b = Build::default();
        b.str("general.architecture", "qwen3")
            .str("tokenizer.chat_template", "bare")
            .str("tokenizer.chat_template.tool_use", "tools");
        let s = read_tokenizer_signature(b.file().path()).unwrap();
        assert_eq!(
            s.chat_templates
                .get("tokenizer.chat_template")
                .map(String::as_str),
            Some("bare")
        );
        assert_eq!(
            s.chat_templates
                .get("tokenizer.chat_template.tool_use")
                .map(String::as_str),
            Some("tools")
        );
    }
}

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
