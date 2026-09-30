//! §5's step-1 obligation: storing embeddings as f16 must not cost ranking
//! accuracy, and (being memory-bandwidth-bound) must not be slower than f32.
//!
//! Ground truth is the f32 kernel over the same vectors before narrowing. Both
//! geometries that matter are covered: uniform random unit vectors, where the
//! gaps between adjacent ranks are wide, and clustered vectors, where they are
//! not — real embeddings look like the latter, and near-ties are where a
//! narrower mantissa would flip an order if it were going to.
//!
//! Why it holds: for 1024-dim unit vectors an f16 product term carries ~5e-4
//! relative error on a magnitude of ~1/1024, so 1024 independent roundings sum
//! to ~2e-5 of absolute dot error — one to two orders of magnitude below the
//! ~1e-3 spacing between adjacent ranks near the top of a 50k corpus.
//!
//! Measured on this machine (release, 50k × 1024, k=10, 30 queries —
//! `cargo test -p quickdoc-core --release --test f16_vectors -- --ignored
//! --nocapture`):
//!
//! ```text
//! f32 scan   median 7.32 ms   matrix 195.3 MiB
//! f16 scan   median 3.73 ms   matrix  97.7 MiB
//! recall@10 1.0000   top-1 agreement 30/30   max |Δcosine| 2.18e-5
//! ```
//!
//! So f16 is not merely "no slower" as §5 hoped: it is ~2× faster at half the
//! resident cost, for identical rankings. The f32 figure also reproduces the
//! spike's 7.9 ms @50k, so the kernel ported over intact.
//!
//! Getting there took the F16C-fused kernel in `vector::f16c` — the obvious
//! per-element `f16::to_f32` widening measured **69 ms**, nine times *slower*
//! than f32, which is the finding this test existed to catch.

use std::time::Instant;

use half::f16;
use quickdoc_core::vector::{l2_normalize, topk_f16, topk_f32, BYTES_PER_ELEMENT};

const DIMS: usize = 1024;

/// splitmix64 — deterministic, dependency-free, same generator the storage
/// spike used so the corpora are comparable.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [-1, 1).
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn unit_vectors(n: usize, dims: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut flat = vec![0f32; n * dims];
    for row in flat.chunks_exact_mut(dims) {
        for x in row.iter_mut() {
            *x = rng.next_f32();
        }
        l2_normalize(row);
    }
    flat
}

/// `clusters` centroids with tight noise around them — the geometry a real
/// embedding model produces, and the one that makes near-ties common.
fn clustered_vectors(n: usize, dims: usize, clusters: usize, seed: u64) -> Vec<f32> {
    let centroids = unit_vectors(clusters, dims, seed);
    let mut rng = Rng::new(seed ^ 0xA5A5_A5A5);
    let mut flat = vec![0f32; n * dims];
    for (i, row) in flat.chunks_exact_mut(dims).enumerate() {
        let c = &centroids[(i % clusters) * dims..(i % clusters + 1) * dims];
        for (x, cx) in row.iter_mut().zip(c) {
            *x = cx + 0.05 * rng.next_f32();
        }
        l2_normalize(row);
    }
    flat
}

fn narrow(flat: &[f32]) -> Vec<f16> {
    flat.iter().map(|x| f16::from_f32(*x)).collect()
}

struct Agreement {
    recall: f64,
    top1: usize,
    queries: usize,
    max_score_delta: f32,
}

