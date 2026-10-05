//! `run.kind = "container"` — the runner (container-runtime design §6).
//!
//! Beside [`batch`](super::batch) and driven by the same `AgentRunExecutor`:
//! **one job kind, two run shapes**, so every run reaches the same surface. A
//! container's rows land in the same [`RunBuffer`](super::batch::RunBuffer),
//! render through the same review table, and end as the same job `result`.
//!
//! What lmgw owns here and what the image owns is the whole design (§2,
//! principle 0). lmgw owns identity (the agent token, minted on demand at
//! start), money (`X-Lmgw-Run` folds into [`RunMeters`](super::RunMeters)),
//! logs (stderr verbatim, non-JSON stdout verbatim), secrets (a `0600` file on
//! a host tmpfs, bind-mounted `ro,Z`) and the shell (`podman run`, the cgroup
//! limits, the cancel sequence). The image owns what the agent actually does.
//!
//! **Transport A — JSONL on stdout** (§3.2): one JSON object per line, fed to
//! [`ledger::decode_body`] — the *same* decoder the HTTP ledger uses, so a
//! container can start on the pipe and grow into service mode without changing
//! how it reports, and an event cannot mean one thing on a pipe and another in
//! a POST. A stdout line that is not JSON is a run-log line verbatim: a library
//! that prints a banner must not be able to kill a run.
//!
//! **`script` sugar rides the same runner** (§4.2): a manifest step that
//! declares a `script` becomes a [`Plan`] over `Settings.agent_script_image`
//! with the embedded shim and the module written into the run directory and
//! mounted read-only, and goes through [`run_plan`] like everything else. That is the point of the sugar being
//! sugar — the limits, the run directory, the token, cancel, the deadline and
//! the ledger are one implementation, not two.
//!
//! **Nothing here invents a bound.** Every limit is a field of
//! [`manifest::Limits`] with a printed default, and `0` means "no limit" —
//! the flag is left off the argv entirely rather than replaced by a number
//! lmgw chose. The one exception is `stop_grace_seconds`, where `0` is
//! *stricter* (SIGKILL at once), and it is labelled as such everywhere.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::jobs::{JobCtx, JobOutcome};
use crate::runtime::registry::CmdOutput;
use crate::runtime::slug;
use crate::state::SharedState;

use super::batch::{Input, Phase, Row};
use super::ledger;
use super::manifest::{self, Access, Limits, PullPolicy, RunSpec};
use super::mounts;
use super::{token, Agent};

/// How often the stream loop looks at the cancel flag.
///
/// Cancellation is an `AtomicBool` with no waker ([`JobCtx`]), so the loop has
/// to poll it. Not a bound on anything the owner can observe: the deadline is
/// honoured on its own timer and the container's output is never delayed by it.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// How many stderr lines a failure message carries — the excerpt shape
/// `Registry::log_excerpt` produces, and §3.2's "plus the last 12 stderr
/// lines". Only the *message* is trimmed: every stderr line is in the run log
/// in full, so nothing is actually dropped.
pub const STDERR_EXCERPT_LINES: usize = 12;

// ---------------------------------------------------------------------------
// Labels and names (§6.1)
// ---------------------------------------------------------------------------

/// `lmgw.kind=agent` — what boot reconciliation filters on.
pub const LABEL_KIND: &str = "lmgw.kind";
pub const KIND_AGENT: &str = "agent";
pub const LABEL_INSTANCE: &str = "lmgw.instance";
pub const LABEL_AGENT: &str = "lmgw.agent";
pub const LABEL_RUN: &str = "lmgw.run";

/// `<prefix>-agent-<slug(id)>-<run>` (§6.1).
///
/// The prefix is `settings.container_prefix`, the one the model containers
/// already use — which is what keeps a dev instance from colliding with the
/// real one, a collision this project has paid for once. The run id is what
/// makes the name unique per run, so `--replace` can only ever collect a
/// leftover of the *same* run.
pub fn container_name(prefix: &str, agent_id: &str, run: i64) -> String {
    format!("{prefix}-agent-{}-{run}", slug(agent_id))
}

/// `<prefix>-agentsvc-<slug(id)>` — the service container's name (§6.5).
///
/// One name per agent, not per start: a service is the agent's single instance
/// (§12's "one map entry per agent"), and a stable name is what lets
/// `--replace` collect the corpse of a previous start and what boot
/// reconciliation removes by label without having to remember a run id that
/// never existed.
pub fn service_container_name(prefix: &str, agent_id: &str) -> String {
    format!("{prefix}-agentsvc-{}", slug(agent_id))
}

/// The `lmgw.run` label a service container carries (§6.5).
///
/// Deliberately not a number: boot reconciliation collects every agent
/// container whose `lmgw.run` is not a **live job row**, and a service has no
/// job at all. `service` fails the `parse::<i64>()` there, which is exactly the
/// "collect it" answer — a service container that outlived the lmgw that
/// started it is a leftover like any other.
pub const RUN_LABEL_SERVICE: &str = "service";

// ---------------------------------------------------------------------------
// The spawn seam (§6.1)
// ---------------------------------------------------------------------------

/// A started child process, as three streams.
///
/// [`registry::CommandRunner`](crate::runtime::registry::CommandRunner) buffers
/// to completion (`TokioRunner` calls `output()`), so it cannot deliver a live
/// row — hence a second, smaller seam rather than bending the first, which
/// would force every existing fake to grow streaming.
///
/// The two line channels are **bounded**: a container that outruns the reader
/// blocks on its own `write`, which is backpressure, not truncation. Nothing is
/// dropped and nothing is capped — the process simply waits, exactly as it
/// would against a pipe.
pub struct Spawned {
    pub stdout: mpsc::Receiver<String>,
    pub stderr: mpsc::Receiver<String>,
    /// Resolves once the child has ended. **Dropping this future kills the
    /// child**: [`TokioSpawner`] sets `kill_on_drop`, which is the last rung of
    /// the stop ladder (§6.3) and the reason a wedged container cannot outlive
    /// its run.
    pub status: BoxFuture<'static, std::io::Result<Exit>>,
    /// Sends a signal to the child while it runs — what cancelling a
    /// `podman build` needs (container-builds §5): SIGTERM first, so podman
    /// can stop its build container, and SIGKILL after a grace
    /// ([`terminate`]). The agent runner does not use it; it stops the
    /// container it named (`podman stop`) and keeps `kill_on_drop` as the last
    /// rung.
    pub kill: KillHandle,
}

/// A signal lmgw sends to a spawned child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM — "stop, cleaning up what you started".
    Term,
    /// SIGKILL — the child gets no say.
    Kill,
}

impl Signal {
    /// The signal number the kernel knows it by.
    pub fn raw(self) -> i32 {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

/// The sending end of a spawned child's signal line.
///
/// A channel into [`Spawned::status`] rather than a pid: the status future
/// owns the child, and it delivers a signal only while `wait` has not
/// returned — so the pid is still this child's (a zombie at worst) and can
/// never have been recycled into someone else's process. The cost is that a
/// signal lands the next time `status` is polled, which is what a caller
/// waiting for the child to end is doing anyway.
#[derive(Debug, Clone)]
pub struct KillHandle(mpsc::UnboundedSender<Signal>);

impl KillHandle {
    /// A handle and the receiving end a spawner (real or fake) drains.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Signal>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    /// A handle to nothing: every [`Self::send`] reports the child gone. For a
    /// fake child with no process to signal.
    pub fn detached() -> Self {
        Self::channel().0
    }

    /// Queue `sig` for the child. `false` when it has already ended (its
    /// status has resolved or been dropped), so there is nothing to signal.
    pub fn send(&self, sig: Signal) -> bool {
        self.0.send(sig).is_ok()
    }
}

/// Stop a spawned child the way a build cancel does (container-builds §5):
/// SIGTERM, up to `grace` for it to go, then SIGKILL, then wait for it.
/// Returns how it ended. A child that has already ended simply reports that.
pub async fn terminate(
    kill: &KillHandle,
    status: &mut BoxFuture<'static, std::io::Result<Exit>>,
    grace: Duration,
) -> std::io::Result<Exit> {
    kill.send(Signal::Term);
    if let Ok(ended) = tokio::time::timeout(grace, &mut *status).await {
        return ended;
    }
    kill.send(Signal::Kill);
    status.await
}

/// How a child ended. `podman run` killed by a signal is not "status -1": a
/// signal is a different fact and the run log says which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    /// The sentence a failure message uses.
    pub fn describe(self) -> String {
        match self {
            Self::Code(n) => format!("the container exited with status {n}"),
            Self::Signal(n) => format!("podman run was killed by signal {n}"),
        }
    }
}

/// How the agent runtime reaches `podman`: one streaming verb and one
/// buffered one.
///
/// `run` is here rather than borrowed from the registry because the runner has
/// to stop **the exact container it started, with the manifest's grace**
/// (`podman stop -t <stop_grace_seconds>`), and the registry's stop verbs are
/// keyed by model class. Everything non-streaming that *does* fit the registry
/// — `rm -f`, `ps`, the log tail — is taken from there.
#[async_trait]
pub trait Spawner: Send + Sync {
    async fn spawn(&self, program: &str, args: &[String]) -> std::io::Result<Spawned>;
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput>;

