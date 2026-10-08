//! A Podman-isolated stdio MCP server's connect, on the real podman (MCP
//! gateway design §9, 2026-10-08; the begin-write review's B-4 and its
//! re-check's R-1 to R-3). The connect creates the container first, the
//! image pull included, and bounds only its start and the MCP handshake by
//! the server's `timeout_ms`: a server that starts and never answers ends
//! `error`, and its container is removed. It stayed `connecting` for good,
//! its container running. The container goes with its session too, with a
//! connect dropped before it ended, and with the boot sweep after a crash.
//!
//! The podman tests skip, with a line that says so, where podman or the
//! base image is missing; nothing here pulls an image. Each gateway here
//! has a `container_prefix` of its own, so its containers are never
//! labelled as the installed app's.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::McpTransport;
use lmgw_core::mcp::container::{sweep_since, LABEL_MCP};
use lmgw_core::ops::{self, McpServerPatch, RowWriter};
use lmgw_core::runtime::registry::{Registry, TokioRunner};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewMcpServer};

/// The image the servers run in: on a Fedora box already, and what the
/// agents' real-podman tests build on.
const IMAGE: &str = "registry.fedoraproject.org/fedora-minimal:44";

/// The `lmgw.instance` label, the agents' and the MCP containers' alike.
const LABEL_INSTANCE: &str = lmgw_core::agents::container::LABEL_INSTANCE;

/// A stdio MCP server in POSIX sh: it answers `initialize` and lists one
/// tool, `echo`, and ends when its stdin closes.
const SH_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      pv=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"sh","version":"0"}}}\n' "$id" "$pv" ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
  esac
done
"#;

/// [`SH_SERVER`], but one that does not end when its stdin closes: it
/// sleeps on as the container's PID 1, which its stop signal does not end
/// either.
fn sh_server_ignoring_eof() -> String {
    format!("{SH_SERVER}\nexec sleep 600\n")
}

