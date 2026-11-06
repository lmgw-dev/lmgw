//! lmgw's own CDI spec (machine-check design §3.6, ruling 4, WP3).
//!
//! Model containers get the graphics card through CDI: `--device
//! nvidia.com/gpu=all` names a device that a spec file describes — which
//! device nodes, libraries and hooks to put into the container. Podman reads
//! those specs from the system's dirs (`/etc/cdi`, `/var/run/cdi`), and the
//! system's spec goes stale on its own: NVIDIA's refresh unit fires on a new
//! kernel module and a new `nvidia-ctk`, not on an `egl-wayland` update that
//! deletes a library the spec names, and every GPU start fails
//! until someone runs `sudo nvidia-ctk cdi generate` again.
//!
//! So lmgw writes its own spec, with the user's rights (`nvidia-ctk cdi
//! generate` needs no root), into a dir of its own, and passes that dir to
//! Podman (`--cdi-spec-dir`, which replaces Podman's default dirs):
//!
//! - **Where.** `$XDG_RUNTIME_DIR/lmgw-machine/<instance>/cdi/nvidia.json`,
//!   `<instance>` the slug of the `container_prefix` as the agents' runtime
//!   dirs use it, so a dev instance never writes the file production reads;
//!   outside `$XDG_RUNTIME_DIR/lmgw/`, whose sweep removes whatever is not a
//!   live agent run. Without an absolute `XDG_RUNTIME_DIR`,
//!   `<data_dir>/machine/`. Every dir lmgw makes there is checked to be a
//!   real directory of this user that nobody else may enter before anything
//!   is read from or written into it. The stamp ([`STAMP_FILE`]) sits beside
//!   the `cdi` dir, not in it: Podman reads every `*.json` in a spec dir as a
//!   spec, and anything else found in the spec dir goes when the spec is
//!   written again.
//! - **When.** Before every GPU container start and every GPU throwaway probe
//!   ([`OwnSpec::ensure_fresh`]): a judgement that costs file reads and stats
//!   (`lmgw_machine::cdi::own`), and a new spec when the driver, the
//!   `nvidia-ctk` file, the rpm database, the `/dev/nvidia*` nodes or the boot
//!   changed since the last one, or a path it names is gone (D20). One
//!   generation at a time; a start that arrives during one waits for it and
//!   then finds the spec fresh. The wait and the generation are bounded by
//!   the start's load timeout ([`LOAD_TIMEOUT_LABEL`]): a program that runs
//!   it out is killed, and the start says which and how long it ran. At
//!   boot it is written after the machine check found an NVIDIA card whose
//!   driver works ([`OwnSpec::ensure_at_boot`], `crate::machine::boot`).
//! - **How.** `nvidia-ctk cdi generate --format=json
//!   --output=<staging>/nvidia.json --nvidia-cdi-hook-path=/usr/bin/nvidia-cdi-hook`
//!   (the hook flag only when this toolkit has it and installed the binary:
//!   asked once of `cdi generate --help`), none of `nvidia-ctk`'s `NVIDIA_CTK_*` variables passed on, into a
//!   private staging dir beside `cdi/`, then a rename into place, so Podman
//!   never reads a half-written file. Not a temporary name inside `cdi/`:
//!   nvidia-ctk appends an extension to an output name without one
//!   (measured with 1.20.1: `--output=.nvidia.yaml.tmp` writes
//!   `.nvidia.yaml.tmp.yaml`), and the CDI library reads every `*.yaml` and
//!   `*.json` there, dotfiles included. When `/dev/nvidia-uvm` or a card's node
//!   (each card in `/proc/driver/nvidia/gpus`) is missing, `nvidia-modprobe`
//!   runs first, as NVIDIA's own refresh creates the control nodes before it
//!   generates. A machine that changed while nvidia-ctk ran (a node it
//!   created itself, a package transaction) is written once more from a
//!   fresh reading.
//! - **Who passes it.** [`OwnSpec::globals`] is the one source of
//!   `--cdi-spec-dir` (§3.6.4): the model start and the throwaway probes
//!   through the registry, the copyable command line, the builds' verify log,
//!   the benchmark's container and an isolated MCP server's container through
//!   [`podman_globals`] and [`ready_globals`]. A row whose run arguments name
//!   a CDI device of another kind gets Podman's configured system dirs first
//!   and lmgw's last (D21). [`OwnSpec::refuse_unpointed`] refuses, where
//!   podman runs, a call that names the card without the option.
//! - **When it does not.** Without `/usr/bin/nvidia-ctk`, or with a Podman
//!   whose `podman --help` has no `--cdi-spec-dir`, lmgw writes no spec and
//!   passes no option: its containers use Podman's default spec dirs, as
//!   before ruling 4, and [`OwnSpec::default_dirs`] says why (D26).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_machine::cdi::own::{Blocked, Current, Stale, NVIDIA_CTK, OWN_SPEC_FILE, STAMP_FILE};
use lmgw_machine::Facts;

