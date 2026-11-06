//! A minimal GGUF *header* writer for tests in other crates and modules — the
//! integration suites need "a file whose header says it is an embedding
//! model" without shipping a real GGUF. Only what a header check reads is
//! written: string / u32 / u64 KVs and tensor names with zero-byte data.
//! Hidden from docs because it is not a general-purpose writer.

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
    /// Bytes of tensor data [`Self::tensor_sized`] has laid out.
    data_len: u64,
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
    /// An array of `uint64`s (audio.cpp's `embedded_files.offsets`).
    pub fn arr_u64(&mut self, k: &str, vals: &[u64]) -> &mut Self {
        let mut b = super::VT_U64.to_le_bytes().to_vec();
        b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
        for v in vals {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.kv(k, super::VT_ARRAY, &b)
    }
    /// An array of `uint8`s (audio.cpp's `embedded_files.data`).
    pub fn arr_u8(&mut self, k: &str, vals: &[u8]) -> &mut Self {
        let mut b = super::VT_U8.to_le_bytes().to_vec();
        b.extend_from_slice(&(vals.len() as u64).to_le_bytes());
        b.extend_from_slice(vals);
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
    /// A tensor with `bytes` of (zeroed) data behind it, laid out after
    /// the ones before it, so [`super::GgufMeta::tensor_size`] reads it
    /// back. [`Self::tensor`]'s carry no data.
    pub fn tensor_sized(&mut self, name: &str, bytes: u64) -> &mut Self {
        put_str(&mut self.tensors, name);
        self.tensors.extend_from_slice(&1u32.to_le_bytes());
        self.tensors.extend_from_slice(&1u64.to_le_bytes());
        self.tensors.extend_from_slice(&0u32.to_le_bytes());
        self.tensors.extend_from_slice(&self.data_len.to_le_bytes());
        self.tensor_count += 1;
        self.data_len += bytes;
        self
    }
    /// [`Self::tensor_sized`] with the dimensions a real descriptor has,
    /// so the element count ([`super::GgufMeta::tensor_elements`]) is
    /// the product of `dims`.
    pub fn tensor_shaped(&mut self, name: &str, dims: &[u64], bytes: u64) -> &mut Self {
        put_str(&mut self.tensors, name);
        self.tensors
            .extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for d in dims {
            self.tensors.extend_from_slice(&d.to_le_bytes());
        }
        self.tensors.extend_from_slice(&0u32.to_le_bytes());
        self.tensors.extend_from_slice(&self.data_len.to_le_bytes());
        self.tensor_count += 1;
        self.data_len += bytes;
        self
    }
    /// A tensor as a real file lays it out: `dims` of the ggml type id
    /// `ggml_type` (`0` F32, `8` Q8_0, `12` Q4_K, `30` BF16, …), its data
    /// that type's exact size and its offset aligned to GGUF's default 32
    /// bytes, the padding before it counted to the tensor before (what a
    /// real file's offsets give). Panics on a type the size table doesn't
    /// know: a test's mistake.
    pub fn tensor_typed(&mut self, name: &str, dims: &[u64], ggml_type: u32) -> &mut Self {
        let elements: u64 = dims.iter().product();
        let bytes = super::tensors::type_bytes(ggml_type, elements)
            .unwrap_or_else(|| panic!("{name}: no size for type {ggml_type} × {elements}"));
        self.data_len = self.data_len.div_ceil(32) * 32;
        put_str(&mut self.tensors, name);
        self.tensors
            .extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for d in dims {
            self.tensors.extend_from_slice(&d.to_le_bytes());
        }
        self.tensors.extend_from_slice(&ggml_type.to_le_bytes());
        self.tensors.extend_from_slice(&self.data_len.to_le_bytes());
        self.tensor_count += 1;
        self.data_len += bytes;
        self
    }
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = b"GGUF".to_vec();
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&self.tensor_count.to_le_bytes());
        out.extend_from_slice(&self.kv_count.to_le_bytes());
        out.extend_from_slice(&self.kv);
        out.extend_from_slice(&self.tensors);
        if self.data_len > 0 {
            let start =
                super::tensors::data_start(out.len() as u64, super::tensors::DEFAULT_ALIGNMENT);
            out.resize((start + self.data_len) as usize, 0);
        }
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

/// A header that reads as an audio.cpp package: `audiocpp` architecture,
/// the family's embedded `model_spec.json`, and `files` laid out the way
/// audio.cpp's packer writes them — a names array, an offsets array one
/// longer, and one `uint8` data array holding every file back to back.
pub fn audiocpp(family: &str, model_spec_json: &str, files: &[(&str, &[u8])]) -> Header {
    let mut h = Header::default();
    let mut offsets = vec![0u64];
    let mut data = Vec::new();
    for (_, bytes) in files {
        data.extend_from_slice(bytes);
        offsets.push(data.len() as u64);
    }
    let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    h.str("general.architecture", "audiocpp")
        .str("general.name", "synthetic audio.cpp package")
        .u32("audiocpp.model_spec.version", 1)
        .str("audiocpp.model_spec.family", family)
        .str("audiocpp.model_spec.json", model_spec_json)
        .arr_str("audiocpp.embedded_files.names", &names)
        .arr_u64("audiocpp.embedded_files.offsets", &offsets)
        .arr_u8("audiocpp.embedded_files.data", &data)
        .arr_str("audiocpp.tensor_names", &["weights/a", "weights/b"]);
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
