//! MCP Tasks for hosted servers (MCP Tasks design §1, §7), driven by a fake
//! device that runs tasks over a real host link (`TaskDevice`, below):
//!
//! - `wire`: which calls become tasks, `task` and `_meta["lmgw/task"]`, the
//!   refusal of a `required` tool without the capability, the server's
//!   `model-immediate-response`, and the answers a call does not expect;
//! - `follow`: a status notification ends a task at once, polling at the
//!   task's `pollInterval` and at the setting, `input_required`'s held
//!   `tasks/result`, every ending of §1.4's table, the end's request row,
//!   a server with open tasks never reaped;
//! - `link`: a link drop and a takeover end nothing, the next link is polled
//!   at once, a device that forgot the task ends it abandoned, a disabled
//!   key's tasks wait, a restart resumes open rows;
//! - `bridge`: a caller with no thread gets the result inline, a timeout
//!   and a caller that stops waiting send `tasks/cancel`, `/mcp`'s
//!   `tools/list` carries no `execution`;
//! - `reuse`: a server that reuses a task id ends the older row, never the
//!   new task;
//! - `registered`: the same over a registered (Streamable HTTP) server,
//!   polled through the lazy connect ([`http_server`]);
//! - `removed`: a server row removed while a task's call was answered ends
//!   the task at its insert, and a follower whose server row is gone ends
//!   its own;
//! - `cancel`: the cancel from lmgw as WP1 builds it (`cancel_task`, the
//!   owed cancel's follower); its routes are MCP Tasks WP2's;
//! - `thread`: the late result in a Chat thread (MCP Tasks WP2).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use lmgw_core::agent::{ToolExecutor, ToolOutcome};
use lmgw_core::mcp::exec::McpExecutor;
use lmgw_core::proxy::RequestCtx;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::common::patience;
use crate::device_chat::Device;
use crate::mcp_host::{device_row, host_world};
use crate::realtime_chat_thread::World;

mod bridge;
pub(crate) mod cancel;
mod count;
mod follow;
mod link;
mod registered;
mod removed;
mod reuse;
pub(crate) mod thread;
mod wire;

/// A tool as `tools/list` carries it, with `execution.taskSupport` when
/// `support` names one.
pub(crate) fn task_tool(name: &str, support: Option<&str>) -> Value {
    let mut t = json!({"name": name, "description": format!("the {name} tool"),
                       "inputSchema": {"type": "object"}});
    if let Some(s) = support {
        t["execution"] = json!({"taskSupport": s});
    }
    t
}

/// One task the fake device runs.
#[derive(Debug, Clone)]
pub(crate) struct FakeTask {
    pub status: String,
    pub message: Option<String>,
    pub poll_interval: Option<u64>,
    /// The `tasks/result` answer once terminal: `Ok(result)` or
    /// `Err((code, message))`.
    pub payload: Option<Result<Value, (i64, String)>>,
}

impl FakeTask {
    fn json(&self, id: &str) -> Value {
        let mut t = json!({"taskId": id, "status": self.status,
            "createdAt": "2026-10-09T10:00:00Z", "lastUpdatedAt": "2026-10-09T10:00:00Z",
            "ttl": null});
        if let Some(m) = &self.message {
            t["statusMessage"] = json!(m);
        }
        if let Some(p) = self.poll_interval {
            t["pollInterval"] = json!(p);
        }
        t
    }

    fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// What the fake device does and knows; shared by the links of one test, so
/// a device that reconnects still knows its tasks (or, made anew, forgot
/// them).
pub(crate) struct Script {
    /// Declare `capabilities.tasks` in `initialize`.
    pub declares: bool,
    /// Declare `capabilities.tasks.cancel` with it.
    pub cancel_cap: bool,
    /// Answer every `tasks/cancel` with this JSON-RPC error.
    pub cancel_error: Option<(i64, String)>,
    pub tools: Vec<Value>,
    pub tasks: HashMap<String, FakeTask>,
    next: u32,
    /// The `pollInterval` a new task gets.
    pub poll_interval: Option<u64>,
    /// The `model-immediate-response` a new task's answer carries.
    pub immediate: Option<String>,
    /// Answer every `tools/call` with a task, asked for or not.
    pub always_task: bool,
    /// Answer a `tools/call` that asks for a task with a normal result.
    pub never_task: bool,
    /// `tasks/result` requests waiting for their task to end.
    held: Vec<(Value, String)>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            declares: true,
            cancel_cap: true,
            cancel_error: None,
            tools: vec![task_tool("build", Some("required"))],
            tasks: HashMap::new(),
            next: 0,
            poll_interval: None,
            immediate: None,
            always_task: false,
            never_task: false,
            held: Vec::new(),
        }
    }
}

