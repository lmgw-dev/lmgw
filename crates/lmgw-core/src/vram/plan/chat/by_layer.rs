//! What a chat row puts on each card of a split (device-ledger design §4.3,
//! WP6): its footprint taken apart by where llama.cpp puts each part when
//! it splits by layers (`--split-mode layer`).
//!
//! - **Each repeating layer** on the card it is given: its weights (less
//!   what the row's tensor overrides keep in host memory, as the one-card
//!   footprint counts them: `--n-cpu-moe`'s experts are the first layers',
//!   so the first card's weights are less by them) and its KV cache.
//! - **The output layer** on the last card (WP0, §1.1): every weight
//!   outside the repeating layers that is on a card — the output head and
//!   its norm, a tied head's copy. The input embeddings stay in host
//!   memory, as on one card.
//! - **On the first card**, llama.cpp's main device: the projector, a draft
//!   model with its cache and compute buffer (lmgw confines it there with
//!   `-devd CUDA0`, §4.3; llama.cpp would spread it over every card), and
//!   the expert cache (`--moe-cache-mib`, which caches the first layers'
//!   host experts).
//! - **An MTP context** (`draft-mtp` without a draft model) runs the
//!   model's last layers, which are the last card's: its cache and compute
//!   buffer are charged there.
//! - **The compute buffers** ([`ByLayer::compute_on`]). Every card reserves
//!   a graph's buffer for the layers it runs, larger on a split than on
//!   one card. The model is `a·C + b` of the one-card figure `C` (the owner's
//!   ruling, 2026-11-18: wrong factors are corrected in the numbers, the
//!   rule stays), fitted to every split measured: WP0's 4B at 16,384 tokens
//!   (C 93 MiB) and the live splits of 2026-11-18 (§13, WP13's Built note):
//!   the 4B and Qwen3-VL-4B at 262,144 tokens (C 1332), an own split of the
//!   4B at 65,536 (C 372) and Qwen3.6-35B-A3B at 262,144 with `--n-cpu-moe`
//!   (C 1088, its op-offload copy 272 of it). Item 4's first model, the
//!   one-card figure times 2.21 and 1.73, over-planned the first card by
//!   0.8–2.2 GiB at 262,144 tokens.
//!   - **On a pipeline** (`sched copies = 4`): every card [`PIPELINE`]
//!     hundredths of C, the first [`PIPELINE_FIRST`] more, every other
//!     [`PIPELINE_OTHER`] more. Measured against planned (MiB): first 205 /
//!     211, 607 / 652, 2155 / 2169; others 160 / 163, 562 / 604, 2110 /
//!     2121.
//!   - **Without one** (`sched copies = 1`): the first card the one-card
//!     figure and [`FLAT_PLUS`], every other card that figure less the
//!     op-offload copy (`ByLayer::copy`: the copy of a host-kept weight a
//!     large batch's product makes, which lives on the first card alone)
//!     and [`FLAT_PLUS`]. Measured against planned: first 1076 / 1120,
//!     others 827.55 / 848.
//!   - **A projector** on the first card: on a pipeline its model buffer
//!     [`PROJECTOR_MODEL`] more (97 MiB measured twice, at C 93 and 1332;
//!     not seen without a pipeline), and its own buffer, which llama.cpp
//!     sizes from per-projector image limits no header carries (322.49 MiB
//!     for Qwen3-VL-4B's F16 projector, 248.10 for Qwen3.6's):
//!     [`PROJECTOR_COMPUTE`], the larger, as a cap. The one-card estimate
//!     charges the projector's weights only and leaves its buffer to the
//!     start margin, which a split may give up.
//!
//!   **When llama.cpp runs a pipeline** (`src/llama-context.cpp`
//!   at ggml-org/llama.cpp master 97069ff6, 2026-11-18, lines 584–590):
//!   more than one device, `n_gpu_layers` above the model's layers (every
//!   layer and the output layer on the cards), split mode `layer`,
//!   `offload_kqv`, no tensor overrides and no MoE cache; and every device
//!   supporting async compute and events (lines 593–608), which a CUDA
//!   card does. A split of lmgw's is over several cards by layers, so the
//!   row decides the rest ([`Parts::pipeline`]): a `--n-gpu-layers` short
//!   of all, `--no-kv-offload`, any tensor override (`--n-cpu-moe`,
//!   `--cpu-moe`, `--override-tensor`, the `n_cpu_moe` field) or a
//!   `--moe-cache-mib` turns it off. What lmgw cannot know from the row —
//!   a device without async compute or events, llama.cpp falling back
//!   without the pipeline when a buffer fails to allocate (line 866) — is
//!   charged the pipeline's, the larger. Once a split has loaded it is
//!   charged what its process holds on each card, where the driver says so
//!   (`vram::ledger`, §4.3).
//!
//! The parts add up to the one-card footprint: [`ByLayer::on_cards`] of one
//! card is its `total_bytes`.