    /// [`Self::spawn`] with `env` added to the child's environment — how a
    /// container build gets its own `TMPDIR` (container-builds §14.2). A
    /// spawner that has no environment to set (a test's fake) ignores it,
    /// which is this default.
    async fn spawn_env(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> std::io::Result<Spawned> {
        let _ = env;
        self.spawn(program, args).await
    }
}

/// The real spawner: `podman` as a child process.
pub struct TokioSpawner;

/// Enough room that an ordinary burst of rows never makes the container wait,
/// small enough that a runaway one cannot grow lmgw's heap. Backpressure, not
/// a cap: see [`Spawned`].
const LINE_BUFFER: usize = 256;

#[async_trait]
impl Spawner for TokioSpawner {
    async fn spawn(&self, program: &str, args: &[String]) -> std::io::Result<Spawned> {
        self.spawn_env(program, args, &[]).await
    }

    async fn spawn_env(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> std::io::Result<Spawned> {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let out = child.stdout.take().expect("stdout was piped");
        let err = child.stderr.take().expect("stderr was piped");
        let (out_tx, stdout) = mpsc::channel(LINE_BUFFER);
        let (err_tx, stderr) = mpsc::channel(LINE_BUFFER);
        tokio::spawn(pump(out, out_tx));
        tokio::spawn(pump(err, err_tx));
        let (kill, mut signals) = KillHandle::channel();
        Ok(Spawned {
            stdout,
            stderr,
            kill,
            status: Box::pin(async move {
                let waited = loop {
                    tokio::select! {
                        // `wait` is cancel-safe, so losing a race to a signal
                        // loses nothing.
                        s = child.wait() => break s,
                        Some(sig) = signals.recv() => {
                            // `wait` has not returned, so the child is not
                            // reaped and its pid is still its own. `None` only
                            // once tokio has reaped it — then there is nothing
                            // left to signal, and the next pass reports how it
                            // ended.
                            if let Some(pid) = child.id() {
                                send_signal(pid, sig);
                            }
                        }
                    }
                };
                waited.map(|s| {
                    use std::os::unix::process::ExitStatusExt;
                    match (s.code(), s.signal()) {
                        (Some(c), _) => Exit::Code(c),
                        (None, Some(sig)) => Exit::Signal(sig),
                        // Neither: not a shape Unix produces, but a `-1` that
                        // claimed to be an exit status would be a lie.
                        (None, None) => Exit::Signal(0),
                    }
                })
            }),
        })
    }

    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CmdOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// `kill(2)` one pid. A failure is logged, not returned: the only one that
/// can happen to a child we still own is ESRCH (it exited between the check
/// and the call), and the status future reports that exit anyway.
fn send_signal(pid: u32, sig: Signal) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: `kill` takes two integers and touches no memory of ours; `pid`
    // is a positive child pid we have not reaped (see the caller).
    if unsafe { libc::kill(pid, sig.raw()) } != 0 {
        tracing::debug!(
            "signal {} to pid {pid}: {}",
            sig.raw(),
            std::io::Error::last_os_error()
        );
    }
}

/// Read one pipe line by line until it closes. No line-length bound: a row
/// event is as long as its columns are.
async fn pump<R>(reader: R, tx: mpsc::Sender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(line).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// The script shim (§4.2)
// ---------------------------------------------------------------------------

/// The shim, embedded: the entrypoint of every `script` step.
///
/// `include_str!` rather than a file beside the binary, for the reason the
/// built-in manifests are embedded too — an install is one file, and the shim
/// cannot drift from the build that speaks its protocol.
///
/// It is written **into the run's own directory** beside `script.mjs`, not into
/// a shared content-addressed cache. That was the first design and it is wrong
/// under SELinux: `:Z` relabels the *source* with a private MCS category, so a
/// second script run starting while a first is still going revokes the first
/// container's read access to the shared file (measured on this box — EACCES on
/// the earlier container, mid-run). A per-run copy is a few kilobytes on a
/// tmpfs that is deleted with the run, and it cannot be relabelled out from
/// under anyone.
pub const SHIM: &str = include_str!("../../assets/agent-shim.mjs");

/// Where the shim and the manifest's script are mounted inside the container.
pub const SHIM_INSIDE: &str = "/lmgw/shim.mjs";
pub const SCRIPT_INSIDE: &str = "/lmgw/script.mjs";

/// A spawner that refuses every verb, with the reason it was installed.
///
/// The agent-runtime sibling of
/// [`NoRuntime`](crate::runtime::registry::NoRuntime), installed in
/// [`AppState::init_for_tests`](crate::state::AppState::init_for_tests) for the
/// same reason: a test that reaches the container runner without meaning to
/// must say so rather than start something on the machine the suite runs on.
pub struct NoSpawner(pub String);

#[async_trait]
impl Spawner for NoSpawner {
    async fn spawn(&self, _program: &str, _args: &[String]) -> std::io::Result<Spawned> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            self.0.clone(),
        ))
    }
    async fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            self.0.clone(),
        ))
    }
}

// ---------------------------------------------------------------------------
// The argv (§6.1)
// ---------------------------------------------------------------------------

/// Which SELinux relabel podman is asked for on a bind mount source (§5.5).
///
/// Two words rather than a `bool` because the difference is not "more or less"
/// but *who else may read it afterwards*, and the wrong one is silent until a
/// second container starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// `:Z` — a private MCS category, for a file this run wrote and owns
    /// alone.
    Private,
    /// `:z` — `container_file_t` with no category, for a source two containers
    /// may hold at once.
    Shared,
}

impl Label {
    /// The suffix podman spells it with.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "Z",
            Self::Shared => "z",
        }
    }
}

/// One `-v` on the argv: the host side, where it lands, and the two words that
/// decide the flag (§5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub inside: String,
    pub access: Access,
    pub label: Label,
}

impl Mount {
    /// A file lmgw wrote into this run's own directory: read-only, and `:Z`
    /// because nobody else will ever hold it — the per-run copy of the shim
    /// exists precisely so that stays true (see [`SHIM`]).
    pub fn lmgw_file(host: PathBuf, inside: impl Into<String>) -> Self {
        Self {
            host,
            inside: inside.into(),
            access: Access::Ro,
            label: Label::Private,
        }
    }

    /// A slot the owner bound, with the access the manifest declared (§5.5).
    ///
    /// **`:z`, shared.** The measurement is the one recorded on [`SHIM`]: `:Z`
    /// relabels the *source* with a private category, so the second container
    /// to start against it revokes the first one's access mid-run. An
    /// owner-chosen folder is exactly where two containers meet — a service
    /// and that agent's phase run, or two agents on one folder — so it is
    /// relabelled `container_file_t` with no category instead, readable by
    /// every container and still by the owner.
    pub fn bound(binding: &mounts::Binding) -> Self {
        Self {
            host: binding.host.clone(),
            inside: binding.field.inside(),
            access: binding.field.access,
            label: Label::Shared,
        }
    }

    /// `<host>:<inside>:<ro|rw>,<Z|z>` — the argument podman is handed.
    pub fn flag(&self) -> String {
        format!(
            "{}:{}:{},{}",
            self.host.display(),
            self.inside,
            self.access.as_str(),
            self.label.as_str()
        )
    }
}

