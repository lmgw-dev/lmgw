//! The package: install from an image, pull, re-import, provenance, the
//! portability line and the dev override (container-runtime design §3.4, WP5).
//!
//! Two halves, the split every WP before this one made:
//!
//! - **Against a fake podman** (the bulk, runs in CI): the ops run for real
//!   over the HTTP plane — `agent_install` reads a manifest out of a fake
//!   image through `create`/`cp`/`rm`, the row lands through the ordinary
//!   import path with its warnings, `agent_pull` compares the digest before and
//!   after and reads the new image's manifest when it moved, `agent_reimport`
//!   adopts it and keeps the config, the export names its caveats, and a
//!   `dev_url` puts a **real** host HTTP server behind the agent's own origin
//!   without starting anything.
//! - **Against real podman** (`the_real_thing_*`): an image built here over
//!   `registry.fedoraproject.org/fedora-minimal:44` that carries its manifest
//!   at `/lmgw/agent.json` and echoes JSONL when it runs. It installs with no
//!   manifest pasted anywhere and then runs end to end through the WP2 runner.
//!   Skips itself with a message when podman is absent, and removes its image
//!   and every container it makes, including on failure.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lmgw_core::agents::container::{self, Spawned, Spawner};
use lmgw_core::runtime::registry::CmdOutput;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common;
use common::{dashboard_key, serve_on, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

/// A gateway whose `bind_addr` setting really is the port it is listening on —
/// the same requirement `tests/it/agents_service.rs` documents, because a
/// `provides.mcp` row's URL is built from that setting.
async fn gateway() -> (SharedState, Gw, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let prefix = format!(
        "lmgwp{}-{}",
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

/// A `GET` on an agent origin (origins §4.1): lmgw's own socket, the agent's
/// host name, and no credential — the origin asks for none.
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

async fn detail(base: &Gw, id: &str) -> Value {
    let (status, body) = get(base, &format!("/api/agents/{id}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Is there a catalog row with this id? Asked by id rather than by counting the
/// catalog: the shipped built-ins are seeded by a background task, so "the
/// catalog is empty" is a race and "this agent was not written" is not.
async fn written(base: &Gw, id: &str) -> bool {
    get(base, &format!("/api/agents/{id}")).await.0 == 200
}

fn warning_codes(detail: &Value) -> Vec<String> {
    detail["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|w| w["code"].as_str().map(str::to_string))
        .collect()
}

/// The manifest a package carries. `version` is what a re-import has to be able
/// to change, and the config field is what it has to be able to keep.
fn doc(version: &str, image: &str, service: bool) -> String {
    let service = if service {
        r#", "service": { "port": 8080, "idle_seconds": 300, "start_timeout_seconds": 10 },
            "provides": { "mcp": "/mcp" }"#
    } else {
        ""
    };
    format!(
        r#"{{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "version": "{version}",
  "description": "an agent that ships as an image",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }},
    "greeting": {{ "type": "string", "default": "hello" }}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "columns": ["subject"],
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32, "stop_grace_seconds": 1 }}{service}
  }}
}}"#
    )
}

// ---------------------------------------------------------------------------
// The fake podman
// ---------------------------------------------------------------------------

/// One image as this box knows it: is it here, what digest does it have, and
/// what (if anything) is at `/lmgw/agent.json`.
#[derive(Clone, Default)]
struct Img {
    present: bool,
    digest: String,
    manifest: Option<String>,
}

/// A fake `podman` with an image table a `pull` can change — which is what
/// makes "the tag moved and the manifest moved with it" testable without a
/// registry. `run -d` binds the host port its argv publishes and serves a page,
/// so "clearing the dev_url goes back to the image" is a real HTTP round trip.
#[derive(Default)]
struct Fake {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    images: Arc<Mutex<HashMap<String, Img>>>,
    /// What `pull` makes of the image it is given, if anything.
    on_pull: Arc<Mutex<HashMap<String, Img>>>,
}

impl Fake {
    fn with(image: &str, digest: &str, manifest: Option<&str>) -> Self {
        let f = Self::default();
        f.images.lock().unwrap().insert(
            image.to_string(),
            Img {
                present: true,
                digest: digest.to_string(),
                manifest: manifest.map(str::to_string),
            },
        );
        f
    }
    /// The image this `pull` produces — a moved tag, or an arrival.
    fn pull_yields(self, image: &str, digest: &str, manifest: Option<&str>) -> Self {
        self.on_pull.lock().unwrap().insert(
            image.to_string(),
            Img {
                present: true,
                digest: digest.to_string(),
                manifest: manifest.map(str::to_string),
            },
        );
        self
    }
    fn calls(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        self.calls.clone()
    }
}

fn verbs(calls: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<String> {
    calls.lock().unwrap().iter().map(|a| a.join(" ")).collect()
}

fn count(calls: &Arc<Mutex<Vec<Vec<String>>>>, verb: &str) -> usize {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.first().map(String::as_str) == Some(verb))
        .count()
}

/// The page the fake container serves, so "served from the image" and "served
/// from the dev server" are told apart by their bodies.
fn fake_container_app() -> axum::Router {
    axum::Router::new().route(
        "/",
        axum::routing::get(|| async { "the container's own page" }),
    )
}

/// The dev server a `dev_url` points at: a real HTTP server on the host, which
/// is what `trunk serve` is in this test.
fn dev_app() -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/", get(|| async { "the dev server's page" }))
        .route(
            "/assets/app.js",
            get(|| async { "console.log('hot reload')" }),
        )
        .route(
            "/mcp",
            axum::routing::post(|| async { "{\"jsonrpc\":\"2.0\"}" }),
        )
        // An absolute `Location` naming the dev server's own origin — read off
        // the `Host` the proxy dialled it with, which is exactly that. The
        // browser is on the agent origin and cannot follow this one (§4.5).
        .route(
            "/go-self",
            get(|headers: axum::http::HeaderMap| async move {
                let host = headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                (
                    axum::http::StatusCode::FOUND,
                    [("location", format!("http://{host}/after"))],
                )
            }),
        )
}

