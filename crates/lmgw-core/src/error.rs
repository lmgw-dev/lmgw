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
    /// A refusal whose status the refusing layer decides, carried into this
    /// type so that `/v1` and `/mcp` can render it in the **client's**
    /// dialect and log it like any other refusal: the principal gate's
    /// capability refusals (principals §3.5, §3.9), and `/v1/systemone`'s
    /// route check, a `501 not_supported` (decision-models design §2.2).
    ///
    /// Its `status`, `code` and `message` are the refusing layer's own — that
    /// layer is the one place that decides them, and a second wording here
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
    /// A 4xx of the OpenAI platform that lmgw does not classify (an unknown
    /// parameter, a deprecated model, ...): OpenAI's own error object, relayed
    /// as sent (its `message`, `type`, `code` and `param`) with its status.
    /// A `401` or `403` is never relayed: the upstream key is lmgw's own
    /// config, and the plain [`Upstream`](Self::Upstream) says so. On the
    /// Anthropic wire the type maps to the nearest Anthropic one. Behaves as
    /// [`Upstream`](Self::Upstream) everywhere else.
    #[error("{}", .0.message)]
    UpstreamRelayed(Box<Relayed>),
    /// A whole Response object the OpenAI platform answered with a 2xx and
    /// `status: "failed"` (openai egress design §7.2.3): the request reached
    /// the model and failed there. OpenAI's own `error.code` and `message`
    /// reach the client, as a stream's `response.failed` does; the usage and
    /// tier the Response reported stay with it for the row, which counts
    /// them like an answer's (decision 22). A `502` like
    /// [`Upstream`](Self::Upstream)'s server errors, and the same kind.
    #[error("{}", .0.text())]
    UpstreamFailed(Box<FailedAnswer>),
    #[error("upstream transport error: {0}")]
    Transport(String),
    /// A cloud upstream could not be connected to (openai egress design
    /// §14.4, decision 28): the connect failed, so nothing reached the
    /// upstream and nothing reached the client. A `502` as
    /// [`Transport`](Self::Transport) is, with the kind `unreachable`; a
    /// fallback answers it (reason `unreachable`). A local container's
    /// connect failure stays `Transport`, which the dead-container recovery
    /// reads.
    #[error("upstream unreachable: {0}")]
    Unreachable(String),
    #[error("upstream timeout")]
    Timeout,
    /// A stream the upstream fell silent on past its headers for longer than
    /// its timeout (`proxy::silence`): [`Timeout`](Self::Timeout)'s kind and
    /// status, and a message that says how long it was silent, which a
    /// turn's caller (the Responses loop's `response.failed`) passes on.
    #[error("{0}")]
    Stalled(String),
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
    /// `" (fallback 'x' cannot serve this endpoint) (tried: a → x)"`.
    ///
    /// Not Anthropic's `overloaded_error`: that type is the SDKs' own
    /// retry-with-backoff signal, and a hold can last an hour — telling a
    /// client to keep retrying would be a lie.
    #[error(
        "'{model}' is a local model and lmgw is holding the GPU{detail} — release the hold or \
         give the model a fallback that can answer"
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
    /// A local model's start on the graphics card failed, and the machine
    /// check names why: the card or its driver (§1.1) or, for a row that
    /// gives its container the NVIDIA card, what lets models use it (§1.2)
    /// — machine-check design §2.5, D23, D64. `problem` is what follows the
    /// model's name (D65): "could not start on the graphics card of the
    /// computer lmgw runs on." and the item's plain sentence, with "Open
    /// lmgw there to see what to do." when the item has a plan or steps to
    /// show (no card at all has words of its own); `detail` the start's own
    /// error and any clause saying why a configured fallback did not
    /// answer, so nothing of Podman's text is lost.
    ///
    /// 503 with its own code, `gpu_setup`, like the hold's family: the
    /// request may succeed once the computer is set up, which no retry
    /// window brings about, so never Anthropic's `overloaded_error`. Not a
    /// deliberate refusal, so not in `telemetry::REFUSAL_KINDS`: the usage
    /// page counts it as a failure.
    #[error("'{model}' {problem}{detail}")]
    GpuSetup {
        model: String,
        problem: String,
        detail: String,
    },
    /// The engine image a local model runs in is not on this machine, and
    /// downloading it before the start failed, or cannot begin (a
    /// `localhost/` build): machine-check design §5, D32. `message` is the
    /// whole plain sentence ("The engine chat model 'm' runs in could not be
    /// downloaded, so the model could not start: …", why in the job's or
    /// Podman's own words after the colon, the image reference and the
    /// setting the engine is chosen in) and any clause saying why a
    /// configured fallback did not answer. A `502`, as WP10 answered it, with a code of its own,
    /// `engine_download`, so a client and the log tell it from an
    /// upstream's error; a configured fallback answers it (reason
    /// `engine_download`, WP5).
    #[error("{message}")]
    EngineDownload { message: String },
    /// Background traffic (candidate-aliases design §4.3, per-request
    /// background design §4) could not be served without disturbing the
    /// owner, and what it named has no fallback that can answer — nothing
    /// local was loaded that could take it, and starting or climbing one
    /// would have meant evicting or waiting on the owner's work. `detail` says
    /// what was in the way ("GPU in use by …"); `on` is what was named, a
    /// candidate alias or a model, which is what the fallback would be
    /// given to.
    ///
    /// 503 with its own code, `gpu_deferred`, on the wire as in the request
    /// log (*changed 2026-10-21*, the owner's decision A: it used to answer
    /// `gpu_hold`). A client must not retry the hold, which can last an hour;
    /// a deferral is "retry later". Not `overloaded_error` for the same reason
    /// as the hold: the SDKs' backoff would spend the wait retrying.
    #[error(
        "'{model}' deferred: {detail} — background traffic never disturbs the owner's models; \
         retry later, or give the {} a fallback",
        .on.noun()
    )]
    GpuDeferred {
        model: String,
        detail: String,
        on: Deferred,
    },
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
    /// `key` is the key as a refusal names it ([`crate::config::ApiKey::described`]).
    #[error("{key} is not allowed to use '{alias}' — {reason}")]
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
    /// A budgeted caller's request on a cloud route whose scope prices none
    /// of the units the request is billed in (openai egress design §13.3,
    /// decision 24): its spend could not be counted, so a budget could not
    /// be enforced on it. Flex without a flex price is one case: no other
    /// tier's price bounds a flex answer. `scope` is whose budget it is
    /// ([`crate::config::ApiKey::described`], or "this gateway" for the
    /// global one), `missing` the price that is missing, its units and the
    /// tiers that do have one. `or_default`: the request asked for flex and
    /// another tier has a price, so sending `service_tier: default` is a way
    /// out too.
    ///
    /// **403 with a code of its own**, like [`KeyBudget`](Self::KeyBudget):
    /// a missing price does not clear during a retry window, and a client
    /// tells "out of budget" from "a price is missing" by the code.
    ///
    /// `target` names the model and its upstream ("'gpt-5' on upstream
    /// 'OpenAI'"), in one field so the variant stays as small as the others.
    #[error(
        "{scope} has a budget, and {target} has no {missing}, so its spend could not be \
         counted: enter one on the Prices page or with lmgw__price_set (a manual price \
         of 0 is fine for a free upstream, such as one on your LAN){}",
        or_send_default(.or_default)
    )]
    BudgetUnpriced {
        scope: String,
        target: String,
        missing: String,
        or_default: bool,
    },
    /// A rate or concurrency limit. Unlike a budget this *does* clear in
    /// seconds, so it keeps 429 and carries a `Retry-After` that is a real
    /// number rather than a shrug.
    #[error("{key} is over its {limit_kind} limit of {limit} — retry in {retry_after}s")]
    KeyRate {
        key: String,
        limit_kind: &'static str,
        limit: i64,
        retry_after: u64,
    },
    /// The key is past its expiry date. `key` is the key as a refusal names
    /// it ([`crate::config::ApiKey::described`]).
    #[error("{key} expired on {expired_at}")]
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
    /// The OpenAI platform's `context_length_exceeded` also arrives here
    /// when its message states the prompt and the limit (openai egress
    /// design §8); one that does not is
    /// [`ContextExceededAsWorded`](Self::ContextExceededAsWorded).
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
    /// The OpenAI platform's `context_length_exceeded` in a wording that does
    /// not state both the prompt and the limit (openai egress design §8),
    /// e.g. "Your input exceeds the context window of this model." The same
    /// code and status as [`ContextExceeded`](Self::ContextExceeded); the
    /// message is OpenAI's, verbatim, after the model's name. `model` is
    /// empty until the send site's `gate::attribute` names it.
    #[error("'{model}': {message}")]
    ContextExceededAsWorded { model: String, message: String },
    /// An upstream refused for a limit of the account or of the service —
    /// read on the OpenAI platform only, so far, by its egress's `map_error`
    /// (openai egress design §8): a rate limit, an exhausted quota or
    /// spend limit, or no flex capacity — told apart because a client acts on
    /// each differently, and the request log keeps them apart by
    /// [`kind`](Self::kind). A `429`, as OpenAI answers it, on the OpenAI
    /// and Responses wires; on Anthropic's, an exhausted quota is its `402
    /// billing_error` ([`Self::anthropic_status`]). `code` and `error_type`
    /// are OpenAI's own `error.code` and `error.type`, which the OpenAI and
    /// Responses wires send back as OpenAI sent them; `message` its
    /// `error.message`, verbatim. `hints` are the upstream's rate-limit
    /// headers, relayed onto lmgw's answer ([`RateHints`]).
    ///
    /// `note` is lmgw's words after the provider's (§14.8: "'C' has no
    /// fallback configured (tried: A → B → C)"), rendered after the message
    /// on every wire. The optional texts are boxed: this variant sizes
    /// `GatewayError`, which `clippy.toml`'s `large-error-threshold` and
    /// the enums carrying it are measured against.
    #[error("{}{}", upstream_limit_message(.limit, .code, .message), note_text(.note))]
    UpstreamLimited {
        limit: UpstreamLimit,
        code: Option<Box<str>>,
        error_type: Option<Box<str>>,
        message: String,
        hints: RateHints,
        note: Option<Box<str>>,
    },
    /// The model, or its upstream, cannot produce the structured output the
    /// request asked for (structured-output design §7–§9): lmgw's early
    /// refusal on an explicit `false` flag, an egress with no equivalent
    /// (Anthropic's missing JSON-object mode), or an upstream refusal
    /// classified as such. A `400` with the code
    /// `response_format_unsupported` and `param` naming the format field, so
    /// a client can step down to a weaker type without reading the text.
    ///
    /// Not [`Upstream`](Self::Upstream) even when an upstream said it: the
    /// reasoning-off retry, the candidate re-pick and the fallbacks read only
    /// that variant as "maybe something else", and this one never is.
    #[error("{message}")]
    ResponseFormatUnsupported { message: String },
    /// The upstream rejected the request's JSON Schema itself (structured-
    /// output design §9). `message` is the upstream's own, verbatim. A `400`
    /// with the code `invalid_json_schema` and `param` naming the format
    /// field.
    #[error("{message}")]
    InvalidJsonSchema { message: String },
}

