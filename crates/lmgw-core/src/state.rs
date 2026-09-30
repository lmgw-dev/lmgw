//! Shared application state (`Arc<AppState>`), used by HTTP handlers and the
//! Tauri tray alike — no internal HTTP hop (§3).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use sqlx::SqlitePool;

use crate::audio::CatalogSnapshot;
use crate::catalog::CatalogCache;
use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::gguf::GgufSummaryCache;
use crate::jobs::JobManager;
use crate::mcp::McpManager;
use crate::runtime;
use crate::store;
use crate::telemetry::TelemetryBus;
use crate::vram::VramScheduler;

/// Per-`init_for_tests` counter behind the unique `data_dir` each test gateway
/// gets.
static NEXT_TEST_DIR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub struct AppState {
    pub db: SqlitePool,
    /// The quickdoc corpus DB (quickdoc design §3): a **separate file** with its
    /// own pool and its own migrations directory. It holds no secrets, so
    /// nuking and re-ingesting it can never touch gateway config, and one
    /// `rsync` of the file is a full backup.
    pub corpus: SqlitePool,
    /// The knowledge bases (chat-complete design §9.1): `knowledge.db`, a
    /// separate **0600** file with its own migrations — the owner's private
    /// documents, never the world-readable corpus file — and each base's
    /// vectors once resident.
    pub knowledge: crate::knowledge::Knowledge,
    snapshot: ArcSwap<Snapshot>,
    pub telemetry: TelemetryBus,
    /// Per-key scope / rate / budget enforcement (usage-analytics §4). The
    /// in-memory half: rolling request and token windows, in-flight counts, and
    /// the cached period spend that keeps the budget check off the DB on the
    /// request path.
    pub policy: std::sync::Arc<crate::policy::PolicyGate>,
    /// Latest fetched audio.cpp model-spec catalog (also persisted in the kv
    /// store; populated on first Audio-tab load or explicit refresh).
    pub audio_catalog: std::sync::Mutex<Option<CatalogSnapshot>>,
    /// Background jobs (§9c): HF downloads today, ingestion / re-embedding /
    /// eval runs as those land. Owns the executor registry and the live
    /// progress of everything currently running.
    pub jobs: JobManager,
    /// The rows of the agent runs currently in flight (agent-catalog §3).
    ///
    /// Deliberately not on the jobs feed: fifty rows of raw model replies on
    /// every 500 ms frame to every open dashboard tab is the wrong pipe. They
    /// are read by `GET /api/agents/runs/{id}`, which the Run tab polls only
    /// when the run's `done` moves.
    pub agent_runs: crate::agents::batch::RunBuffer,
    /// Runs opened through the ledger (container-runtime §3.2) — the ones lmgw
    /// is not driving. Here rather than in a `static` for the same reason
    /// [`Self::agent_runs`] is: two gateways in one test process must not see
    /// each other's runs through colliding job ids.
    pub agent_ledger: crate::agents::ledger::Desk,
    /// Login nonces this gateway has minted and not yet seen spent
    /// (principals design §3.4). Here rather than in a `static` for
    /// [`Self::agent_ledger`]'s reason: a nonce is a credential for whichever
    /// gateway answers it, and two gateways in one test process must not
    /// exchange each other's.
    pub login_nonces: crate::web::session::LoginNonces,
    /// What each open run has spent on traffic that stamped itself with
    /// `X-Lmgw-Run` (§3.1). An in-process run meters itself; a container's own
    /// `/v1` and `/mcp` calls arrive as ordinary requests and are folded in
    /// here, so cost per run works for every caller mode.
    pub agent_meters: crate::agents::RunMeters,
    /// Service containers this gateway has running, one entry per agent
    /// (container-runtime §6.5). Beside the model registry's map rather than in
    /// it: the key is an agent id, not a `(class, model)`, and the start goes
    /// through the agent spawner seam.
    pub agent_services: crate::agents::service::Services,
    /// Southbound MCP connections (§9): the tool-plane analogue of the
    /// container runtime. Live `rmcp` peers, reconciled against the snapshot
    /// on every reload.
    pub mcp: McpManager,
    pub http: reqwest::Client,
    /// The client the service-mode proxy and its health probe use
    /// (container-runtime §3.3), and the **only** thing that distinguishes it
    /// from [`Self::http`]: `redirect::Policy::none()`.
    ///
    /// reqwest follows up to ten redirects by default and does not confine them
    /// to the origin it started on. Through a reverse proxy that is two bugs at
    /// once: every 3xx the agent's app returns is collapsed into whatever the
    /// final hop said (a login redirect becomes the login page at the original
    /// URL), and a `Location:` the container chooses turns lmgw into an
    /// arbitrary-URL fetcher whose answer is relayed on the dashboard's own
    /// origin. A proxy forwards a 3xx; it does not follow it.
    pub proxy_http: reqwest::Client,
    pub catalog: CatalogCache,
    /// GGUF header cache (model capabilities design §3.5): one entry per
    /// absolute path, validated against the file's `(len, mtime)` on every
    /// hit. Keeps `/v1/models` cheap — the alternative is a header read per
    /// local row on every call.
    pub gguf_cache: GgufSummaryCache,
    /// GPU admission control (quickdoc §9b, per-model-containers §4): the VRAM
    /// ledger, the eviction policy, and the queue requests wait in. The plane
    /// that *decides*; [`Self::runtime`] is the plane that *acts*.
    pub vram: VramScheduler,
    /// The token ledger of every guarded unified-KV chat model (unified-KV
    /// design §3.3): what each shared KV pool has reserved for the requests in
    /// flight, and who is queued for room. Inside a container that
    /// [`Self::vram`] already admitted — the two never wait on each other (see
    /// [`crate::gate::pool`]).
    pub kv_pools: crate::gate::pool::PoolLedger,
    /// What lmgw believes is running, one container per model (§3.2).
    ///
    /// Behind an [`ArcSwap`] for exactly the reason [`VramScheduler`]'s probe
    /// is behind an `RwLock`: production installs the one that shells real
    /// `podman`, and a test installs its own fake in its place. Read through
    /// [`Self::runtime`], never cloned out at startup and cached — a stale
    /// handle would drive a registry nobody else can see.
    runtime: ArcSwap<runtime::registry::Registry>,
    /// How a container agent reaches `podman` (container-runtime §6.1).
    ///
    /// A second, streaming seam beside [`Self::runtime`]'s buffered one,
    /// swappable for the same reason: production installs the one that shells a
    /// real `podman`, a test installs a fake that replays scripted
    /// stdout/stderr/exit. A `Mutex` rather than an `ArcSwap` because it is read
    /// once per run, not per request.
    agent_spawner: std::sync::Mutex<Arc<dyn crate::agents::container::Spawner>>,
    pub started_at: Instant,
    /// Wall-clock twin of [`Self::started_at`]: the same moment, expressed as
    /// a timestamp instead of a monotonic reading.
    ///
    /// [`Instant`] is the right type for "how long have we been up" (it cannot
    /// jump when the clock is stepped) and the wrong one for "publish a unix
    /// second", which is what `/v1/models` needs: `created` must be *stable
    /// per process* for every row that has no timestamp of its own (model
    /// capabilities design §2.1), and today's handler stamps `now()` on every
    /// call, so a client diffing two listings sees every model recreated.
    pub started_at_utc: chrono::DateTime<chrono::Utc>,
    pub data_dir: PathBuf,
    /// Serializes every read-modify-write of the whole `Settings` blob
    /// (`store::save_settings`'s callers): clone the snapshot's settings,
    /// mutate the copy, save, `reload_snapshot()` — all under this lock.
    ///
    /// Nothing else guards that round trip, so two writers racing (a
    /// dashboard save and `ops::hold_set`, say) could each read the
    /// pre-mutation blob and the second save would silently revert the
    /// first's change — including `hold.active` after the hold sweep has
    /// already stopped every container, which is the failure that made this
    /// worth fixing (gpu-hold design §3.1).
    pub settings_write: tokio::sync::Mutex<()>,
    /// This process is a **dev instance** (container-builds §10): set from
    /// `LMGW_DEV` at [`AppState::init`] (which `scripts/dev-instance.sh`
    /// exports), and always by [`AppState::init_with`]`(…, true)` — the
    /// headless runner's. Explicit rather than inferred from the data dir, because the
    /// things it guards are shared with production no matter where the data
    /// dir is — the podman image store above all. Read through
    /// [`Self::dev`]; an `AtomicBool` only so a test can flip it
    /// ([`Self::set_dev_for_tests`]), nothing in production writes it after
    /// boot.
    dev: std::sync::atomic::AtomicBool,
    /// What the container-build executor reaches through the gateway rather
    /// than hardcodes (container-builds §5, §7): the forge client that answers
    /// "is this PR merged upstream?" — a no-op until one is installed — and,
    /// for tests only, where the machine-wide build lock and buildah's scratch
    /// directories are.
    pub builds: crate::backends::run::BuildSeams,
    /// The benchmark run in flight, if any, and the seams a test replaces
    /// (benchmark design §3, §9): how a run reaches podman and a port, and
    /// the suite's timing. The run's GPU lease itself is on the snapshot
    /// ([`Self::set_gpu_lease`]).
    pub bench: crate::bench::BenchState,
    /// The Chat's temporary threads (chat-complete design §7): never written
    /// to the DB, gone when discarded or when this process exits. Here rather
    /// than in a `static` for [`Self::agent_ledger`]'s reason.
    pub(crate) chat_temp: crate::web::chat_temp::TempChats,
    /// Each Chat thread's live turn and history generation (review R1
    /// finding 1): at most one turn answers a thread, and a reply is saved
    /// only onto the history it answered. In-process, like the workers it
    /// governs.
    pub(crate) chat_live: crate::web::chat_live::LiveTurns,
}

