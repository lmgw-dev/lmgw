//! The header read from a stream (find-a-model design §2.1, §10 "The header
//! from a stream"): [`read_header_from`] over any forward-only reader with
//! the file's total length, as a Hugging Face file's download body is read.

use std::io::{Cursor, Read};

use super::*;

/// The keys of a small chat model, with a tokenizer.
fn keys() -> synth::Header {
    let mut h = synth::chat("demoarch", 4096);
    let tokens: Vec<String> = (0..1000).map(|i| format!("tok{i}")).collect();
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    h.str("general.name", "demo-model")
        .arr_str("tokenizer.ggml.tokens", &tokens);
    h
}

/// [`keys`] with real shapes behind them: tied F16 embeddings, two
/// repeating layers at Q4_K's 4.5 bits per weight, and an F32 norm.
fn model() -> synth::Header {
    let mut h = keys();
    h.tensor_shaped("token_embd.weight", &[64, 1000], 64 * 1000 * 2)
        .tensor_shaped("blk.0.attn_q.weight", &[64, 64], 64 * 64 * 9 / 16)
        .tensor_shaped("blk.1.attn_q.weight", &[64, 64], 64 * 64 * 9 / 16)
        .tensor_shaped("output_norm.weight", &[64], 64 * 4);
    h
}

/// Where the descriptors begin: the counts are fixed-width, so [`keys`]
/// alone (no tensors, no data, no padding) ends where [`model`]'s keys do.
fn keys_end() -> usize {
    keys().bytes().len()
}

/// The same bytes give the same header from a file and from a stream: the
/// keys, the names, each tensor's bytes and elements, and the data start.
#[test]
fn a_stream_and_a_file_of_the_same_bytes_give_the_same_header() {
    let bytes = model().bytes();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &bytes).unwrap();
    let disk = read_header(tmp.path()).unwrap();
    let stream = read_header_from(Cursor::new(bytes.clone()), bytes.len() as u64).unwrap();
    assert_eq!(
        serde_json::to_value(&disk).unwrap(),
        serde_json::to_value(&stream).unwrap()
    );
    assert_eq!(disk.tensor_bytes, stream.tensor_bytes);
    assert_eq!(disk.tensor_elements, stream.tensor_elements);
    assert_eq!(disk.data_start, stream.data_start);
    assert_eq!(disk.file_size, bytes.len() as u64);
}

/// A stream cut in the keys or in the descriptors is `UnexpectedEof`, as
/// an interrupted download on disk is; cut exactly at the data start, the
/// header is whole. The total is the file's, so no length check stops it
/// first.
#[test]
fn a_cut_stream_is_eof_until_the_data_start() {
    let bytes = model().bytes();
    let total = bytes.len() as u64;
    let whole = read_header_from(Cursor::new(bytes.clone()), total).unwrap();
    let keys = keys_end();
    let cut = |n: usize| read_header_from(Cursor::new(bytes[..n].to_vec()), total);
    assert!(
        matches!(cut(keys - 5), Err(GgufError::UnexpectedEof(_))),
        "in the keys"
    );
    assert!(
        matches!(cut(keys + 5), Err(GgufError::UnexpectedEof(_))),
        "in the descriptors"
    );
    let at_start = cut(whole.data_start as usize).unwrap();
    assert_eq!(at_start.tensor_names, whole.tensor_names);
    assert_eq!(
        at_start.tensor_bytes, whole.tensor_bytes,
        "sized from the total"
    );
}

/// A declared length past the total is refused before anything is
/// allocated for it, as on disk: the total is the ceiling, not the bytes
/// that happen to have arrived.
#[test]
fn a_declared_length_past_the_total_is_refused() {
    let bytes = model().bytes();
    // The tokenizer array declares 1000 strings; a total of 200 bytes cannot
    // hold them.
    let err = read_header_from(Cursor::new(bytes.clone()), 200).unwrap_err();
    assert!(matches!(err, GgufError::TooLarge { .. }), "{err:?}");
}

/// A reader that counts what was taken from it.
struct Counting<R> {
    inner: R,
    taken: u64,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.taken += n as u64;
        Ok(n)
    }
}

/// The reader stops at the header: with a megabyte of tensor data behind
/// it, not one byte past the data start is taken, so a stream can be
/// dropped there.
#[test]
fn no_byte_of_tensor_data_is_read() {
    let mut h = model();
    h.tensor_shaped("blk.1.ffn_down.weight", &[1024, 1024], 1 << 20);
    let bytes = h.bytes();
    let mut counting = Counting {
        inner: Cursor::new(bytes.clone()),
        taken: 0,
    };
    let meta = read_header_from(&mut counting, bytes.len() as u64).unwrap();
    assert!(
        counting.taken <= meta.data_start,
        "{} > {}",
        counting.taken,
        meta.data_start
    );
    assert!(meta.data_start < bytes.len() as u64 - (1 << 20));
}