/// A dev server whose `/mcp` — the path the manifest's `provides.mcp` names —
/// speaks **real** MCP, so lmgw's own aggregator can initialize against it.
/// Separate from [`dev_app`], whose `/mcp` stub is what the byte-for-byte proxy
/// test asserts on.
async fn dev_mcp_server() -> String {
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async { "the dev server's page" }),
        )
        .route("/mcp", axum::routing::post(dev_mcp));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// One JSON-RPC exchange from a dev server standing in for the container's own
/// MCP face — enough for lmgw's aggregator to initialize and list.
async fn dev_mcp(body: String) -> impl axum::response::IntoResponse {
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let Some(id) = req.get("id").cloned() else {
        return (
            axum::http::StatusCode::ACCEPTED,
            [
                ("content-type", "application/json"),
                ("mcp-session-id", "board-dev"),
            ],
            String::new(),
        );
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
        _ => json!({}),
    };
    (
        axum::http::StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("mcp-session-id", "board-dev"),
        ],
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
    )
}

#[async_trait]
impl Spawner for Fake {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        panic!("the package path never uses the streaming seam");
    }

    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(args.to_vec());
        let ok = |stdout: &str| {
            Ok(CmdOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        };
        let failed = |status: i32, stderr: &str| {
            Ok(CmdOutput {
                status,
                stdout: String::new(),
                stderr: stderr.to_string(),
            })
        };
        let img_of = |name: &str| self.images.lock().unwrap().get(name).cloned();
        match args.first().map(String::as_str) {
            // The reference is the **last** argument: every argv that takes
            // one passes `--` before it, so a ref that reads as a flag cannot
            // be mistaken for one.
            Some("image") if args.get(1).map(String::as_str) == Some("exists") => {
                match img_of(args.last().unwrap()) {
                    Some(i) if i.present => ok(""),
                    _ => failed(1, ""),
                }
            }
            // `image inspect --format {{.Digest}}`
            Some("image") => match img_of(args.last().unwrap()) {
                Some(i) if i.present => ok(&format!("{}\n", i.digest)),
                _ => failed(125, "Error: image not known"),
            },
            Some("pull") => {
                let name = args.last().unwrap();
                match self.on_pull.lock().unwrap().get(name).cloned() {
                    Some(next) => {
                        self.images.lock().unwrap().insert(name.to_string(), next);
                        ok("")
                    }
                    // Nothing new upstream: a pull of an image that is here is
                    // a no-op, and of one that is not is a registry error.
                    None => match img_of(name) {
                        Some(i) if i.present => ok(""),
                        _ => failed(125, &format!("Error: {name}: manifest unknown")),
                    },
                }
            }
            Some("create") => {
                let name = args.last().unwrap();
                match img_of(name) {
                    Some(i) if i.present => ok("6158a95fea92\n"),
                    _ => failed(125, "Error: image not known"),
                }
            }
            Some("cp") => {
                // `<container>:/lmgw/agent.json` — the container is the one
                // `create` just made, and this fake has exactly one image in
                // flight per call, so the manifest is looked up by whichever
                // image was created last.
                let image = self
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|a| a.first().map(String::as_str) == Some("create"))
                    .and_then(|a| a.last().cloned())
                    .unwrap_or_default();
                match img_of(&image).and_then(|i| i.manifest) {
                    Some(text) => {
                        std::fs::write(args.last().unwrap(), text)?;
                        ok("")
                    }
                    None => failed(
                        125,
                        "Error: \"/lmgw/agent.json\" could not be found on container: no such \
                         file or directory",
                    ),
                }
            }
            Some("run") => {
                let i = args.iter().position(|a| a == "-p").expect("published");
                let port: u16 = args[i + 1].split(':').nth(1).unwrap().parse().unwrap();
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
                tokio::spawn(async move {
                    let _ = axum::serve(listener, fake_container_app()).await;
                });
                ok("deadbeef\n")
            }
            _ => ok(""),
        }
    }
}

async fn with(fake: Fake) -> (SharedState, Gw, Arc<Mutex<Vec<Vec<String>>>>) {
    let (state, base, _prefix) = gateway().await;
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    (state, base, calls)
}

// ---------------------------------------------------------------------------
// Install (§3.4, §11 item 5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_image_installs_a_working_row_with_no_manifest_pasted_anywhere() {
    let image = "localhost/board:1";
    let (_state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["id"], "board", "{res}");
    assert_eq!(res["replaced"], false, "{res}");
    assert_eq!(res["digest"], "sha256:one", "{res}");
    // The pull policy defaults to `never` and the image was already here, so
    // nothing was downloaded — and the report says so rather than leaving it
    // to be inferred.
    assert_eq!(res["pulled"], false, "{res}");
    assert_eq!(count(&calls, "pull"), 0, "{:?}", verbs(&calls));

    let d = detail(&base, "board").await;
    assert_eq!(d["name"], "Board", "{d}");
    assert_eq!(d["version"], "1.0.0", "{d}");
    assert_eq!(d["kind"], "container", "{d}");
    assert_eq!(d["source"], "imported", "{d}");
    // The whole config form came out of the image.
    let names: Vec<String> = d["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["model", "greeting"], "{d}");
    // Provenance: what was installed, its digest, and where the manifest was.
    assert_eq!(d["provenance"]["image"], image, "{d}");
    assert_eq!(d["provenance"]["digest"], "sha256:one", "{d}");
    assert_eq!(d["provenance"]["manifest_path"], "/lmgw/agent.json", "{d}");
    assert!(
        d["provenance"]["installed_at"].as_str().unwrap().len() >= 20,
        "{d}"
    );
    // The import path's own warning for a `localhost/` image, unchanged: an
    // install is an import, so it earns exactly the same ones.
    assert!(
        warning_codes(&d).contains(&"local_image_on_import".to_string()),
        "{d}"
    );
    // Read without ever starting the image.
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));
    assert_eq!(count(&calls, "create"), 1, "{:?}", verbs(&calls));
    assert_eq!(count(&calls, "rm"), 1, "{:?}", verbs(&calls));
}

