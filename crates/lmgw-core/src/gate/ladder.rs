//! The request path's view of a ladder (ladder design §3.1–3.4, §6): which
//! rung a send was judged on, what the response says about it, and which rung
//! a request that does not fit has to climb to.
//!
//! The climb itself — mark, drain, replacement admission, start — is
//! [`crate::vram::climb`]'s, and so is the permission hook phase 4 plugs into
//! (§12 entry 24). This module only answers the arithmetic questions the send
//! helper ([`super::send`]) asks before it calls it.
//!
//! Rungs are 0-based here, like everywhere in code; every string this module
//! builds for the outside is 1-based (§12 entry 11).

use axum::http::header::HeaderValue;

use super::facts::GateFacts;
use crate::config::LocalModel;
use crate::error::GatewayError;
use crate::state::SharedState;

/// The response header naming the rung a ladder row answered from (ladder
/// design §6): `x-lmgw-rung: <k>/<n>; ctx=<per-slot>; gguf=<file name>`.
pub const RUNG_HEADER: &str = "x-lmgw-rung";

/// The rung one send was judged on — and, once the send is handed over, the
/// rung that served it. What `x-lmgw-rung` says and `request_logs.rung`
/// records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RungTag {
    /// 0-based, like [`crate::runtime::descriptor::RungPos::index`].
    pub index: usize,
    /// How many rungs the ladder the container was started from has.
    pub of: usize,
    /// The rung's per-slot context — the number a request's prompt plus max
    /// output has to fit (§3.3 step 4).
    pub per_slot: u64,
    /// The weights' file name.
    pub gguf_file: String,
}

impl RungTag {
    /// The rung a container runs, from its start facts. `None` for a
    /// container without a ladder, or one whose per-slot context is unknown
    /// (a ladder row always has one — §4.3 rule 3 — and
    /// [`ladder_ready`] refuses a start that somehow does not).
    pub fn of(facts: &GateFacts) -> Option<Self> {
        let pos = facts.rung?;
        Some(Self {
            index: pos.index,
            of: pos.of,
            per_slot: facts.per_request_ctx()?,
            gguf_file: facts.gguf_file().to_string(),
        })
    }

    /// The `request_logs.rung` value: 1-based.
    pub fn log(&self) -> i64 {
        i64::try_from(self.index + 1).unwrap_or(i64::MAX)
    }

    /// The header's value. The `gguf` part is left off when the file name
    /// is not plain ASCII or not a valid header value — the same call
    /// `GateHeaders::stamp` makes for a fallback alias: losing a detail of
    /// the annotation is not worth losing the response.
    pub fn header_value(&self) -> HeaderValue {
        let head = format!("{}/{}; ctx={}", self.index + 1, self.of, self.per_slot);
        let full = self
            .gguf_file
            .is_ascii()
            .then(|| HeaderValue::from_str(&format!("{head}; gguf={}", self.gguf_file)).ok())
            .flatten();
        full.unwrap_or_else(|| {
            HeaderValue::from_str(&head).expect("digits, slashes and 'ctx=' are a valid header")
        })
    }
}

/// Whether a ladder start can be gated at all, and its max-output ceiling if
/// so (§3.2: a ladder's `n_predict` is mandatory).
///
/// Every refusal here is a row that should never have been saved — §4.3 rules
/// 1–3 refuse it — reaching the request path anyway (a row written before the
/// validation existed, a freeform flag that hoisted the KV cache into one
/// pool). It is a 500 naming the fix, never a quiet send: the one condition
/// that matters most, a unified KV cache, is what makes the count beside the
/// send safe (§12 entry 7) — an over-long send on a shared pool aborts every
/// slot on the model, where on split slots it can only truncate its own.
pub(crate) fn ladder_ready(facts: &GateFacts) -> Result<i64, GatewayError> {
    let refused = |why: &str| {
        Err(GatewayError::Internal(format!(
            "'{}' runs a ladder, but {why} — fix the row, save it, and restart the model",
            facts.model_id
        )))
    };
    if facts.params.effective_kv_unified() {
        return refused(
            "its container shares one KV cache across its slots (kv_unified on, or parallel \
             left on auto), and a ladder needs split slots (ladder design §4.3 rule 2): set \
             parallel to an explicit slot count with kv_unified off",
        );
    }
    let Some(n_predict) = facts.n_predict().filter(|&n| n > 0) else {
        return refused(
            "it has no max output, and every request on a ladder is clamped to one (§4.3 rule \
             1): set n_predict",
        );
    };
    if facts.per_request_ctx().is_none() {
        return refused("its container was started without a context size: set ctx_size");
    }
    Ok(n_predict)
}

/// The rung a request that needs `need` tokens per slot (prompt + max output)
/// climbs to: the smallest rung of `row` **above** `running` whose per-slot
/// context holds it (§3.1 "climbs directly to the smallest rung that fits").
///
/// Only upward: a climb never picks the running rung or one below it, even
/// when the row's arithmetic says it would fit — a backstop means
/// llama-server itself refused on the running rung, and that answer wins over
/// the arithmetic (§3.3). The rungs come from the row as it is now, because a
/// climb is a start and every start renders the current row (§4.1).
///
/// Each rung is judged on the slot llama-server really gives it: its per-slot
/// context capped at its GGUF's trained context (`trained`, base first,
/// [`trained_contexts`]; [`crate::ladder::slot_ctx`]) — the number the
/// running rung's own facts carry once it is up.
///
/// `Err` is the top rung, when none holds it: the request is refused with
/// `context_length_exceeded` against that rung's per-slot context (§3.1).
pub(crate) fn target_rung(
    row: &LocalModel,
    trained: &[Option<i64>],
    running: usize,
    need: u64,
) -> Result<usize, TopRung> {
    let top = row.top_rung();
    let per_slot = |i: usize| {
        row.per_slot_ctx(i)
            .map(|c| crate::ladder::slot_ctx(c, trained.get(i).copied().flatten()))
            .and_then(|c| u64::try_from(c).ok())
            .unwrap_or(0)
    };
    ((running + 1)..=top)
        .find(|&i| per_slot(i) >= need)
        .ok_or_else(|| TopRung {
            index: top,
            of: top + 1,
            per_slot: per_slot(top),
            gguf_file: row
                .all_rungs()
                .and_then(|r| r.last().map(|r| r.gguf_path.to_string()))
                .map(|p| crate::runtime::descriptor::file_name(&p).to_string())
                .unwrap_or_default(),
        })
}

