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

pub mod data_dir_lock;
pub mod data_dirs;
pub use data_dirs::{default_data_dir, home_default_data_dir, local_data_dir};
pub mod dev_instance;
pub mod fresh_install;

/// The host an entry point boots a gateway on: podman, the GPU and the rest
/// of the machine (driver files, `nvidia-ctk`, `XDG_RUNTIME_DIR`).
/// [`Host::real`] is this machine's; a test passes fakes, as
/// [`AppState::init_with_host`]'s do.
#[derive(Clone)]
pub struct Host {
    pub runner: Arc<dyn runtime::registry::CommandRunner>,
    pub probe: Arc<dyn crate::vram::GpuProbe>,
    pub machine: Arc<dyn lmgw_machine::Facts>,
    /// Where a container-mode start reads the device CA's wrapping key
    /// (headless server design §9): [`crate::devices::tls::WRAP_KEY_SECRET`]
    /// on this machine, a test's own file in tests.
    pub wrap_key_secret: PathBuf,
}

impl Host {
    /// This machine: the real podman, the GPU probe it detects, and the
    /// system's facts.
    pub fn real() -> Self {
        Self {
            runner: Arc::new(runtime::registry::TokioRunner),
            probe: crate::vram::detect_probe(),
            machine: Arc::new(lmgw_machine::SystemFacts::prepared_by(
                crate::child::system_tool,
            )),
            wrap_key_secret: PathBuf::from(crate::devices::tls::WRAP_KEY_SECRET),
        }
    }
}

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
    /// Accounts' runtime (headless server design §5): the session desk,
    /// the sign-in throttle and the password verifier's permits.
    pub accounts: crate::accounts::Runtime,
    /// The lab cells being answered now (labs-store design §2.6): a second
    /// post of one is the lab's 409, not a second dispatch. Here rather than
    /// in a `static` for [`Self::agent_ledger`]'s reason.
    pub lab_cells: crate::labs::claims::CellClaims,
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
    /// Every upstream request and stream, and the hub's file reads. Decodes
    /// nothing (`no_gzip`): its wire behaviour is what it was before the
    /// build turned the `gzip` feature on (find-a-model design D141).
    pub http: reqwest::Client,
    /// The Hugging Face hub's metadata reads (search, listings, trees,
    /// uploads, overviews), the one client that asks for gzip and decodes it:
    /// the hub's search pages are about 600 KB plain. Same connect timeout and
    /// TLS as [`Self::http`].
    pub hub_http: reqwest::Client,
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
    ///
    /// It decodes nothing (`no_gzip`): what passes through it reaches its
    /// reader as the far end sent it, a container's compressed answer to the
    /// browser with its `Content-Encoding`, and a file with its length. The
    /// other client, `hub_http`, decodes gzip for the hub's metadata reads
    /// alone (find-a-model design D141).
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
    /// The upstream rows remembered out of quota (openai egress design
    /// §14.6): a hop on one with a fallback is skipped to it. In memory, for
    /// the process's life.
    pub upstream_quota: crate::upstream_quota::QuotaMemory,
    /// GGUF header cache (model capabilities design §3.5): one entry per
    /// absolute path, validated against the file's `(len, mtime)` on every
    /// hit. Keeps `/v1/models` cheap — the alternative is a header read per
    /// local row on every call.
    pub gguf_cache: GgufSummaryCache,
    /// Hugging Face files' headers read over HTTP before a download
    /// (find-a-model design §2.1, D4): by the file's content hash, for the
    /// process's life.
    pub hf_headers: crate::hf::RemoteHeaderCache,
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
    /// lmgw's own CDI spec (machine-check design §3.6, ruling 4): where it
    /// lives for this instance, whether it is true of the machine, and the
    /// `--cdi-spec-dir` every GPU podman call carries. Production's is
    /// managed on the real machine; a test state's is unmanaged (no spec, no
    /// option) unless the test installs one ([`Self::set_cdi_for_tests`]).
    cdi: ArcSwap<runtime::cdi::OwnSpec>,
    /// The machine check (machine-check design WP4): whether this computer
    /// can run local models, read on the machine `init_with_host` was given
    /// (a test state's reads a blank one until the test states one,
    /// [`crate::machine::MachineChecks::set_machine`]).
    pub machine: crate::machine::MachineChecks,
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
    /// Serializes the writes that change what a stored fallback reaches
    /// (openai egress design §14.2): every setter whose save runs the
    /// referrer dry run holds it from the snapshot its check reads, through
    /// the dry run, to the real commit and the snapshot reload after it
    /// ([`Self::routing_write`]). Two such saves racing could each pass
    /// their dry run against the routing before the other, and commit
    /// together what neither would have allowed; a write committed while
    /// another one's check ran would be judged as that one's. Only these
    /// writes wait on it — reads, requests and every other write never do.
    routing_write: Arc<tokio::sync::Mutex<()>>,
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
    /// Where this lmgw runs and what its start was told (headless server
    /// design D2, D20): read once by the entry point, given to
    /// [`AppState::init_started`]. Read through [`Self::deployment`] and
    /// [`Self::stop_budget`]; a lock only so a test can set it
    /// ([`Self::set_start_for_tests`]), nothing in production writes it after
    /// boot.
    start: std::sync::RwLock<crate::config::deployment::StartFacts>,
    /// The server image's update notice (headless server design §6.3):
    /// what the last check of the feed found, `None` while this lmgw is the
    /// newest, the check is off, or none ran ([`crate::update::notice`]).
    update_notice: std::sync::RwLock<Option<lmgw_api_types::UpdateNotice>>,
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
    /// The Chat's running image and audio calls, by thread and call id: what
    /// the per-call cancel route stops (chat-media-tools design §6.5).
    pub(crate) media_calls: crate::web::MediaCalls,
    /// Transfers between devices (device transfers design §3, §4): the
    /// upload slots, the offers' lock, a test's free space. The rows and
    /// files are the store's and the data dir's.
    pub transfers: crate::transfers::Transfers,
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
    /// WebRTC calls (calls design §3): the UDP socket groups, the driver
    /// that owns every call, the registry of live calls. In-process, like
    /// the stops: a call is live state.
    pub calls: crate::realtime::calls::Calls,
    /// The main TLS listener while it serves, or why the last start did not
    /// bring it up (headless server design §4): what Settings shows.
    pub main_tls: crate::server::main_tls::TlsListenerSlot,
    /// What proxy mode has seen since the server started (headless server
    /// design §4.5): the main listeners' first peers, the last refusal.
    pub proxy_seen: crate::server::forwarded::ProxySeen,
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
    /// A device's key ended (a new token that ends it, the CA's rotation):
    /// it waits for its next enrolment (device link design §5.2).
    Pending,
    /// A device's waiting key was confirmed in place of the one it kept
    /// (device link design §3.5, §5.2): `key_hash` is the row's key now,
    /// confirmed, and the kept one opens nothing.
    Confirmed {
        key_hash: String,
    },
}

