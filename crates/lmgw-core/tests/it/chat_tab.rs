//! Chat tab end-to-end: thread CRUD + a streamed send dispatched in-process
//! through the gateway egress, against a wiremock upstream. Asserts the relayed
//! SSE events, the persisted assistant turn with token counts, and a `chat`
//! request-log row.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(upstream_base: &str) -> (SharedState, Gw) {
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
            supports_responses: false,
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

    let gw = serve(state.clone()).await;
    (state, gw)
}

#[tokio::test]
async fn chat_send_streams_persists_and_logs() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"timings\":{\"prompt_n\":7,\"prompt_ms\":14.0,\"prompt_per_second\":500.0,\"predicted_n\":2,\"predicted_ms\":8.0,\"predicted_per_second\":250.0,\"cache_n\":0}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();

    // Create a thread and point it at our alias.
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();

    // Stream a reply.
    let resp = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "hi there" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Relayed normalized events.
    assert!(
        body.contains("event: delta"),
        "missing delta events: {body}"
    );
    assert!(
        body.contains("\"text\":\"Hel\""),
        "missing token text: {body}"
    );
    assert!(body.contains("event: usage"), "missing usage event: {body}");
    assert!(body.contains("event: done"), "missing done event: {body}");
    assert!(body.contains("\"completion_tokens\":2"));
    // llama.cpp `timings` are surfaced as a `stats` event and also folded into
    // the final `done` frame (so both carry the server-measured speeds).
    assert!(body.contains("event: stats"), "missing stats event: {body}");
    assert!(
        body.contains("\"prompt_per_second\":500.0"),
        "missing prefill speed: {body}"
    );
    assert_eq!(
        body.matches("\"predicted_per_second\":250.0").count(),
        2,
        "decode speed should appear in both the stats event and the done frame: {body}"
    );

    // Assistant turn persisted with token counts; thread auto-titled.
    let detail: Value = client
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "user + assistant: {detail}");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "Hello");
    assert_eq!(msgs[1]["prompt_tokens"], 7);
    assert_eq!(msgs[1]["completion_tokens"], 2);
    assert_eq!(detail["thread"]["title"], "hi there");

    // Logged as a `chat` request.
    let mut tries = 0;
    let log = loop {
        let logs = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if let Some(l) = logs.iter().find(|l| l.ingress_proto == "chat") {
            break l.clone();
        }
        tries += 1;
        assert!(tries < 50, "chat log row never appeared");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(log.streamed);
    assert_eq!(log.status, 200);
    assert_eq!(log.requested_alias, "my-model");
    assert_eq!(log.prompt_tokens, Some(7));
    assert_eq!(log.completion_tokens, Some(2));
    assert!(log.error_kind.is_none());
}

#[tokio::test]
async fn chat_send_unknown_model_emits_error_frame() {
    let (_state, base) = {
        // No upstream/alias needed; resolution fails before any HTTP.
        let state = AppState::init_for_tests().await.unwrap();
        let gw = serve(state.clone()).await;
        (state, gw)
    };
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "ghost" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();

    let body = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "hello" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("event: error"),
        "expected error frame: {body}"
    );
    assert!(body.contains("unknown model alias"), "{body}");
    assert!(body.contains("event: done"));
}

#[tokio::test]
async fn chat_send_streams_and_persists_reasoning() {
    // A reasoning model: thoughts arrive in `delta.reasoning_content` (with
    // `content` null) before the answer text.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Th\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"ink\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"42\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();

    let body = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "q" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Reasoning is relayed on its own channel, distinct from the answer.
    assert!(
        body.contains("event: reasoning"),
        "missing reasoning event: {body}"
    );
    assert!(
        body.contains("\"text\":\"Th\""),
        "missing thought text: {body}"
    );
    assert!(
        body.contains("event: delta"),
        "missing answer delta: {body}"
    );
    assert!(
        body.contains("\"text\":\"42\""),
        "missing answer text: {body}"
    );

    // Persisted: answer in `content`, thoughts in `reasoning`.
    let detail: Value = client
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let asst = &detail["messages"][1];
    assert_eq!(asst["role"], "assistant");
    assert_eq!(asst["content"], "42");
    assert_eq!(asst["reasoning"], "Think");
}

