//! The principal on the device listener (device link design §4.7).
//!
//! - **With a client certificate**, the principal is the certificate's
//!   device, resolved on every request against the snapshot now (D5), so
//!   Disable, Delete and expiry act on the next request of a kept-alive
//!   connection: a presented bearer, `x-api-key`, session cookie or key
//!   subprotocol is 400 `credential_on_device_listener`; a certificate past
//!   its `notAfter` 401 `device_certificate_expired`; then the row the key
//!   opens (`devices::KeyLookup`): none is `device_key_unknown`, a disabled
//!   one `device_disabled`, an unconfirmed one 403 `device_unconfirmed`
//!   naming the code, and an enabled, confirmed one the device's
//!   `Principal::Key`. Each refusal is pending, rendered by the route's gate
//!   as every pending refusal is.
//! - **Without one**, the principal is `Anonymous` and the request carries
//!   [`NoCertificate`]: the gate answers every route but the device
//!   listener's own 401 `client_certificate_required`, whatever *Require API
//!   key* says, and writes no request row ([`certificate_required`]); the
//!   serving loop puts `Connection: close` on every answer.
//! - **The device listener's own routes** (`DEVICE_ROUTES`) judge no
//!   credential and no certificate: enrolment's token decides (§3.4).
//! - **A device let in with the certificate its pairing token answered**
//!   ends that token (D8's note of 2026-10-15, `devices::tokens::answered`):
//!   it holds its answer, so the handshake without a certificate closes.

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::device_link::{code, unconfirmed_message};

