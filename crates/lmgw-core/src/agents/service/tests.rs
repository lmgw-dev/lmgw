//! Service mode without podman: the start, the idle stop, the in-flight guard
//! and the `agent:<id>` MCP row (container-runtime §3.3, §6.5).
//!
//! The fake spawner does what a real `podman run -d` would do and nothing else:
//! it records the argv and **binds the host port the argv publishes**, so the
//! health probe has something real to answer it. That is what makes "one start
//! for N callers", "stopped after its window" and "a request in flight holds
//! the stop off" assertable here rather than only against a container.
//!
//! `tests/it/agents_service.rs` drives the same paths through the proxy, and its
//! `the_real_thing_*` half against real podman.

use super::*;

use std::sync::atomic::AtomicU32;

use async_trait::async_trait;

use crate::agents::container::{Spawned, Spawner};
use crate::runtime::registry::CmdOutput;
use crate::state::AppState;
use crate::store::AgentRow;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

/// A gateway with its own `container_prefix`, so two tests in one process do
/// not share a run directory or a container name.
async fn gateway() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = format!(
        "lmgwsvc{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    crate::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

fn doc(idle_seconds: i64, start_timeout: u64, provides: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "model": {{ "alias": "m1" }},
  "run": {{
    "kind": "container",
    "image": "localhost/board:1",
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32, "stop_grace_seconds": 1 }},
    "service": {{ "port": 8080, "idle_seconds": {idle_seconds},
                 "start_timeout_seconds": {start_timeout} }}{provides}
  }}
}}"#
    )
}

async fn install(state: &SharedState, manifest: &str) -> Agent {
    let m = crate::agents::manifest::load(manifest).expect("the fixture manifest parses");
    crate::store::insert_agent(&state.db, &m.id, &m.to_json(), "authored")
        .await
        .unwrap();
    agent_of(state, &m.id).await
}

async fn agent_of(state: &SharedState, id: &str) -> Agent {
    let row: AgentRow = crate::store::get_agent(&state.db, id)
        .await
        .unwrap()
        .expect("the row is there");
    Agent::from_row(row).unwrap()
}

// ---------------------------------------------------------------------------
// The fake spawner
// ---------------------------------------------------------------------------

/// Records every `podman` invocation and, for a `run -d`, **binds the host port
/// the argv publishes** — a fake container that answers its health probe.
#[derive(Default)]
struct Fake {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    /// Never bind the port, so the health probe has to time out.
    deaf: bool,
}

impl Fake {
    fn calls(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        self.calls.clone()
    }
    fn deaf() -> Self {
        Self {
            deaf: true,
            ..Default::default()
        }
    }
}

fn published_port(argv: &[String]) -> Option<u16> {
    let i = argv.iter().position(|a| a == "-p")?;
    argv.get(i + 1)?
        .rsplit(':')
        .nth(1)?
        .parse::<u16>()
        .ok()
        .filter(|_| true)
}

fn runs(calls: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<Vec<String>> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.first().map(String::as_str) == Some("run"))
        .cloned()
        .collect()
}

fn verbs(calls: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|a| a.first().cloned())
        .collect()
}