const MIB: u64 = 1024 * 1024;

/// Every card's compute buffer on a split run as a pipeline, in hundredths
/// of the one-card figure (module doc).
pub const PIPELINE: u64 = 158;
/// What the first card's holds more on a pipeline.
pub const PIPELINE_FIRST: u64 = 64 * MIB;
/// What every other card's holds more on a pipeline.
pub const PIPELINE_OTHER: u64 = 16 * MIB;
/// What every card's holds more without a pipeline.
pub const FLAT_PLUS: u64 = 32 * MIB;
/// What a projector on the first card adds to its model buffer on a
/// pipeline: 97 MiB measured.
pub const PROJECTOR_MODEL: u64 = 100 * MIB;
/// A projector's own compute buffer on a split's first card: 322.49 MiB
/// measured for Qwen3-VL-4B's F16 projector, the larger of the two seen.
pub const PROJECTOR_COMPUTE: u64 = 322 * MIB;

/// `bytes` times `hundredths` / 100, rounded up.
fn times(bytes: u64, hundredths: u64) -> u64 {
    let b = u128::from(bytes) * u128::from(hundredths);
    u64::try_from(b.div_ceil(100)).unwrap_or(u64::MAX)
}

/// A chat row's footprint by where a split puts it (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByLayer {
    /// The first repeating layer on a card: 0, or where the row's
    /// `--n-gpu-layers` short of all leaves the first ones on the CPU. A
    /// split's entries count the layers from it on, and the output layer.
    pub first_gpu: u64,
    /// Each repeating layer from `first_gpu` on: its weights on the card
    /// and its KV cache.
    pub layers: Vec<u64>,
    /// On the last card: the output layer, and an MTP context's cache and
    /// compute buffer.
    pub last: u64,
    /// On the first card: the projector, a draft model's weights, cache and
    /// compute buffer, the expert cache.
    pub first: u64,
    /// On every card: the main context's compute buffer, as one card
    /// reserves it; a split's is larger ([`Self::compute_on`]).
    pub each: u64,
    /// Of `each`: the op-offload copy of a host-kept weight (module doc),
    /// on a split the first card's alone.
    pub copy: u64,
    /// Of each of `layers`: its weights on the card, and its recurrent
    /// state; the rest is its KV cache. Empty where not known (a stated
    /// cost), and then a split's parts are not told by kind.
    pub weights: Vec<u64>,
    pub states: Vec<u64>,
    /// The row runs a draft model, which lmgw confines to the first card.
    pub drafter: bool,
    /// llama.cpp runs a split of it as a pipeline (module doc,
    /// [`Parts::pipeline`]).
    pub pipeline: bool,
    /// A projector goes on the first card.
    pub projector: bool,
}

/// What a split puts on one card by kind ([`ByLayer::parts_on`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardKinds {
    /// Its repeating layers.
    pub layers: u64,
    /// Their weights, KV cache and recurrent state.
    pub weights: u64,
    pub kv: u64,
    pub states: u64,
    /// The main context's compute buffer, and the projector's own.
    pub compute: u64,
    pub projector_compute: u64,
    /// The first card's own parts: the projector's weights, a draft model,
    /// the expert cache.
    pub first: u64,
    /// The output layer, an MTP context, the cache no layer accounts for.
    pub output: u64,
}

