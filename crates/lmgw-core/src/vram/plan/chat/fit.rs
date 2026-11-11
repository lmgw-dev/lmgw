//! What a chat row can be shrunk to where llama.cpp's `--fit` would shrink
//! it (b12244 `common/fit.cpp`), sized by lmgw's own footprint so that what
//! admission charges is what the container is told
//! ([`crate::vram::partial::llama`]).
//!
//! **What `--fit` touches.** With `--fit` on (the default), llama.cpp reads
//! each device's free memory as the driver reports it at load and keeps
//! `--fit-target` free on it (1024 MiB per device by default):
//! 1. an unset context is lowered from the trained one, no lower than
//!    `--fit-ctx` (4096) per sequence stream, rounded down to 256 cells per
//!    stream;
//! 2. while `--n-gpu-layers` is left on auto and the row sets no tensor
//!    override (`-ot`, `--cpu-moe`, `--n-cpu-moe`), layers are moved off
//!    the card — a mixture of experts' routed experts first.
//!
//! A value the row sets is one `--fit` leaves alone, and so does lmgw.
//!
//! **What lmgw does instead.** The driver's free figure includes the
//! headroom and what admission keeps free for others (a model still
//! loading, a pipeline's peak), so `--fit` against it could take more than
//! admission gave. lmgw makes the same choices against what admission gives
//! the start, in the same order — the context first, down to the floor,
//! then the experts layer by layer, then whole layers — and renders them as
//! explicit `--ctx-size`, `--n-cpu-moe` and `--n-gpu-layers` with `--fit off`
//! (`runtime::argv::llama_fit`), so `--fit` has nothing left to shrink and
//! the start skips its dry-run load.

use std::sync::Arc;

use crate::gguf::ModelSummary;
use crate::runtime::argv::{Left, LlamaPlacement};

use super::offload::{Family, Override};
use super::shape::Shape;
use super::{sized, ChatFiles};

/// A row's [`Sizer`] on a footprint, and what admission fitted it to on
/// this card ([`crate::vram::partial::llama::fit_to_card`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaFit {
    pub sizer: Arc<Sizer>,
    /// The most of it the whole card holds beside the headroom: what a
    /// start makes room for. All `None` before it is fitted, and where the
    /// row runs unshrunk.
    pub target: LlamaPlacement,
    /// Its least placement fits the card beside the headroom. A row whose
    /// least does not is too large for the card, as any model is.
    pub fits_card: bool,
}

/// A chat row `--fit` would shrink, and how far (module doc).
pub struct Sizer {
    files: Arc<ChatFiles>,
    path: String,
    shape: Shape,
    /// The context it runs unshrunk: the row's, else the trained one.
    pub trained: Option<u64>,
    /// The repeating layers the header declares.
    pub layers: u64,
    /// It has routed experts (`ffn_*_exps` tensors).
    pub moe: bool,
    /// The context may be lowered (`--fit` on, no context set, a trained
    /// one to lower from).
    pub ctx_adapts: bool,
    /// Layers may go off the card (`--fit` on, `--n-gpu-layers` on auto, no
    /// tensor override).
    pub layers_adapt: bool,
    /// The least context it is lowered to: `--fit-ctx` per stream, never
    /// above the trained one.
    pub min_ctx: u64,
    /// The row's own `--fit-target` in bytes: what it asks to keep free on
    /// the card beside it, which admission keeps where it is more than the
    /// headroom (`partial::llama`), since `--fit off` leaves llama.cpp
    /// nothing to keep it with.
    pub fit_target: Option<u64>,
    /// The cells a lowered context is a multiple of: 256 per stream.
    align: u64,
    /// What each placement asked about so far puts on the card: the
    /// fitting asks the same ones again on every ledger read, reservation
    /// and status poll. Bounded by the placements there are (the contexts
    /// in steps of [`Self::align`] and the layer counts).
    memo: std::sync::Mutex<std::collections::HashMap<LlamaPlacement, super::super::Footprint>>,
}

