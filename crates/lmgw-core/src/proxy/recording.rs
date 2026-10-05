//! Per-request context, header constants, and the shared record/pricing/
//! policy helpers behind the public handlers' `record`/`LogParams` logging
//! path (usage-analytics §2/§4).

use std::time::Instant;

use axum::http::header;
use axum::response::{IntoResponse, Response};

use crate::config::{Route, Snapshot};
use crate::error::GatewayError;
use crate::gate::{FallbackReason, GateHeaders};
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, Timings, Usage};
use crate::pricing::{self, TokenUsage};
use crate::state::{AppState, SharedState};
use crate::store::{self, NewRequestLog};
use crate::telemetry::{internal_identity, RequestClass, RequestSummary};

/// Per-request context filled by the root middleware / handlers.
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    /// Who is making this request (principals §3.5), resolved once at the
    /// router root. `Principal::key_id` is the **identity** — the `api_keys.id`
    /// a rename or a delete does not orphan — and `client_key` below is the
    /// label Logs prints beside it.
    pub principal: crate::principal::Principal,
    /// Name of the gateway API key used (never the key itself).
    pub client_key: Option<String>,
    /// The `x-lmgw-reasoning*` headers as parsed by the middleware (§5.2) —
    /// the highest tier, merged field-wise over whatever the body asked for.
    ///
    /// Held as a **`Result`, parsed but not yet judged**: the middleware sees
    /// every `/v1` route, and only the four that consume a reasoning control
    /// (`/v1/chat/completions`, `/v1/messages`, `/v1/messages/count_tokens`,
    /// `/v1/responses`) may refuse a request over one. `GET /v1/models` with a typo'd header is still a
    /// perfectly good models request, and turning it into a 400 in the
    /// middleware would invent a failure mode the header does not have there.
    pub reasoning: Option<Result<crate::ir::ReasoningControl, String>>,
    /// The client's `anthropic-beta` flags
    /// ([`client_betas`](crate::egress::anthropic::client_betas)). Read, like
    /// `reasoning`, by every route but used only by the chat-shaped ones,
    /// which put them on the IR ([`crate::ir::ChatRequest::anthropic_beta`]),
    /// and by `/v1/messages/count_tokens`.
    pub anthropic_beta: Vec<String>,
    /// The calling agent, when the bearer resolved to a `kind = 'agent'` row
    /// (container-runtime §3.1). Set **regardless of `auth_enabled`**: an agent
    /// token is a capability, not a gateway credential, and honouring it only
    /// when the owner happens to have switched auth on would make its scope
    /// decorative.
    pub agent: Option<crate::agents::AgentIdentity>,
    /// `X-Lmgw-Run: <job id>` — which run this request belongs to, so its cost
    /// lands on that run's total (§3.1). A header rather than a token claim,
    /// because one token serves every run. An unknown or foreign id is dropped
    /// with a log line in the middleware and never reaches here: a mis-stamped
    /// request is still a request the owner made, so it is never a refusal.
    pub run: Option<i64>,
}

impl RequestCtx {
    /// The header-tier control, or `None` when absent — or malformed, which
    /// only happens on a route that does not consume it (the consuming routes
    /// reject it first, via [`reasoning_error`](Self::reasoning_error)).
    pub fn reasoning_control(&self) -> Option<crate::ir::ReasoningControl> {
        self.reasoning.as_ref()?.as_ref().ok().cloned()
    }

    /// Why the reasoning headers could not be parsed, for the routes that must
    /// answer 400 rather than guess.
    pub fn reasoning_error(&self) -> Option<&str> {
        self.reasoning.as_ref()?.as_ref().err().map(String::as_str)
    }

    /// The key this request is charged to: the name it authenticated with,
    /// and the principal's `api_keys.id` — which every row is resolved by
    /// when it is written ([`KeyRef`]).
    pub(crate) fn key_ref(&self) -> KeyRef {
        KeyRef {
            name: self.client_key.clone(),
            id: self.principal.key_id(),
        }
    }

    /// The key's name **now**: a request can outlive a rename (a realtime
    /// session lasts as long as its socket), and the key keeps its id but
    /// not its name. The captured name when the request has no key, or the
    /// key is gone.
    pub(crate) fn current_key_name(&self, snap: &Snapshot) -> Option<String> {
        self.key_ref().resolve(snap).0
    }
}

/// Whom a logged call is charged to (usage-analytics §2.2): the key's name
/// as the caller captured it and — for a caller acting for an authenticated
/// key — the key's `api_keys.id`.
///
/// **The id decides, at record time** (realtime design §11, package A review
/// #2). A call can outlive a rename of its key, a realtime session's every
/// call can; looked up by the name captured at the handshake, the renamed
/// key matches no row — the row's `key_id` is NULL, its budget and
/// tokens/minute never see the call, Usage by key loses it — and a new key
/// created under the old name is charged instead. So a row with an id
/// carries that key's id and its current name; one without is resolved by
/// name, as it always was (the in-process callers that only have a label,
/// the internal identities).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyRef {
    pub name: Option<String>,
    pub id: Option<i64>,
}

