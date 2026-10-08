//! Agent identity and the run ledger over real HTTP (container-runtime design
//! §3.1, §3.2 — WP1).
//!
//! Two things are under test and they are the same thing from two sides:
//!
//! - the **token** — honoured whether or not `auth_enabled` is on, scoped to
//!   the model aliases the config currently names, and filtering `/mcp` down to
//!   exactly the manifest's allow list;
//! - the **ledger** — three routes that carry their own bearer check, because
//!   they live on the `/api` plane and cannot inherit one, turning a
//!   hand-posted event sequence into rows the Run tab renders.
//!
//! Everything here runs against the real router with **auth off**, which is the
//! shipped default and therefore the configuration in which an agent token has
//! to work. No podman, no container: the ledger's whole point is that whatever
//! writes to it is somebody else's process.

use std::time::Duration;

use lmgw_core::config::{McpTransport, Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewMcpServer, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub(crate) async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("op {name} is not JSON ({e}): {body}"));
    (status, v)
}

async fn get_json(base: &Gw, path: &str) -> Value {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
}

/// A POST to a ledger route with an optional bearer. Returns status + body.
/// A POST as one named credential, or — with `None` — as a caller with **no**
/// credential at all, which is a different thing now that the dashboard key is
/// what `Gw::client` presents.
pub(crate) async fn post_as(
    base: &Gw,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut req = base.anon().post(format!("{base}{path}")).json(&body);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
    )
}

/// The test agent: two tools on the surface, exactly one of them allowed.
pub(crate) fn doc(id: &str, deadline_seconds: u64) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Labeler",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }}
  }} }} }},
  "tools": [ {{ "label": "gws", "allowed": ["gws__search"] }} ],
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search" }},
    "item": {{ "id": "{{{{item.id}}}}", "columns": {{ "subject": "{{{{item.subject}}}}" }} }},
    "limits": {{ "deadline_seconds": {deadline_seconds} }} }}
}}"#
    )
}

pub(crate) async fn install(base: &Gw, manifest: &str) {
    let (status, body) = op(
        base,
        "agent_set",
        json!({ "manifest": manifest, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "agent_set: {body}");
}

/// Register a `gws`-prefixed server and seed its two tools, without a live MCP
/// server. Not `set_snapshot_for_tests`: that replaces the whole snapshot and
/// would take the agent's `api_keys` row with it.
async fn seed_tools(state: &SharedState) {
    let id = store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "gws".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some("http://127.0.0.1:1/mcp".into()),
            headers: vec![],
            tool_prefix: "gws".into(),
            timeout_ms: 1_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let schema = json!({ "type": "object", "properties": {} });
    state
        .mcp
        .seed_ready_conn_for_tests(
            id,
            vec![
                ("search".to_string(), schema.clone()),
                ("get".to_string(), schema),
            ],
        )
        .await;
}

/// `initialize` + `notifications/initialized`, returning the session id every
/// later `/mcp` POST needs.
async fn mcp_session(base: &Gw, token: Option<&str>) -> String {
    let mut req = base
        .client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } }
        }));
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    sid
}

async fn mcp_post(base: &Gw, sid: &str, token: Option<&str>, body: Value) -> Value {
    let mut req = base
        .client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", sid)
        .json(&body);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("/mcp is not JSON ({e}): {text}"))
}