impl std::fmt::Debug for Sizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sizer")
            .field("path", &self.path)
            .field("trained", &self.trained)
            .field("layers", &self.layers)
            .field("moe", &self.moe)
            .field("ctx_adapts", &self.ctx_adapts)
            .field("layers_adapt", &self.layers_adapt)
            .field("min_ctx", &self.min_ctx)
            .finish()
    }
}

impl PartialEq for Sizer {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.shape == other.shape
    }
}

impl Eq for Sizer {}

impl Sizer {
    /// The sizer of a row of `shape` whose files were read, or `None` where
    /// `--fit` would change nothing: off, or nothing it may touch.
    pub(crate) fn of(files: Arc<ChatFiles>, path: &str, shape: &Shape) -> Option<Self> {
        let s = files.files.summary.as_ref().ok()?;
        let fit = &shape.fit;
        if !fit.on {
            return None;
        }
        let layers = s.block_count.filter(|b| *b > 0)?;
        let trained = shape.ctx.or(s.context_length).filter(|c| *c > 0);
        let streams = if shape.unified { 1 } else { shape.slots.max(1) };
        let ctx_adapts = fit.ctx_adapts() && trained.is_some();
        // `--fit` moves layers only on auto and with no tensor override of
        // the row's (its own `--n-cpu-moe` is one, b12244 step 3).
        let layers_adapt = fit.layers_auto && shape.overrides.is_empty();
        if !ctx_adapts && !layers_adapt {
            return None;
        }
        let min_ctx = fit
            .min_ctx
            .saturating_mul(streams)
            .min(trained.unwrap_or(u64::MAX));
        Some(Self {
            moe: has_experts(s),
            files,
            path: path.to_string(),
            shape: shape.clone(),
            trained,
            layers,
            ctx_adapts,
            layers_adapt,
            min_ctx,
            fit_target: fit.target,
            align: 256 * streams,
            memo: Default::default(),
        })
    }

    /// What `p` puts on the card: the footprint's total with its context,
    /// experts and layers.
    pub fn charge(&self, p: &LlamaPlacement) -> u64 {
        self.footprint(p).total_bytes
    }

