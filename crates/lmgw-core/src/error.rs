//! Single gateway error type, normalized to the client's protocol shape (§14).

use axum::http::StatusCode;

#[derive(Debug, Clone, thiserror::Error)]
pub enum GatewayError {
    #[error("unknown model alias: {0}")]
    UnknownAlias(String),
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    /// An agent token whose agent is switched off (container-runtime §3.1).
    ///
    /// Its own code rather than a bare `Unauthorized`, because Disable is the
    /// kill switch for an agent and the container on the other end has to be
    /// able to tell "the owner turned me off" from "my token is wrong" — one
    /// is fixed by rotating a credential and the other never is.
    #[error("agent '{agent}' is disabled; its token is refused until it is enabled again")]
    AgentDisabled { agent: String },
    /// A capability refusal from the gate (principals §3.5, §3.9), carried
    /// into this type only so that `/v1` and `/mcp` can render it in the
    /// **client's** dialect and log it like any other refusal.
    ///
    /// Its `status`, `code` and `message` are the `principal::Refusal`'s own —
    /// the gate is the one place that decides them, and a second wording here
    /// would be a second policy.
    #[error("{message}")]
    Refused {
        status: u16,
        code: &'static str,
        message: String,
    },
    #[error("bad request: {0}")]
    BadRequest(String),
    /// A request lmgw refuses before sending it anywhere, with a code of its
    /// own a client can branch on — a speech request with nothing to say
    /// but inline tags (`empty_input`), a voice-design model asked to speak
    /// without a description (`instructions_required`). A 400
    /// `invalid_request_error` in both dialects, like
    /// [`BadRequest`](Self::BadRequest), which has no code beyond
    /// `bad_request`.
    #[error("{message}")]
    InvalidRequest { code: &'static str, message: String },
    /// The request body is larger than the `max_body_mb` setting allows.
    ///
    /// Separate from [`BadRequest`](Self::BadRequest) purely so the message can
    /// name the knob: a client told only "payload too large" has no way to know
    /// the ceiling is ours, configurable, and one field away.
    #[error(
        "request body exceeds max_body_mb ({max_mb} MiB) — raise it under Settings → Network & \
         access"
    )]
    BodyTooLarge { max_mb: u32 },
    /// A resource named by the request does not exist — a stored response that
    /// was never created or has been evicted (§21). Distinct from
    /// [`UnknownAlias`](Self::UnknownAlias), which is about routing.
    #[error("not found: {0}")]
    NotFound(String),
    /// Error returned by the upstream provider, detail preserved.
    #[error("upstream error ({status}): {message}")]
    Upstream {
        status: u16,
        provider_type: Option<String>,
        message: String,
    },
    #[error("upstream transport error: {0}")]
    Transport(String),
    #[error("upstream timeout")]
    Timeout,
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The model is larger than the GPU it would have to load on (quickdoc
    /// §9b). Distinct from [`VramQueueTimeout`](Self::VramQueueTimeout) because
    /// waiting cannot help: nothing that could ever be evicted would make this
    /// one fit, so it fails at once instead of holding a queue slot forever.
    #[error(
        "'{model}' needs {need} of GPU memory plus {headroom} headroom, and the GPU holds \
         {capacity} in total — no eviction can make it fit"
    )]
    VramTooLarge {
        model: String,
        need: String,
        headroom: String,
        capacity: String,
    },
    /// The request waited `vram.queue_timeout_seconds` for GPU memory and never
    /// got it. `holding` names what was resident, so the refusal is actionable.
    #[error("'{model}' waited {waited_seconds}s for GPU memory and was refused — {holding}")]
    VramQueueTimeout {
        model: String,
        waited_seconds: u64,
        holding: String,
    },
    /// The request waited `vram.queue_timeout_seconds` for room in a guarded
    /// unified-KV model's shared pool (unified-KV design §3.3 step 6) and never
    /// got it.
    ///
    /// A sibling of [`VramQueueTimeout`](Self::VramQueueTimeout) rather than a
    /// reuse of it: that message says "waited for GPU memory", and this wait
    /// is for KV cells inside a container that is already up — naming the
    /// wrong resource would send the owner hunting for a model to evict on a
    /// card with room to spare. Everything a client keys on is shared,
    /// though: the same 503, the same `vram_queue_timeout` code and the same
    /// Anthropic `overloaded_error` (the spec's own wording, §3.3 step 6), so
    /// a client that already retries the one retries the other.
    #[error(
        "'{model}' waited {waited_seconds}s for room in its shared KV pool and was refused — it \
         needs {needs} tokens (prompt + max output), {reserved} of the pool's {capacity} are \
         reserved by {in_flight} request(s) in flight, and {ahead} request(s) were queued ahead \
         of it"
    )]
    KvPoolTimeout {
        model: String,
        waited_seconds: u64,
        needs: u64,
        capacity: u64,
        reserved: u64,
        in_flight: usize,
        ahead: usize,
    },
    /// A ladder climb waited `vram.queue_timeout_seconds` for the rung it
    /// replaces to go quiet and it did not (ladder design §3.4 step 2, §12
    /// entry 10): lmgw's own sends to it were still in flight, or its
    /// `/slots` still reported slots generating (a client on the container's
    /// own port, or a send lmgw just dropped). The climb is abandoned and the
    /// running rung keeps serving.
    ///
    /// A sibling of [`VramQueueTimeout`](Self::VramQueueTimeout), like
    /// [`KvPoolTimeout`](Self::KvPoolTimeout) and for the same reason: that
    /// message says "waited for GPU memory", and this wait is for other
    /// requests on the same model. Everything a client keys on is shared —
    /// 503, `vram_queue_timeout`, Anthropic `overloaded_error` — so a client
    /// that retries the one retries this.
    #[error("{}", ladder_drain_message(.model, .rung, .waited_seconds, .sends, .slots))]
    LadderDrainTimeout {
        model: String,
        /// The rung it tried to climb to, `k/n (<gguf>)`, 1-based.
        rung: String,
        waited_seconds: u64,
        /// lmgw's sends still in flight on the running rung.
        sends: u32,
        /// Slots its `/slots` still reported generating, once lmgw's own
        /// sends had ended — traffic on the container's own port, or a send
        /// lmgw just dropped that llama-server has not let go of yet.
        slots: usize,
    },
    #[error("internal error: {0}")]
    Internal(String),
    /// GPU hold is active (gpu-hold design §1/§4) and this local model has no
    /// usable fallback — refused at resolve time, before any stream byte.
    /// `detail` is `""` or names why a configured fallback did not help, e.g.
    /// `" (fallback 'x' is itself a local model)"`.
    ///
    /// Not Anthropic's `overloaded_error`: that type is the SDKs' own
    /// retry-with-backoff signal, and a hold can last an hour — telling a
    /// client to keep retrying would be a lie.
    #[error(
        "'{model}' is a local model and lmgw is holding the GPU{detail} — release the hold or \
         configure a fallback alias"
    )]
    GpuHold { model: String, detail: String },
    /// A benchmark run holds the card (benchmark design §3.2): no local model
    /// of any class is admitted until it ends, and this one has no usable
    /// fallback. The hold's twin — 503 and never `overloaded_error`, for the
    /// same reason: a run of a large model takes minutes, and a client told
    /// to retry with backoff would only spend them retrying. `detail` is the
    /// hold's (`""`, or why a configured fallback did not help).
    #[error(
        "'{model}' is a local model and benchmark run {run_id} has the GPU to itself{detail} — \
         it ends when the run finishes or is canceled (Benchmarks page, or lmgw__bench_cancel)"
    )]
    GpuBenchmark {
        model: String,
        run_id: i64,
        detail: String,
    },
    /// Background traffic (candidate-aliases design §4.3) could not be
    /// served without disturbing the owner, and its alias has no fallback
    /// (`none`) — nothing local was loaded that could take it, and starting
    /// or climbing one would have meant evicting or waiting on the owner's
    /// work. `detail` says what was in the way ("GPU in use by …").
    ///
    /// On the wire it is the hold's refusal — 503, code `gpu_hold`
    /// ([`Self::code`]) — because that is the answer the owner's clients
    /// already handle as "local models are paused, not down". In the request
    /// log and the rollup it is its own kind, `gpu_deferred`, so a deferral
    /// can be told from a real hold. Not `overloaded_error` for the same
    /// reason as the hold.
    #[error(
        "'{model}' deferred: {detail} — background traffic never disturbs the owner's models; \
         retry later, or give the alias a fallback"
    )]
    GpuDeferred { model: String, detail: String },
    /// A candidate alias's model went away under a request that was using it
    /// — its container died, or a stop or a failed climb took it — and this
    /// request may not start it again (candidate-aliases design §4.1, §12
    /// entry 46): it was an alternate, which the alias never starts, or the
    /// primary of a background request, whose restart would have disturbed
    /// the owner, or the owner's own model, which a guest never stops (§12
    /// entry 90). `detail` says which.
    ///
    /// Raised by [`crate::vram::LocalHold`]'s dead-container recovery before
    /// anything reached the client, so the chat send can pick again (another
    /// loaded candidate, or the alias fallback). It reaches a client as an
    /// honest 503 — the model the request was on is gone, and the next
    /// request is picked afresh — from `/v1/count_tokens`, which counts on
    /// the candidate it was admitted to and does not pick again: a metadata
    /// call the client simply repeats (§12 entry 76).
    #[error(
        "'{model}' stopped while this request was using it and is not restarted for it: {detail}"
    )]
    CandidateLost { model: String, detail: String },
    /// A candidate alias without background whose **primary** cannot be used
    /// — its row lost a facet the alias enables, was disabled or deleted —
    /// and no other candidate is loaded (candidate-aliases design §4.2, §4.6;
    /// §12 phase-4 decision). The alias only ever starts its primary, so there
    /// is nothing it may load.
    ///
    /// Only when the alias has no fallback that can answer: with one, the
    /// fallback answers instead (reason `unavailable`, §12 entry 70 —
    /// the owner's order ends "otherwise use fallback") and the drift is
    /// logged. `503` like the hold's family (it clears once the
    /// configuration is fixed); its own kind so the log says what it is.
    #[error(
        "'{alias}' cannot serve this request: {detail} — fix the row (its capabilities \
         override, or enable it), or change the alias's candidates or capabilities"
    )]
    CandidateUnavailable { alias: String, detail: String },
    /// The key's scope does not admit this alias (usage-analytics §4.2).
    #[error("key '{key}' is not allowed to use '{alias}' — {reason}")]
    KeyScope {
        key: String,
        alias: String,
        reason: String,
    },
    /// The key, or the gateway as a whole, has spent its budget for the period.
    ///
    /// **403, deliberately not 429.** A 429 is the signal every SDK reads as
    /// "retry with backoff", and a monthly budget will not clear during any
    /// retry window — the same reasoning that kept [`GpuHold`](Self::GpuHold)
    /// off Anthropic's `overloaded_error`.
    #[error(
        "{scope} has spent {spent} of its {period} budget of {budget} — raise it under {set_on}"
    )]
    KeyBudget {
        scope: String,
        spent: String,
        budget: String,
        period: &'static str,
        /// Where the owner raises this one: a key's budget is on Usage →
        /// Keys, the gateway's on Settings → Usage & cost.
        set_on: &'static str,
    },
    /// A rate or concurrency limit. Unlike a budget this *does* clear in
    /// seconds, so it keeps 429 and carries a `Retry-After` that is a real
    /// number rather than a shrug.
    #[error("key '{key}' is over its {limit_kind} limit of {limit} — retry in {retry_after}s")]
    KeyRate {
        key: String,
        limit_kind: &'static str,
        limit: i64,
        retry_after: u64,
    },
    /// The key is past its expiry date.
    #[error("key '{key}' expired on {expired_at}")]
    KeyExpired { key: String, expired_at: String },
    /// The counted prompt (plus max output, when known) does not fit the
    /// model's per-request context — a ladder's top rung (ladder design §3.1
    /// "too big for the top rung") or a guarded unified-KV pool's
    /// per-request limit (unified-KV design §3.3 step 4). Distinct from a
    /// plain [`Upstream`](Self::Upstream) 400 so the client always sees the
    /// same stable `context_length_exceeded` code, whatever wording
    /// llama-server's own `exceed_context_size_error` used that build (§2.1
    /// fact 14 / egress `map_error`'s backstop).
    ///
    /// `max_output` is `None` when it is not known at the point of refusal
    /// (the llama-server backstop, the llama.cpp egress's `map_error`, only
    /// ever learns the prompt count and the limit from the error body itself) —
    /// the message then states the prompt alone against the limit, rather
    /// than a sum that silently dropped a term.
    ///
    /// `top_rung` names the rung `limit` belongs to — `3/3 (top.gguf)` — when
    /// the refusal is a ladder's "too big for the top rung" (ladder design
    /// §3.1): the limit is then the top rung's per-slot context, and the
    /// message says so, so nobody reads it as the rung that happened to run.
    #[error("{}", context_exceeded_message(.model, .prompt_tokens, .max_output, .limit, .top_rung))]
    ContextExceeded {
        model: String,
        prompt_tokens: u64,
        max_output: Option<u64>,
        limit: u64,
        top_rung: Option<String>,
    },
}

