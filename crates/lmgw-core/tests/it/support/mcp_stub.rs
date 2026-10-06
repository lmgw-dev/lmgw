//! A Streamable-HTTP MCP server for the suites that list a label's tools
//! and call them (realtime-server-tools design §1.2, §1.4, §2.4): the tools
//! it is given, every request counted — a connect is an `initialize`, so a
//! server never named sees none — and, when asked, its `tools/list` held
//! until the test lets it go: a cold server still connecting. Its
//! `tools/call`s are kept in arrival order and answered by the test's
//! [`Answer`] (by default `echo`'s: `"echo: <text>"`) — or held until the
//! test lets them go ([`held_call_stub`]): a call still running.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use lmgw_core::config::McpTransport;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewMcpServer};
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

/// How a `tools/call` is answered: its tool's name and arguments → the
/// `CallToolResult`, or `null` for an HTTP 500 instead.
pub type Answer = Arc<dyn Fn(String, Value) -> BoxFuture<'static, Value> + Send + Sync>;

pub struct McpStub {
    pub url: String,
    /// Every request it got.
    pub hits: Arc<AtomicUsize>,
    /// Every `tools/call`: the tool's name and its arguments, in arrival
    /// order.
    pub calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// Lets a held `tools/list` answer (`held`).
    release: watch::Sender<bool>,
}

impl McpStub {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// The `tools/call`s so far.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    /// A held `tools/list` answers now, and every later one at once.
    pub fn release(&self) {
        self.release.send_replace(true);
    }

    /// Until it has seen `n` `tools/call`s; fails after 5 s.
    pub async fn wait_calls(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.calls.lock().unwrap().len() < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server saw {} tools/call, not {n}",
                self.calls.lock().unwrap().len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// One tool, `echo`, answered at once.
pub async fn echo_stub() -> McpStub {
    stub(echo_tools(), false).await
}

/// `echo`, its `tools/list` held until [`McpStub::release`].
pub async fn held_stub() -> McpStub {
    stub(echo_tools(), true).await
}

/// `echo`, every `tools/call` answered only once the test calls
/// `notify_one` on the gate returned — one call per permit. Never let go, a
/// call runs until the gateway drops it.
pub async fn held_call_stub() -> (McpStub, Arc<Notify>) {
    let gate = Arc::new(Notify::new());
    let held = gate.clone();
    let slow = answer(move |name, args| {
        let held = held.clone();
        async move {
            held.notified().await;
            (echo_answer())(name, args).await
        }
    });
    (answering(echo_tools(), false, slow).await, gate)
}

fn echo_tools() -> Value {
    json!([{
        "name": "echo",
        "description": "echo the input",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
    }])
}

pub async fn stub(tools: Value, held: bool) -> McpStub {
    answering(tools, held, echo_answer()).await
}

/// An [`Answer`] from an async function of the tool's name and arguments.
pub fn answer<F, Fut>(f: F) -> Answer
where
    F: Fn(String, Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Value> + Send + 'static,
{
    Arc::new(move |name, args| -> BoxFuture<'static, Value> { Box::pin(f(name, args)) })
}

/// `echo`'s answer: its `text` back, as one text block.
pub fn echo_answer() -> Answer {
    answer(|_name, args| async move {
        let text = args["text"].as_str().unwrap_or_default().to_string();
        json!({"content": [{"type": "text", "text": format!("echo: {text}")}],
               "isError": false})
    })
}

/// A server with `tools`, its `tools/list` held when `held`, answering each
/// `tools/call` with `answer`.
pub async fn answering(tools: Value, held: bool, answer: Answer) -> McpStub {
    let hits = Arc::new(AtomicUsize::new(0));
    let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let (release, released) = watch::channel(!held);
    let counter = hits.clone();
    let seen = calls.clone();
    let handler = move |body: String| {
        let counter = counter.clone();
        let tools = tools.clone();
        let mut released = released.clone();
        let seen = seen.clone();
        let answer = answer.clone();
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
                    "serverInfo": {"name": "stub", "version": "0.1.0"},
                }),
                "tools/list" => {
                    let _ = released.wait_for(|r| *r).await;
                    json!({"tools": tools})
                }
                "tools/call" => {
                    let name = req
                        .pointer("/params/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let args = req
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    seen.lock().unwrap().push((name.clone(), args.clone()));
                    let result = answer(name, args).await;
                    if result.is_null() {
                        // A server that fails the call outright.
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    result
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
    McpStub {
        url: format!("http://{addr}/mcp"),
        hits,
        calls,
        release,
    }
}

/// A URL nothing answers on: a server that cannot be connected.
pub async fn dead_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/mcp")
}

/// Register an HTTP MCP server `name` with tool prefix `prefix` at `url`,
/// `agent` naming the service agent it belongs to, if any.
pub async fn register(
    state: &SharedState,
    name: &str,
    prefix: &str,
    url: &str,
    enabled: bool,
    agent: Option<&str>,
) {
    store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: name.into(),
            enabled,
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
            agent_id: agent.map(str::to_string),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
}