#[tokio::test]
async fn an_absent_image_under_pull_never_warns_instead_of_downloading() {
    let image = "docker.io/acme/board:1";
    // Not on the box, and a pull *would* work — which is the point: the policy
    // is what stops it, not the absence of a registry.
    let (_state, base, calls) =
        with(Fake::default().pull_yields(image, "sha256:new", Some(&doc("1.0.0", image, false))))
            .await;

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 400, "{res}");
    let msg = res["message"].as_str().unwrap_or_default();
    assert!(msg.contains("image_absent_pull_never"), "{msg}");
    assert!(msg.contains(image), "{msg}");
    assert!(msg.contains("pull 'missing'"), "{msg}");
    assert_eq!(count(&calls, "pull"), 0, "{:?}", verbs(&calls));
    assert_eq!(count(&calls, "create"), 0, "{:?}", verbs(&calls));

    // Asking for it explicitly is what downloads it.
    let (status, res) = op(
        &base,
        "agent_install",
        json!({ "image": image, "pull": "missing" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["pulled"], true, "{res}");
    assert_eq!(res["digest"], "sha256:new", "{res}");
    assert_eq!(count(&calls, "pull"), 1, "{:?}", verbs(&calls));
}

#[tokio::test]
async fn an_image_that_is_not_a_package_says_where_it_looked() {
    let image = "docker.io/library/alpine:3";
    let (_state, base, calls) = with(Fake::with(image, "sha256:alpine", None)).await;

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 400, "{res}");
    let msg = res["message"].as_str().unwrap_or_default();
    assert!(msg.contains("carries no /lmgw/agent.json"), "{msg}");
    assert!(msg.contains("package_no_manifest"), "{msg}");
    // The throwaway container is removed even though the copy failed.
    assert_eq!(count(&calls, "rm"), 1, "{:?}", verbs(&calls));
}

#[tokio::test]
async fn an_invalid_manifest_inside_an_image_fails_with_the_imports_own_error() {
    let image = "localhost/broken:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:broken",
        Some(r#"{ "schema_version": 1, "id": "Bad Id", "name": "x" }"#),
    ))
    .await;

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 400, "{res}");
    let msg = res["message"].as_str().unwrap_or_default();
    // The import path's own message, verbatim — not a package-layer paraphrase
    // of "the manifest is invalid".
    assert!(msg.contains("manifest: missing field `model`"), "{msg}");
    assert!(
        !written(&base, "board").await,
        "nothing must have been written"
    );
}

#[tokio::test]
async fn the_manifest_wins_when_it_names_a_different_image_and_both_are_said() {
    // The package is tagged `:1`; the manifest inside it names `:pinned`.
    let installed = "localhost/board:1";
    let declared = "localhost/board:pinned";
    let (_state, base, _calls) = with(Fake::with(
        installed,
        "sha256:one",
        Some(&doc("1.0.0", declared, false)),
    ))
    .await;

    let (status, res) = op(&base, "agent_install", json!({ "image": installed })).await;
    assert_eq!(status, 200, "{res}");
    let warnings = res["warnings"].to_string();
    assert!(
        warnings.contains("names run.image 'localhost/board:pinned'"),
        "{warnings}"
    );
    assert!(warnings.contains("The manifest wins"), "{warnings}");

    let d = detail(&base, "board").await;
    // What a run would start is the manifest's, and the row says where the
    // package came from beside it.
    assert_eq!(d["runtime"]["image"], declared, "{d}");
    assert_eq!(d["provenance"]["image"], installed, "{d}");
    assert!(
        warning_codes(&d).contains(&"install_image_mismatch".to_string()),
        "{d}"
    );
}

#[tokio::test]
async fn replacing_an_existing_agent_is_asked_for_and_keeps_its_config() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, false)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "greeting": "guten tag" } }),
    )
    .await;

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"),
        "{res}"
    );

    let (status, res) = op(
        &base,
        "agent_install",
        json!({ "image": image, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["replaced"], true, "{res}");
    let d = detail(&base, "board").await;
    assert_eq!(d["config"]["greeting"], "guten tag", "{d}");
}

#[tokio::test]
async fn validate_only_reads_the_image_and_writes_nothing() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, false)),
    ))
    .await;
    let (status, res) = op(
        &base,
        "agent_install",
        json!({ "image": image, "validate_only": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["validate_only"], true, "{res}");
    assert!(
        !written(&base, "board").await,
        "validate_only must write nothing"
    );
}

/// §11 names the tool, so the tool path gets its own pass: same op, same
/// report, reached the way a chat agent holding the self-admin token reaches
/// it.
#[tokio::test]
async fn the_self_admin_tool_installs_through_the_same_path() {
    let image = "localhost/board:1";
    let (state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, false)),
    ))
    .await;
    let mut settings = state.snapshot().settings.clone();
    settings.self_admin = lmgw_core::config::SelfAdmin::Full;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        "test-admin-token",
        true,
    )
    .await
    .unwrap();

    let client = base.client();
    let mcp = |body: Value, sid: Option<String>| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let mut req = client
                .post(format!("{base}/mcp/admin"))
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer test-admin-token")
                .json(&body);
            if let Some(sid) = sid {
                req = req.header("mcp-session-id", sid);
            }
            let resp = req.send().await.unwrap();
            let sid = resp
                .headers()
                .get("mcp-session-id")
                .map(|v| v.to_str().unwrap().to_string());
            let text = resp.text().await.unwrap();
            // The admin plane may answer as SSE; the JSON is the `data:` line.
            let json = text
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .unwrap_or(&text)
                .to_string();
            (sid, serde_json::from_str::<Value>(&json).unwrap())
        }
    };
    let (sid, _) = mcp(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "clientInfo": { "name": "t", "version": "0" } } }),
        None,
    )
    .await;
    let sid = sid.expect("a session id");

    let (_, listed) = mcp(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        Some(sid.clone()),
    )
    .await;
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "lmgw__agent_install"),
        "{listed}"
    );

    let (_, res) = mcp(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "lmgw__agent_install",
                            "arguments": { "image": image } } }),
        Some(sid),
    )
    .await;
    assert!(res["error"].is_null(), "{res}");
    assert_ne!(res["result"]["isError"], json!(true), "{res}");
    let payload: Value =
        serde_json::from_str(res["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["id"], "board", "{payload}");
    assert_eq!(payload["image"], image, "{payload}");
    assert!(
        payload["next_step"]
            .as_str()
            .unwrap_or_default()
            .contains("lmgw__agent_get"),
        "{payload}"
    );
    // The same three podman invocations the op makes, and nothing started.
    assert_eq!(count(&calls, "create"), 1, "{:?}", verbs(&calls));
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));
    assert_eq!(detail(&base, "board").await["name"], "Board");
}

