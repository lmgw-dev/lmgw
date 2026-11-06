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
use crate::pricing::{self, Quantities, TokenUsage};
use crate::state::{AppState, SharedState};
use crate::store::NewRequestLog;
use crate::telemetry::{internal_identity, RequestClass};

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
    /// `x-lmgw-background` as parsed by the middleware (per-request
    /// background design §3): the origin it asks for, or why it did not
    /// parse. Parsed but not judged, like [`Self::reasoning`] — and parsed
    /// **only on the header's routes** (§11.12), so it is `None` on every
    /// other route's context (realtime, the Chat, `/mcp`) whatever was sent.
    /// The routes that read it refuse a malformed one
    /// ([`Self::background_refusal`]); [`Self::origin`] is what they hand the
    /// gate.
    pub background: Option<Result<crate::runtime::registry::Origin, String>>,
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
    /// The device revocation generation when the principal was resolved
    /// (client-apps design §1.6): a long-lived connection a device opens
    /// watches for revocations raised after it, so a Disable that lands
    /// between the gate and the connection's first frame still ends it.
    ///
    /// `None` for a context built by hand rather than resolved at the root (a
    /// test, a stored principal resumed later): a watch then counts from the
    /// moment it starts, never every revocation the key ever had (review
    /// W2-14). The credential's fingerprint still catches a Rotate.
    pub revocation_mark: Option<u64>,
    /// The listener the request came in on and its generation then
    /// ([`ServedAt`](crate::server::ServedAt)): its streams and sessions end
    /// at that listener's stop, and the stop waits for its turns. `None` for
    /// a context built by hand, which takes the plain listener serving then.
    pub served_at: Option<crate::server::ServedAt>,
    /// The listener the request came in on (device link design §4.5): the
    /// plain one, or the device listener with a client certificate. A
    /// context built by hand is the plain listener's.
    pub via: crate::server::Via,
    /// On the device listener, the `notAfter` of the client certificate the
    /// principal was resolved from (device link design D5, §5.1): a
    /// long-lived connection it opens ends at that date with
    /// `certificate_expired`. `None` everywhere else.
    pub cert_not_after: Option<chrono::DateTime<chrono::Utc>>,
    /// On the device listener, the connection of a device the certificate
    /// opened (device link design §4.5): its peer, the local address it
    /// arrived on, and its socket handle, which the feed sets
    /// `TCP_USER_TIMEOUT` through (§7.1). `None` everywhere else.
    pub conn: Option<crate::server::Conn>,
    /// The authority the request was sent to (its `Host` header, or the
    /// URI's for HTTP/2), as the client wrote it: what a `/v1/responses`
    /// `mcp` block's `server_url` naming this gateway is compared with (openai
    /// egress design §6). `None` for a context built by hand.
    pub host: Option<String>,
}

impl RequestCtx {
    /// The dashboard's own request: its owner key (`owner:dashboard`) as
    /// the principal — what an in-process test drives the Chat or a bound
    /// session's turn as. `RequestCtx::default()` is anonymous, which the
    /// Chat refuses rather than taking for the owner (review W3-12).
    #[doc(hidden)]
    pub fn dashboard_for_tests(snap: &crate::config::Snapshot) -> Self {
        let key = snap
            .api_keys
            .iter()
            .find(|k| k.name == crate::agents::token::OWNER_DASHBOARD);
        Self {
            principal: key
                .map(crate::principal::Principal::from_key)
                .unwrap_or_default(),
            client_key: key.map(|k| k.name.clone()),
            ..Default::default()
        }
    }

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

    /// Whose traffic this request is, as the gate takes it
    /// (`gate::resolve`'s `origin`): [`Origin::Background`] exactly when
    /// `x-lmgw-background: on` was sent on one of the header's routes, the
    /// owner's otherwise — a malformed value included, which those routes
    /// refuse first ([`Self::background_refusal`]).
    ///
    /// [`Origin::Background`]: crate::runtime::registry::Origin::Background
    pub fn origin(&self) -> crate::runtime::registry::Origin {
        match &self.background {
            Some(Ok(origin)) => *origin,
            _ => crate::runtime::registry::Origin::Owner,
        }
    }

    /// The `400` for an `x-lmgw-background` value that did not parse, naming
    /// the header — `None` when it parsed or was not sent.
    pub fn background_refusal(&self) -> Option<GatewayError> {
        let why = self.background.as_ref()?.as_ref().err()?;
        Some(GatewayError::BadRequest(why.clone()))
    }

