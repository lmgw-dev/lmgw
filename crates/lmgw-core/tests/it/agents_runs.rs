//! Batch runs over the real router (agent-catalog design §2.4, §4, §8): the
//! `agent_run` op, the job the executor drives, the model call it makes and the
//! reads that report it.
//!
//! The model is a wiremock upstream and the tool surface is a small in-test
//! Streamable-HTTP MCP stub, so the whole path runs for real — the op, the job
//! row, the `(kind, key)` guard, `mcp::exec::resolve`, the in-process model
//! call and its `request_logs` row — with no container and no network beyond
//! loopback.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
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
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "GET {path}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
}

/// A gateway with `m1` routed at `upstream_base`.
async fn gateway(upstream_base: &str, protocol: Protocol) -> SharedState {
    gateway_with_timeout(upstream_base, protocol, 5_000).await
}

/// The same, with the upstream's own timeout spelled out — a test that has to
/// tell "the cancel stopped it" from "the stall timeout stopped it" needs the
/// second one far enough away to be ruled out.
async fn gateway_with_timeout(
    upstream_base: &str,
    protocol: Protocol,
    timeout_ms: u64,
) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms,
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
    state
}

/// An MCP stub with the two tools the test agent needs: `search` returns a list
/// of two ids as `structuredContent`, `get` returns one message as JSON text.
/// Counts its `tools/call`s, which is how "refused before any tool call" is
/// checked.
async fn mcp_stub() -> (String, Arc<AtomicUsize>) {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();

    let handler = move |body: String| {
        let calls = counter.clone();
        async move {
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let tool = |name: &str| {
                json!({
                    "name": name,
                    "description": name,
                    "inputSchema": {"type": "object", "properties": {}},
                })
            };
            let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": req.pointer("/params/protocolVersion")
                        .cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "0.1.0"},
                }),
                "tools/list" => json!({"tools": [tool("search"), tool("get")]}),
                "tools/call" => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let name = req
                        .pointer("/params/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name == "search" {
                        json!({
                            "content": [],
                            "structuredContent": {
                                "messages": [{"id": "m1"}, {"id": "m2"}],
                            },
                            "isError": false,
                        })
                    } else {
                        let arg = req
                            .pointer("/params/arguments/messageId")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        json!({
                            "content": [{"type": "text",
                                         "text": json!({"subject": format!("about {arg}")}).to_string()}],
                            "isError": false,
                        })
                    }
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

async fn register_stub(state: &SharedState, url: &str) {
    store::insert_mcp_server(
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
            url: Some(url.into()),
            headers: vec![],
            tool_prefix: "gws".into(),
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

/// The test agent: the mail shape, over the stub's two tools.
fn doc(model_default: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": {{ "alias": "{{{{config.model}}}}", "temperature": 0.0 }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model":      {{ "type": "string", "default": "{model_default}" }},
    "categories": {{ "type": "array", "items": {{ "type": "string" }},
                     "default": ["Work", "Finance"] }}
  }} }} }},
  "tools": [ {{ "label": "gws" }} ],
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search", "args": {{ "q": "is:unread" }} }},
    "items_path": "/messages",
    "item": {{
      "id": "{{{{item.id}}}}",
      "fetch": {{ "tool": "gws__get", "args": {{ "messageId": "{{{{item.id}}}}" }} }},
      "columns": {{ "subject": "{{{{fetched.subject}}}}" }},
      "system": "Pick one of: {{{{config.categories}}}}.",
      "user": "Subject: {{{{fetched.subject}}}}",
      "output": {{ "field": "category", "enum_from": "config.categories",
                   "fallback": "Other" }}
    }},
    "review": {{ "editable": ["category"] }}
  }}
}}"#
    )
}

async fn install(base: &Gw, doc: &str) {
    let resp = base
        .client()
        .post(format!("{base}/api/agents/import?replace=1"))
        .header("content-type", "application/json")
        .body(doc.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "import: {body}");
}

