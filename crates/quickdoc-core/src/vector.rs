//! Embedding storage format and the exact-KNN kernel (§5).
//!
//! The spike settled the engine: a flat row-major matrix scanned in process
//! beat sqlite-vec 9× and LanceDB 17× at exact top-10 over 50k × 1024-dim
//! vectors, for +0.14 MB of binary and zero new native dependencies. The scan
//! is memory-bandwidth-bound, which is why vectors are **f16** on disk *and*
//! while resident — halving the matrix is the whole optimisation. [`dot_f16`]
//! widens each element as it reads it; `tests/f16_vectors.rs` verifies that
//! costs no ranking accuracy.
//!
//! Vectors are L2-normalised before encoding, so a dot product *is* cosine
//! similarity and the kernel needs no per-row norm.

use half::f16;
use half::slice::HalfFloatSliceExt;

use crate::error::{QuickdocError, Result};

/// Bytes per stored element. Public because the Docs tab shows resident cost
/// per corpus (§5) and that number should come from here, not a constant
/// retyped in the UI.
pub const BYTES_PER_ELEMENT: usize = std::mem::size_of::<f16>();

/// Scale to unit length in place. A zero vector is left alone — the caller
/// decides whether that is an error (it is, for a reranker-poisoned embedding:
/// see §9a's zero-vector footgun).
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn is_zero(v: &[f32]) -> bool {
    v.iter().all(|x| *x == 0.0)
}

/// f32 → little-endian f16 blob, as stored in `chunk.embedding`.
pub fn encode_f16(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * BYTES_PER_ELEMENT);
    for x in v {
        out.extend_from_slice(&f16::from_f32(*x).to_le_bytes());
    }
    out
}

/// Blob → f16 elements. Fails on a truncated blob rather than scanning garbage.
pub fn decode_f16(blob: &[u8]) -> Result<Vec<f16>> {
    if !blob.len().is_multiple_of(BYTES_PER_ELEMENT) {
        return Err(QuickdocError::Invalid(format!(
            "embedding blob is {} bytes, not a whole number of f16 elements",
            blob.len()
        )));
    }
    Ok(blob
        .as_chunks::<BYTES_PER_ELEMENT>()
        .0
        .iter()
        .map(|c| f16::from_le_bytes([c[0], c[1]]))
        .collect())
}

pub fn decode_f16_to_f32(blob: &[u8]) -> Result<Vec<f32>> {
    Ok(decode_f16(blob)?.iter().map(|x| x.to_f32()).collect())
}

/// F16C-fused widening: the row is converted eight lanes at a time straight
/// into the FMA, never through memory. See [`dot_f16`] for why that matters.
#[cfg(target_arch = "x86_64")]
mod f16c {
    use half::f16;
    use std::arch::x86_64::*;

    /// Cached because the scan calls this once per row.
    pub fn available() -> bool {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            std::arch::is_x86_feature_detected!("avx")
                && std::arch::is_x86_feature_detected!("f16c")
                && std::arch::is_x86_feature_detected!("fma")
        })
    }

    /// # Safety
    /// Caller must have checked [`available`]. `query` and `row` must be the
    /// same length.
    #[target_feature(enable = "avx,f16c,fma")]
    pub unsafe fn dot(query: &[f32], row: &[f16]) -> f32 {
        let n = query.len().min(row.len());
        let qp = query.as_ptr();
        // f16 is `repr(transparent)` over u16, so eight of them are one
        // unaligned 128-bit load.
        let rp = row.as_ptr();
        // Two accumulators: the FMA latency chain is 4-5 cycles and one
        // dependent chain would stall the loop at a third of peak.
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let mut i = 0usize;
        while i + 16 <= n {
            let r0 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i).cast()));
            let r1 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i + 8).cast()));
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i)), r0, acc0);
            acc1 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i + 8)), r1, acc1);
            i += 16;
        }
        while i + 8 <= n {
            let r0 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i).cast()));
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i)), r0, acc0);
            i += 8;
        }
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), _mm256_add_ps(acc0, acc1));
        let mut total: f32 = lanes.iter().sum();
        while i < n {
            total += *qp.add(i) * (*rp.add(i)).to_f32();
            i += 1;
        }
        total
    }
}

