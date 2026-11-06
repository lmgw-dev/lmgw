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
//!
//! The footprint at any context comes from [`Sizer`], over the plan's one
//! input ([`PlanFiles`], find-a-model design WP2), which read every file
//! before: nothing here touches the disk. A fit on the card
//! ([`super::fit`]) searches the same footprints with [`largest_fitting`].

use serde_json::{Map, Value};

use crate::config::Snapshot;
use crate::hf::fmt_bytes;
use crate::state::SharedState;
use crate::vram::plan::chat::{ChatFiles, Second, Shape};
use crate::vram::plan::GgufFiles;
use crate::vram::Footprint;

use super::plan_files::PlanFiles;

/// What a plan is checked against: the capacity admission refuses beyond
/// (`vram.budget_mb`, else the devices' total) and the headroom it adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Card {
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

    pub fn fits(&self, fp: &Footprint) -> bool {
        fp.total_bytes.saturating_add(self.headroom) <= self.capacity
    }
}

/// The footprint a planned row is charged at any context, from the plan's
/// input and the plan's own parameters (find-a-model design §3.1): what the
/// plan's warning and a fit both search over, so they never disagree about
/// a number. Pure: every file was read into the [`PlanFiles`] before.
pub struct Sizer {
    kind: SizerKind,
    rel: String,
    /// The context the plan sets (`ctx_size`, else the trained one); `None`
    /// when neither is known.
    pub ctx: Option<u64>,
    /// What else can be done, besides a smaller context.
    pub instead: &'static str,
    /// The row is planned on the CPU: nothing on the card.
    pub on_cpu: bool,
}

enum SizerKind {
    Chat {
        files: Box<ChatFiles>,
        shape: Shape,
    },
    Aux {
        files: Box<GgufFiles>,
        ubatch: Option<i64>,
        batch: Option<i64>,
        tied: bool,
    },
}

impl Sizer {
    /// A chat plan's sizer: the planned row's weights, projector, drafter,
    /// KV cache and compute buffers at the planned slots, batch sizes, cache
    /// types and speculative decoding, as admission charges a chat row
    /// ([`crate::vram::plan::chat`]).
    pub fn chat(files: &PlanFiles, params: &Map<String, Value>) -> Self {
        use crate::config::LlamaParams;

        let text = |k: &str| params.get(k).and_then(Value::as_str);
        let on_cpu = params.get("n_gpu_layers").and_then(Value::as_i64) == Some(0);
        let ctx = params
            .get("ctx_size")
            .and_then(Value::as_u64)
            .or(files.summary.context_length);
        let companions: Vec<&str> = ["mmproj_path", "draft_gguf_path"]
            .into_iter()
            .filter_map(text)
            .collect();
        // The plan's parameters are a row's typed fields by name.
        let planned: LlamaParams =
            serde_json::from_value(Value::Object(params.clone())).unwrap_or_default();
        let shape = Shape::of(&planned, &[]);
        let draft = match &shape.second {
            Second::Draft { path, .. } => Some(
                files
                    .companion(path)
                    .map(|c| c.summary.clone())
                    .ok_or_else(|| format!("{path} is not a drafter beside the weights")),
            ),
            _ => None,
        };
        Self {
            kind: SizerKind::Chat {
                files: Box::new(ChatFiles {
                    files: files.gguf_files(&companions),
                    draft,
                }),
                shape,
            },
            rel: files.rel.clone(),
            ctx,
            instead: "run it on the CPU (n_gpu_layers 0)",
            on_cpu,
        }
    }

    /// An aux plan's sizer: the planned row's footprint on the GPU as
    /// admission charges it ([`crate::vram::plan::aux`]). A plan that ties
    /// the ubatch to the context (a whole input in one ubatch) keeps the tie
    /// at every context.
    pub fn aux(files: &PlanFiles, params: &Map<String, Value>) -> Self {
        let int = |k: &str| params.get(k).and_then(Value::as_i64);
        let on_cpu = params.get("backend").and_then(Value::as_str) == Some("cpu");
        let ctx = int("ctx_size")
            .and_then(|c| u64::try_from(c).ok())
            .or(files.summary.context_length);
        let companions: Vec<&str> = params
            .get("mmproj_path")
            .and_then(Value::as_str)
            .into_iter()
            .collect();
        let (ubatch, batch) = (int("ubatch_size"), int("batch_size"));
        Self {
            kind: SizerKind::Aux {
                files: Box::new(files.gguf_files(&companions)),
                ubatch,
                batch,
                tied: ubatch.is_some() && ubatch == int("ctx_size"),
            },
            rel: files.rel.clone(),
            ctx,
            instead: "run it on the CPU (backend=cpu), where it takes no VRAM",
            on_cpu,
        }
    }

    /// The footprint at `c` tokens of context.
    pub fn at(&self, c: u64) -> Footprint {
        self.sized(Some(c))
    }

