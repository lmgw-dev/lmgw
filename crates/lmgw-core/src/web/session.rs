//! The login (principals design §3.4): the four routes that turn a key into
//! the session cookie, and the one-shot nonce the shell opens its window with.
//!
//! All four are [`Cap::Public`] and all four are registered **ahead of the SPA
//! catch-all**, because `/{*path}` takes every path no other plane claimed.
//! Public is not an oversight: a browser that has no session is exactly the
//! caller these routes exist for, and each of them verifies its own credential
//! — the key in the query, the key in the body, or the cookie the request
//! already carries.
//!
//! The cookie is the only thing written here, and it is written in one shape
//! per scheme ([`set_cookie`], §3.3). `HttpOnly`, so the dashboard's own
//! script cannot read it back; host-only, so `127.0.0.1` and a LAN address
//! never share it; `SameSite=Strict`, which is the CSRF defence and the
//! reason the §3.6 rule only has the same-site-other-port gap left to close.
//! On an `https` request (the TLS listener, headless server design §4.3) it
//! is also `Secure` and named `__Host-lmgw_session`, which a browser accepts
//! only from a secure origin, without `Domain` and on `Path=/`.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use lmgw_api_types::SessionView;
use serde::Deserialize;

use crate::agents::token::OWNER_DASHBOARD;
use crate::config::{ApiKey, ApiKeyKind, Snapshot};
use crate::net::Scheme;
use crate::principal::{same_origin, session_cookie_name, Cap, Principal, Refusal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// `Max-Age` on the session cookie: one year, in seconds (§3.3).
///
/// Named because the Keys page prints it — "a browser stays logged in for a
/// year, or until Rotate" — and a number the owner is shown must be the number
/// the server sends. The key itself is persisted, so a browser session should
/// be too; rotation, not expiry, is how a session ends.
pub const SESSION_MAX_AGE_SECS: u64 = 31_536_000;

/// The nonce desk (§3.4).
mod nonces;
pub use nonces::*;

// ---------------------------------------------------------------------------
// The cookie
// ---------------------------------------------------------------------------

/// The `Set-Cookie` value for a request of `scheme`, in the shape §3.3
/// gives it.
///
/// No `Domain` (host-only), by omission and deliberate — see the module
/// docs. `Secure` and the `__Host-` name on `https` only: a plain listener's
/// browser would drop a `Secure` cookie it was handed over `http`.
/// Today's cookie, the owner key itself (accounts off): what the desktop's
/// switch hands account 1's browser when it turns accounts off (§5.11).
pub(crate) fn door_key_cookie(key: &str, scheme: Scheme) -> String {
    set_cookie(key, scheme)
}

fn set_cookie(key: &str, scheme: Scheme) -> String {
    cookie_named(
        session_cookie_name(scheme),
        key,
        SESSION_MAX_AGE_SECS,
        scheme,
    )
}

/// A `Set-Cookie` value of `name` in §3.3's shape, lasting `max_age`
/// seconds: the session cookie with accounts on (`accounts.session_days`,
/// headless server design §5.2) and the known-browser cookie (§5.4).
pub(crate) fn cookie_named(name: &str, value: &str, max_age: u64, scheme: Scheme) -> String {
    format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        secure(scheme)
    )
}

/// The session cookie of a session (headless server design §5.2): `Max-Age`
/// from `accounts.session_days`.
pub(crate) fn session_set_cookie(state: &SharedState, value: &str, scheme: Scheme) -> String {
    let days = u64::from(state.snapshot().settings.accounts.session_days);
    cookie_named(session_cookie_name(scheme), value, days * 86_400, scheme)
}

/// The same cookie with `Max-Age=0`, which is how a browser is told to drop
/// it. The attributes have to match the ones it was set with, or the jar keeps
/// the original entry beside the expired one.
pub(crate) fn clear_cookie(scheme: Scheme) -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        session_cookie_name(scheme),
        secure(scheme)
    )
}

