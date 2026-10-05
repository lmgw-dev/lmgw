//! End-to-end `/v1/responses` tests (§21) over real HTTP.
//!
//! The model is a wiremock upstream and the MCP server is a small in-test
//! Streamable-HTTP stub, so the whole path runs for real: request parsing, the
//! tool loop, MCP connect/list/call through `McpManager`, the `output` array,
//! the SSE event sequence, and the `request_logs` rows.

use std::sync::Arc;

use axum::http::StatusCode;
use lmgw_core::config::{McpTransport, Protocol, UpstreamKind};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewMcpServer, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn serve(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A gateway with `my-model` pointing at `upstream_base`.
async fn setup(upstream_base: &str) -> (SharedState, String) {
    setup_native(upstream_base, false).await
}

async fn setup_native(upstream_base: &str, supports_responses: bool) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "my-model".into(),
            upstream_id: up_id,
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
    (state, base)
}

async fn register_mcp(state: &SharedState, name: &str, prefix: &str, url: &str) {
    store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: name.into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url.into()),
            headers: vec![],
            tool_prefix: prefix.into(),
            timeout_ms: 5_000,
            autostart: false,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
}

/// A minimal MCP Streamable-HTTP server: `initialize` / `tools/list` /
/// `tools/call`, JSON responses (spec-legal), one tool that echoes its
/// arguments. Enough to exercise the real `rmcp` client path without Podman.
async fn mcp_stub() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use axum::response::IntoResponse;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls2 = calls.clone();

    let handler = move |body: String| {
        let calls = calls2.clone();
        async move {
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            // Notifications carry no id and take 202 with no body.
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let m = req.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match m {
                "initialize" => json!({
                    "protocolVersion": req.pointer("/params/protocolVersion")
                        .cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "0.1.0"},
                }),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "echo the input",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                    },
                }]}),
                "tools/call" => {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let args = req
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    json!({
                        "content": [{"type": "text", "text": format!("echoed {args}")}],
                        "isError": false,
                    })
                }
                _ => json!({}),
            };
            let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "stub-session"),
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
    (format!("http://{addr}/mcp"), calls)
}

/// An upstream chat/completions reply carrying plain text.
fn text_reply(text: &str) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4},
    })
}

/// An upstream reply asking for one tool call.
fn call_reply(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function", "function": {
                "name": name, "arguments": args.to_string(),
            }}],
        }, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4},
    })
}

async fn post(base: &str, body: Value) -> (StatusCode, Value) {
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// Mount a sequence of replies, one per successive upstream call.
async fn mount_sequence(mock: &MockServer, replies: Vec<Value>) {
    for (i, reply) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .expect(..=1)
            .mount(mock)
            .await;
    }
}

fn output_types(resp: &Value) -> Vec<&str> {
    resp["output"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i.get("type").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_request_returns_a_response_object() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("pong")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (status, resp) = post(&base, json!({"model": "my-model", "input": "ping"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "response");
    assert_eq!(resp["status"], "completed");
    assert_eq!(resp["model"], "my-model");
    assert!(resp["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(output_types(&resp), vec!["message"]);
    let msg = &resp["output"][0];
    assert_eq!(msg["role"], "assistant");
    assert_eq!(msg["content"][0]["type"], "output_text");
    assert_eq!(msg["content"][0]["text"], "pong");
    // Responses nests usage differently from chat-completions.
    assert_eq!(resp["usage"]["input_tokens"], 10);
    assert_eq!(resp["usage"]["output_tokens"], 4);
    assert_eq!(resp["usage"]["total_tokens"], 14);
    assert_eq!(resp["incomplete_details"], Value::Null);
}

/// `store` reports what the gateway actually did, so a client learns whether
/// `previous_response_id` will work *before* it tries. The request may turn
/// storing off; the Setting may veto it outright.
#[tokio::test]
async fn store_is_reported_honestly() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("a"), text_reply("b")]).await;
    let (state, base) = setup(&mock.uri()).await;

    let (_, on) = post(&base, json!({"model": "my-model", "input": "hi"})).await;
    assert_eq!(on["store"], true, "stored by default, as the API does");
    assert!(
        store::get_response(&state.db, on["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_some(),
        "and it really is in the store"
    );

    let (_, off) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "store": false}),
    )
    .await;
    assert_eq!(off["store"], false);
    assert!(
        store::get_response(&state.db, off["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none(),
        "store:false must not persist"
    );
}

/// The Setting is the ceiling: with storing switched off, a request asking for
/// it is told no rather than quietly getting a response it cannot chain from.
#[tokio::test]
async fn the_store_setting_overrides_the_request() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("ok")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let mut cfg = state.snapshot().settings.clone();
    cfg.responses_store = false;
    store::save_settings(&state.db, &cfg).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (_, resp) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "store": true}),
    )
    .await;
    assert_eq!(resp["store"], false);
    assert!(store::get_response(&state.db, resp["id"].as_str().unwrap())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn instructions_become_a_system_message_upstream() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("ok")]).await;
    let (_state, base) = setup(&mock.uri()).await;
    post(
        &base,
        json!({"model": "my-model", "input": "hi", "instructions": "be terse"}),
    )
    .await;

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "be terse");
    assert_eq!(sent["messages"][1]["content"], "hi");
    assert_eq!(sent["model"], "tgt-model");
}

