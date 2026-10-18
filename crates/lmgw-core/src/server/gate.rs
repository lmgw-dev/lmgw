//! Who a request is, and whether its route lets it in (principals design
//! §3.5): [`principal_mw`] resolves the principal once, at the router root,
//! and [`gate`] — the layer every route declares its capability with
//! ([`require`]) — decides.
//!
//! The device listener's half (device link design §4.6, §4.7) is
//! `device_gate`: there the principal is the client certificate's device,
//! a route is served only if it is on the listener's face, and a connection
//! without a certificate reaches enrolment alone.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::device_gate::NoCertificate;
use super::{reasoning_from_headers, Conn, ServedAt, Via};
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::principal::{Cap, Principal, Refusal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// Resolve the principal, once, at the router root (principals §3.5).
///
/// **Resolution only.** It parses the reasoning headers, looks the presented
/// credential up in the snapshot, reads `X-Lmgw-Run`, fills the derived fields
/// telemetry and pricing have always read, and inserts the [`RequestCtx`]. It
/// decides nothing: which capability a route needs is declared beside the
/// route, and [`require`] is what asks whether this principal holds it.
///
/// The one thing it *refuses* is a disabled row (§3.3) — and even that it does
/// not **render**, because the dialect belongs to the route: the refusal rides
/// along in the extensions and `require` answers it in `/v1`'s dialect or the
/// dashboard's, whichever this route speaks. Every registered route carries a
/// `require` (the §3.2 table has no gaps), so nothing slips past a pending one.
///
/// On the device listener (`Conn::via`) the principal comes from the
/// connection's client certificate instead, and a presented credential is a
/// refusal there (`device_gate`, device link design §4.7).
pub(super) async fn principal_mw(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Response {
    // The revocation mark before the snapshot the credential is resolved
    // against (review W2-5): a Disable or Rotate whose reload this snapshot
    // missed raised its signal after this mark, so the request's
    // connections still see it.
    let mark = state.devices.revocations.mark();
    let snap = state.snapshot();
    // Parsed for every route in one place, but **enforced by none of them
    // here**: only the three chat-shaped routes consume a reasoning control,
    // and they turn a parse failure into a 400 themselves (`reasoning_or_400`).
    // A typo'd header on `GET /v1/models` or `/v1/embeddings` is not a reason
    // to refuse a request that never looks at it.
    let mut ctx = RequestCtx {
        reasoning: reasoning_from_headers(req.headers())
            .map(|c| c.map(Ok))
            .unwrap_or_else(|e| {
                // The bare message: `reasoning_or_400` wraps it in a fresh
                // `BadRequest`, whose Display adds the "bad request: " prefix
                // exactly once.
                Some(Err(match e {
                    GatewayError::BadRequest(m) => m,
                    other => other.to_string(),
                }))
            }),
        anthropic_beta: crate::egress::anthropic::client_betas(req.headers()),
        // Read before the snapshot the credential is resolved against: a
        // revocation raised from then on is one this request's connections
        // must still see.
        revocation_mark: Some(mark),
        // The listener it came in on, and that listener's generation (the
        // serving loop puts it on every request).
        served_at: req.extensions().get::<ServedAt>().copied(),
        ..Default::default()
    };
    // Which listener (device link design §4.5): a router served without the
    // connect info (a suite's own `axum::serve`) is the plain listener's.
    let conn = req
        .extensions()
        .get::<axum::extract::ConnectInfo<Conn>>()
        .map(|c| c.0.clone());
    ctx.via = conn.as_ref().map_or(Via::Plain, |c| c.via);
    if let Some(conn) = conn.filter(|c| c.via == Via::Devices) {
        return super::device_gate::resolve(&state, &snap, &conn, ctx, req, next).await;
    }

    // §3.6, and only here: a cookie is the one credential a foreign page can
    // make a browser attach on its own. A bearer principal skips the rule —
    // `curl` sends no `Origin`, and a page that holds a bearer did not get it
    // from the browser.
    //
    // The browser's key on `/v1/realtime` (realtime design §10.1): a page
    // cannot set headers on a WebSocket, so it offers the key as an
    // `openai-insecure-api-key.<key>` subprotocol. Read here and only for that
    // path — `realtime::subprotocol_key` answers `None` everywhere else —
    // rather than in `token::presented`, which every route shares: widening
    // that would make the header a credential on all of them. A presented
    // bearer still wins, and like a bearer it is a deliberate credential, so
    // the cookie rule below does not apply to it.
    let bearer = crate::agents::token::presented(req.headers()).is_some();
    let subprotocol_key =
        crate::realtime::subprotocol_key(req.uri().path(), req.headers()).filter(|_| !bearer);
    let by_cookie = !bearer
        && subprotocol_key.is_none()
        && crate::principal::session_cookie(req.headers()).is_some();
    let resolved = match subprotocol_key {
        Some(key) => crate::principal::from_bearer(&snap, key),
        None => crate::principal::resolve(req.headers(), &snap),
    };
    // A cookie that matched nothing authenticated nothing, and the rule is
    // about cookies that authenticate — so it is a cookie which *resolved to
    // something*, refusal included, that the origin is judged for. And judged
    // **first**: what a page on another port hears must not depend on the row
    // behind the cookie it replayed, or `owner_key_disabled` tells it which
    // of the owner's keys it is holding.
    let resolved = if by_cookie && !matches!(resolved, Ok(Principal::Anonymous)) {
        crate::principal::same_origin(req.headers()).and(resolved)
    } else {
        resolved
    };

    let who = match resolved {
        Ok(who) => who,
        Err(refusal) => {
            // Nothing authenticated, so the derived fields stay empty and the
            // request travels on as `Anonymous` carrying its refusal.
            req.extensions_mut().insert(refusal);
            req.extensions_mut().insert(ctx);
            return next.run(req).await;
        }
    };

    fill(&state, &mut ctx, who, req.headers());
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// `who` as `ctx`'s principal, with the derived fields.
///
/// The two derived fields, unchanged in meaning (§3.5): `client_key` is the
/// label Logs prints, and `agent` the capability the `/mcp` allow list and
/// the run meter read. They follow the principal now instead of being filled
/// by three separate branches — including with **Require API key** off,
/// where a presented key used to be resolved for agents only and dropped on
/// the floor for everyone else.
pub(super) fn fill(
    state: &SharedState,
    ctx: &mut RequestCtx,
    who: Principal,
    headers: &axum::http::HeaderMap,
) {
    if let Principal::Key {
        id,
        name,
        kind,
        agent_id,
        ..
    } = &who
    {
        ctx.client_key = Some(name.clone());
        if let (crate::config::ApiKeyKind::Agent, Some(agent_id)) = (kind, agent_id) {
            ctx.agent = Some(crate::agents::AgentIdentity {
                agent_id: agent_id.clone(),
                key_id: *id,
                key_name: name.clone(),
            });
        }
    }
    ctx.run = run_header(state, headers, ctx.agent.as_ref());
    ctx.principal = who;
}

// ---------------------------------------------------------------------------
// The gate (principals §3.5)
// ---------------------------------------------------------------------------

/// What one `require` layer carries: the capability the route declared, and
/// whether it is the single route that also accepts `x-lmgw-admin-token`.
type GateState = (SharedState, Cap, bool);

/// [`gate`] as a plain function pointer, which is what makes [`GateLayer`]
/// nameable: an `async fn` returns an opaque future, so its own type cannot be
/// written down and a `fn require(..) -> ..` would have no return type.
type GateFn = fn(State<GateState>, Request, Next) -> futures::future::BoxFuture<'static, Response>;

pub(crate) type GateLayer = middleware::FromFnLayer<GateFn, GateState, (State<GateState>, Request)>;

/// The layer a route declares its capability with (§3.5).
///
/// Group-level `route_layer` where a whole router shares one capability,
/// `Handler::layer` where one path carries two (`/api/agents/{id}/runs`,
/// `/mcp/admin`). Both run **after** routing, so an unknown path is still a
/// 404 rather than a 401 that would tell a stranger which paths exist.
pub(crate) fn require(state: &SharedState, cap: Cap) -> GateLayer {
    middleware::from_fn_with_state((state.clone(), cap, false), gate as GateFn)
}

/// `require(Admin)` plus the third spelling of the bearer that
/// `POST /mcp/admin` — and nothing else — accepts (§3.3).
///
/// Read here rather than in `token::presented` precisely so that it widens
/// nothing: the MCP client configs that send `x-lmgw-admin-token` today keep
/// working, and the header stays inert on `/v1`, on the ledger and on `/api`.
pub(crate) fn require_admin_token(state: &SharedState) -> GateLayer {
    middleware::from_fn_with_state((state.clone(), Cap::Admin, true), gate as GateFn)
}

fn gate(
    State((state, cap, admin_token)): State<GateState>,
    mut req: Request,
    next: Next,
) -> futures::future::BoxFuture<'static, Response> {
    Box::pin(async move {
        let snap = state.snapshot();
        let mut ctx = req
            .extensions()
            .get::<RequestCtx>()
            .cloned()
            .unwrap_or_default();
        let mut pending = req.extensions().get::<Refusal>().cloned();
        // `OriginalUri`: this layer runs inside the `/v1` nest, whose
        // request URI has the prefix stripped.
        let path = req
            .extensions()
            .get::<axum::extract::OriginalUri>()
            .map_or_else(|| req.uri().path().to_string(), |u| u.path().to_string());
        let label = refusal_label(&path);

        // The listener's face first (device link design §4.6): a route off
        // it is the 404 an unknown path is, before any credential is judged.
        if !super::face::serves_on(ctx.via, req.method(), &path, cap) {
            return super::face::not_found();
        }
        // A device listener connection without a client certificate reaches
        // its own routes alone (§4.7): every other route on its face is a
        // 401 that writes no request row.
        if req.extensions().get::<NoCertificate>().is_some()
            && !super::face::is_device_route(req.method(), &path)
        {
            return super::device_gate::certificate_required(cap, req.headers());
        }

        if admin_token {
            if let Some(spelled) = req
                .headers()
                .get("x-lmgw-admin-token")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                match crate::principal::from_bearer(&snap, spelled) {
                    // A value that matches no row leaves whatever the root
                    // resolved standing: the header is an extra door, not a
                    // way to *lose* a session the request already carried.
                    Ok(Principal::Anonymous) => {}
                    Ok(who) => {
                        ctx.client_key = who.key_id().and_then(|id| {
                            snap.api_keys
                                .iter()
                                .find(|k| k.id == id)
                                .map(|k| k.name.clone())
                        });
                        ctx.principal = who;
                        // The **whole** ctx describes the owner now. An agent
                        // bearer that came in alongside the header had filled
                        // `agent` and `run` at the root, and leaving those
                        // standing would attribute this Admin-plane call to
                        // that agent and meter it against its run (§3.5) —
                        // the header is a different principal, not a second
                        // hat on the same one.
                        ctx.agent = None;
                        ctx.run = None;
                        pending = None;
                        req.extensions_mut().insert(ctx.clone());
                    }
                    Err(refusal) => pending = Some(refusal),
                }
            }
        }

        // A `Public` route needs no credential, so a **bad** one is not its
        // business: the pending refusal (§3.5 — a disabled row, resolved at
        // the root and rendered by whichever `require` the route carries) is
        // dropped here and rendered everywhere else. Without this, a browser
        // holding the cookie of a second owner key the owner has just
        // disabled is answered `401 owner_key_disabled` by `GET /`, never
        // sees the login card, and has no way back in but the process log.
        // It stays `Anonymous` for the handler, which is what it is.
        if let Some(refusal) = pending.filter(|_| cap != Cap::Public) {
            return refuse(&state, &ctx, cap, refusal, req.headers(), label).await;
        }
        if !ctx.principal.holds(cap, &snap) {
            // `/v1` with **Require API key** on and nothing presented: the
            // wording every OpenAI-shaped client has read since the first
            // release, unchanged and in its own dialect (§3.9).
            if cap == Cap::Inference && matches!(ctx.principal, Principal::Anonymous) {
                let e = GatewayError::Unauthorized("missing or invalid gateway API key");
                return policy_refusal(&state, &ctx, &e, req.headers(), label).await;
            }
            let refusal = match &ctx.principal {
                // Nothing was presented at all, so there is no row to name —
                // the owner needs the login link, not a forbidden.
                Principal::Anonymous => Refusal::session_required(),
                other => Refusal::forbidden(cap, other),
            };
            return refuse(&state, &ctx, cap, refusal, req.headers(), label).await;
        }

        // A key's expiry on `Chat` too (client-apps design §1.2, L17): a
        // device past its `expires_at` is refused at the door, 401
        // `key_expired` with the date. Expiry alone — no slot is taken and no
        // window counted: a feed or a host link holds no slot, and the model
        // calls inside a turn are checked one by one.
        if cap == Cap::Chat {
            if let Some(id) = ctx.principal.key_id() {
                // Gone, or rotated, between the root's resolution and this
                // gate (review W2-5): the credential opens nothing now. A
                // device is told to pair again, as the root tells one whose
                // row was already gone.
                let Some(key) = snap
                    .api_keys
                    .iter()
                    .find(|k| k.id == id && ctx.principal.presents(k))
                else {
                    let refusal = if ctx.principal.is_device_key() {
                        Refusal::device_key_unknown()
                    } else {
                        Refusal::session_required()
                    };
                    return refuse(&state, &ctx, cap, refusal, req.headers(), label).await;
                };
                if let Err(at) = crate::policy::check_expiry(key, chrono::Utc::now()) {
                    let refusal = Refusal::key_expired(key, &at);
                    return refuse(&state, &ctx, cap, refusal, req.headers(), label).await;
                }
            }
            return next.run(req).await;
        }
        // Expiry, requests/minute, tokens/minute and concurrency
        // (usage-analytics §4.2) — on `Inference` and nowhere else (§3.5). An
        // agent flushing a hundred ledger events a minute is not spending its
        // request budget, and a dashboard read never was. The two checks that
        // need the alias, scope and budget, stay post-body in
        // `policy::check_alias`; the alias is in the body.
        if cap != Cap::Inference {
            return next.run(req).await;
        }
        let Some(key) = ctx
            .principal
            .key_id()
            .and_then(|id| snap.api_keys.iter().find(|k| k.id == id))
        else {
            return next.run(req).await;
        };
        // A call's hangup ends work and starts none (realtime calls design
        // C3): no slot and no count, so a key at its concurrency limit can
        // still hang up. Its expiry alone, as on `Chat`.
        if crate::realtime::is_hangup(&path) {
            if let Err(at) = crate::policy::check_expiry(key, chrono::Utc::now()) {
                let refusal = Refusal::key_expired(key, &at);
                return refuse(&state, &ctx, cap, refusal, req.headers(), label).await;
            }
            return next.run(req).await;
        }
        // A realtime session is not a model call: its calls are counted one
        // by one as it makes them, so the upgrade — and a WebRTC call's POST
        // (calls design §2.3) — takes the slot uncounted (realtime design
        // §10.3).
        let admitted = if path == crate::realtime::PATH || path == crate::realtime::CALLS_PATH {
            state.policy.admit_session(key, chrono::Utc::now())
        } else {
            state.policy.admit(key, chrono::Utc::now())
        };
        let guard = match admitted {
            Ok(g) => g,
            Err(e) => return policy_refusal(&state, &ctx, &e, req.headers(), label).await,
        };
        // Offered to the handler first: a realtime session's response is the
        // 101, whose body ends at the upgrade, so it takes the slot into the
        // session instead (realtime design §10.3, `SlotHandover`). Every other
        // handler leaves it here.
        let handover = guard.map(crate::policy::SlotHandover::new);
        if let Some(h) = &handover {
            req.extensions_mut().insert(h.clone());
        }
        let resp = next.run(req).await;
        // The guard must outlive the *body*, not the handler: a streamed
        // response is built in microseconds and then drains for minutes on a
        // spawned task, so dropping here would let a `concurrency_limit` of 1
        // admit fifty concurrent streams. `hold_until_body_end` moves it into
        // the body's own lifetime, which is the thing the limit is about.
        hold_until_body_end(resp, handover.and_then(|h| h.take()))
    })
}

/// Render a gate refusal in the dialect the route speaks (§3.5).
///
/// `Inference` is `/v1` and `/mcp`, whose clients are OpenAI- and
/// Anthropic-shaped SDKs that parse `{"error": {…}}` and nothing else — and a
/// refusal there is traffic worth a log row, which is the other half of what
/// `policy_refusal` does. Every other capability answers the flat
/// `{ "code", "message" }` the SPA's `decode` already reads.
async fn refuse(
    state: &SharedState,
    ctx: &RequestCtx,
    cap: Cap,
    refusal: Refusal,
    headers: &axum::http::HeaderMap,
    label: ClientProto,
) -> Response {
    if cap != Cap::Inference {
        return refusal.into_response();
    }
    let e = GatewayError::Refused {
        status: refusal.status.as_u16(),
        code: refusal.code,
        message: refusal.message,
    };
    policy_refusal(state, ctx, &e, headers, label).await
}

/// `X-Lmgw-Run: <job id>` — which run a request belongs to (container-runtime
/// §3.1).
///
/// A header rather than a token claim, because one token serves every run, and
/// a header works identically on the JSONL transport's sibling HTTP calls, in
/// service mode, and from a container lmgw did not start.
///
/// **Only an agent token may attribute, and only to its own runs.** The header
/// moves money: whatever it names has its cost, tokens and call count folded
/// into the job's `result`, which the Runs tab and the cost line read. Without
/// the identity check any local process — and `auth_enabled` is off by default,
/// so that means any process at all — could bill an arbitrary live run by
/// guessing a small integer, and the owner would read the inflated figure as
/// something their agent did. A caller who cannot own a run cannot stamp one.
///
/// An unknown or foreign id from a caller who *could* own one is still
/// **ignored with a log line, never a refusal**: a mis-stamped request is a
/// request the owner made, and refusing it would turn a bookkeeping slip into
/// an outage.
fn run_header(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    agent: Option<&crate::agents::AgentIdentity>,
) -> Option<i64> {
    let raw = headers.get("x-lmgw-run")?.to_str().ok()?.trim().to_string();
    let Some(agent) = agent else {
        tracing::debug!(
            "X-Lmgw-Run '{raw}' ignored: the caller presented no agent token, and only an agent \
             may bill its own run"
        );
        return None;
    };
    let Ok(id) = raw.parse::<i64>() else {
        tracing::info!("X-Lmgw-Run: '{raw}' is not a job id — the request is not attributed");
        return None;
    };
    let Some(job) = state.jobs.live_one(id) else {
        tracing::info!("X-Lmgw-Run: {id} names no run in flight — the request is not attributed");
        return None;
    };
    if job.kind != crate::agents::JOB_KIND {
        tracing::info!("X-Lmgw-Run: job {id} is a '{}', not an agent run", job.kind);
        return None;
    }
    let mine = crate::agents::job_key(&agent.agent_id);
    if job.key.as_deref() != Some(mine.as_str()) {
        tracing::info!(
            "X-Lmgw-Run: run {id} is not a run of '{}' — the request is not attributed",
            agent.agent_id
        );
        return None;
    }
    Some(id)
}

/// Keep `guard` alive until the response body has been fully consumed — or
/// dropped, when the client goes away.
///
/// `next.run()` returns as soon as the handler has *built* the response, and a
/// streamed chat response is built in microseconds and then drained for minutes
/// by a spawned task. Dropping the concurrency slot there would let a
/// `concurrency_limit` of 1 admit fifty simultaneous streams while looking
/// perfectly correct in a non-streaming smoke test.
fn hold_until_body_end(resp: Response, guard: Option<crate::policy::ConcurrencyGuard>) -> Response {
    let Some(guard) = guard else { return resp };
    let (parts, body) = resp.into_parts();
    // `unfold` owns the guard for the life of the stream; the guard's Drop runs
    // when the last frame is read or the stream is dropped, whichever happens.
    let stream = futures::stream::unfold(
        (body.into_data_stream(), guard),
        |(mut inner, guard)| async move {
            use futures::StreamExt;
            inner.next().await.map(|item| (item, (inner, guard)))
        },
    );
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// The `request_logs` label of a refusal on `path`: a realtime upgrade's
/// rows say `realtime` like the session's own (realtime design §11), and so
/// do a WebRTC call's POST and hangup (realtime calls design §2.3), a
/// System One request's say `systemone` like its handler's (decision-models
/// design §2.6), the rest of `/v1` and `/mcp` say `openai`.
fn refusal_label(path: &str) -> ClientProto {
    match path {
        crate::realtime::PATH | crate::realtime::CALLS_PATH => ClientProto::Realtime,
        p if crate::realtime::is_hangup(p) => ClientProto::Realtime,
        crate::proxy::systemone::PATH => ClientProto::SystemOne,
        _ => ClientProto::OpenaiChat,
    }
}

async fn policy_refusal(
    state: &SharedState,
    ctx: &RequestCtx,
    e: &GatewayError,
    headers: &axum::http::HeaderMap,
    label: ClientProto,
) -> Response {
    // A refusal is traffic. Without a row, the one refusal class most likely to
    // be hit in a loop — a rate limit — is the one class Logs, Traffic and the
    // Usage page can never show, and "why did my agent stop working" is
    // answerable only from the client's side of the connection.
    crate::proxy::record_middleware_refusal(state, ctx, e, label).await;
    let body = if headers.contains_key("anthropic-version") {
        e.to_anthropic_json()
    } else {
        e.to_openai_json()
    };
    let mut resp = (e.http_status(), Json(body)).into_response();
    if let GatewayError::KeyRate { retry_after, .. } = e {
        if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    resp
}