#[async_trait]
impl Spawner for Fake {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        panic!("service mode never uses the streaming seam");
    }

    async fn run(&self, _p: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        if args.first().map(String::as_str) == Some("run") && !self.deaf {
            // `-p 127.0.0.1:<host>:<container>` — bind the host side, which is
            // exactly what the real container's published port does.
            let port = published_port(args).expect("a service run publishes a port");
            let app = axum::Router::new().route(
                "/",
                axum::routing::get(|| async { "hello from the fake container" }),
            );
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
        }
        Ok(CmdOutput {
            status: 0,
            stdout: "deadbeef\n".into(),
            stderr: String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// The cancel (§3.3)
// ---------------------------------------------------------------------------

/// A Stop that comes before the starter first listens for it — still in its
/// token and run-directory steps — is kept for when it does. Fired with
/// `send`, it was dropped while nobody listened (the channel had no
/// receiver), and the start went on: an unbounded one for good, with the
/// Stop waiting on its answer. `agents_service`'s stop tests met it once
/// their Stop stopped waiting a fixed 200–400 ms first, and under load
/// before that.
#[test]
fn a_cancel_fired_before_anyone_listens_is_kept() {
    use futures::FutureExt;
    let cancel = Cancel::new();
    cancel.fire();
    assert!(
        cancel.cancelled().now_or_never().is_some(),
        "the cancel was lost"
    );
    assert!(
        Cancel::new().cancelled().now_or_never().is_none(),
        "nothing fired"
    );
}

// ---------------------------------------------------------------------------
// The argv (§6.5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_service_argv_is_detached_published_on_loopback_and_labelled_service() {
    let state = gateway().await;
    let fake = Fake::default();
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    let agent = install(&state, &doc(0, 10, "")).await;

    let live = ensure(&state, &agent).await.expect("it starts");
    let argv = runs(&calls).remove(0);
    let at = |s: &str| argv.iter().position(|a| a == s);

    // Detached, and **not** `--rm`: `podman logs` is the only account of a
    // container that dies during its probe.
    assert_eq!(at("-d"), Some(1), "{argv:?}");
    assert!(
        at("--rm").is_none(),
        "a service container is not --rm: {argv:?}"
    );
    assert!(at("--replace").is_some(), "{argv:?}");
    // Loopback only: the proxy is the sole client.
    let p = at("-p").expect("published");
    assert_eq!(argv[p + 1], format!("127.0.0.1:{}:8080", live.host_port));
    // The label boot reconciliation reads: not a job id, because there is no
    // job.
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--label" && w[1] == "lmgw.run=service"),
        "{argv:?}"
    );
    // The hygiene a phase run gets, unchanged.
    for want in [
        "--cap-drop=ALL",
        "--read-only",
        "--pull=never",
        "--security-opt",
    ] {
        assert!(at(want).is_some(), "{want} missing: {argv:?}");
    }
    let env: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(i, _)| i > &0 && argv[i - 1] == "-e")
        .map(|(_, a)| a)
        .collect();
    assert!(env.iter().any(|e| *e == "LMGW_PHASE=service"), "{env:?}");
    // The public origin, with no trailing slash (§4.6) — and the old
    // `LMGW_APP_BASE` gone rather than kept beside it, because an app that
    // still read it would build URLs for a mount that no longer exists.
    assert!(
        env.iter()
            .any(|e| *e == "LMGW_APP_ORIGIN=http://board.localhost:8787"),
        "{env:?}"
    );
    assert!(
        !env.iter().any(|e| e.starts_with("LMGW_APP_BASE")),
        "{env:?}"
    );
    assert!(env.iter().any(|e| *e == "LMGW_PORT=8080"), "{env:?}");
    // A service has no run, so it is told of no ledger, no run id and no
    // deadline rather than being handed a zero it would have to disbelieve.
    for absent in ["LMGW_RUN=", "LMGW_LEDGER_URL=", "LMGW_DEADLINE_SECONDS="] {
        assert!(
            !env.iter().any(|e| e.starts_with(absent)),
            "{absent} must not be set for a service: {env:?}"
        );
    }
    // No secret on the argv, ever.
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("secret") && a.starts_with("-e")),
        "{argv:?}"
    );
    // And `input.json`, read back off the disk lmgw wrote it to: the same
    // origin string the env var carries, under `service.origin` (§4.6) — an
    // image reads its address the one way, whichever half of itself is
    // running.
    let input = mounted_json(&argv, "/lmgw/input.json");
    assert_eq!(input["phase"], "service", "{input}");
    assert_eq!(
        input["service"]["origin"],
        json!("http://board.localhost:8787"),
        "{input}"
    );
    assert!(input["service"].get("app_base").is_none(), "{input}");

    stop(&state, "board", "test over").await;
}

/// One `-v host:inside:…` mount, read back off disk while the run directory is
/// still there.
fn mounted_json(argv: &[String], inside: &str) -> serde_json::Value {
    let host = argv
        .iter()
        .enumerate()
        .filter(|(i, a)| *a == "-v" && i + 1 < argv.len())
        .map(|(i, _)| argv[i + 1].split(':').collect::<Vec<_>>())
        .find(|parts| parts.len() > 1 && parts[1] == inside)
        .map(|parts| parts[0].to_string())
        .unwrap_or_else(|| panic!("no mount at {inside}: {argv:?}"));
    let body = std::fs::read_to_string(&host).expect("the run directory is still there");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{inside} is not JSON ({e}): {body}"))
}