// ---------------------------------------------------------------------------
// Pull, and §12's image-update minimum
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pull_that_moves_the_tag_reports_both_digests_and_the_manifest_that_moved_with_it() {
    let image = "localhost/board:1";
    let (_state, base, calls) = with(
        Fake::with(image, "sha256:one", Some(&doc("1.0.0", image, false))).pull_yields(
            image,
            "sha256:two",
            Some(&doc("2.0.0", image, false)),
        ),
    )
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "greeting": "guten tag" } }),
    )
    .await;
    let installed_at = detail(&base, "board").await["provenance"]["installed_at"].clone();

    let (status, res) = op(&base, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["old_digest"], "sha256:one", "{res}");
    assert_eq!(res["new_digest"], "sha256:two", "{res}");
    assert_eq!(res["changed"], true, "{res}");
    // §12's minimum: the new image's manifest is read and compared, and the
    // answer offers the re-import rather than doing it.
    assert_eq!(res["manifest_differs"], true, "{res}");
    assert!(
        res["note"]
            .as_str()
            .unwrap_or_default()
            .contains("Re-import from image"),
        "{res}"
    );
    // The row still runs the old manifest: a pull adopts nothing.
    let d = detail(&base, "board").await;
    assert_eq!(d["version"], "1.0.0", "{d}");
    assert_eq!(d["provenance"]["digest"], "sha256:two", "{d}");
    // A pull refreshes the digest and `pulled_at`; when this row was installed
    // is a different fact and is left alone.
    assert_eq!(d["provenance"]["installed_at"], installed_at, "{d}");

    // Re-import adopts it and keeps the config, the way §5.1's built-in
    // upgrade does.
    let (status, res) = op(&base, "agent_reimport", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["changed"], true, "{res}");
    let d = detail(&base, "board").await;
    assert_eq!(d["version"], "2.0.0", "{d}");
    assert_eq!(d["config"]["greeting"], "guten tag", "{d}");

    // A second pull with nothing new upstream says so, and does not re-read
    // the manifest.
    let before = count(&calls, "create");
    let (status, res) = op(&base, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["changed"], false, "{res}");
    assert_eq!(res["manifest_differs"], Value::Null, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is unchanged"),
        "{res}"
    );
    assert_eq!(count(&calls, "create"), before, "{:?}", verbs(&calls));
}

#[tokio::test]
async fn a_moved_image_that_is_no_longer_a_package_says_so_instead_of_offering_a_re_import() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(
        Fake::with(image, "sha256:one", Some(&doc("1.0.0", image, false))).pull_yields(
            image,
            "sha256:two",
            None,
        ),
    )
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;

    let (status, res) = op(&base, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["changed"], true, "{res}");
    assert_eq!(res["manifest_differs"], Value::Null, "{res}");
    assert!(
        res["note"]
            .as_str()
            .unwrap_or_default()
            .contains("carries no /lmgw/agent.json"),
        "{res}"
    );
}

#[tokio::test]
async fn a_re_import_of_a_package_carrying_another_id_is_refused_naming_both() {
    let image = "localhost/board:1";
    let other = doc("2.0.0", image, false).replace("\"id\": \"board\"", "\"id\": \"other\"");
    let (_state, base, _calls) = with(
        Fake::with(image, "sha256:one", Some(&doc("1.0.0", image, false))).pull_yields(
            image,
            "sha256:two",
            Some(&other),
        ),
    )
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    op(&base, "agent_pull", json!({ "id": "board" })).await;

    let (status, res) = op(&base, "agent_reimport", json!({ "id": "board" })).await;
    assert_eq!(status, 400, "{res}");
    let msg = res["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("carries the agent 'other', not 'board'"),
        "{msg}"
    );
    // And the row is untouched.
    assert_eq!(detail(&base, "board").await["version"], "1.0.0");
}

