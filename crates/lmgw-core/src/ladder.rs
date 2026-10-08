//! Ladder models (design `docs/design/2026-09-27-ladder-models-design.md`
//! §4.1–§4.2): a local chat row can carry a table of **rungs** instead of one
//! fixed weights file. Everything on the row is shared by every rung —
//! template, reasoning, sampling, projector, drafter, cache types, `parallel`,
//! image and run args — and each rung only ever differs in its GGUF and its
//! `-c` (context size).
//!
//! **The base rung is not stored here.** It is the row's own `gguf_path` +
//! `params.ctx_size`, unchanged, so every existing reader of those two fields
//! (inspect, plan, test, argv) keeps working for it without special-casing.
//! [`Rung`] — and [`crate::config::LocalModel::ladder`] — hold only the
//! *higher* rungs; an empty `ladder` means "not a ladder" (§4.1).
//!
//! **Numbering.** Everything in this module is 0-indexed (`ladder[0]` is the
//! second rung), matching the rest of the code. Rung *numbers* on any
//! external surface — headers, `request_logs.rung`, the dashboard, MCP — are
//! 1-based (design §12 entry 11); converting between the two is each of those
//! surfaces' own job, not this module's.

use serde::{Deserialize, Serialize};

use crate::config::LocalModel;

/// One rung above the base: its own weights and its own `-c`.
/// `ctx_size` is a real token count: a rung with no context at all is not a
/// rung, so saving one with a non-positive value is refused.
// Design §4.1; §4.3 rule 3 is the refusal. `per_slot_ctx`/`switchover` below
// already return `None` for a stored row that somehow has a non-positive
// value (e.g. one written before that rule existed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rung {
    /// GGUF path relative to the models dir, exactly like the base row's
    /// `gguf_path`.
    pub gguf_path: String,
    /// `--ctx-size` for this rung — the same meaning `ctx_size` has
    /// everywhere else: it is `-c`, never the per-slot number.
    pub ctx_size: i64,
}

/// One rung as the derived views need it, base included — index 0 is always
/// the base, so a caller never special-cases "not a ladder": a plain row's
/// [`LocalModel::all_rungs`] is exactly one [`RungView`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RungView<'m> {
    /// 0-indexed; 0 is the base.
    pub index: usize,
    pub gguf_path: &'m str,
    pub ctx_size: i64,
}

impl LocalModel {
    /// Whether this row is a ladder at all (design §4.1): any higher rungs
    /// configured. An empty `ladder` is "not a ladder", not "a ladder of one
    /// rung" — the base alone is just a normal row.
    pub fn is_ladder(&self) -> bool {
        !self.ladder.is_empty()
    }

    /// The top rung's 0-indexed number — `ladder.len()`. `0` when this is not
    /// a ladder, i.e. the base is the only (and therefore top) rung.
    pub fn top_rung(&self) -> usize {
        self.ladder.len()
    }

    /// Every rung, base first, base included (design §4.1). `None` when the
    /// base `ctx_size` is unset or not a positive token count — deriving a
    /// per-slot context from no context at all is meaningless, and §4.3 rule
    /// 3 refuses saving a ladder in that state, so this can only be reached
    /// on a row that predates that rule or has not been validated yet.
    pub fn all_rungs(&self) -> Option<Vec<RungView<'_>>> {
        let base_ctx = self.params.ctx_size.filter(|&c| c > 0)?;
        let mut out = Vec::with_capacity(self.ladder.len() + 1);
        out.push(RungView {
            index: 0,
            gguf_path: &self.gguf_path,
            ctx_size: base_ctx,
        });
        out.extend(self.ladder.iter().enumerate().map(|(i, r)| RungView {
            index: i + 1,
            gguf_path: &r.gguf_path,
            ctx_size: r.ctx_size,
        }));
        Some(out)
    }

    /// Per-slot context of rung `index` (0 = base) as configured: `ctx_size /
    /// parallel` (design §4.2, [`crate::config::LlamaParams::per_request_ctx`]'s
    /// split-row formula). What a slot really holds is this capped at the
    /// rung's trained context ([`slot_ctx`]), which needs the GGUF header;
    /// §4.3's validation refuses a row where the two differ. `None` when
    /// `index` is out of range or the base has no usable `ctx_size` (see
    /// [`Self::all_rungs`]).
    pub fn per_slot_ctx(&self, index: usize) -> Option<i64> {
        let rung = self.all_rungs()?.into_iter().find(|r| r.index == index)?;
        Some(rung.ctx_size / self.params.effective_slots().max(1))
    }

    /// Switchover: the largest prompt this rung takes with the full max
    /// output (design §4.2) — `per_slot_ctx - n_predict`. `None` when
    /// `n_predict` is unset or non-positive (meaningless without a max-output
    /// ceiling — the same thing §4.3 rule 1 refuses to save a ladder
    /// without), or when [`Self::per_slot_ctx`] is `None`.
    pub fn switchover(&self, index: usize) -> Option<i64> {
        let per_slot = self.per_slot_ctx(index)?;
        let n_predict = self.params.n_predict.filter(|&n| n > 0)?;
        Some(per_slot - n_predict)
    }
}

