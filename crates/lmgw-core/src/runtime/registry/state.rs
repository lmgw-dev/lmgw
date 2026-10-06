//! Registry state

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;

use crate::egress::llama_cpp::props::LlamaFacts;
use crate::gate::facts::GateFacts;

use super::*;
use crate::runtime::descriptor::{RungCharge, RungPos};
use crate::runtime::image::ImageCapabilities;
use crate::runtime::{Class, Placement};

pub(super) type Key = (Class, String);

/// Source of [`Entry::generation`]. Process-wide rather than per registry so
/// a generation never repeats even across a test's registry swap.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Lifecycle state of one registry entry (§3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Starting,
    Ready,
    Stopping,
}

impl RuntimeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
        }
    }
}

/// What a waiter is told about the entry it is parked on. Cloned out of a
/// `watch` channel, so it must stay cheap.
#[derive(Debug, Clone)]
pub(super) enum Phase {
    /// The claiming task is still in the start sequence.
    Starting,
    /// A ladder climb is replacing the container (ladder design §3.4): from
    /// its mark, through the drain and the replacement admission, to the
    /// new rung's start. Waited on exactly like [`Self::Starting`] — the
    /// model is being brought up again, on another rung.
    Climbing,
    /// The container answered `/health`; the entry is `ready`.
    ///
    /// Deliberately carries no port: a waiter must re-read the entry under
    /// the lock anyway, because between this notification and the re-read the
    /// model can have been stopped and restarted on a different one. A port
    /// handed out here would be a value that is right often enough to be
    /// trusted and wrong exactly when it matters.
    Ready,
    /// The entry is gone. `Some` = the start failed, and this is why (the
    /// waiter fails with it rather than silently starting a second time);
    /// `None` = it was stopped, or the claim was abandoned — either way a
    /// fresh start is the right next move.
    Gone(Option<String>),
    /// A climb's start of the new rung failed, and the entry is gone
    /// (ladder design §12 entry 22). Not [`Self::Gone`]: a waiter wanted the
    /// model, not that rung, so it neither fails with the climb's error nor
    /// starts the model itself from inside the registry — it goes back to
    /// admission ([`RuntimeError::ClimbFailed`]), which cold-starts the base
    /// through the VRAM decision like any other start. `rung` is the rung that
    /// would not start and `cause` the start's own error: a trigger that
    /// joined that very climb is told it failed instead (§12 entry 52).
    ClimbFailed {
        rung: Option<RungPos>,
        cause: String,
    },
}

pub(super) struct Entry {
    /// Which container this entry is: a fresh number for every claim and
    /// every adoption, so a model stopped and started again — the same name,
    /// a new container, a new host PID — is a different generation. What
    /// per-process attribution caches a container's `State.Pid` against
    /// (candidate-aliases design §4.7).
    pub(super) generation: u64,
    pub(super) container_name: String,
    /// `0` until the start sequence has allocated one. Reported as-is in
    /// [`RuntimeView`] so a status page can tell "starting, no port yet" from
    /// a live entry rather than being shown a plausible-looking lie.
    pub(super) host_port: u16,
    pub(super) state: RuntimeState,
    pub(super) started_at: Instant,
    pub(super) in_flight: u32,
    pub(super) last_used: Instant,
    /// `vram.unload_timeout_seconds`, captured when the entry was claimed.
    ///
    /// Kept on the entry so [`Registry::stop`] keeps the `(class, model_id,
    /// force)` signature the design gives it — the reaper, the eviction path
    /// and the ops surface all call it without having to carry a settings
    /// snapshot to a container they only want gone.
    pub(super) stop_timeout: Duration,
    /// Identity of this entry *and* the channel its waiters are parked on.
    /// Identity matters: by the time a claim finishes, the map may hold a
    /// different entry for the same key (stopped and restarted underneath
    /// it), and removing that one would strand a live container. Every
    /// removal is guarded by an `Arc::ptr_eq` against this handle.
    pub(super) phase: Arc<watch::Sender<Phase>>,
    /// What was wrong with an otherwise successful start (image-generation
    /// design §3): a capabilities route that threw, a row whose declared
    /// modes the loaded pipeline does not back, a flag vocabulary that had to
    /// fall back to the embedded one. Never a reason to fail — a model that
    /// generates is ready — but never silent either.
    pub(super) warnings: Vec<String>,
    /// The `GET /sdcpp/v1/capabilities` body this container answered with,
    /// for image models that got a 200. Not persisted: it describes the
    /// process, so it is re-read on every start and dropped with the entry.
    pub(super) capabilities: Option<ImageCapabilities>,
    /// What a llama-server container said about itself (`GET /props`) and
    /// what its start row says about its projector (llama egress design
    /// §4.2, §8.2; `llama_props.rs`). `None` for every other engine, and
    /// while the entry is `starting` or climbing. Re-read on every start,
    /// climb and adoption, and dropped with the entry.
    pub(super) llama: Option<LlamaEntry>,
    /// What the request gate reads about this container — the row it was
    /// **started** with, never the row as it has been edited since (see
    /// [`GateFacts`]'s module doc for why that difference aborts requests).
    /// `None` for every class but chat, and while the entry is `starting`.
    pub(super) gate: Option<Arc<GateFacts>>,
    /// The rung this container was **started** (or adopted) at, with the
    /// file and context it runs — what the VRAM ledger charges (ladder
    /// design §5). Set where the claim is taken, from the very descriptor the
    /// start renders, so a `starting` entry is charged at its rung from the
    /// first ledger read. `None` on a row without a ladder, which the ledger
    /// then sizes from the row exactly as before.
    pub(super) charge: Option<RungCharge>,
    /// The configuration an audio container was **started** (or adopted)
    /// with, as its learned residency is keyed
    /// ([`ModelRuntime::resident_key`](crate::runtime::descriptor::ModelRuntime::resident_key),
    /// realtime design §9.4): a reading from a container that still runs a
    /// previous configuration — an edit while it was busy, a class image
    /// changed without a restart — must not teach the row's current one.
    /// `None` for every other class.
    pub(super) resident_key: Option<String>,
    /// Where this container computes, as it was **started** (or adopted)
    /// ([`ModelRuntime::placement`](crate::runtime::descriptor::ModelRuntime::placement)):
    /// a row switched to the CPU while its container kept running is still
    /// on the GPU until that container stops. What the ledger, the hold
    /// sweep and the residency read for a running container.
    pub(super) placement: Placement,
    /// Sends in flight on this container, as the ladder gate counts them
    /// ([`SendGuard`]) — what a climb's drain waits for (ladder design §12
    /// entry 10). Not [`Self::in_flight`]: that counts claims, and a turn
    /// runner keeps its claim idle across a whole tool loop. A `watch` so the
    /// drain wakes on the last release instead of polling; dropping it with
    /// the entry is how a drain learns the entry is gone.
    pub(super) sends: watch::Sender<u32>,
    /// A climb in progress, from its mark until the new rung settles. While
    /// it is set no claim and no send is taken on the entry: acquirers park
    /// on [`Self::phase`], and [`Registry::begin_send`] refuses.
    pub(super) climb: Option<ClimbMark>,
    /// Whom this container runs for (candidate-aliases design §4.4): the
    /// origin of the claim that started it, turned `Owner` by any claim of
    /// the owner's ([`ownership`]'s module doc). Never reset while the entry
    /// lives — a climb keeps it — and gone with the entry.
    pub(super) owner: Origin,
}

