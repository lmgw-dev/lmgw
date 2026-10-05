//! audio.cpp's packed GGUFs: the spec and the small files they carry.
//!
//! An audio.cpp package GGUF (`general.architecture = "audiocpp"`) carries
//! its family's `model_spec.json` as a string KV and every non-tensor file
//! of the original model directory (configs, vocabularies, voice styles,
//! speaker tables) as one `uint8` array, `audiocpp.embedded_files.data`,
//! addressed by a names array and an offsets array one entry longer. That
//! array sits in the *header* — 57 MB of it for Supertonic — so the generic
//! [`super::read_header`] walk would read all of it to skip it, and keeps
//! only [`super::ARRAY_PREVIEW`] names besides.
//!
//! This reader walks the same header with the same bounds (every length is
//! checked against the real file size), keeps the spec, the full names and
//! offsets, and the file position where the data array's payload starts —
//! then **seeks** past the payload instead of reading it. A named file is
//! read later by one seek and one bounded read ([`EmbeddedIndex::file`]):
//! the data blob itself is never read whole.
//!
//! What lmgw reads this for is the speech profile (`crate::audio::profile`):
//! native voices and languages live in these files, not in the catalog spec.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{GgufError, Reader, Result, VT_ARRAY, VT_STRING};

/// The most lmgw reads out of a GGUF for one embedded file: 1 MiB. The
/// files it wants (a `config.json`, a `speakers.json`, a voice-style JSON)
/// are a few kilobytes to tens of kilobytes; dictionaries and audio samples
/// in the same table are megabytes and never wanted. A file over it is
/// refused with [`GgufError::EmbeddedFileTooLarge`], which names the file and
/// the bound — never cut short.
pub const MAX_EMBEDDED_FILE: u64 = 1024 * 1024;

const KEY_SPEC_JSON: &str = "audiocpp.model_spec.json";
const KEY_FAMILY: &str = "audiocpp.model_spec.family";
const KEY_NAMES: &str = "audiocpp.embedded_files.names";
const KEY_OFFSETS: &str = "audiocpp.embedded_files.offsets";
const KEY_DATA: &str = "audiocpp.embedded_files.data";

/// What an audio.cpp GGUF says about itself without its tensors or its
/// embedded data: the spec, the family, and where each embedded file is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmbeddedIndex {
    path: PathBuf,
    /// `audiocpp.model_spec.json`, as written.
    pub model_spec_json: Option<String>,
    /// `audiocpp.model_spec.family`.
    pub family: Option<String>,
    /// Embedded file names, in table order.
    pub names: Vec<String>,
    /// `names.len() + 1` ascending offsets into the data array (validated).
    offsets: Vec<u64>,
    /// File position of the data array's first byte; `None` when the GGUF
    /// embeds no files.
    data_start: Option<u64>,
}

impl EmbeddedIndex {
    /// The size of embedded file `name`, when the table has it.
    pub fn len_of(&self, name: &str) -> Option<u64> {
        let i = self.names.iter().position(|n| n == name)?;
        Some(self.offsets[i + 1] - self.offsets[i])
    }

    /// Read embedded file `name`: `Ok(None)` when the table has no such
    /// file, [`GgufError::EmbeddedFileTooLarge`] when it is over
    /// [`MAX_EMBEDDED_FILE`]. One seek and one read of exactly its bytes.
    pub fn file(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(i) = self.names.iter().position(|n| n == name) else {
            return Ok(None);
        };
        let Some(start) = self.data_start else {
            return Ok(None);
        };
        let (from, to) = (self.offsets[i], self.offsets[i + 1]);
        let len = to - from;
        if len > MAX_EMBEDDED_FILE {
            return Err(GgufError::EmbeddedFileTooLarge {
                name: name.to_string(),
                len,
                cap: MAX_EMBEDDED_FILE,
            });
        }
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(start + from))?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => GgufError::UnexpectedEof("an embedded file"),
            _ => GgufError::Io(e),
        })?;
        Ok(Some(buf))
    }
}

