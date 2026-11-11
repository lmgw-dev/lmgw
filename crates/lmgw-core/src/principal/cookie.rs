//! The session cookie's names and their reading (principals §3.3, headless
//! server design §4.3, D43): `lmgw_session` on the plain listener,
//! `__Host-lmgw_session` on `https`, and each scheme reads its own.

use axum::http::HeaderMap;

use super::{matching_row, Principal, Refusal};
use crate::config::{ApiKeyKind, Snapshot};

/// The cookie an owner's browser carries on the plain listener (§3.3).
/// Host-only, `HttpOnly`, `SameSite=Strict`; accepted for `owner` rows and
/// nothing else.
pub const SESSION_COOKIE: &str = "lmgw_session";

/// The same cookie on an `https` request (headless server design §4.3):
/// `Secure`, and the `__Host-` prefix, which a browser refuses to accept
/// with a `Domain` attribute or without `Secure` and `Path=/`, so a page on
/// an agent origin cannot overwrite it from a shared parent domain, and a
/// page over plain `http` cannot set it at all.
pub const SESSION_COOKIE_HTTPS: &str = "__Host-lmgw_session";

/// The session cookie's name on a request of `scheme`.
pub fn session_cookie_name(scheme: crate::net::Scheme) -> &'static str {
    match scheme {
        crate::net::Scheme::Http => SESSION_COOKIE,
        crate::net::Scheme::Https => SESSION_COOKIE_HTTPS,
    }
}

/// The session cookie's value out of the `Cookie` header of a request of
/// `scheme`: `lmgw_session` on `http`, `__Host-lmgw_session` on `https`
/// (headless server design §4.3). The other name is not read: a plain
/// `lmgw_session` is sent to every port of the host whatever its scheme,
/// so on `https` only the cookie a plain page cannot set counts.
///
/// Parsed by hand rather than with a cookie crate: the header is a
/// `;`-separated list of `name=value` pairs, the value lmgw sets is 76 hex-ish
/// ASCII characters with nothing to quote or percent-encode, and reading one
/// name out of that list is the entire requirement.
pub fn session_cookie(headers: &HeaderMap, scheme: crate::net::Scheme) -> Option<&str> {
    cookie_named(headers, session_cookie_name(scheme))
}

/// Either session cookie's value: what the device listener refuses as a
/// credential that does not belong on it.
pub fn any_session_cookie(headers: &HeaderMap) -> Option<&str> {
    cookie_named(headers, SESSION_COOKIE).or_else(|| cookie_named(headers, SESSION_COOKIE_HTTPS))
}

/// The known-browser cookie (headless server design §5.4) on the plain
/// listener: set at a good password sign-in, so the throttle can tell an
/// account's own browsers from a spray.
pub const KNOWN_COOKIE: &str = "lmgw_known";

/// The same on `https`, under the `__Host-` prefix as the session's.
pub const KNOWN_COOKIE_HTTPS: &str = "__Host-lmgw_known";

/// The known-browser cookie's name on a request of `scheme`.
pub fn known_cookie_name(scheme: crate::net::Scheme) -> &'static str {
    match scheme {
        crate::net::Scheme::Http => KNOWN_COOKIE,
        crate::net::Scheme::Https => KNOWN_COOKIE_HTTPS,
    }
}

/// The known-browser cookie's value, as each scheme reads its own.
pub fn known_cookie(headers: &HeaderMap, scheme: crate::net::Scheme) -> Option<&str> {
    cookie_named(headers, known_cookie_name(scheme))
}

/// The session cookie, which is a credential **only in the owner position**
/// (principals §3.3).
///
/// With accounts on (headless server design §5.2) it carries a session
/// ([`crate::accounts::sessions::SESSION_PREFIX`]): resolved through the
/// session desk to the principal of its account's door key, with the
/// account and the session set; an unknown or expired session is
/// `Anonymous`, and so is a cookie that carries an owner key, which
/// accounts on no longer honour (a browser signed in before the desktop's
/// switch went on signs in again, §5.11).
///
/// With accounts off it is today's: an owner key, honoured for owner rows
/// only (a client or agent key sitting in it is not an error and not an
/// authentication, it is simply not a credential there), and on a desktop
/// whose accounts are off an owner row of another account than account 1
/// is paused (§5.11).
pub(super) fn from_cookie(snap: &Snapshot, cookie: &str) -> Result<Principal, Refusal> {
    if snap.accounts_on {
        return from_session(snap, cookie);
    }
    let Some(key) = matching_row(snap, cookie) else {
        tracing::debug!("a presented session cookie matched no key row");
        return Ok(Principal::Anonymous);
    };
    if key.kind != ApiKeyKind::Owner {
        tracing::debug!(
            "the session cookie carried a '{}' key; only owner keys authenticate a cookie",
            key.kind.as_str()
        );
        return Ok(Principal::Anonymous);
    }
    if !key.enabled {
        return Err(Refusal::owner_key_disabled(&key.name));
    }
    if let Some(block) = snap.owner_row_blocked(key.account_id) {
        return Err(block.refusal());
    }
    Ok(Principal::from_key(key))
}

fn from_session(snap: &Snapshot, cookie: &str) -> Result<Principal, Refusal> {
    let Some(found) = snap.sessions.lookup(cookie) else {
        tracing::debug!(
            "the session cookie names no live session (accounts are on: an owner key in it is \
             not honoured)"
        );
        return Ok(Principal::Anonymous);
    };
    let Some(account) = snap.account(found.account_id) else {
        return Ok(Principal::Anonymous);
    };
    if !account.enabled {
        return Err(crate::accounts::OwnerBlock::Disabled(account.login.clone()).refusal());
    }
    let Some(door) = snap.api_keys.iter().find(|k| k.id == account.door_key_id) else {
        tracing::warn!(
            "account '{}' has no door key row (id {}); its sessions authenticate nothing",
            account.login,
            account.door_key_id
        );
        return Ok(Principal::Anonymous);
    };
    if !door.enabled {
        return Err(Refusal::owner_key_disabled(&door.name));
    }
    let mut who = Principal::from_key(door);
    if let Principal::Key {
        account: a,
        session,
        ..
    } = &mut who
    {
        *a = Some(account.id);
        *session = Some(found.id);
    }
    Ok(who)
}

fn cookie_named<'h>(headers: &'h HeaderMap, wanted: &str) -> Option<&'h str> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == wanted)
        .map(|(_, value)| value.trim())
}
