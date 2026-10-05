//! Golden SSE transcripts of every Chat turn route (chat-voice design §7.1,
//! WP2): the body a route streams, byte for byte, against mock upstreams —
//! send, regenerate (a reply, a user message), edit, continue, an Admin Chat
//! and an MCP tool thread, a temporary chat, an auto-mode knowledge
//! retrieval, and the error and `done` payloads of the ways a turn fails or
//! is stopped.
//!
//! Captured before the turn seam (`TurnFrame`) replaced the SSE channel, and
//! kept so the wire cannot drift unnoticed. The transcripts are
//! `tests/fixtures/chat_sse/<name>.sse`. Only wall-clock values are
//! normalized ([`normalize`]); everything else, ids included, is compared as
//! it comes. `LMGW_BLESS=1` rewrites the files from what the routes stream
//! now (the diff then shows what changed); without it, a missing file fails.

use std::time::Duration;

use lmgw_core::config::{McpTransport, Protocol, SelfAdmin, Settings, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewMcpServer, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, gateway_at, openai_sse, post};
use crate::common::{serve, Gw};
use crate::knowledge as kbfix;

// ---------------------------------------------------------------------------
// The golden files
// ---------------------------------------------------------------------------

/// `body` with its wall-clock values zeroed: `ttfb_ms`, `total_ms`, and the
/// `ms` of a tool result or a retrieval. A `null` stays `null`.
pub(crate) fn normalize(body: &str) -> String {
    let mut out = body.to_string();
    for key in ["\"ttfb_ms\":", "\"total_ms\":", "\"ms\":"] {
        let mut s = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.find(key) {
            let (head, tail) = rest.split_at(at + key.len());
            s.push_str(head);
            let n = tail
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(tail.len());
            if n > 0 {
                s.push('0');
            }
            rest = &tail[n..];
        }
        s.push_str(rest);
        out = s;
    }
    out
}

/// Compare `body` (normalized) with the golden transcript `name`.
fn golden(name: &str, body: &str) {
    let got = normalize(body);
    let file = format!(
        "{}/tests/fixtures/chat_sse/{name}.sse",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("LMGW_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(&file).parent().unwrap()).unwrap();
        std::fs::write(&file, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("{file}: {e} — run the suite with LMGW_BLESS=1 to capture it"));
    assert!(
        got == want,
        "the SSE transcript '{name}' changed\n--- want ({file})\n{want}\n--- got\n{got}"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A reasoning model's streamed answer with llama.cpp timings and usage:
/// every frame a plain turn can relay.
const FULL_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me think.\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"timings\":{\"prompt_n\":7,\"prompt_ms\":14.0,\"prompt_per_second\":500.0,\"predicted_n\":2,\"predicted_ms\":8.0,\"predicted_per_second\":250.0,\"cache_n\":0}}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n"
);

fn sse_reply(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into(), "text/event-stream")
}

/// Mount `bodies` as the answers to successive `/chat/completions` calls.
async fn mount_in_order(mock: &MockServer, bodies: &[String]) {
    for (i, body) in bodies.iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_reply(body.clone()))
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

async fn thread(gw: &Gw, extra: Value) -> i64 {
    let mut body = json!({"model_alias": "m"});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    post(gw, "/chat/api/threads", body)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// POST `route` and read the whole SSE body.
async fn stream(gw: &Gw, route: &str, body: Value) -> String {
    let r = post(gw, route, body).await;
    assert_eq!(r.status(), 200, "{route}");
    r.text().await.unwrap()
}

async fn send(gw: &Gw, tid: i64, content: &str) -> String {
    stream(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": content}),
    )
    .await
}

async fn save_settings(state: &SharedState, edit: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    edit(&mut s);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// One streamed tool call `name` with `args` (and the text before it),
/// OpenAI-shaped: the call's start, then its arguments as a second fragment.
pub(crate) fn openai_call(before: &str, id: &str, name: &str, args: &str) -> String {
    let mut s = String::new();
    if !before.is_empty() {
        s.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": before}}]})
        ));
    }
    s.push_str(&format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": ""}}]}}]})
    ));
    s.push_str(&format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0,
            "function": {"arguments": args}}]}}]})
    ));
    s.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
    ));
    s
}

