//! `/v1/responses` regressions for the `mcp` entry parser it now shares with
//! `/v1/realtime` (realtime-server-tools design §1.1, §1.2; WP1): the object
//! form of `allowed_tools`, a built-in's tools matched by both spellings,
//! `read_only` refused, an empty `tool_names` said as such — and a resolve
//! that connects only the servers a request names.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::responses_api::{
    call_reply, mcp_stub, mount_sequence, output_types, post, refusal_message, register_mcp, setup,
    text_reply,
};

/// The `mcp_list_tools` item of `label`.
fn listing<'a>(resp: &'a Value, label: &str) -> &'a Value {
    resp["output"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|i| i["type"] == "mcp_list_tools" && i["server_label"] == label)
        })
        .unwrap_or_else(|| panic!("no listing of '{label}': {resp:#}"))
}

fn listed_names(item: &Value) -> Vec<&str> {
    item["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default()
}

/// A request with one `mcp` entry, answered by a model that says `ok`.
async fn listed_with(entry: Value, register: bool) -> Value {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("ok")]).await;
    let (state, base) = setup(&mock.uri()).await;
    if register {
        let (url, _) = mcp_stub().await;
        register_mcp(&state, "stub", "tools", &url).await;
    }
    let (status, resp) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "tools": [entry]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    resp
}

#[tokio::test]
async fn allowed_tools_takes_the_object_form_and_either_name() {
    for allowed in [
        json!(["echo"]),
        json!({"tool_names": ["echo"]}),
        json!({"tool_names": ["tools__echo"]}),
    ] {
        let resp = listed_with(
            json!({"type": "mcp", "server_label": "tools", "allowed_tools": allowed}),
            true,
        )
        .await;
        assert_eq!(
            listed_names(listing(&resp, "tools")),
            vec!["tools__echo"],
            "{allowed}"
        );
    }
    let resp = listed_with(
        json!({"type": "mcp", "server_label": "tools",
               "allowed_tools": {"tool_names": ["nope"]}}),
        true,
    )
    .await;
    let err = listing(&resp, "tools")["error"].as_str().unwrap();
    assert!(err.contains("nope"), "{err}");
}

/// A built-in's tools match by their full and their short name, as a
/// registered server's do by exposed and upstream name (§1.1).
#[tokio::test]
async fn a_built_in_s_tools_match_either_spelling() {
    for allowed in [
        json!(["docs__query"]),
        json!(["query"]),
        json!({"tool_names": ["query"]}),
    ] {
        let resp = listed_with(
            json!({"type": "mcp", "server_label": "docs", "allowed_tools": allowed}),
            false,
        )
        .await;
        assert_eq!(
            listed_names(listing(&resp, "docs")),
            vec!["docs__query"],
            "{allowed}"
        );
    }
}