/// Everything the `podman run` argv is rendered from. A struct rather than
/// nine arguments so the renderer stays a pure function with a test that reads
/// it token by token, the way `tests/it/runtime_argv.rs` asserts the model one.
pub struct RunSpecArgs<'a> {
    pub name: &'a str,
    pub prefix: &'a str,
    pub agent_id: &'a str,
    /// The `lmgw.run` label's value: a job id for a phase run, and
    /// [`RUN_LABEL_SERVICE`] for a service container, which has no job at all.
    /// A string rather than an `i64` so the one label that is not a number does
    /// not have to be patched into the rendered argv afterwards.
    pub run: &'a str,
    pub image: &'a str,
    pub pull: PullPolicy,
    pub entrypoint: Option<&'a str>,
    pub args: &'a [String],
    pub limits: &'a Limits,
    /// `(name, value)` in the order §3.1's table lists them. Never a secret:
    /// `podman inspect` prints a container's environment, so the token travels
    /// in the secrets file instead.
    pub env: &'a [(String, String)],
    /// Every bind mount, in the order they reach the argv: lmgw's own two
    /// files first, then whatever the owner bound (§5.5).
    pub mounts: &'a [Mount],
    /// `--userns=keep-id`, because this manifest **declares** a mount field —
    /// bound or not, so one image always runs one way (§5.5). What the
    /// container writes is then the owner's, rather than landing under a
    /// subuid nobody can read back.
    pub keep_id: bool,
    /// What [`gateway_access`] needs on the argv for `LMGW_BASE_URL` to be
    /// reachable — empty unless the gateway bound loopback.
    pub network: &'a [String],
    /// Service mode (§6.5): detached, and published on a host port lmgw picked.
    /// `None` for a phase run, which is foreground and publishes nothing.
    pub service: Option<Published>,
}

/// The one port a service container exposes, and where the host reaches it
/// (§3.3).
///
/// Published on **loopback only** (`-p 127.0.0.1:<host>:<container>`): the
/// proxy is the only client, and a container bound to every interface would put
/// an agent's UI on the LAN without anyone asking for it. The host side is
/// picked per start by [`registry::ephemeral_port`](crate::runtime::registry::ephemeral_port)
/// because the container IP is not reachable here — measured on this box, the
/// same pasta fact [`gateway_access`] documents in the other direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    pub host: u16,
    pub container: u32,
}

/// The `podman run` argv for one phase (§6.1).
///
/// A `0` limit omits its flag entirely; it never becomes a number lmgw chose.
pub fn run_argv(spec: &RunSpecArgs<'_>) -> Vec<String> {
    let mut a: Vec<String> = vec!["run".into()];
    match spec.service {
        // A phase run is foreground and gone the moment it ends: `--rm` is what
        // keeps a finished run from leaving a row in `podman ps -a`.
        None => a.push("--rm".into()),
        // A service container is **not** `--rm`: `podman logs` is the only
        // account of a container that died during its health probe, and `--rm`
        // would delete the evidence before the 503 could quote it (§3.3). The
        // stop ladder removes it explicitly instead.
        Some(_) => a.push("-d".into()),
    }
    a.push("--replace".into());
    a.push("--name".into());
    a.push(spec.name.into());
    if let Some(p) = spec.service {
        a.push("-p".into());
        a.push(format!("127.0.0.1:{}:{}", p.host, p.container));
    }
    for (k, v) in [
        (LABEL_INSTANCE, spec.prefix.to_string()),
        (LABEL_KIND, KIND_AGENT.to_string()),
        (LABEL_AGENT, spec.agent_id.to_string()),
        (LABEL_RUN, spec.run.to_string()),
    ] {
        a.push("--label".into());
        a.push(format!("{k}={v}"));
    }
    // After the labels and before the limits, as §5.5 prints it. A property of
    // the manifest rather than of the form: an image that declares a slot runs
    // as the owner's uid whether today's config filled the slot or not.
    if spec.keep_id {
        a.push("--userns=keep-id".into());
    }
    if spec.limits.memory_mb > 0 {
        a.push("--memory".into());
        a.push(format!("{}m", spec.limits.memory_mb));
    }
    if spec.limits.cpus > 0.0 {
        a.push("--cpus".into());
        a.push(format!("{}", spec.limits.cpus));
    }
    if spec.limits.pids > 0 {
        a.push("--pids-limit".into());
        a.push(spec.limits.pids.to_string());
    }
    if spec.limits.read_only {
        a.push("--read-only".into());
        // No size: tmpfs pages are charged to the container's memory cgroup,
        // so `memory_mb` already bounds it (§4.1).
        a.push("--tmpfs".into());
        a.push("/tmp".into());
    }
    // Not configurable and always applied (§4.1).
    a.push("--cap-drop=ALL".into());
    a.push("--security-opt".into());
    a.push("no-new-privileges".into());
    // Printed, never implicit (§3.4): the owner can see which pull policy a
    // Start is running under by reading the argv in the logs.
    a.push(format!("--pull={}", spec.pull.as_str()));
    a.extend(spec.network.iter().cloned());
    for (k, v) in spec.env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    for m in spec.mounts {
        a.push("-v".into());
        a.push(m.flag());
    }
    if let Some(e) = spec.entrypoint {
        a.push("--entrypoint".into());
        a.push(e.into());
    }
    a.push(spec.image.into());
    a.extend(spec.args.iter().cloned());
    a
}

// ---------------------------------------------------------------------------
// The run directory (§6.2)
// ---------------------------------------------------------------------------

/// The per-run directory holding `input.json` and `secrets.json`.
///
/// On a **host tmpfs** (`$XDG_RUNTIME_DIR/lmgw/`) so the token never touches a
/// disk, mode `0700`, and `rm -rf`'d when the run ends — including on every
/// error path, which is what the `Drop` is for. If `XDG_RUNTIME_DIR` is unset
/// the fallback is `<data_dir>/agents/`, and that raises the
/// `secrets_dir_fallback` warning (§4.3): "your tokens are being written to
/// persistent storage" is not a `tracing::warn!`-grade fact.
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
}

/// Where per-run directories live, and whether that is the tmpfs the design
/// wants. `(root, is_fallback)`.
///
/// Under `container_prefix`, for the reason
/// [`runtime::container_name`](crate::runtime::container_name) is: `$XDG_RUNTIME_DIR`
/// is shared by every lmgw on the box, and job ids are per-database, so a dev
/// instance and the real one would otherwise fight over `run-7` — the exact
/// collision class the prefix exists to remove, and one this project has paid
/// for once.
pub fn runs_root(data_dir: &Path, prefix: &str) -> (PathBuf, bool) {
    let leaf = slug(prefix);
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(x) if !x.is_empty() => (PathBuf::from(x).join("lmgw").join(leaf), false),
        _ => (data_dir.join("agents").join(leaf), true),
    }
}

/// One directory level at `0700`, treating "it is already there" as done.
///
/// `DirBuilder::mode` sets the mode at creation rather than after it, so there
/// is no window in which the directory exists with the umask's permissions.
fn mkdir_0700(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("creating {}: {e}", path.display())),
    }
}

impl RunDir {
    /// `<root>/run-<run>`, with **every level this call creates** made `0700`.
    ///
    /// The root is the caller's, resolved through [`runs_root`] — an explicit
    /// parameter rather than derived here, so a test can hand over a tempdir
    /// instead of silently writing into the real `$XDG_RUNTIME_DIR`.
    pub fn create(root: &Path, run: i64) -> Result<Self, String> {
        let path = root.join(format!("run-{run}"));
        // A leftover of the same run id (a crash mid-run) would otherwise be
        // mounted into the new one.
        let _ = std::fs::remove_dir_all(&path);
        // `create_dir_all` applies the umask to every level it makes, so an
        // intermediate directory could end up world-readable while only the
        // leaf was tightened. Each level is created at 0700 instead.
        let mut walk = PathBuf::new();
        for part in path.components() {
            walk.push(part);
            mkdir_0700(&walk)?;
        }
        Ok(Self { path })
    }

