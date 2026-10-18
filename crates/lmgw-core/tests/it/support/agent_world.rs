//! The agent catalog's test world, for the suites that read the agent ops and
//! tools into their types (`agents_wire`, `agents_ops_wire`,
//! `self_admin_agents`): a gateway over a fake `podman` that knows a few
//! images, the manifest an image carries, and can start a service container
//! (a loopback server on the port the argv publishes); and an MCP source whose
//! first call waits until the test lets it go, so a run stays in flight exactly
//! as long as a test needs.
//!
//! Nothing here reaches past loopback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use lmgw_core::agents::container::{Spawned, Spawner};
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::CmdOutput;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::json;
use tokio::sync::watch;

use super::mcp_stub::{answer, answering, register_timing_out_after, McpStub};
use crate::common::{dashboard_key, patience, serve_on, Gw};

/// One image as the fake box knows it.
#[derive(Clone)]
struct Img {
    digest: Option<String>,
    manifest: Option<String>,
}

/// A fake `podman`: images that are here, images a `pull` brings, and the
/// manifest each carries at `/lmgw/agent.json`. An image with no digest is one
/// podman cannot report a digest for.
#[derive(Default)]
pub struct Podman {
    here: Mutex<HashMap<String, Img>>,
    on_pull: Mutex<HashMap<String, Img>>,
    calls: Mutex<Vec<Vec<String>>>,
    /// `run -d` starts a container: a server on the published port.
    serve: bool,
    /// `run -d` never answers, until the start is cancelled.
    hold_start: bool,
}

impl Podman {
    /// `image` is on the box.
    pub fn with(self, image: &str, digest: Option<&str>, manifest: Option<&str>) -> Self {
        self.here
            .lock()
            .unwrap()
            .insert(image.to_string(), img(digest, manifest));
        self
    }

    /// `image` is not on the box, and a `pull` brings it.
    pub fn pulls(self, image: &str, digest: Option<&str>, manifest: Option<&str>) -> Self {
        self.on_pull
            .lock()
            .unwrap()
            .insert(image.to_string(), img(digest, manifest));
        self
    }

    /// `podman run -d` starts a container: a server on loopback, on the host
    /// port the argv publishes, that answers every request `200`. The health
    /// probe finds it, and `podman logs --tail n` answers `tail=<n>` and `n`
    /// lines.
    pub fn serving(mut self) -> Self {
        self.serve = true;
        self
    }

    /// `podman run -d` does not answer until the start is cancelled: a cold
    /// pull that outlasts the test.
    pub fn holding_start(mut self) -> Self {
        self.hold_start = true;
        self
    }
}

fn img(digest: Option<&str>, manifest: Option<&str>) -> Img {
    Img {
        digest: digest.map(str::to_string),
        manifest: manifest.map(str::to_string),
    }
}

impl Spawner for Podman {
    fn spawn<'a>(
        &'a self,
        _program: &'a str,
        _args: &'a [String],
    ) -> Erased<'a, std::io::Result<Spawned>> {
        Box::pin(async move {
            panic!("the install path never uses the streaming seam");
        })
    }

    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(args.to_vec());
            let out = |status: i32, stdout: &str, stderr: &str| {
                Ok(CmdOutput {
                    status,
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                })
            };
            let known = |name: &str| self.here.lock().unwrap().get(name).cloned();
            let last = args.last().cloned().unwrap_or_default();
            match args.first().map(String::as_str) {
                Some("image") if args.get(1).map(String::as_str) == Some("exists") => {
                    out(i32::from(known(&last).is_none()), "", "")
                }
                Some("image") => match known(&last) {
                    Some(Img {
                        digest: Some(d), ..
                    }) => out(0, &format!("{d}\n"), ""),
                    Some(_) => out(125, "", "Error: no digest"),
                    None => out(125, "", "Error: image not known"),
                },
                Some("pull") => {
                    let next = self.on_pull.lock().unwrap().get(&last).cloned();
                    match (next, known(&last)) {
                        (Some(next), _) => {
                            self.here.lock().unwrap().insert(last, next);
                            out(0, "", "")
                        }
                        (None, Some(_)) => out(0, "", ""),
                        (None, None) => out(125, "", &format!("Error: {last}: manifest unknown")),
                    }
                }
                Some("run") if self.hold_start => {
                    // Until the start is cancelled, which drops this future.
                    std::future::pending::<()>().await;
                    out(0, "", "")
                }
                Some("run") if self.serve => {
                    let i = args
                        .iter()
                        .position(|a| a == "-p")
                        .expect("a published port");
                    let port: u16 = args[i + 1].split(':').nth(1).unwrap().parse().unwrap();
                    // lmgw probed this port free and released it before publishing
                    // it, so another test's ephemeral socket can hold it for a
                    // moment: answered as podman answers it, which lmgw retries once
                    // on a fresh port.
                    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                        Ok(listener) => {
                            let app = axum::Router::new().fallback(|| async { "ok" });
                            tokio::spawn(async move {
                                let _ = axum::serve(listener, app).await;
                            });
                            out(0, "deadbeef\n", "")
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => out(
                            126,
                            "",
                            &format!(
                                "Error: Failed to bind port {port} (Address already in use)\n"
                            ),
                        ),
                        Err(e) => Err(e),
                    }
                }
                Some("logs") => {
                    // `--tail <n>`: the number of lines it was asked for, `-1` the
                    // whole log (forty lines here).
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
                    out(0, &format!("tail={tail}\n{body}"), "")
                }
                Some("create") => match known(&last) {
                    Some(_) => out(0, "6158a95fea92\n", ""),
                    None => out(125, "", "Error: image not known"),
                },
                Some("cp") => {
                    // The container `create` made is the last image created.
                    let image = self
                        .calls
                        .lock()
                        .unwrap()
                        .iter()
                        .rev()
                        .find(|a| a.first().map(String::as_str) == Some("create"))
                        .and_then(|a| a.last().cloned())
                        .unwrap_or_default();
                    match known(&image).and_then(|i| i.manifest) {
                        Some(text) => {
                            std::fs::write(&last, text)?;
                            out(0, "", "")
                        }
                        None => out(125, "", "Error: \"/lmgw/agent.json\" could not be found"),
                    }
                }
                _ => out(0, "", ""),
            }
        })
    }
}

