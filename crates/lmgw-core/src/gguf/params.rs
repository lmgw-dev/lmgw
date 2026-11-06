//! What a header's descriptors say about the model's size (find-a-model
//! design D7, D49, D131): the parameter count and the repeating layers' bits
//! per weight, from each tensor's element count
//! ([`GgufMeta::tensor_elements`]), its rank and type, and its bytes
//! ([`layer_tensors`] says which).
//!
//! A split GGUF (`…-00001-of-00002.gguf`, `split.count` 2) spreads its
//! tensors over its parts, and one part's descriptors name only its own. So
//! [`ModelSummary::parameter_count`](super::ModelSummary::parameter_count)
//! is the model's only for a file that isn't split; a part's share is
//! [`own_parameter_count`], and the whole count adds every part's share up
//! ([`whole_parameter_count`]). A later part's header is its descriptors and
//! three keys (16 KB for Qwen3.6-27B BF16's second part, §2.2), so a caller
//! that wants the whole count reads each part's header and adds them; WP2's
//! hub plan is the first such caller.

use super::{GgufMeta, ModelSummary};

/// The parts a split file declares (`split.count`); 1 for a file that isn't
/// split.
pub fn split_count(meta: &GgufMeta) -> u64 {
    meta.u64("split.count").unwrap_or(1).max(1)
}

/// Every descriptor's elements, summed: this file's own tensors. `None`
/// with no tensors, or when a count overflows.
pub fn own_parameter_count(meta: &GgufMeta) -> Option<u64> {
    if meta.tensor_elements.is_empty() {
        return None;
    }
    meta.tensor_elements
        .iter()
        .try_fold(0u64, |sum, e| sum.checked_add((*e)?))
}

/// The model's parameters from one file: [`own_parameter_count`] for a file
/// that isn't split, `None` for a part of one (module doc).
pub fn parameter_count(meta: &GgufMeta) -> Option<u64> {
    match split_count(meta) {
        1 => own_parameter_count(meta),
        _ => None,
    }
}

/// The whole model's parameters from every part's header: each part's own
/// count, added up. `None` unless the parts are exactly the `split.count`
/// the first one declares, each once (`split.no` 0 to count − 1), and every
/// count is known.
pub fn whole_parameter_count(parts: &[&GgufMeta]) -> Option<u64> {
    let first = parts.first()?;
    let count = split_count(first);
    if parts.len() as u64 != count {
        return None;
    }
    let mut seen = vec![false; parts.len()];
    for p in parts {
        let no = match count {
            1 => 0,
            _ => p.u64("split.no")?,
        };
        let slot = seen.get_mut(usize::try_from(no).ok()?)?;
        if *slot || split_count(p) != count {
            return None;
        }
        *slot = true;
    }
    parts
        .iter()
        .try_fold(0u64, |sum, p| sum.checked_add(own_parameter_count(p)?))
}

/// The repeating layers' weight matrices' bits over their elements
/// ([`layer_tensors`]): the width the file's quantization gave its layers,
/// so wide token embeddings and the F32 norms beside every layer don't
/// widen it. Of this file's tensors only (module doc).
pub fn layer_bits_per_weight(meta: &GgufMeta) -> Option<f64> {
    let (bytes, elements) = layer_tensors(meta)?;
    Some(bytes as f64 * 8.0 / elements as f64)
}

/// The repeating layers' quantized weight matrices, their bytes and
/// elements summed: what [`layer_bits_per_weight`] divides (find-a-model
/// design D7).
///
/// - Only the `blk.*` tensors of two or more dimensions: llama.cpp's
///   quantizer touches no other (`tensor_allows_quantization`), so a layer's
///   norms and biases are F32 in every file, and counting them read a real
///   Q8_0 (`bge-small-en-v1.5-q8_0.gguf`) at 8.566 bits, over D7's cap.
/// - Of those, not the ones the quantizer keeps at F32 (MoE gating,
///   Mamba's conv1d, RWKV's small matrices): they would put a Q8_0 of such
///   a model a hair over the cap. An F32 file, whose matrices are all F32,
///   is measured over all of them, at 32 bits.
/// - Each tensor's bytes are its ggml type's size over its elements, without
///   the padding to the next tensor ([`super::tensors::type_bytes`]), so a
///   Q8_0 reads exactly 8.5; the offsets' bytes when the type isn't one the
///   table knows, or when they are fewer (a cut or synthetic file).
///
/// `None` without such tensors, or when a figure is missing or overflows.
pub fn layer_tensors(meta: &GgufMeta) -> Option<(u64, u64)> {
    use super::tensors::{type_bytes, GGML_F32};
    // (bytes, elements) of the quantized matrices, and of the F32 ones.
    let (mut quant, mut f32s) = ((0u64, 0u64), (0u64, 0u64));
    for (i, name) in meta.tensor_names.iter().enumerate() {
        if !name.starts_with("blk.") || meta.tensor_ranks.get(i).copied().unwrap_or(0) < 2 {
            continue;
        }
        let elements = (*meta.tensor_elements.get(i)?)?;
        let on_disk = *meta.tensor_bytes.get(i)?;
        let ty = meta.tensor_types.get(i).copied();
        let bytes = ty
            .and_then(|t| type_bytes(t, elements))
            .map_or(on_disk, |b| b.min(on_disk));
        let sum = match ty == Some(GGML_F32) {
            true => &mut f32s,
            false => &mut quant,
        };
        sum.0 = sum.0.checked_add(bytes)?;
        sum.1 = sum.1.checked_add(elements)?;
    }
    let (bytes, elements) = if quant.1 > 0 { quant } else { f32s };
    (elements > 0).then_some((bytes, elements))
}