    /// A directory named for something that is not a run — service mode's
    /// `service-<slug(id)>` (§6.5), which lives as long as the container does
    /// rather than as long as a job does.
    ///
    /// Same `0700` walk and the same `Drop`: the entry that owns the container
    /// owns this, so stopping the service takes its `secrets.json` with it.
    pub fn create_named(root: &Path, leaf: &str) -> Result<Self, String> {
        let path = root.join(leaf);
        let _ = std::fs::remove_dir_all(&path);
        let mut walk = PathBuf::new();
        for part in path.components() {
            walk.push(part);
            mkdir_0700(&walk)?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One `0600` file in the run directory.
    pub fn write(&self, name: &str, body: &str) -> Result<PathBuf, String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.path.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        Ok(path)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("removing the run directory {}: {e}", self.path.display());
            }
        }
        // And the per-instance root when this was the last run in it. Not
        // recursive and not reported: `remove_dir` simply fails while another
        // run of this instance still has a directory there, which is the
        // correct answer.
        if let Some(root) = self.path.parent() {
            let _ = std::fs::remove_dir(root);
        }
    }
}

// ---------------------------------------------------------------------------
// Input and secrets (§6.2)
// ---------------------------------------------------------------------------

/// `input.json`: `{ phase, agent, run, config, mounts, rows }`, with `config`
/// the effective config **without** secrets. `rows` is present for `apply`
/// only — absent, not empty, so a container can tell "no gate" from "an empty
/// gate".
pub fn input_document(agent: &Agent, phase: Phase, run: i64, rows: &[Row]) -> Value {
    let mut doc = json!({
        "phase": phase.as_str(),
        "agent": { "id": agent.row.id, "name": agent.manifest.name },
        "run": { "id": run },
        "config": public_config(agent),
        "mounts": mounts_document(agent),
    });
    if phase == Phase::Apply {
        let for_apply: Vec<Value> = rows.iter().map(Row::for_apply).collect();
        doc["rows"] = Value::Array(for_apply);
    }
    doc
}

/// The effective config with every `secret` field left out and every mount
/// field's value replaced by its container path — what `input.json` carries,
/// for a phase run and for a service alike (§6.2, mounts §5.6).
///
/// A secret reaches the container through `secrets.json` and nowhere else, so
/// the two documents cannot drift into disagreeing about which is which. A
/// host path reaches it through nothing at all: the container is handed
/// `/lmgw/mounts/<field>`, which is where the folder actually is from inside
/// (mounts principle 3).
pub fn public_config(agent: &Agent) -> Value {
    let fields = agent.manifest.fields().unwrap_or_default();
    let public: Vec<manifest::Field> = fields.iter().filter(|f| !f.is_secret()).cloned().collect();
    let values = manifest::effective_values(&public, &agent.config_values());
    mounts::as_container_paths(&agent.manifest, values)
}

/// `input.json`'s `mounts` array: one entry per **bound** slot, so a container
/// can enumerate what it was given without reading its own schema (§5.6).
///
/// `[]` when the manifest declares no mount field, and an unbound optional
/// field is absent from here exactly as it is from `config`. No host path:
/// `path` is the container's own, the same string `config.<field>` carries.
pub fn mounts_document(agent: &Agent) -> Value {
    let bound = mounts::bound_fields(&agent.manifest, &agent.config_values());
    Value::Array(
        bound
            .iter()
            .map(|f| {
                json!({
                    "field": f.name,
                    "path": f.inside(),
                    "kind": f.kind.as_str(),
                    "access": f.access.as_str(),
                })
            })
            .collect(),
    )
}

/// `secrets.json`: `{ token, config }`. The only place the agent token exists
/// outside `api_keys.key_plain` and `agent_token_get`'s response body (§3.1).
///
/// `with_config = false` for a **script** run (§4.2/§6.2): a script is the
/// deterministic half of an agent and has no business holding a credential; a
/// step that needs one is a container, not a script. The token still travels,
/// because it is what `ctx.tools.call` authenticates with — and it never
/// appears on `ctx`.
pub fn secrets_document(agent: &Agent, token: &str, with_config: bool) -> Value {
    let fields = agent.manifest.fields().unwrap_or_default();
    let values = agent.config_values();
    let mut secrets = Map::new();
    if with_config {
        for f in fields.iter().filter(|f| f.is_secret()) {
            match values.get(&f.name) {
                Some(Value::Null) | None => {}
                Some(v) => {
                    secrets.insert(f.name.clone(), v.clone());
                }
            }
        }
    }
    json!({ "token": token, "config": Value::Object(secrets) })
}

/// The `LMGW_*` environment, in §3.1's order.
///
/// Addressing only — never a secret: `podman inspect` prints a container's
/// environment and so does `podman ps --format`.
///
/// `run = None` is **service mode** (§3.3): there is no run, so there is no
/// ledger URL to post to, no run id to stamp and no deadline — a service
/// container is bounded by its idle window, not by a run's clock. The three
/// keys are left out entirely rather than set to a zero a container would have
/// to know to disbelieve.
pub fn env_for(
    agent_id: &str,
    base_url: &str,
    phase: &str,
    run: Option<i64>,
    deadline_seconds: u64,
) -> Vec<(String, String)> {
    let e = |k: &str, v: String| (k.to_string(), v);
    let mut env = vec![
        e("LMGW_BASE_URL", base_url.to_string()),
        e("LMGW_API_BASE", format!("{base_url}/v1")),
        e("LMGW_MCP_URL", format!("{base_url}/mcp")),
    ];
    if let Some(run) = run {
        env.push(e(
            "LMGW_LEDGER_URL",
            format!("{base_url}/api/agents/runs/{run}/events"),
        ));
    }
    env.push(e("LMGW_AGENT", agent_id.to_string()));
    if let Some(run) = run {
        env.push(e("LMGW_RUN", run.to_string()));
    }
    env.push(e("LMGW_PHASE", phase.to_string()));
    env.push(e("LMGW_INPUT", "/lmgw/input.json".to_string()));
    env.push(e("LMGW_SECRETS", "/lmgw/secrets.json".to_string()));
    if run.is_some() {
        env.push(e("LMGW_DEADLINE_SECONDS", deadline_seconds.to_string()));
    }
    env
}

/// How a container reaches the gateway: the base URL, and the `podman run`
/// flags that make it true (§3.1).
///
/// The port is `settings.bind_addr`'s, never hardcoded. Which *host* works
/// depends on what the gateway bound, and the two cases are not the same
/// network (measured on podman 5.8.4, rootless, pasta):
///
/// - **`0.0.0.0` / a real address**: the gateway is on an interface the
///   container's `host.containers.internal` (pasta's `169.254.1.2`) can reach.
///   No extra flag.
/// - **`127.0.0.1` / `::1` / `localhost`** — lmgw's own default, and the shape
///   every desktop install runs: `host.containers.internal` gets *connection
///   refused*, because a loopback-bound socket is not on that address at all.
///   pasta forwards it on request: `--network=pasta:-T,<port>` makes
///   `127.0.0.1:<port>` **inside** the container reach `127.0.0.1:<port>` on
///   the host, which is exactly the one port the run needs and nothing else.
///   WP2 assumed the default network already did this; it does not, and a
///   script's very first `tools.call` is what found out.
///
/// Two things `-T` also does, both named in the run log by [`loopback_note`]
/// rather than left to be discovered: it **occupies that port on the
/// container's own loopback**, so an image that wanted to listen there cannot;
/// and it forwards to the host's **IPv4** loopback only, so a gateway bound
/// `[::1]` and nothing else is unreachable from a container however the URL is
/// written.
///
/// The flag is per-run and names the port, so nothing beyond the gateway is
/// exposed to the container, and the argv says which case it is in.
pub fn gateway_access(bind_addr: &str) -> Result<(String, Vec<String>), String> {
    let (host, port) = bind_addr.rsplit_once(':').ok_or_else(|| {
        format!(
            "the bind_addr setting is '{bind_addr}', which names no port, so a container cannot \
             be told where the gateway is"
        )
    })?;
    let port: u16 = port.trim().parse().map_err(|_| {
        format!(
            "the bind_addr setting is '{bind_addr}', which names no port, so a container cannot \
             be told where the gateway is"
        )
    })?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        Ok((
            format!("http://127.0.0.1:{port}"),
            vec!["--network".to_string(), format!("pasta:-T,{port}")],
        ))
    } else {
        Ok((
            format!("http://host.containers.internal:{port}"),
            Vec::new(),
        ))
    }
}

