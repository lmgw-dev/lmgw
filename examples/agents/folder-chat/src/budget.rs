//! The prompt budget: how many tokens of excerpts one answer may carry.
//!
//! **Derived, never guessed.** The chat alias's `context_length` (from
//! `/v1/models/{alias}`), minus the room kept for the answer, minus the system
//! prompt, minus the conversation so far — all measured with the one estimator
//! the chunks are measured with ([`crate::chunk::estimator`]). What remains is
//! the retrieval budget, handed to quickdoc as `SearchParams.budget_tokens`.
//!
//! Two facts a model may not report get a named, visible stand-in, and the
//! [`Budget`] says which numbers are reported and which are stand-ins:
//! [`FALLBACK_CONTEXT_TOKENS`] and [`DEFAULT_ANSWER_RESERVE_TOKENS`].
//!
//! **A reported `max_output_tokens` is the reserve only when it leaves room.**
//! Many servers report the most a model *may* generate, which for a local
//! model is often its whole context: kept free in full, it would leave no
//! room for any prompt, and every question would fail. So the reported value
//! is used when it plus the system prompt plus one excerpt (the chunk size,
//! `excerpt_tokens`) fits the context; otherwise
//! [`DEFAULT_ANSWER_RESERVE_TOKENS`] stands in, and the answer's notes say
//! which number the model reported and why it was not used.
//!
//! History is **never truncated** to make room: a conversation that no longer
//! fits is [`BudgetError::ConversationTooLong`], which says to start a new
//! chat. Dropping the oldest turns silently would change what the model was
//! told without anyone seeing it.

use serde::{Deserialize, Serialize};

use crate::gateway::ModelInfo;

/// The context assumed when the chat model reports no `context_length`. Every
/// answer made under it says so in its metadata, and the [`Budget`] marks the
/// number as a stand-in. 8192 is the smallest context any chat model lmgw
/// serves in practice ships with, so the assumption errs towards fitting.
pub const FALLBACK_CONTEXT_TOKENS: u64 = 8192;

/// The room kept for the answer when the model reports no `max_output_tokens`,
/// or reports one that leaves no room for the system prompt and one excerpt
/// (see the module docs). It is **not** sent as `max_tokens` — the answer is
/// not capped — it only keeps the excerpts from filling the context so full
/// that the model has no room left to reply. Shown in every [`Budget`].
pub const DEFAULT_ANSWER_RESERVE_TOKENS: u64 = 2048;

/// Per-message framing a chat template adds around each message (role
/// markers, separators) — OpenAI's published estimate for its chat format.
/// Counted once per message so a long conversation of short turns is not
/// under-counted; part of the `system_tokens` / `history_tokens` shown.
pub const MESSAGE_FRAMING_TOKENS: u64 = 4;

/// What one answer's prompt is allowed to spend, part by part. The UI prints
/// this as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// The model's context window.
    pub context_tokens: u64,
    /// `false` when the model reports none and [`FALLBACK_CONTEXT_TOKENS`]
    /// stands in.
    pub context_reported: bool,
    /// Kept free for the answer.
    pub answer_reserve_tokens: u64,
    /// `false` when [`DEFAULT_ANSWER_RESERVE_TOKENS`] stands in: the model
    /// reports no `max_output_tokens`, or one that leaves no room for the
    /// system prompt and one excerpt.
    pub answer_reserve_reported: bool,
    /// The model's `max_output_tokens` as reported, whether or not it became
    /// the reserve.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// One excerpt — the chunk size (`chunk_tokens`) — which a reported
    /// `max_output_tokens` must leave room for, beside the system prompt, to
    /// be the reserve.
    #[serde(default)]
    pub excerpt_tokens: u64,
    /// The system prompt, framing included.
    pub system_tokens: u64,
    /// Every earlier turn plus this question and its wrapper, framing
    /// included.
    pub history_tokens: u64,
    /// What is left for excerpts.
    pub retrieval_tokens: u64,
    /// How every number above was measured.
    pub estimator: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "this conversation no longer fits the model's context; start a new chat \
         (context {context} tokens{context_note}: answer reserve {reserve}, system prompt \
         {system}, conversation {history} — estimated with {estimator})"
    )]
    ConversationTooLong {
        context: u64,
        context_note: String,
        reserve: u64,
        system: u64,
        history: u64,
        estimator: String,
    },
    #[error(
        "the model's context ({context} tokens{context_note}) cannot hold its own answer \
         reserve ({reserve}{reserve_note}) plus the system prompt ({system}); pick a chat model \
         with a larger context"
    )]
    ReserveExceedsContext {
        context: u64,
        context_note: String,
        reserve: u64,
        reserve_note: String,
        system: u64,
    },
}

