//! The build-run executor (container-builds design §5 as amended by §14):
//! `JobKind::BuildRun`, one background job per run, key `build:<id>`.
//!
//! # A run
//!
//! [`start_run`] refuses up front what would only fail later — a builds dir
//! on tmpfs, git missing or too old, a live run of the same build — then
//! opens the `build_runs` row and its log, and spawns the job. The job
//! ([`BuildRunExecutor`]) walks the phases of §15's job detail, each one a
//! section of the log:
//!
//! 1. **waiting** — for the machine-wide build lock ([`lock`]), naming the
//!    holder; the cancel flag is polled while it waits;
//! 2. **resolve** — CUDA version, arch, each PR's forge state;
//! 3. **fetch** — base and extras into the pool, the Dockerfile chosen at the
//!    base, the config hash and immutable tag — and the **short-circuit**:
//!    that tag already exists and a succeeded run built it → `up_to_date`;
//! 4. **assemble** — a worktree per run, base + extras merged (a conflict
//!    fails the run with the report);
//! 5. **prepare** — the Containerfile copy with the edits applied, the
//!    ignore file, the build args;
//! 6. **build** — `podman build` through the [`Spawner`] seam, every line into
//!    the log, progress into the job detail; cancel is SIGTERM (then SIGKILL)
//!    and a sweep of the leftovers the build made (§14.2);
//! 7. **verify** — [`verify`]: `succeeded`, `broken` or `unverified`;
//! 8. **promote** — [`promote`]: moving tag, retention, a rebuild's orphan;
//! 9. **cleanup** — worktree and build context removed, always.
//!
//! Everything from the fetch to the cleanup happens **under the lock**: the
//! pool, the worktree and podman's leftovers are shared with every other
//! lmgw on the machine.
//!
//! # For the ops layer
//!
//! [`start_run`], [`verify_run`], [`promote_run`], [`read_log`],
//! [`resolve_preview`], [`check_merge`], [`build_env`] here; the image ops in
//! [`crate::backends::images`]. Cancel is the generic `jobs::cancel` on the
//! run's job.
//!
//! [`Spawner`]: crate::agents::container::Spawner

mod lock;
mod log;
mod podman;
mod progress;
mod promote;
mod resolve;
mod verify;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use lmgw_api_types::builds::{BuildPhase, BuildRunJobDetail, BuildRunStarted};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use log::{read_log, read_log_tail, LOG_CHUNK_BYTES};
pub use promote::promote_run;
pub use resolve::{build_env, check_merge, resolve_preview};
pub use verify::verify_run;

pub(crate) use podman::{
    buildah_dirs, cache_mount_dir, cache_mount_root, real_uid, short_id, Podman,
};

use self::lock::{BuildLock, Holder};
use self::log::RunLog;
use self::podman::{BuildCommand, Leftovers};
use self::progress::Progress as LineProgress;
use super::forge::ForgeClient;
use super::git::{self, AssembleOpts, Git, Pool};
use super::images::{describe_users, UsageIndex};
use super::model::{
    BuildRun, BuildRunInputs, BuildRunPatch, BuildRunStatus, BuildSpec, BuildTrigger, NewBuildRun,
    ResolvedInputs,
};
use super::oci::RegistryClient;
use super::presets::{self, DockerfileChoice, DockerfileInfo, SourceFacts};
use super::updates::UpdateStore;
use super::{paths, tags, validate, ForgeLookup, NoForgeLookup};
use crate::agents::container::{terminate, Exit};
use crate::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::state::{AppState, SharedState};
use crate::store;

/// The lock file's name under `$XDG_RUNTIME_DIR` (§5 "Serialization").
pub const LOCK_FILE: &str = "lmgw-build.lock";
/// How often a waiting run tries the lock again (and checks its cancel flag).
const LOCK_POLL: Duration = Duration::from_millis(500);
/// How often a running `podman build` checks the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(250);
/// SIGTERM to SIGKILL on cancel (§5). podman exits within half a second of a
/// SIGTERM (§14.2); the grace is for a build that is committing.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long the output pipes may stay open after `podman build` exited
/// before the reader stops waiting for them (and says so in the log) —
/// something that inherited them and lives on would otherwise hold the run
/// forever.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(30);
/// How often the job detail is refreshed while a long git step runs.
const GIT_PUBLISH: Duration = Duration::from_millis(500);

/// One live run per build.
pub fn job_key(build_id: i64) -> String {
    format!("build:{build_id}")
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// What the executor reaches through [`AppState::builds`]: the forge lookup,
/// the forge HTTP client the `forge_*` ops share with it, the registry client
/// and stored results of the update check (§8), and — for tests — where the
/// lock and buildah's scratch directories are.
#[derive(Default)]
pub struct BuildSeams {
    forge: RwLock<Option<Arc<dyn ForgeLookup>>>,
    forge_client: RwLock<Option<ForgeClient>>,
    registry_client: RwLock<Option<RegistryClient>>,
    updates: UpdateStore,
    lock_path: RwLock<Option<PathBuf>>,
    buildah_tmp: RwLock<Option<PathBuf>>,
    instance: std::sync::OnceLock<String>,
}

impl BuildSeams {
    /// The forge lookup runs ask for PR state: [`NoForgeLookup`] until one is
    /// installed.
    pub fn forge(&self) -> Arc<dyn ForgeLookup> {
        self.forge
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(NoForgeLookup))
    }

    /// Install the forge lookup (the real one at startup —
    /// [`GatewayForge`](crate::backends::forge::GatewayForge) — a fake in a
    /// test).
    pub fn set_forge(&self, forge: Arc<dyn ForgeLookup>) {
        *self.forge.write().unwrap_or_else(|p| p.into_inner()) = Some(forge);
    }

    /// The one forge HTTP client of this gateway: created on first use and
    /// reused (it holds the connection pool), shared by the `forge_*` ops and
    /// [`GatewayForge`](crate::backends::forge::GatewayForge). It carries no
    /// token — each call takes its own from the settings as they are then.
    pub fn forge_client(&self) -> Result<ForgeClient, String> {
        if let Some(c) = self
            .forge_client
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Ok(c);
        }
        let mut slot = self.forge_client.write().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = slot.clone() {
            return Ok(c);
        }
        let c = ForgeClient::new()?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Replace the forge HTTP client — a test's, pointed at a mock
    /// ([`ForgeClient::with_github_api`]) rather than at `LMGW_GITHUB_API`,
    /// which is process-wide and would leak between parallel tests.
    pub fn set_forge_client(&self, client: ForgeClient) {
        *self.forge_client.write().unwrap_or_else(|p| p.into_inner()) = Some(client);
    }

    /// The registry client of the image update check (§8): created on first
    /// use and reused. Anonymous — it carries no credentials at all.
    pub fn registry_client(&self) -> Result<RegistryClient, String> {
        if let Some(c) = self
            .registry_client
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Ok(c);
        }
        let mut slot = self
            .registry_client
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(c) = slot.clone() {
            return Ok(c);
        }
        let c = RegistryClient::new()?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Replace the registry client — a test's, with its endpoints pointed at
    /// a mock ([`RegistryClient::with_endpoint`]).
    pub fn set_registry_client(&self, client: RegistryClient) {
        *self
            .registry_client
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(client);
    }

    /// The update check's stored results (§8, [`super::updates`]).
    pub fn updates(&self) -> &UpdateStore {
        &self.updates
    }

    /// Where the machine-wide lock is: `$XDG_RUNTIME_DIR/lmgw-build.lock`,
    /// else `/tmp/lmgw-build-<uid>.lock` ([`default_lock_path`]).
    pub fn lock_path(&self) -> PathBuf {
        if let Some(p) = self
            .lock_path
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return p;
        }
        default_lock_path(
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            real_uid(),
        )
    }

    /// Where buildah keeps its `buildahNNN` scratch directories: `/var/tmp`.
    pub fn buildah_tmp(&self) -> PathBuf {
        self.buildah_tmp
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| PathBuf::from("/var/tmp"))
    }

    /// This gateway's instance id ([`load_instance_id`]): in every per-run
    /// file name, verify container name and the `dev.lmgw.instance` label, so
    /// instances sharing a builds dir and an image store (dev instances, a
    /// dev copy of production's database) never take each other's run ids
    /// for their own. Set at startup; a gateway that never set it gets a
    /// random one of its own (never shared, never persisted).
    pub fn instance_id(&self) -> &str {
        self.instance.get_or_init(new_instance_id)
    }

    /// Install the instance id (once, at startup — a later call is ignored).
    pub fn set_instance_id(&self, id: String) {
        let _ = self.instance.set(id);
    }

    /// Test-only: a lock of the test's own, so a suite never contends with
    /// (or writes the sidecar of) a real build on the machine.
    #[doc(hidden)]
    pub fn set_lock_path_for_tests(&self, path: PathBuf) {
        *self.lock_path.write().unwrap_or_else(|p| p.into_inner()) = Some(path);
    }

    /// Test-only: a scratch root of the test's own instead of `/var/tmp`.
    #[doc(hidden)]
    pub fn set_buildah_tmp_for_tests(&self, path: PathBuf) {
        *self.buildah_tmp.write().unwrap_or_else(|p| p.into_inner()) = Some(path);
    }
}