#[tokio::test]
async fn chat_reasoning_only_turn_is_not_blank() {
    // Model spends its whole budget thinking and never emits an answer: the
    // turn must still be recorded (reasoning), not a blank assistant message.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"still thinking\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":8}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();
    let _ = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "q" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let detail: Value = client
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let asst = &detail["messages"][1];
    assert_eq!(asst["content"], "", "no answer was produced");
    assert_eq!(
        asst["reasoning"], "still thinking",
        "reasoning must persist"
    );
}

// ---------------------------------------------------------------------------
// Admin Chat (§21 stage 2)
// ---------------------------------------------------------------------------

use lmgw_core::config::{SelfAdmin, Settings};

/// Set the self-admin mode through the store, the way the Settings page does —
/// a hand-swapped snapshot would be discarded by the first `reload_snapshot`.
async fn set_self_admin(state: &SharedState, mode: SelfAdmin) {
    let s = Settings {
        self_admin: mode,
        ..state.snapshot().settings.clone()
    };
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn new_thread(base: &Gw, kind: &str) -> i64 {
    base.client()
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "kind": kind}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// The whole Admin Chat loop: the model calls a real `lmgw__*` tool, the
/// gateway runs it in-process, streams the activity, and persists the turn's IR
/// so the *next* turn still knows what happened.
#[tokio::test]
async fn admin_chat_runs_a_self_admin_tool_and_persists_the_turn() {
    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"lmgw__status\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    for (i, body) in [call, answer].into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&mock)
            .await;
    }
    let (state, base) = setup(&mock.uri()).await;
    set_self_admin(&state, SelfAdmin::ReadOnly).await;
    let tid = new_thread(&base, "admin").await;

    let body = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "how is the gateway doing?"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("event: tool"), "no tool events: {body}");
    assert!(body.contains("lmgw__status"), "{body}");
    assert!(body.contains("\"event\":\"result\""), "{body}");
    assert!(body.contains("all good"), "{body}");
    // Both turns are counted, not just the last one.
    assert!(body.contains("\"prompt_tokens\":31"), "{body}");

    let detail: Value = base
        .client()
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "all good");
    // The tool exchange is stored as IR, so replaying the thread does not make
    // the model forget the call it just made.
    let ir = msgs[1]["ir_messages"].as_str().expect("ir_messages stored");
    assert!(ir.contains("tool_use"), "{ir}");
    assert!(ir.contains("tool_result"), "{ir}");
    // …and the final answer is not duplicated into it.
    assert!(!ir.contains("all good"), "{ir}");

    // The self-admin call is logged as a tool call, distinct from the turns.
    let mut tries = 0;
    loop {
        let logs = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if logs.iter().any(|l| {
            l.ingress_proto == "admin-tool" && l.mcp_tool.as_deref() == Some("lmgw__status")
        }) {
            assert!(logs.iter().any(|l| l.ingress_proto == "admin"));
            break;
        }
        tries += 1;
        assert!(tries < 50, "admin tool log row never appeared: {logs:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The mode gate is the mode gate: with self-admin off, Admin Chat says so
/// instead of quietly becoming a normal chat that hallucinates about the
/// gateway.
#[tokio::test]
async fn admin_chat_with_self_admin_off_refuses_up_front() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    set_self_admin(&state, SelfAdmin::Off).await;
    let tid = new_thread(&base, "admin").await;

    let body = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "status?"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("Self-admin"), "{body}");
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "the model must not be called at all"
    );
}

/// A plain thread is untouched by any of this: no tools offered, no `admin`
/// log rows, and no `ir_messages`.
#[tokio::test]
async fn a_plain_thread_is_not_given_self_admin_tools() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    set_self_admin(&state, SelfAdmin::Full).await;
    let tid = new_thread(&base, "chat").await;

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "hello"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(
        sent.get("tools").is_none(),
        "a plain chat must not be handed tools: {sent:#}"
    );
}

// ---------------------------------------------------------------------------
// MCP servers attached to a thread (§21 stage 3)
// ---------------------------------------------------------------------------

use lmgw_core::config::McpTransport;
use lmgw_core::store::NewMcpServer;

use crate::common;
use common::{serve, Gw};

