//! Service mode end to end (container-runtime design §3.3, §6.5, WP4).
//!
//! Two halves, the split WP2 and WP3 already made:
//!
//! - **Against a fake spawner** (the bulk, runs in CI): the whole proxy path
//!   runs for real — the route, the on-demand start, the header hygiene, a
//!   streamed SSE body, a raw upgrade tunnelled byte for byte, the idle sweep
//!   and its in-flight guard, and the `agent:<id>` MCP row a chat thread's
//!   label resolves through. The fake does what `podman run -d` would do and
//!   nothing more: it records the argv and **binds the host port the argv
//!   publishes**, so there is something real behind the proxy.
//! - **Against real podman** (`the_real_thing_*`): one image built here over
//!   `docker.io/library/node:24-alpine` serving a page, an SSE endpoint, a real
//!   WebSocket (handshake computed with node's `crypto`, one masked client
//!   frame in, one unmasked server frame out) and a minimal MCP endpoint.
//!   Skips itself with a message when podman is absent, and removes its image
//!   and every container it makes, including on failure.
//!
//! Every gateway binds a **real** listener before its `bind_addr` is saved: the
//! `agent:<id>` MCP row's URL is built from that setting, and lmgw's own MCP
//! client dials it — so a default `127.0.0.1:8001` would send the aggregator at
//! whatever else is on this box.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use lmgw_core::agents::container::{Spawned, Spawner};
use lmgw_core::agents::service;
use lmgw_core::runtime::registry::CmdOutput;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common;
use common::{dashboard_key, serve_on, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

/// A gateway whose `bind_addr` setting really is the port it is listening on.
async fn gateway() -> (SharedState, Gw, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let prefix = format!(
        "lmgwa{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = prefix.clone();
    settings.bind_addr = addr.to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let key = dashboard_key(&state);
    let app = build_router(state.clone());
    tokio::spawn(serve_on(listener, app));
    (
        state,
        Gw {
            base: format!("http://{addr}"),
            key,
        },
        prefix,
    )
}

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

async fn get(base: &Gw, path: &str) -> (u16, String) {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.text().await.unwrap_or_default(),
    )
}

/// A WebSocket handshake head addressed to `host`.
///
/// No credential: an upgrade on an agent origin is dispatched by `Host` before
/// any gate (§4.2), and a raw socket has no client to carry one anyway.
fn upgrade_head(host: &str, path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: \
         Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: \
         13\r\n\r\n"
    )
}

/// One HTTP/1.1 request over a bare socket, head and all — for a path no client
/// library will send unmodified, addressed to whichever `Host` the test means.
///
/// The dashboard session rides along by hand because `/agents/{id}/mcp` is an
/// `Admin` route (principals §3.2); on an agent origin it is stripped before
/// the container sees it (§4.6) and nothing there asks for it.
async fn raw_get(base: &Gw, host: &str, path: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(base.addr()).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nConnection: \
         close\r\n\r\n",
        base.key
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = sock.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).to_string()
}

/// A `GET` on an agent origin (origins §4.1): lmgw's own socket, the agent's
/// host name, and **no credential** — the origin is the container's namespace
/// and asks for none.
async fn app_get(base: &Gw, host: &str, path: &str) -> (u16, String) {
    let resp = base
        .origin_client(&[host])
        .get(format!("{}{path}", base.origin(host)))
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.text().await.unwrap_or_default(),
    )
}

async fn get_raw(client: &reqwest::Client, url: &str) -> String {
    client
        .get(url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap_or_default()
}

async fn detail(base: &Gw, id: &str) -> Value {
    let (status, body) = get(base, &format!("/api/agents/{id}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// A service-mode manifest. `provides` is either empty or the `provides` block.
fn doc(image: &str, idle_seconds: i64, start_timeout: u64, provides: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "description": "an agent that serves its own page",
  "model": {{ "alias": "m1" }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "limits": {{ "memory_mb": 256, "cpus": 1.0, "pids": 64, "stop_grace_seconds": 1 }},
    "service": {{ "port": 8080, "idle_seconds": {idle_seconds},
                 "start_timeout_seconds": {start_timeout} }}{provides}
  }}
}}"#
    )
}

const PROVIDES: &str = r#", "provides": { "mcp": "/mcp" }"#;

/// The same service agent with one `rw` directory slot (mounts §5.1), for the
/// one thing a mount changes on this side: a stored path the running container
/// is holding.
fn mount_doc() -> String {
    r#"{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "description": "an agent that serves its own page",
  "model": { "alias": "m1" },
  "config": { "schema": { "type": "object", "properties": {
    "notes": { "type": "string", "format": "directory", "access": "rw" }
  } } },
  "run": {
    "kind": "container",
    "image": "localhost/board:1",
    "limits": { "memory_mb": 256, "cpus": 1.0, "pids": 64, "stop_grace_seconds": 1 },
    "service": { "port": 8080, "idle_seconds": 0, "start_timeout_seconds": 10 }
  }
}"#
    .to_string()
}