/// Every request the gateway sent, by method.
pub(crate) struct Seen {
    pub calls: mpsc::UnboundedReceiver<Value>,
    pub gets: mpsc::UnboundedReceiver<Value>,
    pub results: mpsc::UnboundedReceiver<Value>,
    pub cancels: mpsc::UnboundedReceiver<Value>,
    pub notes: mpsc::UnboundedReceiver<Value>,
}

struct SeenTx {
    calls: mpsc::UnboundedSender<Value>,
    gets: mpsc::UnboundedSender<Value>,
    results: mpsc::UnboundedSender<Value>,
    cancels: mpsc::UnboundedSender<Value>,
    notes: mpsc::UnboundedSender<Value>,
}

/// A connected fake device that runs tasks.
pub(crate) struct TaskDevice {
    out: mpsc::UnboundedSender<Message>,
    pub script: Arc<Mutex<Script>>,
    pub seen: Seen,
    task: tokio::task::JoinHandle<()>,
}

impl TaskDevice {
    /// Connect with `key` and `script`, and wait until the row is `Ready`.
    pub(crate) async fn link(w: &World, d: &Device, script: Arc<Mutex<Script>>) -> Self {
        let mut req = format!("ws://{}/mcp/host", w.addr())
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", d.key).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let dev = Self::run(ws, script);
        let id = device_row(w, d.id).id;
        patience::until_async("the device row is ready", || async {
            w.state.mcp.is_ready(id).await
        })
        .await;
        dev
    }

