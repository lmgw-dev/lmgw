//! The login (principals design §3.4, §3.6, §10 Part 1).
//!
//! What a browser does on its way in: a nonce or a key in a link, a key in a
//! paste, the cookie that comes back, and the four ways that cookie stops
//! working (spent, expired, cleared, rotated).
//!
//! There is no cookie jar here — `reqwest` is built without one — so the jar
//! is done by hand: [`session_of`] reads what the gateway set and [`cookie`]
//! presents it back, which is exactly the round trip a browser makes and is
//! visible in the test rather than hidden in a client.

use crate::common;

use std::time::Duration;

use lmgw_core::agents::token::{set_owner_key, OWNER_DASHBOARD};
use lmgw_core::state::AppState;
use lmgw_core::web::session::{mint_login_nonce, SESSION_MAX_AGE_SECS};
use serde_json::{json, Value};

use common::{serve, Gw};

/// A browser before it has anything: no credential, and no redirect following
/// — what the login route *said* is the whole question here.
fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client with no redirect policy always builds")
}

/// The whole `Set-Cookie` header, if there is one.
fn set_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("set-cookie")?
        .to_str()
        .ok()
        .map(str::to_string)
}

/// The `lmgw_session` value out of it — what the jar would keep.
fn session_of(resp: &reqwest::Response) -> Option<String> {
    let header = set_cookie(resp)?;
    let value = header.strip_prefix("lmgw_session=")?;
    Some(value.split(';').next().unwrap_or_default().to_string())
}

fn location(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// `GET /api/session/login` with whatever query and headers the case is about.
async fn login(gw: &Gw, query: &str, headers: &[(&str, String)]) -> reqwest::Response {
    let mut req = browser().get(format!("{gw}/api/session/login?{query}"));
    for (name, value) in headers {
        req = req.header(*name, value);
    }
    req.send().await.unwrap()
}

/// A request carrying the session cookie, as a browser that holds one sends it.
async fn cookie(gw: &Gw, path: &str, session: &str) -> (u16, Value) {
    let resp = browser()
        .get(format!("{gw}{path}"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------
// The nonce (§3.4)
// ---------------------------------------------------------------------------

/// The shell's login: one nonce, one exchange, and the cookie it hands back is
/// the dashboard key — the nonce itself is dead the moment it is spent, which
/// is what makes a webview history entry harmless.
#[tokio::test]
async fn a_nonce_is_exchanged_exactly_once() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let nonce = mint_login_nonce(&state);

    let resp = login(&gw, &format!("nonce={nonce}"), &[]).await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location(&resp), "/");
    assert_eq!(
        session_of(&resp).as_deref(),
        Some(gw.key.as_str()),
        "a nonce exchanges for the `owner:dashboard` key"
    );

    // The second use of the same link — a reload, a back button, or someone
    // reading it out of the log a minute later.
    let resp = login(&gw, &format!("nonce={nonce}"), &[]).await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location(&resp), "/?login=invalid");
    assert_eq!(set_cookie(&resp), None, "a spent nonce sets no cookie");
}

/// Expiry, on the real `take` and a window narrow enough to watch.
///
/// `LOGIN_NONCE_TTL` is 60 s, and a test that slept a minute would be a test
/// nobody runs; `set_ttl` is the documented seam for exactly this, so what is
/// exercised is the production path with only the width changed.
#[tokio::test]
async fn an_expired_nonce_is_no_longer_a_login() {
    let state = AppState::init_for_tests().await.unwrap();
    state.login_nonces.set_ttl(Duration::from_millis(50));
    let gw = serve(state.clone()).await;
    let nonce = mint_login_nonce(&state);

    tokio::time::sleep(Duration::from_millis(150)).await;
    let resp = login(&gw, &format!("nonce={nonce}"), &[]).await;
    assert_eq!(location(&resp), "/?login=invalid");
    assert_eq!(set_cookie(&resp), None);
}

// ---------------------------------------------------------------------------
// The token (§3.3, §3.4)
// ---------------------------------------------------------------------------

/// The link the process log prints, and the cookie §3.3 writes — asserted as
/// the literal header, because the attributes *are* the security properties:
/// `HttpOnly` is why no script can read it, `SameSite=Strict` is the CSRF
/// defence, and the absent `Domain` is why a LAN address and loopback never
/// share a session.
#[tokio::test]
async fn a_token_link_sets_the_cookie_the_spec_writes() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let resp = login(&gw, &format!("token={}", gw.key), &[]).await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location(&resp), "/");
    assert_eq!(
        set_cookie(&resp).unwrap(),
        format!(
            "lmgw_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_MAX_AGE_SECS}",
            gw.key
        )
    );
    assert_eq!(SESSION_MAX_AGE_SECS, 31_536_000, "one year (§3.3)");

    // And the cookie is a session: the `Admin` plane answers it.
    let session = session_of(&resp).unwrap();
    assert_eq!(cookie(&gw, "/api/status", &session).await.0, 200);
}

