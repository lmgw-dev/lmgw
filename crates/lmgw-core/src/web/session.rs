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
//! The cookie is the only thing written here, and it is written in exactly one
//! shape ([`set_cookie`], §3.3). `HttpOnly`, so the dashboard's own script
//! cannot read it back; host-only, so `127.0.0.1` and a LAN address never
//! share it; `SameSite=Strict`, which is the CSRF defence and the reason the
//! §3.6 rule only has the same-site-other-port gap left to close.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::agents::token::OWNER_DASHBOARD;
use crate::config::{ApiKey, ApiKeyKind, Snapshot};
use crate::principal::{same_origin, Cap, Principal, Refusal, SESSION_COOKIE};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// How long a login nonce stays exchangeable (§3.4).
///
/// It is the width of the window between the shell minting the nonce and the
/// webview it just created asking for the page — a second, normally. Sixty is
/// what survives a cold start on a loaded machine; it is not a session length,
/// and the cookie the exchange hands back is what lasts.
pub const LOGIN_NONCE_TTL: Duration = Duration::from_secs(60);

/// `Max-Age` on the session cookie: one year, in seconds (§3.3).
///
/// Named because the Keys page prints it — "a browser stays logged in for a
/// year, or until Rotate" — and a number the owner is shown must be the number
/// the server sends. The key itself is persisted, so a browser session should
/// be too; rotation, not expiry, is how a session ends.
pub const SESSION_MAX_AGE_SECS: u64 = 31_536_000;

// ---------------------------------------------------------------------------
// The nonce desk
// ---------------------------------------------------------------------------

/// The login nonces this gateway has minted and not yet seen used.
///
/// In-process and per gateway — a field of `AppState` rather than a `static`,
/// for the same reason the ledger's `Desk` is one: two gateways in one test
/// process must not exchange each other's nonces, and a nonce is a credential
/// for whichever gateway answers it.
///
/// Single-use is [`take`](Self::take) removing the entry, and expiry is
/// checked *after* the removal: a nonce presented late is spent, not left
/// lying around for a second try.
pub struct LoginNonces {
    live: Mutex<HashMap<String, Instant>>,
    /// [`LOGIN_NONCE_TTL`], unless a test shortened it — see [`Self::set_ttl`].
    ttl_ms: AtomicU64,
}

impl Default for LoginNonces {
    fn default() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            ttl_ms: AtomicU64::new(LOGIN_NONCE_TTL.as_millis() as u64),
        }
    }
}

impl LoginNonces {
    /// How long a nonce minted now stays exchangeable.
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed))
    }

    /// Shorten (or lengthen) the window — **the seam the expiry test uses**.
    ///
    /// A test that wanted to watch a nonce expire on the real constant would
    /// sleep a minute, so the constant is made injectable instead of the test
    /// being written against a fake clock that the production path never runs.
    /// Nothing in lmgw calls this; it is here so that what the test exercises
    /// is the real `take`, on the real map, with only the width changed.
    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl_ms.store(ttl.as_millis() as u64, Ordering::Relaxed);
    }

    /// A fresh nonce, remembered until it is used or expires.
    fn mint(&self) -> String {
        let nonce = hex::encode(rand::random::<[u8; 32]>());
        let now = Instant::now();
        let ttl = self.ttl();
        let mut live = self.live.lock().expect("the nonce map is never poisoned");
        // One window per nonce, so the map holds as many entries as the owner
        // opened windows in the last minute. Pruning here keeps a long-running
        // shell from accumulating dead ones for the life of the process.
        live.retain(|_, issued| now.duration_since(*issued) <= ttl);
        live.insert(nonce.clone(), now);
        nonce
    }

    /// Spend a nonce: `true` if it was live, `false` if it was unknown,
    /// already used, or older than the TTL.
    fn take(&self, nonce: &str) -> bool {
        let issued = self
            .live
            .lock()
            .expect("the nonce map is never poisoned")
            .remove(nonce);
        match issued {
            Some(issued) => issued.elapsed() <= self.ttl(),
            None => false,
        }
    }
}

