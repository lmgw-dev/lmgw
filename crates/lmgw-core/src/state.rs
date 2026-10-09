//! Shared application state (`Arc<AppState>`), used by HTTP handlers and the
//! Tauri tray alike — no internal HTTP hop (§3).

use std::path::{Path, PathBuf};
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
    /// The audio rows' speech profiles ([`crate::audio::profile`]): what a
    /// TTS row's engine accepts by name, read from its spec and package
    /// GGUF once per change of either.
    pub audio_profiles: Arc<crate::audio::profile::ProfileCache>,
    /// The sample rate each audio row's last WAV said
    /// ([`crate::audio::rates`]) — what a streamed answer's
    /// `x-lmgw-sample-rate` is.
    pub audio_rates: Arc<crate::audio::rates::SampleRates>,
    /// Audio containers a download or a delete left on a `server.json` their
    /// row no longer renders, because they were starting or serving then —
    /// the idle reaper stops each once it is idle
    /// ([`crate::runtime::audio::recheck_left`]).
    pub audio_stale: crate::runtime::audio::LeftStale,
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
    ///
    /// A Hugging Face download takes its first hop with it too
    /// ([`crate::hf::get_with_commit`]): the hub names the commit only on its
    /// own redirect, which reqwest would follow past.
    pub proxy_http: reqwest::Client,
    pub catalog: CatalogCache,
    /// What external `llama_cpp` rows said about themselves in `GET /props`
    /// (llama egress design §4.2): probed in the background, kept until an
    /// edit, a transport failure, a media refusal or the row's Test drops it.
    pub llama_facts: crate::llama_facts::ExternalFacts,
    /// How a reasoning "off" goes out on the cloud models whose provider
    /// refused the protocol's own off form (model-capabilities design §5.6):
    /// learned from the refusal, kept per route for the process's life.
    pub(crate) reasoning_learned: crate::proxy::reasoning_fit::Learned,
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
    /// What realtime sessions score their pauses with instead of Smart Turn
    /// (realtime design §6.3) — `None`, the model, except in a test that
    /// installs a stand-in (`set_turn_score_for_tests`).
    turn_score_hook: std::sync::Mutex<Option<crate::realtime::ScoreHook>>,
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
    /// The background tasks were started (`server::spawn_background_tasks`):
    /// once per state, so a server restarted on the same state ("Restart
    /// gateway") does not start a second reaper, status tick and pruner
    /// beside the first.
    pub(crate) background: std::sync::atomic::AtomicBool,
    pub data_dir: PathBuf,
    /// `init_for_tests`' own data dir, removed with the state: an audio row's
    /// start, an agent run or a corpus write leaves files there, and /tmp is
    /// RAM. `None` for a real data dir.
    _test_dir: Option<TestDir>,
    /// Serializes every read-modify-write of the whole `Settings` blob
    /// (`store::save_settings`'s callers): clone the snapshot's settings,
    /// mutate the copy, save, publish the snapshot — all under this lock.
    /// The MCP reconcile after the publish runs once it is let go
    /// ([`Self::settings_saved`]).
    ///
    /// Nothing else guards that round trip, so two writers racing (a
    /// dashboard save and `ops::hold_set`, say) could each read the
    /// pre-mutation blob and the second save would silently revert the
    /// first's change — including `hold.active` after the hold sweep has
    /// already stopped every container, which is the failure that made this
    /// worth fixing (gpu-hold design §3.1).
    pub settings_write: tokio::sync::Mutex<()>,
    /// The order the snapshot's loads from the store publish in
    /// ([`Self::publish_snapshot`]).
    snapshot_loads: SnapshotLoads,
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
    /// The Chat change feed's in-process half (client-apps design §2.3):
    /// the wake its open streams wait on after a write that recorded a
    /// change, and the live turns, bound sessions and hold it reports. Its
    /// live half is the one [`Self::chat_live`] registers turns in.
    pub(crate) chat_feed: crate::web::chat_feed::Feed,
    /// One lock per Chat folder, held by every write that reads an ongoing
    /// folder's current thread and then moves it (client-apps design L8),
    /// so two clients asking together get one thread.
    pub(crate) chat_folder_locks: crate::web::FolderLocks,
    /// Paired devices' open connections and the revocation signal those
    /// connections watch (client-apps design §1.6). In-process, like the live
    /// turns: a restart ends every connection anyway.
    pub devices: crate::devices::Devices,
    /// The servers' stops (`server::Stops`): every long-lived stream ends
    /// when the server it was opened on stops, so a graceful shutdown does
    /// not wait on a client that stays.
    pub stops: crate::server::Stops,
    /// This state's own `Weak`, set once the `Arc` exists: what a registry's
    /// hooks reach the state through ([`Self::wire_runtime`]) without a cycle.
    me: std::sync::OnceLock<std::sync::Weak<AppState>>,
}

