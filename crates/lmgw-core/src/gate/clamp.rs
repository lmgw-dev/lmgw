//! The max-output clamp (ladder design §3.2, unified-KV design §3.3 step 1):
//! the one rule that makes "the fit check said this fits" stay true for the
//! whole answer, on both a ladder rung and a guarded unified-KV pool.
//!
//! Ladder design §3.2: "Every request is sent with
//! `max_tokens = min(client max_tokens or n_predict, n_predict)`." — a
//! missing value is *filled* with the ceiling, and a client value already at
//! or under the ceiling is *kept exactly*, because "a client that asks for
//! less stays on a lower rung longer" depends on the fit check reading the
//! client's real number, not the row's ceiling.

use crate::ir::Params;

/// Bind `params.max_tokens` to `n_predict`, in place.
///
/// Returns `Some(n_predict)` **only** when a client-set value *above* the cap
/// was lowered — the case a caller must stamp
/// [`crate::proxy::MAX_TOKENS_CLAMPED_HEADER`] and log
/// (`request_logs.max_tokens_clamped`) for, the same way the Anthropic route
/// stamps [`crate::proxy::MAX_TOKENS_DEFAULTED_HEADER`] /
/// [`crate::proxy::MAX_TOKENS_RAISED_HEADER`] for its own cap choices.
/// `None` covers both "nothing was set, so the ceiling was only *filled*" and
/// "the client's value already fit" — neither lowered anything, so neither is
/// an annotation-worthy event.
///
/// `n_predict` is a row's configured ceiling (`LlamaParams.n_predict`), which
/// a guarded row's validation already requires to be `> 0` (ladder design
/// §4.3 rule 1, unified-KV design §3.3 "When it is active"); a caller must not
/// invoke this on a row where that is not the case. A negative or absurd input
/// is still handled defensively — clamped to `0` — rather than panicking or
/// producing a `u32` that silently wrapped.
///
/// # What else can carry a generation limit to llama-server (checked)
///
/// This function only ever touches [`Params::max_tokens`]. Four other places
/// were checked for a second route a limit could reach egress by, since a
/// clamp that only binds one of several paths is not a clamp:
///
/// - **`max_completion_tokens`** — modeled at ingress
///   (`ingress/openai.rs` `parse_chat_request`, which reads
///   `max_completion_tokens` before `max_tokens` into this same
///   [`Params::max_tokens`] field) and listed in `MODELED_KEYS`
///   (`ingress/openai.rs`), so it never also rides through `passthrough`.
///   Already bound by this function.
/// - **`param_overrides` (alias defaults)** — folded into the client's
///   `Params` by [`Params::with_defaults`] (`ir.rs`), called from
///   `proxy::resolve_params` *before* egress is reached. As long as the gate
///   calls [`clamp_max_tokens`] on that already-resolved `Params` — which is
///   what every call site has in hand by the time it is about to forward —
///   an alias-configured default above the ceiling is bound exactly like a
///   client-sent one. No special handling needed, only the ordering: clamp
///   after `resolve_params`, never before.
/// - **Anthropic's own (mandatory) `max_tokens`** — read straight into this
///   same `Params::max_tokens` at Anthropic ingress
///   (`ingress/anthropic.rs`), so a request that started life on `/v1/messages`
///   and is routed to a `llama_server` upstream is already covered.
/// - **`chat_template_kwargs` / other web-chat "extra body" fields** — the
///   dashboard's own chat send (`web/chat.rs`, `send_chat_message`) builds its
///   `ChatRequest` by hand with `passthrough: Default::default()` and
///   `params.max_tokens` taken from the thread's own `max_tokens` column —
///   the same field, no side channel. There is no generic "extra body" merge
///   point in this codebase today; there is no
///   existing mechanism of that kind.
///
/// **One bypass remains, and it is real: a raw, native `n_predict` field in
/// the client's JSON body.** `n_predict` is llama.cpp's own completion field,
/// it is not in `ingress/openai.rs`'s `MODELED_KEYS`, so it survives into
/// `ChatRequest.passthrough` untouched, and `egress::llama_cpp::chat_body`'s
/// passthrough loop (`body.entry(k.clone()).or_insert_with(...)`) re-emits it
/// verbatim because the body never sets a `"n_predict"` key itself — nothing
/// currently stops it from reaching llama-server above the clamp, and fact 1
/// (ladder design §2.1) is exactly the reason that matters: "the per-request
/// limit takes priority over the global one", so a raw `n_predict` above
/// `n_predict` the row was clamped to would win over `max_tokens` on the
/// wire.
///
/// This function cannot close that gap — it only sees `Params`, and
/// `chat_body` must stay a byte-identical thin wrapper for a row without a
/// guard (ladder design §7 test 13; unified-KV design §7 test 14), so the
/// egress layer cannot unconditionally rewrite `n_predict` either. **The gate
/// closes it**, on a guarded row only: its per-send half takes the raw
/// `n_predict` out of the passthrough and folds it into `max_tokens` before
/// calling this (`gate::fit`'s `bind_max_output`; the legacy path does the
/// same on the raw body), so the one value that reaches llama-server is the
/// clamped one.
pub fn clamp_max_tokens(params: &mut Params, n_predict: i64) -> Option<u32> {
    let cap = u32::try_from(n_predict.max(0)).unwrap_or(u32::MAX);
    match params.max_tokens {
        Some(v) if v > cap => {
            params.max_tokens = Some(cap);
            Some(cap)
        }
        Some(_) => None,
        None => {
            params.max_tokens = Some(cap);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_value_above_the_cap_is_lowered_and_reported() {
        let mut p = Params {
            max_tokens: Some(8192),
            ..Default::default()
        };
        let clamped = clamp_max_tokens(&mut p, 4096);
        assert_eq!(clamped, Some(4096));
        assert_eq!(p.max_tokens, Some(4096));
    }

    #[test]
    fn a_client_value_at_or_below_the_cap_is_kept_and_not_reported() {
        let mut p = Params {
            max_tokens: Some(100),
            ..Default::default()
        };
        assert_eq!(clamp_max_tokens(&mut p, 4096), None);
        assert_eq!(
            p.max_tokens,
            Some(100),
            "a lower request stays on a lower rung longer"
        );

        let mut equal = Params {
            max_tokens: Some(4096),
            ..Default::default()
        };
        assert_eq!(clamp_max_tokens(&mut equal, 4096), None);
        assert_eq!(equal.max_tokens, Some(4096));
    }

    #[test]
    fn a_missing_value_is_filled_not_clamped() {
        let mut p = Params::default();
        assert_eq!(
            clamp_max_tokens(&mut p, 4096),
            None,
            "filling an absent value is not the same event as lowering one"
        );
        assert_eq!(p.max_tokens, Some(4096));
    }

    #[test]
    fn a_non_positive_ceiling_never_panics_or_wraps() {
        let mut p = Params {
            max_tokens: Some(10),
            ..Default::default()
        };
        assert_eq!(clamp_max_tokens(&mut p, 0), Some(0));
        assert_eq!(p.max_tokens, Some(0));
    }
}