fn podman_ready(test: &str) -> bool {
    let ok = |args: &[&str]| {
        std::process::Command::new("podman")
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !ok(&["--version"]) {
        eprintln!("SKIP {test}: podman is not available on this box");
        return false;
    }
    if !ok(&["image", "exists", IMAGE]) {
        eprintln!("SKIP {test}: {IMAGE} is not on this box");
        return false;
    }
    true
}

/// A gateway whose `container_prefix` is its own: `lmgw-it-mcp-<tag>-<pid>`.
async fn gateway(tag: &str) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut s = state.snapshot().settings.clone();
    s.container_prefix = format!("lmgw-it-mcp-{tag}-{}", std::process::id());
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

/// The containers labelled `lmgw.test=<tag>`, removed whatever happened.
struct Labelled(String);

impl Labelled {
    fn new(test: &str) -> Self {
        Self(format!("{test}-{}", std::process::id()))
    }

    fn label(&self) -> String {
        format!("lmgw.test={}", self.0)
    }

    /// The ids of the containers that carry the label and match `filters`
    /// (`podman ps --filter`), running or not.
    fn matching(&self, filters: &[String]) -> Vec<String> {
        let mut args = vec![
            "ps".to_string(),
            "-aq".to_string(),
            "--filter".to_string(),
            format!("label={}", self.label()),
        ];
        for f in filters {
            args.push("--filter".to_string());
            args.push(f.clone());
        }
        let out = std::process::Command::new("podman")
            .args(&args)
            .output()
            .expect("podman ps runs");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    fn containers(&self) -> Vec<String> {
        self.matching(&[])
    }

    fn running(&self) -> Vec<String> {
        self.matching(&["status=running".to_string()])
    }

    /// Until no container carries the label; fails after 15 s.
    async fn until_gone(&self, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !self.containers().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: {:?} still there",
                self.containers()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `podman create` a container that carries the label and `labels`,
    /// and runs nothing: its id.
    fn create(&self, labels: &[String]) -> String {
        let mut args = vec!["create".to_string(), "--label".to_string(), self.label()];
        for l in labels {
            args.push("--label".to_string());
            args.push(l.clone());
        }
        args.extend([IMAGE.to_string(), "true".to_string()]);
        let out = std::process::Command::new("podman")
            .args(&args)
            .output()
            .expect("podman create runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Labelled {
    fn drop(&mut self) {
        for id in self.containers() {
            let _ = std::process::Command::new("podman")
                .args(["rm", "-f", "--time", "0", &id])
                .output();
        }
    }
}

/// How a test server runs: `command args` in [`IMAGE`], with `extra` run
/// flags after the test's label.
struct Spec<'a> {
    command: &'a str,
    args: &'a [&'a str],
    extra: &'a [&'a str],
    timeout_ms: u64,
    autostart: bool,
}

/// An isolated server as `spec` says, labelled with `label`: stored, and
/// connected by the reload after it when it is autostart, which returns
/// once the connect settled. Its id.
async fn server(state: &SharedState, label: &Labelled, spec: Spec<'_>) -> i64 {
    let mut extra_run_args = vec!["--label".to_string(), label.label()];
    extra_run_args.extend(spec.extra.iter().map(|a| a.to_string()));
    let id = store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "isolated".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: Some(spec.command.into()),
            args: spec.args.iter().map(|a| a.to_string()).collect(),
            env: vec![],
            cwd: None,
            container_image: Some(IMAGE.into()),
            extra_run_args,
            url: None,
            headers: vec![],
            tool_prefix: "iso".into(),
            timeout_ms: spec.timeout_ms,
            autostart: spec.autostart,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(60), state.reload_snapshot())
        .await
        .expect("the connect settles within its timeout_ms once the container is created")
        .unwrap();
    id
}

/// Server `id`'s badge: its status and detail.
async fn badge(state: &SharedState, id: i64) -> (&'static str, Option<String>) {
    let v = state
        .mcp
        .status_view(id, &state.snapshot())
        .await
        .expect("the server has an entry");
    (v.status, v.detail)
}

/// A server whose container starts and never answers ends `error` at its
/// `timeout_ms`, and the container goes.
#[tokio::test]
async fn an_isolated_server_that_never_answers_ends_error_and_its_container_goes() {
    let test = "an_isolated_server_that_never_answers";
    if !podman_ready(test) {
        return;
    }
    let state = gateway("silent").await;
    let label = Labelled::new("mcp-silent");
    let spec = Spec {
        command: "sleep",
        args: &["600"],
        extra: &[],
        timeout_ms: 2_000,
        autostart: true,
    };
    let id = server(&state, &label, spec).await;

    let (status, detail) = badge(&state, id).await;
    assert_eq!(status, "error", "{detail:?}");
    assert!(
        detail
            .as_deref()
            .is_some_and(|d| d.contains("no answer to the MCP handshake within 2000 ms")),
        "{detail:?}"
    );
    label.until_gone("the silent server's container").await;
}

/// The connect through a created container: the server is `ready` with its
/// tool, its container labelled with the server and this gateway's
/// instance, and once it is deleted its container ends with its stdin. Its
/// run flags hold the two `podman create` refuses and `podman start` takes
/// (R-1): the connect passes them to the start, and it works.
#[tokio::test]
async fn an_isolated_server_connects_through_its_created_container() {
    let test = "an_isolated_server_connects_through_its_created_container";
    if !podman_ready(test) {
        return;
    }
    let state = gateway("sh").await;
    let label = Labelled::new("mcp-sh");
    let spec = Spec {
        command: "sh",
        args: &["-c", SH_SERVER],
        extra: &["--sig-proxy=false", "--detach-keys", "ctrl-x"],
        timeout_ms: 30_000,
        autostart: true,
    };
    let id = server(&state, &label, spec).await;

    let (status, detail) = badge(&state, id).await;
    assert_eq!(status, "ready", "{detail:?}");
    let agg = state.mcp.aggregate(&state.snapshot()).await;
    assert!(agg.reverse.contains_key("iso__echo"), "{:?}", agg.reverse);
    assert_eq!(label.containers().len(), 1, "one container serves it");
    let prefix = state.snapshot().settings.container_prefix.clone();
    let labelled = label.matching(&[
        format!("label={LABEL_MCP}={id}"),
        format!("label={LABEL_INSTANCE}={prefix}"),
    ]);
    assert_eq!(labelled, label.containers(), "labelled with {id}, {prefix}");

    store::delete_mcp_server(&state.db, id).await.unwrap();
    state.reload_snapshot().await.unwrap();
    label.until_gone("the deleted server's container").await;
}

/// A server that ignores its stdin closing kept its container running after
/// a stop or a delete (R-2): the session's close ended only the attached
/// podman client. The container is stopped after it, with the server's
/// `timeout_ms` to end, and is gone by the time the stop returns.
#[tokio::test]
async fn a_server_that_ignores_its_stdin_closing_is_stopped_with_its_container() {
    let test = "a_server_that_ignores_its_stdin_closing";
    if !podman_ready(test) {
        return;
    }
    let state = gateway("stubborn").await;
    let label = Labelled::new("mcp-stubborn");
    let script = sh_server_ignoring_eof();
    let spec = Spec {
        command: "sh",
        args: &["-c", &script],
        extra: &[],
        timeout_ms: 4_000,
        autostart: true,
    };
    let id = server(&state, &label, spec).await;
    assert_eq!(badge(&state, id).await.0, "ready");
    assert_eq!(label.running().len(), 1);

    state.mcp.stop_server(id).await;
    assert_eq!(badge(&state, id).await.0, "stopped");
    label.until_gone("the stopped server's container").await;

    // A reload starts it again (enabled, autostart), and a delete stops it
    // the same way.
    state.reload_snapshot().await.unwrap();
    assert_eq!(badge(&state, id).await.0, "ready");
    assert_eq!(label.running().len(), 1);
    store::delete_mcp_server(&state.db, id).await.unwrap();
    state.reload_snapshot().await.unwrap();
    label.until_gone("the deleted server's container").await;
}

/// A "Test" whose request went away mid-handshake left its container behind
/// (R-3): the connect runs inline there, and the removal after a failed
/// handshake never came. The created container goes with the dropped
/// connect.
#[tokio::test]
async fn a_test_dropped_mid_handshake_takes_its_container_with_it() {
    let test = "a_test_dropped_mid_handshake";
    if !podman_ready(test) {
        return;
    }
    let state = gateway("dropped").await;
    let label = Labelled::new("mcp-dropped");
    let spec = Spec {
        command: "sleep",
        args: &["600"],
        extra: &[],
        timeout_ms: 60_000,
        autostart: false,
    };
    let id = server(&state, &label, spec).await;
    assert!(
        label.containers().is_empty(),
        "not autostart: not connected"
    );
    let row = state.snapshot().mcp_servers[&id].clone();

    // Boxed, so that the drop below drops the connect itself.
    let mut connect = Box::pin(state.mcp.test_connection(&row));
    // Until the container runs: created, and started for the handshake.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            ended = &mut connect => panic!("the test ended before it was dropped: {ended:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        if !label.running().is_empty() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no container ran");
    }
    drop(connect);
    label.until_gone("the dropped test's container").await;
}