/// Poll the job row until it leaves `queued`/`running`.
async fn settle(state: &SharedState, job_id: i64) -> store::JobRow {
    for _ in 0..400 {
        let row = store::get_job(&state.db, job_id).await.unwrap().unwrap();
        if !matches!(row.status.as_str(), "queued" | "running") {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job_id} never finished");
}

fn openai_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"role": "assistant", "content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 7, "completion_tokens": 3}}),
    )
}

fn anthropic_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: message_delta\ndata: {}\n\n\
         event: message_stop\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    )
}

async fn sent_bodies(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .collect()
}

fn rows(detail: &Value) -> Vec<Value> {
    detail["rows"].as_array().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The model call (§4.2, §8)
// ---------------------------------------------------------------------------

/// On an OpenAI-protocol route the upstream receives `response_format` with the
/// enum — that is the whole point of moving the classify call onto the
/// in-process path instead of the mail workflow's raw POST.
#[tokio::test]
async fn an_openai_route_receives_the_response_format_with_the_enum() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(openai_sse(r#"{"category":"Finance"}"#), "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let state = gateway(&mock.uri(), Protocol::Openai).await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let job_id = res["job_id"].as_i64().unwrap();
    let row = settle(&state, job_id).await;
    assert_eq!(row.status, "done", "{:?}", row.error);

    let bodies = sent_bodies(&mock).await;
    assert_eq!(bodies.len(), 2, "one classify call per item");
    let rf = &bodies[0]["response_format"];
    assert_eq!(rf["type"], json!("json_schema"), "{}", bodies[0]);
    assert_eq!(
        rf["json_schema"]["schema"]["properties"]["category"]["enum"],
        json!(["Work", "Finance", "Other"]),
        "the fallback is appended once and last"
    );
    // The model's sampling knobs came from the manifest, and no `max_tokens`
    // was invented for it.
    assert_eq!(bodies[0]["temperature"], json!(0.0));
    assert!(bodies[0].get("max_tokens").is_none(), "{}", bodies[0]);

    let detail = get_json(&base, &format!("/api/agents/runs/{job_id}")).await;
    let rows = rows(&detail);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], json!("m1"));
    assert_eq!(rows[0]["columns"]["subject"], json!("about m1"));
    assert_eq!(rows[0]["output"], json!({"category": "Finance"}));
    assert_eq!(rows[0]["attention"], json!(false));
    // §4.5: the run carries its own total.
    assert_eq!(detail["result"]["model_calls"], json!(2));
    assert_eq!(detail["result"]["usage"]["prompt_tokens"], json!(14));
    // …and the review table can be drawn from the run alone.
    assert_eq!(detail["batch"]["columns"], json!(["subject"]));
    assert_eq!(detail["batch"]["editable"][0]["field"], json!("category"));

    // Every model turn is a Logs row under the agents identity (§4.2).
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    let turns: Vec<_> = logs.iter().filter(|l| l.ingress_proto == "agent").collect();
    assert_eq!(turns.len(), 2, "{logs:#?}");
    let tools: Vec<_> = logs
        .iter()
        .filter(|l| l.ingress_proto == "agent-tool")
        .collect();
    assert_eq!(tools.len(), 3, "one source call plus one fetch per item");
}

/// On an Anthropic route the egress drops `response_format`, so the reply is
/// matched back onto the enum the way the mail workflow did (§4.2).
#[tokio::test]
async fn an_anthropic_route_gets_no_response_format_and_the_reply_is_matched() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    anthropic_sse("Thinking about it — Finance."),
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let state = gateway(&mock.uri(), Protocol::Anthropic).await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (_, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    let job_id = res["job_id"].as_i64().unwrap();
    let row = settle(&state, job_id).await;
    assert_eq!(row.status, "done", "{:?}", row.error);

    for body in sent_bodies(&mock).await {
        assert!(
            body.get("response_format").is_none(),
            "the Anthropic egress must drop it: {body}"
        );
    }
    let detail = get_json(&base, &format!("/api/agents/runs/{job_id}")).await;
    assert_eq!(rows(&detail)[0]["output"], json!({"category": "Finance"}));
    assert_eq!(rows(&detail)[0]["attention"], json!(false));
}

// ---------------------------------------------------------------------------
// Guard rails (§8) — the two the mail workflow's tests pinned, ported
// ---------------------------------------------------------------------------