    /// The key this request is charged to: the name it authenticated with,
    /// and the principal's `api_keys.id` — which every row is resolved by
    /// when it is written ([`KeyRef`]) — and the run it is stamped with.
    pub(crate) fn key_ref(&self) -> KeyRef {
        KeyRef {
            name: self.client_key.clone(),
            id: self.principal.key_id(),
            run: self.run,
            via: self.via,
            public: false,
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
///
/// It also names the run the call belongs to, when the request was stamped
/// `X-Lmgw-Run` ([`RequestCtx::run`]): the row a call writes in-process — a
/// `/v1/responses` turn, a realtime turn — folds into that run's total
/// where it is written ([`note_on_run`]), as a `/v1` request's row does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyRef {
    pub name: Option<String>,
    pub id: Option<i64>,
    pub run: Option<i64>,
    /// The listener the caller's request came in on (device link design
    /// §4.5), which its rows record.
    pub via: crate::server::Via,
    /// The caller is a `/v1` client's own request run in-process (the
    /// `/v1/responses` loop), not one of lmgw's own consumers: without a
    /// key (auth off) its answers' items are recorded under none, as a
    /// public request's are, so it may send its own seals back (openai
    /// egress design §7.3.4, `proxy::answer_items`).
    pub public: bool,
}

impl KeyRef {
    /// A caller known by its label only: resolved by name, as before.
    pub fn named(name: Option<String>) -> Self {
        Self {
            name,
            ..Default::default()
        }
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
    /// What the request's content lost on its way to a model that lacks a
    /// capability (`request_logs.degraded`, [`crate::degraded`]): the send's
    /// [`crate::gate::TurnLease::degraded`]. `None` for anything else.
    pub(super) degraded: Option<String>,
    /// What the call processed besides tokens, measured by its route or
    /// reported by its provider (billable-units design §4):
    /// `Default::default()` when nothing was. `requests` left `None` is
    /// filled by [`record`] (`recording/quantities.rs`).
    pub(super) quantities: Quantities,
    /// The service tier the answer reported (openai egress design §5.2):
    /// [`ServedTier::of`](pricing::ServedTier::of) on a chat answer,
    /// `Default::default()` (no tier dimension) everywhere else.
    pub(super) service_tier: pricing::ServedTier,
}

pub(super) async fn record(
    p: LogParams<'_>,
    status: u16,
    ttfb_ms: Option<i64>,
    usage: Usage,
    error: Option<(&str, String)>,
) {
    record_counted(p, status, ttfb_ms, usage, error, None).await;
}

/// [`record`], with lmgw's own count of an answer whose usage never came
/// (a platform chat stream cut before its usage chunk, openai egress design
/// §13.6): the budgets' lower bound, never the row's tokens.
pub(super) async fn record_counted(
    p: LogParams<'_>,
    status: u16,
    ttfb_ms: Option<i64>,
    usage: Usage,
    error: Option<(&str, String)>,
    local_count: Option<pricing::LocalCount>,
) {
    let total_ms = p.started.elapsed().as_millis() as i64;
    let q = with_requests_default(
        p.quantities,
        p.route.is_some(),
        status,
        error.as_ref().map(|(k, _)| *k),
    );
    let priced = price_call(
        p.state,
        &p.alias,
        p.route,
        &p.ctx.key_ref(),
        p.proto.as_str(),
        &usage,
        &q,
        &p.service_tier,
        &pricing::HostedCalls::default(),
        local_count.as_ref(),
    );
    // A counted answer is one whose usage never came: the note says so
    // before what the budgets counted of it (§13.6).
    let price_note = priced.price_note(local_count.map(|_| {
        no_usage_reason(
            p.streamed,
            error.as_ref(),
            q.requests.is_some_and(|n| n > 0),
        )
    }));
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
        degraded: p.degraded,
        audio_in_ms: quantity_column(q.audio_in_ms),
        chars_in: quantity_column(q.chars_in),
        images_out: quantity_column(q.images_out),
        // A model call: no approval decides one.
        approved_by: None,
        via: p.ctx.via,
        service_tier: p.service_tier.relayed(),
        price_note,
        priced_alias: priced.priced_alias,
    };
    note_on_run(p.state, p.ctx.run, &row, &usage, q.requests);
    // Its own task: a handler dropped mid-insert neither loses the row nor
    // leaves the gauge open (`recording/write.rs`).
    write::write_row(p.state, row, status).await;
}

/// Cost per run, for a caller lmgw did not drive (§3.1): the row of a call
/// stamped `X-Lmgw-Run` folds into that run's total. No `run_id` column on
/// `request_logs`: the job row is already the durable home of a run's totals
/// and a second one would drift.
///
/// Only a call **routed somewhere** folds in: an unknown alias never was.
/// Nor a row that is no model call at all ([`RequestClass::Tool`]): a token
/// count that had to load its model reached an upstream and is still no
/// model turn. What a row adds to the cost is its own
/// [`NewRequestLog::row_cost`], the rollup's rule — a stopped call's priced
/// tokens, a failed one's fee, a failure that spent tokens no one could
/// price — and only an answer without an error is a model call: a run that
/// mistyped its alias forty times, or hit forty refusals, reads as no model
/// calls that cost nothing.
pub(super) fn note_on_run(
    state: &AppState,
    run: Option<i64>,
    row: &NewRequestLog,
    usage: &Usage,
    requests: Option<u64>,
) {
    let Some(run) = run.filter(|_| row.upstream_id.is_some() && row.class != RequestClass::Tool)
    else {
        return;
    };
    // An answer marked `incomplete` was answered all the same: the mark says
    // its usage is missing, not that the call failed.
    // A stop or a hang-up after the upstream answered is one too, by the
    // predicate billing uses: the row's `requests` quantity.
    let answered = match row.error_kind.as_deref() {
        None => true,
        Some(INCOMPLETE) => true,
        Some("canceled" | "client_disconnected") => requests.is_some_and(|n| n > 0),
        Some(_) => false,
    };
    state
        .agent_meters
        .note_row(run, usage, row.row_cost(), answered);
}

/// The row's `error_kind` for an answer that ended without the usage it
/// should have reported: a native `/v1/responses` stream with no terminal
/// event and no error, or a whole body with no `usage` to read (openai
/// egress design §5.2, §6). Not a failure: the request was answered, and
/// counts as such; the mark makes its unpriced row visible as one.
pub const INCOMPLETE: &str = "incomplete";

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
    /// The note for an answer its service tier left unpriced (openai egress
    /// design §5.6).
    tier_note: Option<String>,
    /// The note for an answer its hosted-tool calls left unpriced (§6.1).
    hosted_note: Option<String>,
    /// What the budgets counted for an unpriced answer, as a lower bound
    /// (§13): the clause the note's head ends with.
    floor_note: Option<String>,
    /// The alias the price was looked up under, when it is not the one the
    /// client asked for (a fallback that answered): the row's
    /// `priced_alias`, so a later re-check of its tier reads the same rows.
    pub(super) priced_alias: Option<String>,
}

impl Priced {
    /// The row's `price_note`: the tier's words first, since the
    /// unpriced-tiers list reads a no-tier note by its opening
    /// (`store::unpriced_tier_traffic`), then the hosted-tool fees', which
    /// the hosted list finds anywhere in the note
    /// (`pricing::HostedCalls::from_note`). `reported` is why an answer is
    /// unpriced in the first place (its usage never came, §5.2): it stands
    /// in for the tier's note, which is moot then, never for the hosted
    /// calls', so a cut stream that searched stays on the hosted list. A
    /// budget floor's clause ends the head (§13): it says what the budgets
    /// counted for the reason the head gives.
    pub(super) fn price_note(&self, reported: Option<&str>) -> Option<String> {
        let head = reported
            .map(str::to_string)
            .or_else(|| self.tier_note.clone());
        let head = match (head, self.floor_note.clone()) {
            (Some(h), Some(f)) => Some(format!("{h}; {f}")),
            (h, f) => h.or(f),
        };
        match (head, self.hosted_note.clone()) {
            (Some(h), Some(f)) => Some(format!("{h}; {f}")),
            (h, f) => h.or(f),
        }
    }
}

/// [`Priced`] for one call. `alias` is the name the client asked for, and
/// `route` where the call went. The price is looked up under the alias that
/// answered ([`Route::priced_alias`]): a fallback's own rows apply to the
/// rows it answers, while the row keeps `requested_alias` as asked.
///
/// `q` is what the call processed besides tokens, `requests` already
/// defaulted ([`with_requests_default`]); every unit the answering alias's
/// [`Sheet`](pricing::Sheet) prices is priced from it (billable-units design
/// §3.2).
///
/// `served` is the service tier the answer reported: the sheet is that
/// tier's rows (openai egress design §5.5), and a tier with no row leaves
/// the call unpriced with [`Priced::price_note`] saying so (§5.6) — on an
/// answer with something to price; a call that failed before it spent
/// anything has no tier to speak of.
///
/// `hosted` is the billable hosted-tool calls the answer made (§6.1): their
/// fees have no price unit yet, so a call with any is unpriced, whatever its
/// tokens cost, and the note says what they would have cost alone. A budget
/// still counts what its tokens cost, as a lower bound
/// ([`pricing::Cost::budget_floor_micro`]): the fee nobody can price must not
/// make the tokens beside it free to a budget.
///
/// A call its tier left unpriced keeps a lower bound for the budgets too,
/// when the tier that served it costs at least what another sheet prices it
/// at ([`pricing::tier_floor`], openai egress design §13.2): a Fast answer
/// with no Fast row counts at the standard price. The row stays unpriced,
/// and its note says what the budgets counted. Every other unpriced call
/// adds nothing to a budget's spend.
#[allow(clippy::too_many_arguments)]
pub(super) fn price_call(
    state: &AppState,
    alias: &str,
    route: Option<&Route>,
    key: &KeyRef,
    ingress_proto: &str,
    usage: &Usage,
    q: &Quantities,
    served: &pricing::ServedTier,
    hosted: &pricing::HostedCalls,
    local_count: Option<&pricing::LocalCount>,
) -> Priced {
    let snap = state.snapshot();
    let (client_key, key_id) = charged(&snap, key, ingress_proto);
    // Fold this call into the rolling windows the policy gate enforces on, so
    // a budget check never has to run a SUM on the request path and a
    // tokens/minute limit sees the tokens that were actually spent. A key
    // deleted while its request ran has no window left to enforce, and a
    // fresh one for it would never be read again (A2 review 3). An answer
    // whose usage never came counts lmgw's own count of it instead (openai
    // egress design §13.4), so a cut stream does not slip past the limit.
    if let Some(id) = key_id.filter(|id| snap.api_keys.iter().any(|k| k.id == *id)) {
        let tokens = match local_count.filter(|_| !usage.reported()) {
            Some(c) => c.prompt + c.output,
            None => usage.prompt_tokens.unwrap_or(0) + usage.completion_tokens.unwrap_or(0),
        };
        state.policy.note_tokens(id, tokens as i64);
    }

    let detail = TokenUsage {
        prompt: usage.prompt_tokens,
        completion: usage.completion_tokens,
        cached_in: usage.cached_input_tokens,
        cache_write: usage.cache_write_tokens,
        reasoning: usage.reasoning_tokens,
    };
    let sheet = snap.sheet_for(
        route.map_or(alias, |r| r.priced_alias(alias)),
        route.map(|r| r.upstream.id),
        route.map(|r| r.upstream_model.as_str()),
        served.price_tier(),
    );
    let cost = pricing::price_request(&detail, q, &sheet);
    let something_to_price = detail.prompt.is_some()
        || detail.completion.is_some()
        || q.requests.is_some_and(|n| n > 0)
        || q.audio_in_ms.is_some()
        || q.chars_in.is_some()
        || q.images_out.is_some();
    let tier_note = sheet
        .tier_gap
        .filter(|_| something_to_price)
        .map(|gap| pricing::gap_note(served, &gap, route.map_or(alias, |r| &r.upstream_model)));
    // Unpriced because of its tier, or because its usage never came and
    // lmgw counted it itself: bounded from below by a sheet that costs no
    // more (§13.2, §13.4), priced on the same quantities and on the usage
    // reported, or else the local count.
    let count = local_count.filter(|_| detail.prompt.is_none() && detail.completion.is_none());
    let floor_usage = count.map_or(detail, pricing::LocalCount::usage);
    let floor_tiers = if cost.total_micro.is_some() || !(something_to_price || count.is_some()) {
        Vec::new()
    } else if count.is_some() {
        pricing::count_floor_tiers(served)
    } else if sheet.tier_gap.is_some() || served.price_tier().is_none() {
        pricing::tier_floor(served)
    } else {
        Vec::new()
    };
    let floor = floor_tiers.into_iter().find_map(|tier| {
        let floor_sheet = snap.sheet_for(
            route.map_or(alias, |r| r.priced_alias(alias)),
            route.map(|r| r.upstream.id),
            route.map(|r| r.upstream_model.as_str()),
            Some(tier),
        );
        let micro = pricing::price_request(&floor_usage, q, &floor_sheet).total_micro?;
        Some((tier, micro))
    });
    let floor_note = floor
        .map(|(tier, micro)| pricing::floor_clause(tier, micro, count, &snap.settings.currency));
    let cost = match floor {
        Some((_, micro)) => cost.with_floor(micro),
        None => cost,
    };
    // A fee priced nowhere makes the whole call unknown (§6.1); a local
    // route's zero is real and has no such fee.
    let hosted_note = (!hosted.is_empty() && cost.source != pricing::PriceSource::FreeLocal)
        .then(|| hosted.note(cost.total_micro, &snap.settings.currency));
    // The row is unpriced, but what it did price stays the budgets' lower
    // bound (`Cost::budget_floor_micro`).
    let cost = if hosted_note.is_some() {
        cost.unpriced()
    } else {
        cost
    };

    // Advance the cached spend the budget check reads — every unit's part,
    // since all of it is money in the one currency (billable-units §7).
    // Unpriced calls add nothing — which is the honest answer, and the reason
    // a budget cannot be enforced against traffic whose price nobody knows —
    // except a lower bound where one is known: the known part of one
    // unpriced only for a hosted-tool fee, or the floor of one its tier left
    // unpriced. A client could otherwise name a hosted tool or ask for Fast
    // on every request and spend what no budget sees (openai egress design
    // §6.1, §13).
    if let Some(micro) = cost.budget_micro().filter(|m| *m != 0) {
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
        tier_note,
        hosted_note,
        floor_note,
        priced_alias: route
            .and_then(|r| r.fallback.as_deref())
            .filter(|f| !f.eq_ignore_ascii_case(alias))
            .map(str::to_string),
    }
}

/// Why a row counted locally has no usage, by the row's error: its client
/// left mid-stream (`canceled`) or before the upstream answered (`499`
/// `client_disconnected`); a whole answer reported none; or a stream ended
/// (or broke) before the upstream reported any.
///
/// `answered`: the upstream's answer was read whole before the row was
/// written (a detached unary read, `chat/unary.rs`), so a client that left
/// then did not leave an unanswered request.
pub(super) fn no_usage_reason(
    streamed: bool,
    error: Option<&(&str, String)>,
    answered: bool,
) -> &'static str {
    match error {
        Some(("canceled", _)) => pricing::CLIENT_LEFT_NO_USAGE,
        Some((kind, _)) if *kind == super::CLIENT_DISCONNECTED && answered => {
            pricing::CLIENT_LEFT_ANSWER_NO_USAGE
        }
        Some((kind, _)) if *kind == super::CLIENT_DISCONNECTED => pricing::CLIENT_LEFT_UNANSWERED,
        _ if !streamed => pricing::ANSWER_NO_USAGE,
        _ => pricing::STREAM_NO_USAGE,
    }
}