/// An upstream's own error object, kept whole for
/// [`GatewayError::UpstreamRelayed`] (boxed there: the error type stays small).
#[derive(Debug, Clone)]
pub struct Relayed {
    pub status: u16,
    pub error_type: Option<String>,
    pub code: Option<String>,
    pub param: Option<String>,
    pub message: String,
}

/// What a failed whole Response said and reported, kept for
/// [`GatewayError::UpstreamFailed`] (boxed there, like [`Relayed`]).
#[derive(Debug, Clone)]
pub struct FailedAnswer {
    /// OpenAI's `error.code` (`server_error`, `invalid_prompt`, ...), when
    /// it sent one.
    pub code: Option<String>,
    /// OpenAI's `error.message`, or lmgw's words when it sent none.
    pub message: String,
    /// The usage the failed Response reported (`Usage::default()` when it
    /// reported none: the send site counts locally then).
    pub usage: crate::ir::Usage,
    /// The tier the failed Response names, which the row is priced at.
    pub service_tier: Option<String>,
}

impl FailedAnswer {
    /// The words a client and the row see: "the upstream's response failed:
    /// <message> (<code>)".
    fn text(&self) -> String {
        match self.code.as_deref() {
            Some(code) => format!("the upstream's response failed: {} ({code})", self.message),
            None => format!("the upstream's response failed: {}", self.message),
        }
    }
}

