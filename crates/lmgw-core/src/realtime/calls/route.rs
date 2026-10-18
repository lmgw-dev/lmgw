//! `POST /v1/realtime/calls` and `POST /v1/realtime/calls/{call_id}/hangup`
//! (calls design §2): OpenAI's WebRTC transport for the Realtime API.
//!
//! **The POST** checks what the WebSocket's handshake checks, in its order
//! (§2.2), and mutates nothing: the door (a beta client, an anonymous page
//! from another origin), the body's shape, the query with its parameter
//! rules and the sideband refusal, the listener's socket group (§3.1, the
//! 503 before anything that costs), the body's parts (`body`), then for a
//! bound call the bind's checks (`thread::check`; the folder's lock is let
//! go before the answer) and for an unbound one the models' resolution and
//! the key's scope and budget. Only then is the
//! offer read and answered ([`super::Calls::offer`], §2.4): `201 Created`
//! with the SDP answer and `Location: /v1/realtime/calls/{call_id}`. The
//! call holds the key's concurrency slot from here until it ends (C3), and
//! its owner is the open path (`open`), which binds when `oai-events` opens.
//!
//! | Status | `code` | When |
//! |---|---|---|
//! | 400 | `invalid_value` | the query does not parse or breaks a parameter rule; the `session` part is not a JSON object, or is over `realtime.max_message_mb` |
//! | 400 | `bad_request` | a multipart body that does not parse |
//! | 400 | `sideband_unsupported` | `call_id` (§2.7) |
//! | 400 | `owned_by_thread` | `model` beside `chat_thread` or `chat_folder` |
//! | 400 | `missing_sdp` | a multipart body without `sdp` |
//! | 400 | `invalid_sdp` | the offer does not parse (str0m's reason quoted) |
//! | 400 | `unsupported_offer` | no Opus audio, or no data channel |
//! | 400 | `beta_protocol_unsupported` | `OpenAI-Beta: realtime=v1` |
//! | 403 | `cross_origin_refused` | anonymous, foreign `Origin` |
//! | 404, 409 | the bind's | `thread::check`'s refusals, `chat_thread_bound` with `error.held_by` under `takeover=never` |
//! | 413 | the body-limit refusal | over `max_body_mb` |
//! | 415 | `unsupported_media_type` | neither `application/sdp` nor `multipart/form-data` |
//! | 503 | `webrtc_unavailable` | the listener's socket group (the plain one, or the device listener's) is off, failed or down |
//!
//! **The hangup** asks the call's owner to end it and answers `200` once the
//! call let go — its binding marked closing, its slot back (C3) — bounded by
//! [`STOP_WITHIN`], past which the call is closed by force
//! ([`super::Calls::hangup_within`]), its binding then marked closing only
//! once its owner notices (§8.2). A call that let go and is still closing
//! (its last events not acknowledged yet, §8.5) is closed at once, and the
//! answer is `200` too. It takes no slot and counts no request
//! (the gate, by [`is_hangup`]). The key that made the call may hang it up,
//! and the owner; anyone else, and a call that ended, gets 404
//! `call_not_found`, so nobody learns of another's call.

use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde_json::json;

use super::super::handshake::{self, Door};
use super::super::{thread, RealtimeQuery};
use super::open::{self, Opening, What};
use super::{CallParams, Group, Offer, OfferError};
use crate::config::ApiKeyKind;
use crate::policy::SlotHandover;
use crate::principal::Principal;
use crate::proxy::RequestCtx;
use crate::server::{Conn, STOP_WITHIN};
use crate::state::SharedState;
use crate::web::chat_caller::Caller;

mod body;

/// The POST's path.
pub const PATH: &str = "/v1/realtime/calls";