use crate::config::LOAD_TIMEOUT_LABEL;
use crate::runtime::registry::CommandRunner;
use crate::state::AppState;

pub mod args;
mod generate;

pub use args::{container_options, requested, restart_note, strip_gpu_restart, CdiRequest};

/// The name of lmgw's runtime dir for its machine files, under
/// `$XDG_RUNTIME_DIR`. Not `lmgw/`: that one is the agents' runs root, and
/// its sweep removes everything that is not a live run.
pub const RUNTIME_DIR: &str = "lmgw-machine";

/// The Podman global option this module is the only source of.
pub const SPEC_DIR_OPTION: &str = "--cdi-spec-dir";

/// The files of one instance's own spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecPaths {
    /// The dir everything is under, which lmgw does not make:
    /// `$XDG_RUNTIME_DIR`, else the data dir.
    pub base: PathBuf,
    /// The instance's machine dir.
    pub root: PathBuf,
    /// The spec dir Podman is pointed at.
    pub spec_dir: PathBuf,
    /// The spec itself.
    pub spec: PathBuf,
    /// What it was written for, beside `spec_dir`.
    pub stamp: PathBuf,
}

impl SpecPaths {
    fn under(base: PathBuf, root: PathBuf) -> Self {
        let spec_dir = root.join("cdi");
        Self {
            spec: spec_dir.join(OWN_SPEC_FILE),
            stamp: root.join(STAMP_FILE),
            spec_dir,
            root,
            base,
        }
    }

    /// The dirs lmgw makes and keeps private, outermost first: each one from
    /// below [`Self::base`] down to [`Self::root`], then the spec dir.
    pub fn private_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut walk = self.base.clone();
        if let Ok(rest) = self.root.strip_prefix(&self.base) {
            for part in rest.components() {
                walk.push(part);
                out.push(walk.clone());
            }
        }
        out.push(self.spec_dir.clone());
        out
    }
}

/// Why lmgw writes no spec and passes no option, so that its GPU containers
/// use Podman's default spec dirs (`/etc/cdi`, `/var/run/cdi`), as before
/// ruling 4 (D26). What the machine check (WP4) reads from
/// [`OwnSpec::default_dirs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultDirs {
    /// `nvidia-ctk` is not at [`NVIDIA_CTK`]: there is nothing to write the
    /// spec with.
    NoToolkit,
    /// This Podman's `podman --help` lists no `--cdi-spec-dir`: it would
    /// refuse the option.
    PodmanWithoutOption,
}

impl std::fmt::Display for DefaultDirs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoToolkit => write!(
                f,
                "NVIDIA's container toolkit is not installed ({NVIDIA_CTK} is not there), so lmgw \
                 writes no description of the graphics card of its own and its containers use \
                 the system's in /etc/cdi and /var/run/cdi"
            ),
            Self::PodmanWithoutOption => write!(
                f,
                "this Podman has no {SPEC_DIR_OPTION} option (`podman --help` does not list it), \
                 so lmgw's containers use the system's description of the graphics card in \
                 /etc/cdi and /var/run/cdi instead of lmgw's own"
            ),
        }
    }
}