impl KeyRef {
    /// A caller known by its label only: resolved by name, as before.
    pub fn named(name: Option<String>) -> Self {
        Self { name, id: None }
    }

    /// The row's `client_key` label, and the id when the caller had one. A
    /// key deleted since keeps its id (a delete does not orphan history,
    /// `NewRequestLog::key_id`) and the name it was last seen under.
    fn resolve(&self, snap: &Snapshot) -> (Option<String>, Option<i64>) {
        let Some(id) = self.id else {
            return (self.name.clone(), None);
        };
        let now = snap
            .api_keys
            .iter()
            .find(|k| k.id == id)
            .map(|k| k.name.clone());
        (now.or_else(|| self.name.clone()), Some(id))
    }
}

pub(super) struct LogParams<'a> {
    pub(super) state: &'a SharedState,
    pub(super) proto: ClientProto,
    pub(super) ctx: &'a RequestCtx,
    pub(super) alias: String,
    pub(super) route: Option<&'a Route>,
    pub(super) started: Instant,
    pub(super) streamed: bool,
    /// What kind of work this was (usage-analytics §2.1). Implicit in which
    /// handler is calling until it is written down, and an embed request
    /// averaged into "tokens per chat turn" makes both numbers meaningless.
    pub(super) class: RequestClass,
    /// llama.cpp's own measurements, when the upstream reported them.
    pub(super) timings: Option<Timings>,
    /// [`Annotations::max_tokens_clamped`] for this call, logged next to the
    /// header (ladder design §3.2, unified-KV design §3.3 step 1): the send's
    /// [`crate::gate::TurnLease::max_tokens_clamped`], `None` for anything
    /// that never reached the gate's per-send half.
    pub(super) max_tokens_clamped: Option<u32>,
    /// Why a fallback answered instead of the requested local model — the
    /// request's [`GateHeaders::fallback_reason`], logged next to the header
    /// (candidate-aliases design §4.7). `None` before the gate settled a
    /// route, and whenever no fallback answered.
    pub(super) fallback: Option<FallbackReason>,
    /// The rung a ladder row answered from, 1-based (ladder design §6,
    /// `request_logs.rung`, migration 0043): the send's
    /// [`crate::gate::TurnLease::rung_log`] — the rung that served it, or the
    /// one it was refused on. `None` for every row without a ladder, for a
    /// request a fallback answered, and for anything that never reached the
    /// gate's per-send half.
    pub(super) rung: Option<i64>,
}

pub(super) async fn record(
    p: LogParams<'_>,
    status: u16,
    ttfb_ms: Option<i64>,
    usage: Usage,
    error: Option<(&str, String)>,
) {
    let total_ms = p.started.elapsed().as_millis() as i64;
    let priced = price_call(
        p.state,
        &p.alias,
        p.route.map(|r| r.upstream.id),
        p.route.map(|r| r.upstream_model.as_str()),
        &p.ctx.key_ref(),
        p.proto.as_str(),
        &usage,
    );
    // Cost per run, for a caller lmgw did not drive (§3.1). No `run_id` column
    // on `request_logs`: the job row is already the durable home of a run's
    // totals and a second one would drift.
    //
    // Only a call that **reached an upstream and came back** counts, which is
    // what the in-process `batch::Meter::note_model_call` counts: an unknown
    // alias never routed anywhere, and a transport failure never got an answer.
    // Folding those in would make `model_calls` a count of HTTP requests under
    // a name that promises model turns, and a run that mistyped its alias forty
    // times would read as forty model calls that cost nothing.
    if let Some(run) = p.ctx.run.filter(|_| p.route.is_some() && error.is_none()) {
        p.state
            .agent_meters
            .note_model_call(run, &usage, priced.cost.total_micro);
    }
    let row = NewRequestLog {
        client_key: priced.client_key,
        ingress_proto: p.proto.as_str().to_string(),
        requested_alias: p.alias.clone(),
        upstream_id: p.route.map(|r| r.upstream.id),
        upstream_name: p.route.map(|r| r.upstream.name.clone()),
        upstream_model: p.route.map(|r| r.upstream_model.clone()),
        // LLM traffic never names an MCP tool; the `tools/call` path sets this.
        mcp_tool: None,
        egress_proto: p.route.map(|r| r.upstream.protocol.as_str().to_string()),
        status: status as i64,
        ttfb_ms,
        total_ms: Some(total_ms),
        prompt_tokens: usage.prompt_tokens.map(|v| v as i64),
        completion_tokens: usage.completion_tokens.map(|v| v as i64),
        streamed: p.streamed,
        error_kind: error.as_ref().map(|(k, _)| k.to_string()),
        error_msg: error.as_ref().map(|(_, m)| m.clone()),
        key_id: priced.key_id,
        class: p.class,
        cached_in_tokens: usage.cached_input_tokens.map(|v| v as i64),
        cache_write_tokens: usage.cache_write_tokens.map(|v| v as i64),
        reasoning_tokens: usage.reasoning_tokens.map(|v| v as i64),
        cost: priced.cost,
        timings: p.timings,
        max_tokens_clamped: p.max_tokens_clamped,
        fallback_reason: p.fallback.map(|r| r.as_str().to_string()),
        rung: p.rung,
    };
    let log_id = store::insert_request_log(&p.state.db, &row)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to write request log: {e}");
            0
        });
    p.state.telemetry.request_finished(RequestSummary {
        log_id,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client_key: row.client_key,
        ingress_proto: row.ingress_proto,
        requested_alias: row.requested_alias,
        upstream_name: row.upstream_name,
        upstream_model: row.upstream_model,
        egress_proto: row.egress_proto,
        status,
        ttfb_ms: row.ttfb_ms,
        total_ms: row.total_ms,
        prompt_tokens: row.prompt_tokens,
        completion_tokens: row.completion_tokens,
        cached_in_tokens: row.cached_in_tokens,
        cache_write_tokens: row.cache_write_tokens,
        streamed: row.streamed,
        error_kind: row.error_kind,
        error_msg: row.error_msg,
        cost_micro: row.cost.total_micro,
        class: row.class.as_str().to_string(),
        key_id: row.key_id,
        fallback_reason: row.fallback_reason,
        rung: row.rung,
    });
}