// ---------------------------------------------------------------------------
// One start, many waiters (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_first_requests_produce_exactly_one_podman_run() {
    let state = gateway().await;
    let fake = Fake::default();
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    let agent = install(&state, &doc(0, 10, "")).await;

    let mut set = Vec::new();
    for _ in 0..8 {
        let st = state.clone();
        let a = agent.clone();
        set.push(tokio::spawn(async move { ensure(&st, &a).await }));
    }
    let mut ports = Vec::new();
    for h in set {
        ports.push(h.await.unwrap().expect("it starts").host_port);
    }
    assert_eq!(runs(&calls).len(), 1, "eight callers, one start");
    // And every one of them got the same container.
    assert!(ports.windows(2).all(|w| w[0] == w[1]), "{ports:?}");

    stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn a_second_request_finds_it_warm_and_does_not_start_again() {
    let state = gateway().await;
    let fake = Fake::default();
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    let agent = install(&state, &doc(0, 10, "")).await;

    let a = ensure(&state, &agent).await.unwrap();
    let b = ensure(&state, &agent).await.unwrap();
    assert_eq!(a.host_port, b.host_port);
    assert_eq!(runs(&calls).len(), 1);

    stop(&state, "board", "test over").await;
}

// ---------------------------------------------------------------------------
// Failure (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_start_that_never_answers_fails_with_the_reason_and_the_log() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::deaf()));
    // One second is the whole point: the bound is the manifest's, printed in
    // the message.
    let agent = install(&state, &doc(0, 1, "")).await;

    let e = ensure(&state, &agent).await.expect_err("nothing answers");
    assert!(
        e.reason.contains("run.service.start_timeout_seconds (1s)"),
        "{}",
        e.reason
    );
    assert!(e.reason.contains("http://127.0.0.1:"), "{}", e.reason);
    // The container's own account travels with the refusal.
    assert!(!e.log.is_empty(), "the log tail is quoted back");
    // And nothing is left in the map to be found "running".
    assert!(state.agent_services.get("board").is_none());
    // A second attempt is allowed to try again rather than inheriting the
    // failure.
    let _ = ensure(&state, &agent).await;
}

#[tokio::test]
async fn a_disabled_agent_does_not_start() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    install(&state, &doc(0, 10, "")).await;
    crate::store::set_agent_enabled(&state.db, "board", false)
        .await
        .unwrap();
    let agent = agent_of(&state, "board").await;

    let e = ensure(&state, &agent).await.expect_err("disabled");
    assert!(e.reason.contains("is disabled"), "{}", e.reason);
}

// ---------------------------------------------------------------------------
// The idle stop and its in-flight guard (§3.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_idle_service_is_stopped_after_its_window() {
    let state = gateway().await;
    let fake = Fake::default();
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    let agent = install(&state, &doc(1, 10, "")).await;

    let live = ensure(&state, &agent).await.unwrap();
    // Not yet: the window has not passed.
    assert!(sweep_idle(&state).await.is_empty());
    assert!(state.agent_services.get("board").is_some());

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let stopped = sweep_idle(&state).await;
    assert_eq!(stopped, vec![live.container.clone()]);
    assert!(state.agent_services.get("board").is_none());
    assert!(
        verbs(&calls).contains(&"stop".to_string()),
        "the stop ladder ran: {:?}",
        verbs(&calls)
    );

    // And the next request starts it again — a new container, a new port.
    let again = ensure(&state, &agent).await.unwrap();
    assert_eq!(runs(&calls).len(), 2, "it restarted on demand");
    assert_eq!(again.container, live.container, "same name, new container");
    stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn idle_seconds_zero_is_a_true_warm_keep() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    let agent = install(&state, &doc(0, 10, "")).await;

    let live = ensure(&state, &agent).await.unwrap();
    // Older than any window anyone would set, and still not reaped: `0` is
    // never, exactly as `McpServer::idle_seconds` means it.
    live.touch();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(sweep_idle(&state).await.is_empty());
    assert!(state.agent_services.get("board").is_some());

    stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn a_request_in_flight_holds_off_the_idle_stop() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    let agent = install(&state, &doc(1, 10, "")).await;

    let live = ensure(&state, &agent).await.unwrap();
    let guard = live.guard();
    // `last_used` only advances when a request *finishes*, so a long request
    // looks maximally idle — which is exactly why the counter, not the clock,
    // is what the sweep asks.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(live.in_flight(), 1);
    assert!(
        sweep_idle(&state).await.is_empty(),
        "a request in flight must not be torn down"
    );
    assert!(state.agent_services.get("board").is_some());

    // Releasing it restarts the window rather than making it instantly due.
    drop(guard);
    assert!(sweep_idle(&state).await.is_empty(), "the window restarts");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(sweep_idle(&state).await.len(), 1);
}