// ---------------------------------------------------------------------------
// Client tools — the loop must hand these back, not run them
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_client_function_tool_stops_the_run_and_is_returned() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("call_1", "read_file", json!({"path": "/tmp/x"})),
            text_reply("must not be reached"),
        ],
    )
    .await;
    let (_state, base) = setup(&mock.uri()).await;

    let (status, resp) = post(
        &base,
        json!({
            "model": "my-model",
            "input": "read it",
            "tools": [{"type": "function", "name": "read_file",
                       "parameters": {"type": "object"}}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["status"], "completed");
    assert_eq!(output_types(&resp), vec!["function_call"]);
    let fc = &resp["output"][0];
    assert_eq!(fc["name"], "read_file");
    assert_eq!(fc["call_id"], "call_1");
    assert_eq!(fc["arguments"], json!({"path": "/tmp/x"}).to_string());
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        1,
        "the gateway must not run a second turn for a tool it cannot execute"
    );
}

/// The stateless round trip: the client replays the transcript with its result
/// appended. Without this the client-tool stop above would be a dead end.
#[tokio::test]
async fn a_replayed_function_call_output_continues_the_conversation() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("the file says hi")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (_, resp) = post(
        &base,
        json!({
            "model": "my-model",
            "input": [
                {"role": "user", "content": "read it"},
                {"type": "function_call", "call_id": "call_1", "name": "read_file",
                 "arguments": "{\"path\":\"/tmp/x\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "hi"},
            ],
            "tools": [{"type": "function", "name": "read_file",
                       "parameters": {"type": "object"}}],
        }),
    )
    .await;
    assert_eq!(resp["output"][0]["content"][0]["text"], "the file says hi");

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["messages"][1]["role"], "assistant");
    assert_eq!(sent["messages"][1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(sent["messages"][2]["role"], "tool");
    assert_eq!(sent["messages"][2]["tool_call_id"], "call_1");
    assert_eq!(sent["messages"][2]["content"], "hi");
}

// ---------------------------------------------------------------------------
// MCP tools — the half a chat/completions shim cannot do
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_mcp_tool_is_listed_executed_and_fed_back() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "hello"})),
            text_reply("it echoed"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, tool_calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (status, resp) = post(
        &base,
        json!({
            "model": "my-model",
            "input": "echo hello",
            "tools": [{"type": "mcp", "server_label": "tools", "require_approval": "never"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["status"], "completed", "{resp}");
    assert_eq!(
        output_types(&resp),
        vec!["mcp_list_tools", "mcp_call", "message"]
    );

    let listed = &resp["output"][0];
    assert_eq!(listed["server_label"], "tools");
    assert_eq!(listed["tools"][0]["name"], "tools__echo");

    let call = &resp["output"][1];
    assert_eq!(call["server_label"], "tools");
    assert_eq!(call["name"], "tools__echo");
    assert_eq!(call["error"], Value::Null);
    assert!(
        call["output"].as_str().unwrap().contains("echoed"),
        "the tool's result belongs on the item: {call}"
    );
    assert_eq!(tool_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The second turn must carry the tool result back to the model — that feed
    // back is the whole loop.
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let tool_msg = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result fed back");
    assert!(tool_msg["content"].as_str().unwrap().contains("hello"));
    // And the model was actually offered the tool, under its exposed name.
    let first: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(first["tools"][0]["function"]["name"], "tools__echo");
}

/// `allowed_tools` narrows the surface. A label naming a real server but no
/// matching tool is a listing failure, not a silent empty tool list.
#[tokio::test]
async fn allowed_tools_filters_the_server() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("nothing to do")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (_, resp) = post(
        &base,
        json!({
            "model": "my-model", "input": "hi",
            "tools": [{"type": "mcp", "server_label": "tools",
                       "allowed_tools": ["not_a_tool"]}],
        }),
    )
    .await;
    let listed = &resp["output"][0];
    assert_eq!(listed["type"], "mcp_list_tools");
    assert!(
        listed["error"].as_str().unwrap().contains("not_a_tool"),
        "{listed}"
    );

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert!(
        sent.get("tools").is_none() || sent["tools"].as_array().unwrap().is_empty(),
        "no tool may be offered when the filter matched nothing"
    );
}

/// An unknown label must not 400 the whole request: other servers can still
/// work, and the client gets a failed listing naming what *is* available.
#[tokio::test]
async fn an_unknown_server_label_is_reported_not_fatal() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("no tools then")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (status, resp) = post(
        &base,
        json!({
            "model": "my-model", "input": "hi",
            "tools": [{"type": "mcp", "server_label": "nope"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["status"], "completed");
    let listed = &resp["output"][0];
    assert_eq!(listed["type"], "mcp_list_tools");
    let err = listed["error"].as_str().unwrap();
    assert!(err.contains("nope"), "{err}");
    assert!(
        err.contains("tools"),
        "must name the available labels: {err}"
    );
    assert!(
        err.contains("server_url"),
        "must explain that labels resolve locally: {err}"
    );
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tool_call_budget_ends_the_response_as_incomplete() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(call_reply(
            "c1",
            "tools__echo",
            json!({"text": "again"}),
        )))
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (status, resp) = post(
        &base,
        json!({
            "model": "my-model", "input": "loop forever",
            "max_tool_calls": 2,
            "tools": [{"type": "mcp", "server_label": "tools"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["status"], "incomplete");
    assert_eq!(resp["incomplete_details"]["reason"], "max_tool_calls");
    // Two calls ran. The third is in `output` too — as an `mcp_call` marked
    // `incomplete`, so the client can see what the model wanted next and that
    // the gateway declined to run it, rather than the item vanishing.
    let calls: Vec<&Value> = resp["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "mcp_call")
        .collect();
    assert_eq!(calls.len(), 3, "{resp:#}");
    let statuses: Vec<&str> = calls
        .iter()
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["completed", "completed", "incomplete"]);
}

/// A response the tool-call budget stopped stores a result for the call it
/// did not make, so a chained request replays every call with its result
/// and a strict upstream answers it (chat-voice design §7.3).
#[tokio::test]
async fn a_chain_after_a_budget_stop_replays_every_call_with_its_result() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "one"})),
            call_reply("c2", "tools__echo", json!({"text": "two"})),
            text_reply("fine"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;
    let tools = json!([{"type": "mcp", "server_label": "tools"}]);

    let (_, first) = post(
        &base,
        json!({"model": "my-model", "input": "echo twice", "max_tool_calls": 1,
               "tools": tools}),
    )
    .await;
    assert_eq!(first["status"], "incomplete", "{first:#}");
    let (status, _) = post(
        &base,
        json!({"model": "my-model", "input": "go on", "tools": tools,
               "previous_response_id": first["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let reqs = mock.received_requests().await.unwrap();
    let chained: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    let msgs = chained["messages"].as_array().unwrap();
    let c2 = msgs
        .iter()
        .position(|m| m["tool_calls"][0]["id"] == "c2")
        .expect("the unmade call is replayed");
    assert_eq!(msgs[c2 + 1]["role"], "tool", "{chained:#}");
    assert_eq!(msgs[c2 + 1]["tool_call_id"], "c2", "{chained:#}");
    assert!(msgs[c2 + 1].to_string().contains("not run"), "{chained:#}");
    assert_eq!(msgs.last().unwrap()["content"], "go on");
}

/// The Settings ceiling is a ceiling: a request may lower it, never raise it.
#[tokio::test]
async fn a_request_cannot_raise_the_gateway_tool_call_ceiling() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(call_reply(
            "c1",
            "tools__echo",
            json!({"text": "again"}),
        )))
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let mut s = state.snapshot().settings.clone();
    s.responses_max_tool_calls = 1;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (_, resp) = post(
        &base,
        json!({
            "model": "my-model", "input": "go",
            "max_tool_calls": 999,
            "tools": [{"type": "mcp", "server_label": "tools"}],
        }),
    )
    .await;
    assert_eq!(resp["status"], "incomplete");
    assert_eq!(resp["incomplete_details"]["reason"], "max_tool_calls");
    assert_eq!(resp["max_tool_calls"], 1, "the ceiling is echoed back");
}

// ---------------------------------------------------------------------------
// Refusals — surfaced, never silently approximated
// ---------------------------------------------------------------------------

async fn refusal_message(body: Value) -> String {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("unused")]).await;
    let (_state, base) = setup(&mock.uri()).await;
    let (status, resp) = post(&base, body).await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "{resp}"
    );
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "a refused request must not reach the model"
    );
    resp["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_hosted_tool_is_refused_with_a_readable_reason() {
    let msg = refusal_message(json!({
        "model": "my-model", "input": "search",
        "tools": [{"type": "web_search"}],
    }))
    .await;
    assert!(msg.contains("web_search"), "{msg}");
    assert!(msg.contains("function") && msg.contains("mcp"), "{msg}");
}

/// An id that was never stored (or has been evicted) is a 404 that says which,
/// not a silently fresh conversation.
#[tokio::test]
async fn an_unknown_previous_response_id_is_a_readable_404() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("unused")]).await;
    let (_state, base) = setup(&mock.uri()).await;
    let (status, resp) = post(
        &base,
        json!({"model": "my-model", "input": "go", "previous_response_id": "resp_abc"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("resp_abc"), "{msg}");
    assert!(msg.contains("evicted"), "{msg}");
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "a refused request must not reach the model"
    );
}

/// With storing switched off there is nothing to continue from, and the error
/// names the setting rather than pretending the id was bad.
#[tokio::test]
async fn previous_response_id_names_the_setting_when_storing_is_off() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("unused")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let mut cfg = state.snapshot().settings.clone();
    cfg.responses_store = false;
    store::save_settings(&state.db, &cfg).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, resp) = post(
        &base,
        json!({"model": "my-model", "input": "go", "previous_response_id": "resp_abc"}),
    )
    .await;
    assert!(status.is_client_error(), "{resp}");
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("Settings"), "{msg}");
}

/// A `require_approval` value that is neither spelling is a 400 naming what is
/// accepted — not a shrug that quietly runs the tool.
#[tokio::test]
async fn an_unknown_require_approval_value_is_refused() {
    let msg = refusal_message(json!({
        "model": "my-model", "input": "go",
        "tools": [{"type": "mcp", "server_label": "tools", "require_approval": "maybe"}],
    }))
    .await;
    assert!(msg.contains("require_approval"), "{msg}");
    assert!(msg.contains("never") && msg.contains("always"), "{msg}");
}

#[tokio::test]
async fn background_and_auto_truncation_are_refused() {
    let msg = refusal_message(json!({
        "model": "my-model", "input": "go", "background": true,
    }))
    .await;
    assert!(msg.contains("background"), "{msg}");

    let msg = refusal_message(json!({
        "model": "my-model", "input": "go", "truncation": "auto",
    }))
    .await;
    assert!(msg.contains("truncation"), "{msg}");
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

async fn sse_events(base: &str, body: Value) -> Vec<(String, Value)> {
    let text = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event: ") {
                name = v.to_string();
            } else if let Some(v) = line.strip_prefix("data: ") {
                data = v.to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    out
}

/// A stream that breaks after part of its answer: the text already relayed
/// closes as an `incomplete` message, then `response.failed` — not as a
/// completed answer.
#[tokio::test]
async fn a_stream_failing_mid_answer_closes_its_message_incomplete() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half an\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"the model fell over\"}}\n\n",
            ),
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let events = sse_events(
        &base,
        json!({"model": "my-model", "input": "hi", "stream": true}),
    )
    .await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"response.output_text.delta"), "{names:?}");
    assert_eq!(names.last(), Some(&"response.failed"), "{names:?}");
    let item = events
        .iter()
        .find(|(n, d)| n == "response.output_item.done" && d["item"]["type"] == "message")
        .map(|(_, d)| &d["item"])
        .expect("the message item is closed");
    assert_eq!(item["status"], "incomplete", "{item}");
    assert_eq!(item["content"][0]["text"], "half an");
}