#[tokio::test]
async fn pulling_an_agent_with_no_image_says_there_is_nothing_to_pull() {
    let (_state, base, _calls) = with(Fake::default()).await;
    let chat = r#"{ "schema_version": 1, "id": "talker", "name": "T",
                    "model": { "alias": "m1" }, "run": { "kind": "chat", "system": "hi" } }"#;
    op(&base, "agent_set", json!({ "manifest": chat })).await;
    let (status, res) = op(&base, "agent_pull", json!({ "id": "talker" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("names no image"),
        "{res}"
    );
}

/// A package whose new manifest **drops a config field** must not leave the
/// stored value behind: `validate_values` refuses an undeclared key, so the row
/// would fail on every run and every config save — unusable and unfixable from
/// the form that is supposed to fix it (WP5 review).
#[tokio::test]
async fn a_re_import_that_drops_a_config_field_drops_its_stored_value_too() {
    let image = "localhost/board:1";
    // 2.0.0 of this package keeps `model` and loses `greeting`.
    let slimmer = doc("2.0.0", image, false).replace(
        r#"    "greeting": { "type": "string", "default": "hello" }"#,
        r#"    "tone": { "type": "string", "default": "dry" }"#,
    );
    let (_state, base, _calls) = with(
        Fake::with(image, "sha256:one", Some(&doc("1.0.0", image, false))).pull_yields(
            image,
            "sha256:two",
            Some(&slimmer),
        ),
    )
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "greeting": "guten tag" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    op(&base, "agent_pull", json!({ "id": "board" })).await;

    let (status, res) = op(&base, "agent_reimport", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["dropped_config"], json!(["greeting"]), "{res}");
    assert!(
        res["warnings"]
            .to_string()
            .contains("were removed with the old one"),
        "{res}"
    );
    let d = detail(&base, "board").await;
    assert_eq!(d["version"], "2.0.0", "{d}");
    assert!(d["config"].get("greeting").is_none(), "{d}");

    // And the row is usable: a config save goes through instead of failing on
    // a key the schema no longer declares.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "tone": "warm" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(detail(&base, "board").await["config"]["tone"], "warm");
}

/// The same pruning on the ordinary `agent_set` replace — one rule, every
/// manifest-replace path.
#[tokio::test]
async fn replacing_a_manifest_prunes_the_config_keys_it_no_longer_declares() {
    let (_state, base, _calls) = with(Fake::default()).await;
    let image = "localhost/board:1";
    op(
        &base,
        "agent_set",
        json!({ "manifest": doc("1.0.0", image, false) }),
    )
    .await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": { "greeting": "guten tag" } }),
    )
    .await;
    let slimmer = doc("2.0.0", image, false).replace(
        r#",
    "greeting": { "type": "string", "default": "hello" }"#,
        "",
    );
    let (status, res) = op(
        &base,
        "agent_set",
        json!({ "manifest": slimmer, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["dropped_config"], json!(["greeting"]), "{res}");
    assert!(detail(&base, "board").await["config"]
        .get("greeting")
        .is_none());
}

/// The safety valve for a row that is already in that state — a value orphaned
/// by an older build's replace. `clear` may name a field the schema does not
/// declare, because that is the only gesture that can remove it.
#[tokio::test]
async fn clear_may_name_a_field_the_manifest_does_not_declare() {
    let (state, base, _calls) = with(Fake::default()).await;
    let image = "localhost/board:1";
    op(
        &base,
        "agent_set",
        json!({ "manifest": doc("1.0.0", image, false) }),
    )
    .await;
    // Straight into the column, the way an older build would have left it.
    store::set_agent_config(
        &state.db,
        "board",
        &json!({ "greeting": "hi", "orphan": "left over" }).to_string(),
    )
    .await
    .unwrap();

    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "board", "values": {}, "clear": ["orphan"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["cleared_undeclared"], json!(["orphan"]), "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not declared"),
        "{res}"
    );
    let d = detail(&base, "board").await;
    assert!(d["config"].get("orphan").is_none(), "{d}");
    assert_eq!(d["config"]["greeting"], "hi", "{d}");
}

// ---------------------------------------------------------------------------
// Export: the portability line (§3.4, §8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_export_names_the_localhost_caveat_and_carries_no_token_provenance_or_dev_url() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    // Mint the token, so "the export never carries it" is a real assertion
    // rather than a statement about a column that happens to be empty.
    let (status, tok) = op(&base, "agent_token_get", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{tok}");
    let plaintext = tok["token"].as_str().unwrap().to_string();
    assert!(!plaintext.is_empty());
    let dev = "http://127.0.0.1:5173".to_string();
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let (status, body) = get(&base, "/api/agents/board/export").await;
    assert_eq!(status, 200, "{body}");
    let doc: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["portability"]["portable"], false, "{body}");
    let notes = doc["portability"]["notes"].to_string();
    assert!(
        notes.contains("'localhost/board:1' is local to the machine that built it"),
        "{notes}"
    );
    // The *fact* of a dev server is in the file; its **address** is not. An
    // export gets mailed around, and `dev_url` is on the never-exported list
    // beside the token and the provenance.
    assert!(
        notes.contains("a dev server on the exporting machine"),
        "{notes}"
    );
    assert!(
        !body.contains(&dev),
        "the export must not carry the dev server's address: {body}"
    );
    assert!(!body.contains("5173"), "{body}");
    assert!(
        !body.contains(&plaintext),
        "the token must never be exported"
    );
    assert!(doc.get("provenance").is_none(), "{body}");
    assert!(doc.get("dev_url").is_none(), "{body}");
    assert!(!body.contains("sha256:one"), "{body}");

    // The same verdict the dialog shows before the download — and the
    // dashboard, showing the owner their own row, may name the URL the file
    // may not.
    let d = detail(&base, "board").await;
    assert_eq!(d["portability"]["portable"], doc["portability"]["portable"]);
    assert_eq!(
        d["portability"]["notes"].as_array().unwrap().len(),
        doc["portability"]["notes"].as_array().unwrap().len()
    );
    assert!(d["portability"]["notes"].to_string().contains(&dev), "{d}");

    // And the file re-imports unchanged: `portability` is an envelope key, so
    // the manifest's `deny_unknown_fields` never sees it.
    let (status, res) = op(
        &base,
        "agent_set",
        json!({ "manifest": body, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
}

#[tokio::test]
async fn an_export_with_nothing_local_about_it_says_it_is_portable() {
    let image = "registry.example.com/acme/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, false)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let (_, body) = get(&base, "/api/agents/board/export").await;
    let doc: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["portability"]["portable"], true, "{body}");
    assert_eq!(doc["portability"]["notes"], json!([]), "{body}");
}

// ---------------------------------------------------------------------------
// The dev override (§3.4)
// ---------------------------------------------------------------------------

/// A real HTTP server on the host, which is what `trunk serve` is here.
async fn dev_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, dev_app()).await.unwrap() });
    format!("http://{addr}")
}

/// A `dev_url` with a path prefix — what vite and Tauri used to be pointed at
/// here — is refused now: a dev server is an **origin** (origins §4.7), the
/// app is served at the root of the agent's own origin, and there is no mount
/// prefix left for the proxy to strip.
#[tokio::test]
async fn a_dev_url_with_a_path_prefix_is_refused_as_an_origin_rule() {
    let image = "localhost/board:1";
    let (state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let dev = dev_server().await;

    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": format!("{dev}/base") }),
    )
    .await;
    assert_eq!(status, 400, "{res}");
    let message = res["message"].as_str().unwrap_or_default();
    assert!(message.contains("a dev_url is an origin"), "{res}");
    assert!(message.contains("--public-url"), "{res}");
    // Nothing was stored, and nothing was started for it.
    assert_eq!(detail(&base, "board").await["dev_url"], json!(""), "{res}");
    assert!(store::agent_dev_urls(&state.db).await.unwrap().is_empty());
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));

    // The same server without the prefix is the ordinary case.
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
}