/// Identity + price for one logged call (usage-analytics §2.2, §4.4).
///
/// One helper for every record path, so "what did this cost" and "who was this"
/// are answered the same way for a public request, an in-process chat turn and
/// a corpus ingest — three copies of this would drift within a month.
pub(super) struct Priced {
    /// The row's `client_key` label: the key's name at record time
    /// ([`KeyRef`]).
    pub(super) client_key: Option<String>,
    pub(super) key_id: Option<i64>,
    pub(super) cost: pricing::Cost,
}

pub(super) fn price_call(
    state: &AppState,
    alias: &str,
    upstream_id: Option<i64>,
    upstream_model: Option<&str>,
    key: &KeyRef,
    ingress_proto: &str,
    usage: &Usage,
) -> Priced {
    let snap = state.snapshot();
    let (client_key, known) = key.resolve(&snap);
    // The key's identity when the caller had one (`KeyRef`); otherwise by
    // name — a real key's when the caller authenticated, else the synthetic
    // identity for whichever internal consumer this is.
    let key_id = known.or_else(|| {
        client_key
            .as_deref()
            .or_else(|| internal_identity(ingress_proto))
            .and_then(|n| snap.key_id_for_name(n))
    });
    // Fold this call into the rolling windows the policy gate enforces on, so
    // a budget check never has to run a SUM on the request path and a
    // tokens/minute limit sees the tokens that were actually spent. A key
    // deleted while its request ran has no window left to enforce, and a
    // fresh one for it would never be read again (A2 review 3).
    if let Some(id) = key_id.filter(|id| snap.api_keys.iter().any(|k| k.id == *id)) {
        let tokens = usage.prompt_tokens.unwrap_or(0) + usage.completion_tokens.unwrap_or(0);
        state.policy.note_tokens(id, tokens as i64);
    }

    let detail = TokenUsage {
        prompt: usage.prompt_tokens,
        completion: usage.completion_tokens,
        cached_in: usage.cached_input_tokens,
        cache_write: usage.cache_write_tokens,
        reasoning: usage.reasoning_tokens,
    };
    let prices = snap.prices_for(alias, upstream_id, upstream_model);
    let cost = pricing::price_request(&detail, prices.as_ref());

    // Advance the cached spend the budget check reads. Unpriced calls add
    // nothing — which is the honest answer, and the reason a budget cannot be
    // enforced against traffic whose price nobody knows.
    if let Some(micro) = cost.total_micro.filter(|m| *m != 0) {
        let now = chrono::Utc::now();
        let key_period = key_id.and_then(|id| {
            snap.api_keys
                .iter()
                .find(|k| k.id == id)
                .map(|k| k.policy.budget_period.start_hour_key(now))
        });
        let global_period = snap.settings.global_budget_period.start_hour_key(now);
        state
            .policy
            .note_spend(key_id.zip(key_period.as_deref()), &global_period, micro);
    }

    Priced {
        client_key,
        key_id,
        cost,
    }
}