/// The boot sweep removes the MCP server containers an earlier run of this
/// instance left (R-3), and nothing else: not another instance's on the
/// same podman, not one of this instance that is no MCP server's, and none
/// younger than the process.
#[tokio::test]
async fn the_boot_sweep_removes_this_instance_s_leftovers_only() {
    let test = "the_boot_sweep_removes_this_instance_s_leftovers_only";
    if !podman_ready(test) {
        return;
    }
    let state = gateway("sweep").await;
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(TokioRunner),
        reqwest::Client::new(),
    )));
    let prefix = state.snapshot().settings.container_prefix.clone();
    let label = Labelled::new("mcp-sweep");
    let leftover = label.create(&[
        format!("{LABEL_MCP}=7"),
        format!("{LABEL_INSTANCE}={prefix}"),
    ]);
    let other_instance = label.create(&[
        format!("{LABEL_MCP}=7"),
        format!("{LABEL_INSTANCE}={prefix}-other"),
    ]);
    let not_mcp = label.create(&[format!("{LABEL_INSTANCE}={prefix}")]);
    let ids = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let short = |id: &str| id[..12].to_string();

    let before = chrono::Utc::now() - chrono::Duration::hours(1);
    let swept = sweep_since(&state, before).await;
    assert!(swept.listed, "{swept:?}");
    assert!(
        swept.removed.is_empty(),
        "younger than the process: {swept:?}"
    );
    assert_eq!(label.containers().len(), 3);

    let after = chrono::Utc::now() + chrono::Duration::seconds(5);
    let swept = sweep_since(&state, after).await;
    assert!(swept.errors.is_empty(), "{swept:?}");
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    assert_eq!(
        ids(label.containers()),
        ids(vec![short(&other_instance), short(&not_mcp)]),
        "the leftover {} goes, the rest stays",
        short(&leftover)
    );
}

