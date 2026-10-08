//! A Podman-isolated stdio server's container (MCP gateway design §9). The
//! connect creates it with `podman create`, before the handshake's clock
//! starts, and starts it attached to its stdio. It goes when its session is
//! closed, when its handshake fails, and when whatever held it lets go of
//! it without either: a connect dropped by its caller, a panic. Each one is
//! labelled with its server and this gateway's instance, and the boot sweep
//! removes the ones a crash left behind.

use std::process::Stdio;

use rmcp::service::RunningService;
use rmcp::RoleClient;

use crate::agents::container::LABEL_INSTANCE;
use crate::config::McpServer;
use crate::state::SharedState;

use super::handler::GatewayClientHandler;

/// `lmgw.mcp=<server id>`: the label every MCP server's container carries,
/// beside `lmgw.instance=<container_prefix>` (the agents' label), which
/// tells this gateway's containers from those of a dev or production
/// instance on the same podman.
pub const LABEL_MCP: &str = "lmgw.mcp";

/// A container [`create`](Self::create) made, removed when this is dropped
/// unless it was [`stop`](Self::stop)ped or [`discard`](Self::discard)ed
/// first. A connect that succeeds hands it to its session, which stops it
/// when it closes.
pub(super) struct Container {
    id: String,
    /// What a stop gives the server to end on its own before it is killed:
    /// its `timeout_ms`, in podman's whole seconds, rounded up.
    grace_secs: u64,
    disarmed: bool,
}

impl Container {
    /// `podman create` `server`'s container, labelled for `instance` (the
    /// `container_prefix`): the image pull, when its run flags ask for one,
    /// and the container. Podman's own words when it fails.
    pub(super) async fn create(server: &McpServer, instance: &str) -> Result<Self, String> {
        let labels = [
            (LABEL_INSTANCE.to_string(), instance.to_string()),
            (LABEL_MCP.to_string(), server.id.to_string()),
        ];
        let argv = server
            .container_create_argv(&labels)
            .ok_or("stdio server has no container image")?;
        let out = tokio::process::Command::new("podman")
            .args(&argv)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("spawning `podman create`: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let id = stdout.lines().map(str::trim).rfind(|l| !l.is_empty());
        match id {
            Some(id) if out.status.success() => Ok(Self {
                id: id.to_string(),
                grace_secs: server.timeout_ms.div_ceil(1000),
                disarmed: false,
            }),
            _ => Err(format!(
                "podman create failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    /// Stop and remove the container of a session that was closed. Closing
    /// it closed the server's stdin, and rmcp killed the attached client
    /// when the server did not end; a server that does not end on its stdin
    /// closing runs on in its container all the same. It gets its grace to
    /// end, then is killed.
    pub(super) async fn stop(mut self) {
        self.disarmed = true;
        remove(&self.id, self.grace_secs, "its session closed").await;
    }

    /// Remove the container of a handshake that failed or ran out of time,
    /// at once.
    pub(super) async fn discard(mut self) {
        self.disarmed = true;
        remove(&self.id, 0, "its handshake failed").await;
    }
}

impl Drop for Container {
    /// Let go of without a stop or a discard: by a connect its caller
    /// dropped (a "Test" whose request went away), a panic, or an entry
    /// that went with its session in it. A drop cannot wait, so `podman rm`
    /// is spawned here, at once, and reaped on a thread of its own: it runs
    /// whatever the runtime is doing, at a shutdown too.
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let spawned = std::process::Command::new("podman")
            .args(["rm", "--force", "--ignore", "--time", "0", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => tracing::warn!(
                "mcp: removing container {} its connect let go of: {e}",
                self.id
            ),
        }
    }
}

/// `podman rm --force --time <grace_secs>`: a running container gets its
/// stop signal, `grace_secs` to end and then a kill; one gone already (it
/// is created `--rm`) is no error.
async fn remove(id: &str, grace_secs: u64, after: &str) {
    let out = tokio::process::Command::new("podman")
        .args(["rm", "--force", "--ignore", "--time"])
        .arg(grace_secs.to_string())
        .arg(id)
        .stdin(Stdio::null())
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::debug!(
            "mcp: removing container {id} after {after}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("mcp: removing container {id} after {after}: {e}"),
    }
}

/// Close a session: the transport goes, then the container. Cancelling the
/// session closes the server's stdin and ends the attached client (rmcp);
/// a container whose server ignored that is stopped
/// ([`Container::stop`]).
pub(super) async fn close(
    running: Option<RunningService<RoleClient, GatewayClientHandler>>,
    container: Option<Container>,
) {
    if let Some(running) = running {
        let _ = running.cancel().await;
    }
    if let Some(container) = container {
        container.stop().await;
    }
}

/// What one sweep did. `listed = false` means podman could not answer,
/// which is a different thing from "nothing was left over".
#[derive(Debug, Default, PartialEq)]
pub struct Swept {
    pub listed: bool,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

/// The boot pass: [`sweep_since`] from this process's start, logged.
///
/// Spawned by `server::spawn_background_tasks` beside the agents' boot
/// reconcile, and for its reasons: it lists and removes by
/// `container_prefix`, which a fresh dev data dir only has once the entry
/// point's dev-instance step ran, and `podman ps` is unbounded wall clock.
pub async fn boot_sweep(state: &SharedState) {
    let report = sweep_since(state, state.started_at_utc).await;
    for e in &report.errors {
        tracing::warn!("MCP container sweep: {e}");
    }
    if !report.removed.is_empty() {
        tracing::info!(
            "MCP container sweep: removed {} container(s) an earlier run left behind",
            report.removed.len()
        );
    }
}

/// Remove every MCP server container of this instance (`lmgw.mcp`, and
/// `lmgw.instance` its `container_prefix`) older than `born`. Another
/// instance's are not looked at: a dev and a production gateway share one
/// podman. One younger than `born` is this process's own: the boot
/// reconcile connects servers while this runs (the agents' sweep reads
/// `Created` the same way, container-runtime §6.4). `born` is a parameter
/// for the test that needs a leftover older than the process.
pub async fn sweep_since(state: &SharedState, born: chrono::DateTime<chrono::Utc>) -> Swept {
    let mut report = Swept::default();
    let prefix = state.snapshot().settings.container_prefix.clone();
    let registry = state.runtime();
    let filters = vec![
        format!("label={LABEL_MCP}"),
        format!("label={LABEL_INSTANCE}={prefix}"),
    ];
    let rows = match registry.ps_filtered(&filters).await {
        Ok(rows) => rows,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    report.listed = true;
    for row in rows {
        // `>=`: podman counts whole seconds, and one created in the second
        // this process started is kept, for the next boot to collect.
        if row.created >= born.timestamp() {
            continue;
        }
        match registry.rm_force(&row.name).await {
            Ok(()) => {
                tracing::info!(container = %row.name, "removed a leftover MCP server container");
                report.removed.push(row.name);
            }
            Err(e) => report.errors.push(e),
        }
    }
    report
}