/// The settings-table key of the instance id record.
const INSTANCE_KV: &str = "backends:instance";

/// 8 random hex characters.
fn new_instance_id() -> String {
    hex::encode(rand::random::<[u8; 4]>())
}

/// The instance id record: the id and the data dir it was minted for.
#[derive(Debug, Serialize, Deserialize)]
struct InstanceRecord {
    id: String,
    data_dir: String,
}

/// This gateway's instance id: generated once and kept in the settings table
/// with the data dir it was generated for. A database that turns up under
/// another data dir — a dev instance started on a copy of production's —
/// gets a fresh id (and keeps it), so the copy never passes for production
/// in the builds dir or the image labels they share. A database that cannot
/// be read or written gives a random id for this process and says so.
pub async fn load_instance_id(db: &sqlx::SqlitePool, data_dir: &Path) -> String {
    let here = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf())
        .display()
        .to_string();
    match store::get_kv(db, INSTANCE_KV).await {
        Ok(Some(raw)) => match serde_json::from_str::<InstanceRecord>(&raw) {
            Ok(r) if r.data_dir == here && valid_instance_id(&r.id) => return r.id,
            Ok(r) => tracing::info!(
                "build instance id {} was minted for {} — this data dir ({here}) gets its own",
                r.id,
                r.data_dir
            ),
            Err(e) => {
                tracing::warn!("the build instance id record is unreadable ({e}); minting one")
            }
        },
        Ok(None) => {}
        Err(e) => {
            let id = new_instance_id();
            tracing::warn!("reading the build instance id failed ({e}); using {id} for this run");
            return id;
        }
    }
    let id = new_instance_id();
    let record = InstanceRecord {
        id: id.clone(),
        data_dir: here,
    };
    let json = serde_json::to_string(&record).unwrap_or_default();
    if let Err(e) = store::set_kv(db, INSTANCE_KV, &json).await {
        tracing::warn!("saving the build instance id failed ({e}); {id} lasts this run only");
    }
    id
}

/// What a stored instance id must look like to be used in file and container
/// names: a short lowercase hex string.
fn valid_instance_id(id: &str) -> bool {
    (4..=32).contains(&id.len()) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The machine-wide lock file (§5 "Serialization"): in the session's runtime
/// dir when there is one, else `/tmp/lmgw-build-<uid>.lock` — one fixed path
/// per user either way, so every instance of this user excludes every other
/// whatever builds dir each one uses (a fallback under the builds dir would
/// let two instances with different builds dirs build at once).
fn default_lock_path(runtime_dir: Option<PathBuf>, uid: u32) -> PathBuf {
    match runtime_dir.filter(|p| p.is_absolute() && p.is_dir()) {
        Some(dir) => dir.join(LOCK_FILE),
        None => PathBuf::from(format!("/tmp/lmgw-build-{uid}.lock")),
    }
}

// ---------------------------------------------------------------------------
// Starting a run
// ---------------------------------------------------------------------------

/// The job's input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub run_id: i64,
    pub build_id: i64,
    pub rebuild: bool,
}

