//! A plan that cannot fit the card says so (owner finding 3, 2026-10-13).
//!
//! An embedding row planned at its trained context of 262144 can need more
//! GPU memory than the card has, and admission then refuses it at load ("no
//! eviction can make it fit"), with no hint from the plan. Now a plan for a
//! GPU row checks the footprint admission will charge it ([`crate::vram::plan`], the same
//! estimate) plus `vram.headroom_mb` against what lmgw may use on the GPU,
//! and when it does not fit names the estimate, the capacity and the largest
//! `ctx_size` that fits. It never lowers the context itself: the trained
//! context stays the plan, and the overrun surfaces as this warning.
//!
//! Skipped when admission has nothing to measure against (no GPU telemetry
//! and no `vram.budget_mb`) and for a row planned on the CPU.

use serde_json::{Map, Value};

use crate::config::Snapshot;
use crate::gguf::ModelSummary;
use crate::hf::fmt_bytes;
use crate::state::SharedState;
use crate::vram::Footprint;

/// What a plan is checked against: the capacity admission refuses beyond
/// (`vram.budget_mb`, else the devices' total) and the headroom it adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Card {
    pub capacity: u64,
    pub headroom: u64,
    /// The capacity is `vram.budget_mb`, not the devices' total.
    pub budget: bool,
    /// Admission is on (`vram.enabled`) and refuses a model this large. With
    /// it off the ledger only reports, nothing refuses the start, and the
    /// warning says so instead (review finding 4).
    pub admission: bool,
}

impl Card {
    /// The card of this install, or `None` while admission has nothing to
    /// measure against.
    pub(super) async fn of(state: &SharedState, snap: &Snapshot) -> Option<Self> {
        let g = state.vram.group_capacity(state, snap).await?;
        Some(Self {
            capacity: g.capacity,
            headroom: g.headroom,
            budget: snap.settings.vram.budget_mb > 0,
            admission: snap.settings.vram.enabled,
        })
    }

    fn fits(&self, fp: &Footprint) -> bool {
        fp.total_bytes.saturating_add(self.headroom) <= self.capacity
    }
}