/// Every rung's trained context (`<arch>.context_length`), base first, for
/// [`target_rung`]. Read through the GGUF summary cache — a climb re-reads a
/// header only when its file changed on disk — and `None` where a header
/// cannot be read or does not say, which caps nothing.
pub(crate) async fn trained_contexts(
    state: &SharedState,
    models_dir: &str,
    row: &LocalModel,
) -> Vec<Option<i64>> {
    let mut out = Vec::new();
    for rung in row.all_rungs().unwrap_or_default() {
        out.push(
            crate::capabilities::exposed::trained_context(state, models_dir, rung.gguf_path).await,
        );
    }
    out
}

/// The top rung of a ladder no rung of which holds a request — what its
/// `context_length_exceeded` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TopRung {
    pub index: usize,
    pub of: usize,
    pub per_slot: u64,
    pub gguf_file: String,
}

impl TopRung {
    /// `k/n (<gguf>)`, 1-based — how the climb's own messages name a rung.
    pub fn label(&self) -> String {
        format!("{}/{} ({})", self.index + 1, self.of, self.gguf_file)
    }
}

/// Why a climb is running, as the status surfaces show it (§6):
/// `prompt 41,210 + 8,192 > 30,000`.
pub(crate) fn climb_reason(prompt: u64, max_output: u64, per_slot: u64) -> String {
    format!(
        "prompt {} + {} > {}",
        grouped(prompt),
        grouped(max_output),
        grouped(per_slot)
    )
}

/// `41210` → `41,210`: token counts in a sentence a person reads.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlamaParams;
    use crate::ladder::Rung;

    fn ladder() -> LocalModel {
        LocalModel {
            id: 1,
            model_id: "m".into(),
            gguf_path: "sub/base.gguf".into(),
            params: LlamaParams {
                ctx_size: Some(64),
                parallel: Some(1),
                n_predict: Some(16),
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
            ladder: vec![
                Rung {
                    gguf_path: "mid.gguf".into(),
                    ctx_size: 128,
                },
                Rung {
                    gguf_path: "sub/top.gguf".into(),
                    ctx_size: 512,
                },
            ],
        }
    }

    #[test]
    fn the_target_is_the_smallest_rung_above_that_holds_the_request() {
        let row = ladder();
        assert_eq!(target_rung(&row, &[], 0, 65), Ok(1));
        assert_eq!(
            target_rung(&row, &[], 0, 129),
            Ok(2),
            "straight past rung 2"
        );
        assert_eq!(target_rung(&row, &[], 1, 100), Ok(2), "only upward");
        assert_eq!(
            target_rung(&row, &[], 0, 10),
            Ok(1),
            "a backstop climbs even when the arithmetic says the running rung holds it"
        );
        let top = target_rung(&row, &[], 0, 513).unwrap_err();
        assert_eq!((top.index, top.per_slot), (2, 512));
        assert_eq!(top.label(), "3/3 (top.gguf)");
        assert!(
            target_rung(&row, &[], 2, 1).is_err(),
            "nothing above the top"
        );
    }

    #[test]
    fn each_rung_is_judged_on_the_slot_its_trained_context_leaves() {
        let row = ladder();
        // Rung 2 is configured at 128 but trained at 100: a need of 110 is
        // past it, and so is rung 3's 512 when that is trained at 300.
        let trained = [None, Some(100), Some(300)];
        assert_eq!(target_rung(&row, &trained, 0, 110), Ok(2));
        assert_eq!(target_rung(&row, &trained, 0, 100), Ok(1));
        let top = target_rung(&row, &trained, 0, 301).unwrap_err();
        assert_eq!(top.per_slot, 300, "the refusal names the real slot");
    }

    #[test]
    fn the_header_is_one_based_and_drops_a_name_it_cannot_carry() {
        let mut tag = RungTag {
            index: 0,
            of: 3,
            per_slot: 64,
            gguf_file: "base.gguf".into(),
        };
        assert_eq!(tag.header_value(), "1/3; ctx=64; gguf=base.gguf");
        assert_eq!(tag.log(), 1);
        tag.gguf_file = "bäse.gguf".into();
        assert_eq!(tag.header_value(), "1/3; ctx=64");
        tag.gguf_file = "bad\nname.gguf".into();
        assert_eq!(tag.header_value(), "1/3; ctx=64");
    }

    #[test]
    fn the_reason_groups_its_digits() {
        assert_eq!(
            climb_reason(41_210, 8_192, 30_000),
            "prompt 41,210 + 8,192 > 30,000"
        );
        assert_eq!(climb_reason(7, 16, 1_000_000), "prompt 7 + 16 > 1,000,000");
    }
}