fn tool_names(body: &Value) -> Vec<String> {
    body["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools array: {body}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Wait until `job_id` has left `running`, or give up with what it was doing.
pub(crate) async fn settled(base: &Gw, job_id: i64) -> Value {
    for _ in 0..200 {
        let v = get_json(base, &format!("/api/agents/runs/{job_id}")).await;
        let status = v["job"]["status"].as_str().unwrap_or_default().to_string();
        if status != "running" && status != "queued" {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {job_id} never settled");
}

// ---------------------------------------------------------------------------
// The token (§3.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_is_minted_on_demand_scoped_to_any_model_and_rotates_both_columns() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;

    // Never at import: an agent that is never run never mints a credential.
    let detail = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(detail["token"]["has_value"], json!(false));
    assert_eq!(detail["token"]["name"], json!("agent:labeler"));

    let (status, got) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{got}");
    let token = got["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("lmgw-agent-"), "{token}");

    // The shipped mail labeler's shape: a `model_alias` field with no default
    // and a templated `model.alias`. An allow-list derived from that would be
    // empty, and an empty allow-list refuses every call the agent makes.
    assert_eq!(got["scope_mode"], json!("all"));
    assert_eq!(got["scope_note"], json!("token: any model"));

    // A read, not a reveal-once: asking twice gives the same value.
    let (_, again) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    assert_eq!(again["token"], got["token"]);
    assert_eq!(
        get_json(&base, "/api/agents/labeler").await["token"]["has_value"],
        json!(true)
    );

    // The plaintext is really in the column, and the hash is still the lookup
    // key — the two are written together, so a rotation cannot leave them
    // disagreeing.
    let (id, plain) = store::agent_key(&state.db, "labeler")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plain, token);
    let snap = state.snapshot();
    let row = snap.api_keys.iter().find(|k| k.id == id).unwrap();
    assert_eq!(row.key_hash, lmgw_core::config::hash_api_key(&token));
    assert_eq!(row.kind, lmgw_core::config::ApiKeyKind::Agent);
    assert_eq!(row.agent_id.as_deref(), Some("labeler"));
    // Never serialized out of the process, whatever serializes an `ApiKey`.
    assert!(
        !serde_json::to_string(row).unwrap().contains(&token),
        "key_plain must not serialize"
    );

    let (status, rotated) = op(&base, "agent_token_rotate", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{rotated}");
    let fresh = rotated["token"].as_str().unwrap().to_string();
    assert_ne!(fresh, token, "rotation mints a new value");
    let (id2, plain2) = store::agent_key(&state.db, "labeler")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(id2, id, "the same row, so its usage history stays attached");
    assert_eq!(plain2, fresh);
    let snap = state.snapshot();
    assert!(
        snap.verify_api_key(&token).is_none(),
        "the old token stopped working"
    );
    assert!(snap.verify_api_key(&fresh).is_some());

    // A rotation whose reload fails (review W4-21): the new value is laid
    // over the published snapshot all the same, so the old one is refused
    // at once, and the failure is said beside the new token.
    sqlx::query("ALTER TABLE prices RENAME TO prices_away")
        .execute(&state.db)
        .await
        .unwrap();
    let (status, again) = op(&base, "agent_token_rotate", json!({ "id": "labeler" })).await;
    sqlx::query("ALTER TABLE prices_away RENAME TO prices")
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(status, 200, "{again}");
    let newest = again["token"].as_str().unwrap().to_string();
    let snap = state.snapshot();
    assert!(snap.verify_api_key(&fresh).is_none(), "{again}");
    assert!(snap.verify_api_key(&newest).is_some(), "{again}");
    assert!(
        again.to_string().contains("could not be rewritten"),
        "{again}"
    );
}

/// `POST /api/op/<name>` with an explicit `Origin`.
async fn op_from(base: &Gw, name: &str, origin: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .header("origin", origin)
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
    )
}

/// The two token ops used to carry an `Origin` check of their own
/// (`CREDENTIAL_OPS` in `web/api.rs`), because `/api` had no gate and
/// permissive CORS let a foreign page `fetch` a bearer out of them. Both
/// halves are gone: the plane needs `Admin`, and a cookie-authenticated
/// request is covered by the same-origin rule for *every* route (principals
/// §3.6, asserted in `tests/it/principal_gate.rs`). What is asserted here is that
/// nothing special is left — a credential-less caller is refused like any
/// other `/api` caller, and an authenticated one is answered whatever `Origin`
/// it sends.
#[tokio::test]
async fn the_token_ops_are_ordinary_admin_ops_with_no_rule_of_their_own() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;

    for name in ["agent_token_get", "agent_token_rotate"] {
        let resp = base
            .anon()
            .post(format!("{base}/api/op/{name}"))
            .json(&json!({ "id": "labeler" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "{name}");
        let res: Value = resp.json().await.unwrap();
        assert_eq!(res["code"], json!("session_required"), "{name}: {res}");
        assert!(
            !res.to_string().contains("lmgw-agent-"),
            "{name}: the refusal must not carry the thing it refused: {res}"
        );
    }
    // And nothing was minted by the attempt.
    assert!(store::agent_key(&state.db, "labeler")
        .await
        .unwrap()
        .is_none());

    // A bearer skips the same-origin rule, whatever `Origin` rides along: a
    // page that holds a bearer did not get it from the browser.
    let (status, got) = op_from(
        &base,
        "agent_token_get",
        "https://evil.example",
        json!({ "id": "labeler" }),
    )
    .await;
    assert_eq!(status, 200, "{got}");
    assert!(got["token"].as_str().unwrap().starts_with("lmgw-agent-"));
}

#[tokio::test]
async fn the_scope_follows_the_model_field_and_is_recomputed_on_save() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    op(&base, "agent_token_get", json!({ "id": "labeler" })).await;

    let (status, saved) = op(
        &base,
        "agent_config_set",
        json!({ "id": "labeler", "values": { "model": "qwen3.8" } }),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["token_scope"], json!("token: qwen3.8"));

    let snap = state.snapshot();
    let key = snap
        .api_keys
        .iter()
        .find(|k| k.agent_id.as_deref() == Some("labeler"))
        .unwrap();
    assert_eq!(key.policy.scope_mode, lmgw_core::config::ScopeMode::Allow);
    assert_eq!(key.policy.scope_patterns, "qwen3.8");
    assert!(key.policy.admits("qwen3.8"));
    assert!(!key.policy.admits("claude-opus-5"), "fenced to its picker");

    // Clearing it goes back to "any model" rather than to an allow-list of
    // nothing, which would refuse every call the agent makes.
    op(
        &base,
        "agent_config_set",
        json!({ "id": "labeler", "values": {}, "clear": ["model"] }),
    )
    .await;
    let snap = state.snapshot();
    let key = snap
        .api_keys
        .iter()
        .find(|k| k.agent_id.as_deref() == Some("labeler"))
        .unwrap();
    assert_eq!(key.policy.scope_mode, lmgw_core::config::ScopeMode::All);
}

#[tokio::test]
async fn deleting_the_agent_deletes_its_credential() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, got) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = got["token"].as_str().unwrap().to_string();

    op(&base, "agent_delete", json!({ "id": "labeler" })).await;
    assert!(store::agent_key(&state.db, "labeler")
        .await
        .unwrap()
        .is_none());
    assert!(
        state.snapshot().verify_api_key(&token).is_none(),
        "a deleted agent's token authenticates nothing"
    );
}

// ---------------------------------------------------------------------------
// /mcp filters by the token (§3.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_auth_off_an_agent_token_sees_exactly_its_allow_list() {
    let state = AppState::init_for_tests().await.unwrap();
    assert!(
        !state.snapshot().settings.auth_enabled,
        "the shipped default, and the configuration the token has to work in"
    );
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    seed_tools(&state).await;
    let (_, got) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = got["token"].as_str().unwrap().to_string();

    // Without the token: the whole aggregate, exactly as before.
    let sid = mcp_session(&base, None).await;
    let open = tool_names(
        &mcp_post(
            &base,
            &sid,
            None,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await,
    );
    assert!(open.contains(&"gws__search".to_string()));
    assert!(open.contains(&"gws__get".to_string()));
    assert!(
        open.iter().any(|n| n.starts_with("docs__")),
        "the built-in docs toolset is on the aggregate: {open:?}"
    );

    // With it: the manifest's allow list and nothing else.
    let sid = mcp_session(&base, Some(&token)).await;
    let mine = tool_names(
        &mcp_post(
            &base,
            &sid,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await,
    );
    assert_eq!(mine, vec!["gws__search".to_string()]);

    // …and a name off the list is refused when called, because a list is a
    // snapshot and a call is not.
    let refused = mcp_post(
        &base,
        &sid,
        Some(&token),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "gws__get", "arguments": {} } }),
    )
    .await;
    let text = refused.to_string();
    assert!(
        text.contains("may call") && text.contains("gws__search"),
        "the refusal names what it may call instead: {text}"
    );
    assert!(
        !text.contains("\"result\""),
        "gws__get must not have been routed: {text}"
    );
}

