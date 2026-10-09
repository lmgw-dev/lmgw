//! `GET /v1/mcp/servers` and `GET /v1/mcp/servers/{label}`
//! (realtime-server-tools design §1.4): the list connects nothing and follows
//! the caller's tool scope — enabled servers only, `lmgw` for an owner — the
//! detail lists exactly the tools a `/v1/realtime` listing item carries for
//! the same label, an unknown or out-of-scope label is a 404 naming what is
//! available, a server that cannot be listed a 502, and neither writes a
//! `request_logs` row.

use lmgw_core::config::KeyPolicy;
use lmgw_core::state::SharedState;
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::support::mcp_stub::{dead_url, echo_stub, register, stub, McpStub};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, ChatFake, KEY,
};

struct World {
    state: SharedState,
    addr: String,
    owner: String,
    alpha: McpStub,
    beta: McpStub,
    agent: McpStub,
    gamma: McpStub,
    _fake: ChatFake,
}

/// Auth on; servers `alpha` (prefix `a`), `beta` (`b`), a service agent's
/// `board` (`board`) and a disabled `gamma` (`c`); the client key's tool
/// scope allows `a__*`.
async fn world() -> World {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    sqlx::query(
        "UPDATE api_keys SET tool_scope_mode = 'allow', tool_scope_patterns = 'a__*'
         WHERE name = 'voice'",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (alpha, beta, agent, gamma) = (
        echo_stub().await,
        echo_stub().await,
        echo_stub().await,
        echo_stub().await,
    );
    register(&state, "alpha", "a", &alpha.url, true, None).await;
    register(&state, "beta", "b", &beta.url, true, None).await;
    register(&state, "board", "board", &agent.url, true, Some("board")).await;
    register(&state, "gamma", "c", &gamma.url, false, None).await;
    let owner = crate::common::dashboard_key(&state);
    World {
        state,
        addr,
        owner,
        alpha,
        beta,
        agent,
        gamma,
        _fake: fake,
    }
}

async fn get(w: &World, bearer: Option<&str>, path: &str) -> (StatusCode, Value) {
    let mut req = reqwest::Client::new().get(format!("http://{}{path}", w.addr));
    if let Some(b) = bearer {
        req = req.bearer_auth(b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap())
}

fn labels(list: &Value) -> Vec<&str> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["server_label"].as_str().unwrap())
        .collect()
}