async fn install(base: &Gw, manifest: &str) {
    let (status, body) = op(
        base,
        "agent_set",
        json!({ "manifest": manifest, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "agent_set: {body}");
}

// ---------------------------------------------------------------------------
// The fake container
// ---------------------------------------------------------------------------

/// What the fake container serves. Everything the proxy has to get right, and
/// nothing it has to understand.
fn fake_container_app() -> axum::Router {
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    async fn page() -> impl IntoResponse {
        (
            [("content-type", "text/html")],
            "<h1>board</h1><p>served by the container</p>",
        )
    }
    // The headers the container actually saw, so the hygiene rules can be
    // asserted from the far side rather than from the code that applies them.
    //
    // A repeated header comes back as its values joined by `, `, which is what
    // makes "exactly one `X-Forwarded-Host`, and it is lmgw's" an equality
    // assertion rather than a count nobody can see (§4.6).
    async fn seen(headers: axum::http::HeaderMap) -> impl IntoResponse {
        let mut m = serde_json::Map::new();
        for (k, v) in headers.iter() {
            let value = v.to_str().unwrap_or_default();
            match m.get_mut(k.as_str()) {
                Some(Value::String(seen)) => {
                    seen.push_str(", ");
                    seen.push_str(value);
                }
                _ => {
                    m.insert(k.as_str().to_string(), Value::String(value.to_string()));
                }
            }
        }
        axum::Json(Value::Object(m))
    }
    async fn teapot() -> impl IntoResponse {
        (axum::http::StatusCode::IM_A_TEAPOT, "short and stout")
    }
    /// A 302 the proxy must **forward**, not follow.
    async fn go() -> impl IntoResponse {
        (
            axum::http::StatusCode::FOUND,
            [("location", "/login?next=%2F")],
        )
    }
    /// An absolute `Location` naming the container's **own** published origin
    /// — read off the `Host` the proxy dialled it with, which is exactly that.
    /// The one case the proxy rewrites (§4.5): the browser cannot reach a
    /// loopback port it is not talking to.
    async fn go_self(headers: axum::http::HeaderMap) -> impl IntoResponse {
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        (
            axum::http::StatusCode::FOUND,
            [("location", format!("http://{host}/after?x=1"))],
        )
    }
    /// An absolute `Location` somewhere else: the app sending its user away.
    async fn go_abs() -> impl IntoResponse {
        (
            axum::http::StatusCode::FOUND,
            [("location", "https://example.com/elsewhere")],
        )
    }
    /// The request line the container actually received — the only place the
    /// percent-encoding of a proxied path can be checked honestly.
    async fn echo(req: axum::extract::Request) -> impl IntoResponse {
        axum::Json(json!({
            "path": req.uri().path(),
            "query": req.uri().query(),
        }))
    }
    /// Three events, 200 ms apart: a body that only proves anything if it is
    /// streamed.
    async fn sse() -> impl IntoResponse {
        let s = futures::stream::unfold(0u32, |n| async move {
            if n >= 3 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from(format!("data: tick {}\n\n", n + 1))),
                n + 1,
            ))
        });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(s),
        )
    }
    /// A request that is still in flight for a while — what the idle guard is
    /// for.
    async fn slow() -> impl IntoResponse {
        let s = futures::stream::unfold(0u32, |n| async move {
            if n >= 2 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from("chunk\n")),
                n + 1,
            ))
        });
        axum::body::Body::from_stream(s)
    }
    /// A raw upgrade that echoes bytes.
    ///
    /// Deliberately **not** a WebSocket implementation: the proxy pipes bytes
    /// and parses no frames, so a byte echo is exactly the contract under test
    /// here. The real handshake and a real masked frame are the real-podman
    /// leg's job, where node's `crypto` can compute a correct
    /// `Sec-WebSocket-Accept`.
    async fn ws(mut req: axum::extract::Request) -> impl IntoResponse {
        // What the upgrade request carried of lmgw's per-face headers, handed
        // back on the 101 — the one place a byte pipe's request headers can be
        // read from the far side.
        let joined = |name: &str| {
            req.headers()
                .get_all(name)
                .iter()
                .map(|v| v.to_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (face, peer) = (joined("x-lmgw-face"), joined("x-forwarded-for"));
        let on = req.extensions_mut().remove::<hyper::upgrade::OnUpgrade>();
        tokio::spawn(async move {
            if let Some(on) = on {
                if let Ok(io) = on.await {
                    let mut io = hyper_util::rt::TokioIo::new(io);
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = io.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let mut out = b"echo:".to_vec();
                        out.extend_from_slice(&buf[..n]);
                        if io.write_all(&out).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        axum::http::Response::builder()
            .status(axum::http::StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-accept", "fake")
            .header("x-seen-face", face)
            .header("x-seen-for", peer)
            .body(axum::body::Body::empty())
            .unwrap()
    }
    async fn mcp(body: String) -> impl IntoResponse {
        (
            axum::http::StatusCode::OK,
            [
                ("content-type", "application/json"),
                ("mcp-session-id", "board-session"),
            ],
            mcp_reply(&body),
        )
    }

    axum::Router::new()
        .route("/", get(page))
        .route("/seen", get(seen))
        .route("/teapot", get(teapot))
        .route("/go", get(go))
        .route("/go-self", get(go_self))
        .route("/go-abs", get(go_abs))
        .route("/echo", get(echo))
        .route("/echo/{*rest}", get(echo))
        .route("/mcp/seen", get(seen))
        .route("/mcp/{*rest}", post(mcp).get(echo))
        .route("/sse", get(sse))
        .route("/slow", get(slow))
        .route("/ws", get(ws))
        .route("/mcp", post(mcp))
}

/// One JSON-RPC exchange, the shape `tests/it/agents_script.rs`'s `gws_stub`
/// answers with: the aggregator's client speaks streamable HTTP and takes a
/// plain `application/json` reply.
fn mcp_reply(body: &str) -> String {
    let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let Some(id) = req.get("id").cloned() else {
        return String::new();
    };
    let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": req.pointer("/params/protocolVersion")
                .cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "board", "version": "0.1.0" },
        }),
        "tools/list" => json!({ "tools": [{
            "name": "pin",
            "description": "pin a card to the board",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
        }] }),
        "tools/call" => json!({
            "content": [{ "type": "text", "text": "{\"pinned\":true}" }],
            "structuredContent": { "pinned": true },
            "isError": false,
        }),
        _ => json!({}),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// A fake `podman` that binds the host port its `run -d` argv publishes.
#[derive(Default)]
struct Fake {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    /// How long `podman run -d` takes to answer — a cold pull, in miniature.
    slow_start: Option<Duration>,
    /// Accept TCP and answer nothing: a container that is listening but never
    /// finishes a health probe.
    tcp_only: bool,
    /// Shuts the fake container's listener down, for the "it died after its
    /// probe passed" case.
    kill: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl Fake {
    fn calls(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        self.calls.clone()
    }
    fn slow(ms: u64) -> Self {
        Self {
            slow_start: Some(Duration::from_millis(ms)),
            ..Default::default()
        }
    }
    fn tcp_only() -> Self {
        Self {
            tcp_only: true,
            ..Default::default()
        }
    }
    fn kill_switch(&self) -> Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> {
        self.kill.clone()
    }
}

fn run_count(calls: &Arc<Mutex<Vec<Vec<String>>>>) -> usize {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.first().map(String::as_str) == Some("run"))
        .count()
}

#[async_trait]
impl Spawner for Fake {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        panic!("service mode never uses the streaming seam");
    }
    async fn run(&self, _p: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        if args.first().map(String::as_str) == Some("run") {
            if let Some(d) = self.slow_start {
                tokio::time::sleep(d).await;
            }
            let i = args.iter().position(|a| a == "-p").expect("published");
            let port: u16 = args[i + 1].split(':').nth(1).unwrap().parse().unwrap();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            if self.tcp_only {
                // Accept and say nothing: a process that is listening but will
                // never finish a health probe.
                tokio::spawn(async move {
                    let mut held = Vec::new();
                    while let Ok((sock, _)) = listener.accept().await {
                        held.push(sock);
                    }
                });
            } else {
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                *self.kill.lock().unwrap() = Some(tx);
                tokio::spawn(async move {
                    let _ = axum::serve(listener, fake_container_app())
                        .with_graceful_shutdown(async {
                            let _ = rx.await;
                        })
                        .await;
                });
            }
        }
        // `podman logs --tail <n>` answers with the number of lines it was
        // asked for, so a test can tell "the default excerpt" from "what the
        // reader raised it to" — and from `-1`, the whole log.
        if args.first().map(String::as_str) == Some("logs") {
            let tail = args
                .iter()
                .position(|a| a == "--tail")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let n: usize = if tail == "-1" {
                40
            } else {
                tail.parse().unwrap_or(0)
            };
            let body: String = (0..n).map(|i| format!("line {i}\n")).collect();
            return Ok(CmdOutput {
                status: 0,
                stdout: format!("tail={tail}\n{body}"),
                stderr: String::new(),
            });
        }
        Ok(CmdOutput {
            status: 0,
            stdout: "deadbeef\n".into(),
            stderr: String::new(),
        })
    }
}

async fn with_fake() -> (SharedState, Gw, Arc<Mutex<Vec<Vec<String>>>>) {
    let (state, base, calls, _kill) = with(Fake::default()).await;
    (state, base, calls)
}

type Kill = Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>;

async fn with(fake: Fake) -> (SharedState, Gw, Arc<Mutex<Vec<Vec<String>>>>, Kill) {
    let (state, base, _prefix) = gateway().await;
    let calls = fake.calls();
    let kill = fake.kill_switch();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    (state, base, calls, kill)
}

// ---------------------------------------------------------------------------
// The proxy (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_agents_own_page_loads_through_the_proxy_and_the_start_is_on_demand() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    // Nothing is running before anyone asks (§3.3: on demand).
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
    assert_eq!(d["service"]["idle_seconds"], json!(0), "{d}");
    // The agent's own origin, on lmgw's own port, with its slash (§4.9).
    let port = base.base.rsplit(':').next().unwrap();
    assert_eq!(
        d["service"]["origin"],
        json!(format!("http://board.localhost:{port}/")),
        "{d}"
    );
    assert!(d["service"].get("app_base").is_none(), "{d}");
    // `*.localhost` resolves on any box with systemd-resolved or a browser;
    // the field is a verdict about this machine, so it is asserted as present
    // and boolean rather than as true.
    assert!(d["service"]["origin_resolves"].is_boolean(), "{d}");
    assert_eq!(run_count(&calls), 0);

    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("served by the container"), "{body}");
    assert_eq!(run_count(&calls), 1, "the first request started it");

    // And the detail now names the container and the port it published on.
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(true), "{d}");
    assert!(d["service"]["host_port"].as_u64().unwrap() > 0, "{d}");
    assert!(
        d["service"]["container"]
            .as_str()
            .unwrap()
            .contains("agentsvc-board"),
        "{d}"
    );
    // The catalog card's chip.
    let (_, cards) = get(&base, "/api/agents").await;
    let cards: Value = serde_json::from_str(&cards).unwrap();
    let card = cards
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!("board"))
        .unwrap();
    assert_eq!(card["app"], json!(true), "{card}");

    service::stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn the_status_is_forwarded_verbatim_and_a_404_does_not_fall_through_to_the_spa() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    let (status, body) = app_get(&base, "board.localhost", "/teapot").await;
    assert_eq!(status, 418, "{body}");
    assert_eq!(body, "short and stout");

    // The container's 404, not the SPA shell: every path on the agent origin
    // belongs to the container, and serving index.html there would be a lie.
    let (status, body) = app_get(&base, "board.localhost", "/nope").await;
    assert_eq!(status, 404, "{body}");
    assert!(!body.contains("<!DOCTYPE html>"), "{body}");

    service::stop(&state, "board", "test over").await;
}

/// The old mount answers where the app went, and answers it to a browser with
/// no session at all — `Public`, because an address is not a secret (§4.2).
///
/// The same body for an id that serves an app and for one nobody installed:
/// the route is unauthenticated, and an answer that differed would be a free
/// list of which agents are here and which of them serve a UI.
#[tokio::test]
async fn the_old_app_mount_says_where_the_app_went() {
    let (_state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let port = base.addr().port();

    for path in [
        "/agents/board/app",
        "/agents/board/app/",
        "/agents/board/app/deep/link?tab=two",
    ] {
        let resp = base
            .anon()
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404, "{path}");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["code"], json!("agent_app_moved"), "{path}: {v}");
        // The address, so an old bookmark is told where to go rather than
        // being asked for a session it no longer needs.
        assert_eq!(
            v["origin"],
            json!(format!("http://board.localhost:{port}/")),
            "{path}: {v}"
        );
        assert!(v["message"].as_str().unwrap().contains("board"), "{v}");
    }

    // An id nobody installed gets the *same* answer, address included: the
    // origin is derivable from the id alone, so there is nothing here to read
    // an agent's existence out of.
    let installed: Value = base
        .anon()
        .get(format!("{base}/agents/board/app/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = base
        .anon()
        .get(format!("{base}/agents/nobody/app/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], json!("agent_app_moved"), "{v}");
    assert_eq!(
        v["origin"],
        json!(format!("http://nobody.localhost:{port}/")),
        "{v}"
    );
    // Byte for byte the same sentence, with its own id in the two places an
    // id belongs.
    assert_eq!(
        v["message"].as_str().unwrap().replace("nobody", "board"),
        installed["message"].as_str().unwrap(),
        "{v}"
    );
}

/// A loopback address that is not the test gateway's own, for the clients
/// whose peer address is asserted: `127.0.0.1` would be the right answer
/// whether lmgw read the TCP peer or wrote down its own listener. All of
/// `127.0.0.0/8` is loopback on Linux, so it needs no setup.
const SECOND_LOOPBACK: std::net::Ipv4Addr = std::net::Ipv4Addr::new(127, 0, 0, 2);

/// Nothing of lmgw's reaches the container, and the `X-Forwarded-*` headers
/// and `X-Lmgw-Face` the app is told to trust are lmgw's own (§4.6).
#[tokio::test]
async fn the_proxy_owns_the_forwarded_headers_and_carries_no_credential_in() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let port = base.addr().port();
    // What a browser on this box is, from an address the gateway is not
    // listening on.
    let client = reqwest::Client::builder()
        .resolve("board.localhost", base.addr())
        .local_address(std::net::IpAddr::V4(SECOND_LOOPBACK))
        .build()
        .unwrap();

    let resp = client
        .get(format!("{}/seen", base.origin("board.localhost")))
        // A browser does not attach the dashboard's cookie to another site and
        // cannot be made to; these are sent by hand because the stripping is
        // defence in depth — whatever reaches this hop, the container has its
        // own credential and never sees lmgw's.
        .header("cookie", format!("lmgw_session={}", base.key))
        .header("authorization", format!("Bearer {}", base.key))
        // A client's own forwarded headers. Dropped, never appended to: the
        // app is told to trust these, so lmgw owns them.
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "192.168.1.1")
        .header("x-forwarded-prefix", "/somewhere")
        .header("forwarded", "host=evil.example")
        // A client naming the Admin-gated face itself, on the public one.
        .header("x-lmgw-face", "mcp")
        .header("x-custom", "kept")
        .header("connection", "keep-alive")
        .send()
        .await
        .unwrap();
    let seen: Value = resp.json().await.unwrap();
    assert!(seen.get("cookie").is_none(), "{seen}");
    assert!(seen.get("authorization").is_none(), "{seen}");
    // Hop-by-hop stripped, everything else carried.
    assert!(seen.get("keep-alive").is_none(), "{seen}");
    assert_eq!(seen["x-custom"], json!("kept"), "{seen}");
    // Exactly one of each, and lmgw's: a second value would come back joined
    // onto the first.
    assert_eq!(
        seen["x-forwarded-host"],
        json!(format!("board.localhost:{port}")),
        "{seen}"
    );
    assert_eq!(seen["x-forwarded-proto"], json!("http"), "{seen}");
    // The TCP peer, not the client's `192.168.1.1` and not the listener's own
    // address; and the face this request really came in on.
    assert_eq!(
        seen["x-forwarded-for"],
        json!(SECOND_LOOPBACK.to_string()),
        "{seen}"
    );
    assert_eq!(seen["x-lmgw-face"], json!("app"), "{seen}");
    assert!(seen.get("forwarded").is_none(), "{seen}");
    // The app is mounted at `/`, so there is no prefix to tell it about (§4.4).
    assert!(seen.get("x-forwarded-prefix").is_none(), "{seen}");
    // The `Host` is the one reqwest dialled — the published loopback port —
    // which is exactly why the public one travels in the pair above.
    assert!(
        seen["host"].as_str().unwrap().starts_with("127.0.0.1:"),
        "{seen}"
    );

    service::stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn an_sse_body_arrives_chunk_by_chunk_rather_than_at_the_end() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    let started = Instant::now();
    let resp = base
        .origin_client(&["board.localhost"])
        .get(format!("{}/sse", base.origin("board.localhost")))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let mut stream = resp.bytes_stream();
    let mut first: Option<Duration> = None;
    let mut all = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if first.is_none() {
            first = Some(started.elapsed());
        }
        all.push_str(&String::from_utf8_lossy(&chunk));
    }
    let total = started.elapsed();
    assert!(all.contains("tick 1") && all.contains("tick 3"), "{all}");
    let first = first.expect("at least one chunk");
    // Three events 200 ms apart: the first has to be here long before the last.
    assert!(
        first < total / 2,
        "the body was buffered: first chunk at {first:?}, stream ended at {total:?}"
    );

    service::stop(&state, "board", "test over").await;
}