/// lmgw's own address is refused: the app mount would proxy into the router
/// that serves it.
#[tokio::test]
async fn a_dev_url_pointing_at_the_gateway_itself_is_refused() {
    let image = "localhost/board:1";
    let (state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let own = state.snapshot().settings.bind_addr.clone();
    let port = own.rsplit(':').next().unwrap().to_string();

    for url in [
        format!("http://{own}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
    ] {
        let (status, res) = op(
            &base,
            "agent_dev_url_set",
            json!({ "id": "board", "url": url }),
        )
        .await;
        assert_eq!(status, 400, "{url}: {res}");
        assert!(
            res["message"]
                .as_str()
                .unwrap_or_default()
                .contains("must not be lmgw itself"),
            "{url}: {res}"
        );
    }
    assert_eq!(detail(&base, "board").await["dev_url"], "");
}

#[tokio::test]
async fn a_dev_url_serves_the_app_from_the_host_and_clearing_it_returns_to_the_image() {
    let image = "localhost/board:1";
    let (_state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let dev = dev_server().await;

    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": format!("{dev}/") }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        res["dev_url"], dev,
        "the trailing slash is normalised: {res}"
    );

    let d = detail(&base, "board").await;
    assert_eq!(d["dev_url"], dev, "{d}");
    assert!(
        warning_codes(&d).contains(&"dev_url_active".to_string()),
        "{d}"
    );
    // The `provides.mcp` row still points at lmgw's own stable proxy URL — the
    // override changes where the proxy goes, not where the row points.
    assert_eq!(d["service"]["provides_mcp"], "/mcp", "{d}");

    // The app is the dev server's, and nothing was started for it.
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "the dev server's page");
    let (status, body) = app_get(&base, "board.localhost", "/assets/app.js").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "console.log('hot reload')");
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));

    // The MCP mount goes to the same dev server, at the declared path.
    let resp = base
        .client()
        .post(format!("{base}/agents/board/mcp"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "{\"jsonrpc\":\"2.0\"}");
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));

    // Start says why rather than starting a container nothing routes to.
    let (status, res) = op(&base, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("has a dev_url set"),
        "{res}"
    );

    // Clearing it goes back to the image, and the next request starts it.
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": Value::Null }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "the container's own page");
    assert_eq!(count(&calls, "run"), 1, "{:?}", verbs(&calls));
    op(&base, "agent_service_stop", json!({ "id": "board" })).await;
}

/// The other half of the one surviving `Location` rewrite (§4.5): a dev server
/// that answers with its own origin names a port the browser is not talking to,
/// so it is pointed at the agent origin like the container's own would be.
#[tokio::test]
async fn a_dev_servers_own_origin_in_a_location_is_rewritten_to_the_agent_origin() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let dev = dev_server().await;
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let origin = base.origin("board.localhost");
    let resp = base
        .origin_client_no_redirect(&["board.localhost"])
        .get(format!("{origin}/go-self"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        &format!("{origin}/after")
    );
}

#[tokio::test]
async fn setting_a_dev_url_stops_the_container_that_was_serving_the_app() {
    let image = "localhost/board:1";
    let (_state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    // Bring the container up the ordinary way first.
    let (status, res) = op(&base, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{res}");
    let dev = dev_server().await;

    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert!(res["stopped"].is_string(), "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("was stopped"),
        "{res}"
    );
    let seen = verbs(&calls);
    assert!(
        seen.iter()
            .any(|c| c.starts_with("stop -t 1 ") && c.ends_with("-agentsvc-board")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|c| c.starts_with("rm -f ") && c.ends_with("-agentsvc-board")),
        "{seen:?}"
    );
    assert_eq!(detail(&base, "board").await["service"]["running"], false);
}

#[tokio::test]
async fn a_dev_url_that_is_not_local_or_not_a_base_url_is_refused_naming_why() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;

    for (url, why) in [
        ("https://example.com", "is not loopback"),
        // A LAN address is refused too: the app proxy is on the
        // unauthenticated, CORS-permissive dashboard plane.
        ("http://192.168.1.14:3000", "is not loopback"),
        ("http://user:pass@127.0.0.1:5173", "must not carry userinfo"),
        ("http://127.0.0.1:5173/?x=1", "no query and no fragment"),
        ("not a url", "is not a URL"),
    ] {
        let (status, res) = op(
            &base,
            "agent_dev_url_set",
            json!({ "id": "board", "url": url }),
        )
        .await;
        assert_eq!(status, 400, "{url}: {res}");
        assert!(
            res["message"].as_str().unwrap_or_default().contains(why),
            "{url}: {res}"
        );
    }
    assert_eq!(detail(&base, "board").await["dev_url"], "");
}

#[tokio::test]
async fn a_dev_url_needs_an_agent_that_declares_a_service() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, false)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": "http://127.0.0.1:5173" }),
    )
    .await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("declares no run.service"),
        "{res}"
    );
}

#[tokio::test]
async fn a_dev_server_that_is_not_running_is_a_502_that_says_what_did_not_answer() {
    let image = "localhost/board:1";
    let (_state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    // A port nothing is listening on: bind it, read the number, drop it.
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": format!("http://127.0.0.1:{dead}") }),
    )
    .await;

    let (status, body) = app_get(&base, "board.localhost", "/").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("agent_dev_url_unreachable"), "{body}");
    assert!(body.contains("no container was started"), "{body}");
    // Nothing was started to paper over it.
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));
}

// ---------------------------------------------------------------------------
// The real thing (needs podman)
// ---------------------------------------------------------------------------