/// Cosine similarity of an f32 query against one f16 row. `scratch` must be
/// `row.len()` long; it is only touched on the portable path.
///
/// How the row gets widened is what decides whether §5's memory saving is free.
/// Measured at 50k × 1024 in release (`tests/f16_vectors.rs`; the f32 reference
/// scan is 7.3 ms):
///
/// | widening | median |
/// |---|---|
/// | `f16::to_f32` per element | 69.1 ms |
/// | `convert_to_f32_slice` into a scratch row | 13.5 ms |
/// | F16C fused into the dot product | 3.7 ms |
///
/// Per-element conversion re-runs `half`'s CPU-feature check on every value and
/// blocks LLVM from vectorising the loop. The slice conversion dispatches once
/// but still round-trips a whole f32 row through memory. Only the fused kernel
/// leaves the scan bandwidth-bound, which is where f16 becomes the win the
/// design assumed: half the bytes read, so ~2× *faster* than f32.
#[inline]
pub fn dot_f16(query: &[f32], row: &[f16], scratch: &mut [f32]) -> f32 {
    debug_assert_eq!(query.len(), row.len());
    #[cfg(target_arch = "x86_64")]
    if f16c::available() {
        return unsafe { f16c::dot(query, row) };
    }
    row.convert_to_f32_slice(scratch);
    dot_f32(query, scratch)
}