/// A key that matches no row is a statement about the *link*, so it lands on
/// the card with no cookie — never a `401`, which a browser would render as
/// nothing at all.
#[tokio::test]
async fn a_token_that_matches_nothing_sets_no_cookie() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    for query in ["token=lmgw-owner-nope", "nonce=nope", ""] {
        let resp = login(&gw, query, &[]).await;
        assert_eq!(resp.status().as_u16(), 302, "{query}");
        assert_eq!(location(&resp), "/?login=invalid", "{query}");
        assert_eq!(set_cookie(&resp), None, "{query}");
    }
}

/// The paste fallback and the Rotate follow-up (§3.12): same verification,
/// same cookie, `204` — and `401 login_invalid` for a key that is not one,
/// which is a different sentence from `session_required` ("you sent nothing").
#[tokio::test]
async fn the_post_takes_a_key_and_answers_204() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "token": gw.key }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    let session = session_of(&resp).expect("the cookie is set here too");
    assert_eq!(session, gw.key);
    assert_eq!(cookie(&gw, "/api/status", &session).await.0, 200);

    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "token": "lmgw-owner-nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    assert_eq!(set_cookie(&resp), None);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("login_invalid"), "{body}");
}

// ---------------------------------------------------------------------------
// Reading and dropping the session (§3.4)
// ---------------------------------------------------------------------------

/// What the SPA asks on load. Never a `401`: "am I logged in" answered with a
/// refusal is indistinguishable from a gateway that is down.
#[tokio::test]
async fn the_session_route_reports_the_current_principal() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let (status, body) = cookie(&gw, "/api/session", &gw.key).await;
    assert_eq!(status, 200);
    assert_eq!(body["authenticated"], json!(true), "{body}");
    assert_eq!(body["kind"], json!("owner"), "{body}");
    assert_eq!(body["name"], json!("owner:dashboard"), "{body}");

    let resp = browser()
        .get(format!("{gw}/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "anonymous is an answer, not a refusal"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["authenticated"], json!(false), "{body}");
    assert_eq!(body["kind"], json!(""), "{body}");
}

/// `DELETE` clears it: the same cookie with `Max-Age=0` — the attributes have
/// to match the ones it was set with, or the jar keeps the original entry
/// beside the expired one.
#[tokio::test]
async fn delete_clears_the_cookie_and_the_session_with_it() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={}", gw.key))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert_eq!(
        set_cookie(&resp).unwrap(),
        "lmgw_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
    );

    // What the browser holds afterwards is that empty value, and it is not a
    // credential: the next `Admin` call is the login refusal again.
    let session = session_of(&resp).unwrap();
    let (status, body) = cookie(&gw, "/api/status", &session).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("session_required"), "{body}");
}

// ---------------------------------------------------------------------------
// The same-origin rule on the login GET (§3.6)
// ---------------------------------------------------------------------------

