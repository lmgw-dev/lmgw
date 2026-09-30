//! The `/mcp` ↔ `/mcp/admin` split (§21 stage 2).
//!
//! `mcp_selfadmin.rs` covers what the self-admin tools *do*. This covers where
//! they are reachable from, which is the security-relevant half: the aggregate
//! route must not serve them, and the admin route admits an **enabled owner
//! key** and nothing else (principals §3.7). Both are easy to regress by
//! "helpfully" merging the planes back together, so they are pinned here.

use lmgw_core::agents::token::{OWNER_DASHBOARD, OWNER_SELF_ADMIN};
use lmgw_core::config::{SelfAdmin, Settings};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

async fn serve(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A gateway at `SelfAdmin::Full` holding an `owner:self-admin` key whose
/// plaintext is `token`, and an `owner:dashboard` key beside it.
///
/// `enabled` is the switch that used to be "is the Settings string empty":
/// `false` is a gateway whose self-admin plane is closed.
async fn gateway_with(token: &str, enabled: bool) -> String {
    let state = AppState::init_for_tests().await.unwrap();
    let settings = Settings {
        self_admin: SelfAdmin::Full,
        ..Settings::default()
    };
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    owner_key(&state, OWNER_SELF_ADMIN, token, enabled).await;
    owner_key(&state, OWNER_DASHBOARD, "lmgw-owner-dash", true).await;
    serve(state).await
}

async fn gateway(token: &str) -> String {
    gateway_with(token, true).await
}

/// Install one owner key with a known plaintext — the seam every suite that
/// needs an `Admin` credential goes through.
async fn owner_key(state: &SharedState, name: &str, plaintext: &str, enabled: bool) {
    lmgw_core::agents::token::set_owner_key(state, name, plaintext, enabled)
        .await
        .unwrap();
}

async fn post(
    base: &str,
    route: &str,
    token: Option<&str>,
    session: Option<&str>,
    body: Value,
) -> (u16, Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}{route}"))
        .header("accept", "application/json")
        .json(&body);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        sid,
        serde_json::from_str(&text).unwrap_or(Value::Null),
    )
}

fn init_body() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "split-test", "version": "0"}}
    })
}

async fn tool_names(base: &str, route: &str, token: Option<&str>) -> Vec<String> {
    let (status, sid, body) = post(base, route, token, None, init_body()).await;
    assert_eq!(status, 200, "initialize on {route}: {body}");
    let sid = sid.expect("session id");
    let (_, _, body) = post(
        base,
        route,
        token,
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Even at `self_admin: full`, the aggregate route serves no `lmgw__*` tool.
/// This is the point of the split: an agent handed a gateway key gets the MCP
/// servers, not the gateway's configuration API.
#[tokio::test]
async fn the_aggregate_route_does_not_serve_self_admin_tools() {
    let base = gateway("tok").await;
    let names = tool_names(&base, "/mcp", None).await;
    assert!(
        !names.iter().any(|n| n.starts_with("lmgw__")),
        "self-admin tools leaked onto /mcp: {names:?}"
    );
}

/// And calling one there says where it went, rather than "unknown tool" — the
/// clients that break on this upgrade are the ones that had it configured.
#[tokio::test]
async fn calling_a_self_admin_tool_on_the_aggregate_route_explains_the_move() {
    let base = gateway("tok").await;
    let (_, sid, _) = post(&base, "/mcp", None, None, init_body()).await;
    let (status, _, body) = post(
        &base,
        "/mcp",
        None,
        sid.as_deref(),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "lmgw__status", "arguments": {}}}),
    )
    .await;
    assert_eq!(status, 200);
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("/mcp/admin"), "{body}");
    assert!(msg.contains("owner:self-admin"), "{body}");
}

/// A disabled `owner:self-admin` is what "the plane is closed" means now, and
/// the refusal **names the row** rather than answering `404` — a closed
/// surface and a locked one were two words for one thing (principals §3.7).
#[tokio::test]
async fn a_disabled_self_admin_key_refuses_and_says_which_row_it_is() {
    let base = gateway_with("closed-key", false).await;

    let (status, _, body) = post(&base, "/mcp/admin", Some("closed-key"), None, init_body()).await;
    assert_eq!(status, 401);
    assert_eq!(body["code"], "owner_key_disabled", "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("self-admin"),
        "{body}"
    );

    // Nothing presented at all is the other refusal: there is no row to name,
    // so it points at the login link instead.
    let (status, _, body) = post(&base, "/mcp/admin", None, None, init_body()).await;
    assert_eq!(status, 401);
    assert_eq!(body["code"], "session_required", "{body}");
}