/// Scope + budget for one alias, logged and answered in the caller's dialect.
///
/// The two checks that cannot live in the auth middleware, because the alias
/// they are about is in the request body (usage-analytics §4.2). Returns the
/// finished refusal response when the key may not proceed, so a handler's line
/// is `if let Some(r) = policy_or_refuse(..).await { return r; }`.
pub(super) async fn policy_or_refuse(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
    started: Instant,
    class: RequestClass,
) -> Option<Response> {
    let snap = state.snapshot();
    let e = crate::policy::check_alias(
        &state.policy,
        &state.db,
        &snap,
        ctx.client_key.as_deref(),
        alias,
    )
    .await
    .err()?;
    record_refusal(state, proto, ctx, alias, started, class, &e).await;
    Some(error_response(proto, &e))
}

/// [`policy_or_refuse`]'s check and row, for a caller that answers the
/// refusal itself: a realtime session (realtime design §10.2, §10.3), which
/// reports it as an HTTP error before the upgrade and as an `error` event on
/// the open socket after it. The refusal writes its row all the same.
pub(crate) async fn policy_checked(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    let snap = state.snapshot();
    // The key's name **now**: the session outlives its handshake, and a
    // rename keeps the key (`policy_checked_call`'s doc). By the name the
    // handshake captured, a renamed key would match no row and be checked
    // against no scope and no budget at all (WP2 review R7).
    let name = ctx.current_key_name(&snap);
    checked_as(state, proto, ctx, name.as_deref(), alias, class).await
}

/// [`policy_checked`] for the key named `key_name`.
async fn checked_as(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    key_name: Option<&str>,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    let started = Instant::now();
    let snap = state.snapshot();
    let Err(e) = crate::policy::check_alias(&state.policy, &state.db, &snap, key_name, alias).await
    else {
        return Ok(());
    };
    // `record` closes a `request_started`, and no handler opened one for a
    // check that is not itself a model call.
    state.telemetry.request_started();
    record_refusal(state, proto, ctx, alias, started, class, &e).await;
    Err(e)
}

/// [`policy_checked`] for one model call made **inside** a request that
/// already holds its key's concurrency slot — a realtime session's calls
/// (realtime design §10.3): scope and budget, then the call counted against
/// the key's per-minute windows with [`crate::policy::PolicyGate::count_call`],
/// which takes no second slot. A refusal is logged like any other.
///
/// **The key is looked up again, by identity, on every call.** The request
/// it serves outlives the authentication that let it in — a realtime session
/// lasts as long as its socket — so a key disabled or deleted since then
/// must stop working at its next call, not at its next connection. Looked up
/// by `api_keys.id` rather than by name, because a rename keeps the key and
/// a delete-and-recreate under the same name is a different key.
pub(crate) async fn policy_checked_call(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    let started = Instant::now();
    let snap = state.snapshot();
    let key = match ctx.principal.key_id() {
        None => None,
        Some(id) => match snap.api_keys.iter().find(|k| k.id == id) {
            Some(k) if k.enabled => Some(k),
            _ => {
                let e = GatewayError::Unauthorized(
                    "the API key this request was opened with has since been disabled or \
                     deleted",
                );
                state.telemetry.request_started();
                record_refusal(state, proto, ctx, alias, started, class, &e).await;
                return Err(e);
            }
        },
    };
    // Scope and budget by the key's current name (`policy_checked`).
    let name = key.map(|k| k.name.as_str()).or(ctx.client_key.as_deref());
    checked_as(state, proto, ctx, name, alias, class).await?;
    let Some(key) = key else {
        return Ok(());
    };
    let Err(e) = state.policy.count_call(key, chrono::Utc::now()) else {
        return Ok(());
    };
    state.telemetry.request_started();
    record_refusal(state, proto, ctx, alias, started, class, &e).await;
    Err(e)
}

/// The row a key-policy refusal writes, closing the caller's
/// `request_started`. Shared with the token counters' scope check
/// (`proxy/count.rs`'s `counter_policy`, api-docs design §5.1), which renders
/// its refusal in each counter's own dialect — `/tokenize`'s llama.cpp shape
/// is not a [`ClientProto`] — so `proto` is only what the row's
/// `ingress_proto` says.
pub(super) async fn record_refusal(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
    started: Instant,
    class: RequestClass,
    e: &GatewayError,
) {
    // A refusal is traffic: it gets a row like everything else, so "why did my
    // agent stop working" is answerable from the Logs page rather than from
    // the client's side of the connection only.
    record(
        LogParams {
            state,
            proto,
            ctx,
            alias: alias.to_string(),
            route: None,
            started,
            streamed: false,
            class,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.to_string())),
    )
    .await;
}

pub(super) fn error_response(proto: ClientProto, e: &GatewayError) -> Response {
    let status = e.http_status();
    let body = proto.serialize_error(e);
    (status, axum::Json(body)).into_response()
}