/// What [`ByLayer::of`] takes apart: the one-card footprint's terms as
/// `chat::sized` computed them.
pub(crate) struct Parts<'a> {
    pub s: &'a crate::gguf::ModelSummary,
    pub first_gpu: u64,
    /// The row's tensor overrides, its `--n-gpu-layers`' included.
    pub overrides: &'a [super::offload::Override],
    /// The main model's weights on the card.
    pub main_on: u64,
    /// All weights on the card: the main model's, the projector, a draft
    /// model's, the expert cache.
    pub weights: u64,
    /// The main context's cache, and every context's.
    pub kv_main: u64,
    pub kv: u64,
    /// The main context's compute buffer, and every context's.
    pub compute_main: u64,
    pub compute: u64,
    /// The main context's cache of the layers from `i` on, and of it their
    /// recurrent state.
    pub kv_from: &'a dyn Fn(u64) -> u64,
    pub rs_from: &'a dyn Fn(u64) -> u64,
    pub second: &'a super::Second,
    /// Of `compute_main`: its op-offload copy of a host-kept weight.
    pub copy: u64,
    /// Whether llama.cpp runs a split of the row as a pipeline, by what
    /// the row decides of llama.cpp's condition (module doc;
    /// `src/llama-context.cpp` at ggml-org/llama.cpp 97069ff6, lines
    /// 584–590): every layer on the cards (`n_gpu_layers > n_layer`), its
    /// KV cache offloaded (`offload_kqv`), no tensor overrides
    /// (`has_tensor_overrides`) and no MoE cache (`moe_cache_size == 0`).
    pub pipeline: bool,
    /// The row loads a projector.
    pub projector: bool,
}

impl ByLayer {
    /// `p` taken apart (module doc); `None` where the header does not say
    /// what each layer holds (no layer count, a tensor list missing or
    /// incomplete): such a row is not split.
    pub(crate) fn of(p: Parts<'_>) -> Option<Self> {
        let on = super::offload::layer_bytes(p.s, p.overrides)?;
        let n = on.len() as u64;
        if p.first_gpu >= n {
            return None;
        }
        let mut layers = Vec::with_capacity(on.len());
        let mut weights = Vec::with_capacity(on.len());
        let mut states = Vec::with_capacity(on.len());
        let mut kv_layers = 0u64;
        let mut next = (p.kv_from)(p.first_gpu);
        let mut rs_next = (p.rs_from)(p.first_gpu);
        for i in p.first_gpu..n {
            let rs_after = (p.rs_from)(i + 1);
            states.push(rs_next.saturating_sub(rs_after));
            rs_next = rs_after;
            weights.push(on[i as usize]);
            let after = (p.kv_from)(i + 1);
            let kv = next.saturating_sub(after);
            next = after;
            kv_layers = kv_layers.saturating_add(kv);
            layers.push(on[i as usize].saturating_add(kv));
        }
        let output = p.main_on.saturating_sub(on.iter().sum());
        // Whatever of the main cache no layer accounts for stays charged,
        // on the last card.
        let residue = p.kv_main.saturating_sub(kv_layers);
        let second =
            p.kv.saturating_sub(p.kv_main)
                .saturating_add(p.compute.saturating_sub(p.compute_main));
        let (to_first, to_last, drafter) = match p.second {
            super::Second::Draft { .. } => (second, 0, true),
            super::Second::Mtp | super::Second::None => (0, second, false),
        };
        Some(Self {
            first_gpu: p.first_gpu,
            layers,
            last: output.saturating_add(residue).saturating_add(to_last),
            first: p.weights.saturating_sub(p.main_on).saturating_add(to_first),
            each: p.compute_main,
            copy: p.copy.min(p.compute_main),
            weights,
            states,
            drafter,
            pipeline: p.pipeline,
            projector: p.projector,
        })
    }