pub type SharedState = Arc<AppState>;

/// A Chat thread's lock a test holds ([`AppState::hold_chat_thread_for_tests`]).
#[doc(hidden)]
pub struct ChatThreadHold(#[allow(dead_code)] crate::web::chat_live::HistoryWrite);

/// A turn of a Chat thread a test keeps running
/// ([`AppState::chat_turn_held_for_tests`]).
#[doc(hidden)]
pub struct ChatTurnHold(#[allow(dead_code)] crate::web::chat_live::Ticket);

/// The order the loads of the snapshot from the store publish in (the
/// branch review's N-1). Two reloads may load at once — a key's level saved
/// while a model is saved — and the one that loaded first may publish last:
/// an older snapshot then replaced a newer one, and a level a write had
/// committed and published was taken back until the next publish. Each
/// load takes a number as it starts, and one that started before a load
/// already published is not published over it. Every write that changes
/// the store reloads once it committed, so the load published instead
/// started after that write's commit too, and read at least what it did.
#[derive(Debug, Default)]
struct SnapshotLoads {
    /// The last number a load took.
    started: std::sync::atomic::AtomicU64,
    /// The number of the newest load published; held across its swap.
    published: std::sync::Mutex<u64>,
}

impl SnapshotLoads {
    /// A load starts: its number.
    fn start(&self) -> u64 {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1)
    }
}

/// What a settings save published ([`AppState::settings_saved`]), under
/// [`AppState::settings_write`]. The caller lets go of that lock, then
/// reconciles MCP ([`AppState::reconcile_mcp`]).
#[must_use = "reconcile MCP (AppState::reconcile_mcp) once settings_write is let go"]
pub struct SettingsPublished {
    /// Why the load after the save failed twice, for the answer to say
    /// beside the save: the saved settings were laid over the published
    /// snapshot instead. `None` when it loaded.
    pub reload_failed: Option<String>,
}

impl SettingsPublished {
    fn reloaded() -> Self {
        Self {
            reload_failed: None,
        }
    }
}

/// What a key op wrote to its row ([`AppState::key_written`]).
#[derive(Debug, Clone)]
pub(crate) enum KeyWritten {
    Disabled,
    Deleted,
    /// A new credential: its hash, and the plaintext an owner row keeps.
    Rehashed {
        hash: String,
        plain: Option<String>,
    },
    /// A device's admin-tools level (`ApiKey::self_admin`).
    SelfAdmin(crate::config::DeviceAdmin),
}

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

/// `~/.local/share/lmgw` from `HOME`, whatever `XDG_DATA_HOME` says: the
/// installed app's data dir as it is when the variable is not set.
pub fn home_default_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share/lmgw"))
}

/// `LMGW_DATA_DIR`, with an empty value read as unset: an empty path would
/// open the databases in the working directory (chat-voice WP11 review n7).
/// A relative value is made absolute against the working directory: the dir
/// ends up in container bind mounts, and podman reads a `-v` source that does
/// not start with `/` as the name of a volume, refusing the start.
pub fn data_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("LMGW_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(|v| absolute_dir(PathBuf::from(v)))
}