/// A minimal MCP Streamable-HTTP server exposing two tools, so the tests can
/// tell "the thread got the server" from "the thread got the tools it picked".
/// Same shape as the `/v1/responses` suite's stub — deliberately, since both
/// planes resolve through one `mcp::exec::resolve`.
async fn mcp_stub() -> String {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let handler = move |body: String| async move {
        let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let Some(id) = req.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response();
        };
        let tool = |name: &str| {
            json!({
                "name": name,
                "description": format!("{name} the input"),
                "inputSchema": {"type": "object",
                                "properties": {"text": {"type": "string"}}},
            })
        };
        let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": req.pointer("/params/protocolVersion")
                    .cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stub", "version": "0.1.0"},
            }),
            "tools/list" => json!({"tools": [tool("echo"), tool("ping")]}),
            "tools/call" => {
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
        (
            StatusCode::OK,
            [
                ("content-type", "application/json"),
                ("mcp-session-id", "stub-session"),
            ],
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        )
            .into_response()
    };

    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

async fn register_mcp(state: &SharedState, prefix: &str, url: &str) {
    store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "stub-server".into(),
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

async fn patch_settings(base: &Gw, tid: i64, body: Value) {
    base.client()
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&body)
        .send()
        .await
        .unwrap();
}

/// The gap this closes: a registered MCP server was reachable from
/// `/v1/responses` and `/mcp`, but never from the dashboard's own chat. An
/// ordinary thread that attaches one runs its tools through the same loop.
#[tokio::test]
async fn a_thread_with_an_mcp_server_runs_its_tools() {
    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"stub__echo\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"function\":{\"arguments\":\"{\\\"text\\\":\\\"hi\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"it said hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    for (i, body) in [call, answer].into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&mock)
            .await;
    }
    let (state, base) = setup(&mock.uri()).await;
    let stub = mcp_stub().await;
    register_mcp(&state, "stub", &stub).await;
    let tid = new_thread(&base, "chat").await;
    patch_settings(&base, tid, json!({"mcp_tools": [{"server_label": "stub"}]})).await;

    let body = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "say hi through the tool"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("stub__echo"), "no tool call: {body}");
    assert!(body.contains("echoed"), "tool never ran: {body}");
    assert!(body.contains("it said hi"), "{body}");

    // The whole server's surface, because the thread narrowed nothing.
    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["stub__echo", "stub__ping"], "{sent:#}");

    // The turn is stored as IR, so the next turn remembers the call.
    let detail: Value = base
        .client()
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msgs = detail["messages"].as_array().unwrap();
    let ir = msgs[1]["ir_messages"].as_str().expect("ir_messages stored");
    assert!(
        ir.contains("tool_use") && ir.contains("tool_result"),
        "{ir}"
    );

    // Logged as a chat tool call — a row of its own, apart from the turns.
    let mut tries = 0;
    loop {
        let logs = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if logs
            .iter()
            .any(|l| l.ingress_proto == "chat-tool" && l.mcp_tool.as_deref() == Some("stub__echo"))
        {
            assert!(logs.iter().any(|l| l.ingress_proto == "chat"));
            break;
        }
        tries += 1;
        assert!(tries < 50, "chat tool log row never appeared: {logs:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// `allowed_tools` is honored, so a thread pays prompt tokens only for the
/// tools it picked.
#[tokio::test]
async fn a_thread_can_narrow_a_server_to_some_of_its_tools() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    let stub = mcp_stub().await;
    register_mcp(&state, "stub", &stub).await;
    let tid = new_thread(&base, "chat").await;
    patch_settings(
        &base,
        tid,
        json!({"mcp_tools": [{"server_label": "stub", "allowed_tools": ["stub__echo"]}]}),
    )
    .await;

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "hello"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["stub__echo"], "{sent:#}");
}

/// An attached server that cannot be reached is *named*, not silently dropped:
/// "the model ignored my search tool" is otherwise indistinguishable from "the
/// server never connected".
#[tokio::test]
async fn an_unresolvable_server_is_reported_instead_of_ignored() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "chat").await;
    patch_settings(
        &base,
        tid,
        json!({"mcp_tools": [{"server_label": "ghost"}]}),
    )
    .await;

    let body = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "search for something"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: error"), "{body}");
    assert!(
        body.contains("ghost"),
        "the failed server must be named: {body}"
    );
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "with no tools resolved the model must not be called"
    );
}