/// A request that arrives mid-stop waits for the stop rather than racing a
/// `podman run --replace` against the `rm -f` that is still going.
#[tokio::test]
async fn a_request_arriving_mid_stop_waits_and_then_starts_a_clean_one() {
    let state = gateway().await;
    let fake = Fake::default();
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(Arc::new(fake));
    let agent = install(&state, &doc(0, 10, "")).await;
    ensure(&state, &agent).await.unwrap();

    let st = state.clone();
    let stopper = tokio::spawn(async move { stop(&st, "board", "racing").await });
    let st = state.clone();
    let a = agent.clone();
    let starter = tokio::spawn(async move { ensure(&st, &a).await.map(|l| l.host_port) });

    assert!(stopper.await.unwrap().is_some());
    let port = starter.await.unwrap().expect("it started after the stop");
    assert!(port > 0);
    // Two starts, in order, not two overlapping ones.
    assert_eq!(runs(&calls).len(), 2);
    let order = verbs(&calls);
    assert_eq!(
        order.iter().rposition(|v| v == "run"),
        Some(order.len() - 1),
        "the second start came after the stop and the rm: {order:?}"
    );

    stop(&state, "board", "test over").await;
}

#[tokio::test]
async fn stopping_something_that_is_not_running_is_success() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    install(&state, &doc(0, 10, "")).await;
    assert!(stop(&state, "board", "nothing to do").await.is_none());
}

// ---------------------------------------------------------------------------
// `provides.mcp` (§3.3)
// ---------------------------------------------------------------------------

const PROVIDES: &str = r#", "provides": { "mcp": "/mcp" }"#;

#[test]
fn the_row_points_at_the_proxy_not_at_the_ephemeral_host_port() {
    // The host port changes on every start and an `mcp_servers` row is
    // persistent, so the row names lmgw's own stable proxy path.
    assert_eq!(
        mcp_url("127.0.0.1:8001", "board"),
        "http://127.0.0.1:8001/agents/board/mcp"
    );
    // A wildcard bind resolves to loopback, which is `net::primary_base_url`'s
    // own rule.
    assert!(mcp_url("0.0.0.0:8001", "board").ends_with("/agents/board/mcp"));
    assert_eq!(mcp_row_name("board"), "agent:board");
}

#[tokio::test]
async fn a_provides_mcp_manifest_earns_a_row_and_loses_it_again() {
    let state = gateway().await;
    let agent = install(&state, &doc(300, 10, PROVIDES)).await;

    sync_mcp_registration(&state, &agent).await.unwrap();
    let rows = crate::store::list_mcp_servers(&state.db).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.name == "agent:board")
        .expect("the agent's own row");
    assert_eq!(row.agent_id.as_deref(), Some("board"));
    assert_eq!(row.tool_prefix, "board");
    assert!(!row.autostart, "the proxy starts it, nothing else");
    assert_eq!(row.idle_seconds, 300, "inherited from service.idle_seconds");
    assert!(
        row.url.as_deref().unwrap().ends_with("/agents/board/mcp"),
        "{:?}",
        row.url
    );
    // The gateway must not refuse its own proxy path as a self-loop.
    crate::ops::reject_self_loop(&state.snapshot(), row.url.as_deref().unwrap())
        .expect("/agents/<id>/mcp is not the aggregate endpoint");

    // Taking `provides` out of the manifest takes the row with it.
    let plain = crate::agents::manifest::load(&doc(300, 10, "")).unwrap();
    crate::store::update_agent_manifest(&state.db, "board", &plain.to_json())
        .await
        .unwrap();
    resync(&state, "board").await.unwrap();
    assert!(!crate::store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .iter()
        .any(|r| r.name == "agent:board"));
}