/// A run whose model cannot be resolved is refused **before any tool call**, so
/// a misconfigured agent never touches the outside world.
#[tokio::test]
async fn a_classify_without_a_model_is_refused_before_any_tool_call() {
    let mock = MockServer::start().await;
    let state = gateway(&mock.uri(), Protocol::Openai).await;
    let (url, calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    // The config's model default names an alias this gateway does not serve.
    install(&base, &doc("ghost-model")).await;

    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let row = settle(&state, res["job_id"].as_i64().unwrap()).await;
    assert_eq!(row.status, "failed");
    let err = row.error.unwrap_or_default();
    assert!(err.contains("ghost-model"), "{err}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the source step ran despite an unresolvable model"
    );

    // A list-only run of the same agent works: it needs no model at all, which
    // is the point of it (§2.4).
    let (_, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "list" }),
    )
    .await;
    let row = settle(&state, res["job_id"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    assert!(calls.load(Ordering::SeqCst) >= 3);
}

/// An unknown op is named as such rather than 404ing generically, and an
/// unknown *phase* names the ones there are.
#[tokio::test]
async fn an_unknown_op_and_an_unknown_phase_are_both_named() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    install(&base, &doc("m1")).await;

    let (status, body) = op(&base, "agent_nope", json!({})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown op"),
        "{body}"
    );

    let (status, body) = op(&base, "agent_run", json!({"id": "labeler", "phase": "go"})).await;
    assert_eq!(status, 400, "{body}");
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("unknown phase 'go'"), "{msg}");
    assert!(msg.contains("classify"), "{msg}");

    // A phase at all is required, and the alternatives are named.
    let (status, body) = op(&base, "agent_run", json!({"id": "labeler"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("pass phase"),
        "{body}"
    );

    // Cancelling nothing says so rather than pretending.
    let (status, body) = op(&base, "agent_run_cancel", json!({"id": "labeler"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no run is in flight"),
        "{body}"
    );
}

/// A `chat` agent has no runs; the op says so instead of starting a job that
/// would fail.
#[tokio::test]
async fn a_chat_agent_cannot_be_run() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let (status, body) = op(
        &base,
        "agent_run",
        json!({ "id": "docs-librarian", "phase": "list" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("agent_open_chat"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// One live run per agent (§2.4)
// ---------------------------------------------------------------------------

/// The `(kind, key)` index is the enforcement; a second Start is answered with
/// the job already doing it, not with a duplicate and not with an error.
#[tokio::test]
async fn a_second_start_returns_the_run_already_in_flight() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(openai_sse(r#"{"category":"Work"}"#), "text/event-stream")
                // Slow enough that the second Start lands while the first run
                // is still classifying.
                .set_delay(Duration::from_millis(400)),
        )
        .mount(&mock)
        .await;
    let state = gateway(&mock.uri(), Protocol::Openai).await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (_, first) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    let job_id = first["job_id"].as_i64().unwrap();
    assert_eq!(first["already_running"], json!(false));

    let (status, second) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["already_running"], json!(true), "{second}");
    assert_eq!(second["job_id"], json!(job_id));

    // The live run is on the detail read while it is in flight.
    let detail = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(detail["live_job"]["job_id"], json!(job_id));

    let row = settle(&state, job_id).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    // …and afterwards a fresh Start is a fresh job.
    let (_, third) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "list" }),
    )
    .await;
    assert_ne!(third["job_id"], json!(job_id));
    settle(&state, third["job_id"].as_i64().unwrap()).await;

    let runs = get_json(&base, "/api/agents/labeler/runs").await;
    let runs = runs.as_array().cloned().unwrap_or_default();
    assert_eq!(runs.len(), 2, "both runs are listed, newest first");
    assert_eq!(runs[0]["phase"], json!("list"));
    assert_eq!(runs[1]["phase"], json!("classify"));
}

// ---------------------------------------------------------------------------
// The self-admin tool (§5)
// ---------------------------------------------------------------------------

/// `lmgw__agent_run` starts the read-only phases and refuses apply, because
/// applying an agent's work is a human action (§1, principle 4).
#[tokio::test]
async fn the_self_admin_tool_refuses_to_apply() {
    let state = AppState::init_for_tests().await.unwrap();
    let e = lmgw_core::ops::agent_run(&state, "labeler", "apply")
        .await
        .unwrap_err();
    assert!(e.contains("human action"), "{e}");
    let e = lmgw_core::ops::agent_run(&state, "labeler", "rerun")
        .await
        .unwrap_err();
    assert!(e.contains("'rerun'"), "{e}");
}

// ---------------------------------------------------------------------------
// The live buffer and cancellation (§3, §4.1)
// ---------------------------------------------------------------------------

/// Slow enough that the reads below land while the run is still classifying.
async fn slow_gateway() -> (MockServer, SharedState) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(openai_sse(r#"{"category":"Work"}"#), "text/event-stream")
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&mock)
        .await;
    let state = gateway(&mock.uri(), Protocol::Openai).await;
    (mock, state)
}

/// Poll the run read until it has rows, or give up. Returns the last body.
async fn wait_for_rows(base: &Gw, job_id: i64) -> Value {
    for _ in 0..400 {
        let d = get_json(base, &format!("/api/agents/runs/{job_id}")).await;
        if !rows(&d).is_empty() {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {job_id} never published a row");
}

/// While a run is in flight its rows come from the executor's in-memory buffer
/// — the job's `result` is still NULL — and the same route answers from the
/// result once it has finished. One call, both halves (§3).
#[tokio::test]
async fn a_run_in_flight_serves_its_rows_from_the_live_buffer() {
    let (_mock, state) = slow_gateway().await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (_, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    let job_id = res["job_id"].as_i64().unwrap();

    let live = wait_for_rows(&base, job_id).await;
    assert_eq!(live["job"]["status"], json!("running"), "{live}");
    assert!(
        live["result"].is_null(),
        "nothing is stored yet, so these rows can only be the buffer: {live}"
    );
    assert_eq!(rows(&live)[0]["id"], json!("m1"));

    let row = settle(&state, job_id).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let done = get_json(&base, &format!("/api/agents/runs/{job_id}")).await;
    assert_eq!(rows(&done).len(), 2);
    assert!(!done["result"].is_null(), "and now from the result column");
}

/// Cancel is not destructive: the job row ends `canceled` **with** a result —
/// every row it had, the `canceled` marker and the usage it spent (§4.1).
#[tokio::test]
async fn a_cancelled_run_ends_canceled_and_keeps_its_rows() {
    let (_mock, state) = slow_gateway().await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (_, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    let job_id = res["job_id"].as_i64().unwrap();
    wait_for_rows(&base, job_id).await;

    let (status, body) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{body}");

    let row = settle(&state, job_id).await;
    assert_eq!(row.status, "canceled", "{:?}", row.error);
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap_or("null"))
        .expect("a cancelled run stores its rows");
    assert_eq!(result["canceled"], json!(true), "{result}");
    assert!(
        result["rows"].as_array().is_some_and(|r| !r.is_empty()),
        "the rows it had are kept: {result}"
    );
    assert!(
        result.get("usage").is_some(),
        "…and what it spent: {result}"
    );
    // The buffer is handed over to the result column, not left behind.
    let after = get_json(&base, &format!("/api/agents/runs/{job_id}")).await;
    assert_eq!(after["job"]["status"], json!("canceled"));
    assert!(!rows(&after).is_empty());
}

/// An upstream that *starts* a stream and never finishes it: one SSE frame,
/// then the body stays open. Two observations come back with it — how many
/// calls it has been handed (so a test can wait for one to be genuinely in
/// flight) and whether the client's connection has gone away (so "cancel closed
/// the stream" is observed rather than assumed).
async fn hanging_upstream() -> (String, Arc<AtomicUsize>, Arc<AtomicBool>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let (h, c) = (hits.clone(), closed.clone());

    let handler = move || {
        let (h, c) = (h.clone(), c.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) =
                tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
            let first = format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"role": "assistant", "content": "thinking"}}]})
            );
            tx.send(Ok(axum::body::Bytes::from(first))).await.ok();
            tokio::spawn(async move {
                // Resolves when the response body is dropped, which happens
                // only when the client has gone.
                tx.closed().await;
                c.store(true, Ordering::SeqCst);
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
                .unwrap()
        }
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, closed)
}

/// Poll `cond` every 10 ms for up to two seconds.
async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

/// Cancel while a model call is **in flight**. The classify turn is streaming
/// and will never finish on its own; pressing Cancel abandons that await,
/// closes the upstream connection, and ends the job `canceled` with the rows
/// and tool calls it already had — rather than leaving the owner waiting out
/// the per-turn deadline (§4.1).
#[tokio::test]
async fn a_cancel_reaches_a_streaming_model_call_in_flight() {
    let (upstream, hits, closed) = hanging_upstream().await;
    // The two clocks that could also end this run are minutes away, so a prompt
    // finish can only be the cancel.
    let state = gateway_with_timeout(&upstream, Protocol::Openai, 120_000).await;
    assert!(
        state.snapshot().settings.responses_timeout_seconds >= 60,
        "this case proves cancel beats the deadline, so the deadline has to be far off"
    );
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &doc("m1")).await;

    let (_, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    let job_id = res["job_id"].as_i64().unwrap();
    wait_for_rows(&base, job_id).await;
    until("the classify call never reached the upstream", || {
        hits.load(Ordering::SeqCst) > 0
    })
    .await;

    let (status, body) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{body}");
    let t0 = Instant::now();
    let row = settle(&state, job_id).await;
    let took = t0.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "cancel took {took:?}, which is the deadline ending the turn rather than the cancel"
    );
    assert_eq!(row.status, "canceled", "{:?}", row.error);

    until("the upstream stream was left open", || {
        closed.load(Ordering::SeqCst)
    })
    .await;

    let result: Value = serde_json::from_str(row.result.as_deref().unwrap_or("null"))
        .expect("a cancelled run stores what it did");
    assert_eq!(result["canceled"], json!(true), "{result}");
    assert_eq!(
        result["rows"].as_array().map(Vec::len),
        Some(2),
        "every listed row is still reported: {result}"
    );
    // One source call plus one fetch per item: what it did before the model
    // call it was cut off in.
    assert_eq!(result["tool_calls"], json!(3), "{result}");
    // Abandoning the call by dropping its future skips everything after the
    // await, so the in-flight gauge is closed out by the drop itself or not at
    // all — and "not at all" means the dashboard counts this call forever.
    assert_eq!(
        state.telemetry.stats().active_requests,
        0,
        "the abandoned model call is still counted as in flight"
    );
}