/// [`GatewayError::ContextExceeded`]'s message: states the prompt tokens, the
/// max output when known, their sum, and the per-request limit, so a client
/// (and the owner, reading a log line) never has to do the arithmetic to see
/// why it was refused.
fn context_exceeded_message(
    model: &str,
    prompt_tokens: &u64,
    max_output: &Option<u64>,
    limit: &u64,
    top_rung: &Option<String>,
) -> String {
    let prompt_tokens = *prompt_tokens;
    let limit = *limit;
    let context = match top_rung {
        Some(rung) => format!("the top rung's per-slot context of {limit} tokens (rung {rung})"),
        None => format!("the per-request context of {limit} tokens"),
    };
    match max_output {
        Some(out) => {
            let out = *out;
            // Both terms are client-controlled (the counted prompt, and a
            // `max_tokens`/`n` a request can set to anything it likes) — this
            // message must never be the place an absurd request panics under
            // overflow checks or wraps into a nonsense number (review finding
            // 5).
            format!(
                "'{model}': prompt {prompt_tokens} tokens + max output {out} = {} exceeds \
                 {context} — shorten the conversation or lower max_tokens",
                prompt_tokens.saturating_add(out)
            )
        }
        None => format!(
            "'{model}': prompt {prompt_tokens} tokens exceeds {context} — shorten the \
             conversation"
        ),
    }
}