/// The same allow list binds a `/v1/responses` run. Before the key tool scope
/// it did not: a container could attach any registered label there and get
/// every tool the manifest had left out.
#[tokio::test]
async fn an_agent_tokens_responses_run_gets_only_its_allow_list() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "tgt-model",
            "choices": [{ "index": 0, "finish_reason": "stop",
                          "message": { "role": "assistant", "content": "ok" } }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 }
        })))
        .mount(&upstream)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream.uri().trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m1".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    seed_tools(&state).await;
    let (_, got) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = got["token"].as_str().unwrap().to_string();

    let (status, resp) = post_as(
        &base,
        "/v1/responses",
        Some(&token),
        json!({ "model": "m1", "input": "hi",
                "tools": [ { "type": "mcp", "server_label": "gws",
                             "require_approval": "never" } ] }),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    let item = &resp["output"][0];
    assert_eq!(item["type"], "mcp_list_tools", "{resp}");
    let listed: Vec<&str> = item["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(listed, vec!["gws__search"], "{resp}");
}

// ---------------------------------------------------------------------------
// The ledger (§3.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ledger_routes_carry_their_own_bearer_check() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    install(&base, &doc("other", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let mine = a["token"].as_str().unwrap().to_string();
    let (_, b) = op(&base, "agent_token_get", json!({ "id": "other" })).await;
    let theirs = b["token"].as_str().unwrap().to_string();

    // No bearer: 401, with the code a container branches on.
    let (status, body) = post_as(&base, "/api/agents/labeler/runs", None, json!({})).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("session_required"));

    // A gateway key is not an agent token either.
    let (status, _) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some("lmgw-not-a-token"),
        json!({}),
    )
    .await;
    assert_eq!(status, 401);

    // Another agent's token cannot open this agent's run.
    let (status, body) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&theirs),
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], json!("run_not_owned"));

    // The owning token opens it.
    let (status, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&mine),
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{opened}");
    let run = opened["run"].as_i64().unwrap();
    assert_eq!(
        opened["deadline_seconds"],
        json!(0),
        "printed back, never a bound the caller has to guess"
    );

    // …and it is the only token that may write to it.
    for (token, expect, code) in [
        (None, 401, "session_required"),
        (Some(theirs.as_str()), 403, "run_not_owned"),
    ] {
        let (status, body) = post_as(
            &base,
            &format!("/api/agents/runs/{run}/events"),
            token,
            json!({ "type": "log", "message": "hello" }),
        )
        .await;
        assert_eq!(status, expect, "{body}");
        assert_eq!(body["code"], json!(code));
    }

    let (status, body) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&theirs),
        json!({ "status": "done" }),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], json!("run_not_owned"));

    post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&mine),
        json!({ "status": "done" }),
    )
    .await;
}