/// Just the URL half of [`gateway_access`].
pub fn base_url(bind_addr: &str) -> Result<String, String> {
    gateway_access(bind_addr).map(|(url, _)| url)
}

/// The one thing about [`gateway_access`]' loopback case a run has to be told:
/// `-T` forwards to the host's **IPv4** loopback, so a gateway bound to `::1`
/// alone is not there. `None` when there is nothing to say.
///
/// A run-log line rather than a refusal: the bind may well also be answering on
/// `127.0.0.1` through a second listener, and lmgw cannot know that from the
/// setting alone. What it can do is say which assumption the run is making.
pub fn loopback_note(bind_addr: &str) -> Option<String> {
    let (host, port) = bind_addr.rsplit_once(':')?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let v6 = matches!(
        host.parse::<std::net::IpAddr>(),
        Ok(std::net::IpAddr::V6(ip)) if ip.is_loopback()
    );
    v6.then(|| {
        format!(
            "bind_addr is '{bind_addr}' — an IPv6 loopback. The container reaches the gateway \
             through pasta's -T,{port}, which forwards to the host's IPv4 loopback only, so this \
             run fails unless the gateway also answers on 127.0.0.1:{port}. Bind 127.0.0.1 or \
             0.0.0.0 if it does not."
        )
    })
}

/// The uid `--userns=keep-id` maps the container's process to: this one's own,
/// read from the kernel rather than assumed (§5.5).
///
/// `/proc/self` is owned by the process reading it, so the number in the run
/// log is the number podman will actually use. `None` is a `/proc` that did not
/// answer, and the line says so in words instead of printing a uid lmgw made
/// up.
pub fn process_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// The start line's `keep-id` half (§5.5), for a manifest that declares a slot.
pub fn keep_id_note() -> String {
    match process_uid() {
        Some(uid) => format!(
            "this manifest declares mounts: the container runs as uid {uid} (--userns=keep-id)"
        ),
        None => "this manifest declares mounts: the container runs as this gateway's own uid, \
                 which /proc/self did not answer for (--userns=keep-id)"
            .to_string(),
    }
}