/// The header a fallback-served response carries (gpu-hold design §2/§4).
pub const FALLBACK_HEADER: &str = "x-lmgw-fallback";
/// Why it was served by the fallback: `hold` | `external_vram` | `background`
/// | `unavailable` (candidate-aliases design §4.7, §12 entry 70). Always next to [`FALLBACK_HEADER`] —
/// [`crate::gate::GateHeaders::stamp`] writes both.
pub const FALLBACK_REASON_HEADER: &str = "x-lmgw-fallback-reason";
/// The candidate alias's model that answered (candidate-aliases design
/// §4.1): its model id. Never next to [`FALLBACK_HEADER`] —
/// [`crate::gate::GateHeaders::stamp`] writes one or the other.
pub const CANDIDATE_HEADER: &str = "x-lmgw-candidate";

/// Reasoning controls the answering route cannot express (§5.3), and
/// `enabled` for an off fitted to the model in another form (§5.6). Comma
/// separated, e.g. `enabled,budget`.
pub const REASONING_IGNORED_HEADER: &str = "x-lmgw-reasoning-ignored";

/// The `max_tokens` lmgw chose for an Anthropic route because nothing else did
/// (§5.4) — always stamped when the gateway picked the number, whether it came
/// from the provider's catalog or from the 4096 last resort.
pub const MAX_TOKENS_DEFAULTED_HEADER: &str = "x-lmgw-max-tokens-defaulted";

/// The `max_tokens` lmgw raised a *client-set* cap to, because the thinking
/// budget it asked for would not fit underneath it (§5.3).
pub const MAX_TOKENS_RAISED_HEADER: &str = "x-lmgw-max-tokens-raised";

/// The `max_tokens` lmgw *lowered* a client-set value to, because a ladder
/// rung or a guarded unified-KV pool's `n_predict` is the ceiling every
/// request on that row must respect (ladder design §3.2, unified-KV design
/// §3.3 step 1) — [`crate::gate::clamp::clamp_max_tokens`]'s own "lowered"
/// case, stamped next to [`MAX_TOKENS_DEFAULTED_HEADER`] /
/// [`MAX_TOKENS_RAISED_HEADER`] for the same reason: a cap the gateway chose
/// is invisible in the body otherwise.
pub const MAX_TOKENS_CLAMPED_HEADER: &str = "x-lmgw-max-tokens-clamped";

/// Why a token counter's answer is not exactly what the backend would count
/// (api-docs design §5.1): comma-separated `Approx::as_str()` values, e.g.
/// `flattened,tokenizer_guess`. Absent when the count is exact.
///
/// Stamped by `/v1/count_tokens` and `/v1/messages/count_tokens` through
/// `proxy/count.rs`'s `stamp_count`, which turns a `Count`'s flags into this
/// header's value. `/tokenize` never carries it: it relays llama-server's own
/// token ids verbatim, which are exact by definition.
pub const COUNT_APPROXIMATE_HEADER: &str = "x-lmgw-count-approximate";

/// Which parts of a resolved reasoning control the route does not actually send
/// (§5.3) — the table's "ignored" cells, as a pure function so the handler can
/// report them without the `Egress` trait growing an out-parameter.
///
/// `has_reasoning_object` says whether the client sent an OpenRouter-shaped
/// `reasoning` object: that object rides through passthrough and the egress
/// reconciles `effort`/`enabled` into it, so on that request those controls
/// *are* on the wire and reporting them ignored would be a lie.
///
/// Pure and total: same inputs, same answer, whether or not the request ever
/// reached the upstream.
pub fn reasoning_ignored(
    protocol: crate::config::Protocol,
    kind: crate::config::UpstreamKind,
    c: &crate::ir::ReasoningControl,
    has_reasoning_object: bool,
) -> Vec<&'static str> {
    use crate::config::{Protocol, UpstreamKind};
    let mut out = Vec::new();
    if c.is_empty() {
        return out;
    }
    // "Think" with no how-much has no wire spelling on these two. What rescues
    // it is another field that *implies* thinking is on — and only one that
    // this route actually sends: a budget a generic OpenAI provider drops
    // expresses nothing.
    match protocol {
        // llama-server understands all three.
        Protocol::Openai if kind == UpstreamKind::LlamaServer => {}
        // A generic OpenAI-protocol provider knows `reasoning_effort` and
        // nothing else — unless the client brought OpenRouter's object, which
        // carries `enabled` natively.
        Protocol::Openai => {
            if c.enabled == Some(true) && c.effort.is_none() && !has_reasoning_object {
                out.push("enabled");
            }
            if c.budget_tokens.is_some() {
                out.push("budget");
            }
        }
        // Anthropic expresses everything, but not all at once: the budget form
        // (`thinking: {type: "enabled", budget_tokens}`) has no room for a
        // level beside it, so a level given together with a budget is dropped.
        Protocol::Anthropic => {
            if c.budget_tokens.is_some() && c.effort.is_some() {
                out.push("effort");
            }
        }
        // Gemini has a real budget and a real level but takes one of them, and
        // no bare switch at all.
        Protocol::Gemini => {
            if c.enabled == Some(true) && c.effort.is_none() && c.budget_tokens.is_none() {
                out.push("enabled");
            }
            if c.budget_tokens.is_some() && c.effort.is_some() {
                out.push("effort");
            }
        }
    }
    out
}