/// Whom a call is charged to: the row's `client_key` label and the key's
/// id. The key's identity when the caller had one (`KeyRef`); otherwise by
/// name — a real key's when the caller authenticated, else the synthetic
/// identity for whichever internal consumer `ingress_proto` is.
fn charged(snap: &Snapshot, key: &KeyRef, ingress_proto: &str) -> (Option<String>, Option<i64>) {
    let (client_key, known) = key.resolve(snap);
    let key_id = known.or_else(|| {
        client_key
            .as_deref()
            .or_else(|| internal_identity(ingress_proto))
            .and_then(|n| snap.key_id_for_name(n))
    });
    (client_key, key_id)
}

/// [`crate::policy::check_priced`] for a send of `class` on `route` by the
/// key a call is charged to ([`charged`], so a row and its refusal name the
/// same key): a budgeted caller's request on a scope that prices nothing it
/// is billed in, flex without a flex price among them (openai egress design
/// §13.3, decision 24). `requested` is the request's own `service_tier`.
pub(crate) fn priced_refusal(
    state: &AppState,
    key: &KeyRef,
    ingress_proto: &str,
    alias: &str,
    route: &Route,
    class: RequestClass,
    requested: Option<&str>,
) -> Result<(), GatewayError> {
    let snap = state.snapshot();
    let (_, id) = charged(&snap, key, ingress_proto);
    let key = id.and_then(|id| snap.api_keys.iter().find(|k| k.id == id));
    crate::policy::check_priced(&snap, key, alias, route, class, requested)
}