/// [`GatewayError::LadderDrainTimeout`]'s message: which model, which rung it
/// wanted, how long it waited, and what was still busy.
fn ladder_drain_message(
    model: &str,
    rung: &str,
    waited_seconds: &u64,
    sends: &u32,
    slots: &usize,
) -> String {
    let busy = if *sends > 0 {
        format!("{sends} request(s) were still in flight on the running rung")
    } else {
        // Not only a client on the container's port: a send lmgw has just
        // dropped (a verdict of "does not fit", a hung-up client) keeps its
        // slot until llama-server notices the closed connection.
        format!(
            "the running rung still had {slots} slot(s) generating (a client on its own port, \
             or a send lmgw just dropped)"
        )
    };
    format!(
        "'{model}' waited {waited_seconds}s to climb to rung {rung} and {busy} — it keeps \
         serving on the running rung; retry once they finish, or raise \
         vram.queue_timeout_seconds"
    )
}

impl GatewayError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::UnknownAlias(_) | Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unauthorized(_) | Self::AgentDisabled { .. } => StatusCode::UNAUTHORIZED,
            Self::Refused { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::FORBIDDEN)
            }
            Self::BadRequest(_)
            | Self::InvalidRequest { .. }
            | Self::Unsupported(_)
            | Self::ContextExceeded { .. } => StatusCode::BAD_REQUEST,
            Self::BodyTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            // Pass client-attributable upstream statuses through, otherwise 502.
            Self::Upstream { status, .. } => match *status {
                400 => StatusCode::BAD_REQUEST,
                401 | 403 => StatusCode::BAD_GATEWAY, // upstream auth is *our* config problem
                404 => StatusCode::NOT_FOUND,
                413 => StatusCode::PAYLOAD_TOO_LARGE,
                422 => StatusCode::UNPROCESSABLE_ENTITY,
                429 => StatusCode::TOO_MANY_REQUESTS,
                529 => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::BAD_GATEWAY,
            },
            Self::Transport(_) => StatusCode::BAD_GATEWAY,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::VramTooLarge { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::GpuHold { .. }
            | Self::GpuBenchmark { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::KeyScope { .. } | Self::KeyBudget { .. } => StatusCode::FORBIDDEN,
            Self::KeyRate { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::KeyExpired { .. } => StatusCode::UNAUTHORIZED,
        }
    }

    /// Short machine-readable kind, used in `request_logs.error_kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnknownAlias(_) => "unknown_alias",
            Self::NotFound(_) => "not_found",
            Self::Unauthorized(_) => "auth",
            Self::AgentDisabled { .. } => "agent_disabled",
            // The gate's own code, so Logs and the Usage errors chart group a
            // refusal under the word the refusal itself used.
            Self::Refused { code, .. } => code,
            Self::BadRequest(_) => "bad_request",
            Self::InvalidRequest { code, .. } => code,
            Self::BodyTooLarge { .. } => "body_limit",
            Self::Upstream { .. } => "upstream",
            Self::Transport(_) => "transport",
            Self::Timeout => "timeout",
            Self::Unsupported(_) => "unsupported",
            Self::VramTooLarge { .. } => "vram_too_large",
            // One code for both waits (see `KvPoolTimeout`'s doc comment): a
            // client's retry logic should not have to learn a second word for
            // "at capacity, try again".
            Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. } => "vram_queue_timeout",
            Self::Internal(_) => "internal",
            Self::GpuHold { .. } => "gpu_hold",
            Self::GpuBenchmark { .. } => "gpu_benchmark",
            Self::GpuDeferred { .. } => "gpu_deferred",
            Self::CandidateLost { .. } => "candidate_lost",
            Self::CandidateUnavailable { .. } => "candidate_unavailable",
            Self::KeyScope { .. } => "key_scope",
            Self::KeyBudget { .. } => "key_budget",
            Self::KeyRate { .. } => "key_rate",
            Self::KeyExpired { .. } => "key_expired",
            // The one code both dialects' `code`/error-shape render as
            // `context_length_exceeded` — see `to_openai_json`, which reuses
            // this same `kind()` for its `code` field.
            Self::ContextExceeded { .. } => "context_length_exceeded",
        }
    }

    /// OpenAI error `type` field.
    pub fn openai_type(&self) -> &'static str {
        match self {
            Self::UnknownAlias(_)
            | Self::NotFound(_)
            | Self::BadRequest(_)
            | Self::InvalidRequest { .. }
            | Self::BodyTooLarge { .. }
            | Self::Unsupported(_)
            | Self::ContextExceeded { .. } => "invalid_request_error",
            Self::Unauthorized(_) | Self::AgentDisabled { .. } => "authentication_error",
            Self::Refused { status, .. } => match status {
                401 => "authentication_error",
                _ => "permission_error",
            },
            Self::Upstream { .. }
            | Self::Transport(_)
            | Self::Timeout
            | Self::VramTooLarge { .. }
            | Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. }
            | Self::Internal(_)
            | Self::GpuHold { .. }
            | Self::GpuBenchmark { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => "api_error",
            Self::KeyScope { .. } | Self::KeyBudget { .. } | Self::KeyExpired { .. } => {
                "permission_error"
            }
            Self::KeyRate { .. } => "rate_limit_error",
        }
    }

    /// Anthropic error `type` field.
    pub fn anthropic_type(&self) -> &'static str {
        match self {
            Self::UnknownAlias(_) | Self::NotFound(_) => "not_found_error",
            Self::Unauthorized(_) | Self::AgentDisabled { .. } => "authentication_error",
            Self::Refused { status, .. } => match status {
                401 => "authentication_error",
                _ => "permission_error",
            },
            Self::BadRequest(_)
            | Self::InvalidRequest { .. }
            | Self::BodyTooLarge { .. }
            | Self::Unsupported(_)
            | Self::ContextExceeded { .. } => "invalid_request_error",
            Self::Timeout | Self::Upstream { .. } | Self::Transport(_) => "api_error",
            // Anthropic's own word for "the server is at capacity, retry".
            Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. } => "overloaded_error",
            // Deliberately not `overloaded_error` — see the variant's doc
            // comment: that type tells Anthropic SDKs to retry with backoff,
            // and a hold can last an hour.
            Self::VramTooLarge { .. }
            | Self::Internal(_)
            | Self::GpuHold { .. }
            | Self::GpuBenchmark { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => "api_error",
            Self::KeyScope { .. } | Self::KeyBudget { .. } => "permission_error",
            Self::KeyExpired { .. } => "authentication_error",
            Self::KeyRate { .. } => "rate_limit_error",
        }
    }

    /// The OpenAI body's `code` — what a client branches on. The same word
    /// as [`Self::kind`], except where the log needs a finer word than the
    /// client should have to learn: a background deferral is logged
    /// `gpu_deferred` and answers `gpu_hold`, the code the owner's clients
    /// already read as "local models are paused" (candidate-aliases design
    /// §4.3).
    pub fn code(&self) -> &'static str {
        match self {
            Self::GpuDeferred { .. } => "gpu_hold",
            _ => self.kind(),
        }
    }

    /// OpenAI-shaped error body.
    pub fn to_openai_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "message": self.to_string(),
                "type": self.openai_type(),
                "param": null,
                "code": self.code(),
            }
        })
    }

    /// Anthropic-shaped error body.
    pub fn to_anthropic_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error": {
                "type": self.anthropic_type(),
                "message": self.to_string(),
            }
        })
    }
}