/// `build_run` (§15): start a run of build `build_id`. `rebuild` is **Rebuild
/// anyway** — no short-circuit, and `--pull=newer`.
///
/// Refused, with the reason, when the builds dir is on tmpfs, git is missing
/// or too old, or the build already has a live run. A run that has to wait
/// for another build's lock is started (its phase says `waiting`).
pub async fn start_run(
    state: &SharedState,
    build_id: i64,
    trigger: BuildTrigger,
    rebuild: bool,
) -> Result<BuildRunStarted, String> {
    let build = store::get_build(&state.db, build_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build with id {build_id}"))?;
    let builds_dir = state.builds_dir();
    if let Some(why) = paths::tmpfs_refusal(&builds_dir) {
        return Err(why);
    }
    git::git_available(&Git::new()).await?;
    if let Some(job) = state
        .jobs
        .live_by_key(JobKind::BuildRun, &job_key(build_id))
    {
        return Err(format!(
            "build '{}' already has a run in progress (job {}) — wait for it, or cancel it",
            build.spec.slug, job.id
        ));
    }

    let run_id = store::insert_build_run(&state.db, &NewBuildRun::of(&build, trigger))
        .await
        .map_err(|e| e.to_string())?;
    let instance = state.builds.instance_id();
    let wanted = paths::log_path(&builds_dir, instance, run_id);
    // Created now, so the first poll of the log finds a file — and new, since
    // a file of this name is not this run's ([`RunLog::create`]).
    let opened = RunLog::create(&wanted).map(|log| {
        log.header(&format!(
            "run {run_id} of build {} ({}){} — instance {instance}",
            build.spec.slug,
            trigger.as_str(),
            if rebuild { ", rebuild anyway" } else { "" }
        ));
        log.path().to_path_buf()
    });
    let log_path = opened.as_ref().map_or(wanted.clone(), |p| p.clone());
    store::update_build_run(
        &state.db,
        run_id,
        &BuildRunPatch {
            log_path: Some(log_path.display().to_string()),
            ..BuildRunPatch::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Err(e) = opened {
        let _ = store::finish_build_run(&state.db, run_id, BuildRunStatus::Failed, Some(&e)).await;
        return Err(e);
    }

    let input = Input {
        run_id,
        build_id,
        rebuild,
    };
    let spawned = jobs::spawn(
        state,
        JobKind::BuildRun,
        Some(job_key(build_id)),
        format!("build {}", build.spec.slug),
        serde_json::to_value(&input).map_err(|e| e.to_string())?,
    )
    .await;
    let not_started = |why: String| async move {
        let _ =
            store::finish_build_run(&state.db, run_id, BuildRunStatus::Failed, Some(&why)).await;
        Err::<BuildRunStarted, String>(why)
    };
    match spawned {
        Ok(Spawn::Started(job_id)) => {
            store::update_build_run(
                &state.db,
                run_id,
                &BuildRunPatch {
                    job_id: Some(job_id),
                    ..BuildRunPatch::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(BuildRunStarted { job_id, run_id })
        }
        Ok(Spawn::AlreadyRunning(job_id)) => {
            not_started(format!(
                "not started: build '{}' already has a run in progress (job {job_id})",
                build.spec.slug
            ))
            .await
        }
        Err(e) => not_started(format!("not started: {e}")).await,
    }
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

pub struct BuildRunExecutor;

#[async_trait::async_trait]
impl JobExecutor for BuildRunExecutor {
    fn kind(&self) -> JobKind {
        JobKind::BuildRun
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("build_run input: {e}"))?;
        let state = ctx.state.clone();
        let run = store::get_build_run(&state.db, input.run_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("build run {} no longer exists", input.run_id))?;
        let run_id = run.id;
        let mut exec = match Execution::open(ctx, run, input.build_id, input.rebuild) {
            Ok(x) => x,
            Err(e) => {
                let _ =
                    store::finish_build_run(&state.db, run_id, BuildRunStatus::Failed, Some(&e))
                        .await;
                return Err(e);
            }
        };
        let (status, error) = exec.go().await;
        let outcome = exec.finish(status, error).await;
        // A finished run can clear (or start) an update badge (§8).
        super::updates::publish(&state).await;
        outcome
    }
}

/// Why a run stopped short.
enum Stop {
    Failed(String),
    Canceled,
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

/// A run that ended normally: its verdict.
struct Done {
    status: BuildRunStatus,
    error: Option<String>,
}

/// What prepare leaves for the build.
struct Prepared {
    containerfile: PathBuf,
    ignorefile: PathBuf,
    iidfile: PathBuf,
    build_args: Vec<(String, String)>,
    facts: SourceFacts,
}

/// One run in flight.
struct Execution {
    ctx: JobCtx,
    run: BuildRun,
    spec: BuildSpec,
    rebuild: bool,
    builds_dir: PathBuf,
    log: Arc<RunLog>,
    detail: BuildRunJobDetail,
    /// podman's `[k/N]` stage and `STEP n/m` of the current phase, for the
    /// job row's overall percent ([`progress::overall_percent`]).
    stage: Option<(u64, u64)>,
    step: Option<(u64, u64)>,
    podman: Podman,
}

fn phase_name(p: BuildPhase) -> &'static str {
    match p {
        BuildPhase::Waiting => "waiting",
        BuildPhase::Resolve => "resolve",
        BuildPhase::Fetch => "fetch",
        BuildPhase::Assemble => "assemble",
        BuildPhase::Prepare => "prepare",
        BuildPhase::Build => "build",
        BuildPhase::Verify => "verify",
        BuildPhase::Promote => "promote",
        BuildPhase::Cleanup => "cleanup",
    }
}

impl Execution {
    fn open(ctx: JobCtx, run: BuildRun, build_id: i64, rebuild: bool) -> Result<Self, String> {
        let state = ctx.state.clone();
        let builds_dir = state.builds_dir();
        let log_path =
            run.log_path.clone().map(PathBuf::from).unwrap_or_else(|| {
                paths::log_path(&builds_dir, state.builds.instance_id(), run.id)
            });
        let log = Arc::new(RunLog::open(&log_path)?);
        Ok(Self {
            detail: BuildRunJobDetail {
                run_id: run.id,
                build_id,
                ..BuildRunJobDetail::default()
            },
            spec: run.inputs.config.clone(),
            stage: None,
            step: None,
            podman: Podman::of(&state),
            ctx,
            run,
            rebuild,
            builds_dir,
            log,
        })
    }

    fn state(&self) -> SharedState {
        self.ctx.state.clone()
    }

    /// A git progress sink that writes into this run's log.
    fn echo(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let log = self.log.clone();
        move |line: &str| log.line(line)
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        if self.ctx.canceled() {
            Err(Stop::Canceled)
        } else {
            Ok(())
        }
    }

    /// Push the job detail (the jobs layer throttles the row and the feed).
    async fn publish(&self) {
        let mut detail = self.detail.clone();
        detail.last_line = self.log.last_line();
        let (done, total) = match progress::overall_percent(self.stage, self.step, detail.percent) {
            Some(p) => (p, Some(100)),
            None => (0, None),
        };
        let stage = match &detail.waiting_for {
            Some(w) => format!("waiting for {w}"),
            None => phase_name(detail.phase).to_string(),
        };
        self.ctx
            .progress(JobProgress {
                done,
                total,
                stage,
                detail: serde_json::to_value(&detail).unwrap_or(Value::Null),
            })
            .await;
    }

    async fn phase(&mut self, p: BuildPhase) {
        self.detail.phase = p;
        self.detail.step = None;
        self.detail.stage = None;
        self.detail.percent = None;
        self.stage = None;
        self.step = None;
        self.detail.waiting_for = None;
        self.log.header(phase_name(p));
        self.publish().await;
    }

    /// Await `fut` (a git step, the forge lookups) while keeping the job
    /// detail's `last_line` current, so a long fetch shows what git is
    /// saying — and stop it when the run is canceled: the future is dropped,
    /// which kills its git (`kill_on_drop`). What a git killed mid-fetch
    /// leaves in the pool (`*.lock` files), the next holder of the pool lock
    /// clears ([`Pool::lock_pool`]); a half-made worktree goes with the
    /// run's cleanup.
    async fn publishing<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, Stop> {
        tokio::pin!(fut);
        let mut tick = tokio::time::interval(GIT_PUBLISH);
        loop {
            tokio::select! {
                out = &mut fut => return out.map_err(Stop::Failed),
                _ = tick.tick() => {
                    if self.ctx.canceled() {
                        self.log.line(&format!(
                            "cancel requested — stopping the {} step",
                            phase_name(self.detail.phase)
                        ));
                        return Err(Stop::Canceled);
                    }
                    self.publish().await;
                }
            }
        }
    }

    /// One line of build output: into the log, progress into the detail.
    async fn note_line(&mut self, line: &str) {
        self.log.line(line);
        match progress::parse(line) {
            Some(LineProgress::Step { stage, n, m }) => {
                self.detail.step = Some(format!("{n}/{m}"));
                self.detail.stage = stage.map(|(k, total)| format!("{k}/{total}"));
                self.detail.percent = None;
                self.stage = stage;
                self.step = Some((n, m));
            }
            Some(LineProgress::Percent(p)) => self.detail.percent = Some(p),
            None => {}
        }
        self.publish().await;
    }

    // -- the lock -------------------------------------------------------------

    async fn acquire_lock(&mut self) -> Result<BuildLock, Stop> {
        let state = self.state();
        let path = state.builds.lock_path();
        let mut waiting = false;
        loop {
            if let Some(lock) = BuildLock::try_acquire(&path)? {
                lock.announce(&Holder {
                    slug: self.spec.slug.clone(),
                    run_id: self.run.id,
                    pid: std::process::id(),
                });
                if waiting {
                    self.log.line("the build lock is free — starting");
                }
                return Ok(lock);
            }
            let holder = lock::read_holder(&path)
                .map(|h| h.describe())
                .unwrap_or_else(|| "another build".to_string());
            if !waiting {
                waiting = true;
                self.phase(BuildPhase::Waiting).await;
                self.log.line(&format!(
                    "waiting for {holder} — one build runs at a time on this machine ({})",
                    path.display()
                ));
            }
            if self.detail.waiting_for.as_deref() != Some(holder.as_str()) {
                self.detail.waiting_for = Some(holder);
                self.publish().await;
            }
            self.check_cancel()?;
            tokio::time::sleep(LOCK_POLL).await;
        }
    }

    // -- the run --------------------------------------------------------------

    async fn go(&mut self) -> (BuildRunStatus, Option<String>) {
        let lock = match self.acquire_lock().await {
            Ok(l) => l,
            Err(stop) => return self.stopped(stop),
        };
        let instance = self.state().builds.instance_id().to_string();
        let worktree = paths::worktree_dir(&self.builds_dir, &instance, self.run.id);
        let ctx_dir = paths::context_dir(&self.builds_dir, &instance, self.run.id);
        let pool = Pool::open(Git::new(), &self.builds_dir)
            .await
            .map(|p| p.with_instance(&instance));
        let result = match &pool {
            Ok(pool) => self.pipeline(pool, &worktree, &ctx_dir).await,
            Err(e) => Err(Stop::Failed(e.clone())),
        };
        self.phase(BuildPhase::Cleanup).await;
        if let Ok(pool) = &pool {
            if worktree.exists() {
                let echo = self.echo();
                if let Err(e) = pool.worktree_remove(&worktree, Some(&echo)).await {
                    self.log
                        .line(&format!("could not remove the worktree: {e}"));
                }
            }
        }
        if ctx_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&ctx_dir) {
                self.log.line(&format!(
                    "could not remove the build context {}: {e}",
                    ctx_dir.display()
                ));
            }
        }
        drop(lock);
        match result {
            Ok(done) => (done.status, done.error),
            Err(stop) => self.stopped(stop),
        }
    }

    fn stopped(&self, stop: Stop) -> (BuildRunStatus, Option<String>) {
        match stop {
            Stop::Failed(e) => {
                self.log.line(&format!("failed: {e}"));
                (BuildRunStatus::Failed, Some(e))
            }
            Stop::Canceled => {
                self.log.line("canceled");
                (BuildRunStatus::Canceled, None)
            }
        }
    }

    async fn patch(&self, patch: BuildRunPatch) -> Result<(), Stop> {
        store::update_build_run(&self.ctx.state.db, self.run.id, &patch)
            .await
            .map_err(|e| Stop::Failed(e.to_string()))
    }

    async fn pipeline(
        &mut self,
        pool: &Pool,
        worktree: &Path,
        ctx_dir: &Path,
    ) -> Result<Done, Stop> {
        let state = self.state();
        let spec = self.spec.clone();
        let echo = self.echo();

        // 1–2. Resolve and fetch.
        self.phase(BuildPhase::Resolve).await;
        let host = self
            .publishing(resolve::resolve_host(&state, &spec, &echo))
            .await?;
        self.check_cancel()?;
        self.phase(BuildPhase::Fetch).await;
        let inputs = self
            .publishing(resolve::fetch_inputs(&state, &spec, pool, &host.prs, &echo))
            .await?;
        let base = inputs.base.sha.clone();
        let (choice, _) = self
            .publishing(resolve::pick_dockerfile(pool, &spec, &base, &echo))
            .await?;
        let resolved = resolve::resolved_inputs(&host, &inputs, &choice);
        let cfg = tags::cfg_hash(&spec, &resolved)?;
        let immutable = tags::immutable_tag_for(spec.engine, &spec.slug, &base, &cfg, state.dev())?;
        self.log.line(&format!("config hash {cfg}"));
        self.log.line(&format!("image: {immutable}"));
        self.patch(BuildRunPatch {
            inputs: Some(BuildRunInputs {
                config: spec.clone(),
                resolved: Some(resolved.clone()),
                cfg_hash: Some(cfg.clone()),
            }),
            base_sha: Some(base.clone()),
            cfg_hash: Some(cfg.clone()),
            ..BuildRunPatch::default()
        })
        .await?;
        self.check_cancel()?;

        // The short-circuit (§5 step 2).
        let before = self.podman.image_id(&immutable).await?;
        if let Some(id) = &before {
            if self.rebuild {
                self.log.line(&format!(
                    "{immutable} exists ({}) — Rebuild anyway builds it again, pulling newer \
                     base images",
                    short_id(id)
                ));
            } else if let Some(verified) = self.verified_run_with(&immutable, id).await? {
                return self.up_to_date(&immutable, id, verified).await;
            } else {
                self.log.line(&format!(
                    "{immutable} exists ({}), but no run that built it was verified — building \
                     it again",
                    short_id(id)
                ));
            }
        }

        // 3. Assemble.
        self.phase(BuildPhase::Assemble).await;
        self.publishing(pool.worktree_add(&base, worktree, Some(&echo)))
            .await?;
        let report = self
            .publishing(pool.assemble(
                worktree,
                &base,
                &inputs.assemble,
                AssembleOpts::default(),
                Some(&echo),
            ))
            .await?;
        for step in report.steps() {
            self.log.line(&format!(
                "{} ({}): {} — {}",
                step.label,
                git::short_sha(&step.sha),
                step.outcome,
                step.note
            ));
        }
        if let Some(c) = &report.conflict {
            return Err(Stop::Failed(format!(
                "{} — Check merge shows the same report; reorder, pin or drop the extra",
                c.message()
            )));
        }
        self.check_cancel()?;

        // 4. Prepare.
        self.phase(BuildPhase::Prepare).await;
        let prepared = self
            .prepare(pool, &report.head, &base, &choice, &host, ctx_dir)
            .await?;
        self.check_cancel()?;

        // 5. Build — tagged `<immutable>-r<run>` only ([`tags::run_tag`]):
        // the immutable tag is earned after verify, below.
        self.phase(BuildPhase::Build).await;
        let run_tag = tags::run_tag(&immutable, self.run.id);
        let image = self
            .build(&prepared, &choice, &resolved, &cfg, &run_tag, worktree)
            .await?;
        let size = match self.podman.image_size(&image).await {
            Ok(s) => Some(s),
            Err(e) => {
                self.log
                    .line(&format!("the image size could not be read: {e}"));
                None
            }
        };
        self.patch(BuildRunPatch {
            image_id: Some(image.clone()),
            tags: Some(vec![run_tag.clone()]),
            size_bytes: size,
            ..BuildRunPatch::default()
        })
        .await?;
        self.log.line(&format!(
            "built {run_tag} = {}{}",
            short_id(&image),
            size.map(|s| format!(", {:.2} GB", s as f64 / 1e9))
                .unwrap_or_default()
        ));

        // 6. Verify.
        self.phase(BuildPhase::Verify).await;
        let verdict = verify::verify_image(
            &state,
            spec.engine,
            &choice.verify,
            &image,
            &run_tag,
            self.run.id,
            &self.log,
        )
        .await;
        self.log
            .line(&format!("verify: {}", verdict.status.as_str()));
        self.patch(BuildRunPatch {
            verify: serde_json::to_value(&verdict.report).ok(),
            ..BuildRunPatch::default()
        })
        .await?;

        // The immutable tag names "the image these inputs build", and what a
        // model pinned to it runs. It goes to this image when the image is
        // verified, or when nothing holds it yet (the first image of these
        // inputs takes it whatever its verdict). A broken or unverified
        // rebuild never takes it from the image it names: that one keeps
        // it, and this run keeps its `-r<run>` tag.
        let holder = self.podman.image_id(&immutable).await?;
        let take = match &holder {
            None => true,
            Some(h) => *h == image || verdict.status == BuildRunStatus::Succeeded,
        };
        let own_tags = if take {
            self.claim_immutable(&image, &immutable, &run_tag).await
        } else {
            self.log.line(&format!(
                "kept as {run_tag}: {immutable} still names {} — a run that is not verified \
                 never takes the immutable tag from an image, so whatever is pinned to it keeps \
                 running that one",
                holder.as_deref().map(short_id).unwrap_or("?")
            ));
            vec![run_tag.clone()]
        };
        self.patch(BuildRunPatch {
            tags: Some(own_tags.clone()),
            ..BuildRunPatch::default()
        })
        .await?;
        let replaced = holder.filter(|h| take && *h != image);
        if verdict.status != BuildRunStatus::Succeeded {
            return Ok(Done {
                status: verdict.status,
                error: verdict.error,
            });
        }

        // 7. Promote.
        self.phase(BuildPhase::Promote).await;
        let run = store::get_build_run(&state.db, self.run.id)
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| self.run.clone());
        if let Err(e) =
            promote::after_verify(&state, &run, &image, replaced.as_deref(), &self.log).await
        {
            // Built and verified is what the run is — its verdict stands, and
            // the error says what is missing. Make current (`promote_run`)
            // accepts a succeeded run, so it can retry the move.
            let why = format!(
                "built and verified as {}, but the moving tag could not be moved: {e} — Make \
                 current retries it",
                own_tags.first().map(String::as_str).unwrap_or(&run_tag)
            );
            self.log.line(&why);
            return Ok(Done {
                status: BuildRunStatus::Succeeded,
                error: Some(why),
            });
        }
        Ok(Done {
            status: BuildRunStatus::Succeeded,
            error: None,
        })
    }

    /// Give `image` the immutable tag and drop its `-r<run>` tag: the tags
    /// the run records. A failed tag leaves the image as it was built (and
    /// says so); a failed untag leaves both names, both recorded.
    async fn claim_immutable(&self, image: &str, immutable: &str, run_tag: &str) -> Vec<String> {
        if let Err(e) = self.podman.tag(image, immutable).await {
            self.log.line(&format!(
                "could not tag {immutable}: {e} — the image stays {run_tag}"
            ));
            return vec![run_tag.to_string()];
        }
        match self.podman.untag(image, run_tag).await {
            Ok(()) => {
                self.log.line(&format!("tagged {immutable}"));
                vec![immutable.to_string()]
            }
            Err(e) => {
                self.log.line(&format!(
                    "tagged {immutable}, but {run_tag} could not be removed: {e}"
                ));
                vec![immutable.to_string(), run_tag.to_string()]
            }
        }
    }

    /// A succeeded run of this build whose image is exactly what `tag` names
    /// now (a tag re-pointed by a later, unverified rebuild does not count).
    async fn verified_run_with(&self, tag: &str, id: &str) -> Result<Option<i64>, Stop> {
        let Some(build_id) = self.run.build_id else {
            return Ok(None);
        };
        let runs = store::list_runs_for_build(&self.ctx.state.db, build_id)
            .await
            .map_err(|e| Stop::Failed(e.to_string()))?;
        Ok(runs
            .iter()
            .find(|r| {
                r.id != self.run.id
                    && r.status == BuildRunStatus::Succeeded
                    && r.image_id.as_deref() == Some(id)
                    && r.tags.iter().any(|t| t == tag)
            })
            .map(|r| r.id))
    }

    async fn up_to_date(&mut self, tag: &str, id: &str, verified: i64) -> Result<Done, Stop> {
        self.log.line(&format!(
            "up to date: {tag} is the image run {verified} built and verified ({}) — nothing to \
             build (Rebuild anyway builds it again)",
            short_id(id)
        ));
        let moving = tags::moving_tag_for(self.spec.engine, &self.spec.slug, self.state().dev());
        if self.podman.image_id(&moving).await?.as_deref() != Some(id) {
            self.log.line(&format!(
                "note: {moving} points at another image — Make current on run {verified} moves \
                 it here"
            ));
        }
        let size = self.podman.image_size(id).await.ok();
        self.patch(BuildRunPatch {
            image_id: Some(id.to_string()),
            tags: Some(vec![tag.to_string()]),
            size_bytes: size,
            ..BuildRunPatch::default()
        })
        .await?;
        Ok(Done {
            status: BuildRunStatus::UpToDate,
            error: None,
        })
    }

    async fn prepare(
        &self,
        pool: &Pool,
        head: &str,
        base: &str,
        choice: &DockerfileChoice,
        host: &resolve::HostFacts,
        ctx_dir: &Path,
    ) -> Result<Prepared, Stop> {
        let spec = &self.spec;
        let text = pool
            .read_file(head, &choice.dockerfile)
            .await?
            .ok_or_else(|| {
                format!(
                    "{} is gone from the assembled tree — an extra deleted or moved it; set \
                 dockerfile (Advanced) to its new path",
                    choice.dockerfile
                )
            })?;
        if !choice.target.is_empty() && !DockerfileInfo::parse(&text).has_stage(&choice.target) {
            return Err(Stop::Failed(format!(
                "the assembled {} has no stage '{}' any more (an extra changed it) — set target \
                 (Advanced)",
                choice.dockerfile, choice.target
            )));
        }
        let report =
            presets::apply_edits_report(&text, &choice.edits, &resolve::template_vars(spec));
        for l in report.log_lines() {
            self.log.line(&l);
        }
        report.check()?;
        let info = DockerfileInfo::parse(&report.text);
        let facts = resolve::source_facts(pool, &spec.repo_url, base).await?;
        let args = presets::build_args(
            choice,
            spec.backend,
            &info,
            &facts,
            host.cuda.as_deref(),
            &host.arch,
            &spec.build_args,
        )?;
        for n in &args.notes {
            self.log.line(&format!("note: {n}"));
        }
        std::fs::create_dir_all(ctx_dir)
            .map_err(|e| format!("could not create {}: {e}", ctx_dir.display()))?;
        let containerfile = ctx_dir.join("Containerfile");
        let ignorefile = ctx_dir.join("ignorefile");
        let iidfile = ctx_dir.join("iid");
        std::fs::write(&containerfile, &report.text)
            .map_err(|e| format!("could not write {}: {e}", containerfile.display()))?;
        std::fs::write(&ignorefile, presets::IGNORE_FILE)
            .map_err(|e| format!("could not write {}: {e}", ignorefile.display()))?;
        let _ = std::fs::remove_file(&iidfile);
        Ok(Prepared {
            containerfile,
            ignorefile,
            iidfile,
            build_args: args.args,
            facts,
        })
    }

    /// The `dev.lmgw.*` and OCI labels (§5 "Labels"): enough for an image to
    /// describe itself after its rows are gone.
    fn labels(
        &self,
        resolved: &ResolvedInputs,
        cfg: &str,
        facts: &SourceFacts,
    ) -> Vec<(String, String)> {
        let spec = &self.spec;
        let mut labels: Vec<(&str, String)> = vec![
            (
                "dev.lmgw.instance",
                self.state().builds.instance_id().to_string(),
            ),
            ("dev.lmgw.run", self.run.id.to_string()),
            ("dev.lmgw.build", self.detail.build_id.to_string()),
            ("dev.lmgw.slug", spec.slug.clone()),
            ("dev.lmgw.engine", spec.engine.as_str().into()),
            ("dev.lmgw.repo", validate::redact_url(&spec.repo_url)),
            ("dev.lmgw.ref", spec.git_ref.clone()),
            ("dev.lmgw.base", resolved.base_sha.clone()),
            (
                "dev.lmgw.extras",
                serde_json::to_string(&resolved.extras).unwrap_or_else(|_| "[]".into()),
            ),
            ("dev.lmgw.backend", spec.backend.as_str().into()),
            ("dev.lmgw.arch", resolved.arch.join(";")),
        ];
        if let Some(cuda) = &resolved.cuda_version {
            labels.push(("dev.lmgw.cuda", cuda.clone()));
        }
        labels.extend([
            ("dev.lmgw.cfg", cfg.to_string()),
            (
                "org.opencontainers.image.source",
                presets::web_url(&spec.repo_url),
            ),
            (
                "org.opencontainers.image.revision",
                resolved.base_sha.clone(),
            ),
            ("org.opencontainers.image.version", facts.app_version()),
            (
                "org.opencontainers.image.created",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ),
        ]);
        labels
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    /// `podman build`, streamed, in a scratch directory of the run's own
    /// ([`Self::prepare_scratch`]) that is removed afterwards however the
    /// build ends. The image ID on success.
    async fn build(
        &mut self,
        p: &Prepared,
        choice: &DockerfileChoice,
        resolved: &ResolvedInputs,
        cfg: &str,
        tag: &str,
        worktree: &Path,
    ) -> Result<String, Stop> {
        let instance = self.state().builds.instance_id().to_string();
        let scratch = paths::run_tmp_dir(&self.builds_dir, &instance, self.run.id);
        self.prepare_scratch(&scratch)?;
        let built = self
            .build_in(p, choice, resolved, cfg, tag, worktree, &scratch)
            .await;
        match remove_tree(&self.podman, &scratch).await {
            Ok(()) => {}
            Err(e) => self
                .log
                .line(&format!("the build's scratch dir was not removed: {e}")),
        }
        built
    }

    /// Create the run's `TMPDIR`, `<builds_dir>/tmp/run-<instance>-<id>`
    /// (§14.2): buildah puts its `buildahNNN` scratch directory — what an
    /// interrupted build leaves — and its blob downloads under `$TMPDIR`, so
    /// the leftovers are exactly this directory, never another build's in
    /// `/var/tmp`. buildah also roots its cache mounts at
    /// `$TMPDIR/buildah-cache-<uid>` (measured with podman 5.8, 2026-09-26),
    /// so that name is a symlink to the real cache root under `/var/tmp`:
    /// ccache stays shared across runs and survives the directory's removal
    /// (a removal never follows a symlink).
    fn prepare_scratch(&self, scratch: &Path) -> Result<(), String> {
        std::fs::create_dir_all(scratch)
            .map_err(|e| format!("could not create {}: {e}", scratch.display()))?;
        let root = cache_mount_root(&self.state().builds.buildah_tmp(), real_uid());
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("could not create {}: {e}", root.display()))?;
        let link = scratch.join(root.file_name().unwrap_or_default());
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(&root, &link).map_err(|e| {
                format!(
                    "could not link {} to buildah's cache root {}: {e}",
                    link.display(),
                    root.display()
                )
            })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_in(
        &mut self,
        p: &Prepared,
        choice: &DockerfileChoice,
        resolved: &ResolvedInputs,
        cfg: &str,
        tag: &str,
        worktree: &Path,
        scratch: &Path,
    ) -> Result<String, Stop> {
        let state = self.state();
        let labels = self.labels(resolved, cfg, &p.facts);
        let argv = podman::build_argv(&BuildCommand {
            target: &choice.target,
            containerfile: &p.containerfile,
            ignorefile: &p.ignorefile,
            keep_layers: self.spec.keep_layers,
            pull_newer: self.rebuild,
            cpus: self.spec.cpus.as_deref(),
            build_args: &p.build_args,
            labels: &labels,
            iidfile: &p.iidfile,
            tag,
            context: worktree,
        });
        let env = vec![("TMPDIR".to_string(), scratch.display().to_string())];
        self.log.line(&format!(
            "$ TMPDIR={} podman {}",
            scratch.display(),
            argv.join(" ")
        ));
        let before = match Leftovers::snapshot(&self.podman).await {
            Ok(s) => Some(s),
            Err(e) => {
                self.log.line(&format!(
                    "note: the working containers an interrupted build leaves cannot be told \
                     apart from what was there ({e}); none would be removed"
                ));
                None
            }
        };
        let spawned = state
            .agent_spawner()
            .spawn_env("podman", &argv, &env)
            .await
            .map_err(|e| format!("podman build could not be started: {e}"))?;
        let crate::agents::container::Spawned {
            mut stdout,
            mut stderr,
            mut status,
            kill,
        } = spawned;
        let (mut out_open, mut err_open) = (true, true);
        let mut exit: Option<std::io::Result<Exit>> = None;
        let mut exited_at: Option<Instant> = None;
        let mut tick = tokio::time::interval(CANCEL_POLL);
        loop {
            if exit.is_some() && !out_open && !err_open {
                break;
            }
            // Unbiased on purpose: a build printing without pause must not
            // starve the cancel check (the two pipes were never ordered
            // against each other anyway; each stays in order).
            tokio::select! {
                line = stdout.recv(), if out_open => match line {
                    Some(l) => self.note_line(&l).await,
                    None => out_open = false,
                },
                line = stderr.recv(), if err_open => match line {
                    Some(l) => self.note_line(&l).await,
                    None => err_open = false,
                },
                ended = &mut status, if exit.is_none() => {
                    exit = Some(ended);
                    exited_at = Some(Instant::now());
                }
                _ = tick.tick() => {
                    if exit.is_none() && self.ctx.canceled() {
                        self.log.line("cancel requested — SIGTERM to podman build");
                        let ended = terminate(&kill, &mut status, CANCEL_GRACE).await;
                        self.drain(&mut stdout, &mut stderr).await;
                        self.log.line(&format!("podman build ended: {}", describe_exit(&ended)));
                        self.sweep_leftovers(before.as_ref()).await;
                        // The build may have finished in the moment the
                        // cancel went out: an image no run will own.
                        if let Some(id) = read_iidfile(&p.iidfile) {
                            self.discard_image(&id, tag).await;
                        }
                        return Err(Stop::Canceled);
                    }
                    if exited_at.is_some_and(|t| t.elapsed() > PIPE_DRAIN_GRACE) {
                        self.log.line(&format!(
                            "podman build exited, but its output was still open {}s later — \
                             stopped reading it",
                            PIPE_DRAIN_GRACE.as_secs()
                        ));
                        break;
                    }
                }
            }
        }
        let ended = exit.expect("the loop only ends once the build has exited");
        if !matches!(ended, Ok(Exit::Code(0))) {
            let why = format!(
                "podman build {} — the log has its output",
                describe_exit(&ended)
            );
            self.sweep_leftovers(before.as_ref()).await;
            return Err(Stop::Failed(why));
        }
        match read_iidfile(&p.iidfile) {
            Some(id) => Ok(id),
            None => self.podman.image_id(tag).await?.ok_or_else(|| {
                Stop::Failed(format!(
                    "podman build succeeded but wrote no image ID, and {tag} does not exist"
                ))
            }),
        }
    }

    /// Remove the image a canceled build still produced (it finished as the
    /// cancel went out): it carries only this run's `-r<run>` tag and no run
    /// will record it. Removed by that tag — the last name, so podman takes
    /// the image with it — and only while nothing uses it and it has no
    /// other name; otherwise it is kept, and the log says why.
    async fn discard_image(&self, id: &str, tag: &str) {
        let names = match self.podman.repo_tags(id).await {
            Ok(n) => n,
            Err(_) => return, // gone already
        };
        if names.iter().any(|n| n != tag) {
            self.log.line(&format!(
                "the image the build finished as it was canceled ({}) is kept: it also has the \
                 name(s) {}",
                short_id(id),
                names.join(", ")
            ));
            return;
        }
        match UsageIndex::load(&self.state()).await {
            Ok(u) if !u.users_of(id).is_empty() => {
                self.log.line(&format!(
                    "the image the build finished as it was canceled ({}) is kept: in use by {}",
                    short_id(id),
                    describe_users(&u.users_of(id))
                ));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.log.line(&format!(
                    "the image the build finished as it was canceled ({}) is kept: could not \
                     tell whether it is in use ({e})",
                    short_id(id)
                ));
                return;
            }
        }
        match self.podman.rmi(&[tag]).await {
            Ok(_) => self.log.line(&format!(
                "removed the image the build finished as it was canceled ({tag}, {})",
                short_id(id)
            )),
            Err(e) => self.log.line(&format!(
                "the image the build finished as it was canceled ({tag}) could not be removed: \
                 {e}"
            )),
        }
    }

    /// Log what is still in the pipes of a build that has ended — its last
    /// words, which the log must not lose — until both close, or for
    /// [`PIPE_DRAIN_GRACE`] at most (and then say so).
    async fn drain(
        &self,
        stdout: &mut tokio::sync::mpsc::Receiver<String>,
        stderr: &mut tokio::sync::mpsc::Receiver<String>,
    ) {
        let deadline = tokio::time::sleep(PIPE_DRAIN_GRACE);
        tokio::pin!(deadline);
        let (mut out_open, mut err_open) = (true, true);
        while out_open || err_open {
            tokio::select! {
                line = stdout.recv(), if out_open => match line {
                    Some(l) => self.log.line(&l),
                    None => out_open = false,
                },
                line = stderr.recv(), if err_open => match line {
                    Some(l) => self.log.line(&l),
                    None => err_open = false,
                },
                _ = &mut deadline => {
                    self.log.line(&format!(
                        "podman build's output was still open {}s after it ended — stopped \
                         reading it",
                        PIPE_DRAIN_GRACE.as_secs()
                    ));
                    return;
                }
            }
        }
    }

    /// Remove the buildah working containers this build left behind (§14.2),
    /// under the lock: the ones that appeared while it ran
    /// ([`Leftovers::new_since`]) — nothing else. Its scratch directory goes
    /// with the run's `TMPDIR` ([`Self::build`]).
    async fn sweep_leftovers(&mut self, before: Option<&Leftovers>) {
        let Some(before) = before else {
            self.log
                .line("working containers not swept: there was no snapshot to compare against");
            return;
        };
        let now = match Leftovers::snapshot(&self.podman).await {
            Ok(n) => n,
            Err(e) => {
                self.log.line(&format!("working containers not swept: {e}"));
                return;
            }
        };
        let new = before.new_since(&now);
        if new.containers.is_empty() {
            self.log.line("the build left no working container behind");
            return;
        }
        let ids: Vec<String> = new.containers.iter().map(|c| c.id.clone()).collect();
        let names = new
            .containers
            .iter()
            .map(|c| format!("{} ({})", c.name, short_id(&c.id)))
            .collect::<Vec<_>>()
            .join(", ");
        match self.podman.rm_force(&ids).await {
            Ok(()) => self.log.line(&format!(
                "removed the build's working container(s): {names}"
            )),
            Err(e) => self.log.line(&format!(
                "could not remove the build's working container(s) {names}: {e}"
            )),
        }
    }

    async fn finish(
        self,
        status: BuildRunStatus,
        error: Option<String>,
    ) -> Result<JobOutcome, String> {
        self.log
            .line(&format!("run {} ended: {}", self.run.id, status.as_str()));
        if let Err(e) =
            store::finish_build_run(&self.ctx.state.db, self.run.id, status, error.as_deref()).await
        {
            tracing::warn!("recording the end of build run {}: {e}", self.run.id);
        }
        let value = json!({
            "run_id": self.run.id,
            "build_id": self.detail.build_id,
            "status": status.as_str(),
        });
        Ok(match status {
            BuildRunStatus::Canceled => JobOutcome::Canceled,
            BuildRunStatus::Failed | BuildRunStatus::Broken => JobOutcome::FailedWith {
                error: error.unwrap_or_else(|| status.as_str().to_string()),
                value,
            },
            _ => JobOutcome::Done(value),
        })
    }
}

/// Where run `run`'s log is: the path it recorded, else where this instance
/// would have put it.
pub(crate) fn log_path_of(state: &AppState, run: &BuildRun) -> PathBuf {
    run.log_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::log_path(&state.builds_dir(), state.builds.instance_id(), run.id))
}

/// The image ID a `podman build --iidfile` wrote, if it wrote one.
fn read_iidfile(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().trim_start_matches("sha256:").to_string())
        .filter(|s| !s.is_empty())
}

fn describe_exit(ended: &std::io::Result<Exit>) -> String {
    match ended {
        Ok(Exit::Code(n)) => format!("exited with status {n}"),
        Ok(Exit::Signal(s)) => format!("was killed by signal {s}"),
        Err(e) => format!("could not be waited for: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Whether an entry of `<builds_dir>/work` (or `<builds_dir>/tmp`) is a
/// leftover of **this** instance's: a run's worktree, build context or
/// scratch dir (`run-<instance>-<id>[.ctx]`, [`paths::is_own_run_dir`]), or a
/// Check merge tree of this instance whose process is gone
/// (`check-<instance>-<pid>-<millis>-<n>`; checks do not take the build
/// lock, so a live one is left alone). Another instance's entries, and
/// anything named otherwise — old-format `<id>` dirs included, whose owner
/// cannot be told — are never touched.
fn stale_work_entry(name: &str, instance: &str) -> bool {
    if paths::is_own_run_dir(name, instance) {
        return true;
    }
    let Some(rest) = name
        .strip_prefix("check-")
        .and_then(|r| r.strip_prefix(instance))
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let pid = rest.split('-').next().and_then(|p| p.parse::<i32>().ok());
    match pid {
        // SAFETY: `kill(pid, 0)` sends nothing; it only asks whether the
        // process exists.
        Some(pid) if pid > 0 && !instance.is_empty() => (unsafe { libc::kill(pid, 0) }) != 0,
        _ => false,
    }
}

/// Remove the tree at `dir`: plainly, on a blocking thread (a worktree is
/// gigabytes), then — for what buildah's subordinate UIDs own, which only
/// the user namespace can delete — with `podman unshare rm -rf`. A symlink
/// in it is removed, never followed (a run's scratch dir links buildah's
/// shared cache root, which must survive it).
async fn remove_tree(podman: &Podman, dir: &Path) -> Result<(), String> {
    let owned = dir.to_path_buf();
    let plain = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&owned))
        .await
        .map_err(|e| format!("removing {}: {e}", dir.display()))?;
    match plain {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(plain_err) => podman.unshare_rm(dir).await.map_err(|e| {
            format!(
                "could not remove {}: {plain_err}, and podman unshare rm -rf failed too: {e}",
                dir.display()
            )
        }),
    }
}

/// The boot sweep (§5 "At boot"): the worktrees, build contexts and scratch
/// dirs of this instance's runs a shutdown interrupted (their rows were
/// failed by `store::fail_orphaned_build_runs`) and its dead Check merge
/// trees are removed ([`stale_work_entry`]), and the pool's worktree
/// registrations pruned — including the ones a killed `worktree add` left
/// locked. Only while no build runs anywhere on the machine: the lock is
/// taken without waiting, and a held one skips the sweep (the next boot
/// does it).
pub async fn boot_sweep(state: &AppState) {
    let builds_dir = state.builds_dir();
    let instance = state.builds.instance_id().to_string();
    let dirs = [builds_dir.join("work"), builds_dir.join("tmp")];
    if !dirs.iter().any(|d| d.is_dir()) {
        return;
    }
    let lock_path = state.builds.lock_path();
    let lock = match BuildLock::try_acquire(&lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            tracing::info!(
                "build boot sweep skipped: {} holds the build lock",
                lock::read_holder(&lock_path)
                    .map(|h| h.describe())
                    .unwrap_or_else(|| "another build".into())
            );
            return;
        }
        Err(e) => {
            tracing::warn!("build boot sweep skipped: {e}");
            return;
        }
    };
    let podman = Podman::of(state);
    let mut removed = 0;
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !stale_work_entry(&name, &instance) {
                continue;
            }
            match remove_tree(&podman, &entry.path()).await {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!("build boot sweep: {e}"),
            }
        }
    }
    if Pool::exists(&builds_dir) {
        match Pool::open(Git::new(), &builds_dir).await {
            Ok(pool) => {
                let own = |worktree: &Path| {
                    worktree
                        .file_name()
                        .is_some_and(|n| stale_work_entry(&n.to_string_lossy(), &instance))
                };
                if let Err(e) = pool.prune_stale(own).await {
                    tracing::warn!("build boot sweep: git worktree prune: {e}");
                }
            }
            Err(e) => tracing::warn!("build boot sweep: {e}"),
        }
    }
    drop(lock);
    if removed > 0 {
        tracing::info!(
            "build boot sweep: removed {removed} stale worktree(s)/build context(s)/scratch \
             dir(s) of instance {instance} under {}",
            builds_dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_falls_back_to_a_per_user_path_every_instance_shares() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            default_lock_path(Some(dir.path().to_path_buf()), 1000),
            dir.path().join(LOCK_FILE)
        );
        for missing in [
            None,
            Some(PathBuf::from("relative")),
            Some(dir.path().join("gone")),
        ] {
            assert_eq!(
                default_lock_path(missing, 1000),
                PathBuf::from("/tmp/lmgw-build-1000.lock"),
                "never under a builds dir, which differs between instances"
            );
        }
    }

    #[test]
    fn stale_work_entries_are_this_instances_runs_and_dead_checks() {
        let i = "ab12cd34";
        assert!(stale_work_entry("run-ab12cd34-12", i));
        assert!(stale_work_entry("run-ab12cd34-12.ctx", i));
        assert!(
            !stale_work_entry("run-ffffffff-12", i),
            "another instance's"
        );
        assert!(!stale_work_entry("12", i), "old format: whose is unknown");
        assert!(!stale_work_entry("12.ctx", i));
        assert!(!stale_work_entry(".ctx", i));
        assert!(!stale_work_entry("notes", i));
        assert!(
            !stale_work_entry("check-ab12cd34-0-1-2", i),
            "pid 0 is never ours"
        );
        let me = std::process::id();
        assert!(
            !stale_work_entry(&format!("check-ab12cd34-{me}-1-2"), i),
            "alive"
        );
        assert!(
            stale_work_entry("check-ab12cd34-2147483646-1-2", i),
            "no such pid"
        );
        assert!(
            !stale_work_entry("check-ffffffff-2147483646-1-2", i),
            "another instance's check"
        );
        assert!(!stale_work_entry("check-2147483646-1-2", i), "old format");
    }
}
