//! `POST /devices/enrol` (device link design §3.4, D7): a device sends the
//! link's one-time token with its public key, over TLS pinned to the link's
//! `fp` and without a client certificate, and gets its certificate. The
//! token decides, alone: a client certificate presented anyway, and any
//! credential header, are ignored.
//!
//! - **The token** is taken in the enrolment's one write transaction
//!   (`store::enrol_device`): two devices racing with one token serialize on
//!   the write lock, and one gets the certificate. A used token presented by
//!   the same key within the token's life gets the same answer (its device
//!   lost the first); by another key, 410 `pairing_token_used`, and the row
//!   is marked contested.
//! - **A re-enrolment ends no working key** (§3.5, §5.3, *changed
//!   2026-10-15*): the key the row holds gets a fresh certificate and keeps
//!   its confirmation (a renewal), and a new key beside a confirmed one
//!   waits for the owner's Confirm, which ends the kept one
//!   (`device_confirm`). Whoever photographs a re-pair's QR code gets a
//!   waiting key the owner never confirms, never the working device's end.
//! - **The key** must be a canonical EC P-256 SubjectPublicKeyInfo
//!   (`tls::check_public_key`) that no other device row holds.
//! - **The answer** is 201 with `Connection: close`: the device's next
//!   request comes on a new connection with its certificate, and is refused
//!   403 `device_unconfirmed` until the owner confirms its code (§3.5).
//! - **Failed uses** (an unknown, used, lapsed or ended token) count toward
//!   the address's 429 (`tokens::EnrolFailures`).
//!
//! On the device listener only: the plain listener's face answers it 404
//! (`server::DEVICE_ROUTES`), before its body is read.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use chrono::Utc;
use lmgw_api_types::device_link::{
    code, EnrolRequest, EnrolledDevice, Enrolment, TOKEN_HEX_DIGITS, TOKEN_PREFIX,
};

use super::tokens;
use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store::{self, Enrolled, EnrolledRow};