/// The upgrade path, proven as what it is: a byte pipe — and dispatched by
/// `Host` like every other method on the origin (§4.3).
#[tokio::test]
async fn an_upgrade_is_tunnelled_byte_for_byte() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    let host = format!("board.localhost:{}", base.addr().port());
    let (head, echoed) = raw_upgrade(&base, &host, "/ws", b"ping").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let lower = head.to_lowercase();
    assert!(
        lower.contains("upgrade: websocket"),
        "the 101's own headers came back: {head}"
    );
    assert_eq!(echoed, b"echo:ping");
    // An upgrade is an agent-origin request like any other: lmgw's face and
    // the real peer, and never the ones the client wrote into its head.
    assert!(lower.contains("\r\nx-seen-face: app\r\n"), "{head}");
    assert!(
        lower.contains(&format!("\r\nx-seen-for: {SECOND_LOOPBACK}\r\n")),
        "{head}"
    );

    service::stop(&state, "board", "test over").await;
}

/// Open a raw HTTP/1.1 upgrade, then speak bytes over it.
///
/// From [`SECOND_LOOPBACK`], and with a client's own `X-Lmgw-Face` and
/// `X-Forwarded-For` in the head, so the far side can tell lmgw's values from
/// the ones it was sent and from the listener's own address.
async fn raw_upgrade(base: &Gw, host: &str, path: &str, payload: &[u8]) -> (String, Vec<u8>) {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind((SECOND_LOOPBACK, 0).into()).unwrap();
    let mut sock = sock.connect(base.addr()).await.unwrap();
    let req = upgrade_head(host, path).replacen(
        "\r\n\r\n",
        "\r\nX-Lmgw-Face: mcp\r\nX-Forwarded-For: 192.168.1.1\r\n\r\n",
        1,
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    // Read until the end of the response head, keeping whatever came after it.
    let mut buf = Vec::new();
    let head_end = loop {
        let mut b = [0u8; 1024];
        let n = sock.read(&mut b).await.unwrap();
        assert!(n > 0, "the connection closed before the response head");
        buf.extend_from_slice(&b[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    sock.write_all(payload).await.unwrap();
    let mut back = buf[head_end..].to_vec();
    while back.len() < 5 + payload.len() {
        let mut b = [0u8; 1024];
        let n = sock.read(&mut b).await.unwrap();
        if n == 0 {
            break;
        }
        back.extend_from_slice(&b[..n]);
    }
    (head, back)
}

/// A 3xx is the app's answer, not a hop for lmgw to take (§3.3) — and the one
/// `Location` the proxy still rewrites is the one the browser cannot follow
/// (§4.5).
///
/// The shared `state.http` follows up to ten redirects and does not confine
/// them to the origin it started on, which through a proxy means every 3xx is
/// collapsed into whatever the last hop said — and a `Location:` the container
/// chooses turns lmgw into an arbitrary-URL fetcher answering on the agent's
/// origin. The proxy has its own client for exactly this.
#[tokio::test]
async fn a_redirect_is_forwarded_and_only_the_containers_own_origin_is_rewritten() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let client = base.origin_client_no_redirect(&["board.localhost"]);
    let origin = base.origin("board.localhost");

    // Origin-relative, and right as it is: the app is mounted at `/` on its
    // own origin, so `/login` already names the page it means (§4.4).
    let resp = client.get(format!("{origin}/go")).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the 302 was followed, not forwarded"
    );
    assert_eq!(resp.headers().get("location").unwrap(), "/login?next=%2F");

    // The container's own published loopback origin, which the browser is not
    // talking to and must never be told about: rewritten onto the agent
    // origin, path and query kept.
    let resp = client
        .get(format!("{origin}/go-self"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        &format!("{origin}/after?x=1")
    );

    // An absolute URL somewhere else is the app sending its user away: untouched.
    let resp = client.get(format!("{origin}/go-abs")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "https://example.com/elsewhere"
    );

    service::stop(&state, "board", "test over").await;
}

/// The four ways a decoded `{*rest}` re-assembled by hand goes wrong (§3.3),
/// and what a literal dot segment does now that nothing refuses one (§4.4).
///
/// Asserted from the **container's** side: what it received is the only honest
/// account of what the proxy sent.
#[tokio::test]
async fn a_percent_encoded_path_reaches_the_container_byte_for_byte() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;
    let origin = base.origin("board.localhost");
    let app_client = base.origin_client(&["board.localhost"]);
    let mcp_client = base.client();

    for face in ["app", "mcp"] {
        // The MCP face prepends the manifest's own path, so the container sees
        // `/mcp` + whatever came after `/agents/board/mcp`. The UI face is an
        // origin and the container sees the path as sent.
        let head = if face == "app" { "" } else { "/mcp" };
        let at = |rest: &str| match face {
            "app" => format!("{origin}{rest}"),
            _ => format!("{base}/agents/board/mcp{rest}"),
        };
        let client = if face == "app" {
            &app_client
        } else {
            &mcp_client
        };

        // 1. An encoded `?` must not become a query delimiter, and the real
        //    query must survive.
        let seen: Value =
            serde_json::from_str(&get_raw(client, &at("/echo/a%3Fevil=1?real=1")).await)
                .unwrap_or_else(|_| panic!("{face}: not JSON"));
        assert_eq!(
            seen["path"],
            json!(format!("{head}/echo/a%3Fevil=1")),
            "{face}"
        );
        assert_eq!(seen["query"], json!("real=1"), "{face}");

        // 2. An encoded `#` must not truncate the query.
        let seen: Value =
            serde_json::from_str(&get_raw(client, &at("/echo/a%23frag?real=1")).await).unwrap();
        assert_eq!(
            seen["path"],
            json!(format!("{head}/echo/a%23frag")),
            "{face}"
        );
        assert_eq!(seen["query"], json!("real=1"), "{face}");

        // 3. An encoded slash stays encoded: it is one segment, not two.
        let seen: Value = serde_json::from_str(&get_raw(client, &at("/echo/a%2Fb")).await).unwrap();
        assert_eq!(seen["path"], json!(format!("{head}/echo/a%2Fb")), "{face}");

        // 4. An **encoded** dot segment is one ordinary segment that happens to
        //    spell one, and is forwarded verbatim — at this layer it is not a
        //    segment separator at all.
        let seen: Value =
            serde_json::from_str(&get_raw(client, &at("/echo/%2e%2e%2fsecret")).await).unwrap();
        assert_eq!(
            seen["path"],
            json!(format!("{head}/echo/%2e%2e%2fsecret")),
            "{face}"
        );
    }

    // 5. A **literal** dot segment is no longer refused: there is no mount to
    //    leave, so it is the container's to interpret (§4.4). Sent over a raw
    //    socket, because `reqwest` resolves one client-side before it ever
    //    leaves — and the URL the proxy dials resolves it the same way, against
    //    the **container's** own root.
    let host = format!("board.localhost:{}", base.addr().port());
    let raw = raw_get(&base, &host, "/echo/a/../b?real=1").await;
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    assert!(raw.contains(r#""path":"/echo/b""#), "{raw}");
    assert!(raw.contains(r#""query":"real=1""#), "{raw}");

    // Which is also why it cannot climb out: `..` past the container's root is
    // still the container's root, and lmgw's own routes are on another origin
    // entirely. The container answers its own 404.
    let raw = raw_get(&base, &host, "/../../v1/models").await;
    assert!(raw.starts_with("HTTP/1.1 404"), "{raw}");
    assert!(
        !raw.contains("\"object\":\"list\""),
        "lmgw answered its own route: {raw}"
    );

    service::stop(&state, "board", "test over").await;
}

/// A container that dies after its probe passed must not 502 for ever (§3.3).
#[tokio::test]
async fn a_container_that_stops_answering_is_collected_and_restarted() {
    // `idle_seconds = 0`: nothing else would ever clear the entry, which is
    // what made this permanent.
    let (state, base, calls, kill) = with(Fake::default()).await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    assert_eq!(app_get(&base, "board.localhost", "/").await.0, 200);
    assert_eq!(run_count(&calls), 1);

    // The container goes away underneath us.
    if let Some(tx) = kill.lock().unwrap().take() {
        let _ = tx.send(());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 502, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["code"], json!("agent_service_unreachable"), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("starts a fresh one"),
        "{v}"
    );
    // The entry is gone, so the next request starts one rather than 502ing for
    // as long as the process lives.
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(run_count(&calls), 2);

    service::stop(&state, "board", "test over").await;
}

/// A start in flight is stoppable (§3.3).
#[tokio::test]
async fn stop_cancels_a_start_in_flight_and_says_so() {
    let (state, base, calls, _kill) = with(Fake::slow(1500)).await;
    install(&base, &doc("localhost/board:1", 0, 30, "")).await;

    // A request that will sit in the start for a while.
    let b = base.clone();
    let waiter = tokio::spawn(async move { app_get(&b, "board.localhost", "/").await });
    // Let the claim happen.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        detail(&base, "board").await["service"]["starting"],
        json!(true)
    );

    let (status, v) = op(&base, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["cancelled_start"], json!(true), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("a start was cancelled"),
        "{v}"
    );
    // The waiter is told the start did not finish, rather than being handed a
    // container nobody is tracking. It came in on the **agent origin**, which
    // carries no credential, so what it is told is the public sentence and not
    // the reason (§3.9) — the owner pressed the Stop button and already knows.
    let (status, body) = waiter.await.unwrap();
    assert_eq!(status, 503, "{body}");
    let e: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(e["code"], json!("agent_service_starting"), "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("could not be started"),
        "{e}"
    );
    assert!(
        e["message"].as_str().unwrap().contains("App tab"),
        "the public answer points at where the reason is: {e}"
    );
    // And the container the start created was collected.
    let verbs: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|a| a.first().cloned())
        .collect();
    assert!(verbs.contains(&"rm".to_string()), "{verbs:?}");
    assert_eq!(
        detail(&base, "board").await["service"]["running"],
        json!(false)
    );

    service::stop(&state, "board", "test over").await;
}

/// `start_timeout_seconds = 0` waits as long as the container takes — and Stop
/// still works, which is what makes an unbounded wait a setting rather than a
/// wedge (§3.3).
#[tokio::test]
async fn an_unbounded_start_is_still_stoppable() {
    let (state, base, _calls, _kill) = with(Fake::tcp_only()).await;
    // A container that accepts TCP and answers nothing, probed over HTTP with
    // no timeout at all.
    install(&base, &doc("localhost/board:1", 0, 0, "")).await;

    let b = base.clone();
    let waiter = tokio::spawn(async move { app_get(&b, "board.localhost", "/").await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        detail(&base, "board").await["service"]["starting"],
        json!(true)
    );

    let (status, v) = op(&base, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["cancelled_start"], json!(true), "{v}");
    let (status, _) = waiter.await.unwrap();
    assert_eq!(status, 503);

    service::stop(&state, "board", "test over").await;
}

// ---------------------------------------------------------------------------
// Idle (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_container_stops_after_its_idle_window_and_restarts_on_the_next_request() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 1, 10, "")).await;

    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);
    assert_eq!(run_count(&calls), 1);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        service::sweep_idle(&state).await.len(),
        1,
        "the window passed"
    );
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");

    // On demand again — the whole point of the idle stop being safe.
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("served by the container"), "{body}");
    assert_eq!(run_count(&calls), 2, "it started a second time");

    service::stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn a_request_in_flight_holds_the_idle_stop_off() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 1, 10, "")).await;

    // `/slow` streams for ~1.4 s, longer than the 1 s idle window.
    let resp = base
        .origin_client(&["board.localhost"])
        .get(format!("{}/slow", base.origin("board.localhost")))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"chunk\n");

    // The window has passed on the clock, and the sweep still must not touch
    // it: the counter, not the clock, is what says a request is in flight.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        service::sweep_idle(&state).await.is_empty(),
        "a streaming response was torn down mid-flight"
    );

    // Finish it, and the window starts from there.
    while stream.next().await.is_some() {}
    assert!(service::sweep_idle(&state).await.is_empty());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(service::sweep_idle(&state).await.len(), 1);
}