/// Response annotations that are not the GPU-hold fallback: what the route did
/// not send, and any `max_tokens` the gateway itself picked or raised. They
/// travel as headers, on streams and unary answers alike, because none of it is
/// visible anywhere in the body.
///
/// They are computed from the *resolved route*, so a request that fails before
/// routing — an unparseable body, an unknown alias, a `gpu_hold` refusal —
/// carries none of them: there was no upstream yet to have ignored anything.
#[derive(Debug, Clone, Default)]
pub(crate) struct Annotations {
    pub ignored: Vec<&'static str>,
    pub max_tokens_defaulted: Option<u32>,
    pub max_tokens_raised: Option<u32>,
    /// [`crate::gate::clamp::clamp_max_tokens`]'s "lowered" case (ladder
    /// design §3.2, unified-KV design §3.3 step 1), from the send's
    /// [`crate::gate::TurnLease`] (or, on `/v1/responses`, whose headers leave
    /// before its first turn, [`crate::gate::planned_clamp`]). `None` on every
    /// row the gate does not guard.
    pub max_tokens_clamped: Option<u32>,
}

impl Annotations {
    fn is_empty(&self) -> bool {
        self.ignored.is_empty()
            && self.max_tokens_defaulted.is_none()
            && self.max_tokens_raised.is_none()
            && self.max_tokens_clamped.is_none()
    }
}

/// Stamp [`Annotations`] on a response, next to `x-lmgw-fallback` and for the
/// same reason: a control that was dropped, or a cap the gateway picked, is
/// invisible in the body.
pub(crate) fn with_annotations(mut resp: Response, a: &Annotations) -> Response {
    if a.is_empty() {
        return resp;
    }
    if !a.ignored.is_empty() {
        match header::HeaderValue::from_str(&a.ignored.join(",")) {
            Ok(v) => {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static(REASONING_IGNORED_HEADER), v);
            }
            Err(e) => tracing::warn!("cannot report ignored reasoning controls: {e}"),
        }
    }
    if let Some(n) = a.max_tokens_defaulted {
        resp.headers_mut().insert(
            header::HeaderName::from_static(MAX_TOKENS_DEFAULTED_HEADER),
            header::HeaderValue::from(n),
        );
    }
    if let Some(n) = a.max_tokens_raised {
        resp.headers_mut().insert(
            header::HeaderName::from_static(MAX_TOKENS_RAISED_HEADER),
            header::HeaderValue::from(n),
        );
    }
    if let Some(n) = a.max_tokens_clamped {
        resp.headers_mut().insert(
            header::HeaderName::from_static(MAX_TOKENS_CLAMPED_HEADER),
            header::HeaderValue::from(n),
        );
    }
    resp
}

/// Merge the reasoning control across its three tiers — headers, body, alias
/// defaults — and normalise (§5.1/§5.2).
///
/// Takes the whole request and the resolved route, not just the params,
/// because one body-tier source is route-dependent:
/// `chat_template_kwargs.enable_thinking` is a llama-server template variable
/// and only counts as a control when the route actually renders a template
/// (see [`ChatRequest::llama_kwargs_enabled`]). On a cloud route it stays what
/// the client sent — a passthrough key — and contributes nothing, so it can
/// never turn into an Anthropic `thinking: {type: "disabled"}`.
///
/// Every tier is normalised before it is merged, which is what makes
/// precedence hold per *tier* rather than per field: see
/// [`ReasoningControl::normalised`](crate::ir::ReasoningControl::normalised).
pub(crate) fn resolve_params(
    ir: &ChatRequest,
    ctx: &RequestCtx,
    route: &Route,
) -> crate::ir::Params {
    use crate::ir::ReasoningControl;
    let mut p = ir.params.clone();

    if route.upstream.kind == crate::config::UpstreamKind::LlamaServer {
        if let Some(t) = ir.llama_kwargs_enabled {
            let mut body = p.reasoning.take().unwrap_or_default();
            body.enabled = Some(t);
            p.reasoning = Some(body);
        }
    }
    if let Some(h) = ctx.reasoning_control() {
        let lower = p.reasoning.take().map(ReasoningControl::normalised);
        p.reasoning = Some(h.normalised().merge_over(lower));
    }
    // `with_defaults` normalises the alias tier and this one before merging.
    let mut p = p.with_defaults(&route.param_defaults);
    p.reasoning = p
        .reasoning
        .map(ReasoningControl::normalised)
        .filter(|c| !c.is_empty());
    p
}