/// The request field a structured-output error is about, as the OpenAI error
/// body's `param` names it (structured-output design §8): chat completions
/// say `response_format`, legacy completions `json_schema`, `/v1/responses`
/// `text.format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatParam {
    ResponseFormat,
    TextFormat,
    /// Legacy `/v1/completions`, whose llama.cpp spelling of the format is
    /// the top-level `json_schema` field.
    JsonSchema,
}

impl FormatParam {
    fn as_str(self) -> &'static str {
        match self {
            Self::ResponseFormat => "response_format",
            Self::TextFormat => "text.format",
            Self::JsonSchema => "json_schema",
        }
    }
}

/// What a [`GatewayError::GpuDeferred`] was named for — and so what its
/// message says to give a fallback to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferred {
    /// A candidate alias (candidate-aliases design §4.3): the fallback is the
    /// alias's own.
    Alias,
    /// A local model a request named with `x-lmgw-background: on`
    /// (per-request background design §4): the fallback is its row's.
    Model,
}

impl Deferred {
    fn noun(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Model => "model",
        }
    }
}

/// Which of the OpenAI platform's limits a [`GatewayError::UpstreamLimited`]
/// names (openai egress design §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamLimit {
    /// A rate limit (`rate_limit_exceeded`, `slow_down`): clears in seconds.
    Rate,
    /// The account's credit, quota, spend or usage limit is exhausted:
    /// retrying does not help until the owner raises it.
    Quota,
    /// Flex processing had no capacity (`429 Resource Unavailable`); OpenAI
    /// does not charge for it.
    FlexCapacity,
}

impl UpstreamLimit {
    /// The kind the request log groups by: OpenAI's own word for each, as
    /// [`GatewayError::ContextExceeded`] keeps `context_length_exceeded`.
    /// The quota family's several codes (`credit_balance_exhausted`, the
    /// spend and usage limits) collapse into `insufficient_quota`, the
    /// `error.type` OpenAI gives the family; the exact code stays in the
    /// message, and the OpenAI and Responses wires send the upstream's own
    /// code ([`GatewayError::to_openai_json`]). The wire's `code` where the
    /// upstream sent none.
    fn code(self) -> &'static str {
        match self {
            Self::Rate => "rate_limit_exceeded",
            Self::Quota => "insufficient_quota",
            Self::FlexCapacity => "resource_unavailable",
        }
    }
}

/// The headers an upstream's rate-limit answer carries that tell a client
/// when to come back — `Retry-After`, `retry-after-ms` and every
/// `x-ratelimit-*` — copied off the upstream's `429` by
/// [`GatewayError::with_rate_hints`] and onto lmgw's by
/// [`RateHints::stamp`] (openai egress design §8). Names lower-cased, as the
/// upstream's `HeaderMap` holds them; values verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateHints(Box<[(String, String)]>);

impl RateHints {
    /// Whether `name` (lower-case) is one of the relayed headers.
    pub fn relays(name: &str) -> bool {
        name == "retry-after" || name == "retry-after-ms" || name.starts_with("x-ratelimit-")
    }

