//! The bench container (benchmark design §3.4): how a run starts, watches
//! and removes the one container it measures — never a registry entry, so
//! none of the registry's machinery is in its way, and none of it sees the
//! container.
//!
//! **The seam.** A run reaches podman and a host port only through a
//! [`BenchLauncher`], the way the registry reaches podman only through its
//! [`CommandRunner`]: production's [`PodmanLauncher`] shells `podman` and asks
//! the kernel for a free loopback port; a test installs its own
//! ([`crate::bench::BenchState::set_launcher_for_tests`]) whose port is a fake
//! llama-server's and whose `podman run` only records the argv. Everything
//! else here — the argv, the readiness poll, the log tail, the removal — is
//! the same code in both.
//!
//! **Labels.** The container is rendered by the row's own
//! [`ModelRuntime`](crate::runtime::descriptor::ModelRuntime) through the same
//! [`podman_run_argv`] a model start uses, so it carries `lmgw.instance`,
//! `lmgw.class`, `lmgw.model` and `lmgw.engine` like the model's own
//! container would — plus `lmgw.bench=<run id>` ([`BENCH_LABEL`]), which is
//! what boot reconciliation skips it by and what the boot sweep collects it
//! by (§3.3), and a name of its own, `<prefix>-bench-<run id>`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::agent::Cancel;
use crate::runtime::argv::{podman_run_argv, RenderSpec};
use crate::runtime::registry::{ephemeral_port, log_excerpt_of, CmdOutput, CommandRunner};

/// The label that marks a bench container; its value is the run id.
pub const BENCH_LABEL: &str = "lmgw.bench";

/// How often the load phase probes `/health` — the registry's readiness
/// rate (`runtime::registry`'s `HEALTH_POLL`), a sampling rate, not a bound.
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// How often the load phase asks podman whether the container is still
/// running — the registry's `CONTAINER_EXIT_POLL`, for its reason: a model
/// that does not fit aborts the server in seconds, and the port alone cannot
/// tell that from a slow load.
const EXIT_POLL: Duration = Duration::from_secs(2);

/// How a run reaches podman and a host port (§9's launcher seam).
#[async_trait::async_trait]
pub trait BenchLauncher: Send + Sync {
    /// A free loopback port for the container's 8080.
    fn free_port(&self) -> std::io::Result<u16>;
    /// One `podman` invocation: `argv` without the program name.
    async fn podman(&self, argv: &[String]) -> std::io::Result<CmdOutput>;
}

/// The production launcher: `podman` through a [`CommandRunner`] (the
/// registry's own, so a test gateway without a runtime refuses here too),
/// and the kernel's next free port.
pub struct PodmanLauncher {
    runner: Arc<dyn CommandRunner>,
}

impl PodmanLauncher {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait::async_trait]
impl BenchLauncher for PodmanLauncher {
    fn free_port(&self) -> std::io::Result<u16> {
        ephemeral_port()
    }

    async fn podman(&self, argv: &[String]) -> std::io::Result<CmdOutput> {
        self.runner.run("podman", argv).await
    }
}

/// `<prefix>-bench-<run id>` (§3.4).
pub fn container_name(prefix: &str, run_id: i64) -> String {
    format!("{prefix}-bench-{run_id}")
}

/// The bench container's `podman run` argv: `spec` (already carrying the
/// bench name and port) through the model start's own renderer, plus the
/// [`BENCH_LABEL`] next to the other labels.
pub fn run_argv(spec: &RenderSpec, run_id: i64) -> Vec<String> {
    let mut argv = podman_run_argv(spec);
    let label = vec!["--label".to_string(), format!("{BENCH_LABEL}={run_id}")];
    let after = argv
        .iter()
        .position(|a| a.starts_with("lmgw.engine="))
        .map_or(argv.len().min(3), |i| i + 1);
    argv.splice(after..after, label);
    argv
}