/// Which reserve an answer keeps free, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserve {
    /// The model's own `max_output_tokens`.
    Reported(u64),
    /// [`DEFAULT_ANSWER_RESERVE_TOKENS`]: the model reports none.
    Unreported,
    /// [`DEFAULT_ANSWER_RESERVE_TOKENS`]: the model reports this many, which
    /// leaves no room for the system prompt and one excerpt.
    NoRoom(u64),
}

/// The reserve for a model with `context` tokens (reported or assumed):
/// `max_output_tokens` when it leaves room for `system_tokens` and one
/// excerpt of `excerpt_tokens`, [`DEFAULT_ANSWER_RESERVE_TOKENS`] otherwise.
pub fn reserve_for(
    max_output_tokens: Option<u64>,
    context: u64,
    system_tokens: u64,
    excerpt_tokens: u64,
) -> Reserve {
    match max_output_tokens {
        None => Reserve::Unreported,
        Some(m)
            if m.saturating_add(system_tokens)
                .saturating_add(excerpt_tokens)
                <= context =>
        {
            Reserve::Reported(m)
        }
        Some(m) => Reserve::NoRoom(m),
    }
}

/// Derive the budget for one answer. `system_tokens` and `history_tokens`
/// are already measured (framing included) by the caller; `excerpt_tokens`
/// is one excerpt, the chunk size ([`reserve_for`]).
pub fn derive(
    model: &ModelInfo,
    system_tokens: u64,
    history_tokens: u64,
    excerpt_tokens: u64,
    estimator: &str,
) -> Result<Budget, BudgetError> {
    let (context, context_reported) = match model.context_length {
        Some(c) => (c, true),
        None => (FALLBACK_CONTEXT_TOKENS, false),
    };
    let chosen = reserve_for(
        model.max_output_tokens,
        context,
        system_tokens,
        excerpt_tokens,
    );
    let (reserve, reserve_reported) = match chosen {
        Reserve::Reported(m) => (m, true),
        Reserve::Unreported | Reserve::NoRoom(_) => (DEFAULT_ANSWER_RESERVE_TOKENS, false),
    };
    let context_note = if context_reported {
        String::new()
    } else {
        format!(", assumed: {} reports no context_length", model.alias)
    };
    let fixed = reserve.saturating_add(system_tokens);
    if fixed >= context {
        let reserve_note = match chosen {
            Reserve::Reported(_) => String::new(),
            Reserve::Unreported => format!(
                ", DEFAULT_ANSWER_RESERVE_TOKENS: {} reports no max_output_tokens",
                model.alias
            ),
            Reserve::NoRoom(m) => format!(
                ", DEFAULT_ANSWER_RESERVE_TOKENS: {} reports max_output_tokens {m}, which leaves \
                 no room for the prompt",
                model.alias
            ),
        };
        return Err(BudgetError::ReserveExceedsContext {
            context,
            context_note,
            reserve,
            reserve_note,
            system: system_tokens,
        });
    }
    let used = fixed.saturating_add(history_tokens);
    if used >= context {
        return Err(BudgetError::ConversationTooLong {
            context,
            context_note,
            reserve,
            system: system_tokens,
            history: history_tokens,
            estimator: estimator.to_string(),
        });
    }
    Ok(Budget {
        context_tokens: context,
        context_reported,
        answer_reserve_tokens: reserve,
        answer_reserve_reported: reserve_reported,
        max_output_tokens: model.max_output_tokens,
        excerpt_tokens,
        system_tokens,
        history_tokens,
        retrieval_tokens: context - used,
        estimator: estimator.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(ctx: Option<u64>, out: Option<u64>) -> ModelInfo {
        ModelInfo {
            alias: "m".into(),
            context_length: ctx,
            max_output_tokens: out,
        }
    }

    #[test]
    fn the_remainder_is_what_retrieval_gets() {
        let b = derive(&model(Some(32_768), Some(4096)), 200, 1000, 400, "est").unwrap();
        assert_eq!(b.retrieval_tokens, 32_768 - 4096 - 200 - 1000);
        assert!(b.context_reported && b.answer_reserve_reported);
        assert_eq!(b.max_output_tokens, Some(4096));
    }

    #[test]
    fn unreported_numbers_fall_back_visibly() {
        let b = derive(&model(None, None), 100, 100, 400, "est").unwrap();
        assert_eq!(b.context_tokens, FALLBACK_CONTEXT_TOKENS);
        assert_eq!(b.answer_reserve_tokens, DEFAULT_ANSWER_RESERVE_TOKENS);
        assert!(!b.context_reported && !b.answer_reserve_reported);
        assert_eq!(b.max_output_tokens, None);
        assert_eq!(
            b.retrieval_tokens,
            FALLBACK_CONTEXT_TOKENS - DEFAULT_ANSWER_RESERVE_TOKENS - 200
        );
    }

    #[test]
    fn a_reported_reserve_that_leaves_no_room_is_replaced_by_the_default() {
        // The whole context: many servers report max_output_tokens so.
        let b = derive(&model(Some(32_768), Some(32_768)), 200, 1000, 400, "est").unwrap();
        assert_eq!(b.answer_reserve_tokens, DEFAULT_ANSWER_RESERVE_TOKENS);
        assert!(!b.answer_reserve_reported);
        assert_eq!(b.max_output_tokens, Some(32_768));
        assert_eq!(
            b.retrieval_tokens,
            32_768 - DEFAULT_ANSWER_RESERVE_TOKENS - 200 - 1000
        );
        // Less than the context, but no room for the system prompt and one
        // excerpt beside it: 7500 + 200 + 400 > 8000.
        assert_eq!(
            reserve_for(Some(7500), 8000, 200, 400),
            Reserve::NoRoom(7500)
        );
        let b = derive(&model(Some(8000), Some(7500)), 200, 100, 400, "est").unwrap();
        assert_eq!(b.answer_reserve_tokens, DEFAULT_ANSWER_RESERVE_TOKENS);
        // Exactly room for both: the reported value stands.
        assert_eq!(
            reserve_for(Some(7400), 8000, 200, 400),
            Reserve::Reported(7400)
        );
        let b = derive(&model(Some(8000), Some(7400)), 200, 100, 400, "est").unwrap();
        assert_eq!(b.answer_reserve_tokens, 7400);
        assert!(b.answer_reserve_reported);
        assert_eq!(b.excerpt_tokens, 400);
    }

    #[test]
    fn a_conversation_that_no_longer_fits_is_an_error_not_a_truncation() {
        let e = derive(&model(Some(8000), Some(1000)), 100, 6900, 400, "est").unwrap_err();
        assert!(matches!(e, BudgetError::ConversationTooLong { .. }));
        assert!(e.to_string().contains("start a new chat"), "{e}");
        // Only a context that cannot hold even the default reserve and the
        // system prompt asks for a larger model.
        let e = derive(&model(None, Some(9000)), 7000, 1, 400, "est").unwrap_err();
        assert!(
            matches!(e, BudgetError::ReserveExceedsContext { .. }),
            "{e}"
        );
        let msg = e.to_string();
        assert!(msg.contains("reports no context_length"), "{msg}");
        assert!(msg.contains("max_output_tokens 9000"), "{msg}");
        assert!(msg.contains("DEFAULT_ANSWER_RESERVE_TOKENS"), "{msg}");
    }
}