    /// The relayed headers of `headers`, in their order.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self(
            headers
                .iter()
                .filter(|(name, _)| Self::relays(name.as_str()))
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// When the upstream said to ask again: `retry-after-ms`, or
    /// `retry-after` in seconds or as an HTTP date. `None` when it said
    /// neither, or nothing readable (openai egress design §14.6).
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        let get = |n: &str| {
            self.0
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.trim())
        };
        // Whatever it says, however long: past what a `Duration` holds is
        // `Duration::MAX`, never a panic (review B4) and never a cap of
        // lmgw's own; a negative or non-finite value is unreadable.
        let secs = |s: f64| {
            (s.is_finite() && s >= 0.0).then(|| {
                std::time::Duration::try_from_secs_f64(s).unwrap_or(std::time::Duration::MAX)
            })
        };
        if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok()) {
            return secs(ms / 1000.0);
        }
        let v = get("retry-after")?;
        if let Ok(s) = v.parse::<f64>() {
            return secs(s);
        }
        let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
        let left = at.timestamp() - chrono::Utc::now().timestamp();
        Some(std::time::Duration::from_secs(left.max(0) as u64))
    }

    /// The headers, onto `headers` (an answer's), replacing any of the same
    /// name.
    pub fn stamp(&self, headers: &mut axum::http::HeaderMap) {
        for (name, value) in self.0.iter() {
            match (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => tracing::warn!("cannot relay the upstream's {name} header"),
            }
        }
    }
}

/// [`GatewayError::UpstreamLimited`]'s note, after its message.
fn note_text(note: &Option<Box<str>>) -> String {
    note.as_deref().map(|n| format!(" {n}")).unwrap_or_default()
}

/// [`GatewayError::UpstreamLimited`]'s message: what happened and what helps, in
/// front of OpenAI's own words.
fn upstream_limit_message(limit: &UpstreamLimit, code: &Option<Box<str>>, message: &str) -> String {
    let code = code
        .as_deref()
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    match limit {
        UpstreamLimit::Rate => format!("OpenAI rate limit reached{code}: {message}"),
        UpstreamLimit::Quota => format!(
            "OpenAI quota exhausted{code}: {message} — retrying will not help until the \
             account's credits or limits are raised"
        ),
        UpstreamLimit::FlexCapacity => format!(
            "OpenAI had no flex capacity{code}; not charged — retry, or send service_tier \
             auto: {message}"
        ),
    }
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
         serving on the running rung; retry once they finish, or raise the queue timeout ({})",
        crate::config::QUEUE_TIMEOUT_LABEL
    )
}