#[tokio::test]
async fn the_row_is_rewritten_rather_than_duplicated_on_a_second_sync() {
    let state = gateway().await;
    let agent = install(&state, &doc(300, 10, PROVIDES)).await;
    sync_mcp_registration(&state, &agent).await.unwrap();
    sync_mcp_registration(&state, &agent).await.unwrap();
    let n = crate::store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .iter()
        .filter(|r| r.name == "agent:board")
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_disabled_agent_offers_no_tools() {
    let state = gateway().await;
    install(&state, &doc(300, 10, PROVIDES)).await;
    crate::store::set_agent_enabled(&state.db, "board", false)
        .await
        .unwrap();
    resync(&state, "board").await.unwrap();
    let rows = crate::store::list_mcp_servers(&state.db).await.unwrap();
    let row = rows.iter().find(|r| r.name == "agent:board").unwrap();
    assert!(!row.enabled);
}

#[test]
fn the_reserved_prefix_is_refused_for_an_owner_created_row() {
    assert!(crate::ops::reject_agent_name("agent:board").is_err());
    assert!(crate::ops::reject_agent_name("agents-of-shield").is_ok());
}

// ---------------------------------------------------------------------------
// The dev override (§3.4)
// ---------------------------------------------------------------------------

/// The gateway this box is running, for the self-loop check.
const BIND: &str = "127.0.0.1:8787";

#[test]
fn a_dev_url_is_http_on_loopback_and_nothing_else() {
    for good in [
        "http://127.0.0.1:5173",
        "http://127.0.0.2:5173",
        "http://localhost:5173",
        "http://LOCALHOST:5173",
        "https://localhost:5173",
        "http://[::1]:5173",
    ] {
        assert!(
            validate_dev_url(good, BIND).is_ok(),
            "{good} should be accepted"
        );
    }
    // Everything off this box is refused, LAN included: the app proxy is on the
    // unauthenticated, CORS-permissive dashboard plane, so a dev_url pointing
    // anywhere else would make lmgw an open reverse proxy for that host.
    for (bad, why) in [
        ("https://example.com", "is not loopback"),
        ("http://8.8.8.8:5173", "is not loopback"),
        ("http://192.168.1.14:3000", "is not loopback"),
        ("http://10.0.0.7:8080", "is not loopback"),
        ("http://172.20.0.3:8080", "is not loopback"),
        ("http://169.254.1.2:8080", "is not loopback"),
        ("http://[fd00::1]:5173", "is not loopback"),
        ("http://myhost.local:5173", "is not loopback"),
        ("http://dev.home.arpa:5173", "is not loopback"),
        ("http://user:pass@127.0.0.1:5173", "must not carry userinfo"),
        ("ftp://127.0.0.1", "must be http or https"),
        ("127.0.0.1:5173", "is not a URL"),
        ("http://127.0.0.1:5173/?x=1", "no query and no fragment"),
        // A dev server is an origin now (origins §4.7): there is no mount
        // prefix left for anything to strip.
        ("http://127.0.0.1:5173/base", "cannot carry a path"),
        ("http://127.0.0.1:5173/app/", "cannot carry a path"),
        ("http://127.0.0.1:5173#top", "no query and no fragment"),
        ("", "pass null to clear it"),
    ] {
        let e = validate_dev_url(bad, BIND).expect_err("{bad} should be refused");
        assert!(e.contains(why), "{bad}: {e}");
    }
}

#[test]
fn a_dev_url_pointing_at_lmgw_itself_is_refused_naming_the_loop() {
    // Every spelling of "this gateway" — the port is the comparison, because
    // all of these reach the same listener.
    for own in [
        "http://127.0.0.1:8787",
        "http://localhost:8787",
        "http://[::1]:8787",
        "http://127.0.0.1:8787/app",
    ] {
        let e = validate_dev_url(own, BIND).expect_err("{own} is lmgw itself");
        assert!(e.contains("must not be lmgw itself"), "{own}: {e}");
        assert!(e.contains("loop until"), "{own}: {e}");
    }
    // A different port on the same loopback is the ordinary case.
    assert!(validate_dev_url("http://127.0.0.1:5173", BIND).is_ok());
    // And the check follows the setting rather than a hard-coded 8787.
    assert!(validate_dev_url("http://127.0.0.1:5173", "0.0.0.0:5173").is_err());
}

#[test]
fn a_dev_url_is_stored_without_its_trailing_slash_and_carries_no_path() {
    // The bare slash is the same origin written out, so it is normalised away
    // rather than refused — nobody types an origin twice to find out which
    // spelling this field wanted.
    assert_eq!(
        validate_dev_url("http://127.0.0.1:5173/", BIND).unwrap(),
        "http://127.0.0.1:5173"
    );
    assert_eq!(
        validate_dev_url("  http://127.0.0.1:5173  ", BIND).unwrap(),
        "http://127.0.0.1:5173"
    );
    // Anything more than that slash is a path, and the app is served at the
    // root of its own origin now (§4.7).
    let e = validate_dev_url("http://127.0.0.1:5173/app/", BIND).expect_err("a path");
    assert!(e.contains("a dev_url is an origin"), "{e}");
    assert!(e.contains("--public-url"), "{e}");
}

#[tokio::test]
async fn a_row_with_a_dev_url_is_served_from_it_and_starts_no_container() {
    let state = gateway().await;
    let fake = Arc::new(Fake::default());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    install(&state, &doc(300, 10, "")).await;
    crate::store::set_agent_dev_url(&state.db, "board", Some("http://127.0.0.1:5173"))
        .await
        .unwrap();
    let agent = agent_of(&state, "board").await;

    assert_eq!(dev_url_of(&agent).as_deref(), Some("http://127.0.0.1:5173"));
    match target(&state, &agent).await.expect("a dev target") {
        Target::Dev(url) => assert_eq!(url, "http://127.0.0.1:5173"),
        Target::Container(_) => panic!("a dev_url must not start a container"),
    }
    // The MCP plane's wake-up is a no-op rather than a failure: the dev server
    // is already running and the proxy will reach it.
    ensure_by_id(&state, "board")
        .await
        .expect("nothing to start");
    assert!(runs(&calls).is_empty(), "{:?}", verbs(&calls));

    // Clearing it goes back to the image.
    crate::store::set_agent_dev_url(&state.db, "board", None)
        .await
        .unwrap();
    let agent = agent_of(&state, "board").await;
    assert_eq!(dev_url_of(&agent), None);
    match target(&state, &agent).await.expect("the container starts") {
        Target::Container(live) => assert!(live.host_port > 0),
        Target::Dev(_) => panic!("the dev_url was cleared"),
    }
    assert_eq!(runs(&calls).len(), 1, "{:?}", verbs(&calls));
    stop(&state, "board", "end of test").await;
}

// ---------------------------------------------------------------------------
// The agent origin (origins §4.1, §4.6, §4.9)
// ---------------------------------------------------------------------------

/// A manifest with the given id, serving or not. `service` is what earns an
/// origin, and every rule below applies to that half only.
fn origin_doc(id: &str, service: bool) -> Manifest {
    let block = if service {
        r#", "service": { "port": 8080 }"#
    } else {
        ""
    };
    crate::agents::manifest::load(&format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "An agent",
  "model": {{ "alias": "m1" }},
  "run": {{ "kind": "container", "image": "localhost/x:1"{block} }}
}}"#
    ))
    .expect("the fixture manifest parses")
}