/// Serializable snapshot of one entry, for the status/ops surfaces (§8).
///
/// `PartialEq` so the server's status tick can broadcast the runtime frame
/// **on change** rather than every five seconds. The two age fields are
/// seconds, so an idle registry compares equal tick after tick and publishes
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeView {
    pub class: Class,
    pub model_id: String,
    pub container_name: String,
    /// [`Entry::generation`]. Not published: it identifies a container to
    /// the VRAM attribution cache, and means nothing to a surface.
    #[serde(skip)]
    pub generation: u64,
    /// `0` while the entry is `starting` — no port has been allocated yet.
    pub port: u16,
    pub state: RuntimeState,
    pub in_flight: u32,
    /// Seconds since the entry became `ready` (since it was claimed, while
    /// still starting). An age rather than a timestamp: `Instant` has no
    /// wall-clock meaning to serialize, and every consumer wants the age.
    pub started_at_age_seconds: u64,
    /// Seconds since the last acquire or guard release — the idle reaper's
    /// (§3.7) input, and the LRU key eviction ranks on (§4).
    pub last_used_age_seconds: u64,
    /// Non-fatal problems this model's start found (image-generation §3).
    /// Empty for a clean one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// What an image container reported about its loaded pipeline. `None` for
    /// every other class, and for an image container whose capabilities route
    /// answered anything but a 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_capabilities: Option<ImageCapabilities>,
    /// What a llama-server container said about itself in `GET /props` (llama
    /// egress design §4): its modalities, slot context, template caps and
    /// build. Absent for every other engine, and when the read failed — the
    /// warnings above say why — so a frame without llama containers is byte
    /// for byte what it was.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "llama_props::serialize_facts"
    )]
    pub llama_props: Option<Arc<LlamaFacts>>,
    /// The ladder rung this container runs, 1-based like every outside
    /// surface (ladder design §6, §12 entry 11). Absent without a ladder, so
    /// such a row's frame is byte-for-byte what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung: Option<RungStatus>,
    /// A climb in progress: to which rung, why, and what it is doing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub climbing: Option<ClimbStatus>,
    /// Sends in flight as the ladder gate counts them — what a climb drains.
    /// Only ladder sends are counted, so this is zero (and absent) on every
    /// other row.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sends: u32,
    /// What the ledger charges for this container ([`Entry::charge`]). Not
    /// published: [`Self::rung`] is its outside form.
    #[serde(skip)]
    pub charge: Option<RungCharge>,
    /// [`Entry::resident_key`]. Not published: it identifies a configuration
    /// to the audio residency, and means nothing to a surface.
    #[serde(skip)]
    pub resident_key: Option<String>,
    /// [`Entry::placement`]. Published only when `cpu`, so a frame of GPU
    /// containers is byte for byte what it was.
    #[serde(default, skip_serializing_if = "Placement::is_gpu")]
    pub placement: Placement,
    /// Whom the container runs for ([`Entry::owner`], candidate-aliases
    /// design §4.4, §6). Absent for the owner's, so a frame without
    /// background traffic is byte for byte what it was.
    #[serde(default, skip_serializing_if = "Origin::is_owner")]
    pub owner: Origin,
    /// An owner admission is waiting for room, and background traffic takes
    /// no new work on this model until it is over (§4.5, §12 entry 47).
    /// Absent when not.
    #[serde(default, skip_serializing_if = "ownership::is_false")]
    pub draining_for_owner: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}