#[tokio::test]
async fn stop_and_start_are_ops_the_app_tab_can_press() {
    let (_state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    let (status, v) = op(&base, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["running"], json!(true), "{v}");
    // Where it is now answering (§7): the published host port is lmgw's
    // business, the origin is the reader's.
    let port = base.base.rsplit(':').next().unwrap();
    assert_eq!(
        v["origin"],
        json!(format!("http://board.localhost:{port}/")),
        "{v}"
    );
    assert_eq!(run_count(&calls), 1);

    let (status, v) = op(&base, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["running"], json!(false), "{v}");
    // Stopping twice is success: "make sure this is not running" is what the
    // button means.
    let (status, v) = op(&base, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["stopped"], Value::Null, "{v}");
}

/// §12's open question, answered: rotation stops the container that is holding
/// the token it just invalidated.
#[tokio::test]
async fn rotating_the_token_stops_the_running_service_and_says_so() {
    let (_state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);

    let (status, v) = op(&base, "agent_token_rotate", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{v}");
    assert!(
        v["service_stopped"]
            .as_str()
            .unwrap_or_default()
            .contains("agentsvc-board"),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("was holding it"),
        "{v}"
    );
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");

    // And the next request starts it again, with the new token.
    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);
    assert_eq!(run_count(&calls), 2);
}

/// A service start is a start like any other (mounts §5.5): the folder is on
/// its argv with the shared label, and `--userns=keep-id` is there because the
/// manifest declares a slot.
#[tokio::test]
async fn a_service_start_carries_the_mount_and_keep_id() {
    let (_state, base, calls) = with_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path())
        .unwrap()
        .display()
        .to_string();
    install(&base, &mount_doc()).await;
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);
    let argv = calls
        .lock()
        .unwrap()
        .iter()
        .find(|a| a.first().map(String::as_str) == Some("run"))
        .cloned()
        .expect("the service container was started");
    assert!(argv.iter().any(|a| a == "--userns=keep-id"), "{argv:?}");
    assert!(
        argv.contains(&format!("{notes}:/lmgw/mounts/notes:rw,z")),
        "{argv:?}"
    );
    // And `input.json` reads the same whichever half of the image is running.
    let i = argv.iter().position(|a| a == "-v").unwrap();
    let host = argv[i + 1].split(':').next().unwrap();
    let input: Value = serde_json::from_str(&std::fs::read_to_string(host).unwrap()).unwrap();
    assert_eq!(input["config"]["notes"], "/lmgw/mounts/notes", "{input}");
    assert_eq!(
        input["mounts"],
        json!([{ "field": "notes", "path": "/lmgw/mounts/notes",
                 "kind": "directory", "access": "rw" }]),
        "{input}"
    );
    assert!(!input.to_string().contains(&notes), "{input}");
}

/// The use-time check on this side too (§5.3): a folder that has gone since it
/// was saved fails the start with its code, and podman is never run.
///
/// And the two readers of that failure are not the same person (mounts §5.3,
/// principals §3.9). The agent origin asks for **no credential** — anyone who
/// can reach the port and send `Host: board.localhost` is a reader — so the
/// refusal's own sentence, which names a folder on this machine, is not what
/// goes out there. The owner reads it on the Admin path, whole.
#[tokio::test]
async fn a_service_whose_folder_went_away_is_refused_before_podman() {
    let (_state, base, calls) = with_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path())
        .unwrap()
        .display()
        .to_string();
    install(&base, &mount_doc()).await;
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    std::fs::remove_dir_all(tmp.path()).unwrap();

    // The public origin: that it did not start, and where its owner can find
    // out why. No path, no code, nothing about a mount at all.
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 503, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["code"], json!("agent_service_starting"), "{v}");
    assert_eq!(
        v["message"],
        json!(
            "the app for 'board' could not be started. Its owner can see why on this agent's \
             App tab in the lmgw dashboard."
        ),
        "{v}"
    );
    assert!(
        !body.contains(&notes),
        "the host path went out unauthenticated: {body}"
    );
    assert!(!body.contains("mount_path_missing"), "{body}");

    // The Admin path — the App tab's own Start button — says everything.
    let (status, v) = op(&base, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 400, "{v}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(message.contains("mount_path_missing"), "{v}");
    assert!(
        message.contains(&notes),
        "the owner reads their own path: {v}"
    );
    assert!(
        message.contains("clear the field or point it at a folder that exists"),
        "the refusal ends with the way out: {v}"
    );

    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("run"))
            .count(),
        0,
        "podman must not be reached"
    );
}

/// A service container binds what the **stored** config says, so re-pointing a
/// mount is the tenth reason one is stopped (mounts §5.7).
#[tokio::test]
async fn re_pointing_a_mount_stops_the_app_container_holding_it() {
    let (state, base, _calls) = with_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (first, second) = (root.join("one"), root.join("two"));
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();

    install(&base, &mount_doc()).await;
    let bind = |path: &std::path::Path| json!({ "id": "board", "values": { "notes": path.display().to_string() } });
    let (status, v) = op(&base, "agent_config_set", bind(&first)).await;
    assert_eq!(status, 200, "{v}");
    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);

    // The same value again is not a change, and nothing is stopped for it.
    let (status, v) = op(&base, "agent_config_set", bind(&first)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["mounts_repointed"], json!([]), "{v}");
    assert_eq!(v["service_stopped"], Value::Null, "{v}");
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(true), "{d}");

    // A different folder is: the container is holding the old one.
    let (status, v) = op(&base, "agent_config_set", bind(&second)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["mounts_repointed"], json!(["notes"]), "{v}");
    assert!(
        v["service_stopped"]
            .as_str()
            .unwrap_or_default()
            .contains("agentsvc-board"),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("was stopped"),
        "{v}"
    );
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
    assert_eq!(
        d["config"]["notes"],
        json!(second.display().to_string()),
        "{d}"
    );

    service::stop(&state, "board", "test over").await;
}