/// A minimal MCP Streamable-HTTP server with an `echo` tool, which answers
/// at once, and a `slow` one, which takes `slow` to answer.
pub(crate) async fn mcp_stub(slow: Duration) -> String {
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
            "tools/list" => json!({"tools": [tool("echo"), tool("slow")]}),
            "tools/call" => {
                if req.pointer("/params/name").and_then(Value::as_str) == Some("slow") {
                    tokio::time::sleep(slow).await;
                }
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

/// Register the stub as the MCP server whose tools are `stub__*`.
pub(crate) async fn register_stub(state: &SharedState, url: &str) {
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
            tool_prefix: "stub".into(),
            timeout_ms: 60_000,
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

/// A thread of `gw` that attaches the stub's tools.
pub(crate) async fn tool_thread(gw: &Gw) -> i64 {
    let tid = thread(gw, json!({})).await;
    let r = post(
        gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "stub"}]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    tid
}

/// Read `resp`'s SSE body until `needle` has arrived; what came so far.
pub(crate) async fn read_until(resp: &mut reqwest::Response, needle: &str) -> String {
    let mut body = String::new();
    while !body.contains(needle) {
        let chunk = tokio::time::timeout(Duration::from_secs(20), resp.chunk())
            .await
            .unwrap_or_else(|_| panic!("'{needle}' never arrived: {body}"))
            .unwrap()
            .unwrap_or_else(|| panic!("the stream ended before '{needle}': {body}"));
        body.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    body
}

// ---------------------------------------------------------------------------
// Plain turns
// ---------------------------------------------------------------------------

/// Send, then regenerate the reply, regenerate the user message, and edit
/// it: four turns over one thread, each its own transcript.
#[tokio::test]
async fn send_regenerate_and_edit() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_reply(FULL_SSE))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;

    golden("send", &send(&gw, tid, "hi there").await);
    // Thread: [1 user, 2 reply].
    golden(
        "regenerate_reply",
        &stream(
            &gw,
            &format!("/chat/api/threads/{tid}/messages/2/regenerate"),
            json!({}),
        )
        .await,
    );
    // [1 user, 3 reply].
    golden(
        "regenerate_user",
        &stream(
            &gw,
            &format!("/chat/api/threads/{tid}/messages/1/regenerate"),
            json!({}),
        )
        .await,
    );
    // [1 user, 4 reply].
    golden(
        "edit_user",
        &stream(
            &gw,
            &format!("/chat/api/threads/{tid}/messages/1/edit"),
            json!({"content": "hello again"}),
        )
        .await,
    );
}

/// A continue on llama-server: the deltas are the continuation only, and
/// `done` names the continued row.
#[tokio::test]
async fn continue_a_reply() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_reply(openai_sse(" and more.", 10, 3)))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    store::append_chat_message(&state.db, tid, "user", "q", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "half an answer",
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    golden(
        "continue",
        &stream(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({})).await,
    );
}

/// A temporary chat: negative ids, the same frames.
#[tokio::test]
async fn a_temporary_chat() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_reply(openai_sse("hello there", 5, 2)))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({"temporary": true})).await;
    assert!(tid < 0);
    golden("temporary", &send(&gw, tid, "hi").await);
}

/// Auto mode: the `retrieval` frame between `turn` and the answer.
#[tokio::test]
async fn an_auto_mode_retrieval() {
    let kb_mock = MockServer::start().await;
    kbfix::mount(&kb_mock).await;
    let state = kbfix::setup(&kb_mock).await;
    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_reply(openai_sse("It arrived in May [1].", 10, 5)))
        .mount(&chat)
        .await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "chat-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: chat.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
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
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "chat-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    let taxes = kbfix::ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
        &[("notes.md", kbfix::NOTES.as_bytes().to_vec())],
    )
    .await;
    let tid = thread(&gw, json!({})).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"kb_ids": [taxes]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    golden(
        "kb_auto",
        &send(&gw, tid, "When did the refund arrive?").await,
    );
    kbfix::cleanup(&state);
}

// ---------------------------------------------------------------------------
// Tool threads
// ---------------------------------------------------------------------------

/// An MCP tool thread: reasoning and text before the call, the call's
/// `start` / `args` / `ready` / `result`, then the answer.
#[tokio::test]
async fn an_mcp_tool_thread() {
    let mock = MockServer::start().await;
    let first = format!(
        "data: {}\n\n{}",
        json!({"choices": [{"delta": {"reasoning_content": "Need the tool."}}]}),
        openai_call("Checking. ", "c1", "stub__echo", "{\"text\":\"hi\"}")
    );
    mount_in_order(&mock, &[first, openai_sse("it said hi", 20, 4)]).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;
    golden(
        "tool_thread",
        &send(&gw, tid, "say hi through the tool").await,
    );
}

/// An Admin Chat thread running a real `lmgw__*` tool in-process.
#[tokio::test]
async fn an_admin_chat() {
    let mock = MockServer::start().await;
    mount_in_order(
        &mock,
        &[
            openai_call("", "c1", "lmgw__mcp_servers", "{}"),
            openai_sse("no servers yet", 30, 3),
        ],
    )
    .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    save_settings(&state, |s| s.self_admin = SelfAdmin::ReadOnly).await;
    let tid = thread(&gw, json!({"kind": "admin"})).await;
    golden(
        "admin",
        &send(&gw, tid, "which MCP servers are there?").await,
    );
}