impl From<reqwest::Error> for GatewayError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else {
            Self::Transport(e.to_string())
        }
    }
}

impl From<sqlx::Error> for GatewayError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("db: {e}"))
    }
}

impl From<serde_json::Error> for GatewayError {
    fn from(e: serde_json::Error) -> Self {
        Self::BadRequest(format!("invalid JSON: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exceeded(max_output: Option<u64>) -> GatewayError {
        GatewayError::ContextExceeded {
            model: "m".into(),
            prompt_tokens: 41_210,
            max_output,
            limit: 30_000,
            top_rung: None,
        }
    }

    #[test]
    fn a_ladders_refusal_names_the_top_rung_its_limit_belongs_to() {
        let e = GatewayError::ContextExceeded {
            model: "qwen".into(),
            prompt_tokens: 140_000,
            max_output: Some(8_192),
            limit: 130_000,
            top_rung: Some("3/3 (top.gguf)".into()),
        };
        assert_eq!(
            e.to_string(),
            "'qwen': prompt 140000 tokens + max output 8192 = 148192 exceeds the top rung's \
             per-slot context of 130000 tokens (rung 3/3 (top.gguf)) — shorten the \
             conversation or lower max_tokens"
        );
        assert_eq!(e.kind(), "context_length_exceeded");
    }

    #[test]
    fn the_message_states_prompt_max_output_sum_and_limit() {
        let e = exceeded(Some(8_192));
        assert_eq!(
            e.to_string(),
            "'m': prompt 41210 tokens + max output 8192 = 49402 exceeds the per-request context \
             of 30000 tokens — shorten the conversation or lower max_tokens"
        );
    }

    #[test]
    fn the_message_saturates_instead_of_overflowing() {
        // Both `prompt_tokens` and `max_output` are client-controlled (a
        // request can set `max_tokens`/`n` to anything it likes) — the sum
        // in the message must saturate, never panic under overflow checks or
        // silently wrap (review finding 5).
        let e = GatewayError::ContextExceeded {
            model: "m".into(),
            prompt_tokens: u64::MAX - 10,
            max_output: Some(u64::MAX),
            limit: 30_000,
            top_rung: None,
        };
        assert!(e.to_string().contains(&u64::MAX.to_string()), "{e}");
    }

    #[test]
    fn the_message_drops_the_sum_when_max_output_is_unknown() {
        let e = exceeded(None);
        assert_eq!(
            e.to_string(),
            "'m': prompt 41210 tokens exceeds the per-request context of 30000 tokens — shorten \
             the conversation"
        );
    }

    #[test]
    fn a_ladder_drain_timeout_names_what_was_busy_and_retries_like_a_queue_timeout() {
        let e = GatewayError::LadderDrainTimeout {
            model: "qwen".into(),
            rung: "3/3 (top.gguf)".into(),
            waited_seconds: 30,
            sends: 2,
            slots: 0,
        };
        let msg = e.to_string();
        assert!(
            msg.contains("'qwen'") && msg.contains("rung 3/3 (top.gguf)"),
            "{msg}"
        );
        assert!(msg.contains("2 request(s) were still in flight"), "{msg}");
        assert_eq!(e.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.kind(), "vram_queue_timeout");
        assert_eq!(e.to_openai_json()["error"]["type"], "api_error");
        assert_eq!(e.to_anthropic_json()["error"]["type"], "overloaded_error");

        let direct = GatewayError::LadderDrainTimeout {
            model: "qwen".into(),
            rung: "2/3 (mid.gguf)".into(),
            waited_seconds: 30,
            sends: 0,
            slots: 1,
        };
        assert!(
            direct.to_string().contains("1 slot(s) generating"),
            "{direct}"
        );
    }

    /// A background deferral answers exactly like the hold on the wire —
    /// 503, code `gpu_hold`, never `overloaded_error` — and is its own kind
    /// in the log (candidate-aliases design §4.3).
    #[test]
    fn a_deferral_answers_like_the_hold_and_logs_as_its_own_kind() {
        let e = GatewayError::GpuDeferred {
            model: "jobs".into(),
            detail: "GPU in use by chat/qwen (18.0 GB, 1 in flight)".into(),
        };
        assert_eq!(e.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.kind(), "gpu_deferred");
        assert_eq!(e.code(), "gpu_hold");
        let openai = e.to_openai_json();
        assert_eq!(openai["error"]["code"], "gpu_hold");
        assert_eq!(openai["error"]["type"], "api_error");
        assert_eq!(e.to_anthropic_json()["error"]["type"], "api_error");
        assert!(
            e.to_string()
                .starts_with("'jobs' deferred: GPU in use by chat/qwen"),
            "{e}"
        );

        let lost = GatewayError::CandidateLost {
            model: "alt".into(),
            detail: "an alternate is never started by its alias".into(),
        };
        assert_eq!(lost.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(lost.kind(), "candidate_lost");
        assert_eq!(lost.code(), "candidate_lost");
    }

    #[test]
    fn it_renders_as_a_400_on_both_protocols() {
        let e = exceeded(Some(8_192));
        assert_eq!(e.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(e.kind(), "context_length_exceeded");

        let openai = e.to_openai_json();
        assert_eq!(openai["error"]["type"], "invalid_request_error");
        assert_eq!(openai["error"]["code"], "context_length_exceeded");

        let anthropic = e.to_anthropic_json();
        assert_eq!(anthropic["error"]["type"], "invalid_request_error");
    }
}