/// [`GatewayError::BudgetUnpriced`]'s second way out, when it has one.
fn or_send_default(or_default: &bool) -> &'static str {
    if *or_default {
        ", or send service_tier default"
    } else {
        ""
    }
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
            | Self::ContextExceeded { .. }
            | Self::ContextExceededAsWorded { .. }
            | Self::ResponseFormatUnsupported { .. }
            | Self::InvalidJsonSchema { .. } => StatusCode::BAD_REQUEST,
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
            Self::UpstreamRelayed(r) => StatusCode::from_u16(r.status)
                .ok()
                .filter(|s| s.is_client_error())
                .unwrap_or(StatusCode::BAD_GATEWAY),
            Self::Transport(_)
            | Self::Unreachable(_)
            | Self::UpstreamFailed(_)
            | Self::EngineDownload { .. } => StatusCode::BAD_GATEWAY,
            Self::Timeout | Self::Stalled(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::VramTooLarge { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::GpuHold { .. }
            | Self::GpuBenchmark { .. }
            | Self::GpuSetup { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::KeyScope { .. } | Self::KeyBudget { .. } | Self::BudgetUnpriced { .. } => {
                StatusCode::FORBIDDEN
            }
            Self::KeyRate { .. } | Self::UpstreamLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
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
            Self::Upstream { .. } | Self::UpstreamRelayed(_) | Self::UpstreamFailed(_) => {
                "upstream"
            }
            Self::Transport(_) => "transport",
            Self::Unreachable(_) => "unreachable",
            Self::Timeout | Self::Stalled(_) => "timeout",
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
            Self::GpuSetup { .. } => "gpu_setup",
            Self::EngineDownload { .. } => "engine_download",
            Self::GpuDeferred { .. } => "gpu_deferred",
            Self::CandidateLost { .. } => "candidate_lost",
            Self::CandidateUnavailable { .. } => "candidate_unavailable",
            Self::KeyScope { .. } => "key_scope",
            Self::KeyBudget { .. } => "key_budget",
            Self::BudgetUnpriced { .. } => "budget_unpriced",
            Self::KeyRate { .. } => "key_rate",
            Self::UpstreamLimited { limit, .. } => limit.code(),
            Self::KeyExpired { .. } => "key_expired",
            // The one code both dialects' `code`/error-shape render as
            // `context_length_exceeded` — see `to_openai_json`, which reuses
            // this same `kind()` for its `code` field.
            Self::ContextExceeded { .. } | Self::ContextExceededAsWorded { .. } => {
                "context_length_exceeded"
            }
            Self::ResponseFormatUnsupported { .. } => "response_format_unsupported",
            Self::InvalidJsonSchema { .. } => "invalid_json_schema",
        }
    }

    /// The HTTP status an upstream answered with, for the errors that carry
    /// one: [`Upstream`](Self::Upstream) and the relayed
    /// [`UpstreamRelayed`](Self::UpstreamRelayed). The one reading for a
    /// classification that goes by the upstream's status.
    pub fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::Upstream { status, .. } => Some(*status),
            Self::UpstreamRelayed(r) => Some(r.status),
            _ => None,
        }
    }

    /// The upstream's own message, for the same two variants as
    /// [`upstream_status`](Self::upstream_status).
    pub fn upstream_message(&self) -> Option<&str> {
        match self {
            Self::Upstream { message, .. } => Some(message),
            Self::UpstreamRelayed(r) => Some(&r.message),
            _ => None,
        }
    }

    /// What a failed whole Response reported, for the row that counts it
    /// ([`UpstreamFailed`](Self::UpstreamFailed)); `None` for every other
    /// error, whose row has no usage.
    pub fn failed_answer(&self) -> Option<&FailedAnswer> {
        match self {
            Self::UpstreamFailed(f) => Some(f),
            _ => None,
        }
    }

    /// The error as `request_logs.error_message` keeps it: its text, with a
    /// relayed upstream error led by the same `upstream error (NNN): ` as
    /// [`Upstream`](Self::Upstream)'s, so the status survives in the log.
    /// The client's body keeps the bare message ([`Display`](std::fmt::Display)).
    pub fn log_text(&self) -> String {
        match self {
            Self::UpstreamRelayed(r) => format!("upstream error ({}): {}", r.status, r.message),
            other => other.to_string(),
        }
    }

    /// Whether the error is about the request's structured output — the
    /// errors whose OpenAI body names a `param` (structured-output design §8).
    pub fn about_format(&self) -> bool {
        matches!(
            self,
            Self::ResponseFormatUnsupported { .. } | Self::InvalidJsonSchema { .. }
        )
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
            | Self::ContextExceeded { .. }
            | Self::ContextExceededAsWorded { .. }
            | Self::ResponseFormatUnsupported { .. }
            | Self::InvalidJsonSchema { .. } => "invalid_request_error",
            Self::Unauthorized(_) | Self::AgentDisabled { .. } => "authentication_error",
            Self::Refused { status, .. } => match status {
                401 => "authentication_error",
                501 => "invalid_request_error",
                _ => "permission_error",
            },
            Self::UpstreamRelayed(_) => "invalid_request_error",
            // By the upstream's status: its refusal of the request is the
            // client's request error, its rate limit a rate limit; its own
            // failure, and its refusal of lmgw's key (a 502 here), lmgw's.
            Self::Upstream { status, .. } => match status {
                400 | 404 | 413 | 422 => "invalid_request_error",
                429 => "rate_limit_error",
                _ => "api_error",
            },
            Self::UpstreamFailed(_)
            | Self::Transport(_)
            | Self::Unreachable(_)
            | Self::Timeout
            | Self::Stalled(_)
            | Self::VramTooLarge { .. }
            | Self::VramQueueTimeout { .. }
            | Self::KvPoolTimeout { .. }
            | Self::LadderDrainTimeout { .. }
            | Self::Internal(_)
            | Self::GpuHold { .. }
            | Self::GpuBenchmark { .. }
            | Self::GpuSetup { .. }
            | Self::EngineDownload { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => "api_error",
            Self::KeyScope { .. }
            | Self::KeyBudget { .. }
            | Self::BudgetUnpriced { .. }
            | Self::KeyExpired { .. } => "permission_error",
            Self::KeyRate { .. } => "rate_limit_error",
            Self::UpstreamLimited { limit, .. } => match limit {
                UpstreamLimit::Quota => "insufficient_quota",
                UpstreamLimit::Rate | UpstreamLimit::FlexCapacity => "rate_limit_error",
            },
        }
    }

    /// Anthropic error `type` field.
    pub fn anthropic_type(&self) -> &'static str {
        match self {
            Self::UnknownAlias(_) | Self::NotFound(_) => "not_found_error",
            Self::Unauthorized(_) | Self::AgentDisabled { .. } => "authentication_error",
            Self::Refused { status, .. } => match status {
                401 => "authentication_error",
                501 => "invalid_request_error",
                _ => "permission_error",
            },
            Self::BadRequest(_)
            | Self::InvalidRequest { .. }
            | Self::BodyTooLarge { .. }
            | Self::Unsupported(_)
            | Self::ContextExceeded { .. }
            | Self::ContextExceededAsWorded { .. }
            | Self::ResponseFormatUnsupported { .. }
            | Self::InvalidJsonSchema { .. } => "invalid_request_error",
            Self::Upstream { status, .. } => upstream_anthropic_type(*status),
            Self::Timeout
            | Self::Stalled(_)
            | Self::UpstreamFailed(_)
            | Self::Transport(_)
            | Self::Unreachable(_)
            | Self::EngineDownload { .. } => "api_error",
            // The nearest of Anthropic's types to what OpenAI said, by its
            // type first and its status after.
            Self::UpstreamRelayed(r) => match (r.error_type.as_deref(), r.status) {
                (Some("authentication_error"), _) => "authentication_error",
                (Some("permission_error"), _) => "permission_error",
                (Some("rate_limit_error"), _) => "rate_limit_error",
                (Some("insufficient_quota"), _) => "billing_error",
                (Some("not_found_error"), _) | (_, 404) => "not_found_error",
                (_, 413) => "request_too_large",
                _ => "invalid_request_error",
            },
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
            | Self::GpuSetup { .. }
            | Self::GpuDeferred { .. }
            | Self::CandidateLost { .. }
            | Self::CandidateUnavailable { .. } => "api_error",
            Self::KeyScope { .. } | Self::KeyBudget { .. } | Self::BudgetUnpriced { .. } => {
                "permission_error"
            }
            Self::KeyExpired { .. } => "authentication_error",
            Self::KeyRate { .. } => "rate_limit_error",
            // Anthropic's word for an account out of credit; a rate limit and
            // the flex 429 are its rate_limit_error, as the status says.
            Self::UpstreamLimited { limit, .. } => match limit {
                UpstreamLimit::Quota => "billing_error",
                UpstreamLimit::Rate | UpstreamLimit::FlexCapacity => "rate_limit_error",
            },
        }
    }

    /// The status on Anthropic's wire: [`Self::http_status`], except that an
    /// exhausted quota is Anthropic's `402 billing_error` — "There's an issue
    /// with your billing or payment information" — and not a `429`, which
    /// Anthropic's SDKs retry ("The official SDK automatically retries
    /// transient failures (such as connection errors, rate limits, and 5xx
    /// server errors)"); <https://platform.claude.com/docs/en/api/errors>,
    /// checked 2026-10-24. A rate limit and flex's missing capacity stay its
    /// `429 rate_limit_error`.
    pub fn anthropic_status(&self) -> StatusCode {
        match self {
            Self::UpstreamLimited {
                limit: UpstreamLimit::Quota,
                ..
            } => StatusCode::PAYMENT_REQUIRED,
            _ => self.http_status(),
        }
    }

    /// The upstream's rate-limit headers this error carries, to relay onto
    /// the answer ([`RateHints`]); none for every other error.
    pub fn rate_hints(&self) -> Option<&RateHints> {
        match self {
            Self::UpstreamLimited { hints, .. } if !hints.is_empty() => Some(hints),
            _ => None,
        }
    }

    /// `self` with the rate-limit headers of the upstream answer it was read
    /// from: an [`UpstreamLimited`](Self::UpstreamLimited) takes them, every
    /// other error is returned as it is.
    pub fn with_rate_hints(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        if let Self::UpstreamLimited { hints, .. } = &mut self {
            *hints = RateHints::from_headers(headers);
        }
        self
    }

    /// `self` with lmgw's `clause` after its words (openai egress design
    /// §14.8: " (fallback 'B' is disabled) (tried: A → B)"): an exhausted
    /// quota's [`note`](Self::UpstreamLimited), the text of a variant that
    /// is one, the message of an upstream's error. A variant whose words are
    /// fixed is returned as it is.
    pub fn with_clause(self, clause: &str) -> Self {
        let clause = clause.trim_start();
        if clause.is_empty() {
            return self;
        }
        let add = |s: String| format!("{s} {clause}");
        match self {
            Self::UpstreamLimited {
                limit,
                code,
                error_type,
                message,
                hints,
                note,
            } => Self::UpstreamLimited {
                limit,
                code,
                error_type,
                message,
                hints,
                note: Some(match note {
                    Some(n) => format!("{n} {clause}").into(),
                    None => clause.into(),
                }),
            },
            Self::GpuSetup {
                model,
                problem,
                detail,
            } => Self::GpuSetup {
                model,
                problem,
                detail: add(detail),
            },
            Self::EngineDownload { message } => Self::EngineDownload {
                message: add(message),
            },
            Self::UnknownAlias(s) => Self::UnknownAlias(add(s)),
            Self::Internal(s) => Self::Internal(add(s)),
            Self::Transport(s) => Self::Transport(add(s)),
            Self::Unreachable(s) => Self::Unreachable(add(s)),
            Self::BadRequest(s) => Self::BadRequest(add(s)),
            Self::Unsupported(s) => Self::Unsupported(add(s)),
            Self::Upstream {
                status,
                message,
                provider_type,
            } => Self::Upstream {
                status,
                message: add(message),
                provider_type,
            },
            other => other,
        }
    }

    /// [`Self::rate_hints`] onto an answer built from this error.
    pub fn stamp_rate_hints(&self, resp: &mut axum::response::Response) {
        if let Some(hints) = self.rate_hints() {
            hints.stamp(resp.headers_mut());
        }
    }

    /// OpenAI-shaped error body. Its `code` is [`Self::kind`], the word the
    /// request log records — for every variant, a deferral's `gpu_deferred`
    /// included (per-request background design §8) — except an
    /// [`UpstreamLimited`](Self::UpstreamLimited), whose `code` and `type`
    /// are the upstream's own as it sent them (`credit_balance_exhausted`,
    /// `tokens`, …), the family word only where it sent none. `param` names
    /// `response_format` for a structured-output error, `null` otherwise
    /// (structured-output design §8).
    pub fn to_openai_json(&self) -> serde_json::Value {
        self.openai_json(FormatParam::ResponseFormat)
    }

    /// [`Self::to_openai_json`] for `/v1/responses`, whose structured output
    /// is `text.format` (structured-output design §8).
    pub fn to_responses_json(&self) -> serde_json::Value {
        self.openai_json(FormatParam::TextFormat)
    }

    /// [`Self::to_openai_json`] with a format error's `param` naming
    /// `format`: legacy `/v1/completions` says the field the client sent,
    /// `json_schema` or `response_format`.
    pub fn to_openai_json_naming(&self, format: FormatParam) -> serde_json::Value {
        self.openai_json(format)
    }

    fn openai_json(&self, format: FormatParam) -> serde_json::Value {
        let param = self.about_format().then(|| format.as_str());
        let (kind, code) = match self {
            Self::UpstreamLimited {
                code, error_type, ..
            } => (
                error_type.as_deref().unwrap_or(self.openai_type()),
                code.as_deref().unwrap_or(self.kind()),
            ),
            // OpenAI's own code, as a stream's `response.failed` names it.
            Self::UpstreamFailed(f) => {
                (self.openai_type(), f.code.as_deref().unwrap_or(self.kind()))
            }
            Self::UpstreamRelayed(r) => {
                return serde_json::json!({
                    "error": {
                        "message": self.to_string(),
                        "type": r.error_type.as_deref().unwrap_or(self.openai_type()),
                        "param": r.param,
                        "code": r.code,
                    }
                });
            }
            _ => (self.openai_type(), self.kind()),
        };
        serde_json::json!({
            "error": {
                "message": self.to_string(),
                "type": kind,
                "param": param,
                "code": code,
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

/// Anthropic's error `type` for an upstream's error answer of `status`
/// (<https://platform.claude.com/docs/en/api/errors>): what the client is
/// answered by [`GatewayError::http_status`], in Anthropic's words. A `401`
/// or `403` is lmgw's own upstream key refused, answered `502`: an
/// `api_error`, never the client's `authentication_error`.
fn upstream_anthropic_type(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        // Only an upstream's own 529 is "overloaded": a 503 is answered 502
        // (`http_status`), and a 502 is an `api_error`.
        529 => "overloaded_error",
        _ => "api_error",
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

    /// `GatewayError` rides in the gate's refusal shapes, which
    /// `clippy.toml`'s `large-error-threshold` (224) measures, and in enums
    /// beside smaller variants: a new field boxes its text (openai egress
    /// design §14.8, `UpstreamLimited.note`). 104 bytes as WP10 left it.
    #[test]
    fn the_error_stays_small() {
        let size = std::mem::size_of::<GatewayError>();
        assert!(size <= 104, "{size}");
    }

    /// The note of an exhausted quota comes after the provider's words, on
    /// every wire; an unreachable upstream is a 502 of its own kind.
    #[test]
    fn a_quota_note_follows_the_message_and_unreachable_has_its_kind() {
        let e = GatewayError::UpstreamLimited {
            limit: UpstreamLimit::Quota,
            code: Some("insufficient_quota".into()),
            error_type: Some("insufficient_quota".into()),
            message: "You exceeded your current quota.".into(),
            hints: Default::default(),
            note: Some("'C' has no fallback configured (tried: A → B → C)".into()),
        };
        let text = e.to_string();
        assert!(
            text.ends_with("raised 'C' has no fallback configured (tried: A → B → C)"),
            "{text}"
        );
        assert_eq!(e.to_openai_json()["error"]["message"], text);
        let u = GatewayError::Unreachable("tcp connect error".into());
        assert_eq!(
            (u.http_status().as_u16(), u.kind(), u.openai_type()),
            (502, "unreachable", "api_error")
        );
        assert_eq!(u.to_string(), "upstream unreachable: tcp connect error");
    }

    /// An upstream's error answer types by its status in both dialects
    /// (api-compat design §9 finding 7): a refused request is the client's
    /// `invalid_request_error`, not lmgw's `api_error`; lmgw's own key
    /// refused (a 502 to the client) and the upstream's failure stay
    /// `api_error`.
    #[test]
    fn an_upstream_error_types_by_its_status() {
        let up = |status| GatewayError::Upstream {
            status,
            provider_type: Some("whatever_error".into()),
            message: "m".into(),
        };
        let cases = [
            (400, "invalid_request_error", "invalid_request_error"),
            (401, "api_error", "api_error"),
            (403, "api_error", "api_error"),
            (404, "invalid_request_error", "not_found_error"),
            (413, "invalid_request_error", "request_too_large"),
            (422, "invalid_request_error", "invalid_request_error"),
            (429, "rate_limit_error", "rate_limit_error"),
            (500, "api_error", "api_error"),
            // Answered 502, so an `api_error`, never an overloaded one.
            (503, "api_error", "api_error"),
            (529, "api_error", "overloaded_error"),
        ];
        for (status, openai, anthropic) in cases {
            let e = up(status);
            assert_eq!(e.openai_type(), openai, "{status}");
            assert_eq!(
                e.to_anthropic_json()["error"]["type"],
                anthropic,
                "{status}"
            );
        }
    }

    /// A refusal's status is its layer's: `/v1/systemone`'s 501 reads as a
    /// request problem in both dialects, not as a permission one, and a
    /// capability refusal keeps its own type (decision-models design §2.2).
    #[test]
    fn a_refusal_types_by_its_status() {
        let refused = |status| GatewayError::Refused {
            status,
            code: "not_supported",
            message: "m".into(),
        };
        let types = |e: &GatewayError| {
            (
                e.http_status().as_u16(),
                e.openai_type(),
                e.anthropic_type(),
            )
        };
        assert_eq!(
            types(&refused(501)),
            (501, "invalid_request_error", "invalid_request_error")
        );
        assert_eq!(
            types(&refused(403)),
            (403, "permission_error", "permission_error")
        );
        assert_eq!(
            types(&refused(401)),
            (401, "authentication_error", "authentication_error")
        );
        let body = refused(501).to_openai_json();
        assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
        assert_eq!(body["error"]["code"], "not_supported", "{body}");
    }

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

    /// A background deferral is a 503 with its own code on the wire and in
    /// the log — `gpu_deferred`, never the hold's `gpu_hold` and never
    /// `overloaded_error` (per-request background design §8) — and says what
    /// to give a fallback to.
    #[test]
    fn a_deferral_answers_and_logs_its_own_code() {
        let e = GatewayError::GpuDeferred {
            model: "jobs".into(),
            detail: "GPU in use by chat/qwen (18.0 GiB, 1 in flight)".into(),
            on: Deferred::Alias,
        };
        assert_eq!(e.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.kind(), "gpu_deferred");
        let openai = e.to_openai_json();
        assert_eq!(openai["error"]["code"], "gpu_deferred");
        assert_eq!(openai["error"]["type"], "api_error");
        assert_eq!(e.to_anthropic_json()["error"]["type"], "api_error");
        let msg = e.to_string();
        assert!(
            msg.starts_with("'jobs' deferred: GPU in use by chat/qwen"),
            "{msg}"
        );
        assert!(
            msg.ends_with("retry later, or give the alias a fallback"),
            "{msg}"
        );
        let model = GatewayError::GpuDeferred {
            model: "qwen".into(),
            detail: "GPU in use by the owner".into(),
            on: Deferred::Model,
        };
        assert!(
            model
                .to_string()
                .ends_with("retry later, or give the model a fallback"),
            "{model}"
        );

        let lost = GatewayError::CandidateLost {
            model: "alt".into(),
            detail: "an alternate is never started by its alias".into(),
        };
        assert_eq!(lost.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(lost.kind(), "candidate_lost");
        assert_eq!(lost.to_openai_json()["error"]["code"], "candidate_lost");
    }

    /// A start the machine check blames on the computer (machine-check
    /// design §2.5) is a 503 with its own code, `api_error` in both dialects
    /// (never `overloaded_error`: no retry window sets a computer up), and a
    /// failure in the usage rollup, not a refusal; the plain sentence comes
    /// first and a fallback's clause after the start's own words. A failed
    /// engine download keeps WP10's 502 with a code of its own.
    #[test]
    fn a_machine_not_ready_and_a_failed_download_answer_their_own_codes() {
        let e = GatewayError::GpuSetup {
            model: "qwen".into(),
            problem: "could not start on the graphics card of the computer lmgw runs on. \
                      Restart the computer to finish the graphics driver update."
                .into(),
            detail: " (its start: podman run failed)".into(),
        };
        assert_eq!(e.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.kind(), "gpu_setup");
        assert_eq!(e.to_openai_json()["error"]["code"], "gpu_setup");
        assert_eq!(e.to_openai_json()["error"]["type"], "api_error");
        assert_eq!(e.to_anthropic_json()["error"]["type"], "api_error");
        assert_eq!(
            crate::telemetry::outcome_of(503, Some(e.kind())),
            "upstream_error"
        );
        let e = e.with_clause(" (fallback 'cloud' is disabled) (tried: qwen → cloud)");
        assert_eq!(
            e.to_string(),
            "'qwen' could not start on the graphics card of the computer lmgw runs on. Restart \
             the computer to finish the graphics driver update. (its start: podman run failed) \
             (fallback 'cloud' is disabled) (tried: qwen → cloud)"
        );

        let d = GatewayError::EngineDownload {
            message: "The engine chat model 'qwen' runs in could not be downloaded, so the model \
                      could not start: its download failed: x."
                .into(),
        };
        assert_eq!(d.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(d.kind(), "engine_download");
        assert_eq!(d.to_anthropic_json()["error"]["type"], "api_error");
        assert_eq!(
            d.with_clause(" (tried: qwen → cloud)").to_string(),
            "The engine chat model 'qwen' runs in could not be downloaded, so the model could \
             not start: its download failed: x. (tried: qwen → cloud)"
        );
    }

    /// A relayed upstream error reads its status and message like a plain
    /// one, and its log text leads with the status while the client's body
    /// keeps the bare message.
    #[test]
    fn a_relayed_error_reads_and_logs_like_a_plain_upstream_one() {
        let relayed = GatewayError::UpstreamRelayed(Box::new(Relayed {
            status: 404,
            error_type: Some("invalid_request_error".into()),
            code: Some("model_not_found".into()),
            param: None,
            message: "The model 'x' does not exist".into(),
        }));
        let plain = GatewayError::Upstream {
            status: 404,
            provider_type: None,
            message: "The model 'x' does not exist".into(),
        };
        assert_eq!(relayed.upstream_status(), Some(404));
        assert_eq!(relayed.upstream_message(), plain.upstream_message());
        assert_eq!(relayed.log_text(), plain.to_string());
        assert_eq!(
            relayed.to_openai_json()["error"]["message"],
            "The model 'x' does not exist"
        );
        assert_eq!(GatewayError::Timeout.upstream_status(), None);
    }

    /// The two structured-output codes (structured-output design §8): a 400
    /// `invalid_request_error` in both dialects, `param` naming the format
    /// field per route, the message as given; every other error keeps
    /// `param: null`.
    #[test]
    fn a_format_error_names_its_param_per_route() {
        for (e, code) in [
            (
                GatewayError::ResponseFormatUnsupported {
                    message: "no json_object here".into(),
                },
                "response_format_unsupported",
            ),
            (
                GatewayError::InvalidJsonSchema {
                    message: "JSON schema conversion failed:\nx".into(),
                },
                "invalid_json_schema",
            ),
        ] {
            assert_eq!(e.http_status(), StatusCode::BAD_REQUEST);
            let chat = e.to_openai_json();
            assert_eq!(chat["error"]["code"], code);
            assert_eq!(chat["error"]["type"], "invalid_request_error");
            assert_eq!(chat["error"]["param"], "response_format");
            assert_eq!(chat["error"]["message"], e.to_string());
            assert_eq!(e.to_responses_json()["error"]["param"], "text.format");
            assert_eq!(
                e.to_openai_json_naming(FormatParam::JsonSchema)["error"]["param"],
                "json_schema"
            );
            let anthropic = e.to_anthropic_json();
            assert_eq!(anthropic["error"]["type"], "invalid_request_error");
            assert!(anthropic["error"].get("param").is_none());
        }
        let other = GatewayError::BadRequest("x".into());
        assert!(other.to_openai_json()["error"]["param"].is_null());
        assert!(other.to_responses_json()["error"]["param"].is_null());
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