/// Whether `path` is a call's hangup, `/v1/realtime/calls/{call_id}/hangup`:
/// what the gate admits without a slot or a count (C3).
pub fn is_hangup(path: &str) -> bool {
    path.strip_prefix(PATH)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_suffix("/hangup"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// A refusal in the handshake's OpenAI error shape: `invalid_request_error`,
/// or `server_error` for a 5xx.
fn refuse(status: StatusCode, code: &str, message: String) -> Response {
    let kind = if status.is_server_error() {
        "server_error"
    } else {
        "invalid_request_error"
    };
    handshake::http_error(status, kind, code, message)
}

/// [`refuse`] naming the parameter at fault.
fn refuse_param(status: StatusCode, code: &str, message: String, param: &str) -> Response {
    let body = json!({
        "error": {"message": message, "type": "invalid_request_error", "param": param,
                  "code": code}
    });
    (status, axum::Json(body)).into_response()
}

/// 503 `webrtc_unavailable`, naming why (§2.5).
fn unavailable(why: &str) -> Response {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "webrtc_unavailable",
        format!("WebRTC calls are unavailable on this listener: {why}"),
    )
}

/// An offer's refusal (§2.5).
fn offer_refused(e: OfferError) -> Response {
    match e {
        OfferError::Unavailable(why) => unavailable(&why),
        OfferError::Moving => {
            let mut r = unavailable(&format!(
                "the device group is moving with the device listener and is back within {} s \
                 (server::STOP_WITHIN); retry after the Retry-After",
                STOP_WITHIN.as_secs()
            ));
            r.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from(STOP_WITHIN.as_secs()),
            );
            r
        }
        OfferError::Invalid(why) => body::invalid_sdp(&why),
        OfferError::Unsupported(why) => {
            refuse(StatusCode::BAD_REQUEST, "unsupported_offer", why.into())
        }
    }
}