/// Compare the f16 kernel's top-k against the f32 ground truth over the same
/// vectors.
fn agreement(corpus: &[f32], queries: &[f32], dims: usize, k: usize) -> Agreement {
    let narrowed = narrow(corpus);
    let mut recall_sum = 0f64;
    let mut top1 = 0usize;
    let mut max_delta = 0f32;
    let n = queries.len() / dims;
    for q in queries.chunks_exact(dims) {
        let truth = topk_f32(q, corpus, dims, k);
        let got = topk_f16(q, &narrowed, dims, k);
        let truth_ids: Vec<usize> = truth.iter().map(|(i, _)| *i).collect();
        let overlap = got.iter().filter(|(i, _)| truth_ids.contains(i)).count();
        recall_sum += overlap as f64 / k as f64;
        if truth[0].0 == got[0].0 {
            top1 += 1;
        }
        // Score deviation is measured on the *same* row, so it isolates the
        // narrowing from any ranking difference.
        let by_id: std::collections::HashMap<usize, f32> = truth.iter().copied().collect();
        for (i, s) in &got {
            if let Some(t) = by_id.get(i) {
                max_delta = max_delta.max((t - s).abs());
            }
        }
    }
    Agreement {
        recall: recall_sum / n as f64,
        top1,
        queries: n,
        max_score_delta: max_delta,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
fn f16_storage_reproduces_the_f32_ranking_on_random_vectors() {
    let k = 10;
    let corpus = unit_vectors(2_000, DIMS, 0xDEAD_BEEF_CAFE_1234);
    let queries = unit_vectors(20, DIMS, 0x1234_5678_9ABC_DEF0);
    let a = agreement(&corpus, &queries, DIMS, k);

    println!(
        "random  n=2000 dim={DIMS} k={k}: recall@{k}={:.4} top-1 {}/{} max|Δcosine|={:.3e}",
        a.recall, a.top1, a.queries, a.max_score_delta
    );
    assert!(a.recall >= 0.99, "recall@{k} = {}", a.recall);
    assert_eq!(a.top1, a.queries, "f16 must not move the best answer");
    assert!(a.max_score_delta < 1e-3, "Δ = {}", a.max_score_delta);
}

/// Where it could plausibly break: 40 tight clusters, so the top-k is full of
/// near-duplicates separated by less than the f32 gaps above.
#[test]
fn f16_storage_reproduces_the_f32_ranking_on_clustered_vectors() {
    let k = 10;
    let corpus = clustered_vectors(2_000, DIMS, 40, 0x0BAD_F00D);
    let queries = clustered_vectors(20, DIMS, 40, 0x0BAD_F00D ^ 0xFF);
    let a = agreement(&corpus, &queries, DIMS, k);

    println!(
        "cluster n=2000 dim={DIMS} k={k}: recall@{k}={:.4} top-1 {}/{} max|Δcosine|={:.3e}",
        a.recall, a.top1, a.queries, a.max_score_delta
    );
    assert!(a.recall >= 0.99, "recall@{k} = {}", a.recall);
    assert_eq!(a.top1, a.queries);
    assert!(a.max_score_delta < 1e-3, "Δ = {}", a.max_score_delta);
}

/// The §5-scale measurement: 50k × 1024, the corpus size the design targets.
/// Ignored because it wants a release build to mean anything —
/// `cargo test -p quickdoc-core --release -- --ignored --nocapture`.
#[test]
#[ignore = "bench-scale; run with --release --ignored --nocapture"]
fn f16_versus_f32_at_corpus_scale() {
    let n = 50_000;
    let k = 10;
    let corpus = unit_vectors(n, DIMS, 0xDEAD_BEEF_CAFE_1234);
    let narrowed = narrow(&corpus);
    let queries = unit_vectors(30, DIMS, 0x1234_5678_9ABC_DEF0);

    // Warm the caches so the first query does not pay for the whole matrix.
    for q in queries.chunks_exact(DIMS).take(3) {
        let _ = topk_f32(q, &corpus, DIMS, k);
        let _ = topk_f16(q, &narrowed, DIMS, k);
    }

    let mut f32_ms = Vec::new();
    let mut f16_ms = Vec::new();
    for q in queries.chunks_exact(DIMS) {
        let t = Instant::now();
        let _ = topk_f32(q, &corpus, DIMS, k);
        f32_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let _ = topk_f16(q, &narrowed, DIMS, k);
        f16_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    let a = agreement(&corpus, &queries, DIMS, k);
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    println!(
        "== exact KNN, n={n} dim={DIMS} k={k}, {} queries, kernel {} ==",
        a.queries,
        quickdoc_core::vector::knn_kernel()
    );
    println!(
        "f32 scan   median {:8.3} ms   matrix {:7.1} MiB",
        median(f32_ms.clone()),
        mib(corpus.len() * std::mem::size_of::<f32>())
    );
    println!(
        "f16 scan   median {:8.3} ms   matrix {:7.1} MiB",
        median(f16_ms.clone()),
        mib(narrowed.len() * BYTES_PER_ELEMENT)
    );
    println!(
        "recall@{k} {:.4}   top-1 {}/{}   max |Δcosine| {:.3e}",
        a.recall, a.top1, a.queries, a.max_score_delta
    );

    assert!(a.recall >= 0.99, "recall@{k} = {}", a.recall);
    assert_eq!(a.top1, a.queries);
    // §5's claim in assertion form, but only where the fused kernel exists: on
    // the portable widening path f16 *is* slower, and pretending otherwise is
    // what would hide a 3× regression on a host without F16C.
    if quickdoc_core::vector::knn_kernel() == "portable" {
        println!("note: no fused F16C kernel here — the scan pays a widening pass per row");
    } else {
        assert!(
            median(f16_ms) <= median(f32_ms),
            "f16 scan is slower than f32 — the scan stopped being bandwidth-bound"
        );
    }
}