/// `dir` made absolute against the working directory, without resolving
/// symlinks; unchanged when it already is, or when the working directory
/// cannot be read.
fn absolute_dir(dir: PathBuf) -> PathBuf {
    std::path::absolute(&dir).unwrap_or(dir)
}

/// Whether `dir` is the installed app's data dir, as `installed` (what
/// [`default_data_dir`] names, following `XDG_DATA_HOME`) or as
/// `home_default` ([`home_default_data_dir`]) — compared as resolved paths, so
/// another spelling or a symlink is the same dir. A dev run (the headless
/// runner, a debug shell) refuses it: this build's migrations are one-way, and
/// a dev run rewrites the dir's settings. Both names, because pointing
/// `XDG_DATA_HOME` elsewhere does not move the installed app's dir; it only
/// stops `default_data_dir` from naming it (WP5 review n4, WP11 review m4).
pub fn is_installed_data_dir(dir: &Path, installed: &Path, home_default: Option<&Path>) -> bool {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    same(dir, installed) || home_default.is_some_and(|home| same(dir, home))
}

/// A test state's data dir: removed when the state is dropped (the test's
/// runtime shutting down drops every task holding it).
struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    ///
    /// **It touches no container.** Every podman call that lists or removes
    /// by `container_prefix` — agent-container reconciliation and its run-dir
    /// sweep included — is spawned by [`crate::server::run`], after the entry
    /// point's own dev-instance safety step (`examples/headless.rs` steers a
    /// fresh dir off the production prefix, a debug shell refuses one). A
    /// fresh data dir boots on the production default, so anything here that
    /// asked podman would be asking about the installed app's containers
    /// (chat-voice WP5 review B1).
    pub async fn init_with(data_dir: PathBuf, dev: bool) -> anyhow::Result<SharedState> {
        Self::init_with_host(
            data_dir,
            dev,
            Arc::new(runtime::registry::TokioRunner),
            crate::vram::detect_probe(),
        )
        .await
    }

    /// [`Self::init_with`], reaching podman through `runner` and the GPU
    /// through `probe` — the two host seams [`Self::init_for_tests`] fakes —
    /// so a test can boot the real initialisation on a real data dir and
    /// record every podman call it makes. `pub` (unconditional) to match the
    /// `init_for_tests` convention.
    #[doc(hidden)]
    pub async fn init_with_host(
        data_dir: PathBuf,
        dev: bool,
        runner: Arc<dyn runtime::registry::CommandRunner>,
        probe: Arc<dyn crate::vram::GpuProbe>,
    ) -> anyhow::Result<SharedState> {
        // Every path under the data dir may become a container mount.
        let data_dir = absolute_dir(data_dir);
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
        // A database restored from an older copy is found by the cursors
        // themselves: each names its record's check (review W5-4).
        let chat_feed = crate::web::chat_feed::Feed::new(&snapshot);
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let proxy_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let registry = runtime::registry::Registry::new(runner, http.clone());
        let app = Arc::new(Self {
            db,
            corpus,
            knowledge: crate::knowledge::Knowledge::new(knowledge),
            chat_live: crate::web::chat_live::LiveTurns::with_feed(chat_feed.live.clone()),
            chat_feed,
            snapshot: ArcSwap::from_pointee(snapshot),
            telemetry: TelemetryBus::new(),
            policy: std::sync::Arc::new(crate::policy::PolicyGate::default()),
            audio_catalog: std::sync::Mutex::new(None),
            audio_profiles: Default::default(),
            audio_rates: Default::default(),
            audio_stale: Default::default(),
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
            llama_facts: Default::default(),
            reasoning_learned: Default::default(),
            gguf_cache: GgufSummaryCache::default(),
            vram: VramScheduler::with_probe(probe),
            kv_pools: Default::default(),
            runtime: ArcSwap::from_pointee(registry),
            agent_spawner: std::sync::Mutex::new(Arc::new(crate::agents::container::TokioSpawner)),
            turn_score_hook: std::sync::Mutex::new(None),
            started_at: Instant::now(),
            started_at_utc: chrono::Utc::now(),
            background: Default::default(),
            stops: Default::default(),
            data_dir,
            _test_dir: None,
            settings_write: tokio::sync::Mutex::new(()),
            snapshot_loads: SnapshotLoads::default(),
            dev: std::sync::atomic::AtomicBool::new(dev),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_folder_locks: Default::default(),
            devices: Default::default(),
            me: Default::default(),
        });
        app.builds.set_instance_id(instance);
        let _ = app.me.set(Arc::downgrade(&app));
        app.wire_runtime(&app.runtime());
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
        // Boot reconciliation for agent containers (container-runtime §6.4)
        // is not here: it lists and removes by `container_prefix`, and a fresh
        // data dir still carries the production default at this point. It is
        // spawned by `server::run` (`agents::container::boot_reconcile`), after
        // every entry point's dev-instance safety step (chat-voice WP5 review
        // B1).
        //
        // The build runs `fail_orphaned_build_runs` closed above left their
        // worktrees and build contexts behind (container-builds §5 "At
        // boot"). Spawned rather than awaited: git is unbounded wall clock,
        // and a stale directory is not a startup problem. The sweep takes the
        // machine-wide build lock first and skips itself while another
        // instance holds it. It stays here: what it removes is told by this
        // data dir's build instance id (and a Check's dead pid), never by
        // `container_prefix`.
        let st = app.clone();
        tokio::spawn(async move {
            crate::backends::run::boot_sweep(&st).await;
        });
        Ok(app)
    }

    /// In-memory state for tests.
    pub async fn init_for_tests() -> anyhow::Result<SharedState> {
        Self::init_for_tests_on(store::open_in_memory().await?).await
    }

    /// Chat thread `id`'s lock, held by a test where a write in flight (the
    /// owner's attach of the self-admin toolset) would hold it: between a
    /// route's first read and its write, which must then re-check what it
    /// read (reviews W5-2, W6-6, W6-13). Released when the hold drops.
    #[doc(hidden)]
    pub async fn hold_chat_thread_for_tests(&self, id: i64) -> ChatThreadHold {
        ChatThreadHold(self.chat_live.hold(id).await)
    }

    /// A turn of Chat thread `id` that runs until the hold drops, as the
    /// owner's: while it runs, an ended MCP task's result waits to enter
    /// the thread (MCP Tasks design §3.1), so a test reads the task's row
    /// as the follower left it.
    #[doc(hidden)]
    pub async fn chat_turn_held_for_tests(&self, id: i64) -> ChatTurnHold {
        ChatTurnHold(self.chat_live.begin_as(id, None, 0).await)
    }

    /// What waits to enter Chat thread `id` enters now, if no turn of it
    /// runs, as an ended task's follower delivers it (MCP Tasks design
    /// §3.1): how many results entered. A test that seeds its task rows by
    /// hand has no follower to do it.
    #[doc(hidden)]
    pub async fn deliver_chat_tasks_for_tests(&self, id: i64) -> usize {
        crate::web::chat_tasks::deliver::when_idle(self, id).await
    }

    /// Chat folder `id`'s lock, held by a test where a write in flight would
    /// hold it (review W6-12: a thread created by hand in the folder decides
    /// under it, from the folder as it is then).
    #[doc(hidden)]
    pub async fn hold_chat_folder_for_tests(&self, id: i64) -> tokio::sync::OwnedMutexGuard<()> {
        self.chat_folder_locks.lock(id).await
    }

    /// [`Self::init_for_tests`] on a database another test state already
    /// wrote: a restart, as far as anything kept on disk can tell — every
    /// in-process half (live turns, the Chat feed's live half and wake,
    /// revocations) is new, the database is the one it left.
    #[doc(hidden)]
    pub async fn init_for_tests_on(db: sqlx::SqlitePool) -> anyhow::Result<SharedState> {
        let corpus = quickdoc_core::store::open_in_memory()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let knowledge = crate::knowledge::store::open_in_memory().await?;
        let snapshot = store::load_snapshot(&db)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let chat_feed = crate::web::chat_feed::Feed::new(&snapshot);
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
            chat_live: crate::web::chat_live::LiveTurns::with_feed(chat_feed.live.clone()),
            chat_feed,
            snapshot: ArcSwap::from_pointee(snapshot),
            telemetry: TelemetryBus::new(),
            policy: std::sync::Arc::new(crate::policy::PolicyGate::default()),
            audio_catalog: std::sync::Mutex::new(None),
            audio_profiles: Default::default(),
            audio_rates: Default::default(),
            audio_stale: Default::default(),
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
            llama_facts: Default::default(),
            reasoning_learned: Default::default(),
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
            turn_score_hook: std::sync::Mutex::new(None),
            started_at: Instant::now(),
            started_at_utc: chrono::Utc::now(),
            background: Default::default(),
            stops: Default::default(),
            _test_dir: Some(TestDir(dir.clone())),
            data_dir: dir,
            settings_write: tokio::sync::Mutex::new(()),
            snapshot_loads: SnapshotLoads::default(),
            // Never read from the environment: a developer running the suite
            // from a dev shell must not get a different gateway. A test that
            // wants one says so with `set_dev_for_tests`.
            dev: std::sync::atomic::AtomicBool::new(false),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_folder_locks: Default::default(),
            devices: Default::default(),
            me: Default::default(),
        });
        app.builds.set_instance_id(instance);
        let _ = app.me.set(Arc::downgrade(&app));
        app.wire_runtime(&app.runtime());
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

    /// Refuse a write into `target` (a models dir, or a path in one) on a dev
    /// instance when it lies outside this instance's data dir
    /// ([`crate::config::dev_models_dir_refusal`]): a dev copy keeps
    /// production's models dirs, and what it writes there lands in the
    /// installed app's tree.
    pub fn refuse_shared_models_dir(&self, target: &Path) -> Result<(), String> {
        match crate::config::dev_models_dir_refusal(self.dev(), &self.data_dir, target) {
            Some(why) => Err(why),
            None => Ok(()),
        }
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
        self.wire_runtime(&registry);
        self.runtime.store(registry);
    }

    /// What a registry learns from the state around it: whose containers it
    /// starts — this data dir's instance id, the `lmgw.owner` label the
    /// reconciliation pass after boot keys on (`registry/unheld.rs`) — and
    /// what a settled start tells the rest of lmgw: the new container's PID
    /// (§4.7) and the dashboard's VRAM frame, whether or not its requester is
    /// still there (`registry/owned.rs`).
    fn wire_runtime(&self, registry: &runtime::registry::Registry) {
        registry.set_owner(self.builds.instance_id().to_string());
        let me = self.me.get().cloned();
        registry.set_on_started(Box::new(move || {
            if let Some(state) = me.as_ref().and_then(std::sync::Weak::upgrade) {
                state.vram.cache_pids(&state);
                crate::vram::broadcast(&state);
            }
        }));
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

    /// The stand-in realtime sessions score their pauses with, if a test
    /// installed one (realtime design §6.3).
    pub fn turn_score_hook(&self) -> Option<crate::realtime::ScoreHook> {
        self.turn_score_hook.lock().unwrap().clone()
    }

    /// Test-only: score every realtime pause with `hook` instead of Smart
    /// Turn (`None`: the model again) — what lets a suite make the scorer
    /// fail, or answer a chosen probability. `pub` (unconditional) to match
    /// the other `set_*_for_tests` seams.
    #[doc(hidden)]
    pub fn set_turn_score_for_tests(&self, hook: Option<crate::realtime::ScoreHook>) {
        *self.turn_score_hook.lock().unwrap() = hook;
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

    /// Lay a key op's write over the published snapshot when the reload
    /// after it failed (review W2-6, W3-7): the row is written, and the
    /// snapshot that still says otherwise must not keep admitting the key it
    /// revoked — a new request with a disabled, deleted or rotated key is
    /// refused at once, not at the next successful reload. Every revocation
    /// watch re-reads its key in it.
    ///
    /// Published as every snapshot is ([`Self::publish_over`]): a level laid
    /// over plays as a reloaded one does.
    pub(crate) fn key_written(&self, id: i64, written: KeyWritten) {
        self.publish_over(|cur| {
            let mut next = Snapshot::clone(cur);
            match &written {
                KeyWritten::Disabled => {
                    if let Some(k) = next.api_keys.iter_mut().find(|k| k.id == id) {
                        k.enabled = false;
                    }
                }
                KeyWritten::Deleted => next.api_keys.retain(|k| k.id != id),
                KeyWritten::SelfAdmin(level) => {
                    if let Some(k) = next.api_keys.iter_mut().find(|k| k.id == id) {
                        k.self_admin = *level;
                    }
                }
                KeyWritten::Rehashed { hash, plain } => {
                    if let Some(k) = next.api_keys.iter_mut().find(|k| k.id == id) {
                        k.key_hash = hash.clone();
                        if let Some(p) = plain {
                            k.key_plain = Some(crate::config::Secret::new(p.clone()));
                        }
                    }
                }
            }
            next
        });
        self.devices.revocations.rearm();
    }

    /// Publish the snapshot after a settings save that committed `s`, and
    /// when the load fails twice, lay `s` over the published snapshot
    /// instead (review G-6): the save is in the store, and a snapshot that
    /// still says otherwise must not keep a lowered self-admin level's
    /// devices on threads they no longer reach. Publishing it runs what a
    /// moved level runs.
    ///
    /// The half of a reload that runs under [`Self::settings_write`]: the
    /// next writer copies the published settings under that lock, so a
    /// publish after it is let go could hand that writer the settings from
    /// before this save, and its save would revert this one. The MCP
    /// reconcile is the other half, and the caller runs it once it has let
    /// go of the lock ([`SettingsPublished`]): a stdio server's start inside
    /// it can take as long as a cold image pull, and the GPU hold, which
    /// takes the lock too, would wait for it (the begin-write review's B-1).
    pub async fn settings_saved(&self, s: &crate::config::Settings) -> SettingsPublished {
        let first = match self.publish_snapshot().await {
            Ok(_) => return SettingsPublished::reloaded(),
            Err(e) => e,
        };
        tracing::warn!("settings saved, but the reload failed ({first}); trying again");
        let again = match self.publish_snapshot().await {
            Ok(_) => return SettingsPublished::reloaded(),
            Err(e) => e,
        };
        tracing::error!(
            "settings saved, but the reload failed twice ({again}); the saved settings are \
             laid over the published snapshot"
        );
        // Laid over the snapshot the swap replaces, inside the `rcu` (the
        // branch review's verification, V-9): a publish that lands while
        // this one is made is kept, and the swap retries against it, rather
        // than being replaced by a copy taken before it.
        self.publish_over(|cur| {
            let mut next = Snapshot::clone(cur);
            next.settings = s.clone();
            next
        });
        SettingsPublished {
            reload_failed: Some(again.to_string()),
        }
    }

    /// Reconcile MCP against the snapshot published now: the step a
    /// settings writer runs once it has let go of [`Self::settings_write`]
    /// (`settings_set`, `settings_set_full`, `hold_set`, the router-mode
    /// sweep). Not against the snapshot that writer published: another path
    /// (`mcp_server_set`, `key_set`) may have published since, and a
    /// reconcile of the older one would stop a server the newer one started
    /// or enabled (the begin-write re-check's R-4). A reconcile only moves
    /// the live connections towards the snapshot it is given, so the newest
    /// is always the right one.
    pub async fn reconcile_mcp(&self) {
        self.mcp.reconcile(&self.snapshot()).await;
    }

    /// Publish `snap` with whatever lease the snapshot it replaces carries.
    /// `rcu`, so a lease taken or released while `snap` was being loaded is
    /// never lost or revived: the swap retries against the newer snapshot.
    fn store_carrying_lease(&self, snap: Snapshot) -> Arc<Snapshot> {
        self.publish_over(|cur| {
            let mut next = snap.clone();
            next.gpu_lease = cur.gpu_lease.clone();
            next
        })
    }

    /// Publish `loaded`, read from the store by load number `load`
    /// ([`SnapshotLoads`]), unless a load that started after it published
    /// already: the snapshot published either way. A benchmark's lease is
    /// runtime state the store does not have, so the published snapshot
    /// carries the current one over.
    fn publish_loaded(&self, load: u64, loaded: Snapshot) -> Arc<Snapshot> {
        let mut newest = self
            .snapshot_loads
            .published
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if load < *newest {
            tracing::debug!(
                "snapshot load {load} is older than load {} that published already; not published",
                *newest
            );
            return self.snapshot();
        }
        *newest = load;
        self.store_carrying_lease(loaded)
    }

    /// Publish what `next` makes of the snapshot it replaces, computed inside
    /// the `rcu` so it is always made of the newest one, then run what every
    /// publish runs.
    fn publish_over(&self, next: impl Fn(&Snapshot) -> Snapshot) -> Arc<Snapshot> {
        let mut published = None;
        let mut replaced = None;
        self.snapshot.rcu(|cur| {
            replaced = Some(cur.clone());
            let next = Arc::new(next(cur));
            published = Some(next.clone());
            next
        });
        let published = published.expect("rcu runs its closure at least once");
        let replaced = replaced.expect("rcu runs its closure at least once");
        // The Chat feed's `hold` event and its live buffer (client-apps
        // design §2.2): every published snapshot, whichever path saved it.
        self.chat_feed.published(|| self.snapshot());
        // A device's admin-tools level moved, or the gateway's that caps
        // every device's (client-apps design L3's note, 2026-10-07), whichever
        // path saved it — the gateway's plays as each device's own move does.
        // The save recorded it in the feed (`store::update_key_policy`,
        // `store::save_settings`), and each device's stream reads it now: a
        // level record waits for the snapshot that says it, and a feed opened
        // in between for its levels (`chat_feed`'s stream doc), so they are
        // woken here, at the publish — not after whatever the writer does
        // next, an MCP reconcile that one unreachable server stalls for its
        // connect timeout (the branch review's N-1). The toolset's threads
        // come or go, and a fresh `state`. A device's bound sessions on a
        // thread it no longer reaches close with the neutral 4004 and its
        // turns there are cancelled; its realtime sessions list `lmgw`
        // again. A turn's next call reads the level per call.
        let gateway_moved = replaced.settings.self_admin != published.settings.self_admin;
        let device_moved = published.api_keys.iter().any(|k| {
            replaced
                .api_keys
                .iter()
                .any(|was| was.id == k.id && was.self_admin != k.self_admin)
        });
        if gateway_moved || device_moved {
            self.chat_feed.wake();
            self.chat_live.reach_changed(&published);
            if device_moved {
                self.chat_feed.live.devices_refresh();
            }
            self.devices.reach_moves.moved();
        }
        published
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
    /// load the config and publish it, so every reader is on the new one —
    /// unless a load that started later published already ([`SnapshotLoads`]):
    /// then the newer snapshot stays, and is the one returned.
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
        let load = self.snapshot_loads.start();
        let snap = self.publish_loaded(load, store::load_snapshot(&self.db).await?);
        // Every revocation watch re-reads its key in what was just published
        // (client-apps design §1.6): a key disabled, rotated or deleted by any
        // path — the Keys page, an agent's page, a restore — ends the
        // connections and streams opened with it, not only the ones whose op
        // raised the signal by name.
        self.devices.revocations.rearm();
        // A model's ctx-size, cache types or GGUF path may have just changed,
        // and every one of those changes what it costs on the GPU (§9b).
        self.vram.forget_plans().await;
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::{absolute_dir, dev_flag, is_installed_data_dir, AppState};
    use crate::config::SelfAdmin;
    use std::path::{Path, PathBuf};

    /// The branch review's N-1: two reloads cross a write — the first loads
    /// before its commit, the second after it — and the first publishes
    /// last. Its older snapshot does not replace the newer one; a load that
    /// starts later publishes as ever.
    #[tokio::test]
    async fn an_older_load_is_not_published_over_a_newer_one() {
        let state = AppState::init_for_tests().await.unwrap();
        let level = |state: &AppState| state.snapshot().settings.self_admin;
        let first = state.snapshot_loads.start();
        let stale = crate::store::load_snapshot(&state.db).await.unwrap();
        let mut s = state.snapshot().settings.clone();
        assert_ne!(s.self_admin, SelfAdmin::Full);
        s.self_admin = SelfAdmin::Full;
        crate::store::save_settings(&state.db, &s).await.unwrap();
        let second = state.snapshot_loads.start();
        let fresh = crate::store::load_snapshot(&state.db).await.unwrap();

        state.publish_loaded(second, fresh);
        assert_eq!(level(&state), SelfAdmin::Full);
        let published = state.publish_loaded(first, stale);
        assert_eq!(published.settings.self_admin, SelfAdmin::Full);
        assert_eq!(level(&state), SelfAdmin::Full, "the older load came last");

        s.self_admin = SelfAdmin::Off;
        crate::store::save_settings(&state.db, &s).await.unwrap();
        state.publish_snapshot().await.unwrap();
        assert_eq!(level(&state), SelfAdmin::Off, "a later load publishes");
    }

    #[test]
    fn a_relative_data_dir_is_made_absolute() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_dir(PathBuf::from("target/dev-copy")),
            cwd.join("target/dev-copy")
        );
        assert_eq!(
            absolute_dir(PathBuf::from("/srv/lmgw")),
            PathBuf::from("/srv/lmgw")
        );
    }

    /// A stand-in for the installed app's dir: the guard's own comparison,
    /// never the real path.
    #[test]
    fn the_installed_data_dir_is_refused_under_either_name() {
        let home = tempfile::tempdir().unwrap();
        let installed = home.path().join(".local/share/lmgw");
        std::fs::create_dir_all(&installed).unwrap();
        let moved = Path::new("/srv/xdg-data/lmgw");
        let dev = home.path().join("dev-copy");

        assert!(is_installed_data_dir(&installed, &installed, None));
        // XDG_DATA_HOME points elsewhere: the dir under HOME is still it.
        assert!(is_installed_data_dir(&installed, moved, Some(&installed)));
        // Another spelling of the same dir.
        let spelled = installed.join("..").join("lmgw");
        assert!(is_installed_data_dir(&spelled, moved, Some(&installed)));
        // A symlink to it.
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        assert!(is_installed_data_dir(&link, moved, Some(&installed)));
        // XDG_DATA_HOME's own dir, when it is the one named.
        assert!(is_installed_data_dir(moved, moved, Some(&installed)));
        // A copy is not.
        assert!(!is_installed_data_dir(&dev, moved, Some(&installed)));
        assert!(!is_installed_data_dir(&dev, &installed, None));
    }

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