fn secure(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Https => "; Secure",
        Scheme::Http => "",
    }
}

/// Attach a `Set-Cookie` to a response.
pub(crate) fn with_cookie(mut resp: Response, cookie: String) -> Response {
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            resp.headers_mut().append(header::SET_COOKIE, value);
        }
        // An owner key is `lmgw-owner-<64 hex>` and the attributes are
        // literals, so this cannot happen — and if a future key format ever
        // made it happen, a login that silently half-worked would be worse
        // than one that says why it did not.
        Err(e) => tracing::error!("the session cookie is not a legal header value: {e}"),
    }
    resp
}

/// `302` to `where_to`. Not axum's [`Redirect::to`], which is a `303`: §3.4
/// says `302`, and the browser lands on the SPA either way.
///
/// [`Redirect::to`]: axum::response::Redirect::to
fn found(where_to: &str) -> Response {
    (
        StatusCode::FOUND,
        [(header::LOCATION, where_to.to_string())],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------

/// The largest body `/api/session` and `/api/account/password` take
/// (headless server design §5.2): axum's default written down, so the only
/// ceiling on a password's length is a named one, which a `413` and the
/// form both state.
pub const SESSION_BODY_LIMIT: usize = 2 * 1024 * 1024;

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/api/session/login", get(login))
        .route("/api/session", get(current).post(sign_in).delete(sign_out))
        .layer(axum::extract::DefaultBodyLimit::max(SESSION_BODY_LIMIT))
        // `Public`: this is the plane a browser with no session talks to, and
        // each handler verifies the credential it was given itself.
        .route_layer(crate::server::require(state, Cap::Public))
}

/// The connection a request came in on, when the server recorded one (a
/// suite's own `axum::serve` without connect info records none).
pub(crate) type ConnInfo = Option<Extension<axum::extract::ConnectInfo<crate::server::Conn>>>;

/// Where a sign-in came from, for its session row (§5.2): the client's
/// address and its user agent.
pub(crate) fn client_of(conn: &ConnInfo, headers: &HeaderMap) -> (String, String) {
    let peer = conn
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |c| c.0 .0.peer.ip().to_string());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (peer, ua)
}

/// What a sign-in starts a session for ([`with_session`]).
pub(crate) struct SessionStart<'a> {
    /// The account.
    pub account: i64,
    /// What the session's insert checks again (`sessions::Proof`).
    pub proof: crate::accounts::sessions::Proof<'a>,
    /// The session this browser already carried, if any: it ends once the
    /// new one has started, so a browser holds one session at a time.
    pub carried: Option<i64>,
    /// What it signed in with, as the audit log names it (§5.8):
    /// `password`, `key`, `login link`, `set-password link`.
    pub with: &'static str,
}