/// A manifest replace stops the container it no longer describes.
#[tokio::test]
async fn replacing_the_manifest_stops_the_container_it_started() {
    let (_state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let (status, _) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200);

    let (status, v) = op(
        &base,
        "agent_set",
        json!({ "manifest": doc("localhost/board:2", 0, 10, ""), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let warnings = v["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("was stopped")),
        "{v}"
    );
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
}

// ---------------------------------------------------------------------------
// `provides.mcp` (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_chat_thread_attaching_the_agents_label_sees_the_containers_tool() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;

    // The row the agent lifecycle wrote, pointing at lmgw's own proxy.
    let servers: Value = serde_json::from_str(&get(&base, "/api/mcp-servers").await.1).unwrap();
    let row = servers["mcp_servers"]
        .as_array()
        .unwrap_or_else(|| panic!("/api/mcp-servers: {servers}"))
        .iter()
        .find(|s| s["name"] == json!("agent:board"))
        .unwrap_or_else(|| panic!("no agent:board row: {servers}"));
    assert_eq!(row["tool_prefix"], json!("board"), "{row}");
    assert_eq!(row["agent_id"], json!("board"), "{row}");
    assert_eq!(row["autostart"], json!(false), "{row}");
    assert!(
        row["url"].as_str().unwrap().ends_with("/agents/board/mcp"),
        "{row}"
    );
    // Nothing has started yet: an MCP row is not a reason to run a container.
    assert_eq!(run_count(&calls), 0);

    // And an **aggregate** `tools/list` must not become one either (§3.3): the
    // lazy-list contract connects every enabled server that is not Ready, which
    // for an agent row means `podman run` — so every MCP client handshake,
    // every chat turn carrying tools and every load of the MCP page would start
    // every service agent on the box.
    let agg = state.mcp.list_tools(&state.snapshot()).await;
    assert_eq!(
        run_count(&calls),
        0,
        "an aggregate tools/list started a container"
    );
    assert!(
        !agg.tools.iter().any(|t| t.name == "board__pin"),
        "a sleeping agent's tools are not in the aggregate"
    );
    // The MCP page says why, rather than showing it as a server that failed.
    let servers: Value = serde_json::from_str(&get(&base, "/api/mcp-servers").await.1).unwrap();
    let row = servers["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("agent:board"))
        .unwrap()
        .clone();
    assert!(
        row["status_detail"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sleeping"),
        "{row}"
    );

    // Exactly what a chat thread's send path does with the label it attached.
    let thread = base
        .client()
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "m1" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().expect("a thread id");
    let saved = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&json!({ "mcp_tools": [{ "server_label": "board" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status().as_u16(), 200);

    let specs = vec![lmgw_core::ingress::responses::McpToolSpec {
        server_label: "board".to_string(),
        allowed_tools: None,
        require_approval: Default::default(),
    }];
    let resolved =
        lmgw_core::mcp::exec::resolve(&state, &specs, &lmgw_core::mcp::scope::ToolScope::gateway())
            .await;
    assert!(
        resolved.failed.is_empty(),
        "the label did not resolve: {:?}",
        resolved.failed
    );
    let names: Vec<String> = resolved.tools.iter().map(|t| t.def.name.clone()).collect();
    assert!(
        names.iter().any(|n| n == "board__pin"),
        "the container's tool is missing: {names:?}"
    );
    // Attaching the label *is* the ask: the container started, and listing its
    // tools went through the proxy — the on-demand contract, applied to the MCP
    // face.
    assert_eq!(run_count(&calls), 1);

    service::stop(&state, "board", "test over").await;
}

/// The other ask that starts a sleeping agent: a `tools/call` naming one of its
/// tools (§3.3).
#[tokio::test]
async fn a_tools_call_on_a_sleeping_agents_tool_starts_it() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;
    assert_eq!(run_count(&calls), 0);

    let snap = state.snapshot();
    let (result, server) = state
        .mcp
        .call(&snap, "board__pin", None)
        .await
        .unwrap_or_else(|e| panic!("the call did not reach the container: {e:?}"));
    assert_eq!(server, "agent:board");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(run_count(&calls), 1, "the call started the container");

    service::stop(&state, "board", "test over").await;
}

/// A manifest write keeps lmgw's three fields true and leaves the rest of the
/// row as the owner set it (§3.3).
#[tokio::test]
async fn a_manifest_write_does_not_revert_the_owners_edits_to_the_row() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 300, 10, PROVIDES)).await;
    let row_id = store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "agent:board")
        .expect("the agent's row")
        .id;
    // The default is the one the MCP page gives any new row, not a number this
    // path invented.
    let before = store::get_mcp_server(&state.db, row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.timeout_ms, 60_000);

    // The owner tunes it on the MCP page.
    let (status, v) = op(
        &base,
        "mcp_server_set",
        json!({ "action": "update", "id": row_id, "timeout_ms": 5_000, "idle_seconds": 30 }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    // A manifest save must not undo that.
    install(&base, &doc("localhost/board:2", 300, 10, PROVIDES)).await;
    let after = store::get_mcp_server(&state.db, row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.timeout_ms, 5_000, "the owner's timeout was reverted");
    assert_eq!(
        after.idle_seconds, 30,
        "the owner's idle window was reverted"
    );
    assert_eq!(after.agent_id.as_deref(), Some("board"));
    assert!(after.url.unwrap().ends_with("/agents/board/mcp"));
}

/// The one field of an agent's row that is not the owner's to edit: its URL
/// (principals §10 Part 1).
///
/// That URL is what makes the dial carry the `owner:dashboard` bearer, so
/// re-pointing the row is not an edit to a string — it is a request to post
/// the owner's door key to an address of the caller's choosing, and every
/// `Admin` caller includes a model driving `/mcp/admin`. Refused at the front
/// door, and the dial checks the address again regardless.
#[tokio::test]
async fn an_agent_rows_url_is_derived_and_cannot_be_set() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 300, 10, PROVIDES)).await;
    let row = store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "agent:board")
        .expect("the agent's row");
    let derived = row.url.clone();

    let (status, v) = op(
        &base,
        "mcp_server_set",
        json!({ "action": "update", "id": row.id, "url": "http://attacker.example/mcp" }),
    )
    .await;
    assert_ne!(status, 200, "{v}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(message.contains("derived"), "{v}");
    assert!(message.contains("board"), "{v}");

    let after = store::get_mcp_server(&state.db, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.url, derived,
        "the row still points at lmgw's own proxy"
    );

    // Everything else on the row is still the owner's, so the refusal is
    // about the one field and not about the row being read-only.
    let (status, v) = op(
        &base,
        "mcp_server_set",
        json!({ "action": "update", "id": row.id, "timeout_ms": 5_000 }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
}

/// The row's url is built from `bind_addr`, so a bind address that moves has to
/// take every agent row with it (§3.3).
#[tokio::test]
async fn a_bind_address_change_re_points_every_agent_row() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;

    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "bind_addr": "127.0.0.1:9999" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["message"].as_str().unwrap().contains("re-pointed"), "{v}");
    let row = store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "agent:board")
        .unwrap();
    assert_eq!(
        row.url.as_deref(),
        Some("http://127.0.0.1:9999/agents/board/mcp"),
        "the row still points at the old bind address"
    );
}

/// The reserved namespace, through the real op rather than the pure function:
/// an owner-created `agent:*` row would be adopted and then deleted by the
/// next write to an agent with that id (§3.3).
#[tokio::test]
async fn an_owner_cannot_create_a_row_in_the_agent_namespace() {
    let (_state, base, _calls) = with_fake().await;
    let (status, v) = op(
        &base,
        "mcp_server_set",
        json!({
            "action": "create",
            "name": "agent:board",
            "transport": "http",
            "url": "https://example.com/mcp",
        }),
    )
    .await;
    assert_ne!(status, 200, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved 'agent:' prefix"),
        "{v}"
    );

    // And the name lmgw itself uses is accepted from the agent lifecycle,
    // which is the asymmetry the reservation exists to create.
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;
    let servers: Value = serde_json::from_str(&get(&base, "/api/mcp-servers").await.1).unwrap();
    assert!(
        servers["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == json!("agent:board")),
        "{servers}"
    );
}

// ---------------------------------------------------------------------------
// The agent origin (origins §4.1, §4.10)
// ---------------------------------------------------------------------------

/// A manifest with the given id, `run.service` optional: the origin rules
/// apply to the agents that have an origin and to no others.
fn origin_doc(id: &str, service: bool) -> String {
    let block = if service {
        r#", "service": { "port": 8080, "idle_seconds": 0, "start_timeout_seconds": 10 }"#
    } else {
        ""
    };
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "An agent",
  "model": {{ "alias": "m1" }},
  "run": {{ "kind": "container", "image": "localhost/x:1"{block} }}
}}"#
    )
}