#[tokio::test]
async fn a_hand_posted_event_sequence_becomes_the_runs_review_table() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();

    let (status, applied) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/events"),
        Some(&token),
        json!([
            { "type": "log", "message": "listing" },
            { "type": "row", "id": "m1", "columns": { "subject": "Invoice" } },
            { "type": "row", "id": "m2", "columns": { "subject": "Hello", "spam": 0.1 } },
            { "type": "row", "id": "m1", "output": { "category": "Finance" } },
            { "type": "row", "columns": { "subject": "no id" } },
            { "type": "banner", "message": "a library printed this" },
            { "type": "progress", "done": 2, "total": 2, "stage": "classified" }
        ]),
    )
    .await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(
        applied["applied"],
        json!(5),
        "two rejected: the id-less row and the unknown type — {applied}"
    );

    // Live, before the close: the Run tab reads the same buffer an in-process
    // run publishes to, so a hand-posted run renders identically.
    let live = get_json(&base, &format!("/api/agents/runs/{run}")).await;
    let rows = live["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "upserted by id: {rows:?}");
    assert_eq!(rows[0]["id"], json!("m1"));
    assert_eq!(rows[0]["columns"]["subject"], json!("Invoice"));
    assert_eq!(
        rows[0]["output"]["category"],
        json!("Finance"),
        "a second row event adds to the row the first one made"
    );
    assert_eq!(
        rows[1]["columns"]["spam"],
        json!(0.1),
        "an undeclared column is kept, never dropped"
    );
    let columns: Vec<String> = live["batch"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(
        columns,
        vec!["subject".to_string(), "spam".to_string()],
        "the undeclared key is appended after the declared ones, in first-seen order"
    );
    let log = live["log"].to_string();
    assert!(log.contains("listing"), "{log}");
    assert!(log.contains("no id"), "the rejected row is named: {log}");
    assert!(log.contains("'banner'"), "the unknown type is named: {log}");

    post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&token),
        json!({ "status": "done", "output": { "applied": 2 } }),
    )
    .await;

    let done = settled(&base, run).await;
    assert_eq!(done["job"]["status"], json!("done"));
    assert_eq!(done["job"]["phase"], json!("run"));
    assert_eq!(done["result"]["output"]["applied"], json!(2));
    assert_eq!(
        done["rows"].as_array().unwrap().len(),
        2,
        "kept in `result`"
    );
    assert_eq!(
        done["result"]["cost_micro"],
        Value::Null,
        "nobody priced anything: NULL, not zero"
    );
}