pub type SharedState = Arc<AppState>;

/// Whether `LMGW_DEV` marks this process as a dev instance. Unset, empty,
/// `0`, `false`, `no` and `off` mean no; **anything else means yes**. What
/// the flag guards is destructive in production's shared image store, so a
/// value nobody anticipated (`LMGW_DEV=yes please`) errs on the side of the
/// dev instance refusing, never on the side of it deleting.
pub fn dev_from_env() -> bool {
    dev_flag(std::env::var("LMGW_DEV").ok().as_deref())
}

fn dev_flag(v: Option<&str>) -> bool {
    match v.map(|v| v.trim().to_ascii_lowercase()) {
        None => false,
        Some(v) => !matches!(v.as_str(), "" | "0" | "false" | "no" | "off"),
    }
}

/// XDG data dir for lmgw (`~/.local/share/lmgw`).
pub fn default_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("lmgw");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/lmgw")
}

impl AppState {
    /// The gateway on `data_dir`, a dev instance when `LMGW_DEV` says so
    /// ([`dev_from_env`]) — what the tray app starts.
    pub async fn init(data_dir: PathBuf) -> anyhow::Result<SharedState> {
        Self::init_with(data_dir, dev_from_env()).await
    }

    /// [`Self::init`] with the dev-instance flag given rather than read from
    /// `LMGW_DEV`: the headless runner (`examples/headless.rs`) is never
    /// production, and passes `true` whatever the environment says, so a
    /// dev gateway can never come up without its guards (no image deletion
    /// or pruning, its own tag namespace, its own builds dir).
    pub async fn init_with(data_dir: PathBuf, dev: bool) -> anyhow::Result<SharedState> {
        let db = store::open(&data_dir.join("lmgw.sqlite")).await?;
        let corpus = quickdoc_core::store::open(&data_dir.join(crate::quickdoc::CORPUS_DB_FILE))
            .await
            .map_err(|e| anyhow::anyhow!("opening the corpus DB: {e}"))?;
        let knowledge =
            crate::knowledge::store::open(&data_dir.join(crate::knowledge::KNOWLEDGE_DB_FILE))
                .await
                .map_err(|e| anyhow::anyhow!("opening the knowledge DB: {e}"))?;
        // Files a crash left mid-ingest: nothing is running them now.
        match crate::knowledge::store::reset_ingesting(&knowledge, None).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "{n} knowledge-base file(s) interrupted mid-ingest are pending again"
            ),
            Err(e) => tracing::warn!("resetting interrupted knowledge-base files: {e}"),
        }
        // Nothing can be running in a process that has just opened the DB, so
        // any job row still claiming to be is residue from the last shutdown.
        // Closing them here — before anything can spawn — also releases the
        // `(kind, key)` guard they would otherwise hold against a re-run.
        match store::fail_orphaned_jobs(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("closed {n} job(s) interrupted by the last shutdown"),
            Err(e) => tracing::warn!("closing interrupted jobs: {e}"),
        }
        // The build runs those jobs were executing (container-builds §5):
        // same residue, same reason, and the run row is what the Backends
        // page shows, so it must not claim to be building forever.
        match store::fail_orphaned_build_runs(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("closed {n} build run(s) interrupted by the last shutdown"),
            Err(e) => tracing::warn!("closing interrupted build runs: {e}"),
        }
        // And the benchmark runs (benchmark design §3.3): `interrupted`, with
        // the phases they finished kept. Their containers are collected by
        // `lifecycle::boot`, before it reconciles.
        match store::interrupt_running_bench_runs(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("marked {n} benchmark run(s) interrupted by the last shutdown"),
            Err(e) => tracing::warn!("closing interrupted benchmark runs: {e}"),
        }
        // Before anything names a build file or container (container-builds
        // §5): the id that keeps this data dir's runs apart from another
        // instance's in the builds dir and the image store they share.
        let instance = crate::backends::run::load_instance_id(&db, &data_dir).await;
        if dev {
            tracing::info!(
                "dev instance (LMGW_DEV): image deletion and keep_runs pruning are refused — the \
                 podman image store is shared with production"
            );
        }
        let snapshot = store::load_snapshot(&db)
            .await
            .map_err(|e| anyhow::anyhow!("loading config snapshot: {e}"))?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let proxy_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let registry = runtime::registry::Registry::new(
            Arc::new(runtime::registry::TokioRunner),
            http.clone(),
        );
        let app = Arc::new(Self {
            db,
            corpus,
            knowledge: crate::knowledge::Knowledge::new(knowledge),
            snapshot: ArcSwap::from_pointee(snapshot),
            telemetry: TelemetryBus::new(),
            policy: std::sync::Arc::new(crate::policy::PolicyGate::default()),
            audio_catalog: std::sync::Mutex::new(None),
            jobs: JobManager::new(),
            agent_runs: Default::default(),
            agent_ledger: Default::default(),
            login_nonces: Default::default(),
            agent_meters: Default::default(),
            agent_services: Default::default(),
            mcp: McpManager::new(),
            http,
            proxy_http,
            catalog: CatalogCache::default(),
            gguf_cache: GgufSummaryCache::default(),
            vram: VramScheduler::new(),
            kv_pools: Default::default(),
            runtime: ArcSwap::from_pointee(registry),
            agent_spawner: std::sync::Mutex::new(Arc::new(crate::agents::container::TokioSpawner)),
            started_at: Instant::now(),
            started_at_utc: chrono::Utc::now(),
            data_dir,
            settings_write: tokio::sync::Mutex::new(()),
            dev: std::sync::atomic::AtomicBool::new(dev),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_live: Default::default(),
        });
        app.builds.set_instance_id(instance);
        // Break the handler↔AppState cycle now that the `Arc` exists: the
        // `McpManager` holds a `Weak<AppState>` it hands to each sampling
        // handler, upgraded per call (§9/§19).
        app.mcp.set_state(&app);
        // The real forge behind a build run's "is this PR merged upstream?"
        // (container-builds §5 phase 1, §14.1), in place of the no-op the
        // seams start with. It reads the forge tokens from the settings on
        // every call, so it is installed once and never refreshed. Not in
        // `init_for_tests`: a test must not reach github.com — one that wants
        // the real lookup installs it against a mock.
        app.builds
            .set_forge(Arc::new(crate::backends::forge::GatewayForge::new(&app)));
        // The two owner rows (principals §6), before anything else reads the
        // snapshot: `resync_all` below and the shell's window both want a
        // gateway that already has its door key.
        crate::agents::token::seed_owner_keys(&app).await?;
        // The shipped agents, after migrations and before anything can serve a
        // request (agent-catalog §3). Idempotent, and it never resurrects a
        // built-in the owner deleted — see `agents::seed`.
        crate::agents::seed::seed(&app).await;
        // And each agent's own `agent:<id>` MCP row, re-pointed at the current
        // bind address (container-runtime §3.3). The row's url is written when
        // the manifest is; a `bind_addr` this build boots with that differs
        // from the one the manifest was last saved under would otherwise leave
        // every agent's tools pointing at a dead port, permanently.
        crate::agents::service::resync_all(&app).await;
        // And every stored `dev_url` re-checked against the rules as they
        // stand (origins §4.7). Until now this ran only on a `bind_addr`
        // change; a row written before a dev_url became an origin may carry a
        // path, which nothing strips any more, so the question is asked once
        // at boot and the offender is cleared with its reason rather than
        // proxied to a prefix its dev server never sees.
        let cleared =
            crate::agents::service::revalidate_dev_urls(&app, &app.snapshot().settings.bind_addr)
                .await;
        for (id, url, why) in &cleared {
            tracing::warn!("agent '{id}': the dev server override {url} was cleared — {why}");
        }
        // And the stored `agent_origin_suffix` asked the same question the
        // settings plane asks on write (origins §4.1). It is validated when it
        // is typed, but a `bind_addr` change, a new host name or a new search
        // domain moves the other side of that rule, and a suffix that was fine
        // when it was saved can shadow the dashboard by the next boot. Said
        // out loud, here and on the settings page
        // (`agent_origin_suffix_warning`), and never quietly reset: the owner
        // set that value, and a gateway that renamed every agent's origin
        // behind their back would be the worse surprise.
        if let Some(why) = crate::agents::service::origin_suffix_warning(&app.snapshot().settings) {
            tracing::warn!(
                "agent origin suffix: {why} It is still in effect — change it under Settings → \
                 Agents & tools."
            );
        }
        // Boot reconciliation for agent containers (container-runtime §6.4).
        // `fail_orphaned_jobs` above has already failed every run a restart
        // interrupted, so every container carrying `lmgw.kind=agent` is by
        // definition a leftover — an agent container is never adopted, only
        // collected.
        //
        // Spawned rather than awaited, for the reason `lifecycle::boot` is
        // (`server.rs`): `podman ps` is unbounded wall clock on a box where
        // podman is slow or absent, and holding up `init` for it would make a
        // crash-recovery detail a startup-availability problem.
        let st = app.clone();
        tokio::spawn(async move {
            let report = crate::agents::container::reconcile(&st).await;
            for e in &report.errors {
                tracing::warn!("agent container reconciliation: {e}");
            }
            if !report.removed.is_empty() || !report.swept_dirs.is_empty() {
                tracing::info!(
                    "agent container reconciliation: removed {} container(s), swept {} run \
                     director(ies)",
                    report.removed.len(),
                    report.swept_dirs.len()
                );
            }
        });
        // The build runs `fail_orphaned_build_runs` closed above left their
        // worktrees and build contexts behind (container-builds §5 "At
        // boot"). Spawned for the reason the reconciliation above is: git
        // is unbounded wall clock, and a stale directory is not a startup
        // problem. The sweep takes the machine-wide build lock first and
        // skips itself while another instance holds it.
        let st = app.clone();
        tokio::spawn(async move {
            crate::backends::run::boot_sweep(&st).await;
        });
        Ok(app)
    }

    /// In-memory state for tests.
    pub async fn init_for_tests() -> anyhow::Result<SharedState> {
        let db = store::open_in_memory().await?;
        let corpus = quickdoc_core::store::open_in_memory()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let knowledge = crate::knowledge::store::open_in_memory().await?;
        let snapshot = store::load_snapshot(&db)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // A directory of its own per gateway. Two tests in one process both
        // resolve `container_prefix`-scoped run directories under it, and boot
        // reconciliation now sweeps *every* prefix under this root (agent
        // container runtime §6.4) — sharing one would let one test collect
        // another's secrets files mid-assertion.
        let dir = std::env::temp_dir().join(format!(
            "lmgw-test-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let instance = crate::backends::run::load_instance_id(&db, &dir).await;
        let app = Arc::new(Self {
            db,
            corpus,
            knowledge: crate::knowledge::Knowledge::new(knowledge),
            snapshot: ArcSwap::from_pointee(snapshot),
            telemetry: TelemetryBus::new(),
            policy: std::sync::Arc::new(crate::policy::PolicyGate::default()),
            audio_catalog: std::sync::Mutex::new(None),
            jobs: JobManager::new(),
            agent_runs: Default::default(),
            agent_ledger: Default::default(),
            login_nonces: Default::default(),
            agent_meters: Default::default(),
            agent_services: Default::default(),
            mcp: McpManager::new(),
            http: reqwest::Client::new(),
            proxy_http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with no redirect policy always builds"),
            catalog: CatalogCache::default(),
            gguf_cache: GgufSummaryCache::default(),
            // Deliberately never the real NVML: a test must not read the
            // machine's actual GPU (and must not behave differently on a box
            // that has one). This is the degraded path, which is also what a
            // GPU-less host runs. A test that wants a GPU installs its own with
            // `state.vram.set_probe(...)`.
            vram: VramScheduler::with_probe(Arc::new(crate::vram::nvml::NoTelemetry(
                "test state: no GPU probe installed".into(),
            ))),
            kv_pools: Default::default(),
            // Deliberately never the real podman, for the same reason the probe
            // above is never the real NVML: a test must not start containers on
            // the machine it runs on. Every `podman` verb this registry would
            // shell fails loudly instead, so a test that reaches the container
            // lifecycle without meaning to says so in its error rather than
            // bouncing a dev box's GPU. A test that wants the lifecycle installs
            // its own fake with `set_runtime_for_tests`.
            runtime: ArcSwap::from_pointee(runtime::registry::Registry::new(
                Arc::new(runtime::registry::NoRuntime(
                    "test state: no container runtime installed (see \
                     AppState::set_runtime_for_tests)"
                        .into(),
                )),
                reqwest::Client::new(),
            )),
            // Never the real podman, for the same reason the registry above is
            // not: a test must not start containers on the machine it runs on.
            // A test that wants the runner installs its own with
            // `set_agent_spawner_for_tests`.
            agent_spawner: std::sync::Mutex::new(Arc::new(crate::agents::container::NoSpawner(
                "test state: no container spawner installed (see \
                 AppState::set_agent_spawner_for_tests)"
                    .into(),
            ))),
            started_at: Instant::now(),
            started_at_utc: chrono::Utc::now(),
            data_dir: dir,
            settings_write: tokio::sync::Mutex::new(()),
            // Never read from the environment: a developer running the suite
            // from a dev shell must not get a different gateway. A test that
            // wants one says so with `set_dev_for_tests`.
            dev: std::sync::atomic::AtomicBool::new(false),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_live: Default::default(),
        });
        app.builds.set_instance_id(instance);
        app.mcp.set_state(&app);
        // No boot runs in a test state (`server::run` is what spawns it), so
        // there is no reconciliation to wait for; a test of the boot window
        // clears this itself.
        app.vram.set_boot_settled(true);
        // The owner rows, for the same reason the catalog below is seeded: a
        // test gateway should be the gateway a fresh install is. Without them
        // every `Admin` route in every integration suite would answer a
        // credential nobody could produce, and the suites would be testing a
        // configuration no owner ever runs.
        crate::agents::token::seed_owner_keys(&app).await?;
        // Seeded here too, so a test sees the catalog a fresh install has
        // rather than an emptier one no owner ever runs.
        crate::agents::seed::seed(&app).await;
        // And each agent's own `agent:<id>` MCP row, re-pointed at the current
        // bind address (container-runtime §3.3). The row's url is written when
        // the manifest is; a `bind_addr` this build boots with that differs
        // from the one the manifest was last saved under would otherwise leave
        // every agent's tools pointing at a dead port, permanently.
        crate::agents::service::resync_all(&app).await;
        Ok(app)
    }

    /// Whether this is a dev instance (`LMGW_DEV`, container-builds §10).
    pub fn dev(&self) -> bool {
        self.dev.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Refuse `what` on a dev instance, with the reason (container-builds
    /// §10): image deletion and `keep_runs` pruning act on the podman image
    /// store, which a dev instance shares with production, and a dev build of
    /// lmgw deciding which of production's images are unused is exactly the
    /// collision the dev prefix exists to prevent for containers.
    pub fn refuse_in_dev(&self, what: &str) -> Result<(), String> {
        if self.dev() {
            return Err(format!(
                "{what} is refused on a dev instance (LMGW_DEV is set): the podman image store \
                 is shared with the production lmgw, whose images this instance cannot see the \
                 use of"
            ));
        }
        Ok(())
    }

    /// Test-only: mark this gateway a dev instance (or not). `pub`
    /// (unconditional) to match the other `set_*_for_tests` seams.
    #[doc(hidden)]
    pub fn set_dev_for_tests(&self, dev: bool) {
        self.dev.store(dev, std::sync::atomic::Ordering::Relaxed);
    }

    /// The directory container builds use right now — the `builds_dir`
    /// setting, else the default for this instance
    /// ([`crate::backends::paths::builds_dir`]).
    pub fn builds_dir(&self) -> PathBuf {
        crate::backends::paths::builds_dir(&self.snapshot().settings, &self.data_dir, self.dev())
    }

    /// Cheap atomic read of the current config snapshot (hot path).
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// The container runtime registry (§3.2) — one entry per model lmgw
    /// believes is up, and the `acquire` primitive every local forward goes
    /// through.
    pub fn runtime(&self) -> Arc<runtime::registry::Registry> {
        self.runtime.load_full()
    }

    /// Test-only: swap in a registry driven by a fake `CommandRunner` and a
    /// fixed port allocator. `pub` (unconditional) to match the existing
    /// `init_for_tests` / `set_snapshot_for_tests` convention — there is no
    /// `test-helpers` feature gate in this crate.
    ///
    /// Only safe before anything has acquired: the entries of the registry
    /// being replaced are dropped with it, and their containers (if any were
    /// really started) would be orphaned.
    #[doc(hidden)]
    pub fn set_runtime_for_tests(&self, registry: Arc<runtime::registry::Registry>) {
        registry.set_gpu_lease(self.snapshot().gpu_lease.clone());
        self.runtime.store(registry);
    }

    /// How a container agent reaches `podman` (container-runtime §6.1).
    pub fn agent_spawner(&self) -> Arc<dyn crate::agents::container::Spawner> {
        self.agent_spawner.lock().unwrap().clone()
    }

    /// Test-only: swap in a spawner that replays scripted output instead of
    /// starting a container. `pub` (unconditional) to match the existing
    /// `set_runtime_for_tests` convention.
    #[doc(hidden)]
    pub fn set_agent_spawner_for_tests(&self, spawner: Arc<dyn crate::agents::container::Spawner>) {
        *self.agent_spawner.lock().unwrap() = spawner;
    }

    /// Test-only: atomically swap in a hand-built snapshot (e.g. one carrying a
    /// fake MCP server + overrides) without going through the DB. Lets the
    /// northbound integration tests seed an aggregate against
    /// [`McpManager::seed_ready_conn_for_tests`]. `pub` (unconditional) to match
    /// the existing `init_for_tests` convention — there is no `test-helpers`
    /// feature gate in this crate.
    ///
    /// A benchmark's GPU lease is carried over, as every publish carries it.
    #[doc(hidden)]
    pub fn set_snapshot_for_tests(&self, snap: Snapshot) {
        self.store_carrying_lease(snap);
    }

    /// Take (`Some`) or release (`None`) the GPU for a benchmark run
    /// (benchmark design §3.2): publish the current snapshot with the lease
    /// set, so every reader from now on — the request reroute and the
    /// admission net ([`crate::bench::lease`]) — sees it. Runtime state, not
    /// a setting: nothing is written to the store.
    ///
    /// The registry gets its copy first: it refuses to create an entry while
    /// a lease is held, under its map lock, which is what makes the run's
    /// drain airtight against a start decided just before the lease
    /// (`runtime/registry/lease.rs`).
    pub fn set_gpu_lease(&self, lease: Option<Arc<crate::bench::lease::GpuLease>>) {
        self.runtime().set_gpu_lease(lease.clone());
        self.snapshot.rcu(|cur| {
            let mut next = Snapshot::clone(cur);
            next.gpu_lease = lease.clone();
            next
        });
    }

    /// Publish `snap` with whatever lease the snapshot it replaces carries.
    /// `rcu`, so a lease taken or released while `snap` was being loaded is
    /// never lost or revived: the swap retries against the newer snapshot.
    fn store_carrying_lease(&self, snap: Snapshot) -> Arc<Snapshot> {
        let mut published = None;
        self.snapshot.rcu(|cur| {
            let mut next = snap.clone();
            next.gpu_lease = cur.gpu_lease.clone();
            let next = Arc::new(next);
            published = Some(next.clone());
            next
        });
        published.expect("rcu runs its closure at least once")
    }

    /// Reload the snapshot from the DB after any config mutation, then
    /// reconcile the live MCP connections against it (§9) — start newly
    /// enabled/autostart servers, stop removed/disabled ones, restart on a
    /// connection-config change.
    pub async fn reload_snapshot(&self) -> Result<Arc<Snapshot>, GatewayError> {
        let snap = self.publish_snapshot().await?;
        self.mcp.reconcile(&snap).await;
        Ok(snap)
    }

    /// The half of [`Self::reload_snapshot`] that only touches this process:
    /// load the config and publish it, so every reader is on the new one.
    ///
    /// Split out for `ops::hold_set` (gpu-hold design §5/§6). Reconciling MCP
    /// means a `join_all` over autostart servers, and one unreachable server
    /// stalls it for its whole connect timeout — with the reconcile in the
    /// middle, engaging the hold would not reach its sweep, and the GPU the
    /// owner just asked for stays occupied for as long as some unrelated tool
    /// server takes to fail. Same hazard §5 restructured `boot` for. Arming
    /// the gate is what must happen before the sweep, and that is exactly what
    /// this does; the reconcile can follow the sweep.
    pub(crate) async fn publish_snapshot(&self) -> Result<Arc<Snapshot>, GatewayError> {
        // A benchmark's lease is runtime state the store does not have, so
        // the published snapshot carries the current one over.
        let snap = self.store_carrying_lease(store::load_snapshot(&self.db).await?);
        // A model's ctx-size, cache types or GGUF path may have just changed,
        // and every one of those changes what it costs on the GPU (§9b).
        self.vram.forget_plans().await;
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::dev_flag;

    #[test]
    fn lmgw_dev_errs_on_the_side_of_dev() {
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some(" OFF "),
            Some("no"),
        ] {
            assert!(!dev_flag(off), "{off:?}");
        }
        for on in [Some("1"), Some("true"), Some("yes"), Some("typo")] {
            assert!(dev_flag(on), "{on:?}");
        }
    }
}