/// Mint a login nonce and say so in the process log (§3.4).
///
/// The shell calls this every time it creates its window: the webview's first
/// history entry is then a dead nonce rather than the durable key, so nothing
/// that reads back a browser history finds a credential there.
pub fn mint_login_nonce(state: &SharedState) -> String {
    let nonce = state.login_nonces.mint();
    // At `debug`, with the value: the nonce is single-use and short-lived, and
    // an owner debugging a window that opened on the login card needs to see
    // which nonce the shell handed it and how long it had.
    tracing::debug!(
        "minted login nonce {nonce}, single-use, valid {}s",
        state.login_nonces.ttl().as_secs()
    );
    nonce
}

/// Exchange a nonce, once (§3.4). `true` means the caller may have the cookie.
pub fn exchange_nonce(state: &SharedState, nonce: &str) -> bool {
    let ok = state.login_nonces.take(nonce);
    if !ok {
        tracing::debug!("a login nonce was presented that is unknown, spent or expired");
    }
    ok
}

// ---------------------------------------------------------------------------
// The cookie
// ---------------------------------------------------------------------------

/// The `Set-Cookie` value, in the one shape §3.3 gives it.
///
/// No `Domain` (host-only) and no `Secure` (the listener speaks `http`), both
/// by omission and both deliberate — see the module docs.
fn set_cookie(key: &str) -> String {
    format!(
        "{SESSION_COOKIE}={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_MAX_AGE_SECS}"
    )
}