/// What [`OwnSpec::ensure_fresh`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// The spec is true of this machine as it is.
    Fresh,
    /// It was written now, for these reasons.
    Written(Vec<Stale>),
    /// lmgw writes no spec now, for this reason: Podman's default dirs are
    /// used.
    DefaultDirs(DefaultDirs),
    /// This gateway manages no spec (a test state): nothing to do.
    Unmanaged,
}

/// Why lmgw's spec could not be made true of this machine — what a GPU start
/// that needed it fails with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdiError {
    /// It cannot be written now (a restart pending, no driver, no toolkit).
    Blocked(Blocked),
    /// `nvidia-ctk cdi generate` failed: its exit and its last lines.
    Generate { status: String, said: Vec<String> },
    /// What it wrote names paths that are not there.
    Incomplete(Vec<(PathBuf, String)>),
    /// `program` did not finish within the start's load timeout (`limit`,
    /// for the whole write) and was killed after running for `ran`.
    TimedOut {
        program: String,
        ran: Duration,
        limit: Duration,
    },
    /// Another write held the one-at-a-time lock for the start's whole load
    /// timeout.
    Waited { waited: Duration },
    /// The driver, the toolkit or the packages changed while it was written:
    /// the new spec was not used.
    Changed(Vec<Stale>),
    /// A dir lmgw keeps it in is not a private dir of this user.
    NotPrivate { dir: PathBuf, why: String },
    /// lmgw's own files could not be written or read.
    Io(String),
}

const SPEC_WORDS: &str = "the description of the graphics card that models use";