/// Finding 3 for a chat plan: the planned row's weights, projector, drafter,
/// KV cache and compute buffers at the planned slots, batch sizes, cache
/// types and speculative decoding, as admission charges a chat row
/// ([`crate::vram::plan::chat`]), against the card. `None` without a card,
/// without a context, and when it fits.
pub(super) async fn chat(
    state: &SharedState,
    snap: &Snapshot,
    dir: &str,
    rel: &str,
    s: &ModelSummary,
    params: &Map<String, Value>,
) -> Option<String> {
    use crate::config::LlamaParams;
    use crate::vram::plan::chat::{footprint, ChatFiles, Second, Shape};
    use crate::vram::plan::GgufFiles;

    let text = |k: &str| params.get(k).and_then(Value::as_str);
    if params.get("n_gpu_layers").and_then(Value::as_i64) == Some(0) {
        return None;
    }
    let card = Card::of(state, snap).await?;
    let ctx = params
        .get("ctx_size")
        .and_then(Value::as_u64)
        .or(s.context_length)?;
    let mut companion_bytes = 0u64;
    for p in ["mmproj_path", "draft_gguf_path"]
        .into_iter()
        .filter_map(text)
    {
        let len = tokio::fs::metadata(std::path::Path::new(dir).join(p))
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        companion_bytes = companion_bytes.saturating_add(len);
    }
    // The plan's parameters are a row's typed fields by name.
    let planned: LlamaParams =
        serde_json::from_value(Value::Object(params.clone())).unwrap_or_default();
    let shape = Shape::of(&planned, &[]);
    let draft = match &shape.second {
        Second::Draft { path, .. } => {
            let path = std::path::Path::new(dir).join(path);
            Some(
                tokio::task::spawn_blocking(move || {
                    crate::gguf::summarize(&path).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
            )
        }
        _ => None,
    };
    let files = ChatFiles {
        files: GgufFiles {
            main_bytes: s.file_size,
            companion_bytes,
            summary: Ok(s.clone()),
        },
        draft,
    };
    over_card(&card, ctx, "run it on the CPU (n_gpu_layers 0)", |c| {
        let at = Shape {
            ctx: Some(c),
            ..shape.clone()
        };
        footprint(&files, rel, &at)
    })
}

/// The warning for a GPU row planned at `ctx` whose footprint `at(ctx)` plus
/// the headroom exceeds `card`; `None` when it fits. The largest context
/// that fits is searched over `1..ctx` through `at`, which grows with the
/// context (KV cache, attention mask) and never shrinks. `instead` names
/// what else can be done, besides a smaller context.
pub(super) fn over_card(
    card: &Card,
    ctx: u64,
    instead: &str,
    at: impl Fn(u64) -> Footprint,
) -> Option<String> {
    let ctx = ctx.max(1);
    let planned = at(ctx);
    if card.fits(&planned) {
        return None;
    }
    let terms = |fp: &Footprint| {
        let mut t = format!(
            "weights {}, KV cache {}",
            fmt_bytes(fp.weights_bytes),
            fmt_bytes(fp.kv_cache_bytes)
        );
        if fp.compute_bytes > 0 {
            t.push_str(&format!(", compute buffer {}", fmt_bytes(fp.compute_bytes)));
        }
        t
    };
    let of = if card.budget {
        "vram.budget_mb"
    } else {
        "the GPU's total memory"
    };
    let outcome = if card.admission {
        "admission will refuse to load it (\"no eviction can make it fit\")"
    } else {
        "admission is off (vram.enabled), so nothing refuses the start, and llama-server tries \
         to load it into memory the card does not have"
    };
    let head = format!(
        "on the GPU at ctx_size {ctx}, this row's estimated footprint is {} ({}) plus the {} \
         headroom, more than {of} ({}): {outcome}",
        fmt_bytes(planned.total_bytes),
        terms(&planned),
        fmt_bytes(card.headroom),
        fmt_bytes(card.capacity)
    );
    let smallest = at(1);
    if !card.fits(&smallest) {
        return Some(format!(
            "{head}. No ctx_size fits: at 1 token it is still {} ({}). The plan keeps the \
             trained context; {instead}",
            fmt_bytes(smallest.total_bytes),
            terms(&smallest)
        ));
    }
    // The largest n in 1..ctx with at(n) fitting: at(1) fits, at(ctx) does not.
    let (mut lo, mut hi) = (1u64, ctx);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if card.fits(&at(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(format!(
        "{head}. The largest ctx_size that fits is {lo} ({} there). The plan keeps the trained \
         context, so the overrun stays visible: lower ctx_size to at most {lo} yourself, or \
         {instead}",
        fmt_bytes(at(lo).total_bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// A row of 4 GiB of weights and 144 KiB of KV per token, as
    /// Qwen3-VL-Embedding-8B's f16 cache is.
    fn vl8b(ctx: u64) -> Footprint {
        Footprint::new(5 * GIB, ctx * 147_456, Some(ctx), None).with_compute(GIB / 2)
    }

    fn card() -> Card {
        Card {
            capacity: 24 * GIB,
            headroom: GIB,
            budget: false,
            admission: true,
        }
    }

    /// Review finding 4: with admission off nothing refuses the start, and
    /// the warning says what happens instead.
    #[test]
    fn with_admission_off_the_warning_does_not_promise_a_refusal() {
        let off = Card {
            admission: false,
            ..card()
        };
        let w = over_card(&off, 262_144, "set backend=cpu", vl8b).unwrap();
        assert!(
            w.contains("admission is off") && !w.contains("will refuse"),
            "{w}"
        );
        assert!(w.contains("largest ctx_size that fits"), "{w}");
        let on = over_card(&card(), 262_144, "set backend=cpu", vl8b).unwrap();
        assert!(on.contains("will refuse"), "{on}");
    }

    /// A context of 0 (a header that states one) is searched from 1.
    #[test]
    fn a_zero_context_is_read_as_one() {
        let huge = |ctx| Footprint::new(30 * GIB, ctx * 1024, Some(ctx), None);
        let w = over_card(&card(), 0, "set backend=cpu", huge).unwrap();
        assert!(
            w.contains("ctx_size 1,") || w.contains("ctx_size 1 "),
            "{w}"
        );
        assert!(over_card(&card(), 0, "x", vl8b).is_none());
    }

    #[test]
    fn a_row_that_fits_says_nothing() {
        assert_eq!(over_card(&card(), 32_768, "set backend=cpu", vl8b), None);
    }

    /// A plan refused at the trained context, with the estimate,
    /// the capacity and the largest context that fits named, and the plan's
    /// context left as it is.
    #[test]
    fn a_row_over_the_card_names_the_largest_context_that_fits() {
        let w = over_card(&card(), 262_144, "set backend=cpu", vl8b).unwrap();
        assert!(w.contains("ctx_size 262144"), "{w}");
        assert!(w.contains("41.5 GiB"), "the estimate: {w}");
        assert!(w.contains("compute buffer 512.0 MiB"), "{w}");
        assert!(
            w.contains("24.0 GiB") && w.contains("1.0 GiB headroom"),
            "{w}"
        );
        assert!(w.contains("the GPU's total memory"), "{w}");
        // 24 − 1 − 5.5 GiB of room for 144 KiB per token.
        let room = 35 * GIB / 2;
        let n = room / 147_456;
        assert!(
            w.contains(&format!("largest ctx_size that fits is {n} ")),
            "{w}"
        );
        assert!(card().fits(&vl8b(n)) && !card().fits(&vl8b(n + 1)));
        assert!(w.contains("set backend=cpu"), "{w}");
    }

    #[test]
    fn weights_too_large_for_any_context_are_said_so() {
        let huge = |ctx| Footprint::new(30 * GIB, ctx * 1024, Some(ctx), None);
        let budget = Card {
            budget: true,
            ..card()
        };
        let w = over_card(&budget, 8192, "offload fewer layers", huge).unwrap();
        assert!(
            w.contains("No ctx_size fits") && w.contains("vram.budget_mb"),
            "{w}"
        );
        assert!(
            !w.contains("compute buffer"),
            "no compute term to name: {w}"
        );
    }
}