/// [`priced_refusal`] on the route `alias` is served on, for a request whose
/// handler has no later point where its route is final and checked (the
/// media routes, embeddings, legacy completions): the route the gate's
/// static walk hands back (openai egress design §14.9, [`crate::gate::resolve`],
/// which starts nothing) — the alias's own, or the fallback a GPU hold, a
/// benchmark, a CPU row's container still on the GPU under the hold or a
/// disabled row hands it to — so a held embedder whose fallback is an
/// unpriced cloud encoder is refused as a request naming that encoder would
/// be. A fallback reached at admission (VRAM outside lmgw, a deferred guest)
/// is not checked here yet. An alias the walk refuses is refused later by
/// its handler, with that handler's words, so it passes here.
pub(crate) async fn alias_priced_refusal(
    state: &SharedState,
    key: &KeyRef,
    ingress_proto: &str,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    let routed = crate::gate::resolve(
        state,
        alias,
        crate::gate::RouteCheck::None,
        crate::runtime::registry::Origin::Owner,
    )
    .await;
    let Ok(routed) = routed else {
        return Ok(());
    };
    priced_refusal(
        state,
        key,
        ingress_proto,
        alias,
        routed.resolved(),
        class,
        None,
    )
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
    let e = policy_refusal_priced(state, ctx, proto, alias, class).await?;
    record_refusal(state, proto, ctx, alias, started, class, &e).await;
    Some(error_response(proto, &e))
}