fn settings_with(suffix: &str, bind_addr: &str) -> Settings {
    Settings {
        agent_origin_suffix: suffix.into(),
        bind_addr: bind_addr.into(),
        ..Settings::default()
    }
}

#[test]
fn the_origin_is_the_id_under_the_suffix_on_lmgws_own_port() {
    let s = settings_with("localhost", BIND);
    assert_eq!(origin_host(&s, "board"), "board.localhost");
    assert_eq!(origin_authority(&s, "board"), "board.localhost:8787");
    // Two forms, deliberately: the env var has no trailing slash (it is an
    // origin to concatenate onto), the DTO has one (it is an href).
    assert_eq!(
        agent_origin_base(&s, "board"),
        "http://board.localhost:8787"
    );
    assert_eq!(agent_origin(&s, "board"), "http://board.localhost:8787/");
    // The suffix is the only thing that moves; the label is the id itself.
    let lan = settings_with("lmgw.lan", "192.168.1.10:8001");
    assert_eq!(agent_origin(&lan, "board"), "http://board.lmgw.lan:8001/");
}

#[test]
fn a_service_id_that_is_not_a_dns_label_is_refused() {
    // `validate_id` allows 64 characters and a trailing '-'; DNS allows
    // neither, and only a serving agent is a host name.
    let long = "a".repeat(64);
    let e = origin_label_refusal(&origin_doc(&long, true)).expect("64 is one too many");
    assert!(e.contains("64 characters"), "{e}");
    assert!(e.contains("at most 63"), "{e}");
    let e = origin_label_refusal(&origin_doc("board-", true)).expect("a trailing dash");
    assert!(e.contains("cannot end in '-'"), "{e}");
    // 63 is a label, and the same ids without a service block are nobody's
    // host name.
    assert_eq!(
        origin_label_refusal(&origin_doc(&"a".repeat(63), true)),
        None
    );
    assert_eq!(origin_label_refusal(&origin_doc(&long, false)), None);
    assert_eq!(origin_label_refusal(&origin_doc("board-", false)), None);
}