#[tokio::test]
async fn cancel_refuses_every_later_event_and_keeps_the_rows() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();
    post_as(
        &base,
        &format!("/api/agents/runs/{run}/events"),
        Some(&token),
        json!({ "type": "row", "id": "m1", "columns": { "subject": "kept" } }),
    )
    .await;

    let (status, canceled) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{canceled}");

    // How a container lmgw cannot signal finds out.
    let (status, body) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/events"),
        Some(&token),
        json!({ "type": "row", "id": "m2" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], json!("run_cancelled"));
    let (status, body) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&token),
        json!({ "status": "done" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], json!("run_cancelled"));

    let ended = settled(&base, run).await;
    assert_eq!(ended["job"]["status"], json!("canceled"));
    assert_eq!(
        ended["rows"].as_array().unwrap().len(),
        1,
        "Cancel must not be destructive"
    );
}

#[tokio::test]
async fn a_run_that_never_closes_ends_failed_with_no_close() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 1)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();
    assert_eq!(opened["deadline_seconds"], json!(1));
    post_as(
        &base,
        &format!("/api/agents/runs/{run}/events"),
        Some(&token),
        json!({ "type": "row", "id": "m1" }),
    )
    .await;

    let ended = settled(&base, run).await;
    assert_eq!(ended["job"]["status"], json!("failed"));
    let error = ended["job"]["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("no_close:"), "{error}");
    assert!(error.contains("deadline of 1s"), "{error}");
    assert_eq!(
        ended["rows"].as_array().unwrap().len(),
        1,
        "what it did report is still reported"
    );

    // The (kind, key) guard is released, so the agent is runnable again.
    let (status, again) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{again}");
    assert_ne!(
        again["run"].as_i64(),
        Some(run),
        "a new run, not the old one"
    );
}

// ---------------------------------------------------------------------------
// X-Lmgw-Run (§3.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn x_lmgw_run_puts_a_containers_own_call_on_the_runs_meter() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "tgt-model",
            "choices": [{ "index": 0, "finish_reason": "stop",
                          "message": { "role": "assistant", "content": "ok" } }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18 }
        })))
        .mount(&upstream)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream.uri().trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m1".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();
    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();

    // A second agent, for the foreign-token case.
    install(&base, &doc("other", 0)).await;
    let (_, b) = op(&base, "agent_token_get", json!({ "id": "other" })).await;
    let foreign = b["token"].as_str().unwrap().to_string();

    let chat = |bearer: Option<String>, run_header: String| {
        let base = base.clone();
        async move {
            let mut req = base
                .client()
                .post(format!("{base}/v1/chat/completions"))
                .header("x-lmgw-run", run_header)
                .json(&json!({ "model": "m1",
                               "messages": [{ "role": "user", "content": "hi" }] }));
            if let Some(t) = bearer {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            req.send().await.unwrap().status().as_u16()
        }
    };

    // The header moves money, so only an agent may stamp, and only its own
    // runs. With auth off — the shipped default — *anything* on the box can
    // reach `/v1`, and a forged `x-lmgw-run: 1` would otherwise bill a run the
    // caller has nothing to do with.
    assert_eq!(chat(None, run.to_string()).await, 200, "still served");
    assert_eq!(
        chat(Some(foreign.clone()), run.to_string()).await,
        200,
        "another agent's token is served too — it just cannot attribute"
    );

    // A foreign / unknown run id is ignored with a log line, never a refusal:
    // a mis-stamped request is still a request the owner made.
    assert_eq!(chat(Some(token.clone()), "999999".into()).await, 200);
    assert_eq!(chat(Some(token.clone()), "not-a-number".into()).await, 200);

    // Only this one is the run's own.
    assert_eq!(chat(Some(token.clone()), run.to_string()).await, 200);

    post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&token),
        json!({ "status": "done" }),
    )
    .await;
    let done = settled(&base, run).await;
    assert_eq!(
        done["result"]["model_calls"],
        json!(1),
        "exactly the one call that named this run and could: {}",
        done["result"]
    );
    assert_eq!(done["result"]["usage"]["prompt_tokens"], json!(11));
    assert_eq!(done["result"]["usage"]["completion_tokens"], json!(7));
    assert_eq!(
        done["job"]["tokens"],
        json!(18),
        "and the Runs tab reads it the same way it reads an in-process run"
    );
}