    /// The main context's compute buffer on the card at place `pos` of `k`
    /// cards that run its layers, in the bus's order (module doc): the
    /// one-card figure on one card; on a split `a·C + b` by whether it runs
    /// as a pipeline, with a projector's on the first.
    pub fn compute_on(&self, pos: usize, k: usize) -> u64 {
        if k < 2 {
            return self.each;
        }
        let projector = |model: u64, own_model: u64| match (pos, self.projector) {
            (0, true) => model
                .saturating_add(own_model)
                .saturating_add(PROJECTOR_COMPUTE),
            _ => model,
        };
        match (self.pipeline, pos) {
            (true, 0) => projector(
                times(self.each, PIPELINE).saturating_add(PIPELINE_FIRST),
                PROJECTOR_MODEL,
            ),
            (true, _) => times(self.each, PIPELINE).saturating_add(PIPELINE_OTHER),
            (false, 0) => projector(self.each.saturating_add(FLAT_PLUS), 0),
            (false, _) => self
                .each
                .saturating_sub(self.copy)
                .saturating_add(FLAT_PLUS),
        }
    }

    /// The compute buffers of a split over `k` cards together
    /// ([`Self::compute_on`]).
    pub fn computes(&self, k: usize) -> u64 {
        (0..k.max(1)).fold(0u64, |a, pos| a.saturating_add(self.compute_on(pos, k)))
    }

    /// What `counts` repeating layers per card, in the cards' order (a run
    /// of whole layers each, from `first_gpu` on), put on each card; `None`
    /// where they are not every layer, or a card is given none but the last
    /// (the output layer is the last card's whatever its count).
    pub fn on_cards(&self, counts: &[u64]) -> Option<Vec<u64>> {
        let k = counts.len();
        if k == 0 || counts.iter().sum::<u64>() != self.layers.len() as u64 {
            return None;
        }
        let mut at = 0usize;
        let mut out = Vec::with_capacity(k);
        for (j, n) in counts.iter().enumerate() {
            let n = usize::try_from(*n).ok()?;
            let layers: u64 = self.layers[at..at + n].iter().sum();
            at += n;
            let mut bytes = layers.saturating_add(self.compute_on(j, k));
            if j == 0 {
                bytes = bytes.saturating_add(self.first);
            }
            if j + 1 == k {
                bytes = bytes.saturating_add(self.last);
            }
            out.push(bytes);
        }
        Some(out)
    }

    /// What a row's own `--tensor-split` (device-ledger design §7.2,
    /// ruling 11, WP11) puts on each card: `entries`, one per card in the
    /// cards' order, divided as llama.cpp divides them. It normalises the
    /// entries' running sums to 1 and gives the layer `i` places from the
    /// first on a card (the output layer the last of them, `n` layers
    /// with it in all) to the first card whose running share is more than
    /// `i / n`, in `f32` as it does. Each card is charged the layers it is
    /// given and, where it is given any, a compute buffer — a split's
    /// ([`Self::compute_on`]) where more than one card is; the first card
    /// the first card's parts, the card the output layer lands on the last
    /// card's. The repeating layers on each card, the bytes, and the card
    /// the output layer lands on (by its place among them); `None`
    /// where the entries are none, all zero, or not numbers of 0 or more
    /// (llama.cpp then splits by free memory, which lmgw cannot predict).
    pub fn by_entries(&self, entries: &[f32]) -> Option<(Vec<u64>, Vec<u64>, usize)> {
        let k = entries.len();
        if k == 0 || entries.iter().any(|e| !e.is_finite() || *e < 0.0) {
            return None;
        }
        let mut running = Vec::with_capacity(k);
        let mut sum = 0f32;
        for e in entries {
            sum += e;
            running.push(sum);
        }
        if sum <= 0.0 || !sum.is_finite() {
            return None;
        }
        for r in running.iter_mut() {
            *r /= sum;
        }
        let n = self.layers.len() + 1;
        let card = |i: usize| -> usize {
            let x = i as f32 / n as f32;
            running.iter().position(|r| *r > x).unwrap_or(k - 1)
        };
        let mut counts = vec![0u64; k];
        let mut bytes = vec![0u64; k];
        for (i, b) in self.layers.iter().enumerate() {
            let j = card(i);
            counts[j] += 1;
            bytes[j] = bytes[j].saturating_add(*b);
        }
        let output = card(self.layers.len());
        // The cards that reserve a compute buffer, in order: a split over
        // more than one of them is a pipeline's ([`Self::compute_on`]).
        let runs: Vec<usize> = (0..k).filter(|&j| counts[j] > 0 || j == output).collect();
        for (j, b) in bytes.iter_mut().enumerate() {
            if let Some(pos) = runs.iter().position(|&r| r == j) {
                *b = b.saturating_add(self.compute_on(pos, runs.len()));
            }
            if j == 0 {
                *b = b.saturating_add(self.first);
            }
            if j == output {
                *b = b.saturating_add(self.last);
            }
        }
        Some((counts, bytes, output))
    }