#[tokio::test]
async fn the_admin_route_rejects_a_credential_it_does_not_know() {
    let base = gateway("right").await;
    let (status, _, body) = post(&base, "/mcp/admin", Some("wrong"), None, init_body()).await;
    assert_eq!(status, 401);
    assert_eq!(
        body["code"], "session_required",
        "a bearer matching no row is the anonymous principal (§3.3): {body}"
    );
    let (status, _, _) = post(&base, "/mcp/admin", None, None, init_body()).await;
    assert_eq!(status, 401, "no credential at all is the same refusal");
}

/// Every enabled owner row holds every capability (§3.1) — the rows differ
/// only in which one leaked, so the dashboard key opens this plane too.
#[tokio::test]
async fn any_enabled_owner_key_opens_the_admin_route() {
    let base = gateway("right").await;
    for key in ["right", "lmgw-owner-dash"] {
        let names = tool_names(&base, "/mcp/admin", Some(key)).await;
        assert!(
            names.iter().any(|n| n == "lmgw__status"),
            "{key}: {names:?}"
        );
    }
}

/// `x-lmgw-admin-token` is the third spelling, honoured **here only**, so the
/// MCP client configs that use it today keep working without an edit (§3.3).
#[tokio::test]
async fn the_admin_token_header_is_honoured_here_and_nowhere_else() {
    let base = gateway("right").await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json")
        .header("x-lmgw-admin-token", "right")
        .json(&init_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // On `/mcp` it is not a credential; with auth off that route is open to
    // the anonymous principal anyway, so the assertion that matters is that
    // the aggregate plane still serves no self-admin tool to it.
    let names = tool_names(&base, "/mcp", None).await;
    assert!(!names.iter().any(|n| n.starts_with("lmgw__")), "{names:?}");

    // And on the dashboard plane it is nothing at all: the header is read by
    // *that route's* `require` layer, not by `token::presented`, so it widens
    // exactly one route and no other.
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/status"))
        .header("x-lmgw-admin-token", "right")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "session_required", "{body}");
}

/// One `initialize` + one `tools/call` on `route`, with `extra` headers on
/// both — for the cases that are about what a request *carries*, which the
/// two-argument [`post`] cannot express.
async fn call_with(base: &str, route: &str, extra: &[(&str, String)], name: &str) -> Value {
    let send = |body: Value, sid: Option<String>| async move {
        let mut req = reqwest::Client::new()
            .post(format!("{base}{route}"))
            .header("accept", "application/json")
            .json(&body);
        for (header, value) in extra {
            req = req.header(*header, value);
        }
        if let Some(sid) = sid {
            req = req.header("mcp-session-id", sid);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let text = resp.text().await.unwrap_or_default();
        (
            status,
            sid,
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
        )
    };

    let (status, sid, body) = send(init_body(), None).await;
    assert_eq!(status, 200, "initialize on {route}: {body}");
    let (status, _, body) = send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": name, "arguments": {}}}),
        Some(sid.expect("session id")),
    )
    .await;
    assert_eq!(status, 200, "{name} on {route}: {body}");
    body
}

/// A batch agent that may call the `docs` toolset — something it can really
/// complete on `/mcp`, so the metering below has a positive half.
const LABELER: &str = r#"{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": { "alias": "m1" },
  "tools": [{ "label": "docs" }],
  "run": { "kind": "batch",
    "source": { "tool": "gws__search" },
    "item": { "id": "{{item.id}}", "columns": { "subject": "{{item.subject}}" } },
    "limits": { "deadline_seconds": 0 } }
}"#;