/// The handler.
pub async fn enrol(State(st): State<SharedState>, req: Request) -> Response {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<crate::server::Conn>>()
        .map(|c| c.0.peer.ip());
    let mut resp = match answer(&st, peer, req.into_body()).await {
        Ok(enrolment) => (StatusCode::CREATED, Json(enrolment)).into_response(),
        Err(refused) => refused,
    };
    // Every answer ends the connection: a connection without a certificate
    // gets this from the serving loop anyway, and one with a certificate
    // that enrolled starts again with the new one (§3.4).
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

async fn answer(
    st: &SharedState,
    peer: Option<std::net::IpAddr>,
    body: Body,
) -> Result<Enrolment, Response> {
    let Some(peer) = peer else {
        // Served without the listener's connect info: no device listener.
        return Err(refusal(StatusCode::NOT_FOUND, "not_found", "no such route"));
    };
    let per_min = st.snapshot().settings.devices.enrol_refuse_per_min;
    if let Err(limited) = st
        .devices
        .failures
        .check(peer, per_min, tokio::time::Instant::now())
    {
        let secs = limited.retry_after.as_secs().max(1);
        let mut resp = refusal(
            StatusCode::TOO_MANY_REQUESTS,
            code::PAIRING_RATE_LIMITED,
            &format!(
                "{per_min} failed enrolments came from this address in the last minute \
                 (devices.enrol_refuse_per_min); try again in {secs} s"
            ),
        );
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return Err(resp);
    }
    let bytes = super::read_body(body)
        .await
        .map_err(IntoResponse::into_response)?;
    let ask: EnrolRequest = serde_json::from_slice(&bytes).map_err(|e| {
        refusal(
            StatusCode::BAD_REQUEST,
            "bad_request",
            &format!("the body is not an enrolment ({{token, public_key, client}}, JSON): {e}"),
        )
    })?;
    let unsupported =
        |why: String| refusal(StatusCode::BAD_REQUEST, code::PUBLIC_KEY_UNSUPPORTED, &why);
    let spki = base64::engine::general_purpose::STANDARD
        .decode(ask.public_key.trim())
        .map_err(|_| {
            unsupported(
                "public_key is not standard base64 (RFC 4648 §4, padded); send the DER \
                 SubjectPublicKeyInfo of an EC P-256 key"
                    .into(),
            )
        })?;
    super::tls::check_public_key(&spki).map_err(|e| unsupported(e.to_string()))?;
    let wall = Utc::now();
    let now = tokio::time::Instant::now();
    let failed = |what: &str| st.devices.failures.count(peer, what, now, wall);
    if !is_token(&ask.token) {
        failed("a token that is not one");
        return Err(unknown_token());
    }
    let Some(ca) = st.devices.trust.ca() else {
        return Err(refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            code::DEVICES_CA_UNAVAILABLE,
            &ca_unavailable(st),
        ));
    };
    let days = st.snapshot().settings.devices.client_cert_days;
    let key_hash = super::tls::key_hash(&spki);
    let from = peer.to_string();
    let client = ask.client.as_deref().map(client_text);
    let client = client.as_deref().filter(|c| !c.is_empty());
    let token_hash = tokens::hash(&ask.token);
    let enrolled = store::enrol_device(
        &st.db,
        &store::EnrolAsk {
            token_hash: &token_hash,
            key_hash: &key_hash,
            from: &from,
            client,
            now: wall,
        },
        |name| {
            super::tls::issue_device(&ca, &spki, name, days, wall).map(|c| store::Issued {
                der_b64: base64::engine::general_purpose::STANDARD.encode(&c.der),
                serial: c.serial,
                not_after: c.not_after,
                notice_after: c.notice_after,
            })
        },
    )
    .await
    .map_err(|e| {
        tracing::warn!("device enrolment from {peer}: the store failed: {e}");
        refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the enrolment could not be written; try again",
        )
    })?;
    let ca_b64 = base64::engine::general_purpose::STANDARD.encode(ca.certificate().as_ref());
    match enrolled {
        Enrolled::New {
            row,
            joined,
            serial,
        } => {
            // A key that never worked, replaced: it ends, and a stale
            // snapshot must not keep it (the row's own new key waits for its
            // reload, as a row that failed closed does).
            let ended = match &joined {
                store::Joined::Replaced { previous } => {
                    previous.starts_with(crate::config::DeviceLink::KEY_PREFIX)
                }
                store::Joined::Renewed | store::Joined::Waiting => false,
            };
            if let Err(e) = st.reload_snapshot().await {
                tracing::warn!(
                    "device '{}' enrolled, and the configuration could not be reloaded ({e}); \
                     it waits for the next reload",
                    super::short_name(&row.name)
                );
                if ended {
                    st.key_written(row.key_id, crate::state::KeyWritten::Pending);
                }
            }
            // Used: lmgw's window hands its link to no desktop client any
            // more, whose own key would only contest it (the token lives
            // on for this key's repeat, §3.3's note), until the device makes
            // a request with this certificate (D8's note of 2026-10-15).
            st.devices.minted.forget(row.key_id);
            st.devices.pending.answered(row.key_id, &serial);
            if ended {
                st.devices
                    .revocations
                    .revoke(row.key_id, super::RevokeReason::Rotated);
            }
            let code = lmgw_api_types::device_link::confirm_code(&sha256(&spki));
            let bare = super::short_name(&row.name);
            match joined {
                store::Joined::Renewed => tracing::info!(
                    "device '{bare}' renewed its certificate from {peer} (code {code}); its \
                     key and its confirmation stay"
                ),
                store::Joined::Waiting => tracing::info!(
                    "device '{bare}' enrolled a new key from {peer} with code {code}; it waits \
                     for the owner's confirmation, and the device's current key works until then"
                ),
                store::Joined::Replaced { .. } => tracing::info!(
                    "device '{bare}' enrolled from {peer} with code {code}; it waits for the \
                     owner's confirmation"
                ),
            }
            st.telemetry.keys(row.key_id, "enrolled");
            Ok(enrolment(row, ca_b64, &spki))
        }
        Enrolled::Repeat(row) => Ok(enrolment(row, ca_b64, &spki)),
        Enrolled::Unknown => {
            failed("an unknown token");
            Err(unknown_token())
        }
        Enrolled::Expired { at } => {
            failed("a lapsed token");
            Err(refusal(
                StatusCode::GONE,
                code::PAIRING_TOKEN_EXPIRED,
                &format!(
                    "this pairing token lapsed at {at} (a token lives devices.token_minutes); \
                     make a new one on lmgw's Devices card"
                ),
            ))
        }
        Enrolled::Ended { at } => {
            failed("an ended token");
            Err(refusal(
                StatusCode::GONE,
                code::PAIRING_TOKEN_EXPIRED,
                &format!(
                    "this pairing token was ended on lmgw's Devices card at {at}; make a new one \
                     there"
                ),
            ))
        }
        Enrolled::UsedElsewhere {
            key_id,
            name,
            newly,
        } => {
            failed("a token another key used");
            tracing::warn!(
                "the pairing token of device '{}' was used, then presented again from {peer} by \
                 another key: the device is marked contested",
                super::short_name(&name)
            );
            // The mark is the first presentation's; a later one changed
            // nothing to reload or to say.
            if newly {
                if let Err(e) = st.reload_snapshot().await {
                    tracing::warn!("a contested device was marked, and the reload failed ({e})");
                }
                st.telemetry.keys(key_id, "contested");
            }
            Err(refusal(
                StatusCode::GONE,
                code::PAIRING_TOKEN_USED,
                "this pairing token was used by another device: someone else used this code — \
                 delete the device on lmgw's Devices card if it was not yours",
            ))
        }
        Enrolled::Disabled { name } => Err(Refusal::device_disabled(&name).into_response()),
        Enrolled::KeyInUse { name } => Err(refusal(
            StatusCode::CONFLICT,
            code::PUBLIC_KEY_IN_USE,
            &format!(
                "device '{}' enrolled this key already; a device enrols a key of its own",
                super::short_name(&name)
            ),
        )),
        Enrolled::NotIssued(e) => match e {
            super::tls::IssueError::PublicKey(k) => Err(unsupported(k.to_string())),
            super::tls::IssueError::Sign(why) => {
                tracing::warn!("device enrolment from {peer}: {why}");
                Err(refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::DEVICES_CA_UNAVAILABLE,
                    &format!("the device CA could not sign the certificate: {why}"),
                ))
            }
        },
    }
}