#[tokio::test]
async fn the_token_authenticates_with_auth_on_too_and_a_stranger_still_does_not() {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    let models = |bearer: Option<String>| {
        let base = base.clone();
        async move {
            let mut req = base.anon().get(format!("{base}/v1/models"));
            if let Some(b) = bearer {
                req = req.header("authorization", format!("Bearer {b}"));
            }
            req.send().await.unwrap().status().as_u16()
        }
    };

    assert_eq!(models(None).await, 401, "auth is on");
    assert_eq!(models(Some("lmgw-nonsense".into())).await, 401);
    assert_eq!(
        models(Some(token)).await,
        200,
        "the agent token is a real credential on the gateway too"
    );
}

/// An unknown alias never reaches an upstream, so it is not a model call — the
/// same thing the in-process `batch::Meter` counts. Without this a run that
/// mistyped its alias forty times would report forty model calls that cost
/// nothing, and `tool_calls` would count names the allow list refused.
#[tokio::test]
async fn a_refused_call_is_not_a_model_call() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    seed_tools(&state).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();
    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();

    // No alias `nope` exists, so nothing was routed anywhere.
    let status = base
        .client()
        .post(format!("{base}/v1/chat/completions"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-lmgw-run", run.to_string())
        .json(&json!({ "model": "nope", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 404, "unknown alias");

    // A name off the allow list never reached a tool either.
    let sid = mcp_session(&base, Some(&token)).await;
    mcp_post(
        &base,
        &sid,
        Some(&token),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "gws__get", "arguments": {} } }),
    )
    .await;

    post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&token),
        json!({ "status": "done" }),
    )
    .await;
    let done = settled(&base, run).await;
    assert_eq!(
        done["result"]["model_calls"],
        json!(0),
        "{}",
        done["result"]
    );
    assert_eq!(done["result"]["tool_calls"], json!(0), "{}", done["result"]);
    assert_eq!(done["result"]["cost_micro"], Value::Null);
}

/// An events body over axum's silent 2 MiB `DefaultBodyLimit` is a perfectly
/// ordinary NDJSON flush, and a plain-text 413 nobody chose would be exactly
/// the kind of invisible ceiling this codebase refuses to ship.
#[tokio::test]
async fn a_three_megabyte_event_body_is_not_refused_by_a_cap_nobody_chose() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();
    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();

    // ~3 MiB of NDJSON: 3000 rows carrying a kilobyte of subject each.
    let filler = "x".repeat(1024);
    let body: String = (0..3000)
        .map(|i| {
            json!({ "type": "row", "id": format!("m{i}"),
                    "columns": { "subject": filler } })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.len() > 3 * 1024 * 1024, "{} bytes", body.len());

    let resp = base
        .client()
        .post(format!("{base}/api/agents/runs/{run}/events"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "a 413 here is a hidden cap");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["applied"], json!(3000));
    let live = get_json(&base, &format!("/api/agents/runs/{run}")).await;
    assert_eq!(live["rows"].as_array().unwrap().len(), 3000);
}