/// An agent bearer **plus** a valid owner `x-lmgw-admin-token` is the owner's
/// request, and the whole ctx has to say so (principals §3.3, §3.5).
///
/// The header replaces the principal; the agent identity and the run that the
/// bearer filled in at the router root do not survive it. Left standing, an
/// Admin-plane call is attributed to that agent and **metered against its
/// run** — the owner's configuration traffic would land on a container's
/// budget line and be shown in the Runs tab as that agent's work.
#[tokio::test]
async fn the_admin_token_replaces_the_agent_identity_it_arrived_with() {
    let state = AppState::init_for_tests().await.unwrap();
    let settings = Settings {
        self_admin: SelfAdmin::Full,
        ..Settings::default()
    };
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    owner_key(&state, OWNER_DASHBOARD, "lmgw-owner-dash", true).await;
    let base = serve(state.clone()).await;

    // As the owner: install the agent and mint its token.
    let owner = |path: String, body: Value| async move {
        reqwest::Client::new()
            .post(path)
            .header("authorization", "Bearer lmgw-owner-dash")
            .json(&body)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap_or(Value::Null)
    };
    let saved = owner(
        format!("{base}/api/op/agent_set"),
        json!({ "manifest": LABELER, "replace": true }),
    )
    .await;
    assert_eq!(saved["ok"], json!(true), "{saved}");
    let minted = owner(
        format!("{base}/api/op/agent_token_get"),
        json!({"id": "labeler"}),
    )
    .await;
    let token = minted["token"]
        .as_str()
        .expect("a minted token")
        .to_string();

    // As the container: one run of its own, which is what `X-Lmgw-Run` names.
    let opened = reqwest::Client::new()
        .post(format!("{base}/api/agents/labeler/runs"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "phase": "run" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let run = opened["run"].as_i64().unwrap_or_else(|| panic!("{opened}"));
    let as_agent = [
        ("authorization", format!("Bearer {token}")),
        ("x-lmgw-run", run.to_string()),
    ];

    // The positive half: on the aggregate plane that bearer and that header
    // are exactly what they claim to be, and the call lands on the run.
    let answered = call_with(&base, "/mcp", &as_agent, "docs__resolve").await;
    assert!(answered["result"].is_object(), "{answered}");
    assert_eq!(
        state.agent_meters.read(run).tool_calls,
        1,
        "the agent's own tool call was not metered against its run"
    );

    // The same container, now also presenting the owner's admin token.
    let as_owner = [
        ("authorization", format!("Bearer {token}")),
        ("x-lmgw-run", run.to_string()),
        ("x-lmgw-admin-token", "lmgw-owner-dash".to_string()),
    ];
    let answered = call_with(&base, "/mcp/admin", &as_owner, "lmgw__status").await;
    assert_eq!(
        answered["result"]["isError"],
        json!(false),
        "the header admits it: {answered}"
    );
    assert_eq!(
        state.agent_meters.read(run).tool_calls,
        1,
        "an Admin-plane call was metered against the agent's run"
    );

    // And the row it wrote names the owner key it was admitted by, not the
    // agent whose bearer happened to be on the same request.
    let rows = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    let row = rows
        .iter()
        .find(|r| r.mcp_tool.as_deref() == Some("lmgw__status"))
        .unwrap_or_else(|| panic!("no log row for the admin call: {rows:#?}"));
    assert_eq!(row.client_key.as_deref(), Some(OWNER_DASHBOARD), "{row:#?}");
}

/// The `GET` and `DELETE` on `/mcp/admin` are the **aggregate** plane's
/// session handlers and keep `Inference` (principals §3.5, §3.7): the split
/// capability is per handler, not per path, and a `require(Admin)` slapped on
/// the whole path would break every client that closes its session.
#[tokio::test]
async fn the_session_handlers_on_the_admin_path_are_not_the_admin_plane() {
    let base = gateway("tok").await;
    let client = reqwest::Client::new();
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let status = client
            .request(method.clone(), format!("{base}/mcp/admin"))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(
            status, 400,
            "{method} with no credential is the session error it always was,              not the `Admin` 401 the POST beside it answers"
        );
    }
}

#[tokio::test]
async fn the_admin_route_serves_only_self_admin_tools() {
    let base = gateway("tok").await;
    let names = tool_names(&base, "/mcp/admin", Some("tok")).await;
    assert!(!names.is_empty());
    assert!(
        names.iter().all(|n| n.starts_with("lmgw__")),
        "the admin plane must not aggregate southbound servers: {names:?}"
    );
    assert!(names.iter().any(|n| n == "lmgw__status"));
}

/// The mode gate still applies on the admin route: an owner key buys
/// reachability, not permission.
#[tokio::test]
async fn the_token_does_not_bypass_the_self_admin_mode() {
    let state = AppState::init_for_tests().await.unwrap();
    let settings = Settings {
        self_admin: SelfAdmin::Off,
        ..Settings::default()
    };
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    owner_key(&state, OWNER_SELF_ADMIN, "tok", true).await;
    let base = serve(state).await;

    let names = tool_names(&base, "/mcp/admin", Some("tok")).await;
    assert!(
        names.is_empty(),
        "self_admin: off must list nothing: {names:?}"
    );
}