/// The answer for `row`, with the CA certificate `ca_b64` and the code of
/// `spki`, the key that enrolled.
fn enrolment(row: EnrolledRow, ca_b64: String, spki: &[u8]) -> Enrolment {
    Enrolment {
        device: EnrolledDevice {
            id: row.key_id,
            name: super::short_name(&row.name).to_string(),
        },
        certificate: row.certificate_b64,
        ca_certificate: ca_b64,
        not_after: row.not_after,
        notice_after: row.notice_after,
        urls: row.urls,
        confirm_code: lmgw_api_types::device_link::confirm_code(&sha256(spki)),
        confirmed: row.confirmed,
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// The enrolment's `client` as the card shows it: without control, format
/// (direction overrides and isolates, zero-width characters) and line
/// separator characters, which would make the card's line or a log line
/// read as something else; trimmed. Free text, never trusted for anything.
fn client_text(client: &str) -> String {
    client
        .chars()
        .filter(|c| !super::is_invisible(*c))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether `t` is a pairing token by its shape; any other string is no
/// token lmgw minted.
fn is_token(t: &str) -> bool {
    t.strip_prefix(TOKEN_PREFIX).is_some_and(|hex| {
        hex.len() == TOKEN_HEX_DIGITS
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn unknown_token() -> Response {
    refusal(
        StatusCode::UNAUTHORIZED,
        code::PAIRING_TOKEN_UNKNOWN,
        "lmgw knows no such pairing token (never made, or its device was deleted); make a new \
         link on lmgw's Devices card",
    )
}

/// Why the CA cannot sign, as the status says it.
fn ca_unavailable(st: &SharedState) -> String {
    let ca = st.devices.trust.status(&st.data_dir).ca;
    match ca.error {
        Some(e) => format!("the device CA cannot be used: {}", e.message),
        None => "the device CA is not loaded; the device listener opens it when it starts".into(),
    }
}

fn refusal(status: StatusCode, code: &'static str, message: &str) -> Response {
    Refusal {
        status,
        code,
        message: message.to_string(),
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's line and a log line show the client as the device said
    /// it, less what would make them read as something else (review,
    /// 2026-10-15): controls, direction overrides and isolates, zero-width
    /// characters, line separators.
    #[test]
    fn the_client_text_keeps_what_reads_as_itself() {
        assert_eq!(
            client_text("  Kai mobile 0.1.0 (Android 16) "),
            "Kai mobile 0.1.0 (Android 16)"
        );
        assert_eq!(
            client_text("Kai\u{202E}elibom\u{202C} 1\n2\u{200B}\u{2066}x\u{2069}"),
            "Kaielibom 12x"
        );
        assert_eq!(client_text("\u{2028}\u{feff}\t"), "");
        assert_eq!(client_text("Kai für Ärzte 1.0"), "Kai für Ärzte 1.0");
    }
}
