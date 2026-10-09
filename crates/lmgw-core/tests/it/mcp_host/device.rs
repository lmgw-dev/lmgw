//! A fake device that hosts tools over `GET /mcp/host`: an MCP server on the
//! client end of a WebSocket, as a desktop client's would be.
//!
//! It answers `initialize` and `tools/list` by itself (its tools are set by
//! the test), hands every `tools/call` to the test (and answers it at once
//! when `auto` is on), and reports every notification and the close it got.
//! It serves resources too: `resources/list` answers what the test set
//! (none by default), every listing request is reported, and every
//! `resources/read` is reported and answered with a page naming its URI —
//! unless `hold_reads` is on, when it is left unanswered.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderName;
use tokio_tungstenite::tungstenite::Message;

/// A connected fake device.
pub(crate) struct FakeDevice {
    out: mpsc::UnboundedSender<Message>,
    /// Every `tools/call` the gateway sent, whole.
    pub calls: mpsc::UnboundedReceiver<Value>,
    /// Every notification the gateway sent (`notifications/initialized`,
    /// `notifications/cancelled`), whole.
    pub notes: mpsc::UnboundedReceiver<Value>,
    /// Every answer to a request the device sent.
    pub answers: mpsc::UnboundedReceiver<Value>,
    /// Every `initialize` the gateway sent, whole.
    pub inits: mpsc::UnboundedReceiver<Value>,
    /// Every `resources/read` the gateway sent, whole.
    pub reads: mpsc::UnboundedReceiver<Value>,
    /// Every `resources/list` and `resources/templates/list` the gateway
    /// sent, whole.
    pub lists: mpsc::UnboundedReceiver<Value>,
    /// Leave every `resources/read` unanswered.
    pub hold_reads: Arc<AtomicBool>,
    /// The close the gateway sent: `(code, reason)`, once.
    closed: watch::Receiver<Option<(u16, String)>>,
    /// Answer calls at once with "ran <name>".
    pub auto: Arc<AtomicBool>,
    tools: Arc<Mutex<Vec<Value>>>,
    resources: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

/// One tool, as `tools/list` carries it.
pub(crate) fn tool(name: &str) -> Value {
    json!({"name": name, "description": format!("the {name} tool"),
           "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}})
}

/// The upgrade's answer when it is no 101: its status and body.
pub(crate) async fn refused(addr: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    match open(addr, headers).await {
        Ok(_) => panic!("the upgrade was accepted"),
        Err(e) => e,
    }
}

/// A link opened with `key` that nothing reads: a device that went silent.
pub(crate) async fn silent(addr: &str, key: &str) -> super::Ws {
    let auth = format!("Bearer {key}");
    open(addr, &[("authorization", &auth)])
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"))
}

async fn open(addr: &str, headers: &[(&str, &str)]) -> Result<super::Ws, (u16, Value)> {
    let mut req = format!("ws://{addr}/mcp/host")
        .into_client_request()
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status().as_u16();
            let body = resp
                .body()
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null);
            Err((status, body))
        }
        Err(e) => panic!("the upgrade failed: {e}"),
    }
}

impl FakeDevice {
    /// Connect with `key`, offering `tools`.
    pub(crate) async fn connect(addr: &str, key: &str, tools: Vec<Value>) -> Self {
        let auth = format!("Bearer {key}");
        let ws = open(addr, &[("authorization", &auth)])
            .await
            .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
        Self::run(ws, tools)
    }