#[test]
fn an_agent_origin_that_is_the_gateways_own_address_is_refused() {
    // A bind address that is a name rather than an IP: `reachable_urls` passes
    // it through, so `board.lan` is an address this gateway answers on.
    let s = settings_with("lan", "board.lan:8001");
    let e = origin_shadows_refusal(&s, &origin_doc("board", true)).expect("it takes the door");
    assert!(e.contains("board.lan"), "{e}");
    assert!(e.contains("this gateway's own address"), "{e}");
    // Any other label under the same suffix is fine, and a non-serving agent
    // named like the gateway has no origin to collide with.
    assert_eq!(origin_shadows_refusal(&s, &origin_doc("cards", true)), None);
    assert_eq!(
        origin_shadows_refusal(&s, &origin_doc("board", false)),
        None
    );
    // The default case: nothing under `.localhost` is an IP literal.
    let d = settings_with("localhost", BIND);
    assert_eq!(origin_shadows_refusal(&d, &origin_doc("board", true)), None);
}

/// Every shape [`origin_label`] has to read the same way the resolver does
/// (§4.2) — the dispatch's whole namespace decision is this function.
#[test]
fn origin_label_reads_a_host_back_to_an_id() {
    let s = settings_with("localhost", BIND);
    let label = |host: &str| origin_label(&s, host);

    // The port is the caller's to strip, so both forms arrive here.
    assert_eq!(label("board.localhost"), Some("board".into()));
    assert_eq!(label("BOARD.LocalHost"), Some("board".into()));
    // The absolute form: one trailing dot is the same name, and a host that
    // fell through here would be served the dashboard on an agent's name.
    assert_eq!(label("board.localhost."), Some("board".into()));
    assert_eq!(label("BOARD.LOCALHOST."), Some("board".into()));
    // Exactly one, though: a double dot is an empty label and no host at all.
    assert_eq!(label("board.localhost.."), None);

    // Addresses are addresses, in either spelling and with or without the
    // rooting dot.
    assert_eq!(label("[::1]"), None);
    assert_eq!(label("127.0.0.1"), None);
    assert_eq!(label("127.0.0.1."), None);
    // A suffix of `1` must not make `127.0.0.1` the agent `127.0.0`.
    assert_eq!(origin_label(&settings_with("1", BIND), "127.0.0.1"), None);

    // The suffix on its own is a host the gateway may well answer on: an empty
    // label is not an id.
    assert_eq!(label("localhost"), None);
    assert_eq!(label("localhost."), None);
    assert_eq!(label(".localhost"), None);

    // Two labels, and userinfo somebody spliced in: both come back as they are
    // and are refused by the id lookup, because an id is `[a-z0-9-]` and
    // neither of these can be one. The parser does not get to decide what an
    // agent is.
    assert_eq!(label("evil.board.localhost"), Some("evil.board".into()));
    assert_eq!(label("user@board.localhost"), Some("user@board".into()));
}