/// The desk forgets a run the moment it ends, so "closed" and "never existed"
/// would be the same 404 depending on timing. They are answered from the
/// durable job row instead — and a foreign token is told nothing either way.
#[tokio::test]
async fn a_second_close_is_409_run_closed_and_a_foreign_token_still_gets_403() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    install(&base, &doc("other", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let mine = a["token"].as_str().unwrap().to_string();
    let (_, b) = op(&base, "agent_token_get", json!({ "id": "other" })).await;
    let theirs = b["token"].as_str().unwrap().to_string();

    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&mine),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();
    let close_path = format!("/api/agents/runs/{run}/close");
    let close = || post_as(&base, &close_path, Some(&mine), json!({ "status": "done" }));
    assert_eq!(close().await.0, 200);
    settled(&base, run).await;

    // Deterministic, not a race with `Desk::forget`.
    let (status, body) = close().await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], json!("run_closed"));
    let (status, body) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/events"),
        Some(&mine),
        json!({ "type": "log", "message": "late" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], json!("run_closed"));

    // Ownership is answered before existence: a foreign token learns nothing
    // about the run, finished or not.
    let (status, body) = post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&theirs),
        json!({ "status": "done" }),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], json!("run_not_owned"));

    // An id nothing knows is nobody's, so nobody is foreign to it.
    let (status, body) = post_as(
        &base,
        "/api/agents/runs/424242/close",
        Some(&mine),
        json!({ "status": "done" }),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], json!("not_found"));
}