    /// The footprint `p` puts on the card ([`Self::charge`] with its terms).
    pub fn footprint(&self, p: &LlamaPlacement) -> super::super::Footprint {
        let hit = self
            .memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(p)
            .cloned();
        if let Some(fp) = hit {
            return fp;
        }
        let fp = self.size(p);
        self.memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*p, fp.clone());
        fp
    }

    /// [`Self::footprint`], sized.
    fn size(&self, p: &LlamaPlacement) -> super::super::Footprint {
        let mut shape = self.shape.clone();
        if let Some(c) = p.ctx {
            shape.ctx = Some(c);
        }
        if let Some(k) = p.n_cpu_moe.filter(|k| *k > 0) {
            shape.overrides.insert(
                0,
                Override::FirstLayers {
                    flag: format!("--n-cpu-moe {k}"),
                    family: Family::Experts,
                    layers: k,
                },
            );
        }
        if let Some(n) = p.n_gpu_layers {
            shape.fit.gpu_layers = Some(n);
            shape.fit.layers_auto = false;
        }
        self.files
            .files
            .flag(sized(&self.files, &self.path, &shape))
    }

    /// The least it can run with: the floor context and, where layers may
    /// go, the output layer alone on the card (every expert in host memory
    /// first).
    pub fn minimum(&self) -> LlamaPlacement {
        LlamaPlacement {
            ctx: self.ctx_adapts.then_some(self.min_ctx),
            n_cpu_moe: (self.layers_adapt && self.moe).then_some(self.layers),
            n_gpu_layers: self.layers_adapt.then_some(1),
        }
    }

    /// The most of it that fits in `avail` bytes, in `--fit`'s order (module
    /// doc); `None` when not even [`Self::minimum`] does. The unshrunk
    /// placement (nothing to render but `--fit off`) is all `None`s.
    pub fn best(&self, avail: u64) -> Option<LlamaPlacement> {
        let whole = LlamaPlacement::default();
        let fits = |p: &LlamaPlacement| self.charge(p) <= avail;
        // 1. The context, every layer on the card.
        if self.ctx_adapts {
            if let Some(c) = self.largest_ctx(avail) {
                // The trained context is the row unshrunk.
                return Some(LlamaPlacement {
                    ctx: (Some(c) != self.trained).then_some(c),
                    ..whole
                });
            }
        } else if fits(&whole) {
            return Some(whole);
        }
        if !self.layers_adapt {
            return None;
        }
        let ctx = self.ctx_adapts.then_some(self.min_ctx);
        // 2. The experts, layer by layer from the first.
        if self.moe {
            let with = |k: u64| LlamaPlacement {
                ctx,
                n_cpu_moe: Some(k),
                n_gpu_layers: None,
            };
            if let Some(k) = smallest(1, self.layers, |k| fits(&with(k))) {
                return Some(with(k));
            }
        }
        // 3. Whole layers, from the back: the most that fit.
        let with = |n: u64| LlamaPlacement {
            ctx,
            n_cpu_moe: self.moe.then_some(self.layers),
            n_gpu_layers: Some(n),
        };
        largest(1, self.layers, |n| fits(&with(n))).map(with)
    }

    /// The largest context in `min_ctx..=trained` with every layer on the
    /// card that fits in `avail`: the trained one, else a multiple of the
    /// alignment.
    fn largest_ctx(&self, avail: u64) -> Option<u64> {
        let trained = self.trained?;
        let at = |c: u64| {
            self.charge(&LlamaPlacement {
                ctx: Some(c),
                ..LlamaPlacement::default()
            }) <= avail
        };
        if at(trained) {
            return Some(trained);
        }
        if !at(self.min_ctx) {
            return None;
        }
        let steps = largest(self.min_ctx / self.align, trained / self.align, |k| {
            at((k * self.align).max(self.min_ctx))
        });
        Some(steps.map_or(self.min_ctx, |k| (k * self.align).max(self.min_ctx)))
    }
}

/// What a row with `params` and `args` leaves to llama.cpp's `--fit`, as
/// [`Sizer::of`] reads it from its shape, without its header: what an
/// adopted container's placement may still be rendered over
/// (`registry::reconcile`).
pub(crate) fn left(params: &crate::config::LlamaParams, args: &[String]) -> Left {
    let shape = Shape::of(params, args);
    Left {
        ctx: shape.fit.ctx_adapts(),
        layers: shape.fit.on && shape.fit.layers_auto && shape.overrides.is_empty(),
    }
}

/// Whether `s` has routed experts.
fn has_experts(s: &ModelSummary) -> bool {
    s.tensor_list
        .tensors
        .iter()
        .any(|(n, _)| super::offload::experts_regex().is_match(n))
        || s.expert_used_count.is_some_and(|n| n > 0)
}

/// The largest `n` in `lo..=hi` with `ok(n)`, for an `ok` that holds up to
/// some point and not after.
fn largest(lo: u64, hi: u64, ok: impl Fn(u64) -> bool) -> Option<u64> {
    if lo > hi || !ok(lo) {
        return None;
    }
    let (mut good, mut bad) = (lo, hi + 1);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if ok(mid) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    Some(good)
}

/// The smallest `n` in `lo..=hi` with `ok(n)`, for an `ok` that fails up to
/// some point and holds after.
fn smallest(lo: u64, hi: u64, ok: impl Fn(u64) -> bool) -> Option<u64> {
    if lo > hi || !ok(hi) {
        return None;
    }
    let (mut bad, mut good) = (lo.checked_sub(1), hi);
    loop {
        let from = bad.map_or(lo, |b| b + 1);
        if from >= good {
            return Some(good);
        }
        let mid = from + (good - from) / 2;
        if ok(mid) {
            good = mid;
        } else {
            bad = Some(mid);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