/// A response that starts a session and carries its cookie (§5.2): every
/// sign-in path ends here while accounts are on.
pub(crate) async fn with_session(
    state: &SharedState,
    resp: Response,
    start: SessionStart<'_>,
    scheme: Scheme,
    (peer, ua): (String, String),
) -> Response {
    let started =
        crate::accounts::sessions::start(state, start.account, &peer, &ua, start.proof).await;
    match started {
        Ok(started) => {
            // After the answer, as every sign-in route's entry (D83).
            let snap = state.snapshot();
            let actor = crate::audit::signed_in(&snap, start.account, started.id, peer.clone());
            let target = crate::audit::sign_in_target(&snap, Some(start.account), start.with);
            let entry = actor.entry("sign-in", target, resp.status().as_u16());
            crate::audit::record_detached(state, entry);
            if let Some(old) = start.carried {
                let which = crate::accounts::sessions::Which::One(old);
                if let Err(e) = crate::accounts::sessions::end(state, which).await {
                    tracing::warn!("ending the session this browser carried before: {e}");
                }
            }
            with_cookie(resp, session_set_cookie(state, &started.cookie, scheme))
        }
        Err(refusal) => {
            tracing::warn!("a sign-in started no session: {}", refusal.message);
            refusal.into_response()
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct LoginQuery {
    nonce: Option<String>,
    token: Option<String>,
}

/// What a login link resolved to.
enum Login {
    /// The cookie carries this owner key (accounts off).
    Key(String),
    /// A session for this account (accounts on).
    Account(i64),
}

/// `GET /api/session/login?nonce=<n>` or `?token=<key>` (§3.4).
///
/// The shell's window opens here with a nonce; the owner's browser opens here
/// with the token from the line `server::run` printed, or a nonce from
/// `lmgw-server login-link`. Either match sets the cookie and lands on `/`;
/// anything else lands on `/?login=invalid` with no cookie, which is the
/// card the SPA shows for a link that did not work. With accounts on
/// (headless server design §5.2) the cookie is a session of the account the
/// nonce or the key belongs to, so a key never travels to the browser.
///
/// The §3.6 rule applies **unconditionally** — there is no cookie yet, so
/// there is nothing for `principal_mw` to have applied it to. `Sec-Fetch-Site:
/// none` is a typed URL, a bookmark or the shell and is the normal case here;
/// `cross-site` is a foreign page navigating this browser to a key of its
/// choosing, which is the one thing this route must not do.
async fn login(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    conn: ConnInfo,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    let scheme = ctx.scheme;
    if let Err(refusal) = same_origin(&headers, scheme) {
        return refusal.into_response();
    }
    let snap = state.snapshot();
    let on = snap.accounts_on;
    let login = match (&q.nonce, &q.token) {
        (Some(nonce), _) => match exchange_nonce(&state, nonce) {
            Some(account) if on => enabled_account(&snap, account).map(Login::Account),
            // Accounts off: only account 1's nonce, the shell's, signs in,
            // and the shell's session is the door key. Another account's
            // (a `login-link` minted before the switch went off) is spent.
            Some(account) if account != crate::accounts::ACCOUNT_ONE => {
                tracing::debug!(
                    "a login nonce of account {account} was presented while accounts are off"
                );
                None
            }
            Some(_) => {
                let key = snap.owner_key(OWNER_DASHBOARD).map(str::to_string);
                if key.is_none() {
                    // Unreachable: the row is seeded at init and cannot be
                    // disabled or deleted (§3.1). Said out loud anyway,
                    // because the owner would otherwise see a window that
                    // landed on the login card with nothing in the log to
                    // explain it.
                    tracing::warn!(
                        "a login nonce was exchanged, but '{OWNER_DASHBOARD}' is missing or \
                         disabled — there is no session to hand back"
                    );
                }
                key.map(Login::Key)
            }
            None => None,
        },
        // A token is whichever owner row it matches — a second browser or a
        // LAN device logs in as its own row, not as the dashboard's.
        (_, Some(token)) => usable_owner(&snap, token).map(|k| match k.account_id {
            Some(account) if on => Login::Account(account),
            _ => Login::Key(token.clone()),
        }),
        _ => None,
    };
    match login {
        Some(Login::Key(key)) => with_cookie(found("/"), set_cookie(&key, scheme)),
        Some(Login::Account(account)) => {
            let client = client_of(&conn, &headers);
            let start = SessionStart {
                account,
                proof: crate::accounts::sessions::Proof::Credential,
                carried: ctx.principal.session(),
                with: if q.nonce.is_some() {
                    "login link"
                } else {
                    "key"
                },
            };
            with_session(&state, found("/"), start, scheme, client).await
        }
        None => {
            if on {
                let with = if q.nonce.is_some() {
                    "login link"
                } else {
                    "key"
                };
                let target = crate::audit::sign_in_target(&snap, None, with);
                let actor = crate::audit::anonymous(client_of(&conn, &headers).0);
                crate::audit::record_detached(&state, actor.entry("sign-in failed", target, 401));
            }
            found("/?login=invalid")
        }
    }
}

/// `account` when it exists and is enabled.
fn enabled_account(snap: &Snapshot, account: i64) -> Option<i64> {
    snap.account(account).filter(|a| a.enabled).map(|a| a.id)
}

/// `POST /api/session` — the paste fallback `{ token }` (§3.4, and the way
/// the SPA re-logs itself after rotating `owner:dashboard`, §3.12), or with
/// accounts on a sign-in name and a password `{ login, password }`
/// (headless server design §5.2, [`password`]).
///
/// A token that matches no usable owner row is `401 login_invalid`: the
/// credential the caller sent is the thing that failed, and it is not
/// `session_required` — that one means "you sent nothing", and the card
/// would print the wrong sentence. With accounts on a good token signs this
/// browser into its account's session, and the key stays out of the
/// browser.
async fn sign_in(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    conn: ConnInfo,
    headers: HeaderMap,
    Json(body): Json<SignIn>,
) -> Response {
    let scheme = ctx.scheme;
    if let Err(refusal) = same_origin(&headers, scheme) {
        return refusal.into_response();
    }
    let token = match body {
        SignIn::Password { login, password } => {
            let carried = ctx.principal.session();
            return password::sign_in(&state, scheme, &conn, &headers, carried, &login, &password)
                .await;
        }
        SignIn::Token { token } => token,
    };
    let snap = state.snapshot();
    let Some(key) = usable_owner(&snap, &token) else {
        tracing::debug!("a sign-in presented a token that matches no enabled owner row");
        if snap.accounts_on {
            let target = crate::audit::sign_in_target(&snap, None, "key");
            let actor = crate::audit::anonymous(client_of(&conn, &headers).0);
            crate::audit::record_detached(&state, actor.entry("sign-in failed", target, 401));
        }
        return Refusal {
            status: StatusCode::UNAUTHORIZED,
            code: "login_invalid",
            message: "that is not an enabled owner key — copy it again from Usage → Keys, or \
                      from the login link in the process log"
                .into(),
        }
        .into_response();
    };
    match key.account_id.filter(|_| snap.accounts_on) {
        Some(account) => {
            let client = client_of(&conn, &headers);
            let start = SessionStart {
                account,
                proof: crate::accounts::sessions::Proof::Credential,
                carried: ctx.principal.session(),
                with: "key",
            };
            with_session(
                &state,
                StatusCode::NO_CONTENT.into_response(),
                start,
                scheme,
                client,
            )
            .await
        }
        None => with_cookie(
            StatusCode::NO_CONTENT.into_response(),
            set_cookie(&token, scheme),
        ),
    }
}

/// `POST /api/session`'s body: a key, or a sign-in name and a password.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum SignIn {
    Token {
        /// An owner key (the paste fallback).
        #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
        token: String,
    },
    Password {
        /// The sign-in name (letters a–z, digits, `.`, `_`, `-`; any case).
        login: String,
        /// The password (at least 12 characters, up to 2 MiB).
        #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
        password: String,
    },
}

/// The password sign-in (headless server design §5.2, §5.4).
mod password;

/// `GET /api/session` — never a `401`, whatever the request carried (§3.4).
///
/// It is the question "am I logged in", and answering it with a refusal would
/// make the SPA's load check indistinguishable from a gateway that is down.
async fn current(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Json<SessionView> {
    let snap = state.snapshot();
    Json(match &ctx.principal {
        Principal::Anonymous => SessionView {
            accounts_on: snap.accounts_on,
            container: state.deployment().is_container(),
            ..SessionView::default()
        },
        Principal::Key { kind, name, .. } => {
            let account = ctx
                .principal
                .account()
                .filter(|_| snap.accounts_on)
                .and_then(|id| snap.account(id))
                .map(|a| lmgw_api_types::SessionAccount {
                    id: a.id,
                    login: a.login.clone(),
                    display_name: a.display_name.clone(),
                    has_password: a.has_password,
                });
            SessionView {
                authenticated: true,
                kind: kind.as_str().to_string(),
                name: name.clone(),
                accounts_on: snap.accounts_on,
                container: state.deployment().is_container(),
                account,
                expires_at: ctx
                    .principal
                    .session()
                    .and_then(|id| snap.sessions.expires(id))
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            }
        }
    })
}

/// `DELETE /api/session` — sign out: this browser's session ends (headless
/// server design §5.2) and the cookie is cleared, `204`.
///
/// Same-origin only, like the `GET` and the `POST` beside it (§3.6). Dropping
/// a session is a write to the browser's jar, and a foreign page that can make
/// this call logs the owner out of their own dashboard — small, but it is a
/// cross-site request forgery and the rule that answers it is already written.
///
/// Nothing else is asked: a caller holding a session it wants to stop holding
/// is never refused, whatever that session resolves to, and a key itself is
/// untouched (that is Rotate, §3.12). Ending the session closes what it
/// opened (its event streams and sockets).
async fn sign_out(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
) -> Response {
    let scheme = ctx.scheme;
    if let Err(refusal) = same_origin(&headers, scheme) {
        return refusal.into_response();
    }
    if let Some(id) = ctx.principal.session() {
        let snap = state.snapshot();
        if snap.accounts_on {
            let actor = crate::audit::Actor::with_via(&snap, &ctx, crate::audit::Via::Dashboard);
            let target = crate::audit::target("session", id);
            crate::audit::record_detached(&state, actor.entry("sign-out", target, 204));
        }
        if let Err(e) =
            crate::accounts::sessions::end(&state, crate::accounts::sessions::Which::One(id)).await
        {
            tracing::warn!("signing out: {e}");
        }
    }
    with_cookie(StatusCode::NO_CONTENT.into_response(), clear_cookie(scheme))
}

/// The owner row a presented key matches and that may sign in: enabled,
/// and not held by a disabled or paused account (headless server design
/// §5.1, §5.11).
///
/// `verify_api_key` already skips disabled rows; the kind filter is what makes
/// this a *login*: a client or agent key is a perfectly good credential in the
/// bearer position and is not one here (§3.3).
fn usable_owner<'a>(snap: &'a Snapshot, presented: &str) -> Option<&'a ApiKey> {
    snap.verify_api_key(presented)
        .filter(|k| k.kind == ApiKeyKind::Owner)
        .filter(|k| snap.owner_row_blocked(k.account_id).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one shape §3.3 gives the cookie, asserted as the literal string the
    /// spec writes — the attributes are the security properties, so a silent
    /// edit to any of them is a silent change to the threat model.
    #[test]
    fn the_cookie_is_exactly_what_the_spec_writes() {
        assert_eq!(
            set_cookie("lmgw-owner-abc", Scheme::Http),
            "lmgw_session=lmgw-owner-abc; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"
        );
        assert_eq!(SESSION_MAX_AGE_SECS, 365 * 24 * 60 * 60);
        // The clearing cookie differs in `Max-Age` and in nothing else: a
        // browser matches the entry to drop by name, path and domain.
        assert_eq!(
            clear_cookie(Scheme::Http),
            "lmgw_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
        );
        // On `https` (headless server design §4.3): `Secure`, and the
        // `__Host-` name a browser takes only from a secure origin, without
        // `Domain`, on `Path=/`.
        assert_eq!(
            set_cookie("lmgw-owner-abc", Scheme::Https),
            "__Host-lmgw_session=lmgw-owner-abc; Path=/; HttpOnly; SameSite=Strict; \
             Max-Age=31536000; Secure"
        );
        assert_eq!(
            clear_cookie(Scheme::Https),
            "__Host-lmgw_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0; Secure"
        );
    }
}