/// The same cookie with `Max-Age=0`, which is how a browser is told to drop
/// it. The attributes have to match the ones it was set with, or the jar keeps
/// the original entry beside the expired one.
fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Attach a `Set-Cookie` to a response.
fn with_cookie(mut resp: Response, cookie: String) -> Response {
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            resp.headers_mut().insert(header::SET_COOKIE, value);
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

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/api/session/login", get(login))
        .route("/api/session", get(current).post(sign_in).delete(sign_out))
        // `Public`: this is the plane a browser with no session talks to, and
        // each handler verifies the credential it was given itself.
        .route_layer(crate::server::require(state, Cap::Public))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct LoginQuery {
    nonce: Option<String>,
    token: Option<String>,
}

/// `GET /api/session/login?nonce=<n>` or `?token=<key>` (§3.4).
///
/// The shell's window opens here with a nonce; the owner's browser opens here
/// with the token from the line `server::run` printed. Either match sets the
/// cookie and lands on `/`; anything else lands on `/?login=invalid` with no
/// cookie, which is the card the SPA shows for a link that did not work.
///
/// The §3.6 rule applies **unconditionally** — there is no cookie yet, so
/// there is nothing for `principal_mw` to have applied it to. `Sec-Fetch-Site:
/// none` is a typed URL, a bookmark or the shell and is the normal case here;
/// `cross-site` is a foreign page navigating this browser to a key of its
/// choosing, which is the one thing this route must not do.
async fn login(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    if let Err(refusal) = same_origin(&headers) {
        return refusal.into_response();
    }
    let snap = state.snapshot();
    let key = match (&q.nonce, &q.token) {
        (Some(nonce), _) if exchange_nonce(&state, nonce) => {
            // A nonce is the shell's, and the shell's session is the door key.
            let key = snap.owner_key(OWNER_DASHBOARD).map(str::to_string);
            if key.is_none() {
                // Unreachable: the row is seeded at init and cannot be
                // disabled or deleted (§3.1). Said out loud anyway, because
                // the owner would otherwise see a window that landed on the
                // login card with nothing in the log to explain it.
                tracing::warn!(
                    "a login nonce was exchanged, but '{OWNER_DASHBOARD}' is missing or \
                     disabled — there is no session to hand back"
                );
            }
            key
        }
        // A token is whichever owner row it matches — a second browser or a
        // LAN device logs in as its own row, not as the dashboard's.
        (_, Some(token)) => enabled_owner(&snap, token).map(|_| token.clone()),
        _ => None,
    };
    match key {
        Some(key) => with_cookie(found("/"), set_cookie(&key)),
        None => found("/?login=invalid"),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SignIn {
    token: String,
}

/// `POST /api/session { token }` (§3.4) — the paste fallback, and the way the
/// SPA re-logs itself after rotating `owner:dashboard` (§3.12).
///
/// Same verification as the `GET`, same cookie, `204`. A token that matches no
/// enabled owner row is `401 login_invalid`: the credential the caller sent is
/// the thing that failed, and it is not `session_required` — that one means
/// "you sent nothing", and the card would print the wrong sentence.
async fn sign_in(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<SignIn>,
) -> Response {
    if let Err(refusal) = same_origin(&headers) {
        return refusal.into_response();
    }
    if enabled_owner(&state.snapshot(), &body.token).is_none() {
        tracing::debug!("a sign-in presented a token that matches no enabled owner row");
        return Refusal {
            status: StatusCode::UNAUTHORIZED,
            code: "login_invalid",
            message: "that is not an enabled owner key — copy it again from Usage → Keys, or \
                      from the login link in the process log"
                .into(),
        }
        .into_response();
    }
    with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        set_cookie(&body.token),
    )
}

/// What `GET /api/session` answers: the current principal, as the dashboard's
/// load check reads it.
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub(crate) struct SessionView {
    authenticated: bool,
    /// `key`, `agent`, `owner` — empty when nobody is signed in. Present
    /// and empty rather than absent, so the response has one shape.
    kind: String,
    /// The name of the API key in use (`owner:dashboard` for the dashboard's
    /// own), empty when nobody is signed in.
    name: String,
}

/// `GET /api/session` — never a `401`, whatever the request carried (§3.4).
///
/// It is the question "am I logged in", and answering it with a refusal would
/// make the SPA's load check indistinguishable from a gateway that is down.
async fn current(Extension(ctx): Extension<RequestCtx>) -> Json<SessionView> {
    Json(match &ctx.principal {
        Principal::Anonymous => SessionView::default(),
        Principal::Key { kind, name, .. } => SessionView {
            authenticated: true,
            kind: kind.as_str().to_string(),
            name: name.clone(),
        },
    })
}

/// `DELETE /api/session` — clear the cookie, `204`.
///
/// Same-origin only, like the `GET` and the `POST` beside it (§3.6). Dropping
/// a session is a write to the browser's jar, and a foreign page that can make
/// this call logs the owner out of their own dashboard — small, but it is a
/// cross-site request forgery and the rule that answers it is already written.
///
/// Nothing else is asked: a caller holding a session it wants to stop holding
/// is never refused, whatever that session resolves to, and the key itself is
/// untouched (that is Rotate, §3.12).
async fn sign_out(headers: HeaderMap) -> Response {
    if let Err(refusal) = same_origin(&headers) {
        return refusal.into_response();
    }
    with_cookie(StatusCode::NO_CONTENT.into_response(), clear_cookie())
}

/// The enabled **owner** row a presented key matches, if any.
///
/// `verify_api_key` already skips disabled rows; the kind filter is what makes
/// this a *login*: a client or agent key is a perfectly good credential in the
/// bearer position and is not one here (§3.3).
fn enabled_owner<'a>(snap: &'a Snapshot, presented: &str) -> Option<&'a ApiKey> {
    snap.verify_api_key(presented)
        .filter(|k| k.kind == ApiKeyKind::Owner)
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
            set_cookie("lmgw-owner-abc"),
            "lmgw_session=lmgw-owner-abc; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"
        );
        assert_eq!(SESSION_MAX_AGE_SECS, 365 * 24 * 60 * 60);
        // The clearing cookie differs in `Max-Age` and in nothing else: a
        // browser matches the entry to drop by name, path and domain.
        assert_eq!(
            clear_cookie(),
            "lmgw_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
        );
    }

    #[test]
    fn a_nonce_is_single_use_and_then_expires() {
        let desk = LoginNonces::default();
        assert_eq!(desk.ttl(), LOGIN_NONCE_TTL);

        let nonce = desk.mint();
        assert!(desk.take(&nonce), "the first exchange");
        assert!(!desk.take(&nonce), "the second one is a spent nonce");
        assert!(!desk.take("not-a-nonce"));

        // Expiry, on a window narrow enough to test: `take` is the real one.
        desk.set_ttl(Duration::from_millis(0));
        let nonce = desk.mint();
        std::thread::sleep(Duration::from_millis(5));
        assert!(!desk.take(&nonce), "older than the TTL");
    }
}