    fn run(ws: crate::mcp_host::Ws, script: Arc<Mutex<Script>>) -> Self {
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (calls, calls_rx) = mpsc::unbounded_channel();
        let (gets, gets_rx) = mpsc::unbounded_channel();
        let (results, results_rx) = mpsc::unbounded_channel();
        let (cancels, cancels_rx) = mpsc::unbounded_channel();
        let (notes, notes_rx) = mpsc::unbounded_channel();
        let tx = SeenTx {
            calls,
            gets,
            results,
            cancels,
            notes,
        };
        let (s, o) = (script.clone(), out.clone());
        let task = tokio::spawn(async move {
            let (mut sink, mut stream) = ws.split();
            loop {
                tokio::select! {
                    frame = stream.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let v: Value = serde_json::from_str(text.as_str()).unwrap();
                            for reply in handle(&v, &s, &tx) {
                                let _ = o.send(Message::Text(reply.to_string().into()));
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => {}
                    },
                    Some(m) = out_rx.recv() => {
                        if sink.send(m).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            out,
            script,
            seen: Seen {
                calls: calls_rx,
                gets: gets_rx,
                results: results_rx,
                cancels: cancels_rx,
                notes: notes_rx,
            },
            task,
        }
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(Message::Text(v.to_string().into()));
    }

    /// Move task `id` to `status` (with `message`), answer any `tasks/result`
    /// it held once it is terminal, and send the status notification when
    /// `notify`.
    pub(crate) fn set(
        &self,
        id: &str,
        status: &str,
        message: Option<&str>,
        payload: Option<Result<Value, (i64, String)>>,
        notify: bool,
    ) {
        let (task, replies) = {
            let mut s = self.script.lock().unwrap();
            let t = s.tasks.get_mut(id).expect("a task the device runs");
            t.status = status.into();
            t.message = message.map(str::to_string);
            if payload.is_some() {
                t.payload = payload;
            }
            let t = t.clone();
            let replies = if t.terminal() {
                release(&mut s, id)
            } else {
                Vec::new()
            };
            (t, replies)
        };
        for r in replies {
            self.send(r);
        }
        if notify {
            self.send(
                json!({"jsonrpc": "2.0", "method": "notifications/tasks/status",
                             "params": task.json(id)}),
            );
        }
    }

    /// Complete task `id` with the text `text`.
    pub(crate) fn complete(&self, id: &str, text: &str, notify: bool) {
        let result = json!({"content": [{"type": "text", "text": text}]});
        self.set(id, "completed", None, Some(Ok(result)), notify);
    }

    /// Drop the link without a close frame, as a device that vanished.
    pub(crate) fn vanish(self) {
        self.task.abort();
    }
}

/// The held `tasks/result` answers of task `id`, now terminal.
fn release(s: &mut Script, id: &str) -> Vec<Value> {
    let t = s.tasks[id].clone();
    let (mine, rest): (Vec<_>, Vec<_>) = s.held.drain(..).partition(|(_, t)| t == id);
    s.held = rest;
    mine.into_iter().map(|(rid, _)| payload(&rid, &t)).collect()
}

fn payload(rid: &Value, t: &FakeTask) -> Value {
    match &t.payload {
        Some(Ok(result)) => json!({"jsonrpc": "2.0", "id": rid, "result": result}),
        Some(Err((code, message))) => {
            json!({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}})
        }
        None => json!({"jsonrpc": "2.0", "id": rid, "result": {"content": []}}),
    }
}

fn unknown(rid: &Value, task: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": rid,
           "error": {"code": -32602, "message": format!("unknown task: {task}")}})
}

/// The device's side of one message: its answers.
fn handle(v: &Value, script: &Mutex<Script>, seen: &SeenTx) -> Vec<Value> {
    let Some(rid) = v.get("id").cloned() else {
        let _ = seen.notes.send(v.clone());
        return Vec::new();
    };
    let ok = |result: Value| vec![json!({"jsonrpc": "2.0", "id": rid, "result": result})];
    let mut s = script.lock().unwrap();
    let task_of = |v: &Value| {
        v["params"]["taskId"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    match v["method"].as_str() {
        Some("initialize") => {
            let mut caps = json!({"tools": {"listChanged": true}});
            if s.declares {
                caps["tasks"] = json!({"requests": {"tools": {"call": {}}}});
                if s.cancel_cap {
                    caps["tasks"]["cancel"] = json!({});
                }
            }
            ok(json!({"protocolVersion": v["params"]["protocolVersion"],
                      "capabilities": caps,
                      "serverInfo": {"name": "task-device", "version": "1"}}))
        }
        Some("tools/list") => ok(json!({"tools": s.tools.clone()})),
        Some("tools/call") => {
            let _ = seen.calls.send(v.clone());
            let asked = v["params"].get("task").is_some();
            if (asked && !s.never_task) || s.always_task {
                s.next += 1;
                let id = format!("t{}", s.next);
                let task = FakeTask {
                    status: "working".into(),
                    message: None,
                    poll_interval: s.poll_interval,
                    payload: None,
                };
                let mut result = json!({"task": task.json(&id)});
                if let Some(said) = &s.immediate {
                    result["_meta"] =
                        json!({"io.modelcontextprotocol/model-immediate-response": said});
                }
                s.tasks.insert(id, task);
                ok(result)
            } else {
                let name = v["params"]["name"].as_str().unwrap_or("?");
                ok(json!({"content": [{"type": "text", "text": format!("ran {name}")}]}))
            }
        }
        Some("tasks/get") => {
            let _ = seen.gets.send(v.clone());
            let id = task_of(v);
            match s.tasks.get(&id) {
                Some(t) => ok(t.json(&id)),
                None => vec![unknown(&rid, &id)],
            }
        }
        Some("tasks/result") => {
            let _ = seen.results.send(v.clone());
            let id = task_of(v);
            match s.tasks.get(&id) {
                Some(t) if t.terminal() => vec![payload(&rid, t)],
                Some(_) => {
                    s.held.push((rid, id));
                    Vec::new()
                }
                None => vec![unknown(&rid, &id)],
            }
        }
        Some("tasks/cancel") => {
            let _ = seen.cancels.send(v.clone());
            let id = task_of(v);
            if let Some((code, message)) = &s.cancel_error {
                return vec![json!({"jsonrpc": "2.0", "id": rid,
                                   "error": {"code": code, "message": message}})];
            }
            match s.tasks.get_mut(&id) {
                Some(t) if !t.terminal() => {
                    t.status = "cancelled".into();
                    let t = t.clone();
                    let mut out = release(&mut s, &id);
                    out.insert(
                        0,
                        json!({"jsonrpc": "2.0", "id": rid, "result": t.json(&id)}),
                    );
                    out
                }
                _ => vec![unknown(&rid, &id)],
            }
        }
        Some("ping") => ok(json!({})),
        _ => Vec::new(),
    }
}

/// A registered server: the fake's script behind a Streamable HTTP
/// endpoint answering in JSON (no stream, so no status notifications: a
/// registered server's task is followed by polling here).
pub(crate) struct HttpServer {
    pub url: String,
    pub script: Arc<Mutex<Script>>,
    pub seen: Seen,
    /// Every `initialize`: one per connect.
    pub inits: Arc<AtomicUsize>,
}

impl HttpServer {
    pub(crate) fn inits(&self) -> usize {
        self.inits.load(Ordering::SeqCst)
    }

    /// Complete task `id` with the text `text` (no notification).
    pub(crate) fn complete(&self, id: &str, text: &str) {
        let mut s = self.script.lock().unwrap();
        let t = s.tasks.get_mut(id).expect("a task the server runs");
        t.status = "completed".into();
        t.payload = Some(Ok(json!({"content": [{"type": "text", "text": text}]})));
    }
}

/// Serve `script` as a registered server would.
pub(crate) async fn http_server(script: Script) -> HttpServer {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let script = Arc::new(Mutex::new(script));
    let (calls, calls_rx) = mpsc::unbounded_channel();
    let (gets, gets_rx) = mpsc::unbounded_channel();
    let (results, results_rx) = mpsc::unbounded_channel();
    let (cancels, cancels_rx) = mpsc::unbounded_channel();
    let (notes, notes_rx) = mpsc::unbounded_channel();
    let tx = Arc::new(SeenTx {
        calls,
        gets,
        results,
        cancels,
        notes,
    });
    let inits = Arc::new(AtomicUsize::new(0));
    let (s, counter) = (script.clone(), inits.clone());
    let handler = move |body: String| {
        let (s, tx, counter) = (s.clone(), tx.clone(), counter.clone());
        async move {
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if v["method"] == "initialize" {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            match handle(&v, &s, &tx).into_iter().next() {
                Some(reply) => (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", "tasks-session"),
                    ],
                    reply.to_string(),
                )
                    .into_response(),
                None => StatusCode::ACCEPTED.into_response(),
            }
        }
    };
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(handler).delete(|| async { StatusCode::OK }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    HttpServer {
        url: format!("http://{addr}/mcp"),
        script,
        seen: Seen {
            calls: calls_rx,
            gets: gets_rx,
            results: results_rx,
            cancels: cancels_rx,
            notes: notes_rx,
        },
        inits,
    }
}

/// A world with device `desktop` hosting `script`'s tools, linked: the
/// world, the device's key, the link, and the row's id.
pub(crate) async fn task_world(script: Script) -> (World, Device, TaskDevice, i64) {
    let (w, d) = host_world().await;
    let dev = TaskDevice::link(&w, &d, Arc::new(Mutex::new(script))).await;
    let id = device_row(&w, d.id).id;
    (w, d, dev, id)
}

/// Set Settings → MCP's task poll interval.
pub(crate) async fn poll_every(w: &World, secs: u32) {
    crate::realtime_chat_thread::settings(&w.state, |s| s.mcp.task_poll_interval_s = secs).await;
}

/// An executor for the device's tools, as a run offers them: the late path
/// for thread `late` when given.
pub(crate) fn executor(w: &World, server: i64, late: Option<i64>) -> McpExecutor {
    let listed: HashMap<String, i64> = ["desktop__build", "desktop__opt", "desktop__plain"]
        .into_iter()
        .map(|n| (n.to_string(), server))
        .collect();
    let exec = McpExecutor::new(w.state.clone(), RequestCtx::default())
        .with_proto(lmgw_core::telemetry::CHAT_TOOL_PROTO)
        .with_listed(listed);
    match late {
        Some(tid) => exec.with_late(tid, true),
        None => exec,
    }
}

/// Call `name` through `exec`.
pub(crate) async fn call(exec: &McpExecutor, name: &str) -> ToolOutcome {
    exec.call(name, &json!({"text": "go"})).await
}

/// An outcome's text.
pub(crate) fn text(o: &ToolOutcome) -> String {
    lmgw_core::ir::flatten_tool_result(&o.blocks).0
}

/// A stored thread to start late tasks for, with a turn kept running in
/// it for the test's length: the results its tasks end with stay in their
/// rows for the test to read, rather than entering the thread (MCP Tasks
/// WP2 delivers them only while no turn runs; `thread` tests that).
pub(crate) async fn thread(w: &World) -> i64 {
    let tid = w.thread("chatty", json!({})).await;
    std::mem::forget(w.state.chat_turn_held_for_tests(tid).await);
    tid
}

/// The only stored task row.
pub(crate) async fn the_row(w: &World) -> lmgw_core::store::mcp_tasks::McpTaskRow {
    let rows = lmgw_core::store::mcp_tasks::all(&w.state.db).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows.into_iter().next().unwrap()
}

/// Every task row, in their order.
pub(crate) async fn rows(w: &World) -> Vec<lmgw_core::store::mcp_tasks::McpTaskRow> {
    lmgw_core::store::mcp_tasks::all(&w.state.db).await.unwrap()
}

/// Wait until the only task row is in `state`: that row.
pub(crate) async fn row_in(w: &World, state: &str) -> lmgw_core::store::mcp_tasks::McpTaskRow {
    patience::until_async(&format!("the task row is {state}"), || async {
        lmgw_core::store::mcp_tasks::all(&w.state.db)
            .await
            .unwrap()
            .first()
            .is_some_and(|r| r.state == state)
    })
    .await;
    the_row(w).await
}

/// A stored result's text.
pub(crate) fn result_text(row: &lmgw_core::store::mcp_tasks::McpTaskRow) -> String {
    let blocks: Vec<lmgw_core::ir::ToolResultBlock> =
        serde_json::from_str(row.result.as_deref().expect("a result")).unwrap();
    lmgw_core::ir::flatten_tool_result(&blocks).0
}

/// The next message on `rx`, within patience.
pub(crate) async fn next(what: &str, rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    patience::within(what, rx.recv())
        .await
        .expect("the device is gone")
}