#[tokio::test]
async fn streaming_emits_the_response_event_sequence() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
                "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;

    let events = sse_events(
        &base,
        json!({"model": "my-model", "input": "hi", "stream": true}),
    )
    .await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    // sequence_number is monotonic across the whole response.
    let seqs: Vec<u64> = events
        .iter()
        .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>());

    let deltas: String = events
        .iter()
        .filter(|(n, _)| n == "response.output_text.delta")
        .map(|(_, v)| v["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "hello");

    let (_, done) = events.last().unwrap();
    assert_eq!(done["response"]["status"], "completed");
    assert_eq!(done["response"]["output"][0]["content"][0]["text"], "hello");
    assert_eq!(done["response"]["usage"]["output_tokens"], 2);
}

/// A streamed run and an unstreamed one must build the same `output`; they
/// share the encoder precisely so they cannot drift.
#[tokio::test]
async fn a_streamed_mcp_run_emits_call_events_and_the_same_output() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
                "\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"tools__echo\",",
                "\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
                "\"function\":{\"arguments\":\"{\\\"text\\\":\\\"hi\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},",
                "\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .with_priority(2)
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let events = sse_events(
        &base,
        json!({
            "model": "my-model", "input": "echo", "stream": true,
            "tools": [{"type": "mcp", "server_label": "tools"}],
        }),
    )
    .await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [
        "response.mcp_call_arguments.delta",
        "response.mcp_call_arguments.done",
        "response.mcp_call.in_progress",
        "response.mcp_call.completed",
        "response.completed",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }

    let (_, done) = events.last().unwrap();
    let types: Vec<&str> = done["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, vec!["mcp_list_tools", "mcp_call", "message"]);
    assert!(done["response"]["output"][1]["output"]
        .as_str()
        .unwrap()
        .contains("hi"));
}

// ---------------------------------------------------------------------------
// Native passthrough
// ---------------------------------------------------------------------------

/// An upstream that implements `/v1/responses` gets the body forwarded — the
/// loop must not synthesize on top of it, or the provider's own reasoning items
/// would be lost between tool calls.
#[tokio::test]
async fn a_native_upstream_is_forwarded_verbatim() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_upstream", "object": "response", "status": "completed",
            "output": [{"type": "reasoning", "id": "rs_1",
                        "summary": [{"type": "summary_text", "text": "thought"}]}],
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, base) = setup_native(&mock.uri(), true).await;

    let (status, resp) = post(&base, json!({"model": "my-model", "input": "hi"})).await;
    assert_eq!(status, StatusCode::OK);
    // The upstream's own id and reasoning item survive untouched.
    assert_eq!(resp["id"], "resp_upstream");
    assert_eq!(resp["output"][0]["type"], "reasoning");

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "tgt-model", "only the alias is rewritten");
    assert_eq!(sent["input"], "hi");
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// One row per model turn plus one per tool call, with the tool rows excluded
/// from the token aggregates (they have none).
#[tokio::test]
async fn turns_and_tool_calls_are_logged_separately() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "x"})),
            text_reply("done"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    post(
        &base,
        json!({
            "model": "my-model", "input": "echo",
            "tools": [{"type": "mcp", "server_label": "tools"}],
        }),
    )
    .await;

    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let turns: Vec<_> = logs
        .iter()
        .filter(|l| l.ingress_proto == "responses")
        .collect();
    let tools: Vec<_> = logs
        .iter()
        .filter(|l| l.ingress_proto == "responses-tool")
        .collect();
    assert_eq!(turns.len(), 2, "one row per model turn");
    assert_eq!(tools.len(), 1, "one row per tool call");
    assert_eq!(tools[0].mcp_tool.as_deref(), Some("tools__echo"));
    assert_eq!(tools[0].status, 200);
    assert!(
        tools[0].prompt_tokens.is_none() && tools[0].completion_tokens.is_none(),
        "a tool call has no tokens and must not pollute the aggregates"
    );
    assert!(turns.iter().all(|t| t.prompt_tokens == Some(10)));
}