/// `podman …` as one copyable, shell-quoted line (what the run stores).
pub fn command_line(argv: &[String]) -> String {
    std::iter::once("podman".to_string())
        .chain(argv.iter().cloned())
        .map(|tok| {
            shlex::try_quote(&tok)
                .map(|q| q.into_owned())
                .unwrap_or(tok)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why the load phase did not end healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    Canceled,
    /// The sentence, with the log excerpt and the classifier's hint when
    /// there is one.
    Failed(String),
}

/// One bench container, by name.
pub struct BenchContainer<'a> {
    pub launcher: &'a dyn BenchLauncher,
    pub name: String,
}

impl BenchContainer<'_> {
    /// `podman run -d …`: returns once podman has, with the container
    /// starting.
    pub async fn launch(&self, argv: &[String]) -> Result<(), String> {
        let out = self
            .launcher
            .podman(argv)
            .await
            .map_err(|e| format!("podman could not be run: {e}"))?;
        if out.ok() {
            return Ok(());
        }
        Err(format!(
            "podman run failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        ))
    }

    /// Poll `GET /health` until it answers 200, the container exits, `cancel`
    /// is raised, or `timeout` (`vram.load_timeout_seconds`, the registry's
    /// own bound on a load) is spent.
    pub async fn await_healthy(
        &self,
        http: &reqwest::Client,
        port: u16,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<(), LoadError> {
        let url = format!("http://127.0.0.1:{port}/health");
        let deadline = Instant::now() + timeout;
        let mut next_exit_check = Instant::now() + EXIT_POLL;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(LoadError::Failed(format!(
                    "no HTTP 200 from {url} within vram.load_timeout_seconds ({timeout:?})"
                )));
            }
            let probe = http.get(&url).timeout(left).send();
            match cancel.guard(probe).await {
                None => return Err(LoadError::Canceled),
                Some(Ok(resp)) if resp.status().as_u16() == 200 => return Ok(()),
                Some(_) => {}
            }
            if Instant::now() >= next_exit_check {
                next_exit_check = Instant::now() + EXIT_POLL;
                // A `podman inspect` can stall on podman's locks; the cancel
                // is what bounds it.
                match cancel.guard(self.exited()).await {
                    None => return Err(LoadError::Canceled),
                    Some(Some(dead)) => {
                        return Err(LoadError::Failed(format!(
                            "{dead} before {url} answered 200"
                        )))
                    }
                    Some(None) => {}
                }
            }
            let nap = HEALTH_POLL.min(deadline.saturating_duration_since(Instant::now()));
            if cancel.guard(tokio::time::sleep(nap)).await.is_none() {
                return Err(LoadError::Canceled);
            }
        }
    }

    /// `Some("the container exited with code N")` once podman says it is no
    /// longer running; anything podman cannot answer clearly is `None`
    /// ("keep waiting"), as in the registry.
    async fn exited(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(rename = "State")]
            state: Option<State>,
        }
        #[derive(Deserialize)]
        struct State {
            #[serde(rename = "Status", default)]
            status: String,
            #[serde(rename = "Running", default)]
            running: bool,
            #[serde(rename = "ExitCode", default)]
            exit_code: i64,
        }
        let argv = ["inspect", "--format", "json", &self.name].map(String::from);
        let out = self.launcher.podman(&argv).await.ok()?;
        if !out.ok() {
            return None;
        }
        let rows: Vec<Row> = serde_json::from_str(&out.stdout).ok()?;
        let state = rows.into_iter().next()?.state?;
        match state.status.as_str() {
            "exited" | "stopped" | "dead" if !state.running => Some(format!(
                "the container exited with code {} ({})",
                state.exit_code, state.status
            )),
            _ => None,
        }
    }

    /// The diagnosis lines of the container's log tail
    /// ([`log_excerpt_of`]), empty when podman cannot say.
    pub async fn log_excerpt(&self) -> Vec<String> {
        let argv = [
            "logs".to_string(),
            "--tail".to_string(),
            crate::runtime::registry::LOG_TAIL.to_string(),
            self.name.clone(),
        ];
        match self.launcher.podman(&argv).await {
            Ok(out) => log_excerpt_of(&format!("{}\n{}", out.stdout, out.stderr)),
            Err(_) => Vec::new(),
        }
    }

    /// A load that failed, in words: `why`, the classifier's hint for the
    /// log tail when it knows the cause (`modelinfo`'s, §2.4), and the tail.
    pub async fn load_failure(&self, why: &str) -> String {
        let lines = self.log_excerpt().await;
        let haystack = format!("{why}\n{}", lines.join("\n")).to_ascii_lowercase();
        let mut out = why.to_string();
        if let Some(hint) = crate::modelinfo::load_failure_hint(&haystack) {
            out.push_str(&format!(" — {hint}"));
        }
        if !lines.is_empty() {
            out.push_str(&format!("\ncontainer log:\n{}", lines.join("\n")));
        }
        out
    }

    /// Stop and remove it: `podman rm -f`, which kills at once — a bench
    /// container's work is the run's, which is over. "No such container" is
    /// the state asked for.
    pub async fn remove(&self) -> Result<(), String> {
        remove(self.launcher, &self.name).await
    }
}