/// [`policy_refusal`], and then a budget enforced only on what is priced
/// (decision 24) on the route `alias` resolves to: for a handler of `class`
/// whose route is not checked again where it is final.
pub(super) async fn policy_refusal_priced(
    state: &SharedState,
    ctx: &RequestCtx,
    proto: ClientProto,
    alias: &str,
    class: RequestClass,
) -> Option<GatewayError> {
    match policy_refusal(state, ctx, alias).await {
        Some(e) => Some(e),
        None => alias_priced_refusal(state, &ctx.key_ref(), proto.as_str(), alias, class)
            .await
            .err(),
    }
}

/// [`policy_or_refuse`]'s check alone, for a handler that writes the
/// refusal's row itself — one holding an [`super::Unanswered`], which has to
/// hand the row over right before it is written.
pub(super) async fn policy_refusal(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
) -> Option<GatewayError> {
    let snap = state.snapshot();
    crate::policy::check_alias(
        &state.policy,
        &state.db,
        &snap,
        ctx.client_key.as_deref(),
        alias,
    )
    .await
    .err()
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

/// A key's refusal that is no alias check — a device turn's concurrency
/// slot (review W3-8): its row, as every refusal gets one.
pub(crate) async fn refused_for_key(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
    class: RequestClass,
    e: &GatewayError,
) {
    // `record` closes a `request_started`; nothing opened one here.
    state.telemetry.request_started();
    record_refusal(state, proto, ctx, alias, Instant::now(), class, e).await;
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
    let mut checked =
        crate::policy::check_alias(&state.policy, &state.db, &snap, key_name, alias).await;
    // A budget is enforced only on what is priced (decision 24). A model
    // turn is checked where its route is final instead (each turn's open,
    // `serve_opened`).
    if checked.is_ok() && class != RequestClass::Chat {
        checked = alias_priced_refusal(state, &ctx.key_ref(), proto.as_str(), alias, class).await;
    }
    let Err(e) = checked else {
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
/// a delete-and-recreate under the same name is a different key — and
/// compared by its credential (`Principal::presents`), because a Rotate
/// keeps the row and changes the key: a turn or a session opened with the
/// old key stops at its next call (client-apps design §1.6, review W2-2).
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
            Some(k) if k.enabled && ctx.principal.presents(k) => Some(k),
            _ => {
                let e = GatewayError::Unauthorized(
                    "the API key this request was opened with has since been disabled, rotated \
                     or deleted",
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
            degraded: None,
            quantities: Quantities::default(),
            service_tier: Default::default(),
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.log_text())),
    )
    .await;
}

/// `e` as the client's protocol answers it: its status and body, and an
/// upstream rate refusal's rate-limit headers (openai egress design §8).
pub(super) fn error_response(proto: ClientProto, e: &GatewayError) -> Response {
    let status = proto.error_status(e);
    let body = proto.serialize_error(e);
    let mut resp = (status, axum::Json(body)).into_response();
    e.stamp_rate_hints(&mut resp);
    resp
}

/// The header a fallback-served response carries (gpu-hold design §2/§4).
pub const FALLBACK_HEADER: &str = "x-lmgw-fallback";
/// Why it was served by the fallback: `hold` | `external_vram` | `background`
/// | `unavailable` (candidate-aliases design §4.7, §12 entry 70). Always next to [`FALLBACK_HEADER`] —
/// [`crate::gate::GateHeaders::stamp`] writes both.
pub const FALLBACK_REASON_HEADER: &str = "x-lmgw-fallback-reason";
/// Every hop a fallback-served request was handed to, in order, each with its
/// reason (openai egress design §14.5): an RFC 8941 list,
/// `"B";reason=hold, "C";reason=quota`. Next to [`FALLBACK_HEADER`] on every
/// answer a fallback served — [`crate::gate::GateHeaders::stamp`] writes it.
pub const FALLBACK_CHAIN_HEADER: &str = "x-lmgw-fallback-chain";
/// The candidate alias's model that answered (candidate-aliases design
/// §4.1): its model id. Never next to [`FALLBACK_HEADER`] —
/// [`crate::gate::GateHeaders::stamp`] writes one or the other.
pub const CANDIDATE_HEADER: &str = "x-lmgw-candidate";

/// Reasoning controls the answering route cannot express (§5.3), and
/// `enabled` for an off fitted to the model in another form (§5.6). Comma
/// separated, e.g. `enabled,budget`.
pub const REASONING_IGNORED_HEADER: &str = "x-lmgw-reasoning-ignored";

/// Modelled sampling parameters the answering route does not send (openai
/// egress design §7.1, P2), next to [`REASONING_IGNORED_HEADER`]. Comma
/// separated, e.g. `top_k,min_p`.
pub const PARAMS_IGNORED_HEADER: &str = "x-lmgw-params-ignored";

/// Modelled parameters the answering model refused, which lmgw left out of
/// the send (`proxy::reasoning_fit::params`): on the request that met the
/// refusal and retried without them, and on every later one to the same
/// model. Comma separated, e.g. `temperature,top_p`.
pub const PARAMS_REFUSED_HEADER: &str = "x-lmgw-params-refused";

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

/// How many of the request's images went to the fallback that answered as
/// text placeholders, because its capabilities say it cannot see
/// ([`crate::gate::fallback_images`]). Stamped next to `x-lmgw-fallback`,
/// which says who answered: the placeholders themselves reach the model,
/// not the client.
pub const IMAGES_OMITTED_HEADER: &str = "x-lmgw-images-omitted";

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
    c: &crate::ir::ReasoningControl,
    has_reasoning_object: bool,
) -> Vec<&'static str> {
    use crate::config::Protocol;
    let mut out = Vec::new();
    if c.is_empty() {
        return out;
    }
    // Anthropic's `between_tools` off (notes between tool calls, no
    // up-front thinking) has no spelling elsewhere: those routes send their
    // own off, which is the nearest meaning, and say the rest was not kept.
    if c.between_tools && protocol != Protocol::Anthropic {
        out.push("between_tools");
    }
    // "Think" with no how-much has no wire spelling on these two. What rescues
    // it is another field that *implies* thinking is on — and only one that
    // this route actually sends: a budget a generic OpenAI provider drops
    // expresses nothing.
    match protocol {
        // llama-server understands all three.
        Protocol::LlamaCpp => {}
        // A generic OpenAI-protocol provider knows `reasoning_effort` and
        // nothing else — unless the client brought OpenRouter's object, which
        // carries `enabled` natively.
        Protocol::OpenaiCompatible => {
            if c.enabled == Some(true) && c.effort.is_none() && !has_reasoning_object {
                out.push("enabled");
            }
            if c.budget_tokens.is_some() {
                out.push("budget");
            }
        }
        // The OpenAI platform knows `reasoning_effort` alone, and translates
        // OpenRouter's object into it rather than forwarding it (openai
        // egress design §7.1, P3): the object rescues nothing here.
        Protocol::Openai => {
            if c.enabled == Some(true) && c.effort.is_none() {
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

/// Which of the request's modelled sampling parameters the route does not
/// send (openai egress design §7.1, P2), in a fixed order — the
/// [`PARAMS_IGNORED_HEADER`]'s value, as a pure function beside
/// [`reasoning_ignored`] and for the same reason.
///
/// Read off the egresses' one table ([`crate::egress::sampling`]), which the
/// Chat's split reads too: the OpenAI platform leaves out the llama samplers
/// (its Chat reference, checked 2026-10-24,
/// <https://developers.openai.com/api/reference/resources/chat.md>, "Create
/// chat completion", has none of them, and it refuses unknown parameters);
/// Anthropic leaves out `min_p`, `repeat_penalty`, the penalties and `seed`;
/// Gemini `min_p`, `repeat_penalty` and the penalties; the compatible and
/// llama.cpp egresses send them all. A client's own unknown keys are not
/// read here: they go verbatim.
pub fn params_ignored(
    protocol: crate::config::Protocol,
    p: &crate::ir::Params,
) -> Vec<&'static str> {
    crate::egress::sampling::dropped(p, protocol)
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
    /// [`params_ignored`]: the modelled samplers the route does not send.
    pub params_ignored: Vec<&'static str>,
    /// [`PARAMS_REFUSED_HEADER`]: the parameters the model refused, left out.
    pub params_refused: Vec<&'static str>,
    pub max_tokens_defaulted: Option<u32>,
    pub max_tokens_raised: Option<u32>,
    /// [`crate::gate::clamp::clamp_max_tokens`]'s "lowered" case (ladder
    /// design §3.2, unified-KV design §3.3 step 1), from the send's
    /// [`crate::gate::TurnLease`] (or, on `/v1/responses`, whose headers leave
    /// before its first turn, [`crate::gate::planned_clamp`]). `None` on every
    /// row the gate does not guard.
    pub max_tokens_clamped: Option<u32>,
    /// How many images went to the answering fallback as placeholders,
    /// because it cannot see ([`crate::gate::fallback_images`]): the send's
    /// [`crate::gate::TurnLease::images_omitted`] (on `/v1/responses`, the
    /// images the run opened with). `None` when none were left out.
    pub images_omitted: Option<usize>,
    /// The verbatim Anthropic blocks the send or the answer lost on the way
    /// ([`crate::ir::ignored_names`]): `content.<type>` for a request's
    /// block an upstream other than Anthropic's was not sent,
    /// `answer.<type>` for an answer's block a client of another wire was
    /// not given. [`PARAMS_IGNORED_HEADER`] lists them after
    /// [`Self::params_ignored`].
    pub blocks_ignored: Vec<String>,
}

impl Annotations {
    fn is_empty(&self) -> bool {
        self.ignored.is_empty()
            && self.params_ignored.is_empty()
            && self.blocks_ignored.is_empty()
            && self.params_refused.is_empty()
            && self.max_tokens_defaulted.is_none()
            && self.max_tokens_raised.is_none()
            && self.max_tokens_clamped.is_none()
            && self.images_omitted.is_none()
    }
}

/// Name on `resp` the verbatim Anthropic blocks of a whole answer that a
/// client of `proto` is not given, `answer.<type>` in
/// [`PARAMS_IGNORED_HEADER`] ([`crate::ir::ignored_names`]): every wire but
/// the Messages wire has no place for one. [`with_annotations`] puts what
/// the send left out ahead of them.
pub(crate) fn name_answer_blocks(
    mut resp: Response,
    proto: crate::ingress::ClientProto,
    content: &[crate::ir::ContentPart],
) -> Response {
    if proto == crate::ingress::ClientProto::AnthropicMessages {
        return resp;
    }
    let names = crate::ir::ignored_names("answer", content);
    if names.is_empty() {
        return resp;
    }
    match header::HeaderValue::from_str(&names.join(",")) {
        Ok(v) => {
            resp.headers_mut()
                .insert(header::HeaderName::from_static(PARAMS_IGNORED_HEADER), v);
        }
        Err(e) => tracing::warn!("cannot report the answer's ignored blocks: {e}"),
    }
    resp
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
    if !a.params_ignored.is_empty() || !a.blocks_ignored.is_empty() {
        // A unary answer's own blocks were named on the response already
        // ([`name_answer_blocks`]); they follow what the send left out.
        let mut names: Vec<String> = a.params_ignored.iter().map(|s| s.to_string()).collect();
        names.extend(a.blocks_ignored.iter().cloned());
        if let Some(answer) = resp
            .headers()
            .get(PARAMS_IGNORED_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            names.extend(answer.split(',').map(str::to_string));
        }
        match header::HeaderValue::from_str(&names.join(",")) {
            Ok(v) => {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static(PARAMS_IGNORED_HEADER), v);
            }
            Err(e) => tracing::warn!("cannot report ignored parameters: {e}"),
        }
    }
    if !a.params_refused.is_empty() {
        match header::HeaderValue::from_str(&a.params_refused.join(",")) {
            Ok(v) => {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static(PARAMS_REFUSED_HEADER), v);
            }
            Err(e) => tracing::warn!("cannot report refused parameters: {e}"),
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
    if let Some(n) = a.images_omitted {
        resp.headers_mut().insert(
            header::HeaderName::from_static(IMAGES_OMITTED_HEADER),
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
            degraded: None,
            quantities: Quantities::default(),
            service_tier: Default::default(),
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.log_text())),
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
    state: &SharedState,
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
        Some((e.kind().to_string(), e.log_text())),
        None,
        &Reported::default(),
        Quantities::default(),
    )
    .await;
}

/// What a native `/v1/responses` answer reported, for its row (openai
/// egress design §5.2): its usage (`Usage::default()` when none arrived,
/// which leaves the token columns NULL), the service tier it said served
/// it, and a `price_note` when its cost is not plain — an answer that ended
/// before its usage was reported. `hosted_calls` is the billable hosted-tool
/// calls its Response object's output made (§6.1), which leave it unpriced.
/// `local_count` is lmgw's own count of an answer whose usage never came, a
/// stream cut before its terminal event on the platform (§13.4): the
/// budgets' lower bound, never the row's tokens.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reported {
    pub usage: Usage,
    pub service_tier: pricing::ServedTier,
    pub price_note: Option<String>,
    pub hosted_calls: pricing::HostedCalls,
    pub local_count: Option<pricing::LocalCount>,
}