/// Every shape the `Host` dispatch has to get right (§4.2).
///
/// Over a raw socket throughout: a `Host` no client library will send
/// unmodified is exactly what this is about.
#[tokio::test]
async fn the_host_dispatch_reads_an_agent_origin_and_lets_every_other_host_through() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    // A second agent with no app at all: an id nobody serves is nobody's host
    // name (§4.1).
    let (status, v) = op(
        &base,
        "agent_set",
        json!({ "manifest": origin_doc("plain", false) }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let port = base.addr().port();

    // With the port, without it, in whatever case it was typed in, and in the
    // absolute form with the rooting dot — DNS treats all of these as one
    // name and so does this. A dot that fell through here would put the
    // dashboard, login card and all, on an agent's own host name.
    for host in [
        format!("board.localhost:{port}"),
        "board.localhost".to_string(),
        format!("BOARD.LocalHost:{port}"),
        format!("board.localhost.:{port}"),
        "board.localhost.".to_string(),
        format!("BOARD.LOCALHOST.:{port}"),
    ] {
        let raw = raw_get(&base, &host, "/").await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{host}: {raw}");
        assert!(raw.contains("served by the container"), "{host}: {raw}");
    }

    // A bracketed IPv6 literal and a bare address are addresses, not origins:
    // they fall through to the main router untouched.
    for host in [format!("[::1]:{port}"), format!("127.0.0.1:{port}")] {
        let raw = raw_get(&base, &host, "/api/version").await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{host}: {raw}");
        assert!(raw.contains("version"), "{host}: {raw}");
    }

    // A label nothing answers for, and a label that is an agent with no app:
    // a JSON `404` naming the label. Never the SPA — a name that is not the
    // dashboard's must not be served the dashboard. And the *same* JSON for
    // both, because this answer needs no credential and "no agent has that
    // id" told apart from "that agent serves no app" is a free inventory
    // (F9).
    let mut bodies = Vec::new();
    for (host, label) in [
        (format!("nope.localhost:{port}"), "nope"),
        (format!("plain.localhost:{port}"), "plain"),
        // The absolute form of a name nobody serves is still not the SPA.
        (format!("nope.localhost.:{port}"), "nope"),
    ] {
        let raw = raw_get(&base, &host, "/").await;
        assert!(raw.starts_with("HTTP/1.1 404"), "{host}: {raw}");
        assert!(raw.contains(r#""code":"not_found""#), "{host}: {raw}");
        assert!(raw.contains(label), "{host}: {raw}");
        assert!(
            !raw.contains("<!DOCTYPE html>"),
            "the SPA answered on an agent origin: {raw}"
        );
        bodies.push(
            raw.split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .replace(label, "<id>"),
        );
    }
    assert_eq!(
        bodies[0], bodies[1],
        "the origin 404 says which ids are installed"
    );

    service::stop(&state, "board", "test over").await;
}

/// The dispatch sits **outside** `CorsLayer` and inside the trace (§3.11): a
/// foreign page can navigate a browser to an agent origin — every browser
/// resolves `*.localhost` to loopback — and still cannot read the answer.
#[tokio::test]
async fn an_agent_origin_answers_without_cors_and_the_dashboard_still_answers_with_it() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;

    let resp = base
        .origin_client(&["board.localhost"])
        .get(base.origin("board.localhost"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "an agent origin answered with CORS: {:?}",
        resp.headers()
    );

    // The main router keeps it, as today: on `/v1` and `/mcp` it is what lets
    // a browser-side client call the gateway with a bearer.
    let resp = base
        .anon()
        .get(format!("{base}/api/version"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("*")
    );

    service::stop(&state, "board", "test over").await;
}

/// The MCP face stays a path on the main origin, gated on the owner — and the
/// credential that opens it never reaches the container (§4.8).
#[tokio::test]
async fn the_mcp_face_is_the_owners_and_its_container_never_sees_the_key() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;
    let port = base.addr().port();

    // The row stores no credential of its own: it is exported and read back on
    // the MCP page, so the bearer is attached at connect time instead.
    let servers: Value = serde_json::from_str(&get(&base, "/api/mcp-servers").await.1).unwrap();
    let row = servers["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("agent:board"))
        .unwrap_or_else(|| panic!("no agent:board row: {servers}"))
        .clone();
    assert_eq!(row["headers"], json!(""), "{row}");

    // With no principal it is the gate that refuses, not the container.
    let resp = base
        .anon()
        .post(format!("{base}/agents/board/mcp"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // And the owner's bearer — what lmgw's own MCP client presents at connect
    // time — is stripped before the container sees the request (§4.6). The
    // client names the other face and an address of its own; lmgw replaces
    // both. From [`SECOND_LOOPBACK`], so the address is visibly the peer's.
    let seen: Value = reqwest::Client::builder()
        .local_address(std::net::IpAddr::V4(SECOND_LOOPBACK))
        .build()
        .unwrap()
        .get(format!("{base}/agents/board/mcp/seen"))
        .bearer_auth(&base.key)
        .header("x-lmgw-face", "app")
        .header("x-forwarded-for", "192.168.1.1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(seen.get("authorization").is_none(), "{seen}");
    assert!(seen.get("cookie").is_none(), "{seen}");
    assert_eq!(
        seen["x-forwarded-host"],
        json!(format!("board.localhost:{port}")),
        "{seen}"
    );
    // The face, which is what tells the container this is the Admin-gated
    // call and not the same path sent to its origin — and the peer, which on
    // this face is whoever passed that gate (lmgw's own client, in practice).
    assert_eq!(seen["x-lmgw-face"], json!("mcp"), "{seen}");
    assert_eq!(
        seen["x-forwarded-for"],
        json!(SECOND_LOOPBACK.to_string()),
        "{seen}"
    );

    // The same path sent to the agent's **origin** is the public face, and
    // says so, whatever the client claims.
    let seen: Value = base
        .origin_client(&["board.localhost"])
        .get(format!("{}/mcp/seen", base.origin("board.localhost")))
        .header("x-lmgw-face", "mcp")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seen["x-lmgw-face"], json!("app"), "{seen}");
    assert_eq!(seen["x-forwarded-for"], json!("127.0.0.1"), "{seen}");

    service::stop(&state, "board", "test over").await;
}

/// The rebinding guard on `/mcp` gains one shape and keeps the rest (§4.2,
/// §12): an agent's own UI may call the gateway with a bearer its backend gave
/// it, and a page on `evil.example` may not.
#[tokio::test]
async fn the_mcp_origin_guard_admits_an_agent_origin_and_refuses_a_foreign_page() {
    let (_state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let port = base.addr().port();
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25" },
    });

    for origin in [
        format!("http://board.localhost:{port}"),
        "http://localhost".to_string(),
    ] {
        let resp = base
            .client()
            .post(format!("{base}/mcp"))
            .header("origin", &origin)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{origin}");
    }

    // A foreign page, and a label under the suffix that serves no app: neither
    // is an origin this gateway answers for.
    for origin in [
        "http://evil.example".to_string(),
        format!("http://nope.localhost:{port}"),
    ] {
        let resp = base
            .client()
            .post(format!("{base}/mcp"))
            .header("origin", &origin)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403, "{origin}");
    }
}

/// The port is part of an origin (§4.2, F6): `http://localhost:9999` is
/// another process on this box, and a page it serves is as foreign as one on
/// `evil.example`.
#[tokio::test]
async fn the_mcp_origin_guard_reads_the_port_and_keeps_the_agent_shape_off_the_admin_plane() {
    let (_state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let port = base.addr().port();
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25" },
    });

    // A right-looking name on a port lmgw is not listening on — the agent
    // shape and the loopback list both.
    for origin in [
        format!("http://board.localhost:{}", port.wrapping_add(1)),
        format!("http://localhost:{}", port.wrapping_add(1)),
        format!("http://127.0.0.1:{}", port.wrapping_add(1)),
    ] {
        let resp = base
            .client()
            .post(format!("{base}/mcp"))
            .header("origin", &origin)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403, "{origin}");
    }

    // The self-admin plane keeps the loopback list it had: an agent's page
    // gets the aggregate plane with the bearer its backend holds, and never
    // this one.
    async fn admin(base: &Gw, origin: String, init: &Value) -> String {
        let resp = base
            .client()
            .post(format!("{base}/mcp/admin"))
            .header("origin", &origin)
            .json(init)
            .send()
            .await
            .unwrap();
        resp.text().await.unwrap_or_default()
    }
    assert_eq!(
        admin(&base, format!("http://board.localhost:{port}"), &init).await,
        "origin not allowed"
    );
    assert_ne!(
        admin(&base, format!("http://localhost:{port}"), &init).await,
        "origin not allowed"
    );
}

/// The MCP face is still a path under lmgw's own origin, so a dot segment in
/// one is a request to leave that prefix — and `reqwest` would resolve it on
/// the way out, landing at `/admin` inside the container. Refused with
/// `bad_path`; the UI face, which is a whole origin, is not (§4.4).
#[tokio::test]
async fn a_dot_segment_in_the_mcp_path_is_refused_and_the_agent_origin_is_untouched() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;

    // Over a raw socket: no client library sends this path unmodified.
    for path in [
        "/agents/board/mcp/../admin",
        "/agents/board/mcp/./tools",
        "/agents/board/mcp/a/%2e%2e/../b",
    ] {
        let raw = raw_get(&base, &format!("127.0.0.1:{}", base.addr().port()), path).await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{path}: {raw}");
        assert!(raw.contains(r#""code":"bad_path""#), "{path}: {raw}");
    }

    // The same segments on the agent's own origin are the container's to
    // interpret: there is no prefix there to leave.
    let raw = raw_get(&base, "board.localhost", "/deep/../page").await;
    assert!(!raw.contains("bad_path"), "{raw}");
    assert!(!raw.starts_with("HTTP/1.1 400"), "{raw}");

    service::stop(&state, "board", "test over").await;
}

/// The id of a serving agent is also a DNS label, and `validate_id`'s 64
/// characters and trailing `-` are both one too far for one (§4.1).
#[tokio::test]
async fn a_service_id_that_is_not_a_dns_label_is_refused_with_its_code() {
    let (_state, base, _calls) = with_fake().await;
    for id in ["a".repeat(64), "board-".to_string()] {
        let (status, v) = op(
            &base,
            "agent_set",
            json!({ "manifest": origin_doc(&id, true) }),
        )
        .await;
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["code"], json!("origin_label_invalid"), "{v}");
        // The refusal names the rule rather than only the field.
        let m = v["message"].as_str().unwrap_or_default();
        assert!(m.contains("run.service"), "{v}");
        // ... and the same id with no app to serve is nobody's host name.
        let (status, v) = op(
            &base,
            "agent_set",
            json!({ "manifest": origin_doc(&id, false) }),
        )
        .await;
        assert_eq!(status, 200, "{v}");
    }
}

/// And the agent whose origin *is* the gateway's own address (§4.1).
#[tokio::test]
async fn an_agent_origin_that_takes_the_gateways_own_address_is_refused() {
    let (state, base, _calls) = with_fake().await;
    // A gateway reachable under a name rather than an IP, and a suffix that is
    // that name's tail: `board.lan` would then be both the dashboard and the
    // agent. Written straight to the settings because the address is a
    // fixture, not something the test is exercising.
    let mut settings = state.snapshot().settings.clone();
    settings.bind_addr = "board.lan:8001".into();
    settings.agent_origin_suffix = "lan".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, v) = op(
        &base,
        "agent_set",
        json!({ "manifest": origin_doc("board", true) }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("origin_shadows_gateway"), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("board.lan"),
        "{v}"
    );
    // Nothing was written: the refusal comes before the row.
    let (status, body) = get(&base, "/api/agents/board").await;
    assert_eq!(status, 404, "{body}");

    // Any other label under the same suffix is an ordinary agent.
    let (status, v) = op(
        &base,
        "agent_set",
        json!({ "manifest": origin_doc("cards", true) }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
}

/// The same rule from the settings end, with its own code (§4.1).
#[tokio::test]
async fn an_origin_suffix_that_shadows_the_gateway_is_refused() {
    let (_state, base, _calls) = with_fake().await;
    // The gateway answers on `127.0.0.1`, so `.1` is part of its own address.
    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "agent_origin_suffix": "1" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("origin_suffix_shadows_gateway"), "{v}");

    // The shape rules are ordinary input errors, and say what is wrong.
    for (bad, why) in [
        ("lmgw.lan:8001", "carries a port"),
        ("http://lmgw.lan", "not a URL"),
        ("", "cannot be empty"),
        ("lmgw_.lan", "contains '_'"),
    ] {
        let (status, v) = op(
            &base,
            "settings_set_full",
            json!({ "agent_origin_suffix": bad }),
        )
        .await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], json!("op_failed"), "{bad}: {v}");
        assert!(
            v["message"].as_str().unwrap_or_default().contains(why),
            "{bad}: {v}"
        );
    }

    // And a suffix that shadows nothing is saved, upper case and all.
    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "agent_origin_suffix": "Lmgw.Lan" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (_, body) = get(&base, "/api/settings-full").await;
    let full: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(full["agent_origin_suffix"], json!("lmgw.lan"), "{full}");
}

/// Origins are computed per request, so a suffix change resyncs nothing — but
/// a running container is holding the old one in its environment, and the
/// ninth reason a service container stops is exactly that (§4.10).
#[tokio::test]
async fn a_suffix_change_stops_every_running_app_container() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(run_count(&calls), 1);
    assert_eq!(
        detail(&base, "board").await["service"]["running"],
        json!(true)
    );

    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "agent_origin_suffix": "lmgw.lan" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(message.contains("old origin"), "{v}");
    assert!(message.contains("http://<id>.lmgw.lan"), "{v}");

    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
    let port = base.base.rsplit(':').next().unwrap();
    assert_eq!(
        d["service"]["origin"],
        json!(format!("http://board.lmgw.lan:{port}/")),
        "{d}"
    );
    // Saving the same suffix again is not a change, so nothing is stopped for
    // it — the next request starts the container under the new origin, which
    // is where the app answers from the moment the setting is saved.
    let (status, body) = app_get(&base, "board.lmgw.lan", "/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(run_count(&calls), 2);
    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "agent_origin_suffix": "lmgw.lan" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        detail(&base, "board").await["service"]["running"],
        json!(true),
        "an unchanged suffix stops nothing"
    );
    service::stop(&state, "board", "test over").await;
}

