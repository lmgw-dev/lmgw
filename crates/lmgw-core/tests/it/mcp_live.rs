//! Gated **live** end-to-end MCP integration test (§16, §17 M5).
//!
//! Spins a real reference stdio MCP server inside **Podman**, points an
//! `McpServer` config at it (Podman-isolated stdio, the default posture, §5.1),
//! and drives the *whole* gateway through it: connect → `tools/list` (the
//! reference tools must appear `__`-prefixed) → `tools/call` one of them. If the
//! reference server can sample, it also exercises `sampling/createMessage` served
//! by a configured alias.
//!
//! # Why it is `#[ignore]`-d
//! It needs **Podman + a pulled image + (optionally) a reachable
//! `sampling_alias`** — none of which exist in CI or the dev sandbox. The normal
//! `cargo test` run therefore does **not** include it (it stays green without
//! Podman). Run it manually:
//!
//! ```text
//! # 1. Pull the official MCP reference "everything" server image once:
//! podman pull docker.io/mcp/everything:latest
//! #    (equivalently `npx -y @modelcontextprotocol/server-everything`, but the
//! #     container image is what this test drives — Podman-isolated, not Docker.)
//! #
//! # 2. (sampling leg only) make a sampling alias reachable. EITHER point
//! #    LMGW_LIVE_SAMPLING_ALIAS at a real alias served by a running lmgw/llama.cpp
//! #    (NEVER Ollama), OR leave it unset to skip just the sampling assertion.
//! #
//! # 3. Run the ignored test:
//! cargo test -p lmgw-core --test it mcp_live:: -- --ignored --nocapture
//! ```
//!
//! # Prerequisites (all must hold for the full run)
//! - `podman` on `PATH` and able to `run --rm -i docker.io/mcp/everything`.
//! - The `mcp/everything` image pulled (first call otherwise pays the pull; the
//!   test allows generous time for it but a pre-pull is recommended).
//! - For the sampling leg: `LMGW_LIVE_SAMPLING_ALIAS` set to an alias this test's
//!   in-memory gateway can resolve. Since the test builds an isolated in-memory
//!   `AppState`, the simplest path is to also register that alias against a real
//!   upstream — see `maybe_sampling_leg` for how it's wired. If unset, the
//!   sampling assertion is skipped (the connect/list/call legs still run).
//!
//! # What it asserts
//! - the server reaches `Ready` and `tools/list` exposes the reference tools
//!   under the configured `everything__` prefix (e.g. `everything__echo`);
//! - `tools/call everything__echo` round-trips a payload back through the
//!   gateway's reverse-map routing;
//! - (gated) a sampling call is served by the configured alias and logged as
//!   `mcp-sampling`.

use std::time::Duration;

use lmgw_core::config::{McpServer, McpTransport, Snapshot};
use lmgw_core::state::{AppState, SharedState};

/// The official MCP reference server image (Podman-isolated stdio). Documented
/// here, not hidden in a helper, so the exact image is obvious to a manual runner.
const EVERYTHING_IMAGE: &str = "docker.io/mcp/everything:latest";
/// The per-server `tool_prefix` we expose the reference tools under.
const PREFIX: &str = "everything";

/// Is `podman` usable on this host? A cheap `podman --version`. The test is
/// `#[ignore]`-d anyway; this only turns a missing-Podman manual run into a clear
/// skip+message instead of a confusing transport error.
fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build an `McpServer` pointing at the reference server, Podman-isolated stdio.
/// `allow_sampling` follows whether a sampling alias was provided.
fn everything_server(allow_sampling: bool, sampling_alias: Option<String>) -> McpServer {
    McpServer {
        id: 1,
        name: "everything".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        // Podman-isolated: container_image set ⇒ stdio_argv synthesizes
        // `podman run --rm -i --quiet … <image>`. No bind mounts here, so no `:Z`.
        command: None,
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: Some(EVERYTHING_IMAGE.into()),
        extra_run_args: vec![],
        url: None,
        headers: vec![],
        tool_prefix: PREFIX.into(),
        timeout_ms: 60_000,
        autostart: true,
        idle_seconds: 0,
        allow_sampling,
        sampling_alias,
        agent_id: None,
    }
}