const BASE_IMAGE: &str = "registry.fedoraproject.org/fedora-minimal:44";

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn image_present(image: &str) -> bool {
    std::process::Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// An image carrying its manifest at `/lmgw/agent.json` — an agent package —
/// removed whatever happens, including on a panic.
struct BuiltImage(String);

impl BuiltImage {
    fn new(tag: &str, manifest: &str, script: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lmgw-agent-package-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.json"), manifest).unwrap();
        std::fs::write(dir.join("run.sh"), script).unwrap();
        std::fs::write(
            dir.join("Containerfile"),
            format!(
                "FROM {BASE_IMAGE}\nCOPY agent.json /lmgw/agent.json\nCOPY run.sh /lmgw-run.sh\n\
                 ENTRYPOINT [\"/bin/sh\", \"/lmgw-run.sh\"]\n"
            ),
        )
        .unwrap();
        let image = format!("localhost/lmgw-agent-package:{tag}");
        let out = std::process::Command::new("podman")
            .args(["build", "-q", "-t", &image, "-f", "Containerfile", "."])
            .current_dir(&dir)
            .output()
            .expect("podman build runs");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "podman build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self(image)
    }
    fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for BuiltImage {
    fn drop(&mut self) {
        let _ = std::process::Command::new("podman")
            .args(["rmi", "-f", &self.0])
            .output();
    }
}

/// Every container **this test's instance** may have left, removed whatever
/// happened.
///
/// Scoped to `lmgw.instance=<this test's container_prefix>` and nothing else —
/// WP5 review. A sweep of every `lmgw-pkg-*` on the box would reach into a
/// production lmgw that happened to be reading a package at that moment, which
/// is precisely the collision `container_prefix` exists to prevent. The
/// package container carries the instance label for this reason.
struct ContainerSweep(String);

impl Drop for ContainerSweep {
    fn drop(&mut self) {
        if let Ok(out) = std::process::Command::new("podman")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=lmgw.instance={}", self.0),
            ])
            .output()
        {
            for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let _ = std::process::Command::new("podman")
                    .args(["rm", "-f", id])
                    .output();
            }
        }
    }
}

const REAL_SCRIPT: &str = r#"#!/bin/sh
echo "{\"type\":\"log\",\"message\":\"phase $LMGW_PHASE\"}"
echo "{\"type\":\"row\",\"id\":\"m1\",\"columns\":{\"subject\":\"Invoice\"},\"output\":{\"category\":\"Work\"}}"
echo "{\"type\":\"output\",\"output\":{\"seen\":1}}"
"#;

fn real_manifest(image: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "packaged",
  "name": "Packaged",
  "version": "1.0.0",
  "description": "shipped as an image, manifest and all",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }},
    "greeting": {{ "type": "string", "default": "hello" }}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "columns": ["subject"],
    "phases": ["run"],
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32,
                "deadline_seconds": 120, "stop_grace_seconds": 1 }},
    "output": {{ "run": {{ "type": "object",
                        "properties": {{ "seen": {{ "type": "integer" }} }},
                        "required": ["seen"] }} }}
  }}
}}"#
    )
}

