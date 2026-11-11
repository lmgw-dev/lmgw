//! `POST /api/session { login, password }` (headless server design §5.2):
//! in order, the same-origin rule (the caller's), the transport rule (D26),
//! the throttle (§5.4), a verification permit (§5.10), argon2 verification
//! (D12, D13), then a session, `204`, `last_login_at` and
//! `failed_logins = 0` written.
//!
//! Wrong, unknown, disabled, without a password or not a sign-in name at
//! all: one answer, `401 login_invalid`, after one verification each and
//! one lookup each (D13), and the throttle counts every presented name
//! alike, existing or not, by its hash. The failure's bookkeeping runs after
//! the answer.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use super::{client_of, cookie_named, with_cookie, with_session, ConnInfo, SessionStart};
use crate::accounts::ops::{require_secure, Transport};
use crate::accounts::sessions::Proof;
use crate::accounts::{password, throttle};
use crate::net::Scheme;
use crate::principal::{known_cookie, known_cookie_name, Refusal};
use crate::state::SharedState;

/// `401 login_invalid` for a password sign-in (§5.2).
fn invalid() -> Refusal {
    Refusal {
        status: StatusCode::UNAUTHORIZED,
        code: "login_invalid",
        message: "the sign-in name or password is wrong".into(),
    }
}

fn with_retry_after(refusal: Refusal, secs: u64) -> Response {
    let mut resp = refusal.into_response();
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

pub(super) async fn sign_in(
    state: &SharedState,
    scheme: Scheme,
    conn: &ConnInfo,
    headers: &HeaderMap,
    carried: Option<i64>,
    login: &str,
    password: &str,
) -> Response {
    let snap = state.snapshot();
    if !snap.accounts_on {
        return crate::accounts::accounts_off().into_response();
    }
    let direct_loopback = conn.as_ref().is_some_and(|c| c.0 .0.is_direct_loopback());
    if let Err(r) = require_secure(Transport {
        scheme,
        direct_loopback,
    }) {
        return r.into_response();
    }
    // A name that is not one (empty, too long, outside the alphabet) is
    // looked up as nothing and verified like any other (D13); the throttle
    // keys on the name as presented, folded and hashed, either way.
    let folded = crate::accounts::normalize_login(login).ok();
    let addresses = conn.as_ref().map_or_else(
        || vec!["unknown".to_string()],
        |c| throttle::address_keys(c.0 .0.peer.ip()),
    );
    let known = match (
        known_cookie(headers, scheme),
        folded.as_deref().and_then(|l| snap.account_by_login(l)),
    ) {
        (Some(token), Some(a)) => {
            let hash = crate::config::hash_api_key(token);
            let theirs = crate::store::known_browser(&state.db, &hash)
                .await
                .ok()
                .flatten()
                == Some(a.id);
            theirs.then_some(hash)
        }
        _ => None,
    };
    let attempt = throttle::Attempt::sign_in(login, addresses, known.as_deref());
    let rt = &state.accounts;
    let reservation = match rt.throttle.reserve_noting(&attempt) {
        Ok(r) => r,
        Err((wait, held, first)) => {
            // One entry when a hold starts, not one per `429` (D91).
            if first {
                let target = sign_in_target(&snap, folded.as_deref());
                let actor = crate::audit::anonymous(attempt_address(conn));
                crate::audit::record_detached(state, actor.entry("sign-in throttled", target, 429));
            }
            return with_retry_after(
                throttle::throttled(wait, held),
                throttle::retry_after_secs(wait),
            );
        }
    };
    let permit = if known.is_some() {
        rt.verifier.try_permit_known()
    } else {
        rt.verifier.try_permit()
    };
    let Some(permit) = permit else {
        rt.throttle.undo(reservation);
        return with_retry_after(password::busy(rt.verifier.count()), 1);
    };
    // One query for every name, existing or not: an enabled account with a
    // password, or nothing to verify against but the dummy (D13).
    let stored = match crate::store::sign_in_hash(&state.db, folded.as_deref().unwrap_or("")).await
    {
        Ok(found) => found,
        Err(e) => {
            rt.throttle.undo(reservation);
            return Refusal::from(e.to_string()).into_response();
        }
    };
    let (verified, took) = rt
        .verifier
        .verify(
            permit,
            password.to_string(),
            stored.as_ref().map(|(_, h)| h.clone()),
        )
        .await;
    tracing::debug!("a password verification took {} ms", took.as_millis());
    let (password::Verified::Good(rehash), Some((account_id, verified_hash))) = (verified, stored)
    else {
        // The answer first: the counter is written off its path, and no
        // snapshot is reloaded for it (the card reads it from the store),
        // so an existing name's failure takes what an unknown one's does.
        let failed = folded
            .as_deref()
            .and_then(|l| snap.account_by_login(l))
            .map(|a| a.id);
        if let Some(id) = failed {
            let db = state.db.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::store::record_login_failed(&db, id).await {
                    tracing::debug!("counting a failed sign-in of account {id} failed: {e}");
                }
            });
        }
        crate::audit::record_detached(
            state,
            crate::audit::anonymous(attempt_address(conn)).entry(
                "sign-in failed",
                sign_in_target(&snap, folded.as_deref()),
                401,
            ),
        );
        tracing::debug!(
            "a password sign-in failed (name {}…, {} characters) from {}",
            &attempt.name_key()[..12],
            login.chars().count(),
            attempt_address(conn),
        );
        return invalid().into_response();
    };
    rt.throttle.succeeded(reservation);
    if let Err(e) =
        crate::store::record_login_ok(&state.db, account_id, &verified_hash, rehash.as_deref())
            .await
    {
        tracing::warn!("recording the sign-in of account {account_id} failed: {e}");
    }
    let client = client_of(conn, headers);
    tracing::info!(
        "accounts: '{}' signed in with a password from {}",
        snap.account(account_id)
            .map_or_else(|| format!("#{account_id}"), |a| a.login.clone()),
        client.0
    );
    let resp = known_browser(
        state,
        StatusCode::NO_CONTENT.into_response(),
        account_id,
        scheme,
    )
    .await;
    // What the session's insert checks again: the hash as it stands after
    // this sign-in's own rehash.
    let now_held = rehash.unwrap_or(verified_hash);
    let start = SessionStart {
        account: account_id,
        proof: Proof::Password(&now_held),
        carried,
        with: "password",
    };
    with_session(state, resp, start, scheme, client).await
}