/// A request that never reaches the model still leaves a trace — otherwise the
/// per-turn rows would be the only record and a rejected request would vanish.
#[tokio::test]
async fn a_request_rejected_before_any_turn_is_still_logged() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("unused")]).await;
    let (state, base) = setup(&mock.uri()).await;

    let (status, _) = post(&base, json!({"model": "no-such-alias", "input": "hi"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = logs
        .iter()
        .find(|l| l.ingress_proto == "responses")
        .expect("the rejection is logged");
    assert_eq!(row.requested_alias, "no-such-alias");
    assert_eq!(row.status, 404);
    assert!(row.error_msg.is_some());
}

/// Probe: does the in-test stub satisfy the real `rmcp` client at all? Kept as
/// a test so a stub that silently stops connecting fails here, loudly, instead
/// of turning every MCP assertion above into a confusing "no tools" result.
#[tokio::test]
async fn the_mcp_stub_is_reachable() {
    let state = AppState::init_for_tests().await.unwrap();
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;
    let server = state
        .snapshot()
        .mcp_servers
        .values()
        .next()
        .unwrap()
        .clone();
    let n = state.mcp.test_connection(&server).await;
    assert_eq!(n, Ok(1), "stub connect/list failed: {n:?}");
}

// ---------------------------------------------------------------------------
// Stored conversations (§21 stage 2)
// ---------------------------------------------------------------------------

async fn get_json(url: &str) -> (StatusCode, Value) {
    let r = reqwest::Client::new().get(url).send().await.unwrap();
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// The point of storing: the second call sends only the new turn, and the model
/// still receives the whole conversation.
#[tokio::test]
async fn previous_response_id_replays_the_conversation() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("first"), text_reply("second")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (_, one) = post(&base, json!({"model": "my-model", "input": "remember 41"})).await;
    let first_id = one["id"].as_str().unwrap().to_string();

    let (status, two) = post(
        &base,
        json!({
            "model": "my-model", "input": "add one",
            "previous_response_id": first_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(two["previous_response_id"], first_id);

    // The upstream's second request must carry all four turns, not just the
    // new one — that is the whole difference storing makes.
    let reqs = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "{body:#}");
    assert_eq!(msgs[0]["content"], "remember 41");
    assert_eq!(msgs[1]["content"], "first");
    assert_eq!(msgs[2]["content"], "add one");
}

/// `GET`, `DELETE` and `input_items` — the rest of the resource.
#[tokio::test]
async fn a_stored_response_can_be_fetched_and_deleted() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("hello")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (_, resp) = post(&base, json!({"model": "my-model", "input": "hi"})).await;
    let id = resp["id"].as_str().unwrap().to_string();

    let (status, fetched) = get_json(&format!("{base}/v1/responses/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, resp, "GET returns exactly what POST returned");

    let (status, items) = get_json(&format!("{base}/v1/responses/{id}/input_items")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(items["object"], "list");
    assert_eq!(items["data"][0]["content"][0]["text"], "hi");

    let r = reqwest::Client::new()
        .delete(format!("{base}/v1/responses/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["deleted"], true);

    let (status, _) = get_json(&format!("{base}/v1/responses/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Instructions are swapped, not duplicated, when the client sends new ones —
/// and kept when it says nothing, so a continuation cannot silently lose the
/// system prompt the conversation has been running under.
#[tokio::test]
async fn instructions_are_swapped_on_a_chained_call_and_kept_when_omitted() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![text_reply("a"), text_reply("b"), text_reply("c")],
    )
    .await;
    let (_state, base) = setup(&mock.uri()).await;

    let (_, one) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "instructions": "be terse"}),
    )
    .await;
    let id1 = one["id"].as_str().unwrap().to_string();

    let (_, two) = post(
        &base,
        json!({"model": "my-model", "input": "again", "previous_response_id": id1,
               "instructions": "be verbose"}),
    )
    .await;
    let id2 = two["id"].as_str().unwrap().to_string();

    let (_, _three) = post(
        &base,
        json!({"model": "my-model", "input": "more", "previous_response_id": id2}),
    )
    .await;

    let reqs = mock.received_requests().await.unwrap();
    let systems = |i: usize| -> Vec<String> {
        let b: Value = serde_json::from_slice(&reqs[i].body).unwrap();
        b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(systems(1), vec!["be verbose"], "swapped, not appended");
    assert_eq!(systems(2), vec!["be verbose"], "kept when omitted");
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

/// The whole round trip: a gated tool stops the run with an approval request,
/// and approving it on the next call runs the tool and continues.
#[tokio::test]
async fn a_gated_tool_asks_first_and_runs_once_approved() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "hi"})),
            text_reply("done"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;
    let tools = json!([
        {"type": "mcp", "server_label": "tools", "require_approval": "always"}
    ]);

    let (status, asked) = post(
        &base,
        json!({"model": "my-model", "input": "echo hi", "tools": tools}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // OpenAI reports an approval-pending response as completed: the response is
    // finished, and the request in `output` is what the client acts on.
    assert_eq!(asked["status"], "completed");
    let req_item = asked["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_request")
        .unwrap_or_else(|| panic!("no approval request in {asked:#}"));
    assert_eq!(req_item["server_label"], "tools");
    assert_eq!(req_item["name"], "tools__echo");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool must not have run"
    );

    let approval_id = req_item["id"].as_str().unwrap().to_string();
    let (status, done) = post(
        &base,
        json!({
            "model": "my-model", "tools": tools,
            "previous_response_id": asked["id"],
            "input": [{"type": "mcp_approval_response",
                       "approval_request_id": approval_id, "approve": true}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "approving must run it exactly once: {done:#}"
    );
    let executed = done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_call")
        .unwrap_or_else(|| panic!("no mcp_call in {done:#}"));
    assert!(executed["output"].as_str().unwrap().contains("echoed"));
}

/// Denying it tells the *model* the call was refused, with the client's reason,
/// so it can carry on rather than wait for an answer that never comes.
#[tokio::test]
async fn denying_a_gated_tool_feeds_the_refusal_back_to_the_model() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "hi"})),
            text_reply("understood"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;
    let tools = json!([
        {"type": "mcp", "server_label": "tools", "require_approval": "always"}
    ]);

    let (_, asked) = post(
        &base,
        json!({"model": "my-model", "input": "echo hi", "tools": tools}),
    )
    .await;
    let approval_id = asked["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_request")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (_, done) = post(
        &base,
        json!({
            "model": "my-model", "tools": tools,
            "previous_response_id": asked["id"],
            "input": [{"type": "mcp_approval_response",
                       "approval_request_id": approval_id, "approve": false,
                       "reason": "not on a Sunday"}],
        }),
    )
    .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(done["status"], "completed");

    let reqs = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let tool_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool result fed back: {body:#}"));
    let text = tool_msg["content"].as_str().unwrap();
    assert!(text.contains("declined"), "{text}");
    assert!(text.contains("not on a Sunday"), "{text}");
}

/// A continuation that ignores a pending approval is an error naming what has
/// to be answered — silently treating silence as "no" would hide an incomplete
/// request from the client that made it.
#[tokio::test]
async fn an_unanswered_approval_blocks_the_continuation() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![call_reply("c1", "tools__echo", json!({"text": "hi"}))],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (_, asked) = post(
        &base,
        json!({"model": "my-model", "input": "echo hi",
               "tools": [{"type": "mcp", "server_label": "tools",
                          "require_approval": "always"}]}),
    )
    .await;

    let (status, resp) = post(
        &base,
        json!({"model": "my-model", "input": "never mind",
               "previous_response_id": asked["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("c1"), "{msg}");
    assert!(msg.contains("approve: false"), "{msg}");
}

/// `require_approval: {"never": {...}}` gates everything *not* listed — the
/// reading that fails closed.
#[tokio::test]
async fn a_never_list_auto_approves_only_what_it_names() {
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![
            call_reply("c1", "tools__echo", json!({"text": "hi"})),
            text_reply("done"),
        ],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (_, resp) = post(
        &base,
        json!({"model": "my-model", "input": "echo hi",
               "tools": [{"type": "mcp", "server_label": "tools",
                          "require_approval": {"never": {"tool_names": ["echo"]}}}]}),
    )
    .await;
    // `echo` is named, so it runs without asking — and matching on the server's
    // own tool name, not just the prefixed one, is the point.
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "{resp:#}"
    );
    assert!(!output_types(&resp).contains(&"mcp_approval_request"));
}

// ---------------------------------------------------------------------------
// Output items
// ---------------------------------------------------------------------------

/// Two tool calls in one turn must produce two distinct, correctly-populated
/// items. They share a turn but not an id, and each keeps its own arguments.
#[tokio::test]
async fn a_multi_call_turn_produces_distinct_output_items() {
    let mock = MockServer::start().await;
    let two_calls = json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {
            "role": "assistant", "content": null,
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {
                    "name": "tools__echo", "arguments": "{\"text\":\"one\"}"}},
                {"id": "c2", "type": "function", "function": {
                    "name": "tools__echo", "arguments": "{\"text\":\"two\"}"}},
            ],
        }, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4},
    });
    mount_sequence(&mock, vec![two_calls, text_reply("both done")]).await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, _) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;

    let (_, resp) = post(
        &base,
        json!({"model": "my-model", "input": "echo twice",
               "tools": [{"type": "mcp", "server_label": "tools"}]}),
    )
    .await;

    let calls: Vec<&Value> = resp["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "mcp_call")
        .collect();
    assert_eq!(calls.len(), 2, "{resp:#}");
    assert_ne!(calls[0]["id"], calls[1]["id"], "ids must not be shared");
    assert!(calls[0]["arguments"].as_str().unwrap().contains("one"));
    assert!(calls[1]["arguments"].as_str().unwrap().contains("two"));
    assert!(calls[0]["output"].as_str().unwrap().contains("one"));
    assert!(calls[1]["output"].as_str().unwrap().contains("two"));

    // `output_index` must still be a dense, ordered index into `output`.
    let types = output_types(&resp);
    assert_eq!(
        types,
        vec!["mcp_list_tools", "mcp_call", "mcp_call", "message"],
        "{resp:#}"
    );
}

// ---------------------------------------------------------------------------
// Reasoning control (model-capabilities design §5.5)
// ---------------------------------------------------------------------------

/// `reasoning.effort` was in the parser's modeled keys from day one and then
/// never read — so a Responses client's effort reached nothing. It does now.
#[tokio::test]
async fn reasoning_effort_reaches_the_upstream() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("done")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (status, _) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "reasoning": {"effort": "high"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["reasoning_effort"], "high");
}

/// The header tier reaches the loop too, and outranks the body.
#[tokio::test]
async fn a_reasoning_header_outranks_the_responses_body() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("done")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("x-lmgw-reasoning-effort", "low")
        .json(&json!({"model": "my-model", "input": "hi", "reasoning": {"effort": "high"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // A generic OpenAI upstream expresses an effort, so nothing is reported
    // ignored.
    assert!(resp.headers().get("x-lmgw-reasoning-ignored").is_none());

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["reasoning_effort"], "low");
}

/// On a native `/v1/responses` upstream the body is forwarded untouched — with
/// the one control that API can carry written in, and the ones it cannot named
/// on the response instead of dropped.
#[tokio::test]
async fn native_passthrough_takes_the_header_effort_and_reports_the_rest() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_upstream", "object": "response", "status": "completed", "output": [],
        })))
        .mount(&mock)
        .await;
    let (_state, base) = setup_native(&mock.uri(), true).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("x-lmgw-reasoning-effort", "xhigh")
        .header("x-lmgw-reasoning-budget", "4096")
        .json(&json!({"model": "my-model", "input": "hi", "reasoning": {"summary": "auto"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-reasoning-ignored"], "budget");

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["reasoning"]["effort"], "xhigh");
    // The client's own key in the same object survives.
    assert_eq!(sent["reasoning"]["summary"], "auto");
}

/// The alias' own reasoning default belongs to what the alias *means*, so it
/// reaches a native upstream too — the one route that forwards the body
/// verbatim was also the one where it used to be dropped.
#[tokio::test]
async fn an_alias_reasoning_default_reaches_a_native_upstream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_upstream", "object": "response", "status": "completed", "output": [],
        })))
        .mount(&mock)
        .await;
    let (state, base) = setup_native(&mock.uri(), true).await;

    let cur = store::list_aliases(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.alias == "my-model")
        .unwrap();
    store::update_alias(
        &state.db,
        cur.id,
        &store::NewAlias {
            alias: cur.alias,
            upstream_id: cur.upstream_id,
            upstream_model_id: cur.upstream_model_id,
            param_overrides: lmgw_core::ir::Params {
                reasoning: Some(lmgw_core::ir::ReasoningControl {
                    effort: Some("xhigh".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, _) = post(&base, json!({"model": "my-model", "input": "hi"})).await;
    assert_eq!(status, StatusCode::OK);

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["reasoning"]["effort"], "xhigh");

    // …and the body still outranks it, as the tier order says.
    let (status, _) = post(
        &base,
        json!({"model": "my-model", "input": "hi", "reasoning": {"effort": "low"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[1].body).unwrap();
    assert_eq!(sent["reasoning"]["effort"], "low");
}