    fn run(ws: super::Ws, tools: Vec<Value>) -> Self {
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (notes_tx, notes) = mpsc::unbounded_channel();
        let (answers_tx, answers) = mpsc::unbounded_channel();
        let (inits_tx, inits) = mpsc::unbounded_channel();
        let (reads_tx, reads) = mpsc::unbounded_channel();
        let (lists_tx, lists) = mpsc::unbounded_channel();
        let hold_reads = Arc::new(AtomicBool::new(false));
        let hold = hold_reads.clone();
        let resources: Arc<Mutex<Vec<Value>>> = Arc::default();
        let res = resources.clone();
        let (closed_tx, closed) = watch::channel(None);
        let auto = Arc::new(AtomicBool::new(true));
        let tools = Arc::new(Mutex::new(tools));
        let (t, a, o) = (tools.clone(), auto.clone(), out.clone());
        let task = tokio::spawn(async move {
            let (mut sink, mut stream) = ws.split();
            loop {
                tokio::select! {
                    frame = stream.next() => {
                        let msg = match frame {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        match msg {
                            Message::Text(text) => {
                                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                                if v["method"] == "initialize" {
                                    let _ = inits_tx.send(v.clone());
                                }
                                if matches!(v["method"].as_str(),
                                            Some("resources/list" | "resources/templates/list")) {
                                    let _ = lists_tx.send(v.clone());
                                }
                                let reply = if v["method"] == "resources/list" {
                                    Some(json!({"jsonrpc": "2.0", "id": v["id"],
                                        "result": {"resources": res.lock().unwrap().clone()}}))
                                } else if v["method"] == "resources/templates/list" {
                                    Some(json!({"jsonrpc": "2.0", "id": v["id"],
                                        "result": {"resourceTemplates": []}}))
                                } else if v["method"] == "resources/read" {
                                    let _ = reads_tx.send(v.clone());
                                    let uri = v["params"]["uri"].clone();
                                    (!hold.load(Ordering::SeqCst)).then(|| json!({"jsonrpc": "2.0", "id": v["id"], "result": {
                                        "contents": [{"uri": uri, "mimeType": "text/html;profile=mcp-app",
                                                      "text": format!("page of {}", uri.as_str().unwrap_or("?"))}]}}))
                                } else {
                                    handle(&v, &t, &a, &calls_tx, &notes_tx, &answers_tx)
                                };
                                if let Some(r) = reply {
                                    let _ = o.send(Message::Text(r.to_string().into()));
                                }
                            }
                            Message::Close(frame) => {
                                let said = frame
                                    .map(|f| (u16::from(f.code), f.reason.to_string()))
                                    .unwrap_or((1005, String::new()));
                                let _ = closed_tx.send(Some(said));
                                break;
                            }
                            _ => {}
                        }
                    }
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
            calls,
            notes,
            answers,
            inits,
            reads,
            lists,
            hold_reads,
            closed,
            auto,
            tools,
            resources,
            task,
        }
    }

    /// List `resources` from now on.
    pub(crate) fn set_resources(&self, resources: Vec<Value>) {
        *self.resources.lock().unwrap() = resources;
    }

    /// Send one raw frame.
    pub(crate) fn send(&self, msg: Message) {
        let _ = self.out.send(msg);
    }

    /// Send one JSON-RPC message.
    pub(crate) fn send_json(&self, v: Value) {
        self.send(Message::Text(v.to_string().into()));
    }

    /// Offer `tools` from now on, and say so.
    pub(crate) fn change_tools(&self, tools: Vec<Value>) {
        *self.tools.lock().unwrap() = tools;
        self.send_json(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
    }

    /// The next `tools/call`.
    pub(crate) async fn next_call(&mut self) -> Value {
        crate::common::patience::within("the device's next tools/call", self.calls.recv())
            .await
            .expect("the device is gone")
    }

    /// The next notification named `method`, skipping others.
    pub(crate) async fn next_note(&mut self, method: &str) -> Value {
        loop {
            let n = crate::common::patience::within(
                "the device's next notification",
                self.notes.recv(),
            )
            .await
            .expect("the device is gone");
            if n["method"] == method {
                return n;
            }
        }
    }

    /// The close the gateway sent.
    pub(crate) async fn closed(&mut self) -> (u16, String) {
        let got = crate::common::patience::within(
            "the gateway's close",
            self.closed.wait_for(Option::is_some),
        )
        .await
        .expect("the device task ended without a close");
        got.clone().unwrap()
    }

    /// Drop the link without a close frame, as a device that vanished.
    pub(crate) fn vanish(self) {
        self.task.abort();
    }
}

/// The device's side of one message: its answer, if it has one.
fn handle(
    v: &Value,
    tools: &Mutex<Vec<Value>>,
    auto: &AtomicBool,
    calls: &mpsc::UnboundedSender<Value>,
    notes: &mpsc::UnboundedSender<Value>,
    answers: &mpsc::UnboundedSender<Value>,
) -> Option<Value> {
    let id = v.get("id").cloned();
    match (v["method"].as_str(), id) {
        (Some("initialize"), Some(id)) => Some(json!({"jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": v["params"]["protocolVersion"],
            "capabilities": {"tools": {"listChanged": true}, "resources": {}},
            "serverInfo": {"name": "fake-device", "version": "1"}}})),
        (Some("tools/list"), Some(id)) => Some(json!({"jsonrpc": "2.0", "id": id,
            "result": {"tools": tools.lock().unwrap().clone()}})),
        (Some("tools/call"), Some(id)) => {
            let _ = calls.send(v.clone());
            auto.load(Ordering::SeqCst).then(|| {
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": [
                    {"type": "text", "text": format!("ran {}", v["params"]["name"].as_str().unwrap_or("?"))}
                ]}})
            })
        }
        (Some("ping"), Some(id)) => Some(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        (Some(_), None) => {
            let _ = notes.send(v.clone());
            None
        }
        (None, Some(_)) => {
            let _ = answers.send(v.clone());
            None
        }
        _ => None,
    }
}
