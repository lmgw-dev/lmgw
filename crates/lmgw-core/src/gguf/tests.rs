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

const QWEN38_TPL: &str = include_str!("../../tests/fixtures/chat_templates/qwen3.8.jinja");
const QWEN36_TPL: &str = include_str!("../../tests/fixtures/chat_templates/qwen3.6.jinja");
const GEMMA4_TPL: &str = include_str!("../../tests/fixtures/chat_templates/gemma4.jinja");
const DEEPSEEK_V4_FLASH_TPL: &str =
    include_str!("../../tests/fixtures/chat_templates/deepseek-v4-flash.jinja");
const COHERE_NORTH_TPL: &str =
    include_str!("../../tests/fixtures/chat_templates/cohere-north.jinja");
const LFM2_TPL: &str = include_str!("../../tests/fixtures/chat_templates/lfm2.jinja");
const QWEN3VL_TPL: &str = include_str!("../../tests/fixtures/chat_templates/qwen3vl.jinja");
const MEDGEMMA_TPL: &str = include_str!("../../tests/fixtures/chat_templates/medgemma.jinja");
const MISTRAL3_TPL: &str = include_str!("../../tests/fixtures/chat_templates/mistral3.jinja");
const MUSE_GLIMMER_TPL: &str =
    include_str!("../../tests/fixtures/chat_templates/muse-glimmer.jinja");

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
    let s = TemplateSignals::from_template("{%- if reasoning_effort == \"high\" %}yes{%- endif %}");
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