/// A `dev_url` stored before a dev server was an origin may carry a path, and
/// nothing strips one any more. The boot pass clears it and says why (§4.7).
/// The suffix is checked when it is typed, but `bind_addr`, this machine's
/// host name and its search domains are the other half of that rule and move
/// without it (§4.1, F3). Boot asks the question again — and says so rather
/// than resetting a value the owner chose.
#[tokio::test]
async fn a_stored_suffix_that_shadows_the_gateway_survives_boot_and_says_so() {
    let (state, base, _calls) = with_fake().await;
    // Straight into the settings row: the plane refuses this value, so the
    // only way a gateway holds one is a bind address that moved under it.
    let mut settings = state.snapshot().settings.clone();
    settings.agent_origin_suffix = "1".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    // Exactly the call `AppState::init` makes beside `revalidate_dev_urls`.
    let why = service::origin_suffix_warning(&state.snapshot().settings)
        .expect("the gateway answers on 127.0.0.1, so '1' is part of its own address");
    assert!(why.contains("127.0.0.1"), "{why}");

    // The setting is still what it was, and the dashboard reads the same
    // sentence the boot log carries.
    let (_, body) = get(&base, "/api/settings-full").await;
    let full: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(full["agent_origin_suffix"], json!("1"), "{full}");
    assert_eq!(full["agent_origin_suffix_warning"], json!(why), "{full}");

    // And a suffix that shadows nothing carries no warning at all.
    let (status, v) = op(
        &base,
        "settings_set_full",
        json!({ "agent_origin_suffix": "localhost" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (_, body) = get(&base, "/api/settings-full").await;
    let full: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(full["agent_origin_suffix_warning"], json!(null), "{full}");
}

#[tokio::test]
async fn a_stored_dev_url_carrying_a_path_is_cleared_at_boot() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    // Straight into the column: this is a row written by an older build, which
    // is the only way one of these exists.
    store::set_agent_dev_url(&state.db, "board", Some("http://127.0.0.1:5173/base"))
        .await
        .unwrap();

    // Exactly the call `AppState::init` makes after the seed and `resync_all`.
    let bind = state.snapshot().settings.bind_addr.clone();
    let cleared = service::revalidate_dev_urls(&state, &bind).await;
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!(cleared[0].0, "board");
    assert!(cleared[0].2.contains("cannot carry a path"), "{cleared:?}");

    let d = detail(&base, "board").await;
    assert_eq!(d["dev_url"], json!(""), "{d}");
    let warning = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == json!("dev_url_cleared"))
        .unwrap_or_else(|| panic!("no dev_url_cleared warning: {d}"))
        .clone();
    let m = warning["message"].as_str().unwrap_or_default();
    assert!(m.contains("http://127.0.0.1:5173/base"), "{warning}");
    assert!(m.contains("cannot carry a path"), "{warning}");

    // A dev_url that is an origin survives the same pass untouched.
    let (status, v) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": "http://127.0.0.1:5173" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(service::revalidate_dev_urls(&state, &bind).await.is_empty());
}

// ---------------------------------------------------------------------------
// The real thing (needs podman)
// ---------------------------------------------------------------------------

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Removes every container of this instance's prefix, and the image the test
/// built, whatever happened.
struct Sweep {
    prefix: String,
    image: String,
}

impl Drop for Sweep {
    fn drop(&mut self) {
        if let Ok(out) = std::process::Command::new("podman")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=lmgw.instance={}", self.prefix),
            ])
            .output()
        {
            for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let _ = std::process::Command::new("podman")
                    .args(["rm", "-f", id])
                    .output();
            }
        }
        let _ = std::process::Command::new("podman")
            .args(["rmi", "-f", &self.image])
            .output();
    }
}

/// The 60-line server the real image runs: a page, an SSE endpoint, a real
/// WebSocket and a minimal MCP endpoint.
const SERVER_MJS: &str = r#"
import http from 'node:http';
import crypto from 'node:crypto';

const origin = process.env.LMGW_APP_ORIGIN || '';
const port = Number(process.env.LMGW_PORT || 8080);

const mcp = (body) => {
  const req = JSON.parse(body || '{}');
  if (req.id === undefined) return null;
  const m = req.method;
  let result = {};
  if (m === 'initialize') {
    result = { protocolVersion: (req.params && req.params.protocolVersion) || '2025-06-18',
               capabilities: { tools: {} },
               serverInfo: { name: 'board', version: '0.1.0' } };
  } else if (m === 'tools/list') {
    result = { tools: [{ name: 'pin', description: 'pin a card to the board',
                         inputSchema: { type: 'object', properties: { text: { type: 'string' } } } }] };
  } else if (m === 'tools/call') {
    result = { content: [{ type: 'text', text: '{"pinned":true}' }],
               structuredContent: { pinned: true }, isError: false };
  }
  return JSON.stringify({ jsonrpc: '2.0', id: req.id, result });
};

const server = http.createServer((req, res) => {
  const p = new URL(req.url, 'http://x').pathname;
  if (p === '/') {
    res.writeHead(200, { 'content-type': 'text/html' });
    res.end('<h1>board</h1><p>origin=' + origin + '</p>');
  } else if (p === '/seen') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(req.headers));
  } else if (p === '/sse') {
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    let n = 0;
    const t = setInterval(() => {
      n += 1;
      res.write('data: tick ' + n + '\n\n');
      if (n >= 3) { clearInterval(t); res.end(); }
    }, 200);
    req.on('close', () => clearInterval(t));
  } else if (p === '/mcp' && req.method === 'POST') {
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      const out = mcp(body);
      if (out === null) { res.writeHead(202); res.end(); return; }
      res.writeHead(200, { 'content-type': 'application/json', 'mcp-session-id': 'board' });
      res.end(out);
    });
  } else {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('no');
  }
});

// A real handshake and one real frame each way: the accept is computed, the
// client's masked text frame is unmasked, and the reply is an unmasked one.
server.on('upgrade', (req, socket) => {
  const key = req.headers['sec-websocket-key'] || '';
  const accept = crypto.createHash('sha1')
    .update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n' +
               'Connection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
  socket.on('data', (buf) => {
    const len = buf[1] & 0x7f;
    const masked = (buf[1] & 0x80) !== 0;
    let off = 2;
    let mask = null;
    if (masked) { mask = buf.subarray(off, off + 4); off += 4; }
    const payload = buf.subarray(off, off + len);
    let text = '';
    for (let i = 0; i < len; i++) {
      text += String.fromCharCode(masked ? (payload[i] ^ mask[i % 4]) : payload[i]);
    }
    const body = Buffer.from('echo:' + text, 'utf8');
    socket.write(Buffer.concat([Buffer.from([0x81, body.length]), body]));
  });
});

server.listen(port, '0.0.0.0', () => console.log('board listening on ' + port));
"#;

/// Build the throwaway image from `node:24-alpine`, which is already on the box.
fn build_image(tag: &str) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    std::fs::write(dir.path().join("server.mjs"), SERVER_MJS).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.path().join("Containerfile"),
        "FROM docker.io/library/node:24-alpine\nCOPY server.mjs /app/server.mjs\n\
         ENTRYPOINT [\"node\", \"/app/server.mjs\"]\n",
    )
    .map_err(|e| e.to_string())?;
    let out = std::process::Command::new("podman")
        .args(["build", "--pull=never", "-t", tag, "."])
        .current_dir(dir.path())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(dir)
}