    /// What a split puts on each card by kind, for the start's log (owner,
    /// 2026-11-18: so a live check compares it with llama.cpp's buffer
    /// lines): `counts` repeating layers per card in the cards' order,
    /// `bytes` what each card is charged (the engine's cost left out),
    /// `output` the card the output layer is on. The compute buffer is what
    /// is left of a card's bytes. `None` where the layers' kinds are not
    /// known or the counts are not every layer.
    pub fn parts_on(&self, counts: &[u64], bytes: &[u64], output: usize) -> Option<Vec<CardKinds>> {
        let n = self.layers.len();
        if self.weights.len() != n
            || self.states.len() != n
            || counts.len() != bytes.len()
            || counts.iter().sum::<u64>() != n as u64
        {
            return None;
        }
        let mut at = 0usize;
        let mut out = Vec::with_capacity(counts.len());
        for (j, c) in counts.iter().enumerate() {
            let c = usize::try_from(*c).ok()?;
            let sum = |v: &[u64]| v[at..at + c].iter().fold(0u64, |a, b| a.saturating_add(*b));
            let (layers, weights, states) =
                (sum(&self.layers), sum(&self.weights), sum(&self.states));
            at += c;
            let first = if j == 0 { self.first } else { 0 };
            let last = if j == output { self.last } else { 0 };
            let compute = bytes[j]
                .saturating_sub(layers)
                .saturating_sub(first)
                .saturating_sub(last);
            let projector = match (j, self.projector) {
                (0, true) => PROJECTOR_COMPUTE.min(compute),
                _ => 0,
            };
            out.push(CardKinds {
                layers: c as u64,
                weights,
                kv: layers.saturating_sub(weights).saturating_sub(states),
                states,
                compute: compute.saturating_sub(projector),
                projector_compute: projector,
                first,
                output: last,
            });
        }
        Some(out)
    }