/// A gate written with a built-in's short name gates. Before the shared
/// parser it matched only the full name, so this one silently did not.
#[tokio::test]
async fn a_gate_on_a_built_in_s_short_name_gates() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![call_reply(
            "c1",
            "docs__query",
            json!({"corpus": "x", "query": "q"}),
        )],
    )
    .await;
    let (_state, base) = setup(&mock.uri()).await;
    let (status, resp) = post(
        &base,
        json!({"model": "my-model", "input": "look it up",
               "tools": [{"type": "mcp", "server_label": "docs",
                          "require_approval": {"always": {"tool_names": ["query"]}}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let types = output_types(&resp);
    assert!(types.contains(&"mcp_approval_request"), "{resp:#}");
    assert!(!types.contains(&"mcp_call"), "{resp:#}");
}

#[tokio::test]
async fn read_only_and_a_wrong_typed_allowed_tools_are_refused() {
    for (field, value) in [
        ("allowed_tools", json!({"read_only": true})),
        (
            "allowed_tools",
            json!({"tool_names": ["echo"], "read_only": false}),
        ),
        ("require_approval", json!({"always": {"read_only": true}})),
        ("require_approval", json!({"never": {"read_only": true}})),
    ] {
        let msg = refusal_message(json!({
            "model": "my-model", "input": "go",
            "tools": [{"type": "mcp", "server_label": "tools", field: value}],
        }))
        .await;
        assert!(
            msg.contains("lmgw does not read MCP tool annotations")
                && msg.contains(field)
                && msg.contains("'tools'"),
            "{field}: {msg}"
        );
    }
    // Read as "no filter", a typo would offer every tool the server has.
    let msg = refusal_message(json!({
        "model": "my-model", "input": "go",
        "tools": [{"type": "mcp", "server_label": "tools", "allowed_tools": "echo"}],
    }))
    .await;
    assert!(
        msg.contains("allowed_tools") && msg.contains("string"),
        "{msg}"
    );
    // Dropped, an entry that is no name would leave another filter.
    let msg = refusal_message(json!({
        "model": "my-model", "input": "go",
        "tools": [{"type": "mcp", "server_label": "tools",
                   "allowed_tools": {"tool_names": ["echo", 1]}}],
    }))
    .await;
    assert!(
        msg.contains("allowed_tools.tool_names on mcp server 'tools': entry 1 is a number"),
        "{msg}"
    );
}

/// `{tool_names: []}` is the client's own filter leaving nothing — not a
/// server or a toolset that has nothing. Such a server is not connected.
#[tokio::test]
async fn an_empty_tool_names_list_says_it_left_nothing() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("ok")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let (url, hits) = counting_stub().await;
    register_mcp(&state, "stub", "tools", &url).await;

    let (_, resp) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "tools": [
            {"type": "mcp", "server_label": "tools", "allowed_tools": {"tool_names": []}},
            {"type": "mcp", "server_label": "docs", "allowed_tools": []},
        ]}),
    )
    .await;
    for label in ["tools", "docs"] {
        let item = listing(&resp, label);
        let err = item["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("allowed_tools is an empty list"),
            "{label}: {item}"
        );
        assert!(listed_names(item).is_empty(), "{item}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the server was contacted");
}

/// Resolve connects the servers a request names and no other; a built-in
/// label or an unknown one connects none (§1.2, finding 6).
#[tokio::test]
async fn resolve_connects_only_the_named_servers() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![text_reply("one"), text_reply("two"), text_reply("three")],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (alpha_url, alpha) = counting_stub().await;
    let (beta_url, beta) = counting_stub().await;
    register_mcp(&state, "alpha", "a", &alpha_url).await;
    register_mcp(&state, "beta", "b", &beta_url).await;

    let ask = |label: &str| {
        json!({"model": "my-model", "input": "hi",
               "tools": [{"type": "mcp", "server_label": label}]})
    };
    for label in ["docs", "nope"] {
        let (status, resp) = post(&base, ask(label)).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
    }
    assert_eq!(
        alpha.load(Ordering::SeqCst),
        0,
        "a built-in or unknown label"
    );
    assert_eq!(
        beta.load(Ordering::SeqCst),
        0,
        "a built-in or unknown label"
    );

    let (_, resp) = post(&base, ask("a")).await;
    assert_eq!(listed_names(listing(&resp, "a")), vec!["a__echo"]);
    assert!(alpha.load(Ordering::SeqCst) > 0);
    assert_eq!(beta.load(Ordering::SeqCst), 0, "beta was not named");
}

/// A Streamable-HTTP MCP server with one tool, `echo`, counting every request
/// it gets — a connect is an `initialize`, so a server never named sees none.
async fn counting_stub() -> (String, Arc<AtomicUsize>) {
    use axum::response::IntoResponse;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let handler = move |body: String| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": req.pointer("/params/protocolVersion")
                        .cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "counting", "version": "0.1.0"},
                }),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "echo the input",
                    "inputSchema": {"type": "object"},
                }]}),
                _ => json!({}),
            };
            let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "counting-session"),
                ],
                body,
            )
                .into_response()
        }
    };
    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), hits)
}