/// `POST /v1/realtime/calls` (module doc).
pub(crate) async fn create(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    slot: Option<Extension<SlotHandover>>,
    query: Result<Query<RealtimeQuery>, QueryRejection>,
    req: Request,
) -> Response {
    if let Some(refused) = handshake::door(&state, &ctx, req.headers(), Door::Call).await {
        return refused;
    }
    let shape = match body::shape(req.headers()) {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    let query = match handshake::query(query) {
        Ok(q) => q,
        Err(refused) => return refused,
    };
    let binding = match query.binding() {
        Ok(b) => b,
        Err(refused) => return refused.into_response(),
    };
    // The connection the POST came in on: its listener picks the socket
    // group, its local address is the call's one candidate (§3.2). The 503
    // comes before anything that costs, the body's read included.
    let conn = req
        .extensions()
        .get::<ConnectInfo<Conn>>()
        .map(|c| c.0.clone());
    let Some(conn) = conn else {
        return unavailable("the connection's local address is not known to this server");
    };
    let group = Group::of(conn.via);
    if let Err(e) = state.calls.ready(group, ctx.served_at) {
        return offer_refused(e);
    }
    let posted = match body::read(&state, req, shape).await {
        Ok(p) => p,
        Err(refused) => return refused,
    };
    // A revocation from here to the channel's open ends the call (§2.8).
    let revocation = crate::devices::watch(&state, &ctx);
    let requested = query.model;
    let what = match binding {
        // The checks only: nothing is made and nothing is bound, and a
        // folder's lock goes before the answer (§2.8).
        Some((target, takeover)) => {
            let checked = thread::check(&state, &ctx, target, requested.is_some(), takeover);
            if let Err(refused) = checked.await {
                return refused.into_response();
            }
            What::Bound { target, takeover }
        }
        None => match handshake::unbound(&state, &ctx, requested.as_deref()).await {
            Ok(resolved) => What::Unbound {
                requested,
                resolved: Box::new(resolved),
            },
            Err(refused) => return refused,
        },
    };
    // Counted by a stopping server from the POST on, as an upgrade is from
    // before its 101 (`SessionInit::running`).
    let running = state.stops.running_at(state.stops.at_or_now(ctx.served_at));
    let slot = slot.and_then(|Extension(h)| h.take());
    let offer = Offer {
        sdp: posted.sdp,
        group,
        local: conn.local.ip(),
        served_at: ctx.served_at,
        creator: ctx.principal.clone(),
        params: CallParams::from_settings(&state.snapshot().settings.realtime),
        keep: slot.map(|g| Box::new(g) as Box<dyn std::any::Any + Send>),
    };
    let answered = match state.calls.offer(offer).await {
        Ok(a) => a,
        Err(e) => return offer_refused(e),
    };
    let id = answered.handle.id().to_string();
    tokio::spawn(open::run(Opening {
        state,
        ctx,
        handle: answered.handle,
        what,
        session: posted.session,
        revocation,
        running,
    }));
    (
        StatusCode::CREATED,
        [
            (header::CONTENT_TYPE, "application/sdp".to_string()),
            (header::LOCATION, format!("{PATH}/{id}")),
        ],
        answered.sdp,
    )
        .into_response()
}

/// `POST /v1/realtime/calls/{call_id}/hangup` (module doc).
pub(crate) async fn hangup(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(call_id): Path<String>,
) -> Response {
    let not_found = || {
        refuse(
            StatusCode::NOT_FOUND,
            "call_not_found",
            format!("there is no live call '{call_id}' for this credential"),
        )
    };
    let Some(info) = state.calls.get(&call_id) else {
        return not_found();
    };
    if !may_hang_up(&ctx.principal, &info.creator) {
        return not_found();
    }
    // Named as `voice.bound.by` names a client: what a bound call's
    // `lmgw.session.closed {kind: "closed", by}` says (§2.6).
    let by = (ctx.principal != Principal::Anonymous).then(|| Caller::of(&ctx).named());
    match state.calls.hangup_within(&call_id, by, STOP_WITHIN).await {
        Some(()) => StatusCode::OK.into_response(),
        None => not_found(),
    }
}

/// Whether `who` may hang up a call `creator` made (§2.6): the same key, or
/// the owner. With **Require API key** off an anonymous caller is one, and
/// hangs up the anonymous calls.
fn may_hang_up(who: &Principal, creator: &Principal) -> bool {
    match who {
        Principal::Key {
            kind: ApiKeyKind::Owner,
            ..
        } => true,
        Principal::Anonymous => *creator == Principal::Anonymous,
        Principal::Key { id, .. } => creator.key_id() == Some(*id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hangup_path_is_told_from_the_others() {
        assert!(is_hangup("/v1/realtime/calls/rtc_0123/hangup"));
        assert!(!is_hangup("/v1/realtime/calls"));
        assert!(!is_hangup("/v1/realtime/calls//hangup"));
        assert!(!is_hangup("/v1/realtime/calls/a/b/hangup"));
        assert!(!is_hangup("/v1/realtime/calls/rtc_0123/accept"));
        assert!(!is_hangup("/v1/realtime"));
    }

    /// The gap of a device group's move is its own 503: it names the move
    /// and the bound, and says when to retry.
    #[test]
    fn the_gap_of_a_move_is_a_503_with_retry_after() {
        let r = offer_refused(OfferError::Moving);
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            r.headers().get(header::RETRY_AFTER).unwrap(),
            &STOP_WITHIN.as_secs().to_string()
        );
    }

    /// Each listener's calls take its own socket group (§3.1).
    #[test]
    fn each_listener_has_its_group() {
        use crate::server::Via;
        assert_eq!(Group::of(Via::Plain), Group::Plain);
        assert_eq!(Group::of(Via::Devices), Group::Devices);
    }

    #[test]
    fn a_call_is_hung_up_by_its_key_or_the_owner() {
        let key = |id, kind| Principal::Key {
            id,
            name: "k".into(),
            kind,
            agent_id: None,
            fingerprint: String::new(),
        };
        let device = key(7, ApiKeyKind::Device);
        assert!(may_hang_up(&device, &device));
        assert!(!may_hang_up(&key(8, ApiKeyKind::Device), &device));
        assert!(may_hang_up(&key(1, ApiKeyKind::Owner), &device));
        assert!(!may_hang_up(&Principal::Anonymous, &device));
        assert!(may_hang_up(&Principal::Anonymous, &Principal::Anonymous));
        assert!(!may_hang_up(&device, &Principal::Anonymous));
    }
}