// ---------------------------------------------------------------------------
// Built-in toolsets log as the agent surface (§4.3)
// ---------------------------------------------------------------------------

/// A list-only agent whose source is a built-in `lmgw` tool.
fn builtin_doc() -> String {
    r#"{
  "schema_version": 1,
  "id": "inspector",
  "name": "Inspector",
  "model": { "alias": "m1" },
  "tools": [ { "label": "lmgw", "allowed": ["lmgw__models"] } ],
  "run": { "kind": "batch",
    "source": { "tool": "lmgw__models", "args": {} },
    "items_path": "/models",
    "item": { "id": "{{item.name}}", "columns": { "kind": "{{item.kind}}" } }
  }
}"#
    .to_string()
}

/// A tool call an agent made is an agent's tool call whichever half of the
/// plane served it: the built-in `lmgw` toolset logs under `agent-tool` too,
/// not under Admin Chat's proto (§4.3).
#[tokio::test]
async fn a_builtin_tool_call_logs_under_the_agent_proto() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    install(&base, &builtin_doc()).await;

    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "inspector", "phase": "list" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let row = settle(&state, res["job_id"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);

    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    let call = logs
        .iter()
        .find(|l| l.mcp_tool.as_deref() == Some("lmgw__models"))
        .expect("the built-in call was logged");
    assert_eq!(
        call.ingress_proto, "agent-tool",
        "a built-in toolset must not file the row under another surface"
    );
    assert!(
        !lmgw_core::telemetry::counts_in_token_stats(&call.ingress_proto),
        "a tool call has no tokens and must stay out of the aggregates"
    );
}