/// The run flags no connect can pass (R-1) are refused when the row is
/// saved, by the dashboard's op and the self-admin tool alike, and nothing
/// is stored: no row from a create, an unchanged row from an update.
#[tokio::test]
async fn a_run_flag_only_podman_run_takes_is_refused_at_save() {
    let state = AppState::init_for_tests().await.unwrap();
    for writer in [RowWriter::Dashboard, RowWriter::Tool] {
        let refused = ops::mcp_server_set(
            &state,
            McpServerPatch {
                action: "create".into(),
                name: Some("isolated".into()),
                transport: Some("stdio".into()),
                container_image: Some(IMAGE.into()),
                command: Some("sleep".into()),
                extra_run_args: Some("-v\n/a:/b:Z\n--rmi".into()),
                ..Default::default()
            },
            writer,
        )
        .await
        .expect_err("--rmi is refused");
        assert!(refused.contains("`--rmi`"), "{refused}");
        assert!(refused.contains("`podman create`"), "{refused}");
        assert!(store::list_mcp_servers(&state.db).await.unwrap().is_empty());
    }

    let created = ops::mcp_server_set(
        &state,
        McpServerPatch {
            action: "create".into(),
            name: Some("isolated".into()),
            transport: Some("stdio".into()),
            container_image: Some(IMAGE.into()),
            command: Some("sleep".into()),
            extra_run_args: Some("--sig-proxy=false".into()),
            autostart: Some(false),
            ..Default::default()
        },
        RowWriter::Dashboard,
    )
    .await
    .expect("a flag podman start takes is fine");
    let id = created["id"].as_i64().unwrap();
    for writer in [RowWriter::Dashboard, RowWriter::Tool] {
        let refused = ops::mcp_server_set(
            &state,
            McpServerPatch {
                action: "update".into(),
                id: Some(id),
                extra_run_args: Some("-d\n--preserve-fds=1".into()),
                timeout_ms: Some(1_234),
                ..Default::default()
            },
            writer,
        )
        .await
        .expect_err("-d is refused");
        assert!(
            refused.contains("`-d`") && refused.contains("`--preserve-fds`"),
            "{refused}"
        );
        let row = store::get_mcp_server(&state.db, id).await.unwrap().unwrap();
        assert_eq!(row.extra_run_args, ["--sig-proxy=false"]);
        assert_ne!(row.timeout_ms, 1_234, "nothing of the update stored");
    }
}
