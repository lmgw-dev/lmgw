//! What podman says is there: `ps`, `inspect`, the adoption compare and the
//! host PIDs (§3.4, candidate-aliases §4.7).

use std::collections::HashMap;

use serde::Deserialize;

use super::{is_no_such_container, Podman};
use crate::runtime::argv::{render_engine_args, run_args_digest, RenderSpec, RUN_ARGS_LABEL};
use crate::runtime::launcher::types::{Found, Listed, Listing, Presence};
use crate::runtime::registry::CmdOutput;

/// The subset of `podman inspect <name>` [`Podman::inspect_container`]
/// reads: the command, the labels, the published port and whether it runs.
#[derive(Debug, Deserialize)]
struct InspectRow {
    #[serde(default, rename = "Config")]
    config: InspectConfig,
    #[serde(default, rename = "NetworkSettings")]
    network_settings: InspectNetwork,
    #[serde(default, rename = "State")]
    state: Option<InspectState>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectConfig {
    /// The container command — everything after the image name in `podman
    /// run` (the image's own `ENTRYPOINT` is *not* part of it). For a managed
    /// container that is exactly [`render_engine_args`]'s output, which is
    /// what makes the two comparable at all.
    #[serde(default, rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(default, rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectNetwork {
    /// `{"8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": "39311"}]}`. The
    /// value is nullable for an unpublished port.
    #[serde(default, rename = "Ports")]
    ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

#[derive(Debug, Deserialize)]
struct PortBinding {
    #[serde(default, rename = "HostPort")]
    host_port: String,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(default, rename = "Running")]
    running: bool,
}

impl Podman {
    /// Does a container with exactly this name exist (in any state)?
    ///
    /// Three answers, not two: "podman says it is not there" and "podman could
    /// not answer" look identical to a `bool` and mean opposite things to the
    /// caller (the legacy sweep, §3.4).
    pub(super) async fn container_exists(&self, name: &str) -> Presence {
        match self.podman(&["inspect", "--format", "json", name]).await {
            Ok(out) if out.ok() => Presence::Present,
            Ok(out) if is_no_such_container(&out.stderr) => Presence::Absent,
            Ok(out) => Presence::Unknown(format!(
                "podman inspect {name} failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            )),
            Err(e) => Presence::Unknown(format!("podman inspect {name} could not be run: {e}")),
        }
    }

    /// `podman ps [-a] --format json --filter label=lmgw.instance=<prefix>`.
    pub(super) async fn ps(
        &self,
        container_prefix: &str,
        which: Listing,
    ) -> Result<Vec<Listed>, String> {
        let filter = format!("label=lmgw.instance={container_prefix}");
        let out = match which {
            Listing::All => {
                self.podman(&["ps", "-a", "--format", "json", "--filter", &filter])
                    .await
            }
            Listing::Running => {
                self.podman(&["ps", "--format", "json", "--filter", &filter])
                    .await
            }
        };
        decode_ps(out)
    }

    /// `podman inspect` one container, decoded down to what adoption reads.
    pub(super) async fn inspect_container(&self, name: &str) -> Result<Found, String> {
        let out = self
            .podman(&["inspect", "--format", "json", name])
            .await
            .map_err(|e| format!("podman inspect could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman inspect failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        let rows: Vec<InspectRow> = serde_json::from_str(&out.stdout)
            .map_err(|e| format!("podman inspect returned unreadable JSON: {e}"))?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| "podman inspect returned no container".to_string())?;
        let host_port = host_port(&row);
        Ok(Found {
            running: row.state.map(|s| s.running),
            labels: row.config.labels.unwrap_or_default(),
            cmd: row.config.cmd.unwrap_or_default(),
            host_port,
        })
    }

    /// Would `render` start exactly this container? The test is the
    /// container command (`Config.Cmd` against [`render_engine_args`]) and
    /// the run args through the label a start records them in
    /// ([`RUN_ARGS_LABEL`], a digest): podman keeps a label as written,
    /// where it normalizes the flags themselves. The image, the mounts, `-p`
    /// and the other labels are not compared — podman normalizes them, so a
    /// textual comparison would tear down healthy warm containers on every
    /// boot for a difference that is not one.
    pub(super) fn same_container(
        &self,
        container_prefix: &str,
        render: &RenderSpec,
        found: &Found,
        labels: &HashMap<String, String>,
    ) -> Result<(), String> {
        // With the options a start passes, so the restart policy it left out
        // is left out of this digest too.
        let mut render = render.clone();
        let globals = self.cdi().globals(container_prefix, &render.extra_run_args);
        render.set_globals(globals);
        let expected = render_engine_args(&render);
        if found.cmd != expected {
            return Err(format!(
                "its command is not the one this model renders now ({} vs {} args)",
                found.cmd.len(),
                expected.len()
            ));
        }
        match labels.get(RUN_ARGS_LABEL) {
            Some(got) if *got == run_args_digest(&render.extra_run_args) => Ok(()),
            Some(_) => Err("its run args are not the ones this model renders now".into()),
            None => Err(
                "it carries no record of its run args (started by an older lmgw), so they \
                 cannot be told to be the ones this model renders now"
                    .into(),
            ),
        }
    }

    /// The host PID of each named container's init process — `State.Pid` —
    /// in one `podman inspect` for all of them.
    ///
    /// Measured 2026-09-27 under rootless podman: for a chat container this
    /// is llama-server itself, and the PID NVML lists. A name podman does not
    /// know, or a container that is not running (`State.Pid` 0), is simply
    /// absent from the map — podman prints the containers it found and names
    /// the rest on stderr, exiting non-zero. `Err` only when podman could not
    /// be run, or failed without naming a single container.
    pub(super) async fn init_pids(&self, names: &[String]) -> Result<HashMap<String, u32>, String> {
        if names.is_empty() {
            return Ok(HashMap::new());
        }
        let mut argv: Vec<String> =
            vec!["inspect".into(), "--format".into(), HOST_PID_FORMAT.into()];
        argv.extend(names.iter().cloned());
        let out = self
            .podman_argv(&argv)
            .await
            .map_err(|e| format!("podman inspect could not be run: {e}"))?;
        let pids = parse_host_pids(&out.stdout);
        if pids.is_empty() && !out.ok() {
            return Err(format!(
                "podman inspect failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(pids)
    }
}

/// A `podman ps --format json` answer, read: an empty stdout is podman's
/// answer for "no containers match", not a malformed one. Shared with the
/// registry's own `ps` for the containers outside the launcher.
pub(crate) fn decode_ps(out: std::io::Result<CmdOutput>) -> Result<Vec<Listed>, String> {
    let out = out.map_err(|e| format!("podman ps could not be run: {e}"))?;
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
    serde_json::from_str(&out.stdout)
        .map_err(|e| format!("podman ps returned unreadable JSON: {e}"))
}

/// The `--format` of [`Podman::init_pids`]: one `<name> <pid>` line per
/// container. A space is a safe separator — podman refuses a container name
/// with one.
pub const HOST_PID_FORMAT: &str = "{{.Name}} {{.State.Pid}}";

/// [`HOST_PID_FORMAT`]'s output, as `name -> pid` for every running
/// container. A line that does not parse — or names PID 0, a container that
/// is not running — is left out.
fn parse_host_pids(stdout: &str) -> HashMap<String, u32> {
    stdout
        .lines()
        .filter_map(|l| {
            let (name, pid) = l.trim().rsplit_once(' ')?;
            let pid: u32 = pid.trim().parse().ok()?;
            (pid > 0 && !name.is_empty()).then(|| (name.trim().to_string(), pid))
        })
        .collect()
}

/// Recover the published host port from `podman inspect` (§3.4/§3.5): the
/// binding for the container's own `8080/tcp`, which is the only port a
/// managed container ever publishes ([`crate::runtime::argv::podman_run_argv`]
/// writes `-p <host>:8080`). Any other key is somebody else's container or a
/// hand-edited one, and is not guessed at.
fn host_port(row: &InspectRow) -> Option<u16> {
    row.network_settings
        .ports
        .as_ref()?
        .get("8080/tcp")?
        .as_ref()?
        .iter()
        .find_map(|b| b.host_port.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_host_pids;

    /// What podman printed for `inspect --format '{{.Name}} {{.State.Pid}}'`
    /// over a running container, an exited one and a name it did not know
    /// (measured 2026-09-27: found containers on stdout, the unknown one on
    /// stderr, exit 125).
    #[test]
    fn host_pids_keep_running_containers_only() {
        let pids = parse_host_pids("lmgw-chat-a-1a2b3c 1213087\nlmgw-aux-b-4d5e6f 0\n\ngarbage\n");
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert_eq!(pids["lmgw-chat-a-1a2b3c"], 1213087);
    }
}