/// A password sign-in's audit target: the account whose name was
/// presented, when one has it (`audit::sign_in_target`).
fn sign_in_target(
    snap: &crate::config::Snapshot,
    folded: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let account = folded.and_then(|l| snap.account_by_login(l)).map(|a| a.id);
    crate::audit::sign_in_target(snap, account, "password")
}

/// The client address a failed sign-in's debug line names.
fn attempt_address(conn: &ConnInfo) -> String {
    conn.as_ref()
        .map_or_else(|| "unknown".into(), |c| c.0 .0.peer.ip().to_string())
}

/// Mark this browser as one of `account`'s (§5.4): a token in the
/// known-browser cookie and its hash in `known_browsers`, for
/// `accounts.session_days`.
pub(crate) async fn known_browser(
    state: &SharedState,
    resp: Response,
    account: i64,
    scheme: Scheme,
) -> Response {
    let days = state.snapshot().settings.accounts.session_days;
    let (token, hash) = crate::accounts::mint_secret();
    let expires = chrono::Utc::now() + chrono::Duration::days(i64::from(days));
    let at = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Err(e) = crate::store::put_known_browser(&state.db, &hash, account, &at).await {
        tracing::warn!("remembering a known browser failed: {e}");
        return resp;
    }
    with_cookie(
        resp,
        cookie_named(
            known_cookie_name(scheme),
            &token,
            u64::from(days) * 86_400,
            scheme,
        ),
    )
}