/// Whether `LMGW_DEV` marks this process as a dev instance. Unset, empty,
/// `0`, `false`, `no` and `off` mean no; **anything else means yes**. What
/// the flag guards is destructive in production's shared image store, so a
/// value nobody anticipated (`LMGW_DEV=yes please`) errs on the side of the
/// dev instance refusing, never on the side of it deleting.
pub fn dev_from_env() -> bool {
    dev_flag(std::env::var("LMGW_DEV").ok().as_deref())
}

/// [`dev_from_env`]'s reading of a value of `LMGW_DEV` (`None`: unset), for
/// an entry point that reads its environment through a lookup of its own.
pub fn dev_flag(v: Option<&str>) -> bool {
    match v.map(|v| v.trim().to_ascii_lowercase()) {
        None => false,
        Some(v) => !matches!(v.as_str(), "" | "0" | "false" | "no" | "off"),
    }
}

/// Where a start keeps the device CA's wrapping key (device link design
/// D2): production's start, the session's Secret Service; a dev instance
/// (`dev`: the headless runner, a debug build, `LMGW_DEV`), none, so its
/// key stays plain in its own data dir as a headless start's does.
///
/// A dev instance runs in the user's desktop session, and the session bus
/// there is the owner's real wallet's whatever its environment says: zbus takes an
/// unset `DBUS_SESSION_BUS_ADDRESS` to mean `$XDG_RUNTIME_DIR/bus`, and a
/// private bus activates a Secret Service of its own on the user's keyring files.
/// So the rule is the start's, not the environment's; the bus itself stays
/// as it is, rootless podman's systemd cgroup manager needing it.
///
/// In container mode (headless server design §9, WP8) there is no session
/// bus to ask; the store is the image's fixed key, the Podman secret
/// (`Secret=lmgw-wrap-key` in the unit), read once per start, dev instance
/// or not: the rule above keeps dev instances off the desktop's wallet,
/// which a file is not. Without the file it answers as no Secret Service
/// does (the key stays plain, 0600, on the data volume, with the warn
/// line), but its sentences name the secret. The start installs it and
/// wraps a plain key at once ([`crate::devices::tls::Trust::start_on_secret`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapKeyPlace {
    /// The session's Secret Service: a desktop start that is production's.
    SecretService,
    /// The image's secret: every container-mode start.
    ImageSecret,
    /// None: a dev instance on the desktop.
    Plain,
}