/// The context a slot really holds: `per_slot` (`ctx_size / parallel`),
/// capped at the weights' trained context — llama-server itself caps every
/// slot there ("capping" in `server-context.cpp`, split and unified slots
/// alike), so a rung configured above it runs, and fits requests, at the
/// trained context. An unknown (or non-positive) trained context caps
/// nothing.
///
/// Not a limit lmgw invents: it is the number the server enforces, used
/// wherever lmgw judges or publishes a ladder rung, so the judgement and the
/// published context match what llama-server will do. §4.3's validation
/// refuses a rung that would be capped, with both numbers, so a row saved
/// since never differs; one saved before is judged on the real slot.
pub fn slot_ctx(per_slot: i64, trained: Option<i64>) -> i64 {
    match trained.filter(|&t| t > 0) {
        Some(t) => per_slot.min(t),
        None => per_slot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlamaParams;

    #[test]
    fn a_slot_is_capped_at_the_trained_context_and_only_then() {
        assert_eq!(slot_ctx(262_144, Some(131_072)), 131_072);
        assert_eq!(slot_ctx(65_536, Some(131_072)), 65_536);
        assert_eq!(slot_ctx(65_536, None), 65_536, "unknown caps nothing");
        assert_eq!(slot_ctx(65_536, Some(0)), 65_536);
    }

    fn row(ctx_size: Option<i64>, parallel: Option<i64>, n_predict: Option<i64>) -> LocalModel {
        LocalModel {
            id: 1,
            model_id: "m".into(),
            gguf_path: "base.gguf".into(),
            params: LlamaParams {
                ctx_size,
                parallel,
                n_predict,
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        }
    }

    #[test]
    fn empty_ladder_is_not_a_ladder() {
        let m = row(Some(4096), Some(2), Some(512));
        assert!(!m.is_ladder());
        assert_eq!(m.top_rung(), 0);
        let rungs = m.all_rungs().unwrap();
        assert_eq!(rungs.len(), 1);
        assert_eq!(rungs[0].gguf_path, "base.gguf");
        assert_eq!(rungs[0].ctx_size, 4096);
    }

    #[test]
    fn all_rungs_puts_the_base_first_and_keeps_ladder_order() {
        let mut m = row(Some(4096), Some(2), Some(512));
        m.ladder = vec![
            Rung {
                gguf_path: "mid.gguf".into(),
                ctx_size: 32768,
            },
            Rung {
                gguf_path: "top.gguf".into(),
                ctx_size: 131072,
            },
        ];
        let rungs = m.all_rungs().unwrap();
        assert_eq!(rungs.len(), 3);
        assert_eq!(
            rungs.iter().map(|r| r.gguf_path).collect::<Vec<_>>(),
            vec!["base.gguf", "mid.gguf", "top.gguf"]
        );
        assert_eq!(
            rungs.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(m.is_ladder());
        assert_eq!(m.top_rung(), 2);
    }

    #[test]
    fn per_slot_ctx_divides_by_effective_slots() {
        let mut m = row(Some(4096), Some(2), Some(512));
        m.ladder = vec![Rung {
            gguf_path: "top.gguf".into(),
            ctx_size: 32768,
        }];
        assert_eq!(m.per_slot_ctx(0), Some(2048));
        assert_eq!(m.per_slot_ctx(1), Some(16384));
        assert_eq!(m.per_slot_ctx(2), None, "no such rung");
    }

    #[test]
    fn switchover_is_per_slot_minus_max_output() {
        let mut m = row(Some(4096), Some(2), Some(512));
        m.ladder = vec![Rung {
            gguf_path: "top.gguf".into(),
            ctx_size: 32768,
        }];
        assert_eq!(m.switchover(0), Some(2048 - 512));
        assert_eq!(m.switchover(1), Some(16384 - 512));
    }

    #[test]
    fn no_max_output_means_no_switchover() {
        let m = row(Some(4096), Some(2), None);
        assert_eq!(m.switchover(0), None);
    }

    #[test]
    fn unset_base_ctx_means_no_rungs_at_all() {
        let m = row(None, Some(2), Some(512));
        assert_eq!(m.all_rungs(), None);
        assert_eq!(m.per_slot_ctx(0), None);
    }
}