/// Disable is the kill switch (§3.1): the token stops working everywhere, and
/// the refusal says which of the two it is — one is fixed by rotating a
/// credential and the other never is.
#[tokio::test]
async fn disabling_an_agent_revokes_its_token_everywhere() {
    let state = AppState::init_for_tests().await.unwrap();
    assert!(
        !state.snapshot().settings.auth_enabled,
        "the shipped default"
    );
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    seed_tools(&state).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    // It works first, so the refusal below is the switch and not the setup.
    let sid = mcp_session(&base, Some(&token)).await;
    assert_eq!(
        tool_names(
            &mcp_post(
                &base,
                &sid,
                Some(&token),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            )
            .await
        ),
        vec!["gws__search".to_string()]
    );

    let (status, body) = op(
        &base,
        "agent_enable",
        json!({ "id": "labeler", "enabled": false }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // /v1 — the named 401, not a bare "invalid key".
    let resp = base
        .client()
        .get(format!("{base}/v1/models"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], json!("agent_disabled"), "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("labeler"),
        "{v}"
    );

    // The ledger routes, which do their own check.
    let (status, body) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("agent_disabled"));

    // …and back on again, with the same token: Disable is a switch, not a
    // revocation that costs the owner a rotation.
    op(
        &base,
        "agent_enable",
        json!({ "id": "labeler", "enabled": true }),
    )
    .await;
    let resp = base
        .client()
        .get(format!("{base}/v1/models"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// "One live run per agent" is a refusal, not a redirect: the job the index
/// hands back may be an in-process run, or a ledger run opened elsewhere whose
/// deadline started at a different moment.
#[tokio::test]
async fn a_second_open_is_409_already_running_and_names_the_live_run() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();
    let open = || {
        post_as(
            &base,
            "/api/agents/labeler/runs",
            Some(&token),
            json!({ "phase": "run" }),
        )
    };

    // Two at once collapse to one run, because the `(kind, key)` index is the
    // thing enforcing it — not a check-then-act in the handler.
    let (first, second) = tokio::join!(open(), open());
    let mut codes = [first.0, second.0];
    codes.sort_unstable();
    assert_eq!(codes, [200, 409], "{first:?} {second:?}");
    let (winner, loser) = if first.0 == 200 {
        (first.1, second.1)
    } else {
        (second.1, first.1)
    };
    let run = winner["run"].as_i64().unwrap();
    assert_eq!(loser["code"], json!("already_running"));
    assert_eq!(
        loser["run"].as_i64(),
        Some(run),
        "the refusal names the run that won, so the caller can watch it"
    );

    // A third, sequentially, gets the same answer.
    let (status, body) = open().await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["run"].as_i64(), Some(run));

    post_as(
        &base,
        &format!("/api/agents/runs/{run}/close"),
        Some(&token),
        json!({ "status": "done" }),
    )
    .await;
}

/// A `tools[]` entry with no `allowed` takes the label's whole current surface
/// — the branch a manifest that says "this server, all of it" relies on.
#[tokio::test]
async fn a_tool_entry_without_allowed_takes_the_whole_label() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let whole = doc("labeler", 0).replace(
        r#"{ "label": "gws", "allowed": ["gws__search"] }"#,
        r#"{ "label": "gws" }"#,
    );
    install(&base, &whole).await;
    seed_tools(&state).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    let sid = mcp_session(&base, Some(&token)).await;
    let mut names = tool_names(
        &mcp_post(
            &base,
            &sid,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await,
    );
    names.sort();
    assert_eq!(
        names,
        vec!["gws__get".to_string(), "gws__search".to_string()],
        "the whole label, and still nothing from any other one"
    );
}

/// `/mcp/admin` needs an owner credential, and a manifest's `tools[]` is not
/// one. Presenting an agent token there changes nothing, in either direction.
#[tokio::test]
async fn an_agent_token_does_not_change_the_admin_plane() {
    let state = AppState::init_for_tests().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        "admin-secret",
        true,
    )
    .await
    .unwrap();

    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();

    // The agent token is not the admin token, and does not become one.
    let resp = base
        .client()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                                   "clientInfo": { "name": "t", "version": "0" } } }))
        .send()
        .await
        .unwrap();
    // 403, not 401: the gate resolved the token, found an agent principal,
    // and refuses by capability — naming both halves (principals §3.9).
    assert_eq!(resp.status().as_u16(), 403, "the admin gate still holds");

    // Now hold *both*: the admin token as the bearer the gate reads, and the
    // agent token as `x-api-key`, which the root resolver accepts — so
    // `ctx.agent` is set on a request that passes the admin gate. The plane
    // behaves exactly as it does for anyone else: the full self-admin catalog,
    // not the agent's one-tool allow list.
    let client = base.client();
    let admin = |body: Value, sid: Option<&str>| {
        let mut req = client
            .post(format!("{base}/mcp/admin"))
            .header("accept", "application/json, text/event-stream")
            .header("authorization", "Bearer admin-secret")
            .header("x-api-key", token.clone())
            .json(&body);
        if let Some(s) = sid {
            req = req.header("mcp-session-id", s);
        }
        req.send()
    };
    let resp = admin(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                            "clientInfo": { "name": "t", "version": "0" } } }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the admin token opens it");
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    let body: Value = serde_json::from_str(
        &admin(
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            Some(&sid),
        )
        .await
        .unwrap()
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    let names = tool_names(&body);
    assert!(names.len() > 1, "the whole catalog: {names:?}");
    assert!(names.iter().all(|n| n.starts_with("lmgw__")), "{names:?}");

    // And on `/mcp`, where the allow list does apply, a reserved name still
    // gets the sentence that says where the tools went.
    let sid = mcp_session(&base, Some(&token)).await;
    let refused = mcp_post(
        &base,
        &sid,
        Some(&token),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "lmgw__status", "arguments": {} } }),
    )
    .await;
    assert!(
        refused.to_string().contains("/mcp/admin"),
        "the hint survives for an agent caller: {refused}"
    );
}

/// A body that is not UTF-8 is refused by name. `from_utf8_lossy` would rewrite
/// a row id into something the sender never wrote, with no way to notice.
#[tokio::test]
async fn a_non_utf8_event_body_is_named_not_mangled() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &doc("labeler", 0)).await;
    let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = a["token"].as_str().unwrap().to_string();
    let (_, opened) = post_as(
        &base,
        "/api/agents/labeler/runs",
        Some(&token),
        json!({ "phase": "run" }),
    )
    .await;
    let run = opened["run"].as_i64().unwrap();

    let mut body = br#"{"type":"row","id":"m"#.to_vec();
    body.extend_from_slice(&[0xff, 0xfe]);
    body.extend_from_slice(br#""}"#);
    let resp = base
        .client()
        .post(format!("{base}/api/agents/runs/{run}/events"))
        .header("authorization", format!("Bearer {token}"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], json!("invalid_utf8"));
    let live = get_json(&base, &format!("/api/agents/runs/{run}")).await;
    assert!(
        live["rows"].as_array().unwrap().is_empty(),
        "nothing landed"
    );
}