fn wrap_key_place(dev: bool, deployment: crate::config::deployment::Deployment) -> WrapKeyPlace {
    if deployment.is_container() {
        WrapKeyPlace::ImageSecret
    } else if dev {
        WrapKeyPlace::Plain
    } else {
        WrapKeyPlace::SecretService
    }
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
/// runtime shutting down drops every task holding it). A state something
/// still holds when the process exits (a thread outside the runtime, a task
/// the runtime never got to drop) is never dropped, which left one directory
/// per such test on the RAM-backed `/tmp`; so every test dir is also in
/// [`TEST_DIRS`], and the process's exit sweeps what is left.
struct TestDir(PathBuf);

impl TestDir {
    fn new(dir: PathBuf) -> Self {
        static SWEEP: std::sync::Once = std::sync::Once::new();
        SWEEP.call_once(|| {
            extern "C" fn sweep() {
                // Never waited for: exit cannot hang on a lock a thread still
                // holds (that thread's dirs are left, as before this sweep).
                let dirs = match TEST_DIRS.try_lock() {
                    Ok(dirs) => dirs,
                    Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => return,
                };
                for dir in dirs.iter() {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            // SAFETY: `atexit` only registers the handler, which touches a
            // static and the filesystem.
            unsafe { libc::atexit(sweep) };
        });
        if let Ok(mut dirs) = TEST_DIRS.lock() {
            dirs.push(dir.clone());
        }
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        // Gone: nothing for the exit sweep to do, and the list stays as long
        // as the states alive.
        if let Ok(mut dirs) = TEST_DIRS.lock() {
            dirs.retain(|d| d != &self.0);
        }
    }
}

/// The data dirs of the test states alive in this process, for the exit sweep.
static TEST_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

impl AppState {
    /// Take the routing-write lock ([`Self::routing_write`]'s field doc):
    /// owned, so a save's detached commit carries it to its end.
    pub async fn routing_write(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.routing_write).lock_owned().await
    }

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
        Self::init_started(data_dir, dev, crate::config::deployment::Start::desktop()).await
    }

    /// [`Self::init_with`] for an entry point that says where it runs
    /// (headless server design §1): `lmgw-server` passes the deployment
    /// and, in container mode, what its environment pins and seeds, which
    /// is applied to the stored settings before the snapshot is first
    /// loaded — before anything reads or binds `bind_addr`.
    pub async fn init_started(
        data_dir: PathBuf,
        dev: bool,
        start: crate::config::deployment::Start,
    ) -> anyhow::Result<SharedState> {
        Self::init_started_on(data_dir, dev, start, Host::real()).await
    }

    /// [`Self::init_started`] on `host`'s seams: the whole start, the
    /// Secret Service's rule included, for an entry point whose tests boot
    /// it on a podman and a GPU of their own (`lmgw-server`'s `start`).
    pub async fn init_started_on(
        data_dir: PathBuf,
        dev: bool,
        start: crate::config::deployment::Start,
        host: Host,
    ) -> anyhow::Result<SharedState> {
        let deployment = start.deployment;
        let wrap_key_secret = host.wrap_key_secret;
        let state = Self::init_started_with_host(
            data_dir,
            dev,
            start,
            host.runner,
            host.probe,
            host.machine,
        )
        .await?;
        let trust = &state.devices.trust;
        match wrap_key_place(dev, deployment) {
            #[cfg(not(windows))]
            WrapKeyPlace::SecretService => {
                trust.set_store(Arc::new(crate::devices::tls::SecretServiceStore))
            }
            // Windows' counterpart (Windows design §4.4, WP6).
            #[cfg(windows)]
            WrapKeyPlace::SecretService => {
                trust.set_store(Arc::new(crate::devices::tls::CredentialManagerStore))
            }
            WrapKeyPlace::ImageSecret => {
                let found = crate::devices::tls::WrapKeySecret::read(&wrap_key_secret);
                trust.start_on_secret(&state.data_dir, found).await
            }
            WrapKeyPlace::Plain => tracing::info!(
                "a dev instance never asks the desktop's Secret Service: the device CA's key, \
                 once the device listener makes one, is kept plain (0600) in {}",
                state.data_dir.join(crate::devices::tls::TLS_DIR).display()
            ),
        }
        Ok(state)
    }

    /// [`Self::init_with`], reaching podman through `runner`, the GPU through
    /// `probe` and the rest of the machine (driver files, `nvidia-ctk`,
    /// `XDG_RUNTIME_DIR`) through `machine` — the host seams
    /// [`Self::init_for_tests`] fakes — so a test can boot the real
    /// initialisation on a real data dir and record every podman call it
    /// makes. `pub` (unconditional) to match the `init_for_tests` convention.
    #[doc(hidden)]
    pub async fn init_with_host(
        data_dir: PathBuf,
        dev: bool,
        runner: Arc<dyn runtime::registry::CommandRunner>,
        probe: Arc<dyn crate::vram::GpuProbe>,
        machine: Arc<dyn lmgw_machine::Facts>,
    ) -> anyhow::Result<SharedState> {
        let start = crate::config::deployment::Start::desktop();
        Self::init_started_with_host(data_dir, dev, start, runner, probe, machine).await
    }

    /// [`Self::init_started`] through the host seams of
    /// [`Self::init_with_host`]. `pub` (hidden) for the integration tests.
    #[doc(hidden)]
    pub async fn init_started_with_host(
        data_dir: PathBuf,
        dev: bool,
        start: crate::config::deployment::Start,
        runner: Arc<dyn runtime::registry::CommandRunner>,
        probe: Arc<dyn crate::vram::GpuProbe>,
        machine: Arc<dyn lmgw_machine::Facts>,
    ) -> anyhow::Result<SharedState> {
        // Every path under the data dir may become a container mount.
        let data_dir = absolute_dir(data_dir);
        // A new data dir also gets its models dirs (`fresh_install`).
        let db = fresh_install::open_db(&data_dir, dev, start.deployment).await?;
        // The container's pinned and seeded settings, before the snapshot is
        // first loaded: everything below reads `bind_addr` as pinned.
        let mut start_facts = crate::config::deployment::StartFacts::from(&start);
        if start.deployment.is_container() {
            start_facts.sources =
                crate::config::deployment::apply_at_start(&db, &start.env).await?;
        }
        // Their own migrations, each copied first in container mode as
        // `lmgw.sqlite` is (ruling 14, D117).
        let backups = start
            .deployment
            .is_container()
            .then(|| store::backups_dir(&data_dir));
        let corpus = store::migration_copy::around_open(
            &data_dir,
            &store::migration_copy::CORPUS,
            backups.as_deref(),
            async {
                quickdoc_core::store::open(&data_dir.join(crate::quickdoc::CORPUS_DB_FILE))
                    .await
                    .map_err(|e| anyhow::anyhow!("opening the corpus DB: {e}"))
            },
        )
        .await?;
        let knowledge = store::migration_copy::around_open(
            &data_dir,
            &store::migration_copy::KNOWLEDGE,
            backups.as_deref(),
            async {
                crate::knowledge::store::open(&data_dir.join(crate::knowledge::KNOWLEDGE_DB_FILE))
                    .await
                    .map_err(|e| anyhow::anyhow!("opening the knowledge DB: {e}"))
            },
        )
        .await?;
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
        // A set run a crash left half written (labs-store design §3.5): its
        // row is not ready, so nothing lists or runs it; its items go too.
        match store::delete_unready_lab_batches(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("deleted {n} set run(s) the last shutdown left half written"),
            Err(e) => tracing::warn!("deleting half-written set runs: {e}"),
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
        // Before anything seals: the key the seals' configuration
        // fingerprints are keyed by (`ir::seal`, WP8 verify V-N2).
        crate::ir::install_seal_key(
            store::seal_key(&db)
                .await
                .map_err(|e| anyhow::anyhow!("reading the seal key: {e}"))?,
        );
        if dev {
            tracing::info!(
                "dev instance (LMGW_DEV): image deletion and keep_runs pruning are refused — the \
                 podman image store is shared with production"
            );
        }
        let snapshot = store::load_snapshot(&db)
            .await
            .map_err(|e| anyhow::anyhow!("loading config snapshot: {e}"))?;
        crate::devices::grant_audit::warn_at_boot(&snapshot.api_keys);
        // A database restored from an older copy is found by the cursors
        // themselves: each names its record's check (review W5-4).
        let chat_feed = crate::web::chat_feed::Feed::new(&snapshot);
        // Every client but `hub_http` opts out of gzip (find-a-model design
        // D141): the feature is on for the build, and an upstream's request
        // and stream stay as they were.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .no_gzip()
            .build()?;
        let hub_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let proxy_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .build()?;
        // lmgw's own CDI spec asks the same podman whether it takes
        // `--cdi-spec-dir`.
        let checks = crate::machine::MachineChecks::system(machine.clone());
        // On Windows no container reads a CDI spec: a managed one made every
        // model start there run podman's `--help` and read NVIDIA's Linux
        // driver files, two warnings per start (Windows design §7, WP8).
        #[cfg(windows)]
        let cdi = {
            drop(machine);
            runtime::cdi::OwnSpec::unmanaged(
                "engines run as native processes on Windows: no container reads a CDI spec",
            )
        };
        #[cfg(not(windows))]
        let cdi = runtime::cdi::OwnSpec::managed(machine, data_dir.clone(), runner.clone());
        // Windows starts every engine as a native child process (Windows
        // design §4.1); Linux keeps the Podman launcher, and builds the
        // native one only through `Registry::with_launcher` (no setting
        // selects it yet).
        #[cfg(windows)]
        let native = Arc::new(
            runtime::launcher::Native::new(&data_dir)
                .map_err(|e| anyhow::anyhow!("starting the native engine launcher: {e}"))?,
        );
        #[cfg(windows)]
        let registry = runtime::registry::Registry::with_launcher(
            native.clone(),
            runner,
            http.clone(),
            Arc::new(runtime::registry::ephemeral_port),
        );
        #[cfg(not(windows))]
        let registry = runtime::registry::Registry::new(runner, http.clone());
        let app = Arc::new(Self {
            db,
            corpus,
            knowledge: crate::knowledge::Knowledge::new(knowledge),
            chat_live: crate::web::chat_live::LiveTurns::with_feed(chat_feed.live.clone()),
            chat_feed,
            media_calls: Default::default(),
            transfers: Default::default(),
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
            accounts: Default::default(),
            lab_cells: Default::default(),
            agent_meters: Default::default(),
            agent_services: Default::default(),
            mcp: McpManager::new(),
            http,
            hub_http,
            proxy_http,
            catalog: CatalogCache::default(),
            llama_facts: Default::default(),
            reasoning_learned: Default::default(),
            upstream_quota: Default::default(),
            gguf_cache: GgufSummaryCache::default(),
            hf_headers: Default::default(),
            vram: VramScheduler::with_probe(probe),
            kv_pools: Default::default(),
            runtime: ArcSwap::from_pointee(registry),
            cdi: ArcSwap::from_pointee(cdi),
            machine: checks,
            agent_spawner: std::sync::Mutex::new(Arc::new(crate::agents::container::TokioSpawner)),
            turn_score_hook: std::sync::Mutex::new(None),
            started_at: Instant::now(),
            started_at_utc: chrono::Utc::now(),
            background: Default::default(),
            stops: Default::default(),
            calls: Default::default(),
            main_tls: Default::default(),
            proxy_seen: Default::default(),
            data_dir,
            _test_dir: None,
            settings_write: tokio::sync::Mutex::new(()),
            routing_write: Default::default(),
            snapshot_loads: SnapshotLoads::default(),
            dev: std::sync::atomic::AtomicBool::new(dev),
            start: std::sync::RwLock::new(start_facts),
            update_notice: Default::default(),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_folder_locks: Default::default(),
            devices: Default::default(),
            me: Default::default(),
        });
        app.builds.set_instance_id(instance);
        // The device link's pairing as the store left it (device link design
        // §2.2, §3.3): the URL hosts every server certificate carries, and the
        // tokens alive, while which a handshake may come without a
        // certificate.
        crate::devices::published(&app, &app.snapshot());
        crate::devices::tokens::sync(&app).await;
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
        // Pulls through Podman's API, for their byte counts (machine-check
        // design §5): a private service on this instance's own socket,
        // started when a pull needs it. Not in `init_for_tests`, for the
        // reason the forge above is not: a test must not start a podman
        // service or reach a registry.
        app.builds
            .set_pull_api(Arc::new(crate::backends::pull::ServicePull::for_instance(
                app.builds.instance_id(),
            )));
        // The two owner rows (principals §6), before anything else reads the
        // snapshot: `resync_all` below and the shell's window both want a
        // gateway that already has its door key.
        crate::agents::token::seed_owner_keys(&app).await?;
        // The audit log's retention (headless server design §5.8), for days
        // that passed while nothing was written.
        crate::audit::prune_at_start(&app).await;
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
        // The native launcher chooses the engine build, which an image row's
        // learned figures are keyed by (`vram::plan::image::engine_key`): its
        // choice, from facts read once now and again at every start.
        #[cfg(windows)]
        {
            let chooser = native.clone();
            app.vram
                .set_engine_build(Arc::new(move |class| chooser.expected_build(class)));
            tokio::spawn(async move { native.read_facts().await });
        }
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
        let test_dir = TestDir::new(dir.clone());
        let instance = crate::backends::run::load_instance_id(&db, &dir).await;
        // As a start does; the first gateway's key stays this process's
        // (`ir::seal`'s key module).
        crate::ir::install_seal_key(
            store::seal_key(&db)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        );
        let app = Arc::new(Self {
            db,
            corpus,
            knowledge: crate::knowledge::Knowledge::new(knowledge),
            chat_live: crate::web::chat_live::LiveTurns::with_feed(chat_feed.live.clone()),
            chat_feed,
            media_calls: Default::default(),
            transfers: Default::default(),
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
            accounts: Default::default(),
            lab_cells: Default::default(),
            agent_meters: Default::default(),
            agent_services: Default::default(),
            mcp: McpManager::new(),
            http: reqwest::Client::builder()
                .no_gzip()
                .build()
                .expect("a default client always builds"),
            hub_http: reqwest::Client::new(),
            proxy_http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_gzip()
                .build()
                .expect("a client with no redirect policy always builds"),
            catalog: CatalogCache::default(),
            llama_facts: Default::default(),
            reasoning_learned: Default::default(),
            upstream_quota: Default::default(),
            gguf_cache: GgufSummaryCache::default(),
            hf_headers: Default::default(),
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
            // No CDI spec and no `--cdi-spec-dir`, for the reason the registry
            // above has no podman: a test must not run `nvidia-ctk` on the
            // machine it runs on, nor write into its `XDG_RUNTIME_DIR`. A test
            // of the spec installs a managed one on a stated machine with
            // `set_cdi_for_tests`.
            cdi: ArcSwap::from_pointee(runtime::cdi::OwnSpec::unmanaged(
                "test state: no CDI spec managed (see AppState::set_cdi_for_tests)",
            )),
            // A blank machine, for the same reason: a test's check reads
            // nothing of the computer it runs on unless it states one.
            machine: crate::machine::MachineChecks::blank(),
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
            calls: Default::default(),
            main_tls: Default::default(),
            proxy_seen: Default::default(),
            _test_dir: Some(test_dir),
            data_dir: dir,
            settings_write: tokio::sync::Mutex::new(()),
            routing_write: Default::default(),
            snapshot_loads: SnapshotLoads::default(),
            // Never read from the environment: a developer running the suite
            // from a dev shell must not get a different gateway. A test that
            // wants one says so with `set_dev_for_tests`.
            dev: std::sync::atomic::AtomicBool::new(false),
            start: Default::default(),
            update_notice: Default::default(),
            builds: Default::default(),
            bench: Default::default(),
            chat_temp: Default::default(),
            chat_folder_locks: Default::default(),
            devices: Default::default(),
            me: Default::default(),
        });
        app.builds.set_instance_id(instance);
        // The device link's pairing as the store left it (device link design
        // §2.2, §3.3): the URL hosts every server certificate carries, and the
        // tokens alive, while which a handshake may come without a
        // certificate.
        crate::devices::published(&app, &app.snapshot());
        crate::devices::tokens::sync(&app).await;
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

    /// What this gateway's start was told and did (headless server design
    /// D2, §2.2): the deployment, the unit's stop timeout, and where each
    /// pinned setting's value came from.
    pub fn start_facts(&self) -> crate::config::deployment::StartFacts {
        *self.start.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Where this lmgw runs (headless server design D2).
    pub fn deployment(&self) -> crate::config::deployment::Deployment {
        self.start_facts().deployment
    }

    /// The update notice the last check of the feed left (§6.3).
    pub fn update_notice(&self) -> Option<lmgw_api_types::UpdateNotice> {
        self.update_notice
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Keep `notice` as the update notice; the one it replaces comes back.
    pub(crate) fn set_update_notice(
        &self,
        notice: Option<lmgw_api_types::UpdateNotice>,
    ) -> Option<lmgw_api_types::UpdateNotice> {
        std::mem::replace(
            &mut *self
                .update_notice
                .write()
                .unwrap_or_else(|p| p.into_inner()),
            notice,
        )
    }

    /// The quit's length against the unit's stop timeout (D20), with the
    /// unload timeout stored now; `None` on the desktop, which has no unit.
    pub fn stop_budget(&self) -> Option<crate::config::deployment::StopBudget> {
        let facts = self.start_facts();
        facts.deployment.is_container().then(|| {
            crate::config::deployment::stop_budget(
                facts.stop_timeout,
                self.snapshot().settings.vram.unload_timeout_seconds,
            )
        })
    }

    /// Test-only: what this gateway's start was told. `pub` (unconditional)
    /// to match the other `set_*_for_tests` seams.
    #[doc(hidden)]
    pub fn set_start_for_tests(&self, facts: crate::config::deployment::StartFacts) {
        *self.start.write().unwrap_or_else(|p| p.into_inner()) = facts;
        // Accounts on follows container mode (D25): republished at once.
        self.publish_over(Snapshot::clone);
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
        self.wire_cdi(&self.cdi());
        registry.set_cdi(self.cdi());
        let me = self.me.get().cloned();
        registry.set_on_started(Box::new(move || {
            if let Some(state) = me.as_ref().and_then(std::sync::Weak::upgrade) {
                state.vram.cache_pids(&state);
                crate::vram::broadcast(&state);
            }
        }));
    }

    /// lmgw's own CDI spec ([`runtime::cdi`]).
    pub fn cdi(&self) -> Arc<runtime::cdi::OwnSpec> {
        self.cdi.load_full()
    }

    /// What the spec learns from the state around it: the load timeout a
    /// throwaway probe's write may take, read from the settings when it runs.
    fn wire_cdi(&self, cdi: &runtime::cdi::OwnSpec) {
        let me = self.me.get().cloned();
        cdi.set_load_timeout(move || {
            me.as_ref()
                .and_then(std::sync::Weak::upgrade)
                .map(|state| runtime::cdi::load_timeout(&state))
                .unwrap_or_else(|| {
                    std::time::Duration::from_secs(
                        crate::config::VramSettings::default().load_timeout_seconds,
                    )
                })
        });
    }

    /// Test-only: manage the CDI spec as `cdi` says (a managed one on a
    /// stated machine), for this state and its registry. `pub`
    /// (unconditional) to match the `set_runtime_for_tests` convention.
    #[doc(hidden)]
    pub fn set_cdi_for_tests(&self, cdi: Arc<runtime::cdi::OwnSpec>) {
        self.wire_cdi(&cdi);
        self.runtime().set_cdi(cdi.clone());
        self.cdi.store(cdi);
    }

    /// How a container agent reaches `podman` (container-runtime §6.1): the
    /// spawner installed, behind lmgw's CDI check
    /// ([`crate::agents::container::CdiChecked`]), which refuses a podman
    /// call that gives a container the graphics card without lmgw's spec dir.
    pub fn agent_spawner(&self) -> Arc<dyn crate::agents::container::Spawner> {
        Arc::new(crate::agents::container::CdiChecked {
            inner: self.agent_spawner.lock().unwrap().clone(),
            cdi: self.cdi(),
        })
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
        // The Chat feed's `hold` says a benchmark's lease (chat-plane design
        // D6), so a lease taken or released is a `hold` event.
        self.chat_feed.published(|| self.snapshot());
    }

    /// Publish the main TLS listener that serves now, or that none does
    /// (headless server design §4.3): runtime state on the snapshot, as the
    /// GPU lease is, read by every URL builder. Called by
    /// `server::main_tls::TlsListenerSlot` under its own lock, so a start
    /// and an older start's stop never publish out of order.
    pub fn set_serving_tls(&self, tls: Option<crate::net::ServingTls>) {
        self.snapshot.rcu(|cur| {
            let mut next = Snapshot::clone(cur);
            next.serving_tls = tls.clone();
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
                KeyWritten::Pending => {
                    if let Some(k) = next.api_keys.iter_mut().find(|k| k.id == id) {
                        k.key_hash = crate::config::DeviceLink::pending_hash();
                        k.link.confirmed_at = None;
                        k.link.waiting = None;
                    }
                }
                KeyWritten::Confirmed { key_hash } => {
                    if let Some(k) = next.api_keys.iter_mut().find(|k| k.id == id) {
                        k.key_hash = key_hash.clone();
                        k.link.waiting = None;
                        k.link.confirmed_at = Some(crate::store::stamp(chrono::Utc::now()));
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
    ///
    /// A save that switches the update check off clears the server image's
    /// update notice (headless server design §6.3).
    pub async fn settings_saved(&self, s: &crate::config::Settings) -> SettingsPublished {
        crate::update::notice::switched(
            self,
            self.snapshot().settings.update_check_enabled,
            s.update_check_enabled,
        );
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

    /// Publish `snap` with whatever lease and serving TLS listener the
    /// snapshot it replaces carries. `rcu`, so a lease taken or released (a
    /// TLS listener brought up or stopped) while `snap` was being loaded is
    /// never lost or revived: the swap retries against the newer snapshot.
    fn store_carrying_lease(&self, snap: Snapshot) -> Arc<Snapshot> {
        self.publish_over(|cur| {
            let mut next = snap.clone();
            next.gpu_lease = cur.gpu_lease.clone();
            next.serving_tls = cur.serving_tls.clone();
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
            let mut next = next(cur);
            // Proxy mode's networks, read once per publish rather than per
            // request (headless server design §4.5).
            next.trusted_nets = crate::config::trusted_nets(&next.settings.trusted_proxies);
            // Accounts on (D25) and the session desk (§5.2), per publish.
            next.accounts_on = self.deployment().is_container() || next.settings.accounts.enabled;
            next.sessions = self.accounts.sessions.clone();
            let next = Arc::new(next);
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
        // The machine check's §1.4 and its verdict follow the settings and
        // rows just published, whichever path saved them (machine-check
        // design D135): the switch it offers is the one these give. The live
        // feed hears of the change (D183); a turn to `ready` begins the
        // pre-fetch, as any does.
        let refit = self.machine.refit(&published);
        if let Some((summary, _)) = &refit {
            self.telemetry.machine(summary.clone());
        }
        if refit.is_some_and(|(_, turned_ready)| turned_ready) {
            match self.me.get().and_then(std::sync::Weak::upgrade) {
                Some(me) if tokio::runtime::Handle::try_current().is_ok() => {
                    crate::machine::begin_prefetch(&me)
                }
                _ => tracing::info!(
                    "the machine check turned ready with the settings just saved, and the \
                     engine pre-fetch could not begin from here; each model's first start \
                     downloads its engine"
                ),
            }
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

    /// [`Self::reload_snapshot`] for a routing write: `routing`
    /// ([`Self::routing_write`]) is let go once the snapshot is published,
    /// before the MCP reconcile — which can take a server's whole connect
    /// timeout, and changes nothing a fallback reaches — and on a failed
    /// load too.
    pub(crate) async fn reload_snapshot_releasing(
        &self,
        routing: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Arc<Snapshot>, GatewayError> {
        let published = self.publish_snapshot().await;
        drop(routing);
        let snap = published?;
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
        let badge_before = crate::backends::updates::badge_image_names(&self.snapshot());
        let snap = self.publish_loaded(load, store::load_snapshot(&self.db).await?);
        // The server certificates carry every device's URL hosts (device
        // link design §2.2).
        crate::devices::published(self, &snap);
        // Every revocation watch re-reads its key in what was just published
        // (client-apps design §1.6): a key disabled, rotated or deleted by any
        // path — the Keys page, an agent's page, a restore — ends the
        // connections and streams opened with it, not only the ones whose op
        // raised the signal by name.
        self.devices.revocations.rearm();
        // A model's ctx-size, cache types or GGUF path may have just changed,
        // and every one of those changes what it costs on the GPU (§9b).
        self.vram.forget_plans().await;
        // The nav's engine-update badge counts the images a model row runs
        // (container-builds §8): a row that starts or stops running one moves
        // it without a registry check, so the open dashboards hear it now.
        if crate::backends::updates::badge_image_names(&snap) != badge_before {
            crate::backends::updates::publish(self).await;
        }
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        absolute_dir, dev_flag, is_installed_data_dir, wrap_key_place, AppState, WrapKeyPlace,
    };
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
        // Joined part by part: Windows' `absolute` writes its own separator.
        assert_eq!(
            absolute_dir(PathBuf::from("target/dev-copy")),
            cwd.join("target").join("dev-copy")
        );
        // An absolute dir stays as it is; a path with no drive is not one on
        // Windows.
        let abs = if cfg!(windows) {
            r"C:\srv\lmgw"
        } else {
            "/srv/lmgw"
        };
        assert_eq!(absolute_dir(PathBuf::from(abs)), PathBuf::from(abs));
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
        #[cfg(unix)]
        {
            let link = home.path().join("link");
            std::os::unix::fs::symlink(&installed, &link).unwrap();
            assert!(is_installed_data_dir(&link, moved, Some(&installed)));
        }
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

    /// A dev instance never reaches the desktop's Secret Service (device
    /// link design D2): it gets no store, so its device CA's key is plain.
    #[test]
    fn only_a_production_start_reaches_the_secret_service() {
        use crate::config::deployment::Deployment;
        assert_eq!(
            wrap_key_place(true, Deployment::Desktop),
            WrapKeyPlace::Plain
        );
        assert_eq!(
            wrap_key_place(false, Deployment::Desktop),
            WrapKeyPlace::SecretService
        );
        // Container mode asks no Secret Service (headless server design §9):
        // it reads the image's secret, whatever `LMGW_DEV` says (D100).
        for dev in [false, true] {
            assert_eq!(
                wrap_key_place(dev, Deployment::Container),
                WrapKeyPlace::ImageSecret
            );
        }
    }
}