/// Pick the Anthropic `max_tokens` when neither the client nor the alias did
/// (§5.4), write it into the route's defaults, and return it so the handler can
/// stamp [`MAX_TOKENS_DEFAULTED_HEADER`]. `None` means lmgw chose nothing —
/// another protocol, or a cap that was already set.
///
/// **Streaming decides which number.** The API refuses a *non-streaming*
/// request whose `max_tokens` implies a generation longer than its
/// non-streaming limit (~10 minutes of output), so handing a non-streamed call
/// the catalog's 64000 would turn a working request into a provider 400. A
/// stream has no such limit and should get the model's real maximum rather
/// than a cap lmgw invented. So: streamed ⇒ the catalog's published maximum,
/// non-streamed ⇒ the conservative 4096.
///
/// Either way the number is **lmgw's choice, not the client's**, so it is
/// always surfaced on the response — a cap that only shows up as a truncated
/// answer is exactly the hidden limit this gateway refuses to have.
pub(crate) async fn anthropic_max_tokens_default(
    state: &SharedState,
    route: &mut Route,
    client: &crate::ir::Params,
    stream: bool,
) -> Option<u32> {
    if route.upstream.protocol != crate::config::Protocol::Anthropic
        || client.max_tokens.is_some()
        || route.param_defaults.max_tokens.is_some()
    {
        return None;
    }
    let last_resort = crate::egress::anthropic::DEFAULT_MAX_TOKENS;
    let chosen = if stream {
        let published = match crate::catalog::upstream_models(state, &route.upstream).await {
            Ok(models) => models
                .iter()
                .find(|m| m.id == route.upstream_model)
                .and_then(|m| m.max_output_tokens),
            Err(e) => {
                tracing::debug!(
                    "anthropic catalog lookup for '{}' failed: {e}",
                    route.upstream_model
                );
                None
            }
        };
        match published {
            Some(n) => n.min(u32::MAX as u64) as u32,
            None => {
                tracing::debug!(
                    model = %route.upstream_model,
                    "no published max output tokens — defaulting max_tokens to {last_resort}"
                );
                last_resort
            }
        }
    } else {
        tracing::debug!(
            model = %route.upstream_model,
            "non-streaming anthropic request — defaulting max_tokens to {last_resort}, \
             the provider rejects a non-streamed call that could run past its limit"
        );
        last_resort
    };
    route.param_defaults.max_tokens = Some(chosen);
    Some(chosen)
}

/// The `max_tokens` the Anthropic egress will raise this request to, because
/// the thinking budget would not fit underneath the cap in force (§5.3).
///
/// Deterministic from the same two values the egress uses, so the handler can
/// report the raise ([`MAX_TOKENS_RAISED_HEADER`]) without the egress having to
/// report anything back. `None` when nothing is raised.
pub(crate) fn anthropic_max_tokens_raised(
    route: &Route,
    params: &crate::ir::Params,
) -> Option<u32> {
    if route.upstream.protocol != crate::config::Protocol::Anthropic {
        return None;
    }
    let floor = crate::egress::anthropic::budget_floor(&params.reasoning_control())
        .ok()
        .flatten()?;
    let in_force = params
        .max_tokens
        .unwrap_or(crate::egress::anthropic::DEFAULT_MAX_TOKENS);
    (floor > in_force).then_some(floor)
}

/// What an inner helper hands its HTTP wrapper: the JSON body, the route it
/// was actually produced on (for the request log's `upstream_*` columns), the
/// gate's headers — the fallback alias when one was used (gpu-hold design §4)
/// — and the usage to bill. Named because the tuple is the widest thing in
/// this file and a bare four-element return type is unreadable at every call
/// site.
pub(super) type Served<T> = (Route, GateHeaders, T, Usage);

/// The failure half of [`Served`]: how far the route got (for the log row's
/// `upstream_*` columns), the gate's headers (the fallback that was answering
/// when it failed), and the error.
///
/// The fallback travels with the **error** and not only with the success on
/// purpose (gpu-hold design §4). A held request that was re-routed to a cloud
/// alias whose provider then 500s is still a request lmgw answered somewhere
/// else, and `x-lmgw-fallback` is the only place that substitution is visible
/// — the body's `model` stays the alias the client asked for. Dropping the
/// header on the error path would leave an owner debugging "why did my local
/// model return 502" with nothing at all pointing at the fallback.
pub(super) type Failed = (Option<Box<Route>>, GateHeaders, GatewayError);

/// One `request_logs` row for a refusal made in the auth middleware, before any
/// handler ran (usage-analytics §4.2).
///
/// The alias is unknown at that point — the middleware deliberately does not
/// buffer the body to read it — so the row carries `"?"`, which is the same
/// placeholder every other pre-parse failure uses. Everything else is real:
/// the key, the status, the error kind. Without this, `key_rate` and
/// `key_expired` refusals exist only in the client's error handler. `proto`
/// is the row's label: the route's — `realtime` for `GET /v1/realtime`
/// (realtime design §11), `openai` for the rest of `/v1`.
pub(crate) async fn record_middleware_refusal(
    state: &SharedState,
    ctx: &RequestCtx,
    e: &GatewayError,
    proto: ClientProto,
) {
    let started = Instant::now();
    state.telemetry.request_started();
    record(
        LogParams {
            state,
            proto,
            ctx,
            alias: "?".to_string(),
            route: None,
            started,
            streamed: false,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.to_string())),
    )
    .await;
}