/// One masked client text frame out, one unmasked server text frame back.
async fn ws_text(base: &Gw, host: &str, path: &str, msg: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(base.addr()).await.unwrap();
    let req = upgrade_head(host, path);
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let head_end = loop {
        let mut b = [0u8; 1024];
        let n = sock.read(&mut b).await.unwrap();
        assert!(n > 0, "the connection closed before the 101");
        buf.extend_from_slice(&b[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    // RFC 6455's own example key, so the accept is a known constant: this is
    // the real handshake, computed by the container and carried back verbatim.
    assert!(
        head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "the container's Sec-WebSocket-Accept did not survive the proxy: {head}"
    );

    let payload = msg.as_bytes();
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x81u8, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    sock.write_all(&frame).await.unwrap();

    let mut back = buf[head_end..].to_vec();
    while back.len() < 2 || back.len() < 2 + back.get(1).copied().unwrap_or(0) as usize {
        let mut b = [0u8; 1024];
        let n = sock.read(&mut b).await.unwrap();
        if n == 0 {
            break;
        }
        back.extend_from_slice(&b[..n]);
    }
    assert_eq!(back[0], 0x81, "a text frame came back: {back:?}");
    let len = back[1] as usize;
    String::from_utf8_lossy(&back[2..2 + len]).to_string()
}

/// The whole of §11's WP4 "done when", against a real container: the page, a
/// live SSE stream, a WebSocket round trip, the idle stop and the restart, and
/// the container's own tool on `/mcp`.
#[tokio::test]
async fn the_real_thing_serves_a_page_an_sse_stream_a_websocket_and_its_tools() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_serves…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let image = format!("localhost/lmgw-wp4-{}:1", std::process::id());
    let _sweep = Sweep {
        prefix: prefix.clone(),
        image: image.clone(),
    };
    let _ctx = match build_image(&image) {
        Ok(d) => d,
        Err(e) => panic!("building the test image failed: {e}"),
    };
    state.set_agent_spawner_for_tests(Arc::new(lmgw_core::agents::container::TokioSpawner));
    // One second of idle and a 30 s start budget: the window has to be short
    // enough to wait out, the start long enough for a cold container.
    install(&base, &doc(&image, 1, 30, PROVIDES)).await;

    // 1. The page, through the proxy, started on demand.
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>board</h1>"), "{body}");
    // The container knows its public address, from the environment lmgw set
    // (origins §4.6) — the origin with no trailing slash.
    let port = base.base.rsplit(':').next().unwrap();
    assert!(
        body.contains(&format!("origin=http://board.localhost:{port}")),
        "{body}"
    );

    // 2. An SSE stream, live.
    let started = Instant::now();
    let resp = base
        .origin_client(&["board.localhost"])
        .get(format!("{}/sse", base.origin("board.localhost")))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let mut first = None;
    let mut all = String::new();
    while let Some(c) = stream.next().await {
        if first.is_none() {
            first = Some(started.elapsed());
        }
        all.push_str(&String::from_utf8_lossy(&c.unwrap()));
    }
    assert!(all.contains("tick 1") && all.contains("tick 3"), "{all}");
    assert!(
        first.unwrap() < started.elapsed() / 2,
        "the SSE body was buffered"
    );

    // The stream is finished, so its in-flight guard is gone: the idle window
    // starts from here rather than from when the request began.
    assert_eq!(
        detail(&base, "board").await["service"]["in_flight"],
        json!(0),
        "the finished SSE stream left its in-flight guard behind"
    );
    // 3. A real WebSocket round trip.
    assert_eq!(
        ws_text(
            &base,
            &format!("board.localhost:{}", base.addr().port()),
            "/ws",
            "hello"
        )
        .await,
        "echo:hello"
    );
    // §3.3: the tunnel is torn down when *either* end goes, so a closed
    // WebSocket releases its guard and the container can idle-stop. A proxy
    // that waited for both halves pinned it for good.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        detail(&base, "board").await["service"]["in_flight"],
        json!(0),
        "the closed WebSocket left its in-flight guard behind"
    );

    // 4. The container's tool, through `/mcp`, under the manifest's prefix.
    let specs = vec![lmgw_core::ingress::responses::McpToolSpec {
        server_label: "board".to_string(),
        allowed_tools: None,
        require_approval: Default::default(),
    }];
    let resolved =
        lmgw_core::mcp::exec::resolve(&state, &specs, &lmgw_core::mcp::scope::ToolScope::gateway())
            .await;
    assert!(
        resolved.failed.is_empty(),
        "the label did not resolve: {:?}",
        resolved.failed
    );
    let names: Vec<String> = resolved.tools.iter().map(|t| t.def.name.clone()).collect();
    assert!(names.iter().any(|n| n == "board__pin"), "{names:?}");

    // 5. Idle-stopped after its window, and started again by the next request.
    let before = detail(&base, "board").await;
    let container = before["service"]["container"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(service::sweep_idle(&state).await, vec![container.clone()]);
    let after = detail(&base, "board").await;
    assert_eq!(after["service"]["running"], json!(false), "{after}");
    let gone = std::process::Command::new("podman")
        .args(["ps", "-a", "--filter", &format!("name={container}"), "-q"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&gone.stdout).trim().is_empty(),
        "the stop ladder left {container} behind"
    );

    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>board</h1>"), "{body}");

    service::stop(&state, "board", "test over").await;
}

/// A start that never answers ends as a `503` naming the bound it waited under,
/// with the container's own log — and leaves nothing running.
#[tokio::test]
async fn the_real_thing_reports_a_container_that_never_answers() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_reports…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let image = format!("localhost/lmgw-wp4-deaf-{}:1", std::process::id());
    let _sweep = Sweep {
        prefix: prefix.clone(),
        image: image.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Containerfile"),
        "FROM docker.io/library/node:24-alpine\n\
         ENTRYPOINT [\"node\", \"-e\", \"console.log('up but listening at nothing'); \
         setInterval(() => {}, 1000)\"]\n",
    )
    .unwrap();
    let out = std::process::Command::new("podman")
        .args(["build", "--pull=never", "-t", &image, "."])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    state.set_agent_spawner_for_tests(Arc::new(lmgw_core::agents::container::TokioSpawner));
    install(&base, &doc(&image, 0, 2, "")).await;

    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 503, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["code"], json!("agent_service_starting"), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("could not be started"),
        "{v}"
    );
    assert!(
        v.get("log").is_none(),
        "a container's log tail is the owner's, not the origin's: {v}"
    );
    // The bound it waited under and the container's own log, on the Admin path
    // the App tab reads (§3.9).
    let (status, v) = op(&base, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 400, "{v}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("run.service.start_timeout_seconds (2s)"),
        "{v}"
    );
    assert!(
        message.contains("up but listening at nothing"),
        "the container's own log is quoted back: {v}"
    );
    // Nothing half-started is left behind.
    let d = detail(&base, "board").await;
    assert_eq!(d["service"]["running"], json!(false), "{d}");
    let left = std::process::Command::new("podman")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=lmgw.instance={prefix}"),
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&left.stdout).trim().is_empty(),
        "a failed start left a container behind"
    );
}

// ---------------------------------------------------------------------------
// The App tab's log block is the reader's, not a constant (§7, final review)
// ---------------------------------------------------------------------------

/// A detached service container writes no ledger, no job row and no `result`,
/// so `podman logs` is the whole account there is of it — which made the fixed
/// `STDERR_EXCERPT_LINES` excerpt a cap on the only diagnostic available.
#[tokio::test]
async fn the_app_log_tail_defaults_to_the_excerpt_and_the_reader_can_raise_it() {
    let (state, base, _calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, "")).await;
    // The first proxied request is the start.
    assert_eq!(app_get(&base, "board.localhost", "/").await.0, 200);

    // What the page draws itself with: the excerpt, with its size travelling
    // beside it so nobody has to guess whether it is all of it.
    let d = detail(&base, "board").await;
    let n = d["service"]["log_tail_lines"].as_u64().unwrap();
    assert_eq!(n, 12, "{d}");
    assert!(
        d["service"]["log_tail"]
            .as_str()
            .unwrap()
            .starts_with(&format!("tail={n}")),
        "{d}"
    );

    // And the dial behind it.
    let (status, res) = op(
        &base,
        "agent_service_log",
        json!({ "id": "board", "lines": 200 }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["lines"], json!(200), "{res}");
    assert_eq!(res["running"], json!(true), "{res}");
    assert!(
        res["log"].as_str().unwrap().starts_with("tail=200"),
        "{res}"
    );
    assert!(
        res["message"]
            .as_str()
            .unwrap()
            .contains("the last 200 lines"),
        "{res}"
    );

    // `0` is the whole log, the house reading — not podman's own `--tail 0`,
    // which prints nothing.
    let (status, res) = op(
        &base,
        "agent_service_log",
        json!({ "id": "board", "lines": 0 }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert!(res["log"].as_str().unwrap().starts_with("tail=-1"), "{res}");
    assert!(
        res["message"].as_str().unwrap().contains("the whole log"),
        "{res}"
    );

    // Omitted means the same excerpt the page starts from.
    let (_, res) = op(&base, "agent_service_log", json!({ "id": "board" })).await;
    assert_eq!(res["lines"], json!(12), "{res}");

    // Nothing running is an answer, not an error — the container may have
    // idle-stopped between the page load and the click.
    service::stop(&state, "board", "test").await;
    let (status, res) = op(&base, "agent_service_log", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["running"], json!(false), "{res}");
    assert_eq!(res["log"], json!(""), "{res}");

    // And an agent with no service block says so rather than answering blank.
    install(&base, &no_service_doc()).await;
    let (status, res) = op(&base, "agent_service_log", json!({ "id": "plain" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"].as_str().unwrap().contains("no run.service"),
        "{res}"
    );
}

/// A container agent with no `run.service` at all.
fn no_service_doc() -> String {
    r#"{
  "schema_version": 1,
  "id": "plain",
  "name": "Plain",
  "model": { "alias": "{{config.model}}" },
  "config": { "schema": { "type": "object", "properties": {
    "model": { "type": "string", "format": "model_alias" }
  } } },
  "run": { "kind": "container", "image": "localhost/plain:1", "columns": ["subject"] }
}"#
    .to_string()
}