/// One run-log line per bound mount, host to container, with what binding did
/// to the folder — §5.6, and principle 4: nothing about a mount is implicit.
///
/// The relabel is the half an owner cannot see from anywhere else: it is
/// recursive, it is permanent, and nothing undoes it when the agent stops.
pub fn mount_note(b: &mounts::Binding) -> String {
    format!(
        "mount {}: {} → {} ({}, {}; relabelled container_file_t, recursive, permanent)",
        b.field.name,
        b.host.display(),
        b.field.inside(),
        b.field.access.as_str(),
        b.field.kind.as_str(),
    )
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// How the container's invocation ended, before it is turned into a
/// [`JobOutcome`].
#[derive(Debug, PartialEq)]
enum Ending {
    Exited(Exit),
    /// Cancel was pressed; the exit code that followed the stop is not the
    /// story.
    Cancelled,
    /// The deadline ran out and the stop sequence ran. `failed`, not
    /// `canceled`, because nobody asked for it to stop.
    Deadline(u64),
    /// Podman itself could not be run, or the child could not be waited on.
    NoProcess(String),
}

/// One phase's invocation, resolved: what to start and what to mount.
///
/// The seam between `run.kind = "container"` and `script` sugar (§4.2). Both
/// go through [`run_plan`], which is the whole point of the sugar being sugar:
/// the limits, the run directory, the token, the JSONL decoder, cancel, the
/// deadline and the ledger are one implementation, not two.
pub struct Plan {
    pub image: String,
    pub pull: PullPolicy,
    pub entrypoint: Option<String>,
    pub args: Vec<String>,
    /// Files lmgw writes into the run directory and mounts `ro,Z` beyond
    /// `input.json` and `secrets.json`: `(file name, contents, path inside)`.
    pub files: Vec<(String, String, String)>,
    /// Host paths that already exist, beyond the owner's bound slots — which
    /// [`run_plan`] adds itself, from the config, for every plan alike.
    /// Nothing uses this today: every file a run needs is written per-run
    /// instead, because `:Z` rewrites the source's MCS label and a shared file
    /// mounted twice locks the first container out. Kept for a mount that is
    /// genuinely immutable and shared.
    pub mounts: Vec<Mount>,
    /// The schema this phase's `output` is checked against at close.
    pub output_schema: Option<Value>,
    /// Whether `secrets.json` carries the manifest's `secret` config fields.
    pub secret_config: bool,
    /// Extra `LMGW_*` entries beyond the addressing set. Never a secret.
    pub env: Vec<(String, String)>,
}

/// Drive one phase of a container agent.
///
/// Called from [`batch::execute`](super::batch::execute) once the row is
/// loaded, the manifest validated and the agent found enabled, so both run
/// shapes share that preamble.
pub async fn execute(ctx: &JobCtx, input: &Input, agent: Agent) -> Result<JobOutcome, String> {
    let RunSpec::Container {
        image,
        pull,
        entrypoint,
        args,
        phases,
        ..
    } = &agent.manifest.run
    else {
        return Err(format!(
            "'{}' is a {} agent, not a container one",
            agent.row.id,
            agent.manifest.kind()
        ));
    };
    let phase = input.phase;
    match phase {
        Phase::Run | Phase::Apply => {}
        other => {
            return Err(format!(
                "'{}' is a container agent: its phases are {}, and '{}' belongs to the batch \
                 pipeline",
                agent.row.id,
                phases.join(", "),
                other.as_str()
            ))
        }
    }
    if !phases.iter().any(|p| p == phase.as_str()) {
        return Err(format!(
            "this agent's image declares the phases {}, so it has no '{}' phase",
            phases.join(", "),
            phase.as_str()
        ));
    }
    if phase == Phase::Apply && input.rows.is_empty() {
        return Err("nothing to apply: no rows were checked".to_string());
    }
    let Some(image) = image.as_deref().filter(|i| !i.trim().is_empty()) else {
        return Err(format!(
            "'{}' declares no run.image, so there is nothing to start. A container agent needs \
             an image for its run and apply phases even when it also serves a UI.",
            agent.row.id
        ));
    };
    let plan = Plan {
        image: image.to_string(),
        pull: *pull,
        entrypoint: entrypoint.clone(),
        args: args.clone(),
        files: Vec::new(),
        mounts: Vec::new(),
        output_schema: agent.manifest.output_schema(phase.as_str()).cloned(),
        secret_config: true,
        env: Vec::new(),
    };
    run_plan(ctx, input, agent, plan).await
}

/// Drive one `script` step as a container run of the script image (§4.2).
///
/// Not a second runtime: the only differences from an image agent are the
/// image (`Settings.agent_script_image`), the entrypoint (`node
/// /lmgw/shim.mjs`), two more files in the run directory, and a `secrets.json`
/// with no config half. Everything else is [`run_plan`], unchanged.
pub async fn execute_script(
    ctx: &JobCtx,
    input: &Input,
    agent: Agent,
    script: &manifest::Script,
    output: Option<&Value>,
) -> Result<JobOutcome, String> {
    let image = ctx
        .state
        .snapshot()
        .settings
        .agent_script_image
        .trim()
        .to_string();
    if image.is_empty() {
        return Err(
            "the agent script image (Settings → Agents & tools) is empty, so there is no image \
             for this script step to run in"
                .to_string(),
        );
    }
    let plan = Plan {
        image,
        // The one implicit fetch in the system, and a deliberate one: the
        // script image is lmgw's own choice of runtime rather than an image the
        // owner built, so a first script run downloading a stock Node once is
        // the setup step nobody should have to be told about. It is visible —
        // `--pull=missing` is in the argv and the run log names the image.
        pull: PullPolicy::Missing,
        entrypoint: Some("node".to_string()),
        args: vec![SHIM_INSIDE.to_string()],
        files: vec![
            // Per-run, `0600`, on the same tmpfs as the secrets and gone with
            // them: a shared host file mounted `:Z` would have its MCS label
            // rewritten by the next run that mounts it.
            (
                "shim.mjs".to_string(),
                SHIM.to_string(),
                SHIM_INSIDE.to_string(),
            ),
            (
                "script.mjs".to_string(),
                script.text(),
                SCRIPT_INSIDE.to_string(),
            ),
        ],
        mounts: Vec::new(),
        output_schema: output.cloned(),
        secret_config: false,
        env: vec![("LMGW_SCRIPT".to_string(), SCRIPT_INSIDE.to_string())],
    };
    run_plan(ctx, input, agent, plan).await
}

/// Everything both run shapes share: the run directory, the token, the argv,
/// the stream loop and the outcome.
async fn run_plan(
    ctx: &JobCtx,
    input: &Input,
    agent: Agent,
    plan: Plan,
) -> Result<JobOutcome, String> {
    let phase = input.phase;
    let state = &ctx.state;
    let snap = state.snapshot();
    let limits = agent.manifest.limits();
    let (base, network) = gateway_access(&snap.settings.bind_addr)?;
    let prefix = snap.settings.container_prefix.clone();
    let name = container_name(&prefix, &agent.row.id, ctx.id);

    // The path rules again, at use time (mounts §5.3), against the config this
    // run is actually going to use — `execute` has already merged the form over
    // the stored values, so `agent` is that config. Before the token is minted
    // and before the run directory exists: a folder that was renamed since it
    // was saved fails the run with `mount_path_missing` and podman is never
    // reached.
    let bound = match agent.manifest.declares_mounts() {
        false => Vec::new(),
        true => {
            let mctx = mounts::ctx(state).await;
            mounts::check_values(
                &agent.row.id,
                &agent.manifest,
                &agent.config_values(),
                &mctx,
                mounts::Moment::Use,
            )
            .map_err(|r| format!("{} ({})", r.message, r.code))?
        }
    };

    // The token is minted here if this is the first time (§3.1): an agent that
    // is never run and never copied never mints a credential. The scope is
    // recomputed on the way through, so what the container is handed is scoped
    // to the config as it is now.
    let token = token::ensure(state, &agent).await?;

    let (root, secrets_on_disk) = runs_root(&state.data_dir, &prefix);
    let dir = RunDir::create(&root, ctx.id)?;
    let input_path = dir.write(
        "input.json",
        &serde_json::to_string_pretty(&input_document(&agent, phase, ctx.id, &input.rows))
            .map_err(|e| e.to_string())?,
    )?;
    let secrets_path = dir.write(
        "secrets.json",
        &serde_json::to_string(&secrets_document(&agent, &token, plan.secret_config))
            .map_err(|e| e.to_string())?,
    )?;
    let mut mounts = vec![
        Mount::lmgw_file(input_path, "/lmgw/input.json"),
        Mount::lmgw_file(secrets_path, "/lmgw/secrets.json"),
    ];
    for (name, body, inside) in &plan.files {
        mounts.push(Mount::lmgw_file(dir.write(name, body)?, inside.clone()));
    }
    mounts.extend(plan.mounts.iter().cloned());
    // Last, and `:z` rather than `:Z`: the owner's folders (§5.5).
    mounts.extend(bound.iter().map(Mount::bound));

    let run = ledger::Run::new(
        agent.row.id.clone(),
        phase,
        super::batch::review_columns(&agent.manifest),
        limits.deadline_seconds,
        // §3.1's escape hatch, closed on lmgw's side: a container that prints
        // its own `/lmgw/secrets.json` would otherwise write its live bearer
        // into the run log, the job's stored `result` and the Run tab.
        Some(token.clone()),
    );
    run.seed_rows(&input.rows);
    if secrets_on_disk {
        note(
            ctx,
            &run,
            format!(
                "XDG_RUNTIME_DIR is unset, so this run's secrets were written to {} instead of a \
             host tmpfs (secrets_dir_fallback)",
                dir.path().display()
            ),
        );
    }
    if let Some(line) = loopback_note(&snap.settings.bind_addr) {
        note(ctx, &run, line);
    }
    // Before the start line, one line per bound mount (§5.6): what was bound,
    // where it lands, and that the folder has been relabelled for good.
    for b in &bound {
        note(ctx, &run, mount_note(b));
    }
    let keep_id = agent.manifest.declares_mounts();
    let start = match limits.deadline_seconds {
        0 => format!(
            "starting {} as {name} (pull {}); no deadline (run.limits.deadline_seconds = 0)",
            plan.image,
            plan.pull.as_str()
        ),
        n => format!(
            "starting {} as {name} (pull {}); deadline in {n}s from the container's start",
            plan.image,
            plan.pull.as_str()
        ),
    };
    // The uid rides on the start line rather than on one of its own, because it
    // is a fact about *this* start: an image that needs to be root at runtime
    // gets `EACCES` from the kernel, and the reason has to be beside the
    // container it applies to (§5.5).
    note(
        ctx,
        &run,
        match keep_id {
            false => start,
            true => format!("{start}; {}", keep_id_note()),
        },
    );
    state.agent_runs.put(ctx.id, &run.rows());

    let mut env = env_for(
        &agent.row.id,
        &base,
        phase.as_str(),
        Some(ctx.id),
        limits.deadline_seconds,
    );
    env.extend(plan.env.iter().cloned());
    let argv = run_argv(&RunSpecArgs {
        name: &name,
        prefix: &prefix,
        agent_id: &agent.row.id,
        run: &ctx.id.to_string(),
        image: &plan.image,
        pull: plan.pull,
        entrypoint: plan.entrypoint.as_deref(),
        args: &plan.args,
        limits: &limits,
        env: &env,
        mounts: &mounts,
        keep_id,
        network: &network,
        service: None,
    });

    let spawner = state.agent_spawner();
    let spawned = match spawner.spawn("podman", &argv).await {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "podman is required for container agents and could not be run: {e}"
            ))
        }
    };

    let mut stderr_tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let ending = stream(
        ctx,
        &run,
        spawned,
        spawner.clone(),
        &name,
        &limits,
        &mut stderr_tail,
    )
    .await;

    // The directory goes now rather than at the end of the function: the run is
    // over, and a secrets file outliving the process that needed it is the one
    // thing §6.2 exists to prevent.
    drop(dir);

    finish(
        ctx,
        &agent,
        input,
        &run,
        ending,
        &stderr_tail,
        plan.output_schema.as_ref(),
    )
    .await
}

/// Why the stop ladder was started. The **first** cause wins the run's ending:
/// a cancel that arrives while the deadline sequence is already running is
/// logged, not promoted — the run was already over, and saying it was
/// cancelled would hide why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    Cancel,
    Deadline(u64),
}

/// One run-log line, live.
///
/// Both halves, because they answer different readers: [`ledger::Run`] is what
/// ends up in the job's `result`, and the buffer is what the Run tab shows
/// while the run is still going (§3.2's "the run log itself is
/// `AgentRunDetail.log`").
fn note(ctx: &JobCtx, run: &ledger::Run, line: String) {
    // Redacted once, then pushed to both sinks: the live buffer the Run tab
    // polls and the run's own log are two copies of the same line, and a token
    // taken out of one and left in the other is a token that leaked.
    let line = run.redact(line);
    ctx.state.agent_runs.push_log(ctx.id, &line);
    run.note(line);
}