/// The parameter count is every descriptor's elements; the repeating
/// layers' bits per weight are the `blk.*` tensors' alone, so wide token
/// embeddings don't widen the layers (D7).
#[test]
fn the_parameter_count_and_the_layers_bits_come_from_the_descriptors() {
    let bytes = model().bytes();
    let s = ModelSummary::from_meta(
        &read_header_from(Cursor::new(bytes.clone()), bytes.len() as u64).unwrap(),
    );
    assert_eq!(s.parameter_count, Some(64 * 1000 + 2 * 64 * 64 + 64));
    let bits = s.layer_bits_per_weight.unwrap();
    assert!((bits - 4.5).abs() < 1e-9, "{bits}");

    // A projector names its layers `v.blk.*`: a count, and no layer width.
    let mut proj = synth::Header::default();
    proj.str("general.type", "mmproj").tensor_shaped(
        "v.blk.0.attn_q.weight",
        &[32, 32],
        32 * 32 * 2,
    );
    let pb = proj.bytes();
    let ps = ModelSummary::from_meta(
        &read_header_from(Cursor::new(pb.clone()), pb.len() as u64).unwrap(),
    );
    assert_eq!(ps.parameter_count, Some(1024));
    assert_eq!(ps.layer_bits_per_weight, None);

    // No tensors, no count; an overflowing shape, no count either.
    let empty = synth::chat("demoarch", 8).bytes();
    let es = ModelSummary::from_meta(
        &read_header_from(Cursor::new(empty.clone()), empty.len() as u64).unwrap(),
    );
    assert_eq!(es.parameter_count, None);
    let mut huge = synth::Header::default();
    huge.tensor_shaped("blk.0.x", &[u64::MAX, 2], 1);
    let hb = huge.bytes();
    let hs = ModelSummary::from_meta(
        &read_header_from(Cursor::new(hb.clone()), hb.len() as u64).unwrap(),
    );
    assert_eq!(hs.parameter_count, None);
    assert_eq!(hs.layer_bits_per_weight, None);
}

/// A GGUF v3 header with no tensors and one key, `key`, whose type tag and
/// value bytes are `value`: the start of a hostile stream.
fn one_key(key: &str, value: &[u8]) -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // tensors
    b.extend_from_slice(&1u64.to_le_bytes()); // keys
    b.extend_from_slice(&(key.len() as u64).to_le_bytes());
    b.extend_from_slice(key.as_bytes());
    b.extend_from_slice(value);
    b
}

/// A string tag and a declared length of `len`.
fn string_of(len: u64) -> Vec<u8> {
    let mut v = VT_STRING.to_le_bytes().to_vec();
    v.extend_from_slice(&len.to_le_bytes());
    v
}

/// An array tag of `len` elements of type `elem`.
fn array_of(elem: u32, len: u64) -> Vec<u8> {
    let mut v = VT_ARRAY.to_le_bytes().to_vec();
    v.extend_from_slice(&elem.to_le_bytes());
    v.extend_from_slice(&len.to_le_bytes());
    v
}

/// A total a hostile hub could name: a 20 GB file.
const HUGE_TOTAL: u64 = 20_000_000_000;

/// A string that declares 19 GB under a 20 GB total and sends 200 KB: the
/// read ends at the stream's end with an error, holding only what came
/// (find-a-model design D129). Before, the reader asked for all 19 GB before
/// reading a byte of it, which aborts the process on a small box.
#[test]
fn a_huge_declared_string_holds_only_what_arrived() {
    let mut bytes = one_key("general.name", &string_of(19_000_000_000));
    bytes.extend(std::iter::repeat_n(b'a', 200_000));
    let err = read_header_from(Cursor::new(bytes), HUGE_TOTAL).unwrap_err();
    assert!(matches!(err, GgufError::UnexpectedEof(_)), "{err:?}");
}

/// Under a bound, a declared string or array that would carry the header
/// past it stops the read before a byte of it is taken, whatever the total
/// allows: a 1 GB string, a 5 GB array of bytes, and an array of 300
/// strings of 100 bytes, whose declaration fits and which stops where the
/// strings so far reach the bound (D129).
#[test]
fn a_bound_stops_a_hostile_header_before_its_bytes_are_taken() {
    let bound = 4096;
    let over = |bytes: Vec<u8>| {
        let mut counting = Counting {
            inner: Cursor::new(bytes),
            taken: 0,
        };
        let err = read_header_within(&mut counting, HUGE_TOTAL, bound).unwrap_err();
        (err, counting.taken)
    };

    let mut string = one_key("general.name", &string_of(1_000_000_000));
    string.extend(std::iter::repeat_n(b'a', 100_000));
    let (err, taken) = over(string.clone());
    let GgufError::OverBound {
        at, declared_end, ..
    } = err
    else {
        panic!("{err:?}");
    };
    assert_eq!(at, taken, "nothing of the string was taken");
    assert_eq!(declared_end, at + 1_000_000_000);

    let mut bytes = one_key("tokenizer.ggml.token_type", &array_of(VT_U8, 5_000_000_000));
    bytes.extend(std::iter::repeat_n(0u8, 100_000));
    let (err, taken) = over(bytes);
    assert!(matches!(err, GgufError::OverBound { .. }), "{err:?}");
    assert!(taken < 100, "{taken}");

    let mut strings = one_key("tokenizer.ggml.tokens", &array_of(VT_STRING, 300));
    for _ in 0..300 {
        strings.extend_from_slice(&100u64.to_le_bytes());
        strings.extend(std::iter::repeat_n(b't', 100));
    }
    let (err, taken) = over(strings);
    assert!(matches!(err, GgufError::OverBound { .. }), "{err:?}");
    assert!(taken <= bound, "{taken} > {bound}");

    // The same string under a bound past it reads as before.
    let fine = model().bytes();
    let total = fine.len() as u64;
    assert!(read_header_within(Cursor::new(fine), total, total).is_ok());
}
