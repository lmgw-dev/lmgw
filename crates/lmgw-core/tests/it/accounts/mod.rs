//! Accounts and sessions (headless server design §5, §12's accounts,
//! sessions, desktop-off, switch, throttle and login-link groups).
//!
//! No cookie jar, as in `session_login`: each case reads the `Set-Cookie`
//! headers the gateway answered and presents the cookie back by hand.

use crate::common;

use lmgw_core::state::SharedState;
use serde_json::{json, Value};

pub use common::{serve, Gw};

// Starts an agent (crate::agents_service's), which is not on Windows
// (Windows design §4.5).
#[cfg(unix)]
mod agent_mcp;
mod keys;
mod links;
mod off;
mod seed;
mod sessions;
mod sign_in;
mod start_log;
mod switch;
mod throttle;

/// A password every case can use: 12 code points and more.
pub const PASSWORD: &str = "correct horse battery staple";

/// A client that follows no redirect and carries nothing.
pub fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Every `Set-Cookie` the answer carries, as `(name, value, whole header)`.
pub fn set_cookies(resp: &reqwest::Response) -> Vec<(String, String, String)> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|h| {
            let (pair, _) = h.split_once(';').unwrap_or((h, ""));
            let (n, v) = pair.split_once('=')?;
            Some((n.to_string(), v.to_string(), h.to_string()))
        })
        .collect()
}

/// The `lmgw_session` value the answer set.
pub fn session_of(resp: &reqwest::Response) -> Option<String> {
    set_cookies(resp)
        .into_iter()
        .find(|(n, ..)| n == "lmgw_session")
        .map(|(_, v, _)| v)
}

/// `POST /api/op/{name}` with the dashboard's bearer (account 1, no
/// session).
pub async fn op(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST /api/op/{name}` as the browser holding `session`.
pub async fn op_as(gw: &Gw, session: &str, name: &str, args: Value) -> (u16, Value) {
    let resp = browser()
        .post(format!("{gw}/api/op/{name}"))
        .header("cookie", format!("lmgw_session={session}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `GET path` as the browser holding `session`.
pub async fn get_as(gw: &Gw, session: &str, path: &str) -> (u16, Value) {
    let resp = browser()
        .get(format!("{gw}{path}"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Turn accounts on with the dashboard's bearer: the session the answer
/// hands account 1's browser.
pub async fn turn_on(gw: &Gw) -> String {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/accounts_switch"))
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    session_of(&resp).expect("switching on hands this browser a session")
}

/// `POST /api/session { login, password }` from loopback: status, body,
/// and the session it set.
pub async fn sign_in(gw: &Gw, login: &str, password: &str) -> (u16, Value, Option<String>) {
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "login": login, "password": password }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let session = session_of(&resp);
    (status, resp.json().await.unwrap_or(Value::Null), session)
}

/// A new account with `login` and its password set through its link:
/// its id and the session the link's page signed in.
pub async fn account_with_password(gw: &Gw, login: &str) -> (i64, String) {
    let (status, link) = op(gw, "account_create", json!({ "login": login })).await;
    assert_eq!(status, 200, "{link}");
    let token = link["url"]
        .as_str()
        .unwrap()
        .split_once("#token=")
        .unwrap()
        .1
        .to_string();
    let resp = browser()
        .post(format!("{gw}/api/account/password"))
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let session = session_of(&resp).unwrap();
    (link["account_id"].as_i64().unwrap(), session)
}

/// The number of rows in `table`.
pub async fn rows(state: &SharedState, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// Trust this machine's loopback as a proxy (§4.5), so a case can sign in
/// from several client addresses (`sign_in_from`).
pub async fn trust_loopback_proxy(state: &SharedState) {
    let mut s = state.snapshot().settings.clone();
    s.trusted_proxies = vec!["127.0.0.1/32".into()];
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A password sign-in through the loopback proxy, from client `ip` over
/// `https` (`trust_loopback_proxy` first): status and body.
pub async fn sign_in_from(gw: &Gw, ip: &str, login: &str, password: &str) -> (u16, Value) {
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .header("x-forwarded-for", ip)
        .header("x-forwarded-proto", "https")
        .json(&json!({ "login": login, "password": password }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Account `id`'s failed sign-ins as the card reads them, once the count a
/// failure writes after its answer has landed (`want`), or what it is
/// after five seconds.
pub async fn failed_logins(gw: &Gw, id: i64, want: i64) -> i64 {
    let mut n = -1;
    for _ in 0..100 {
        let (_, list) = op(gw, "account_list", json!({})).await;
        n = list["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()["failed_logins"]
            .as_i64()
            .unwrap();
        if n == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    n
}

/// Reads a streamed answer until it ends: its text, when it did `within`.
pub async fn text_until_end(
    resp: reqwest::Response,
    within: std::time::Duration,
) -> Option<String> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut text = Vec::new();
    tokio::time::timeout(within, async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => text.extend_from_slice(&c),
                Err(_) => break,
            }
        }
    })
    .await
    .ok()?;
    Some(String::from_utf8_lossy(&text).into_owned())
}

/// The `revoked` frame's payload in an `/api/events` stream's text.
pub fn revoked_frame(text: &str) -> Option<Value> {
    let (_, rest) = text.split_once(
        "event: revoked
",
    )?;
    let data = rest.lines().find_map(|l| l.strip_prefix("data: "))?;
    serde_json::from_str(data).ok()
}

/// Reads a streamed answer until it ends: whether it did `within`.
// Its one user (`agent_mcp`) is Unix's: agents are not on Windows (§4.5).
#[cfg_attr(windows, allow(dead_code))]
pub async fn ends_within(resp: reqwest::Response, within: std::time::Duration) -> bool {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    tokio::time::timeout(within, async {
        while let Some(chunk) = stream.next().await {
            if chunk.is_err() {
                break;
            }
        }
    })
    .await
    .is_ok()
}