/// An in-memory gateway state with the reference server (and optionally a
/// sampling alias) installed in the snapshot, reconciled so the connection comes
/// up. Returns the state.
async fn gateway_with_everything() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let alias = std::env::var("LMGW_LIVE_SAMPLING_ALIAS")
        .ok()
        .filter(|s| !s.is_empty());

    let mut snap = Snapshot::default();
    snap.mcp_servers
        .insert(1, everything_server(alias.is_some(), alias.clone()));
    if let Some(a) = &alias {
        // Surface the global default too, so resolution doesn't depend solely on
        // the per-server override.
        snap.settings.sampling_alias = a.clone();
    }
    state.set_snapshot_for_tests(snap);
    // Connect the southbound server (Podman pull on first run may be slow).
    state.mcp.reconcile(&state.snapshot()).await;
    state
}

#[tokio::test]
#[ignore = "live: needs Podman + the mcp/everything image (and optionally a sampling alias); run with --ignored"]
async fn live_everything_connect_list_call() {
    if !podman_available() {
        eprintln!("SKIP live_everything: podman not available on PATH");
        return;
    }

    let state = gateway_with_everything().await;
    let snap = state.snapshot();

    // Give the connection up to a generous budget to reach Ready (cold image
    // pull is the slow case). reconcile already kicked the connect; poll status.
    let mut ready = false;
    for _ in 0..120 {
        let views = state.mcp.status_views(&snap).await;
        if views.iter().any(|v| v.status == "ready") {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        ready,
        "the everything server must reach Ready (is the image pulled?)"
    );

    // tools/list: the reference tools must appear under the `everything__` prefix.
    let agg = state.mcp.list_tools(&snap).await;
    let names: Vec<String> = agg.tools.iter().map(|t| t.name.to_string()).collect();
    assert!(
        names.iter().any(|n| n.starts_with(&format!("{PREFIX}__"))),
        "expected __-prefixed reference tools, got: {names:?}"
    );
    // `echo` is a stable tool of the everything server.
    let echo = format!("{PREFIX}__echo");
    assert!(
        names.contains(&echo),
        "expected {echo} in the aggregate, got: {names:?}"
    );

    // tools/call everything__echo — routes through the reverse map to the server.
    let mut args = serde_json::Map::new();
    args.insert("message".into(), serde_json::json!("lmgw-live"));
    let (result, server_name) = state
        .mcp
        .call(&snap, &echo, Some(args))
        .await
        .expect("echo call must succeed");
    assert_eq!(server_name, "everything");
    let text = serde_json::to_string(&result.content).unwrap_or_default();
    assert!(
        text.contains("lmgw-live"),
        "echo must round-trip the payload, got: {text}"
    );

    maybe_sampling_leg(&state).await;
}

/// Sampling leg (gated within the gated test): if `LMGW_LIVE_SAMPLING_ALIAS` is
/// set AND resolves, ask the everything server to run its `sampleLLM` tool, which
/// issues a `sampling/createMessage` back to us — served by the configured alias
/// and logged as `mcp-sampling`. If the alias is unset/unresolvable, skip this
/// leg (the connect/list/call assertions above already ran).
async fn maybe_sampling_leg(state: &SharedState) {
    let snap = state.snapshot();
    let Some(alias) = std::env::var("LMGW_LIVE_SAMPLING_ALIAS")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP sampling leg: LMGW_LIVE_SAMPLING_ALIAS not set");
        return;
    };
    if snap.resolve(&alias).is_err() {
        eprintln!("SKIP sampling leg: alias '{alias}' does not resolve in this test gateway");
        return;
    }

    let sample_tool = format!("{PREFIX}__sampleLLM");
    if !state
        .mcp
        .list_tools(&snap)
        .await
        .tools
        .iter()
        .any(|t| t.name.as_ref() == sample_tool)
    {
        eprintln!("SKIP sampling leg: this everything server build has no {sample_tool}");
        return;
    }

    let mut args = serde_json::Map::new();
    args.insert("prompt".into(), serde_json::json!("Say hi in one word."));
    args.insert("maxTokens".into(), serde_json::json!(16));
    let (result, _server) = state
        .mcp
        .call(&snap, &sample_tool, Some(args))
        .await
        .expect("sampleLLM (sampling) call must succeed");
    let text = serde_json::to_string(&result.content).unwrap_or_default();
    assert!(!text.is_empty(), "sampleLLM must return content");

    // A mcp-sampling row must have been written (served by our alias).
    let rows = lmgw_core::store::query_logs(
        &state.db,
        &lmgw_core::store::LogFilter {
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        rows.iter().any(|r| r.ingress_proto == "mcp-sampling"),
        "the sampling sub-call must log an mcp-sampling row"
    );
}