    /// The most `n` consecutive layers put on a card, wherever the run
    /// starts: what a card given `n` layers needs at most before it is
    /// known which.
    pub fn widest(&self, n: usize) -> u64 {
        let n = n.min(self.layers.len());
        if n == 0 {
            return 0;
        }
        let mut run: u64 = self.layers[..n].iter().sum();
        let mut most = run;
        for i in n..self.layers.len() {
            run = run + self.layers[i] - self.layers[i - n];
            most = most.max(run);
        }
        most
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn four() -> ByLayer {
        ByLayer {
            first_gpu: 0,
            layers: vec![10, 10, 20, 10],
            last: 5,
            first: 7,
            each: 3,
            copy: 0,
            weights: Vec::new(),
            states: Vec::new(),
            drafter: false,
            pipeline: false,
            projector: false,
        }
    }

    /// `four`'s compute buffer on a split: it runs no pipeline, so every
    /// card's is the one-card figure and [`FLAT_PLUS`].
    const C: u64 = 3 + FLAT_PLUS;

    /// The first card gets its layers, its compute buffer and the first
    /// card's parts; the last its layers, its compute buffer and the output
    /// layer; one card all of it once, at the one-card compute buffer.
    #[test]
    fn each_card_gets_its_run_of_layers_and_its_parts() {
        let b = four();
        assert_eq!(b.on_cards(&[1, 3]), Some(vec![10 + C + 7, 40 + C + 5]));
        assert_eq!(b.on_cards(&[4]), Some(vec![50 + 3 + 7 + 5]));
        assert_eq!(
            b.on_cards(&[2, 1, 1]),
            Some(vec![20 + C + 7, 20 + C, 10 + C + 5])
        );
        assert_eq!(b.on_cards(&[2, 1]), None, "not every layer");
    }

    /// A row's own split divides as llama.cpp does: `1,1` over four layers
    /// and the output layer gives the first card the layers whose place is
    /// below half (0, 1, 2), the second the rest and the output layer;
    /// `3,1` the first four layers to the first card. A card given no layer
    /// is charged neither layers nor a compute buffer; the output layer
    /// lands on the last card given a share. Unreadable entries: `None`.
    #[test]
    fn a_row_s_own_entries_divide_as_llama_cpp_does() {
        let b = four();
        assert_eq!(
            b.by_entries(&[1.0, 1.0]),
            Some((vec![3, 1], vec![40 + C + 7, 10 + C + 5], 1))
        );
        assert_eq!(
            b.by_entries(&[3.0, 1.0]),
            Some((vec![4, 0], vec![50 + C + 7, C + 5], 1))
        );
        assert_eq!(
            b.by_entries(&[1.0, 0.0]),
            Some((vec![4, 0], vec![50 + 3 + 7 + 5, 0], 0))
        );
        assert_eq!(
            b.by_entries(&[0.0, 1.0]),
            Some((vec![0, 4], vec![7, 50 + 3 + 5], 1))
        );
        assert_eq!(b.by_entries(&[0.0, 0.0]), None);
        assert_eq!(b.by_entries(&[]), None);
        assert_eq!(b.by_entries(&[1.0, f32::NAN]), None);
        // Every layer is somewhere, once.
        let (counts, _, _) = b.by_entries(&[0.2, 0.5, 0.3]).unwrap();
        assert_eq!(counts.iter().sum::<u64>(), 4);
    }

    /// One measured split: its one-card figure `c` and op-offload copy (lmgw's
    /// estimate for its header at its settings), whether it ran as a
    /// pipeline and had a projector, and what llama.cpp's buffer lines said
    /// on its first and its other cards, in MiB (the projector's own buffer
    /// left out).
    struct Measured {
        name: &'static str,
        c: f64,
        copy: f64,
        pipeline: bool,
        projector: bool,
        first: f64,
        other: f64,
    }

    /// Every split measured (module doc): WP0's 4B and its vision model at
    /// 16,384 tokens (2026-11-16), and the live splits of 2026-11-18 —
    /// A, Qwen3-VL-4B Q8_0 at 262,144 tokens, 4 slots, q8_0 cache; B, the
    /// 4B Q4_K_M likewise; C, Qwen3.6-35B-A3B Q8_0 with `--n-cpu-moe 10`;
    /// D, the 4B's own split at 65,536. `c` and `copy` are lmgw's estimate
    /// for each file's header at those settings (read from the published
    /// headers, 2026-11-18).
    const MEASURED: [Measured; 6] = [
        Measured {
            name: "WP0 4B",
            c: 93.0,
            copy: 0.0,
            pipeline: true,
            projector: false,
            first: 205.0,
            other: 160.0,
        },
        Measured {
            name: "WP0 VL",
            c: 93.0,
            copy: 0.0,
            pipeline: true,
            projector: true,
            first: 302.0,
            other: 160.0,
        },
        Measured {
            name: "A",
            c: 1332.0,
            copy: 0.0,
            pipeline: true,
            projector: true,
            first: 2252.08,
            other: 2110.06,
        },
        Measured {
            name: "B",
            c: 1332.0,
            copy: 0.0,
            pipeline: true,
            projector: false,
            first: 2155.05,
            other: 2110.04,
        },
        Measured {
            name: "C",
            c: 1088.0,
            copy: 272.0,
            pipeline: false,
            projector: true,
            first: 1076.0,
            other: 827.55,
        },
        Measured {
            name: "D",
            c: 372.0,
            copy: 0.0,
            pipeline: true,
            projector: false,
            first: 607.05,
            other: 562.04,
        },
    ];

    /// The model covers every split measured, on every card, by a few tens
    /// of MiB at most (owner, 2026-11-18): never less than llama.cpp
    /// reserved, never more than 64 MiB or 5 % above it. The projector's own
    /// buffer is charged the larger measured (322.49 and 248.10 MiB seen).
    /// One card keeps the one-card figure.
    #[test]
    fn a_split_s_compute_buffers_cover_every_split_measured() {
        let mib = |x: f64| (x * MIB as f64).round() as u64;
        for m in &MEASURED {
            let b = ByLayer {
                each: mib(m.c),
                copy: mib(m.copy),
                pipeline: m.pipeline,
                projector: m.projector,
                ..four()
            };
            assert_eq!(b.compute_on(0, 1), mib(m.c), "{}: one card", m.name);
            let own = if m.projector { PROJECTOR_COMPUTE } else { 0 };
            for (planned, real, which) in [
                (b.compute_on(0, 2) - own, m.first, "first"),
                (b.compute_on(1, 2), m.other, "other"),
                (b.compute_on(2, 3), m.other, "third"),
            ] {
                let planned = planned as f64 / MIB as f64;
                assert!(
                    planned >= real && planned <= real + (real * 0.05).max(64.0),
                    "{} {which}: planned {planned:.1} MiB, measured {real:.1}",
                    m.name
                );
            }
        }
        // The projector's own buffer: the larger of the two measured.
        assert!(PROJECTOR_COMPUTE as f64 / MIB as f64 >= 322.0);
        let b = ByLayer {
            each: 93 * MIB,
            pipeline: true,
            ..four()
        };
        assert_eq!(b.computes(3), b.compute_on(0, 3) + 2 * b.compute_on(1, 3));
        assert_eq!(b.computes(1), 93 * MIB);
        // The cards' parts carry them, one card its footprint.
        let parts = b.on_cards(&[2, 2]).unwrap();
        assert_eq!(parts[0], 20 + b.compute_on(0, 2) + 7);
        assert_eq!(parts[1], 30 + b.compute_on(1, 2) + 5);
        assert_eq!(b.on_cards(&[4]).unwrap(), vec![50 + 93 * MIB + 7 + 5]);
    }

    /// A split's parts by kind (the start's log): each card's layers as
    /// weights, recurrent state and KV cache, the compute buffer what is
    /// left of its bytes, the projector's own apart on the first card, the
    /// first card's own parts and the output layer where they are; the
    /// kinds add up to what the card is charged.
    #[test]
    fn a_split_s_parts_by_kind_add_up_to_each_card() {
        let b = ByLayer {
            each: 10 * MIB,
            weights: vec![6, 6, 12, 6],
            states: vec![1, 1, 1, 1],
            projector: true,
            ..four()
        };
        let bytes = b.on_cards(&[1, 3]).unwrap();
        let k = b.parts_on(&[1, 3], &bytes, 1).unwrap();
        assert_eq!(
            (
                k[0].layers,
                k[0].weights,
                k[0].states,
                k[0].kv,
                k[0].first,
                k[0].output
            ),
            (1, 6, 1, 3, 7, 0)
        );
        assert_eq!(k[0].projector_compute, PROJECTOR_COMPUTE);
        assert_eq!(k[0].compute, b.compute_on(0, 2) - PROJECTOR_COMPUTE);
        assert_eq!(
            (k[1].layers, k[1].weights, k[1].states, k[1].kv, k[1].output),
            (3, 24, 3, 13, 5)
        );
        assert_eq!(k[1].compute, b.compute_on(1, 2));
        for (j, k) in k.iter().enumerate() {
            assert_eq!(
                k.weights + k.kv + k.states + k.compute + k.projector_compute + k.first + k.output,
                bytes[j]
            );
        }
        // Without the layers' kinds (a stated cost): not told.
        assert_eq!(four().parts_on(&[1, 3], &bytes, 1), None);
    }

    #[test]
    fn the_widest_run() {
        let b = four();
        assert_eq!(b.widest(1), 20);
        assert_eq!(b.widest(2), 30);
        assert_eq!(b.widest(4), 50);
        assert_eq!(b.widest(0), 0);
    }
}