/// Plain f32 dot product, accumulated in eight lanes so LLVM autovectorises
/// (AVX2 does eight per iteration). Serves the portable widening path, and is
/// the ground truth the f16 kernel is measured against
/// (`tests/f16_vectors.rs`).
#[inline]
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0f32; 8];
    let (ia, ra) = a.as_chunks::<8>();
    let (ib, rb) = b.as_chunks::<8>();
    for (ca, cb) in ia.iter().zip(ib) {
        for k in 0..8 {
            acc[k] += ca[k] * cb[k];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Which widening kernel the exact-KNN scan uses on this machine. Surfaced
/// (search trace, Docs tab) so a latency surprise has a visible cause rather
/// than being a silent 3× on a host without F16C.
pub fn knn_kernel() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if f16c::available() {
        return "f16c+avx+fma";
    }
    "portable"
}

/// Bounded insertion into a k-sized list: no per-candidate allocation, and the
/// common case (candidate worse than the current k-th) is one comparison.
#[inline]
fn offer(best: &mut Vec<(usize, f32)>, k: usize, idx: usize, score: f32) {
    if best.len() == k && score <= best[k - 1].1 {
        return;
    }
    let pos = best.partition_point(|(_, s)| *s > score);
    best.insert(pos, (idx, score));
    if best.len() > k {
        best.pop();
    }
}

/// Exact top-k over a flat row-major f16 matrix. Returns `(row index, cosine)`
/// best first.
pub fn topk_f16(query: &[f32], flat: &[f16], dims: usize, k: usize) -> Vec<(usize, f32)> {
    let mut best: Vec<(usize, f32)> = Vec::with_capacity(k + 1);
    if k == 0 || dims == 0 {
        return best;
    }
    // Dispatch once, outside the loop.
    #[cfg(target_arch = "x86_64")]
    if f16c::available() {
        for (i, row) in flat.chunks_exact(dims).enumerate() {
            offer(&mut best, k, i, unsafe { f16c::dot(query, row) });
        }
        return best;
    }
    // Portable path: one row's worth of widened floats, reused for the whole
    // scan — 4 KiB at 1024 dims, so it stays in L1 while the matrix streams
    // past it.
    let mut scratch = vec![0f32; dims];
    for (i, row) in flat.chunks_exact(dims).enumerate() {
        row.convert_to_f32_slice(&mut scratch);
        offer(&mut best, k, i, dot_f32(query, &scratch));
    }
    best
}

/// Same scan over an f32 matrix — the ground truth for [`topk_f16`].
pub fn topk_f32(query: &[f32], flat: &[f32], dims: usize, k: usize) -> Vec<(usize, f32)> {
    let mut best: Vec<(usize, f32)> = Vec::with_capacity(k + 1);
    if k == 0 || dims == 0 {
        return best;
    }
    for (i, row) in flat.chunks_exact(dims).enumerate() {
        offer(&mut best, k, i, dot_f32(query, row));
    }
    best
}

/// One corpus's vectors, resident. Built once per [`crate::retrieve::Retriever`]
/// and scanned per query.
///
/// The cost is deliberately visible rather than capped: [`resident_bytes`] is
/// what the Docs tab reports, and a corpus that does not fit is an error the
/// owner sees, not a silently truncated index.
///
/// [`resident_bytes`]: VectorMatrix::resident_bytes
#[derive(Debug, Clone)]
pub struct VectorMatrix {
    dims: usize,
    ids: Vec<String>,
    data: Vec<f16>,
}

impl VectorMatrix {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            ids: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Append one stored blob. `corpus` only names the corpus in the error.
    pub fn push_blob(&mut self, corpus: &str, id: String, blob: &[u8]) -> Result<()> {
        let row = decode_f16(blob)?;
        if row.len() != self.dims {
            return Err(QuickdocError::DimsMismatch {
                corpus: corpus.to_string(),
                expected: self.dims,
                got: row.len(),
            });
        }
        self.ids.push(id);
        self.data.extend_from_slice(&row);
        Ok(())
    }

    /// Append an f32 vector, normalising and narrowing it the way the store
    /// does. Used by the in-memory fixtures and by re-embed paths that already
    /// hold the vector.
    pub fn push_vector(&mut self, corpus: &str, id: String, v: &[f32]) -> Result<()> {
        if v.len() != self.dims {
            return Err(QuickdocError::DimsMismatch {
                corpus: corpus.to_string(),
                expected: self.dims,
                got: v.len(),
            });
        }
        let mut owned = v.to_vec();
        l2_normalize(&mut owned);
        self.ids.push(id);
        self.data.extend(owned.iter().map(|x| f16::from_f32(*x)));
        Ok(())
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Bytes the matrix itself holds (ids excluded — they are the chunk keys,
    /// not the index).
    pub fn resident_bytes(&self) -> usize {
        self.data.len() * BYTES_PER_ELEMENT
    }

    /// Exact KNN. `query` is normalised by the caller; a width mismatch is an
    /// error, never a truncated or padded comparison.
    pub fn search(&self, corpus: &str, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        if query.len() != self.dims {
            return Err(QuickdocError::DimsMismatch {
                corpus: corpus.to_string(),
                expected: self.dims,
                got: query.len(),
            });
        }
        Ok(topk_f16(query, &self.data, self.dims, k)
            .into_iter()
            .map(|(i, s)| (self.ids[i].clone(), s))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_f16_within_half_precision() {
        let mut v = vec![0.5f32, -0.25, 0.125, 0.0625];
        l2_normalize(&mut v);
        let back = decode_f16_to_f32(&encode_f16(&v)).unwrap();
        for (a, b) in v.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn rejects_a_truncated_blob() {
        assert!(decode_f16(&[0u8; 3]).is_err());
    }

    #[test]
    fn search_ranks_by_cosine_and_refuses_a_wrong_width_query() {
        let mut m = VectorMatrix::new(4);
        m.push_vector("t@1", "near".into(), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        m.push_vector("t@1", "mid".into(), &[0.7, 0.7, 0.0, 0.0])
            .unwrap();
        m.push_vector("t@1", "far".into(), &[0.0, 0.0, 0.0, 1.0])
            .unwrap();
        let top = m.search("t@1", &[1.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(top[0].0, "near");
        assert_eq!(top[1].0, "mid");
        assert!(m.search("t@1", &[1.0, 0.0], 2).is_err());
        assert_eq!(m.resident_bytes(), 3 * 4 * 2);
    }
}
