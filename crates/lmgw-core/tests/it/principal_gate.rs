//! The gate itself (principals design §3.5, §3.6, §10 Part 1).
//!
//! `route_walk.rs` asks whether every route declares the right capability.
//! This asks what the two layers *do* once one has: which checks run where,
//! and which of them a cookie-carrying browser is additionally held to.

use crate::common;

use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};

use common::{serve, Gw};

/// A batch agent with nothing to run — the ledger routes never execute it, and
/// what is under test is who may write to its run.
fn doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Labeler",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }}
  }} }} }},
  "tools": [],
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search" }},
    "item": {{ "id": "{{{{item.id}}}}", "columns": {{ "subject": "{{{{item.subject}}}}" }} }},
    "limits": {{ "deadline_seconds": 0 }} }}
}}"#
    )
}

async fn op(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// `POST` as one agent token — the caller a container is.
async fn post_as(gw: &Gw, path: &str, token: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .anon()
        .post(format!("{gw}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// `policy::admit` — expiry, rpm, tpm, concurrency — runs on `Inference` and
/// **only** there (§3.5).
///
/// An agent flushing a hundred ledger rows a minute is not spending its
/// request budget: the budget is about model calls, and before this it was
/// spent by whichever half of the agent's traffic happened to arrive first.
/// One key with `rpm_limit: 1` says both halves of that in one test.
#[tokio::test]
async fn policy_admit_runs_on_inference_and_never_on_the_ledger() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, body) = op(
        &gw,
        "agent_set",
        json!({ "manifest": doc("labeler"), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, minted) = op(&gw, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = minted["token"].as_str().unwrap().to_string();

    sqlx::query("UPDATE api_keys SET rpm_limit = 1 WHERE name = 'agent:labeler'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, opened) = post_as(&gw, "/api/agents/labeler/runs", &token, json!({})).await;
    assert_eq!(
        status, 200,
        "opening a run is `Ledger`, not a model call: {opened}"
    );
    let run = opened["run"].as_i64().unwrap();

    // Two events inside one minute, on a key allowed one request a minute.
    for nth in 1..=2 {
        let (status, body) = post_as(
            &gw,
            &format!("/api/agents/runs/{run}/events"),
            &token,
            json!({ "type": "log", "message": "hello" }),
        )
        .await;
        assert_eq!(status, 200, "ledger event {nth}: {body}");
    }

    // The same key, the same minute, on `/v1`: the limit is real there.
    let models = || async {
        gw.anon()
            .get(format!("{gw}/v1/models"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(models().await, 200, "the first model call of the minute");
    assert_eq!(
        models().await,
        429,
        "the second one is over `rpm_limit`, which the ledger never touched"
    );
}

// ---------------------------------------------------------------------------
// §3.6 — the same-origin rule for cookie requests
// ---------------------------------------------------------------------------

/// `GET /api/status` with the session in the **cookie**, plus whatever browser
/// headers the case is about.
async fn as_browser(gw: &Gw, headers: &[(&str, String)]) -> (u16, Value) {
    let mut req = gw
        .anon()
        .get(format!("{gw}/api/status"))
        .header("cookie", format!("lmgw_session={}", gw.key));
    for (name, value) in headers {
        req = req.header(*name, value);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// The rule is checked against the request's **own `Host`** (§3.6), which is
/// what makes a LAN-bound gateway opened from a LAN address work — and what
/// makes the one case `SameSite=Strict` cannot see, a page on the same site
/// but another port, a refusal.
#[tokio::test]
async fn a_cookie_is_honoured_only_from_the_gateways_own_origin() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    let own = gw.base.clone();

    let (status, _) = as_browser(&gw, &[("origin", own.clone())]).await;
    assert_eq!(status, 200, "the dashboard's own origin");

    for foreign in [
        // Same site, another port: a local process can serve this.
        "http://127.0.0.1:9999".to_string(),
        "http://evil.example".to_string(),
        // The listener speaks `http` and nothing else, so this is not it.
        own.replace("http://", "https://"),
    ] {
        let (status, body) = as_browser(&gw, &[("origin", foreign.clone())]).await;
        assert_eq!(status, 403, "{foreign}: {body}");
        assert_eq!(body["code"], json!("cross_origin_refused"), "{body}");
    }

    // No `Origin`: the fetch metadata decides, and `none` is the owner typing
    // the address or opening a bookmark.
    for site in ["same-origin", "none"] {
        let (status, body) = as_browser(&gw, &[("sec-fetch-site", site.to_string())]).await;
        assert_eq!(status, 200, "{site}: {body}");
    }
    for site in ["cross-site", "same-site"] {
        let (status, body) = as_browser(&gw, &[("sec-fetch-site", site.to_string())]).await;
        assert_eq!(status, 403, "{site}: {body}");
        assert_eq!(body["code"], json!("cross_origin_refused"), "{body}");
    }

    // Neither header at all: curl, a container, the shell's own window.
    assert_eq!(as_browser(&gw, &[]).await.0, 200);

    // A **bearer** skips the rule entirely: `curl` sends no `Origin`, and a
    // bearer is not something a foreign page can make a browser attach.
    let resp = gw
        .client()
        .get(format!("{gw}/api/status"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// A cookie carrying a **client** key is not a credential in that position
/// (§3.3), so the request is anonymous — and the same-origin rule, which is
/// about cookies that authenticate, does not fire on it either.
#[tokio::test]
async fn a_client_key_in_the_cookie_is_anonymous_not_refused() {
    let state = AppState::init_for_tests().await.unwrap();
    let plaintext = "lmgw-in-the-wrong-jar";
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('laptop', ?1, 1)")
        .bind(lmgw_core::config::hash_api_key(plaintext))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state).await;

    let resp = gw
        .anon()
        .get(format!("{gw}/api/status"))
        .header("cookie", format!("lmgw_session={plaintext}"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["code"],
        json!("session_required"),
        "not `cross_origin_refused`: nothing authenticated, so there was no \
         cookie principal to hold to the rule: {body}"
    );
}

/// The other half of §3.3's "a matched disabled row is never a fall-through":
/// a **client** key the owner switched off says so, where it used to read as
/// the wording for a typo.
#[tokio::test]
async fn a_disabled_client_key_names_itself_on_v1() {
    let state = AppState::init_for_tests().await.unwrap();
    let plaintext = "lmgw-switched-off";
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('laptop', ?1, 0)")
        .bind(lmgw_core::config::hash_api_key(plaintext))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state).await;

    let resp = gw
        .anon()
        .get(format!("{gw}/v1/models"))
        .header("authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    // `/v1` answers in the client's own dialect, as it always has (§3.5).
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], json!("key_disabled"), "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("laptop"),
        "{body}"
    );
}

/// Attribution follows the principal now, **including with Require API key
/// off** (§3.5): a key that was presented is a key that is logged, where
/// before the toggle decided whether the gateway bothered to look.
#[tokio::test]
async fn a_presented_client_key_is_attributed_with_the_toggle_off() {
    let state = AppState::init_for_tests().await.unwrap();
    let plaintext = "lmgw-attributed";
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('laptop', ?1, 1)")
        .bind(lmgw_core::config::hash_api_key(plaintext))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    assert!(
        !state.snapshot().settings.auth_enabled,
        "the default install has the toggle off"
    );
    let gw = serve(state.clone()).await;

    let status = gw
        .anon()
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {plaintext}"))
        .json(&json!({ "model": "no-such-alias", "messages": [] }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 404, "an unknown alias, logged as a refusal");

    let rows = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.client_key.as_deref()),
        Some("laptop"),
        "the row names the key that made the call"
    );
}

/// `Ledger` is **agent-only** (§3.2), so the *layer* is what refuses an owner
/// — not the handler's ownership check.
///
/// Both are `403` and the two are easy to confuse, which is why the code is
/// asserted: `forbidden` means the credential does not hold the capability at
/// all, and `run_not_owned` means it holds it over somebody else's run. An
/// owner's is the first, always, whether or not the run exists — a run is
/// written by the agent that owns it, and an owner has nothing to gain from
/// forging events into one.
#[tokio::test]
async fn an_owner_does_not_hold_the_ledger_at_all() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    let resp = gw
        .client()
        .post(format!("{gw}/api/agents/runs/1/events"))
        .json(&json!({ "type": "log", "message": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("forbidden"), "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("ledger"), "{message}");
    assert!(message.contains("owner key"), "{message}");
}

/// `AgentSelf` is "own id, own runs" (§3.2): the layer admits *an* agent, and
/// the handler is what says which.
#[tokio::test]
async fn an_agent_reads_its_own_row_and_not_another_agents() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for id in ["mine", "yours"] {
        let (status, body) = op(
            &gw,
            "agent_set",
            json!({ "manifest": doc(id), "replace": true }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    let (_, minted) = op(&gw, "agent_token_get", json!({ "id": "mine" })).await;
    let token = minted["token"].as_str().unwrap().to_string();

    let read = |id: &'static str| {
        let token = token.clone();
        let gw = &gw;
        async move {
            let resp = gw
                .anon()
                .get(format!("{gw}/api/agents/{id}"))
                .header("authorization", format!("Bearer {token}"))
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            (status, body)
        }
    };

    let (status, body) = read("mine").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], json!("mine"));

    let (status, body) = read("yours").await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], json!("agent_not_owned"), "{body}");
}

/// A container reaching for the configuration plane is refused by capability,
/// not by luck (§3.10): its token is a credential, and `403` says exactly that
/// — the credential is real and does not hold this.
#[tokio::test]
async fn an_agent_token_is_forbidden_on_the_admin_plane() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    let (status, body) = op(
        &gw,
        "agent_set",
        json!({ "manifest": doc("labeler"), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, minted) = op(&gw, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = minted["token"].as_str().unwrap();

    let resp = gw
        .anon()
        .post(format!("{gw}/api/op/settings_set"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "auth_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("forbidden"), "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("admin"), "{message}");
    assert!(message.contains("labeler"), "{message}");
}