/// Log a native `/v1/responses` passthrough (§21, openai egress design
/// §5.2), once it has ended: `reported` is what the upstream's Response
/// object said ([`Reported`]), priced like every other row. `quantities` is
/// what the relay knows, `Quantities::answered()` once the upstream's 2xx
/// arrived.
///
/// The caller opened the in-flight gauge when it sent the request
/// ([`crate::telemetry::TelemetryBus::request_started`]); this row closes it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_passthrough(
    state: &SharedState,
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
    degraded: Option<String>,
    reported: &Reported,
    quantities: Quantities,
) {
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
        degraded,
        reported,
        quantities,
    )
    .await;
}

/// Row writer for callers that have a free-form `ingress_proto` and may have no
/// route at all — the gap between [`record`] (keyed on `ClientProto`) and
/// [`crate::proxy::record_in_process`] (route required). Its tokens are
/// `usage`: none for a request that failed before generating any, the
/// upstream's own report for a native passthrough.
///
/// Priced like every row all the same (billable-units design §3.2, §4.5): a
/// local route is a real 0 (Q1), an answered one counts its request, and a
/// token-priced scope stays unpriced for want of tokens — the unpriced
/// remainder, which is exactly what it is.
#[allow(clippy::too_many_arguments)]
async fn record_free_form(
    state: &SharedState,
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
    degraded: Option<String>,
    reported: &Reported,
    quantities: Quantities,
) {
    let usage = &reported.usage;
    let q = with_requests_default(
        quantities,
        route.is_some(),
        status,
        error.as_ref().map(|(k, _)| k.as_str()),
    );
    let priced = price_call(
        state,
        alias,
        route,
        &ctx.key_ref(),
        ingress_proto,
        usage,
        &q,
        &reported.service_tier,
        &reported.hosted_calls,
        reported.local_count.as_ref(),
    );
    let price_note = priced.price_note(reported.price_note.as_deref());
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
        prompt_tokens: usage.prompt_tokens.map(|v| v as i64),
        completion_tokens: usage.completion_tokens.map(|v| v as i64),
        cached_in_tokens: usage.cached_input_tokens.map(|v| v as i64),
        cache_write_tokens: usage.cache_write_tokens.map(|v| v as i64),
        reasoning_tokens: usage.reasoning_tokens.map(|v| v as i64),
        streamed,
        error_kind: error.as_ref().map(|(k, _)| k.clone()),
        error_msg: error.as_ref().map(|(_, m)| m.clone()),
        key_id: priced.key_id,
        fallback_reason: fallback.map(|r| r.as_str().to_string()),
        degraded,
        cost: priced.cost,
        audio_in_ms: quantity_column(q.audio_in_ms),
        chars_in: quantity_column(q.chars_in),
        images_out: quantity_column(q.images_out),
        via: ctx.via,
        service_tier: reported.service_tier.relayed(),
        // An answer whose usage never came has that as its reason; its tier
        // is moot then, its hosted calls are not.
        price_note,
        priced_alias: priced.priced_alias,
        ..Default::default()
    };
    // A stamped native passthrough's answer is the run's like any `/v1`
    // row, its tokens included; one that reported none stays unpriced on a
    // token-priced scope, and then a gap in the run as it is in Usage —
    // never a partial sum without it.
    note_on_run(state, ctx.run, &row, usage, q.requests);
    // Its own task, as `record`'s: a handler dropped mid-insert neither
    // loses the row nor leaves the gauge open (`recording/write.rs`).
    write::write_row(state, row, status).await;
}

mod quantities;
pub(super) use quantities::{quantity_column, with_requests_default};

mod write;
pub(super) use write::summary as row_summary;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod row_tests;

#[cfg(test)]
mod run_tests;