/// Read the container's two pipes until they close, wait for the process, and
/// bound both by the cancel flag and the deadline.
///
/// One loop rather than two phases: a container that closes its own stdout and
/// then hangs must be as bounded as one that keeps talking, and a stop that
/// does not bite must not wedge the run either. Every exit from here is
/// reached in finite time — the last rung of [`stop_ladder`] is dropping
/// `status`, and `TokioSpawner` sets `kill_on_drop`.
async fn stream(
    ctx: &JobCtx,
    run: &ledger::Run,
    spawned: Spawned,
    spawner: Arc<dyn Spawner>,
    name: &str,
    limits: &Limits,
    stderr_tail: &mut std::collections::VecDeque<String>,
) -> Ending {
    // `kill` goes unused on purpose: this runner stops the container it named
    // (`stop_ladder`), and the process-level last rung is `kill_on_drop`.
    let Spawned {
        mut stdout,
        mut stderr,
        mut status,
        kill: _,
    } = spawned;
    let mut out_done = false;
    let mut err_done = false;
    let mut exited: Option<std::io::Result<Exit>> = None;
    let mut cause: Option<StopCause> = None;
    let mut cancel_seen = false;
    let mut poll = tokio::time::interval(CANCEL_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `0` is unbounded, and the branch is disabled outright below rather than
    // given a far-future instant it would otherwise be compared against.
    let deadline = tokio::time::sleep(Duration::from_secs(limits.deadline_seconds.max(1)));
    tokio::pin!(deadline);
    let bounded = limits.deadline_seconds > 0;
    // Created only when a stop is triggered; `OptionFuture` is `Ready(None)`
    // until then, which `select!`'s pattern simply disables for that pass.
    let mut ladder: futures::future::OptionFuture<BoxFuture<'_, ()>> = None.into();
    let mut gave_up = false;

    loop {
        if out_done && err_done && exited.is_some() {
            break;
        }
        tokio::select! {
            line = stdout.recv(), if !out_done => match line {
                Some(l) => on_stdout(ctx, run, &l).await,
                None => out_done = true,
            },
            line = stderr.recv(), if !err_done => match line {
                Some(l) => {
                    // Verbatim into the run log (§3.2). A container that prints
                    // its own secrets file leaks its token into its own run log
                    // — named in the spec as the one escape hatch, and true.
                    // The row table is deliberately *not* republished here: a
                    // stderr line changes no row, and re-cloning the table per
                    // line is the wrong cost for a chatty container.
                    if stderr_tail.len() == STDERR_EXCERPT_LINES {
                        stderr_tail.pop_front();
                    }
                    stderr_tail.push_back(l.clone());
                    note(ctx, run, l);
                    // The frame still has to move, though: `log_lines` is how
                    // the Run tab learns there is something new to fetch, and a
                    // container whose only output is stderr would otherwise
                    // show an empty tab until it exited (final review).
                    // `note_progress` throttles this to the same two frames a
                    // second every other reporter gets.
                    ctx.progress_detail(ledger::detail_of(run)).await;
                }
                None => err_done = true,
            },
            r = &mut status, if exited.is_none() => exited = Some(r),
            _ = &mut deadline, if bounded && cause.is_none() => {
                cause = Some(StopCause::Deadline(limits.deadline_seconds));
                note(ctx, run, format!(
                    "the run exceeded its deadline of {}s; stopping the container",
                    limits.deadline_seconds
                ));
                ladder = Some(Box::pin(stop_ladder(
                    ctx, spawner.clone(), name, limits.stop_grace_seconds, run,
                )) as BoxFuture<'_, ()>).into();
            }
            // Ungated by the stop sequence on purpose: a cancel pressed while
            // the deadline's ladder is climbing must still be seen and said.
            _ = poll.tick(), if !cancel_seen => {
                if ctx.canceled() {
                    cancel_seen = true;
                    match cause {
                        None => {
                            cause = Some(StopCause::Cancel);
                            note(ctx, run, format!(
                                "cancel requested; podman stop -t {} (SIGTERM, grace, SIGKILL)",
                                limits.stop_grace_seconds
                            ));
                            ladder = Some(Box::pin(stop_ladder(
                                ctx, spawner.clone(), name, limits.stop_grace_seconds, run,
                            )) as BoxFuture<'_, ()>).into();
                        }
                        Some(_) => note(ctx, run, format!(
                            "cancel requested while the container was already being stopped \
                             ({}); the run keeps the reason it already had",
                            match cause {
                                Some(StopCause::Deadline(n)) => format!("deadline of {n}s"),
                                _ => "cancel".to_string(),
                            }
                        )),
                    }
                }
            }
            Some(()) = &mut ladder => {
                // Every rung has been climbed and the child is still there.
                gave_up = true;
                break;
            }
        }
    }

    if gave_up {
        note(
            ctx,
            run,
            "the container did not go after stop and rm -f; killing the podman run process and \
             ending the run with the rows it produced"
                .to_string(),
        );
    }
    // Dropping `status` kills the child (`kill_on_drop`), which is the point.
    drop(status);

    match (cause, exited) {
        (Some(StopCause::Cancel), _) => Ending::Cancelled,
        (Some(StopCause::Deadline(n)), _) => Ending::Deadline(n),
        (None, Some(Ok(e))) => Ending::Exited(e),
        (None, Some(Err(e))) => Ending::NoProcess(format!(
            "lmgw could not wait for the container process: {e}"
        )),
        // Unreachable: the loop only leaves without a cause once `status` has
        // resolved. Reported rather than unwrapped, because a panic in a job is
        // a run with no explanation at all.
        (None, None) => Ending::NoProcess(
            "the container process ended without a status lmgw could read".to_string(),
        ),
    }
}

/// Stop the container, and keep escalating until something works (§6.3,
/// hardened after the WP2 review).
///
/// `podman stop` is not guaranteed to bite: the container may not exist yet
/// because `podman run` is still pulling, the name may lose a sub-second race
/// with its own creation, or podman may simply fail. Each rung therefore has a
/// louder successor, each writes one run-log line naming what it did, and the
/// whole ladder is bounded by the **visible** `stop_grace_seconds` — no new
/// constant. `0` there means every rung fires at once, which is exactly what
/// "SIGKILL with no chance to flush" already meant.
///
/// The caller stops polling this the moment the child exits, so a healthy stop
/// never reaches rung two.
async fn stop_ladder(
    ctx: &JobCtx,
    spawner: Arc<dyn Spawner>,
    name: &str,
    grace: u64,
    run: &ledger::Run,
) {
    let note = |line: String| note(ctx, run, line);
    let settle = || tokio::time::sleep(Duration::from_secs(grace));

    // Rung 1: podman's own SIGTERM → grace → SIGKILL sequence.
    let argv = vec![
        "stop".to_string(),
        "-t".to_string(),
        grace.to_string(),
        name.to_string(),
    ];
    match spawner.run("podman", &argv).await {
        Ok(out) if out.ok() => note(format!(
            "podman stop -t {grace} {name} returned; waiting {grace}s for it to exit"
        )),
        Ok(out) => note(format!(
            "podman stop -t {grace} {name} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => note(format!("podman stop {name} could not be run: {e}")),
    }
    settle().await;

    // Rung 2: remove it outright. The registry's verb, which already treats
    // "already gone" as done.
    match ctx.state.runtime().rm_force(name).await {
        Ok(()) => note(format!(
            "the container was still there after stop; podman rm -f {name} ran"
        )),
        // `rm_force` already phrases itself as a sentence naming the container.
        Err(e) => note(e),
    }
    settle().await;
    // Rung 3 is the caller's: it drops `status`, and `kill_on_drop` does the
    // rest.
}

/// Stop and remove a **detached service container** (§6.5).
///
/// The WP2 ladder's two meaningful rungs, both on the **agent** seam. Rung 3
/// (dropping the child so `kill_on_drop` fires) does not exist here — a
/// detached container has no child of ours — and neither does the settle
/// between the rungs, because `podman stop` returns only once the container is
/// down; the run path's sleeps would be `2 x grace` of dead time on every idle
/// stop. `rm -f` is the second rung rather than a third call after
/// `Registry::rm_force`: a service container is *not* `--rm`, so a clean stop
/// leaves the stopped object behind, and doing it through the registry's runner
/// would mean a test that fakes this seam also has to fake that one.
/// "Already gone" is the requested state, so a non-zero exit is a log line and
/// not an error.
pub async fn stop_service_container(
    spawner: &Arc<dyn Spawner>,
    name: &str,
    grace: u64,
    note: &(dyn Fn(String) + Send + Sync),
) {
    let rungs = [
        vec![
            "stop".to_string(),
            "-t".to_string(),
            grace.to_string(),
            name.to_string(),
        ],
        vec!["rm".to_string(), "-f".to_string(), name.to_string()],
    ];
    for argv in rungs {
        let verb = argv[0].clone();
        match spawner.run("podman", &argv).await {
            Ok(out) if out.ok() => note(format!("podman {verb} {name} returned")),
            Ok(out) => note(format!(
                "podman {verb} {name} failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            )),
            Err(e) => note(format!("podman {verb} {name} could not be run: {e}")),
        }
    }
}

/// One line of stdout: a ledger event, or a run-log line verbatim.
async fn on_stdout(ctx: &JobCtx, run: &ledger::Run, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    // The HTTP ledger's own decoder, deliberately: both transports carry the
    // same four events, so a line that means one thing on a pipe cannot mean
    // another in a POST. It also answers the two edge cases for free — a line
    // that is not JSON becomes a run-log line verbatim (a banner must not kill
    // a run), and a line that happens to be a JSON *array* is read as the
    // several events it is.
    let before = run.log_len();
    for event in ledger::decode_body(trimmed) {
        run.apply(&event);
    }
    // Whatever the events wrote to the run log — a `log` event, a rejected row,
    // an undeclared column — mirrored into the live buffer the Run tab reads.
    for line in run.log_from(before) {
        ctx.state.agent_runs.push_log(ctx.id, &line);
    }
    let rows = run.rows();
    ctx.state.agent_runs.put(ctx.id, &rows);
    let detail = ledger::detail_of(run);
    let pending = run.take_pending();
    if pending.is_empty() {
        // A line that only went to the log still has to move the jobs frame:
        // the Run tab re-reads on a frame change, and a container that is
        // pulling, printing a banner or reporting a diagnostic produces
        // nothing else for minutes (§3.2, final review). `detail_of`'s
        // monotonic `log_lines` is what changed; the counters are re-read off
        // the live job so a log line cannot rewind them.
        if run.log_len() != before {
            ctx.progress_detail(detail).await;
        }
        return;
    }
    for mut p in pending {
        p.detail = detail.clone();
        ctx.progress(p).await;
    }
}

/// Turn the ending into the job's outcome, §3.2's table row by row.
async fn finish(
    ctx: &JobCtx,
    agent: &Agent,
    input: &Input,
    run: &ledger::Run,
    ending: Ending,
    stderr_tail: &std::collections::VecDeque<String>,
    output_schema: Option<&Value>,
) -> Result<JobOutcome, String> {
    let totals = ctx.state.agent_meters.take(ctx.id);
    let rows = run.rows();
    let mut result = ledger::result_of(run, input.phase, &agent.row.id, &rows, &totals);
    if let Some(m) = result.as_object_mut() {
        m.insert("container".into(), json!(true));
        // The Runs tab's "Applied" block reads the same shape an in-process
        // apply writes, so a container's apply renders through it unchanged.
        // `tool_calls` stays empty on purpose: the container's calls arrived
        // over `/mcp` and are Logs rows, not a list this run assembled.
        if input.phase == Phase::Apply {
            if let Some(out) = run.output() {
                m.insert(
                    "applied".into(),
                    json!({ "output": out, "text": "", "tool_calls": [] }),
                );
            }
        }
    }

    Ok(match ending {
        Ending::Cancelled => JobOutcome::CanceledWith(result),
        Ending::Deadline(n) => JobOutcome::FailedWith {
            error: format!("the run exceeded its deadline of {n}s (run.limits.deadline_seconds)"),
            value: result,
        },
        // One sentence: the caller already said what could not be done.
        Ending::NoProcess(e) => JobOutcome::FailedWith {
            error: e,
            value: result,
        },
        Ending::Exited(Exit::Code(0)) => {
            let at = format!("run.output.{}", input.phase.as_str());
            match ledger::check_output(output_schema, run.output().as_ref(), &at) {
                Ok(()) => JobOutcome::Done(result),
                // §3.2: a container that exits 0 without the output its
                // manifest declares has not done what it said it would.
                Err(_) if run.output().is_none() => JobOutcome::FailedWith {
                    error: format!(
                        "the container exited successfully but emitted no output; {at} declares \
                         a schema"
                    ),
                    value: result,
                },
                Err(error) => JobOutcome::FailedWith {
                    error,
                    value: result,
                },
            }
        }
        Ending::Exited(e) => JobOutcome::FailedWith {
            error: format!("{}{}", e.describe(), excerpt(stderr_tail)),
            value: result,
        },
    })
}

/// The stderr excerpt a failure message carries — the last
/// [`STDERR_EXCERPT_LINES`] lines, which is all `stderr_tail` ever holds.
///
/// Newline-joined, the shape `Registry::log_excerpt`'s callers render: a
/// traceback squashed onto one line with slashes is a traceback nobody reads.
/// The *full* stderr is in the run log regardless; only the message is short.
fn excerpt(lines: &std::collections::VecDeque<String>) -> String {
    if lines.is_empty() {
        return String::new();
    }
    format!("\n{}", lines.iter().cloned().collect::<Vec<_>>().join("\n"))
}

// ---------------------------------------------------------------------------
// Boot reconciliation (§6.4): `reconcile`
// ---------------------------------------------------------------------------

mod reconcile;
pub use reconcile::{boot_reconcile, reconcile, reconcile_since, Reconciled};

// ---------------------------------------------------------------------------
// The runtime half of the warnings (§4.3)
// ---------------------------------------------------------------------------

/// Is podman usable on this box right now? `Ok(())`, or the reason it is not.
///
/// Evaluated per page load rather than cached: it changes between two of them
/// (a podman upgrade, a user session ending), and a stored answer would be a
/// Start gate lmgw could not explain.
pub async fn podman_available(state: &SharedState) -> Result<(), String> {
    match state
        .agent_spawner()
        .run("podman", &["--version".to_string()])
        .await
    {
        Ok(out) if out.ok() => Ok(()),
        Ok(out) => Err(out.stderr.trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Three answers, like `Registry::container_exists`: "podman says it is not
/// there" and "podman could not answer" mean opposite things to a Start gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePresence {
    Present,
    Absent,
    Unknown(String),
}

/// `podman image exists <ref>`.
pub async fn image_present(state: &SharedState, image: &str) -> ImagePresence {
    // `--` before the reference: `podman image exists --help` exits 0, so a
    // reference that reads as a flag would come back `Present` and every
    // diagnosis after it would be about the wrong thing.
    let argv = vec![
        "image".to_string(),
        "exists".to_string(),
        "--".to_string(),
        image.to_string(),
    ];
    match state.agent_spawner().run("podman", &argv).await {
        Ok(out) if out.ok() => ImagePresence::Present,
        // `podman image exists` answers with its exit code alone: 1 is "no".
        Ok(out) if out.status == 1 => ImagePresence::Absent,
        Ok(out) => ImagePresence::Unknown(format!(
            "podman image exists {image} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => ImagePresence::Unknown(format!("podman image exists could not be run: {e}")),
    }
}

/// `true` for an image reference that is local to the machine that built it: a
/// `localhost/` prefix, or no registry host at all (§3.4).
///
/// The registry-host test is docker's own: the first path segment is a host
/// only if it contains a `.` or a `:`, or is literally `localhost`.
pub fn is_local_image(image: &str) -> bool {
    let first = image.split('/').next().unwrap_or_default();
    if image.contains('/') {
        first == "localhost" || !(first.contains('.') || first.contains(':'))
    } else {
        true
    }
}

/// `podman logs --tail <n> <name>`, through the **agent** spawner.
///
/// `Registry::logs_tail` does the same thing through the registry's own runner,
/// which is the model-container seam; a service container is started by the
/// agent seam, so a test that fakes the one must not have to fake the other to
/// read a log tail. Best effort: a container podman cannot find has no log, and
/// saying why is more useful than an empty string.
///
/// `tail = 0` is **everything** (`--tail -1`, podman's own spelling of "all
/// lines"; podman 5.8 refuses the word `all`), the house reading of `0` — not
/// podman's, where `--tail 0` prints nothing. A caller asking for "no bound"
/// must not get silence.
pub async fn logs_tail(spawner: &Arc<dyn Spawner>, name: &str, tail: usize) -> String {
    let argv = vec![
        "logs".to_string(),
        "--tail".to_string(),
        if tail == 0 {
            "-1".to_string()
        } else {
            tail.to_string()
        },
        name.to_string(),
    ];
    match spawner.run("podman", &argv).await {
        Ok(out) if out.ok() => format!("{}{}", out.stdout, out.stderr),
        Ok(out) => format!(
            "podman logs {name} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        ),
        Err(e) => format!("podman logs {name} could not be run: {e}"),
    }
}

#[cfg(test)]
mod tests;