/// The settings endpoint is a **patch**: the header's model picker and the
/// settings drawer each send their own half, and neither may blank the other's.
#[tokio::test]
async fn thread_settings_patch_leaves_absent_fields_alone() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "chat").await;
    let get = || async {
        base.client()
            .get(format!("{base}/chat/api/threads/{tid}"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["thread"]
            .clone()
    };

    // The drawer: prompt, sampling and servers, but no model.
    patch_settings(
        &base,
        tid,
        json!({"system_prompt": "be brief", "temperature": 0.4, "max_tokens": 128,
               "mcp_tools": [{"server_label": "stub"}]}),
    )
    .await;
    let t = get().await;
    assert_eq!(
        t["model_alias"], "my-model",
        "the model must survive: {t:#}"
    );

    // The header's model picker: only the alias.
    patch_settings(&base, tid, json!({"model_alias": "my-model"})).await;
    let t = get().await;
    assert_eq!(t["system_prompt"], "be brief", "{t:#}");
    assert_eq!(t["temperature"], 0.4, "{t:#}");
    assert_eq!(t["max_tokens"], 128, "{t:#}");
    assert_eq!(t["mcp_tools"][0]["server_label"], "stub", "{t:#}");

    // …and `null` still clears a sampling setting (it was sent, not omitted).
    patch_settings(&base, tid, json!({"temperature": null})).await;
    let t = get().await;
    assert!(t["temperature"].is_null(), "{t:#}");
    assert_eq!(t["max_tokens"], 128, "{t:#}");
}

#[tokio::test]
async fn chat_replays_a_persisted_trace_on_the_next_send() {
    // The thoughts a reasoning model produced on turn one must go back to it
    // on turn two as `reasoning_content` — llama-server's `--reasoning-preserve`
    // renders only what the request carries. The plain chat path keeps the
    // trace in its own column, so this is the column round-tripping.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Think\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"42\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(2)
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();
    for q in ["q", "and again?"] {
        // Read the whole stream: the turn is persisted when it ends, and
        // dropping the body early is a client hang-up that aborts it.
        let body = client
            .post(format!("{base}/chat/api/threads/{tid}/send"))
            .json(&json!({ "content": q }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("event: done"), "{body}");
    }

    let seen = mock.received_requests().await.unwrap();
    assert_eq!(seen.len(), 2);
    let second: Value = serde_json::from_slice(&seen[1].body).unwrap();
    // The turns, past the thread's default system prompt.
    assert_eq!(second["messages"][0]["role"], "system", "{second}");
    let msgs = &second["messages"].as_array().unwrap()[1..];
    assert_eq!(msgs.len(), 3, "{second}");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "42");
    assert_eq!(msgs[1]["reasoning_content"], "Think");
    // The user turns carry no such field.
    assert!(msgs[0].get("reasoning_content").is_none());
    assert!(msgs[2].get("reasoning_content").is_none());
}

/// A tool turn's record holds the calls and their results, and its final
/// answer lives in `content` alone — so the next turn must replay both, or the
/// model never sees what it answered. Text said *before* a call is in the
/// record already and must not come back twice.
#[tokio::test]
async fn a_tool_turn_replays_its_final_answer_on_the_next_send() {
    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Checking. \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"lmgw__status\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let plain = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"fine\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    for (i, body) in [call, answer, plain].into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&mock)
            .await;
    }
    let (state, base) = setup(&mock.uri()).await;
    set_self_admin(&state, SelfAdmin::ReadOnly).await;
    let tid = new_thread(&base, "admin").await;
    for q in ["how is it?", "and now?"] {
        let body = base
            .client()
            .post(format!("{base}/chat/api/threads/{tid}/send"))
            .json(&json!({"content": q}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("event: done"), "{body}");
    }

    let detail: Value = base
        .client()
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["content"], "Checking. all good", "{detail}");
    assert!(
        msgs[1]["ir_messages"].is_string(),
        "a tool turn keeps its record"
    );
    assert!(
        msgs[3]["ir_messages"].is_null(),
        "a turn that ran no tool has no record: {detail}"
    );

    let seen = mock.received_requests().await.unwrap();
    assert_eq!(seen.len(), 3);
    let third: Value = serde_json::from_slice(&seen[2].body).unwrap();
    let texts: Vec<(String, String)> = third["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| {
            (
                m["content"].as_str().unwrap_or_default().to_string(),
                m["tool_calls"].to_string(),
            )
        })
        .collect();
    assert_eq!(texts.len(), 2, "{third}");
    assert_eq!(texts[0].0, "Checking. ", "{third}");
    assert!(texts[0].1.contains("lmgw__status"), "{third}");
    assert_eq!(texts[1].0, "all good", "the final answer, once: {third}");
}