/// §11 item 5's done-criterion, whole: a locally built image installs into a
/// working catalog row with no manifest pasted anywhere, that row runs through
/// the WP2 runner, and its export names the `localhost/` caveat.
#[tokio::test]
async fn the_real_thing_installs_from_an_image_and_the_row_runs() {
    if !podman_available() || !image_present(BASE_IMAGE) {
        eprintln!("SKIP the_real_thing_installs…: podman or the base image is not available");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let tag = format!("install{}", std::process::id());
    // The manifest names the image it is baked into, which is what a real
    // package does — built in two steps because the tag has to be known first.
    let image = format!("localhost/lmgw-agent-package:{tag}");
    let built = BuiltImage::new(&tag, &real_manifest(&image), REAL_SCRIPT);
    assert_eq!(built.name(), image);
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));

    // --- install: an image reference and nothing else ---
    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["id"], "packaged", "{res}");
    assert_eq!(res["pulled"], false, "{res}");
    let digest = res["digest"].as_str().unwrap_or_default().to_string();
    assert!(digest.starts_with("sha256:"), "{res}");

    let d = detail(&base, "packaged").await;
    assert_eq!(d["name"], "Packaged", "{d}");
    assert_eq!(d["provenance"]["image"], image, "{d}");
    assert_eq!(d["provenance"]["digest"], digest, "{d}");
    assert!(
        warning_codes(&d).contains(&"local_image_on_import".to_string()),
        "{d}"
    );
    assert!(
        !warning_codes(&d).contains(&"image_absent_pull_never".to_string()),
        "{d}"
    );

    // --- the export names the localhost caveat ---
    let (_, body) = get(&base, "/api/agents/packaged/export").await;
    let exported: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(exported["portability"]["portable"], false, "{body}");
    assert!(
        exported["portability"]["notes"]
            .to_string()
            .contains("local to the machine that built it"),
        "{body}"
    );

    // --- and the row really runs, through the WP2 runner ---
    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "packaged", "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let job = res["job_id"].as_i64().unwrap();
    let mut done = Value::Null;
    for _ in 0..600 {
        let (_, body) = get(&base, &format!("/api/agents/runs/{job}")).await;
        let v: Value = serde_json::from_str(&body).unwrap();
        let st = v["job"]["status"].as_str().unwrap_or_default().to_string();
        if st != "queued" && st != "running" {
            done = v;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(done["job"]["status"], "done", "{done}");
    assert_eq!(done["result"]["output"], json!({ "seen": 1 }), "{done}");
    let rows = done["rows"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 1, "{done}");
    assert_eq!(rows[0]["columns"]["subject"], "Invoice", "{done}");

    // --- a pull of a purely local image fails, and says so without lying
    //     about the digest it already has ---
    let (status, res) = op(&base, "agent_pull", json!({ "id": "packaged" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("podman pull"),
        "{res}"
    );
    assert_eq!(
        detail(&base, "packaged").await["provenance"]["digest"],
        digest
    );

    // --- a re-import of the same image changes nothing and says so ---
    let (status, res) = op(&base, "agent_reimport", json!({ "id": "packaged" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["changed"], false, "{res}");

    // Nothing left behind: the throwaway package containers and the run
    // directories are all gone.
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let left: Vec<String> = std::fs::read_dir(&root)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
    // This instance's containers, not the box's: another lmgw on this machine
    // is entitled to have one mid-install.
    let ps = std::process::Command::new("podman")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=lmgw.instance={prefix}"),
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "a container of this instance was left behind"
    );
}

/// The other half of the done-criterion: an absent image under `pull: never` is
/// reported rather than downloaded — against the real podman, so "nothing was
/// downloaded" is a fact about the box and not about a fake.
#[tokio::test]
async fn the_real_thing_refuses_an_absent_image_under_pull_never() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_refuses…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix);
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    // A reference that is certainly not on this box and would need a network
    // round trip to fetch. `pull: never` must not make one.
    let image = format!("localhost/lmgw-absent-{}:1", std::process::id());
    assert!(!image_present(&image));

    let (status, res) = op(&base, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 400, "{res}");
    let msg = res["message"].as_str().unwrap_or_default();
    assert!(msg.contains("image_absent_pull_never"), "{msg}");
    assert!(msg.contains(&image), "{msg}");
    // No wall-clock bound: "the image is still not on this box" is the fact
    // that matters, and a timing assertion would only be a flaky proxy for it
    // on a loaded machine.
    assert!(!image_present(&image), "nothing must have been downloaded");
    assert!(!written(&base, "packaged").await);
}

// ---------------------------------------------------------------------------
// A dev row is served, not sleeping (container-runtime §3.3/§3.4, final review)
// ---------------------------------------------------------------------------

/// The aggregate `tools/list` refuses to connect an `agent:<id>` row because
/// connecting means `podman run`. A `dev_url` row has **nothing to start**, so
/// that reason does not apply to it — and while it did, a dev agent's tools
/// were permanently unlistable: a chat thread attaching its label learned
/// nothing, and the App tab's "attaching the label starts it" was false for the
/// one row the owner is actively working on.
#[tokio::test]
async fn a_dev_url_agents_provided_tools_are_listed_on_the_aggregate_plane() {
    let image = "localhost/board:1";
    let (state, base, calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;

    // Before the override: the container rule applies, and the row is sleeping.
    let agg = state.mcp.list_tools(&state.snapshot()).await;
    assert!(
        !agg.tools.iter().any(|t| t.name == "board__pin"),
        "a sleeping agent's tools are not in the aggregate"
    );
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));

    let dev = dev_mcp_server().await;
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    // Now the same call lists them — through lmgw's own `/agents/board/mcp`
    // proxy, which resolves to the dev server rather than to a container.
    let agg = state.mcp.list_tools(&state.snapshot()).await;
    let names: Vec<String> = agg.tools.iter().map(|t| t.name.to_string()).collect();
    assert!(
        names.iter().any(|n| n == "board__pin"),
        "the dev server's tool is missing: {names:?}"
    );
    // And still nothing was started: that is the whole point of the exception.
    assert_eq!(count(&calls, "run"), 0, "{:?}", verbs(&calls));

    // The MCP page says which of the two it is, rather than "sleeping".
    let servers: Value = serde_json::from_str(&get(&base, "/api/mcp-servers").await.1).unwrap();
    let row = servers["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("agent:board"))
        .unwrap()
        .clone();
    let detail = row["status_detail"].as_str().unwrap_or_default();
    assert!(detail.starts_with("served from a dev server"), "{row}");
    assert!(detail.contains(&dev), "{row}");
}

// ---------------------------------------------------------------------------
// A bind address that moves onto a stored dev port (§3.4, final review)
// ---------------------------------------------------------------------------

/// `validate_dev_url` refuses lmgw's own port because the app mount would proxy
/// the router that serves it. Nothing re-asked that question when the *bind
/// address* moved, so a settings save could arm the loop from the other side.
#[tokio::test]
async fn moving_the_bind_address_onto_a_stored_dev_port_clears_that_dev_url() {
    let image = "localhost/board:1";
    let (state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let dev = dev_server().await;
    let dev_port: u16 = dev.rsplit(':').next().unwrap().parse().unwrap();

    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev.clone() }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(detail(&base, "board").await["dev_url"], dev);

    // The owner moves the gateway onto that very port.
    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "bind_addr": format!("127.0.0.1:{dev_port}") }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let message = res["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("dev server override was cleared") && message.contains("board"),
        "the save has to say what it took away: {res}"
    );
    assert!(message.contains("must not be lmgw itself"), "{res}");

    // The column is empty, and the row says why — the bind address only takes
    // effect on the next start, so the reason has to outlive this process.
    let d = detail(&base, "board").await;
    assert_eq!(d["dev_url"], "", "{d}");
    let codes = warning_codes(&d);
    assert!(codes.contains(&"dev_url_cleared".to_string()), "{d}");
    assert!(!codes.contains(&"dev_url_active".to_string()), "{d}");
    assert!(
        d["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["message"].as_str().unwrap_or_default().contains(&dev)),
        "the warning names the url that was dropped: {d}"
    );

    // Setting one again is the owner answering it.
    let other = dev_server().await;
    let (status, res) = op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": other }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert!(
        !warning_codes(&detail(&base, "board").await).contains(&"dev_url_cleared".to_string()),
        "the note is cleared once it has been acted on"
    );
    let _ = state;
}

/// A bind address that moves somewhere else leaves a legal dev_url alone.
#[tokio::test]
async fn moving_the_bind_address_elsewhere_keeps_a_still_legal_dev_url() {
    let image = "localhost/board:1";
    let (_state, base, _calls) = with(Fake::with(
        image,
        "sha256:one",
        Some(&doc("1.0.0", image, true)),
    ))
    .await;
    op(&base, "agent_install", json!({ "image": image })).await;
    let dev = dev_server().await;
    op(
        &base,
        "agent_dev_url_set",
        json!({ "id": "board", "url": dev.clone() }),
    )
    .await;

    // A free port that is neither the gateway's nor the dev server's.
    let spare = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let spare_port = spare.local_addr().unwrap().port();
    drop(spare);
    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "bind_addr": format!("127.0.0.1:{spare_port}") }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert!(
        !res["message"]
            .as_str()
            .unwrap_or_default()
            .contains("dev server override was cleared"),
        "{res}"
    );
    assert_eq!(detail(&base, "board").await["dev_url"], dev);
}