/// Log a request that failed **before or around** the model calls it would have
/// made — a bad body, an unknown alias, a native-passthrough upstream that
/// refused. Without this a `/v1/responses` request that never reached a turn
/// would leave no trace at all, since the per-turn rows are written by
/// [`crate::proxy::sample_once`] / [`crate::proxy::stream_once`] and there is
/// no parent row (§21).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_request_failure(
    state: &AppState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    alias: &str,
    route: Option<&Route>,
    fallback: Option<FallbackReason>,
    started: Instant,
    e: &GatewayError,
) {
    state.telemetry.request_started();
    record_free_form(
        state,
        ctx,
        ingress_proto,
        alias,
        route,
        fallback,
        started,
        None,
        false,
        e.http_status().as_u16(),
        Some((e.kind().to_string(), e.to_string())),
    )
    .await;
}

/// Log a byte-level passthrough whose body we deliberately never parsed, so it
/// has latency and status but no token counts (§21 native `/v1/responses`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_passthrough(
    state: &AppState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    alias: &str,
    route: &Route,
    fallback: Option<FallbackReason>,
    started: Instant,
    ttfb_ms: Option<i64>,
    streamed: bool,
    status: u16,
    error: Option<(String, String)>,
) {
    state.telemetry.request_started();
    record_free_form(
        state,
        ctx,
        ingress_proto,
        alias,
        Some(route),
        fallback,
        started,
        ttfb_ms,
        streamed,
        status,
        error,
    )
    .await;
}

/// Row writer for callers that have a free-form `ingress_proto` and may have no
/// route at all — the gap between [`record`] (keyed on `ClientProto`) and
/// [`crate::proxy::record_in_process`] (route required). Tokens are always NULL here: every
/// caller either failed before generating any or never parsed the body.
#[allow(clippy::too_many_arguments)]
async fn record_free_form(
    state: &AppState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    alias: &str,
    route: Option<&Route>,
    fallback: Option<FallbackReason>,
    started: Instant,
    ttfb_ms: Option<i64>,
    streamed: bool,
    status: u16,
    error: Option<(String, String)>,
) {
    let priced = price_call(
        state,
        alias,
        route.map(|r| r.upstream.id),
        route.map(|r| r.upstream_model.as_str()),
        &ctx.key_ref(),
        ingress_proto,
        &Usage::default(),
    );
    let row = NewRequestLog {
        client_key: priced.client_key,
        ingress_proto: ingress_proto.to_string(),
        requested_alias: alias.to_string(),
        upstream_id: route.map(|r| r.upstream.id),
        upstream_name: route.map(|r| r.upstream.name.clone()),
        upstream_model: route.map(|r| r.upstream_model.clone()),
        mcp_tool: None,
        egress_proto: route.map(|r| r.upstream.protocol.as_str().to_string()),
        status: status as i64,
        ttfb_ms,
        total_ms: Some(started.elapsed().as_millis() as i64),
        prompt_tokens: None,
        completion_tokens: None,
        streamed,
        error_kind: error.as_ref().map(|(k, _)| k.clone()),
        error_msg: error.as_ref().map(|(_, m)| m.clone()),
        key_id: priced.key_id,
        fallback_reason: fallback.map(|r| r.as_str().to_string()),
        // Free-form rows are the audio/task passthroughs and early refusals:
        // no usage was ever reported, so there is nothing to price. They land
        // in the unpriced remainder, which is exactly what they are.
        ..Default::default()
    };
    let log_id = store::insert_request_log(&state.db, &row)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to write request log: {e}");
            0
        });
    state.telemetry.request_finished(RequestSummary {
        log_id,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client_key: row.client_key,
        ingress_proto: row.ingress_proto,
        requested_alias: row.requested_alias,
        upstream_name: row.upstream_name,
        upstream_model: row.upstream_model,
        egress_proto: row.egress_proto,
        status,
        ttfb_ms: row.ttfb_ms,
        total_ms: row.total_ms,
        prompt_tokens: row.prompt_tokens,
        completion_tokens: row.completion_tokens,
        cached_in_tokens: row.cached_in_tokens,
        cache_write_tokens: row.cache_write_tokens,
        streamed: row.streamed,
        error_kind: row.error_kind,
        error_msg: row.error_msg,
        cost_micro: row.cost.total_micro,
        class: row.class.as_str().to_string(),
        key_id: row.key_id,
        fallback_reason: row.fallback_reason,
        rung: row.rung,
    });
}

#[cfg(test)]
mod tests;