use super::{Conn, DevicePeer};
use crate::config::Snapshot;
use crate::devices::KeyMatch;
use crate::error::GatewayError;
use crate::principal::{Cap, Principal, Refusal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// A device listener request whose connection presented no client
/// certificate.
#[derive(Debug, Clone, Copy)]
pub(super) struct NoCertificate;

/// `principal_mw`'s device listener half.
pub(super) async fn resolve(
    state: &SharedState,
    snap: &Snapshot,
    conn: &Conn,
    mut ctx: RequestCtx,
    mut req: Request,
    next: Next,
) -> Response {
    let own_route = super::face::is_device_route(req.method(), req.uri().path());
    let resolved = match &conn.device {
        _ if own_route => Ok(Principal::Anonymous),
        None => {
            req.extensions_mut().insert(NoCertificate);
            Ok(Principal::Anonymous)
        }
        Some(peer) => device_principal(state, snap, peer, req.uri().path(), req.headers()),
    };
    match resolved {
        Ok(who) => {
            let device = conn.device.as_ref().zip(who.key_id());
            // Its long-lived connections end at the certificate's date.
            ctx.cert_not_after = device.map(|(p, _)| p.not_after);
            ctx.conn = device.map(|_| conn.clone());
            // A confirmed device's request with the certificate its pairing
            // token answered: the token has done its work (D8's note of
            // 2026-10-15).
            if let Some((peer, id)) = device {
                crate::devices::tokens::answered(state, id, &peer.serial);
            }
            super::gate::fill(state, &mut ctx, who, req.headers())
        }
        Err(refusal) => {
            req.extensions_mut().insert(refusal);
        }
    }
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// The device `peer`'s certificate opens, for a request to `path` with
/// `headers`, as `snap` says now.
fn device_principal(
    state: &SharedState,
    snap: &Snapshot,
    peer: &DevicePeer,
    path: &str,
    headers: &HeaderMap,
) -> Result<Principal, Refusal> {
    if let Some(what) = presented_credential(path, headers) {
        tracing::debug!("a device listener request presented {what}; refused");
        return Err(credential_on_device_listener());
    }
    if peer.lapsed(chrono::Utc::now()) {
        return Err(certificate_expired(peer));
    }
    match state.devices.keys.find(snap, &peer.key_hash()) {
        KeyMatch::Unknown => Err(Refusal::device_key_unknown()),
        KeyMatch::Found { key, .. } if !key.enabled => Err(Refusal::device_disabled(&key.name)),
        KeyMatch::Found {
            confirmed: false, ..
        } => Err(unconfirmed(peer)),
        KeyMatch::Found { key, .. } => Ok(Principal::from_key(&key)),
    }
}

/// The credential a request presents beside its certificate, named for the
/// log line: an `Authorization` header of any scheme, `x-api-key`,
/// `x-lmgw-admin-token`, the session cookie, or a key subprotocol on the
/// realtime path.
fn presented_credential(path: &str, headers: &HeaderMap) -> Option<&'static str> {
    if headers.contains_key(axum::http::header::AUTHORIZATION) {
        return Some("an Authorization header");
    }
    if headers.contains_key("x-api-key") {
        return Some("an x-api-key header");
    }
    if headers.contains_key("x-lmgw-admin-token") {
        return Some("an x-lmgw-admin-token header");
    }
    if crate::principal::session_cookie(headers).is_some() {
        return Some("the session cookie");
    }
    if crate::realtime::subprotocol_key(path, headers).is_some() {
        return Some("a key subprotocol");
    }
    None
}

/// 400 [`code::CREDENTIAL_ON_DEVICE_LISTENER`].
fn credential_on_device_listener() -> Refusal {
    Refusal {
        status: StatusCode::BAD_REQUEST,
        code: code::CREDENTIAL_ON_DEVICE_LISTENER,
        message: "the device listener authenticates by client certificate; send no \
                  Authorization header, API key, cookie or key subprotocol"
            .into(),
    }
}

/// 401 [`code::DEVICE_CERTIFICATE_EXPIRED`], with the date.
fn certificate_expired(peer: &DevicePeer) -> Refusal {
    Refusal {
        status: StatusCode::UNAUTHORIZED,
        code: code::DEVICE_CERTIFICATE_EXPIRED,
        message: format!(
            "this device's certificate lapsed on {}; pair the device again",
            peer.not_after
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
    }
}

/// 403 [`code::DEVICE_UNCONFIRMED`], naming the key's code.
fn unconfirmed(peer: &DevicePeer) -> Refusal {
    Refusal {
        status: StatusCode::FORBIDDEN,
        code: code::DEVICE_UNCONFIRMED,
        message: unconfirmed_message(&peer.confirm_code()),
    }
}

/// 401 [`code::CLIENT_CERTIFICATE_REQUIRED`] for a route that declared
/// `cap`, in its dialect: `/v1`'s error envelope for `Inference` (the
/// Anthropic one when the client says `anthropic-version`), the flat
/// `{code, message}` elsewhere. Unlike every other refusal on `Inference`,
/// no request row: a connection without a certificate writes none (§4.7).
pub(super) fn certificate_required(cap: Cap, headers: &HeaderMap) -> Response {
    let refusal = Refusal {
        status: StatusCode::UNAUTHORIZED,
        code: code::CLIENT_CERTIFICATE_REQUIRED,
        message: "this route needs the device's certificate; pair the device on lmgw's \
                  Devices card"
            .into(),
    };
    if cap != Cap::Inference {
        return refusal.into_response();
    }
    let e = GatewayError::Refused {
        status: refusal.status.as_u16(),
        code: refusal.code,
        message: refusal.message,
    };
    let body = if headers.contains_key("anthropic-version") {
        e.to_anthropic_json()
    } else {
        e.to_openai_json()
    };
    (e.http_status(), Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D5: the certificate's `notAfter` is checked on every request, so a
    /// kept-alive connection whose certificate lapses is refused on its
    /// next one; and the row is read as the snapshot says now.
    #[tokio::test]
    async fn a_lapsed_certificate_is_refused_and_a_confirmed_row_opens_its_device() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let keys = std::sync::Arc::new(crate::devices::MemoryKeys::default());
        state.devices.keys.set(keys.clone());
        let peer = DevicePeer {
            spki_sha256: [7; 32],
            serial: "01".into(),
            not_after: chrono::Utc::now() + chrono::Duration::days(1),
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO api_keys (name, key_hash, enabled, kind) VALUES ('device:p', ?1, 1, \
             'device') RETURNING id",
        )
        .bind(peer.key_hash())
        .fetch_one(&state.db)
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();
        let ask = |peer: &DevicePeer| {
            device_principal(
                &state,
                &state.snapshot(),
                peer,
                "/v1/models",
                &HeaderMap::new(),
            )
        };
        assert_eq!(ask(&peer).unwrap_err().code, "device_key_unknown");
        keys.set(&peer.key_hash(), id, false);
        let refused = ask(&peer).unwrap_err();
        assert_eq!(refused.code, code::DEVICE_UNCONFIRMED);
        assert!(refused.message.contains(&peer.confirm_code()));
        // Disabled and unconfirmed: the switch the owner turned is what it
        // hears, never to wait for a confirmation.
        sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        assert_eq!(ask(&peer).unwrap_err().code, "device_disabled");
        sqlx::query("UPDATE api_keys SET enabled = 1 WHERE id = ?1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        keys.set(&peer.key_hash(), id, true);
        let who = ask(&peer).unwrap();
        assert_eq!(who.key_id(), Some(id));
        assert!(who.is_device_key());
        let lapsed = DevicePeer {
            not_after: chrono::Utc::now() - chrono::Duration::seconds(1),
            ..peer.clone()
        };
        assert_eq!(
            ask(&lapsed).unwrap_err().code,
            code::DEVICE_CERTIFICATE_EXPIRED
        );
        // The gateway's own lookup fails closed until confirmation is
        // stored (WP3): the row is found, and unconfirmed.
        state
            .devices
            .keys
            .set(std::sync::Arc::new(crate::devices::StoredKeys));
        assert_eq!(ask(&peer).unwrap_err().code, code::DEVICE_UNCONFIRMED);
    }

    #[test]
    fn every_credential_spelling_is_named() {
        let mut h = HeaderMap::new();
        assert_eq!(presented_credential("/v1/models", &h), None);
        h.insert("authorization", "Basic eDp5".parse().unwrap());
        assert!(presented_credential("/v1/models", &h).is_some());
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "k".parse().unwrap());
        assert!(presented_credential("/v1/models", &h).is_some());
        let mut h = HeaderMap::new();
        h.insert("cookie", "a=b; lmgw_session=x".parse().unwrap());
        assert!(presented_credential("/chat/api/threads", &h).is_some());
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.k".parse().unwrap(),
        );
        assert!(presented_credential(crate::realtime::PATH, &h).is_some());
        // An unrelated cookie is no credential.
        let mut h = HeaderMap::new();
        h.insert("cookie", "theme=dark".parse().unwrap());
        assert_eq!(presented_credential("/chat/api/threads", &h), None);
    }
}