    /// The footprint at the planned context, none included.
    pub fn at_planned(&self) -> Footprint {
        self.sized(self.ctx)
    }

    fn sized(&self, c: Option<u64>) -> Footprint {
        match &self.kind {
            SizerKind::Chat { files, shape } => {
                let at = Shape {
                    ctx: c,
                    ..shape.clone()
                };
                crate::vram::plan::chat::footprint(files, &self.rel, &at)
            }
            SizerKind::Aux {
                files,
                ubatch,
                batch,
                tied,
            } => {
                use crate::runtime::aux::{LLAMA_DEFAULT_BATCH, LLAMA_DEFAULT_UBATCH};
                use crate::vram::plan::aux::{footprint, Batches, Placement};
                let count = |n: Option<i64>, default: i64| n.unwrap_or(default).unsigned_abs();
                let tied_to = c.filter(|_| *tied);
                let batches = Batches {
                    ubatch: tied_to.unwrap_or_else(|| count(*ubatch, LLAMA_DEFAULT_UBATCH)),
                    batch: tied_to.unwrap_or_else(|| count(*batch, LLAMA_DEFAULT_BATCH)),
                    flash_attn_off: false,
                };
                // The plan renders no cache type, so the KV cache is f16.
                let ctx = c.map(|c| i64::try_from(c).unwrap_or(i64::MAX));
                footprint(files, &self.rel, ctx, (16, 16), Placement::Gpu, batches)
            }
        }
    }
}

/// Finding 3 for a plan: the warning when the planned row doesn't fit the
/// card, with the largest context that does. `None` for a row planned on
/// the CPU, without a card, without a context, and when it fits.
pub(super) async fn warning(state: &SharedState, snap: &Snapshot, sizer: &Sizer) -> Option<String> {
    if sizer.on_cpu {
        return None;
    }
    let card = Card::of(state, snap).await?;
    let ctx = sizer.ctx?;
    over_card(&card, ctx, sizer.instead, |c| sizer.at(c))
}

/// What the search over the contexts `1..=ctx` found ([`largest_fitting`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Search {
    /// The whole `ctx` fits.
    All,
    /// It doesn't, and this is the largest that does.
    Upto(u64),
    /// Not even one token fits.
    None,
}

/// The largest context in `1..=ctx` whose footprint `at` fits `card` (a
/// context of 0 is read as 1). `at` grows with the context (KV cache,
/// attention mask) and never shrinks, so a bisection finds it.
pub fn largest_fitting(card: &Card, ctx: u64, at: impl Fn(u64) -> Footprint) -> Search {
    let ctx = ctx.max(1);
    if card.fits(&at(ctx)) {
        return Search::All;
    }
    if !card.fits(&at(1)) {
        return Search::None;
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
    Search::Upto(lo)
}

/// The warning for a GPU row planned at `ctx` whose footprint `at(ctx)` plus
/// the headroom exceeds `card`; `None` when it fits. The largest context
/// that fits is searched over `1..ctx` through `at` ([`largest_fitting`]).
/// `instead` names what else can be done, besides a smaller context.
pub(super) fn over_card(
    card: &Card,
    ctx: u64,
    instead: &str,
    at: impl Fn(u64) -> Footprint,
) -> Option<String> {
    let ctx = ctx.max(1);
    let lo = match largest_fitting(card, ctx, &at) {
        Search::All => return None,
        Search::None => None,
        Search::Upto(n) => Some(n),
    };
    let planned = at(ctx);
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
        format!(
            "the declared GPU memory budget, {} ({})",
            fmt_bytes(card.capacity),
            crate::config::BUDGET_LABEL
        )
    } else {
        format!("the GPU's total memory ({})", fmt_bytes(card.capacity))
    };
    let outcome = if card.admission {
        "admission will refuse to load it (\"no eviction can make it fit\")".to_string()
    } else {
        format!(
            "admission is off ({}), so nothing refuses the start, and llama-server tries to \
             load it into memory the card does not have",
            crate::config::ADMISSION_LABEL
        )
    };
    let head = format!(
        "on the GPU at ctx_size {ctx}, this row's estimated footprint is {} ({}) plus the {} \
         headroom, more than {of}: {outcome}",
        fmt_bytes(planned.total_bytes),
        terms(&planned),
        fmt_bytes(card.headroom),
    );
    let Some(lo) = lo else {
        let smallest = at(1);
        return Some(format!(
            "{head}. No ctx_size fits: at 1 token it is still {} ({}). The plan keeps the \
             trained context; {instead}",
            fmt_bytes(smallest.total_bytes),
            terms(&smallest)
        ));
    };
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
        assert!(w.contains("compute buffer 512 MiB"), "{w}");
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
            w.contains("No ctx_size fits") && w.contains("GPU memory budget"),
            "{w}"
        );
        assert!(
            !w.contains("compute buffer"),
            "no compute term to name: {w}"
        );
    }
}