/// The tool-call budget runs out: the loop's `error`, then `done`.
#[tokio::test]
async fn a_tool_thread_out_of_budget() {
    let mock = MockServer::start().await;
    let two_calls = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "c1", "type": "function",
             "function": {"name": "stub__echo", "arguments": "{\"text\":\"a\"}"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 1, "id": "c2", "type": "function",
             "function": {"name": "stub__echo", "arguments": "{\"text\":\"b\"}"}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    );
    mount_in_order(&mock, &[two_calls]).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    save_settings(&state, |s| s.responses_max_tool_calls = 1).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;
    golden("tool_budget", &send(&gw, tid, "echo twice").await);
}

/// An attached server that does not resolve: named, then the refusal.
#[tokio::test]
async fn a_tool_thread_with_no_usable_server() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "ghost"}]}),
    )
    .await;
    golden("tool_unresolvable", &send(&gw, tid, "search").await);
}

/// The model's upstream refuses a tool thread's first call.
#[tokio::test]
async fn a_tool_thread_whose_upstream_fails() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "the model fell over"}})),
        )
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;
    golden("tool_upstream_error", &send(&gw, tid, "hi").await);
}

/// A newer send replaces a tool turn whose tool is still running: the
/// abandoned call's result, the loop's `error`, `superseded`, `done`.
#[tokio::test]
async fn a_tool_turn_superseded_mid_tool() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("take your time"))
        .respond_with(sse_reply(openai_call(
            "",
            "c1",
            "stub__slow",
            "{\"text\":\"zzz\"}",
        )))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("never mind"))
        .respond_with(sse_reply(openai_sse("fine", 3, 1)))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::from_secs(30)).await).await;
    let tid = tool_thread(&gw).await;

    let mut first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "take your time"}),
    )
    .await;
    let mut body = read_until(&mut first, "\"event\":\"ready\"").await;
    let second = send(&gw, tid, "never mind").await;
    body.push_str(&first.text().await.unwrap());
    golden("tool_superseded", &body);
    golden("tool_superseding", &second);
}

// ---------------------------------------------------------------------------
// Failures and stops
// ---------------------------------------------------------------------------

/// An alias that does not resolve: `error`, `done {aborted}`.
#[tokio::test]
async fn an_unknown_model() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "ghost"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    golden("unknown_model", &send(&gw, tid, "hello").await);
}

/// The upstream answers 500.
#[tokio::test]
async fn an_upstream_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "the model fell over"}})),
        )
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    golden("upstream_error", &send(&gw, tid, "hello").await);
}

/// A newer send replaces a turn still waiting for its first token.
#[tokio::test]
async fn a_turn_superseded_before_its_first_token() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("second question"))
        .respond_with(sse_reply(openai_sse("in time", 5, 2)))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("first question"))
        .respond_with(sse_reply(openai_sse("too late", 5, 2)).set_delay(Duration::from_secs(5)))
        .with_priority(2)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    let mut first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "first question"}),
    )
    .await;
    let mut body = read_until(&mut first, "event: turn").await;
    // Long enough for the first turn's request to be on its way.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = send(&gw, tid, "second question").await;
    body.push_str(&first.text().await.unwrap());
    golden("superseded", &body);
    golden("superseding", &second);
}

/// An upstream whose answer to every chat call is `body`, held back: it says
/// when a request arrived (`received`), and answers only once `release` is
/// notified.
async fn held_answer(
    body: String,
    received: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
) -> String {
    let handler = move || {
        let (body, received, release) = (body.clone(), received.clone(), release.clone());
        async move {
            received.notify_one();
            release.notified().await;
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A reply whose row was edited behind the live-turn registry's back: the
/// save is refused, `error {not_saved}`, `done {saved: false}`.
///
/// The edit lands between the continue reading its row and saving onto it
/// by construction, not by timing: the upstream has the continue's request
/// (so the row was read and the turn is live) and answers only after the
/// edit.
#[tokio::test]
async fn a_refused_save() {
    let received = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let base = held_answer(
        openai_sse(" and more.", 10, 2),
        received.clone(),
        release.clone(),
    )
    .await;
    let (state, gw) = gateway_at(&base, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    store::append_chat_message(&state.db, tid, "user", "q", "", None, None, None)
        .await
        .unwrap();
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "half an answer",
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let gw2 = gw.clone();
    let cont = tokio::spawn(async move {
        stream(
            &gw2,
            &format!("/chat/api/threads/{tid}/continue"),
            json!({}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(20), received.notified())
        .await
        .expect("the continue's request reaches the upstream");
    let update = store::ChatMessageUpdate {
        content: "edited by hand".into(),
        ..Default::default()
    };
    assert!(store::update_chat_message(&state.db, tid, rid, &update)
        .await
        .unwrap());
    release.notify_one();
    golden("not_saved", &cont.await.unwrap());
}

#[test]
fn normalize_zeroes_only_wall_clock_values() {
    assert_eq!(
        normalize(
            r#"{"ttfb_ms":12,"total_ms":345,"ms":6,"prompt_ms":14.0,"x":null,"ttfb_ms":null}"#
        ),
        r#"{"ttfb_ms":0,"total_ms":0,"ms":0,"prompt_ms":14.0,"x":null,"ttfb_ms":null}"#
    );
}