/// `podman rm -f <name>`, "already gone" counting as done.
pub async fn remove(launcher: &dyn BenchLauncher, name: &str) -> Result<(), String> {
    let argv = ["rm", "-f", name].map(String::from);
    match launcher.podman(&argv).await {
        Ok(out) if out.ok() => Ok(()),
        Ok(out) if no_such_container(&out.stderr) => Ok(()),
        Ok(out) => Err(format!(
            "podman rm -f {name} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => Err(format!("podman rm -f {name} could not be run: {e}")),
    }
}

fn no_such_container(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    low.contains("no such container") || low.contains("no container with name")
}

/// Every bench container of this instance (`prefix`), running or not, that
/// podman created before `born` (unix seconds, when this process started) —
/// what the boot sweep removes (§3.3).
///
/// The labels are checked again on what podman lists, not only filtered on:
/// a container without [`BENCH_LABEL`] is a model's, and removing one of
/// those here would be the one mistake this sweep must never make.
///
/// A container created at or after `born` is this process's own, whatever
/// else is true — boot runs unawaited, and a run started meanwhile must not
/// lose its container to the sweep (agents' `reconcile_since` has the same
/// guard for the same race). `>=`, with podman's second granularity: one
/// created in the same second is kept, and collected at the next boot.
pub async fn leftovers(
    launcher: &dyn BenchLauncher,
    prefix: &str,
    born: i64,
) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "Names", default)]
        names: Vec<String>,
        #[serde(rename = "Labels", default)]
        labels: Option<std::collections::HashMap<String, String>>,
        /// Unix seconds (podman's `Created`, not `CreatedAt`).
        #[serde(rename = "Created", default)]
        created: i64,
    }
    let argv = [
        "ps".to_string(),
        "-a".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--filter".to_string(),
        format!("label=lmgw.instance={prefix}"),
        "--filter".to_string(),
        format!("label={BENCH_LABEL}"),
    ];
    let out = launcher
        .podman(&argv)
        .await
        .map_err(|e| format!("podman ps could not be run: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "podman ps failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        ));
    }
    if out.stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<Row> = serde_json::from_str(&out.stdout)
        .map_err(|e| format!("podman ps returned unreadable JSON: {e}"))?;
    Ok(rows
        .into_iter()
        .filter(|r| r.created < born)
        .filter(|r| {
            r.labels.as_ref().is_some_and(|l| {
                l.contains_key(BENCH_LABEL)
                    && l.get("lmgw.instance").map(String::as_str) == Some(prefix)
            })
        })
        .filter_map(|r| r.names.into_iter().next())
        .collect())
}

/// What `podman image inspect` says about the run's image (§2.2, §6
/// `build`): its ID and every label. `Err` when the image is not on this
/// machine (or podman cannot say) — the run refuses then, rather than let
/// `podman run` pull it inside the load phase.
pub async fn image_facts(
    launcher: &dyn BenchLauncher,
    image: &str,
) -> Result<(String, std::collections::BTreeMap<String, String>), String> {
    let argv = [
        "image",
        "inspect",
        "--format",
        "{{.Id}} {{json .Labels}}",
        image,
    ]
    .map(String::from);
    let out = launcher
        .podman(&argv)
        .await
        .map_err(|e| format!("podman image inspect could not be run: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "podman image inspect '{image}' failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_image_facts(&out.stdout)
        .ok_or_else(|| format!("podman image inspect '{image}' named no image ID"))
}

/// `{{.Id}} {{json .Labels}}`: the ID (without `sha256:`) and the labels
/// (`null` for an image without any).
fn parse_image_facts(stdout: &str) -> Option<(String, std::collections::BTreeMap<String, String>)> {
    let line = stdout.lines().next()?.trim();
    let (id, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let id = id.trim_start_matches("sha256:");
    if id.is_empty() {
        return None;
    }
    let labels =
        serde_json::from_str::<Option<std::collections::BTreeMap<String, String>>>(rest.trim())
            .ok()
            .flatten()
            .unwrap_or_default();
    Some((id.to_string(), labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_facts_parse_id_and_labels() {
        let (id, labels) = parse_image_facts(
            "sha256:abc123 {\"dev.lmgw.slug\":\"official-master\",\"org.opencontainers.image.version\":\"b11226\"}\n",
        )
        .unwrap();
        assert_eq!(id, "abc123");
        assert_eq!(labels["dev.lmgw.slug"], "official-master");
        let (_, none) = parse_image_facts("abc null").unwrap();
        assert!(none.is_empty());
        assert_eq!(parse_image_facts(""), None);
    }

    #[test]
    fn the_command_line_is_shell_quoted() {
        let argv = vec!["run".to_string(), "a b".to_string()];
        assert_eq!(command_line(&argv), "podman run 'a b'");
        assert_eq!(container_name("lmgw", 12), "lmgw-bench-12");
    }
}