/// The rule applies **unconditionally** here — there is no cookie yet, so
/// `principal_mw` had nothing to apply it to — and `none` is admitted, because
/// a typed URL, a bookmark and the shell's own window are what this route is
/// for. `cross-site` is the case it exists to refuse: a foreign page must not
/// be able to navigate this browser into a key of its choosing.
#[tokio::test]
async fn the_login_link_is_only_followed_from_this_gateway_or_nowhere() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    let query = format!("token={}", gw.key);

    for site in ["none", "same-origin"] {
        let resp = login(&gw, &query, &[("sec-fetch-site", site.to_string())]).await;
        assert_eq!(location(&resp), "/", "{site}");
        assert!(session_of(&resp).is_some(), "{site}");
    }
    for site in ["cross-site", "same-site"] {
        let resp = login(&gw, &query, &[("sec-fetch-site", site.to_string())]).await;
        assert_eq!(resp.status().as_u16(), 403, "{site}");
        assert_eq!(set_cookie(&resp), None, "{site}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["code"], json!("cross_origin_refused"), "{body}");
    }

    // An `Origin`, if the browser sent one, must be this gateway's own.
    let resp = login(&gw, &query, &[("origin", gw.base.clone())]).await;
    assert_eq!(location(&resp), "/");
    assert!(session_of(&resp).is_some());
    for foreign in ["http://127.0.0.1:9999", "http://evil.example"] {
        let resp = login(&gw, &query, &[("origin", foreign.to_string())]).await;
        assert_eq!(resp.status().as_u16(), 403, "{foreign}");
        assert_eq!(set_cookie(&resp), None, "{foreign}");
    }

    // The `POST` is held to the same rule, and for the same reason.
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .header("origin", "http://evil.example")
        .json(&json!({ "token": gw.key }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(set_cookie(&resp), None);
}

/// And so is the `DELETE` (§3.6). Logging the owner out is a small thing to
/// do to them, but it is a write to their jar, and a foreign page that can
/// make this call does it without being asked — the rule that answers that is
/// already written, and this route was the one that skipped it.
#[tokio::test]
async fn a_cross_site_page_cannot_log_the_owner_out() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={}", gw.key))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(set_cookie(&resp), None, "nothing was cleared");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("cross_origin_refused"), "{body}");

    // A foreign `Origin` is the same refusal, and the session is untouched by
    // either: the browser still holds a cookie that opens the `Admin` plane.
    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={}", gw.key))
        .header("origin", "http://127.0.0.1:9999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(cookie(&gw, "/api/status", &gw.key).await.0, 200);

    // The owner's own tab still logs out, which is what the route is for.
    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={}", gw.key))
        .header("origin", gw.base.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert!(set_cookie(&resp).is_some());
}

// ---------------------------------------------------------------------------
// Rotation and a disabled row (§3.1, §3.12)
// ---------------------------------------------------------------------------

/// Rotating `owner:dashboard` invalidates every browser's cookie, including
/// the one that rotated it — the old value now matches no row at all, so it is
/// `session_required` and not `owner_key_disabled`.
#[tokio::test]
async fn a_rotated_dashboard_key_kills_the_old_cookie() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    assert_eq!(cookie(&gw, "/api/status", &gw.key).await.0, 200);

    let fresh = lmgw_core::agents::token::mint_owner();
    set_owner_key(&state, OWNER_DASHBOARD, &fresh, true)
        .await
        .unwrap();

    let (status, body) = cookie(&gw, "/api/status", &gw.key).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("session_required"), "{body}");
    assert_eq!(cookie(&gw, "/api/status", &fresh).await.0, 200);
}

/// A browser holding the cookie of an owner row the owner has since switched
/// off must still be able to *see* the login card (§3.4).
///
/// The refusal `principal_mw` parks in the extensions is rendered by every
/// `require` — except `require(Public)`, which needs no credential and so has
/// no business judging a bad one. Without that exception the SPA itself is
/// `401 owner_key_disabled`, the browser renders nothing, and the way back in
/// is the process log.
#[tokio::test]
async fn a_disabled_owner_cookie_still_gets_the_login_card() {
    let state = AppState::init_for_tests().await.unwrap();
    let second = lmgw_core::agents::token::mint_owner();
    set_owner_key(&state, "owner:second", &second, false)
        .await
        .unwrap();
    let gw = serve(state).await;

    let resp = browser()
        .get(format!("{gw}/"))
        .header("cookie", format!("lmgw_session={second}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the SPA is served");
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html"),
        "and it is the page, not an error body"
    );

    let (status, body) = cookie(&gw, "/api/session", &second).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["authenticated"],
        json!(false),
        "a disabled row authenticates nothing: {body}"
    );

    // Every non-`Public` route still names the row, which is the whole point
    // of a disabled credential not reading as a typo (§3.3).
    let (status, body) = cookie(&gw, "/api/status", &second).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("owner_key_disabled"), "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("second"),
        "{body}"
    );

    // But it names it to the **owner**. The same cookie replayed by a page on
    // another port hears the origin rule and nothing else (§3.6): which of
    // the owner's keys a browser is holding, and whether it is switched on,
    // is not something a foreign page gets to learn by asking.
    let resp = browser()
        .get(format!("{gw}/api/status"))
        .header("cookie", format!("lmgw_session={second}"))
        .header("origin", "http://127.0.0.1:9999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("cross_origin_refused"), "{body}");
    assert!(
        !body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("second"),
        "the refusal must not name the row either: {body}"
    );
}