fn reasons(stale: &[Stale]) -> String {
    stale
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl std::fmt::Display for CdiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(b @ Blocked::Restart { .. }) => write!(
                f,
                "Restart the computer to finish the graphics driver update: {b}."
            ),
            Self::Blocked(b @ Blocked::NoModule) => {
                write!(f, "The NVIDIA graphics driver is not running: {b}.")
            }
            Self::Blocked(Blocked::NoToolkit { .. }) => write!(
                f,
                "NVIDIA's container toolkit is not installed: {NVIDIA_CTK} is not there, so lmgw \
                 cannot write {SPEC_WORDS}."
            ),
            Self::Generate { status, said } => write!(
                f,
                "lmgw could not write {SPEC_WORDS}: `nvidia-ctk cdi generate` exited {status}{}",
                if said.is_empty() {
                    String::new()
                } else {
                    format!(": {}", said.join(" / "))
                }
            ),
            Self::Incomplete(gone) => write!(
                f,
                "lmgw wrote {SPEC_WORDS}, and it names paths that are not there: {}",
                gone.iter()
                    .map(|(p, why)| format!("{} ({why})", p.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::TimedOut {
                program,
                ran,
                limit,
            } => write!(
                f,
                "`{program}` ran for {:.1} s without finishing and lmgw stopped it, because \
                 writing {SPEC_WORDS} may take {} s, the load timeout \
                 ({LOAD_TIMEOUT_LABEL}).",
                ran.as_secs_f64(),
                limit.as_secs()
            ),
            Self::Waited { waited } => write!(
                f,
                "lmgw waited {} s, the load timeout ({LOAD_TIMEOUT_LABEL}), for another writing \
                 of {SPEC_WORDS} to finish, and it did not.",
                waited.as_secs()
            ),
            Self::Changed(stale) => write!(
                f,
                "The graphics driver, its toolkit or the installed packages changed while lmgw \
                 wrote {SPEC_WORDS} ({}), so it was not used; the next start writes it again.",
                reasons(stale)
            ),
            Self::NotPrivate { dir, why } => write!(
                f,
                "lmgw keeps {SPEC_WORDS} in {}, and {why}, so lmgw does not use it.",
                dir.display()
            ),
            Self::Io(e) => write!(f, "lmgw could not write {SPEC_WORDS}: {e}"),
        }
    }
}

impl std::error::Error for CdiError {}

/// The outcome of the last [`OwnSpec::ensure_fresh`], for the machine check
/// (WP4): when, and what came of it.
#[derive(Debug, Clone)]
pub struct LastRun {
    pub at: chrono::DateTime<chrono::Utc>,
    pub paths: SpecPaths,
    pub outcome: Result<Ensured, CdiError>,
}

/// Where a throwaway probe's budget comes from: the gateway's load timeout,
/// read when the probe runs.
type LoadTimeout = Arc<dyn Fn() -> Duration + Send + Sync>;

enum Mode {
    /// lmgw writes and passes its spec, judged against the machine `facts`
    /// describes; the data dir is the fallback root; `podman` answers the
    /// one `podman --help` that says whether it takes the option.
    Managed {
        facts: Arc<dyn Facts>,
        data_dir: PathBuf,
        podman: Arc<dyn CommandRunner>,
    },
    /// No spec and no option: a test state, or a registry no gateway wired.
    Unmanaged(String),
}

/// lmgw's own CDI spec, for one gateway. See the module doc.
pub struct OwnSpec {
    mode: Mode,
    /// One generation at a time, held by the blocking work itself: a caller
    /// dropped mid-generation (a cancelled benchmark) leaves it running to
    /// its end, which the load timeout bounds, and the next caller waits for
    /// that end.
    generating: Arc<tokio::sync::Mutex<()>>,
    /// Written by the blocking work itself, so a dropped caller does not lose
    /// the outcome.
    last: Arc<Mutex<Option<LastRun>>>,
    /// Whether this Podman takes `--cdi-spec-dir`, asked once.
    podman_has_option: tokio::sync::OnceCell<bool>,
    /// The [`DefaultDirs`] reason last logged, so the log says it once per
    /// change.
    told: Mutex<Option<DefaultDirs>>,
    load_timeout: Mutex<LoadTimeout>,
    /// Whether this `nvidia-ctk` has the hook flag, asked of its `--help`.
    hook_flag: Arc<generate::HookFlagCache>,
}

/// The load timeout's default, until a gateway wires its own setting in.
fn default_load_timeout() -> Duration {
    Duration::from_secs(crate::config::VramSettings::default().load_timeout_seconds)
}

impl OwnSpec {
    fn with_mode(mode: Mode) -> Self {
        Self {
            mode,
            generating: Arc::new(tokio::sync::Mutex::new(())),
            last: Arc::new(Mutex::new(None)),
            podman_has_option: tokio::sync::OnceCell::new(),
            told: Mutex::new(None),
            load_timeout: Mutex::new(Arc::new(default_load_timeout)),
            hook_flag: Arc::new(Mutex::new(None)),
        }
    }

    /// The spec of a gateway on `data_dir`, read and written on the machine
    /// `facts` describes, passed to the Podman `podman` runs.
    pub fn managed(
        facts: Arc<dyn Facts>,
        data_dir: PathBuf,
        podman: Arc<dyn CommandRunner>,
    ) -> Self {
        Self::with_mode(Mode::Managed {
            facts,
            data_dir,
            podman,
        })
    }

    /// No spec and no option: a test state (which must not run `nvidia-ctk`
    /// on the machine the suite runs on), or a registry no gateway wired.
    pub fn unmanaged(why: impl Into<String>) -> Self {
        Self::with_mode(Mode::Unmanaged(why.into()))
    }

    /// Why this gateway manages no spec; `None` when it does.
    pub fn why_unmanaged(&self) -> Option<&str> {
        match &self.mode {
            Mode::Managed { .. } => None,
            Mode::Unmanaged(why) => Some(why),
        }
    }

    /// The machine this spec is judged against; `None` when unmanaged.
    pub fn facts(&self) -> Option<Arc<dyn Facts>> {
        match &self.mode {
            Mode::Managed { facts, .. } => Some(facts.clone()),
            Mode::Unmanaged(_) => None,
        }
    }

    /// Read the load timeout a throwaway probe's write gets from `source`
    /// (the gateway's setting) from now on.
    pub fn set_load_timeout(&self, source: impl Fn() -> Duration + Send + Sync + 'static) {
        *self.load_timeout.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(source);
    }

    /// The load timeout ([`LOAD_TIMEOUT_LABEL`]) as the gateway has it now:
    /// what a write for a call that carries no budget of its own may take.
    pub fn load_timeout(&self) -> Duration {
        let source = self
            .load_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        source()
    }

    /// Where the instance `container_prefix` keeps its spec; `None` when
    /// unmanaged.
    pub fn paths(&self, container_prefix: &str) -> Option<SpecPaths> {
        let Mode::Managed {
            facts, data_dir, ..
        } = &self.mode
        else {
            return None;
        };
        // An `XDG_RUNTIME_DIR` that is not absolute names nothing in
        // particular; the data dir is used as without one.
        let runtime = facts
            .env("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|x| x.is_absolute());
        Some(match runtime {
            Some(base) => {
                let root = base.join(RUNTIME_DIR).join(super::slug(container_prefix));
                SpecPaths::under(base, root)
            }
            None => SpecPaths::under(data_dir.clone(), data_dir.join("machine")),
        })
    }

    /// Why lmgw writes no spec and passes no option now, so that Podman's
    /// default spec dirs are used (D26); `None` when it manages its own, or
    /// manages nothing (a test state). A `stat` of `nvidia-ctk` and the
    /// cached answer of `podman --help`; the log says each change once.
    pub fn default_dirs(&self) -> Option<DefaultDirs> {
        let Mode::Managed { facts, .. } = &self.mode else {
            return None;
        };
        let why = if self.podman_has_option.get() == Some(&false) {
            Some(DefaultDirs::PodmanWithoutOption)
        } else if !lmgw_machine::facts::exists(&**facts, Path::new(NVIDIA_CTK)) {
            Some(DefaultDirs::NoToolkit)
        } else {
            None
        };
        let mut told = self.told.lock().unwrap_or_else(|e| e.into_inner());
        if *told != why {
            match why {
                Some(why) => tracing::warn!("lmgw's own CDI spec: {why}"),
                None => tracing::info!(
                    "lmgw's own CDI spec: written and passed to Podman again ({} is there)",
                    NVIDIA_CTK
                ),
            }
            *told = why;
        }
        why
    }

    /// The Podman global options for a podman call carrying `run_args` (the
    /// class's or the row's), to go before the subcommand: lmgw's spec dir
    /// when they name an NVIDIA CDI device, Podman's configured system dirs
    /// before it when they name another kind, nothing otherwise — and nothing
    /// while [`Self::default_dirs`] says so.
    ///
    /// The only source of [`SPEC_DIR_OPTION`] in lmgw.
    ///
    /// Until a GPU start or probe has asked `podman --help` once, this Podman
    /// is taken to have the option, so a command-line preview before that
    /// shows it.
    pub fn globals(&self, container_prefix: &str, run_args: &[String]) -> Vec<String> {
        let (Some(paths), Some(facts)) = (self.paths(container_prefix), self.facts()) else {
            return Vec::new();
        };
        let req = requested(run_args);
        if !req.nvidia && req.other_kinds.is_empty() {
            return Vec::new();
        }
        if self.default_dirs().is_some() {
            return Vec::new();
        }
        let mut dirs: Vec<PathBuf> = Vec::new();
        if !req.other_kinds.is_empty() {
            dirs.extend(lmgw_machine::cdi::configured_spec_dirs(&*facts).dirs);
        }
        dirs.push(paths.spec_dir);
        dirs.into_iter()
            .flat_map(|d| [SPEC_DIR_OPTION.to_string(), d.display().to_string()])
            .collect()
    }

    /// Make lmgw's spec true of this machine, writing it when it is not —
    /// before a podman call whose run arguments need it
    /// (`requested(..).nvidia`). `budget` is the start's load timeout: the
    /// wait for another write and this write together end when it does.
    pub async fn ensure_fresh(
        &self,
        container_prefix: &str,
        budget: Duration,
    ) -> Result<Ensured, CdiError> {
        self.ensure_within(container_prefix, generate::Limit::from_now(budget))
            .await
    }

    /// [`Self::ensure_fresh`] within a limit that already runs, so one
    /// deadline covers whatever the caller did before it.
    async fn ensure_within(
        &self,
        container_prefix: &str,
        limit: generate::Limit,
    ) -> Result<Ensured, CdiError> {
        let budget = limit.budget();
        let (Some(paths), Some(facts)) = (self.paths(container_prefix), self.facts()) else {
            return Ok(Ensured::Unmanaged);
        };
        if let Some(why) = self.default_dirs() {
            // Podman's default dirs are used, but a pending driver restart
            // (kernel module and libraries of two versions) still fails the
            // container: say so here, in words, not inside it.
            let outcome = match Current::read(&*facts).driver_block() {
                Some(b) => Err(CdiError::Blocked(b)),
                None => Ok(Ensured::DefaultDirs(why)),
            };
            record(&self.last, &paths, &outcome);
            return outcome;
        }
        let one =
            match tokio::time::timeout(limit.left(), self.generating.clone().lock_owned()).await {
                Ok(one) => one,
                Err(_) => {
                    let outcome = Err(CdiError::Waited { waited: budget });
                    record(&self.last, &paths, &outcome);
                    return outcome;
                }
            };
        let last = self.last.clone();
        let hook_flag = self.hook_flag.clone();
        tokio::task::spawn_blocking(move || {
            let _one = one;
            let outcome = generate::ensure(&*facts, &paths, limit, &hook_flag);
            record(&last, &paths, &outcome);
            outcome
        })
        .await
        .unwrap_or_else(|e| Err(CdiError::Io(format!("the spec check did not finish: {e}"))))
    }

    /// Ask this Podman once whether it takes [`SPEC_DIR_OPTION`]
    /// (`podman --help` lists it), within `budget`. An answer that does not
    /// come is not cached: the option is passed, and the podman call itself
    /// says what is wrong.
    async fn probe_podman(&self, limit: generate::Limit) {
        let budget = limit.left();
        let Mode::Managed { podman, .. } = &self.mode else {
            return;
        };
        if self.podman_has_option.initialized() {
            return;
        }
        let argv = ["--help".to_string()];
        match tokio::time::timeout(budget, podman.run("podman", &argv)).await {
            Ok(Ok(out)) if out.ok() => {
                let has =
                    out.stdout.contains(SPEC_DIR_OPTION) || out.stderr.contains(SPEC_DIR_OPTION);
                let _ = self.podman_has_option.set(has);
            }
            Ok(Ok(out)) => tracing::warn!(
                "`podman --help` exited {}: {}; lmgw passes {SPEC_DIR_OPTION} as if it took it",
                out.status,
                out.stderr.trim()
            ),
            Ok(Err(e)) => tracing::warn!(
                "`podman --help` could not be run ({e}); lmgw passes {SPEC_DIR_OPTION} as if it \
                 took it"
            ),
            Err(_) => tracing::warn!(
                "`podman --help` did not answer within {} s, what is left of the load timeout \
                 ({LOAD_TIMEOUT_LABEL}); lmgw passes {SPEC_DIR_OPTION} as if it took it",
                budget.as_secs()
            ),
        }
    }

    /// At boot (machine-check design §3.6.2): ask this Podman whether it
    /// takes [`SPEC_DIR_OPTION`], then make the spec true of this machine,
    /// both within `budget` (the load timeout). The machine check calls it
    /// once it found an NVIDIA card whose driver works.
    pub async fn ensure_at_boot(
        &self,
        container_prefix: &str,
        budget: Duration,
    ) -> Result<Ensured, CdiError> {
        let limit = generate::Limit::from_now(budget);
        self.probe_podman(limit).await;
        self.ensure_within(container_prefix, limit).await
    }

    /// [`Self::ensure_fresh`] when `run_args` need lmgw's spec, then
    /// [`Self::globals`] for them. `budget` is the caller's load timeout.
    pub async fn ready_globals(
        &self,
        container_prefix: &str,
        run_args: &[String],
        budget: Duration,
    ) -> Result<Vec<String>, CdiError> {
        let req = requested(run_args);
        if !req.nvidia && req.other_kinds.is_empty() {
            return Ok(Vec::new());
        }
        // One deadline for the probe, the wait for another write and the
        // write.
        let limit = generate::Limit::from_now(budget);
        self.probe_podman(limit).await;
        if req.nvidia {
            self.ensure_within(container_prefix, limit).await?;
        }
        Ok(self.globals(container_prefix, run_args))
    }

    /// Refuse a podman `argv` (without the program name) that gives a
    /// container the graphics card (`nvidia.com/gpu`, `--gpus`) without
    /// [`SPEC_DIR_OPTION`] before its subcommand, while this gateway passes
    /// its own spec: Podman would read the system's, which goes stale. Asked
    /// where podman runs (the registry's runner, the benchmark's launcher,
    /// the agent spawner, an isolated MCP server's container), so a podman
    /// call that skipped [`Self::globals`] fails with this sentence instead
    /// of starting on a stale spec.
    pub fn refuse_unpointed(&self, argv: &[String]) -> Result<(), String> {
        if self.why_unmanaged().is_some() {
            return Ok(());
        }
        let pointed = argv
            .first()
            .is_some_and(|a| a == SPEC_DIR_OPTION || a.starts_with("--cdi-spec-dir="));
        // Only the options before the image can give the card: what follows
        // it is the container's own command.
        let names_card = args::container_options(argv).is_some_and(|o| requested(o).nvidia);
        if pointed || !names_card || self.default_dirs().is_some() {
            return Ok(());
        }
        let line = argv.join(" ");
        tracing::error!(
            "refused `podman {line}`: it gives the container the graphics card without \
             {SPEC_DIR_OPTION}"
        );
        Err(format!(
            "lmgw did not run `podman {line}`: it gives a container the graphics card \
             without pointing Podman at lmgw's own description of it ({SPEC_DIR_OPTION} before \
             the subcommand), so Podman would read the system's, which can be out of date. \
             Every podman call that gives a container the card takes that option from one \
             place in lmgw; this one did not, which is a bug in lmgw."
        ))
    }

    /// The last [`Self::ensure_fresh`], if any ran.
    pub fn last(&self) -> Option<LastRun> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Log `outcome` and keep it as the last run.
fn record(last: &Mutex<Option<LastRun>>, paths: &SpecPaths, outcome: &Result<Ensured, CdiError>) {
    match outcome {
        Ok(Ensured::Written(why)) => tracing::info!(
            "wrote lmgw's CDI spec {} ({})",
            paths.spec.display(),
            reasons(why)
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("lmgw's CDI spec {}: {e}", paths.spec.display()),
    }
    *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(LastRun {
        at: chrono::Utc::now(),
        paths: paths.clone(),
        outcome: outcome.clone(),
    });
}

/// The load timeout ([`LOAD_TIMEOUT_LABEL`]) as `state`'s settings have it.
pub fn load_timeout(state: &AppState) -> Duration {
    Duration::from_secs(state.snapshot().settings.vram.load_timeout_seconds)
}

/// [`OwnSpec::globals`] for this gateway's instance — what a podman command
/// line that carries `run_args` puts before its subcommand.
pub fn podman_globals(state: &AppState, run_args: &[String]) -> Vec<String> {
    state
        .cdi()
        .globals(&state.snapshot().settings.container_prefix, run_args)
}

/// [`OwnSpec::ready_globals`] for this gateway's instance, within its load
/// timeout.
pub async fn ready_globals(state: &AppState, run_args: &[String]) -> Result<Vec<String>, CdiError> {
    let prefix = state.snapshot().settings.container_prefix.clone();
    state
        .cdi()
        .ready_globals(&prefix, run_args, load_timeout(state))
        .await
}