async fn rows(state: &SharedState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_list_follows_the_caller_and_connects_nothing() {
    let w = world().await;
    let (status, list) = get(&w, Some(&w.owner), "/v1/mcp/servers").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["object"], "list");
    assert_eq!(labels(&list), vec!["lmgw", "docs", "kb", "a", "b", "board"]);
    let data = list["data"].as_array().unwrap();
    assert_eq!(
        data[3],
        json!({"object": "mcp.server", "server_label": "a", "kind": "server", "name": "alpha",
               "description": ""})
    );
    assert_eq!(data[5]["kind"], "agent");
    for builtin in &data[..3] {
        assert_eq!(builtin["kind"], "builtin", "{builtin}");
        assert!(
            !builtin["description"].as_str().unwrap().is_empty(),
            "{builtin}"
        );
        assert!(builtin.get("tools").is_none(), "{builtin}");
    }

    // A client key: what its tool scope reaches, never `lmgw`.
    let (_, list) = get(&w, Some(KEY), "/v1/mcp/servers").await;
    assert_eq!(labels(&list), vec!["a"]);

    for stub in [&w.alpha, &w.beta, &w.agent, &w.gamma] {
        assert_eq!(stub.hits(), 0, "the list connected {}", stub.url);
    }
    assert_eq!(rows(&w.state).await, 0);
    // The inference credential, as on /mcp (the gate logs its refusal).
    let (status, _) = get(&w, None, "/v1/mcp/servers").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_detail_lists_one_label_by_its_wire_names() {
    let w = world().await;
    let (status, a) = get(&w, Some(KEY), "/v1/mcp/servers/a").await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(
        a,
        json!({"object": "mcp.server", "server_label": "a", "kind": "server", "name": "alpha",
               "description": "",
               "tools": [{"name": "echo", "description": "echo the input",
                          "input_schema": {"type": "object",
                                           "properties": {"text": {"type": "string"}}}}]})
    );
    assert!(w.alpha.hits() > 0);
    assert_eq!(w.beta.hits(), 0, "only the named server connects");

    // Addressed by its name, it is the same object under its label.
    let (_, by_name) = get(&w, Some(&w.owner), "/v1/mcp/servers/alpha").await;
    assert_eq!(by_name, a);

    let (status, docs) = get(&w, Some(&w.owner), "/v1/mcp/servers/docs").await;
    assert_eq!(status, StatusCode::OK, "{docs}");
    let names: Vec<&str> = docs["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["resolve", "query", "request"]);
    assert_eq!(rows(&w.state).await, 0);
}

#[tokio::test]
async fn a_label_the_caller_may_not_use_is_a_404_naming_the_available() {
    let w = world().await;
    for (bearer, label, says) in [
        (KEY, "b", "(available: a)"),
        (KEY, "nope", "(available: a)"),
        (KEY, "lmgw", "needs an owner credential"),
        (w.owner.as_str(), "c", "is disabled"),
    ] {
        let (status, body) = get(&w, Some(bearer), &format!("/v1/mcp/servers/{label}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}: {body}");
        assert_eq!(body["error"]["code"], "mcp_server_not_found", "{body}");
        assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{label}: {message}");
    }
    assert_eq!(w.beta.hits() + w.gamma.hits(), 0);
}

#[tokio::test]
async fn a_server_that_cannot_be_listed_is_a_502() {
    let w = world().await;
    let (dead, _held) = dead_url();
    register(&w.state, "dead", "d", &dead, true, None).await;
    let (status, body) = get(&w, Some(&w.owner), "/v1/mcp/servers/d").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "mcp_list_tools_failed", "{body}");
    assert!(
        !body["error"]["message"].as_str().unwrap().is_empty(),
        "{body}"
    );
}

/// The detail and a realtime session's `mcp_list_tools` item come from one
/// function: for the same caller and label, the same tools (§1.4).
#[tokio::test]
async fn the_detail_and_the_listing_item_agree() {
    let w = world().await;
    let auth = format!("Bearer {}", w.owner);
    let mut ws = open(
        &w.addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &auth)],
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    for label in ["a", "docs", "lmgw"] {
        send(
            &mut ws,
            json!({"type": "session.update", "session": {"type": "realtime",
                   "tools": [{"type": "mcp", "server_label": label}]}}),
        )
        .await;
        assert_eq!(next_event(&mut ws).await["type"], "session.updated");
        let events = events_until(&mut ws, "conversation.item.done").await;
        let mut listed = events.last().unwrap()["item"]["tools"].clone();
        for t in listed.as_array_mut().unwrap() {
            assert_eq!(
                t.as_object_mut().unwrap().remove("annotations"),
                Some(Value::Null)
            );
        }
        let (status, detail) = get(&w, Some(&w.owner), &format!("/v1/mcp/servers/{label}")).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["tools"], listed, "{label}");
        assert!(!listed.as_array().unwrap().is_empty(), "{label}");
    }
}

/// A bare server — no tool prefix — has no namespace a key's tool scope can
/// be read against. The list shows it to a scoped key only once a tool of
/// it the key admits is listed, and the detail of one whose every tool the
/// scope keeps out is a 404 like any label the key may not use, in the same
/// words as a label that does not exist (final review #3).
#[tokio::test]
async fn a_bare_server_is_shown_to_a_scoped_key_only_with_a_tool_it_admits() {
    let w = world().await;
    sqlx::query("UPDATE api_keys SET tool_scope_patterns = 'a__*\nshout' WHERE name = 'voice'")
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    // `echo`, outside the key's scope; `shout`, inside it.
    let zeta = echo_stub().await;
    let yell = stub(
        json!([{"name": "shout", "description": "shout it", "inputSchema": {"type": "object"}}]),
        false,
    )
    .await;
    register(&w.state, "zeta", "", &zeta.url, true, None).await;
    register(&w.state, "yell", "", &yell.url, true, None).await;

    // Nothing of either is listed: the key is told of neither, the owner of
    // both.
    let (_, list) = get(&w, Some(KEY), "/v1/mcp/servers").await;
    assert_eq!(labels(&list), vec!["a"]);
    let (_, list) = get(&w, Some(&w.owner), "/v1/mcp/servers").await;
    let all = labels(&list);
    assert!(all.contains(&"zeta") && all.contains(&"yell"), "{all:?}");

    let (status, body) = get(&w, Some(KEY), "/v1/mcp/servers/zeta").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "mcp_server_not_found", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no MCP server with label 'zeta'") && message.contains("(available: a)"),
        "{message}"
    );

    let (status, body) = get(&w, Some(KEY), "/v1/mcp/servers/yell").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tools"][0]["name"], "shout", "{body}");
    // Both are connected now: the key is told of the one it may use.
    let (_, list) = get(&w, Some(KEY), "/v1/mcp/servers").await;
    assert_eq!(labels(&list), vec!["a", "yell"]);
    let (status, _) = get(&w, Some(KEY), "/v1/mcp/servers/zeta").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The owner's detail of zeta is its tools.
    let (status, body) = get(&w, Some(&w.owner), "/v1/mcp/servers/zeta").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tools"][0]["name"], "echo", "{body}");
}