/// Read the embedded-file index of the GGUF at `path`. A GGUF that is not an
/// audio.cpp package reads as an empty index (no spec, no files); a corrupt
/// one is an error.
pub fn read_embedded_index(path: &Path) -> Result<EmbeddedIndex> {
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
    let _tensor_count = r.read_u64("the tensor count")?;
    let kv_count = r.read_u64("the metadata count")?;
    r.check_fits("the metadata count", kv_count, 13)?;

    let mut out = EmbeddedIndex {
        path: path.to_path_buf(),
        ..EmbeddedIndex::default()
    };
    let mut offsets: Option<Vec<u64>> = None;
    let mut data_len: Option<u64> = None;
    for _ in 0..kv_count {
        let key = r.read_string("a metadata key")?;
        let ty = r.read_u32("a metadata value type")?;
        match key.as_str() {
            KEY_SPEC_JSON if ty == VT_STRING => {
                out.model_spec_json = Some(r.read_string("the model spec")?);
            }
            KEY_FAMILY if ty == VT_STRING => {
                out.family = Some(r.read_string("the model spec family")?);
            }
            KEY_NAMES if ty == VT_ARRAY => out.names = read_strings(&mut r)?,
            KEY_OFFSETS if ty == VT_ARRAY => offsets = Some(read_unsigned(&mut r)?),
            KEY_DATA if ty == VT_ARRAY => {
                let (start, len) = seek_past_bytes(&mut r)?;
                out.data_start = Some(start);
                data_len = Some(len);
            }
            _ => r.skip_value(ty)?,
        }
        // Everything wanted is in hand: the tensor name/shape tables that
        // follow are not read.
        if out.model_spec_json.is_some() && offsets.is_some() && data_len.is_some() {
            break;
        }
    }

    match (offsets, data_len) {
        (Some(offsets), Some(data_len)) => {
            if offsets.len() != out.names.len() + 1 {
                return Err(GgufError::Malformed(format!(
                    "{} names but {} offsets (expected one more than names)",
                    out.names.len(),
                    offsets.len()
                )));
            }
            if offsets.windows(2).any(|w| w[0] > w[1]) {
                return Err(GgufError::Malformed("offsets do not ascend".into()));
            }
            if offsets.last().is_some_and(|&end| end > data_len) {
                return Err(GgufError::Malformed(format!(
                    "the last file ends at {} but the data array holds {data_len} bytes",
                    offsets.last().copied().unwrap_or_default()
                )));
            }
            out.offsets = offsets;
        }
        // No table, or half of one: the files cannot be addressed.
        _ => {
            out.names.clear();
            out.data_start = None;
        }
    }
    Ok(out)
}

/// An array of strings, read whole. Bounded like every other read: the
/// declared count must fit in the file at 8 bytes per string.
fn read_strings<R: Read>(r: &mut Reader<R>) -> Result<Vec<String>> {
    let elem_type = r.read_u32("an array element type")?;
    let len = r.read_u64("an array length")?;
    if elem_type != VT_STRING {
        // Not the shape audio.cpp writes: skip it as the value it is.
        r.check_fits("an array", len, super::min_encoded_size(elem_type)?)?;
        for _ in 0..len {
            r.skip_value(elem_type)?;
        }
        return Ok(Vec::new());
    }
    r.check_fits("an array", len, 8)?;
    let mut out = Vec::with_capacity(len.min(4096) as usize);
    for _ in 0..len {
        out.push(r.read_string("an embedded file name")?);
    }
    Ok(out)
}

/// An array of unsigned integers of any width, widened to `u64` and read
/// whole.
fn read_unsigned<R: Read>(r: &mut Reader<R>) -> Result<Vec<u64>> {
    let elem_type = r.read_u32("an array element type")?;
    let len = r.read_u64("an array length")?;
    r.check_fits("an array", len, super::min_encoded_size(elem_type)?)?;
    let mut out = Vec::with_capacity(len.min(4096) as usize);
    for _ in 0..len {
        let v = r.read_value(elem_type, super::MAX_PREVIEW_DEPTH)?;
        out.push(v.as_u64().ok_or_else(|| {
            GgufError::Malformed("an offset is not a non-negative integer".into())
        })?);
    }
    Ok(out)
}