/// [`whole_parameter_count`] over the parts' summaries, the form a header
/// read off the hub keeps ([`crate::hf::remote_header`]): each part's own
/// count added up, `None` unless the parts are exactly the set the first
/// declares, each once.
pub fn whole_parameter_count_of(parts: &[&ModelSummary]) -> Option<u64> {
    if !exactly_the_set(parts) {
        return None;
    }
    parts
        .iter()
        .try_fold(0u64, |sum, p| sum.checked_add(p.part_parameter_count?))
}

/// The repeating layers' width over every part of a split file: the parts'
/// `blk.*` bytes over their elements, each part's own tensors added up
/// (D131). `None` unless the parts are exactly the set, or when no part has
/// such tensors. A part without any (a later part that holds only the
/// output head) adds nothing.
pub fn whole_layer_bits(parts: &[&ModelSummary]) -> Option<f64> {
    if !exactly_the_set(parts) {
        return None;
    }
    let (mut bytes, mut elements) = (0u64, 0u64);
    for p in parts {
        if let Some((b, e)) = p.layer_tensors {
            bytes = bytes.checked_add(b)?;
            elements = elements.checked_add(e)?;
        }
    }
    (elements > 0).then(|| bytes as f64 * 8.0 / elements as f64)
}

/// Whether `parts` are exactly the split set the first declares, each once
/// (`split.no` 0 to count − 1); a file that isn't split is its own set.
fn exactly_the_set(parts: &[&ModelSummary]) -> bool {
    let Some(first) = parts.first() else {
        return false;
    };
    let count = first.split_count.unwrap_or(1).max(1);
    if parts.len() as u64 != count {
        return false;
    }
    let mut seen = vec![false; parts.len()];
    for p in parts {
        let no = match count {
            1 => 0,
            _ => match p.split_no {
                Some(n) => n,
                None => return false,
            },
        };
        let Some(slot) = usize::try_from(no).ok().and_then(|n| seen.get_mut(n)) else {
            return false;
        };
        if *slot || p.split_count.unwrap_or(1).max(1) != count {
            return false;
        }
        *slot = true;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::super::{read_header_from, synth, ModelSummary};
    use super::*;

    fn read(h: &synth::Header) -> GgufMeta {
        let b = h.bytes();
        read_header_from(Cursor::new(b.clone()), b.len() as u64).unwrap()
    }

    /// A part of a split file: `split.no`, `split.count`, and its tensors.
    fn part(no: u64, count: u64, dims: &[u64]) -> synth::Header {
        let mut h = synth::chat("demoarch", 4096);
        h.u64("split.no", no).u64("split.count", count);
        h.tensor_shaped(
            &format!("blk.{no}.attn_q.weight"),
            dims,
            dims.iter().product(),
        );
        h
    }

    /// A part has no model count of its own, only its share; the parts'
    /// shares add up to the model's, and a set that is short, doubled or
    /// mixed gives none (D131).
    #[test]
    fn a_split_files_count_is_its_parts_added_up() {
        let (a, b) = (read(&part(0, 2, &[64, 64])), read(&part(1, 2, &[32, 32])));
        let sa = ModelSummary::from_meta(&a);
        assert_eq!(sa.parameter_count, None, "a part isn't the model");
        assert_eq!(sa.part_parameter_count, Some(64 * 64));
        assert_eq!(whole_parameter_count(&[&a, &b]), Some(64 * 64 + 32 * 32));
        assert_eq!(whole_parameter_count(&[&b, &a]), Some(64 * 64 + 32 * 32));
        assert_eq!(whole_parameter_count(&[&a]), None, "a part missing");
        assert_eq!(whole_parameter_count(&[&a, &a]), None, "a part twice");
        let other = read(&part(1, 3, &[8, 8]));
        assert_eq!(whole_parameter_count(&[&a, &other]), None, "another set");

        // A file that isn't split is its own whole.
        let mut whole = synth::chat("demoarch", 4096);
        whole.tensor_shaped("blk.0.attn_q.weight", &[16, 16], 256);
        let w = read(&whole);
        let sw = ModelSummary::from_meta(&w);
        assert_eq!(sw.parameter_count, Some(256));
        assert_eq!(sw.part_parameter_count, Some(256));
        assert_eq!(whole_parameter_count(&[&w]), Some(256));
    }
    /// A Q8_0 file as llama.cpp's quantizer writes it: its layers' matrices
    /// Q8_0, one of them a shape whose bytes aren't a multiple of the
    /// alignment (so the offsets give it padding), the norms and biases F32
    /// and 1-D, and an MoE gate kept F32 though it is 2-D. The width is
    /// Q8_0's own 8.5 bits, not more (D7's cap); an F32 file reads 32; a
    /// file without layer matrices has none.
    #[test]
    fn the_width_is_the_quantized_matrices_alone() {
        const F32: u32 = 0;
        const Q8_0: u32 = 8;
        let mut q8 = synth::chat("demoarch", 4096);
        q8.tensor_typed("token_embd.weight", &[64, 1000], F32);
        for i in 0..2 {
            q8.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[64], F32)
                .tensor_typed(&format!("blk.{i}.attn_q.weight"), &[64, 64], Q8_0)
                .tensor_typed(&format!("blk.{i}.attn_q.bias"), &[64], F32)
                .tensor_typed(&format!("blk.{i}.ffn_gate_inp.weight"), &[64, 8], F32)
                .tensor_typed(&format!("blk.{i}.ffn_odd.weight"), &[32, 3], Q8_0);
        }
        q8.tensor_typed("output_norm.weight", &[64], F32);
        let meta = read(&q8);
        let odd = meta
            .tensor_names
            .iter()
            .position(|n| n == "blk.0.ffn_odd.weight")
            .unwrap();
        assert!(meta.tensor_bytes[odd] > 102, "the offsets pad it");
        assert_eq!(layer_bits_per_weight(&meta), Some(8.5));

        let mut f32s = synth::chat("demoarch", 4096);
        f32s.tensor_typed("blk.0.attn_norm.weight", &[64], F32)
            .tensor_typed("blk.0.attn_q.weight", &[64, 64], F32);
        assert_eq!(layer_bits_per_weight(&read(&f32s)), Some(32.0));

        let mut flat = synth::chat("demoarch", 4096);
        flat.tensor_typed("blk.0.attn_norm.weight", &[64], F32);
        assert_eq!(layer_bits_per_weight(&read(&flat)), None);
    }

    /// The real Q8_0 the review measured (`bge-small-en-v1.5-q8_0.gguf`,
    /// 8.566 bits over every `blk.*` tensor) reads Q8_0's 8.5 from its
    /// header. Skipped unless `LMGW_TEST_BGE_Q8_GGUF` names a local copy of
    /// `bge-small-en-v1.5-q8_0.gguf`; the file is not part of the repo.
    #[test]
    fn a_real_q8_0_reads_8_5_bits() {
        let Some(path) = std::env::var_os("LMGW_TEST_BGE_Q8_GGUF").map(std::path::PathBuf::from)
        else {
            return;
        };
        if !path.is_file() {
            eprintln!("skipped: {} is not here", path.display());
            return;
        }
        let meta = super::super::read_header(&path).unwrap();
        assert_eq!(layer_bits_per_weight(&meta), Some(8.5));
        let s = ModelSummary::from_meta(&meta);
        assert_eq!(s.layer_bits_per_weight, Some(8.5));
    }

    /// The summaries' form gives the same whole count, and the width over
    /// every part weighs each part's layers by their elements (D131): a part
    /// at 8 bits and one at 4 bits of a quarter the elements give 7.2.
    #[test]
    fn the_summaries_add_up_the_count_and_the_width() {
        let mut a = part(0, 2, &[64, 64]);
        a.tensor_shaped("blk.1.ffn_up.weight", &[64, 64], 64 * 64);
        let mut b = synth::chat("demoarch", 4096);
        b.u64("split.no", 1).u64("split.count", 2);
        b.tensor_shaped("blk.2.ffn_up.weight", &[64, 32], 64 * 32 / 2);
        let (sa, sb) = (
            ModelSummary::from_meta(&read(&a)),
            ModelSummary::from_meta(&read(&b)),
        );
        assert_eq!(sa.split_no, Some(0));
        assert_eq!(sb.split_count, Some(2));
        assert_eq!(sa.layer_bits_per_weight, Some(8.0));
        assert_eq!(sb.layer_bits_per_weight, Some(4.0));
        assert_eq!(
            whole_parameter_count_of(&[&sa, &sb]),
            Some(2 * 64 * 64 + 64 * 32)
        );
        let bits = whole_layer_bits(&[&sb, &sa]).unwrap();
        let want = (2.0 * 64.0 * 64.0 + 64.0 * 16.0) * 8.0 / (2.0 * 64.0 * 64.0 + 64.0 * 32.0);
        assert!((bits - want).abs() < 1e-9, "{bits} vs {want}");
        assert_eq!(whole_layer_bits(&[&sa]), None, "a part missing");
        assert_eq!(whole_parameter_count_of(&[&sa, &sa]), None, "a part twice");
    }
}