/// The mDNS zone, refused by name (§4.1, F2): nothing here can make `.local`
/// answer for an arbitrary id, and the box already owns a name in it.
#[test]
fn the_mdns_zone_is_not_a_suffix_anyone_can_have() {
    let e = origin_suffix_refusal("local", BIND).expect("'local' is mDNS");
    assert!(e.contains("mDNS"), "{e}");
    assert!(e.contains("<host>.local"), "{e}");
    // Whatever the box is called and however it is bound.
    assert!(origin_suffix_refusal("LOCAL.", "0.0.0.0:8001").is_some());
    // A zone of one's own under it is a different question and stays legal.
    assert_eq!(
        suffix_refusal_against("apps.local", &["127.0.0.1".into(), "myhost.local".into()]),
        None
    );
}

/// The cookie direction (§4.1, F8): a page under the suffix cannot read the
/// dashboard's session cookie, but it can overwrite it through a parent domain
/// they share.
#[test]
fn a_suffix_sharing_a_cookie_parent_with_the_dashboard_is_refused() {
    let own = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

    let e = suffix_refusal_against("apps.lmgw.lan", &own(&["127.0.0.1", "myhost.lmgw.lan"]))
        .expect("board.apps.lmgw.lan can set Domain=lmgw.lan");
    assert!(e.contains("lmgw.lan"), "{e}");
    assert!(e.contains("Domain=lmgw.lan"), "{e}");
    assert!(e.contains("overwrite"), "{e}");
    // The dashboard's own domain, exactly, is the same clash.
    assert!(suffix_refusal_against("lmgw.lan", &own(&["myhost.lmgw.lan"])).is_some());

    // And the passes. A browser refuses a single-label cookie domain, so the
    // shipped default is safe beside a bare host name, beside an IP bind, and
    // beside the box's own mDNS name — `local` has one label.
    assert_eq!(
        suffix_refusal_against("localhost", &own(&["127.0.0.1", "myhost", "myhost.local"])),
        None
    );
    assert_eq!(
        suffix_refusal_against(
            "localhost",
            &own(&["192.168.1.10", "myhost", "myhost.lmgw.lan"])
        ),
        None
    );
    // A zone that shares nothing with the dashboard's name is what the setting
    // is for.
    assert_eq!(
        suffix_refusal_against("agents.example", &own(&["myhost.lmgw.lan"])),
        None
    );
}

/// The resolver verdict the App tab prints, against the real resolver (§4.9).
#[tokio::test]
async fn origin_resolves_asks_the_resolver_and_a_name_that_is_not_there_is_false() {
    // `127` under the suffix `0.0.1` is the authority `127.0.0.1:8787` — a
    // name NSS answers on every box, with no zone and no hosts entry, so this
    // exercises `lookup_host` rather than this machine's configuration.
    let s = settings_with("0.0.1", BIND);
    assert_eq!(origin_authority(&s, "127"), "127.0.0.1:8787");
    assert!(origin_resolves(&s, "127").await);

    // And a name no resolver may answer: `.invalid` is reserved for exactly
    // this (RFC 2606). A refusal and a timeout are the same verdict here, so
    // the assertion is the verdict *and* that it came back inside the bound
    // the App tab is written around.
    let gone = settings_with("invalid", BIND);
    let started = Instant::now();
    assert!(!origin_resolves(&gone, "no-such-agent-a7f3").await);
    assert!(
        started.elapsed() < ORIGIN_LOOKUP_TIMEOUT * 3,
        "the lookup outran its own bound: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_suffix_that_is_part_of_the_gateways_own_address_is_refused() {
    // The same rule from the other end: under `.lan`, an agent with the id
    // `board` would be served at the gateway's own name.
    let e = origin_suffix_refusal("lan", "board.lan:8001").expect("it shadows the gateway");
    assert!(e.contains("board.lan"), "{e}");
    // The shipped default is not a suffix of any loopback spelling.
    assert_eq!(origin_suffix_refusal("localhost", BIND), None);
    assert_eq!(origin_suffix_refusal("lmgw.lan", "board.lan:8001"), None);
    // An IP literal is an address the gateway answers on too, and is compared
    // the same way rather than being explained away.
    assert!(origin_suffix_refusal("1", "127.0.0.1:8787").is_some());
}