/// The data array: note where its payload starts and seek past it. `(start,
/// byte length)`.
fn seek_past_bytes(r: &mut Reader<BufReader<File>>) -> Result<(u64, u64)> {
    let elem_type = r.read_u32("an array element type")?;
    let len = r.read_u64("an array length")?;
    let width = super::scalar_size(elem_type)
        .ok_or_else(|| GgufError::Malformed("the embedded data is not an array of bytes".into()))?;
    let bytes = len.checked_mul(width).ok_or(GgufError::TooLarge {
        what: "the embedded data",
        len,
        remaining: r.remaining(),
    })?;
    r.check_fits("the embedded data", bytes, 1)?;
    let start = r.pos;
    let step = i64::try_from(bytes).map_err(|_| GgufError::TooLarge {
        what: "the embedded data",
        len: bytes,
        remaining: r.remaining(),
    })?;
    r.inner.seek_relative(step)?;
    r.pos += bytes;
    Ok((start, bytes))
}

#[cfg(test)]
mod tests {
    use super::super::synth;
    use super::*;

    fn write(h: &synth::Header) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), h.bytes()).unwrap();
        f
    }

    #[test]
    fn files_are_read_by_seek_and_the_spec_comes_along() {
        let big = vec![7u8; 4096];
        let h = synth::audiocpp(
            "qwen3_tts",
            r#"{"family":"qwen3_tts"}"#,
            &[
                ("README.md", b"hello"),
                ("samples/big.wav", &big),
                ("config.json", br#"{"a":1}"#),
            ],
        );
        let f = write(&h);
        let idx = read_embedded_index(f.path()).unwrap();
        assert_eq!(idx.family.as_deref(), Some("qwen3_tts"));
        assert_eq!(
            idx.model_spec_json.as_deref(),
            Some(r#"{"family":"qwen3_tts"}"#)
        );
        assert_eq!(idx.names, ["README.md", "samples/big.wav", "config.json"]);
        assert_eq!(idx.file("config.json").unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(idx.file("README.md").unwrap().unwrap(), b"hello");
        assert_eq!(idx.len_of("samples/big.wav"), Some(4096));
        assert!(idx.file("absent.json").unwrap().is_none());
    }

    #[test]
    fn a_file_over_the_bound_is_refused_by_name_not_cut_short() {
        let big = vec![1u8; MAX_EMBEDDED_FILE as usize + 1];
        let f = write(&synth::audiocpp("x", "{}", &[("huge.dict", &big)]));
        let idx = read_embedded_index(f.path()).unwrap();
        let e = idx.file("huge.dict").unwrap_err().to_string();
        assert!(e.contains("huge.dict") && e.contains("1048576"), "{e}");
    }

    #[test]
    fn a_plain_gguf_has_an_empty_index_and_a_broken_table_is_an_error() {
        let f = write(&synth::chat("llama", 4096));
        let idx = read_embedded_index(f.path()).unwrap();
        assert!(idx.names.is_empty() && idx.model_spec_json.is_none());
        assert!(idx.file("config.json").unwrap().is_none());

        let mut h = synth::Header::default();
        h.str("general.architecture", "audiocpp")
            .arr_str("audiocpp.embedded_files.names", &["a", "b"])
            .arr_u64("audiocpp.embedded_files.offsets", &[0, 5])
            .arr_u8("audiocpp.embedded_files.data", b"hello");
        let f = write(&h);
        assert!(matches!(
            read_embedded_index(f.path()),
            Err(GgufError::Malformed(_))
        ));

        let mut h = synth::Header::default();
        h.arr_str("audiocpp.embedded_files.names", &["a"])
            .arr_u64("audiocpp.embedded_files.offsets", &[0, 50])
            .arr_u8("audiocpp.embedded_files.data", b"hello");
        let f = write(&h);
        assert!(matches!(
            read_embedded_index(f.path()),
            Err(GgufError::Malformed(_))
        ));
    }

    #[test]
    fn truncation_is_an_error_never_a_panic() {
        let h = synth::audiocpp("x", "{}", &[("config.json", b"{}")]);
        let bytes = h.bytes();
        for cut in 0..bytes.len() {
            let f = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(f.path(), &bytes[..cut]).unwrap();
            let _ = read_embedded_index(f.path());
        }
    }
}