/// The manifest of a `container` agent `id`: `tools` is the manifest's
/// `"tools": [...],` line or nothing, `props` more config properties
/// (`, "name": {...}`), `run_image` the `run.image` it names.
pub fn pkg(id: &str, version: &str, run_image: &str, tools: &str, props: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "{id}",
  "version": "{version}",
  "description": "an agent that ships as an image",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  {tools}
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }}{props}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{run_image}",
    "columns": ["subject"],
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32, "stop_grace_seconds": 1 }}
  }}
}}"#
    )
}

/// A chat agent `id` on the model alias `alias`.
pub fn chat(id: &str, alias: &str) -> String {
    json!({
        "schema_version": 1, "id": id, "name": id,
        "model": { "alias": alias },
        "run": { "kind": "chat", "system": "Be brief." }
    })
    .to_string()
}

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

/// A gateway over `podman`. Its `bind_addr` is the port it listens on and its
/// container prefix is its own, as the container runtime's suites need.
pub async fn gateway(podman: Podman) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = format!(
        "lmgwa{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    settings.bind_addr = addr.to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    state.set_agent_spawner_for_tests(Arc::new(podman));
    let key = dashboard_key(&state);
    tokio::spawn(serve_on(listener, build_router(state.clone())));
    (
        state,
        Gw {
            base: format!("http://{addr}"),
            key,
        },
    )
}

/// Lets the `search` calls of [`held_source`] answer.
pub struct Release(watch::Sender<bool>);

impl Release {
    /// The waiting `search` calls answer now, and every later one at once.
    pub fn now(&self) {
        self.0.send_replace(true);
    }
}

/// An MCP server with the two tools the `labeler` agent of `agents_runs` reads
/// (`search`, then `get` per message), whose `search` answers only once
/// [`Release::now`] is called: a run that reads it is in flight until then.
/// Register it with [`register_held`], so the gateway's own call timeout does
/// not end the run first.
pub async fn held_source() -> (McpStub, Release) {
    let (release, released) = watch::channel(false);
    let tool = |name: &str| {
        json!({ "name": name, "description": name,
                "inputSchema": { "type": "object", "properties": {} } })
    };
    let reply = answer(move |name, _args| {
        let mut released = released.clone();
        async move {
            if name == "search" {
                let _ = released.wait_for(|r| *r).await;
                json!({ "content": [], "isError": false,
                        "structuredContent": { "messages": [{ "id": "m1" }] } })
            } else {
                json!({ "content": [{ "type": "text",
                        "text": json!({ "subject": "about m1" }).to_string() }],
                        "isError": false })
            }
        }
    });
    let stub = answering(json!([tool("search"), tool("get")]), false, reply).await;
    (stub, Release(release))
}

/// [`held_source`]'s server, registered as the `gws` label the `labeler` agent
/// reads.
///
/// A registered server has a `timeout_ms`, and the gateway ends a call that
/// takes longer: a held `search` would fail the run 5 s after it was sent, by
/// the clock and not when the test lets it go. A test that is slow for those
/// 5 s (one `common::round_trips` builds the whole OpenAPI document on its
/// first call, 5 s of CPU on a debug build, and that holds a test's runtime
/// for the duration) then finds its run over. The server's timeout here is the
/// whole of what the harness lets a test run (`patience::budget`), so the run
/// ends when [`Release::now`] is called or when the harness ends the test.
pub async fn register_held(state: &SharedState, source: &McpStub) {
    let timeout_ms = patience::budget().map_or(u64::from(u32::MAX), |budget| {
        u64::try_from(budget.as_millis()).unwrap_or(u64::from(u32::MAX))
    });
    register_timing_out_after(state, "gws", "gws", &source.url, true, None, timeout_ms).await;
}
