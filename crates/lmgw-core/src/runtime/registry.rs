//! The container runtime registry and the `acquire` primitive (design §3.2,
//! §3.5, §3.6).
//!
//! The failure this exists for: with one container per model, "is the model
//! up?" and "forward to it" are two different moments, and between them the
//! idle reaper, an eviction or an explicit `stop` can take the container
//! away. A request that lost that race gets a connection refused against a
//! port that was valid microseconds ago. So residency and the claim on it are
//! decided *together*, under one lock: an [`AcquireGuard`] is proof that the
//! container was up at the instant the claim was taken and that nothing may
//! stop it until the guard drops.
//!
//! **The map lock is never held across an await.** It guards a `HashMap` and
//! nothing else — every podman call, every `/health` probe and every sleep
//! happens with the lock released. That is not a micro-optimization: it is
//! what makes starts of *different* models overlap, which is the §4 fix for
//! the queueing the spike measured (§10.6). A start that held the lock would
//! reintroduce exactly the serialization the design set out to remove.
//!
//! **One start per model, many waiters.** The task that finds a model absent
//! claims it by inserting a `starting` entry and then does the work outside
//! the lock; everyone else who wants the same model finds that entry, clones
//! its `watch` channel and awaits the verdict. So N concurrent requests for
//! one cold model produce exactly one `podman run`, and N requests for N
//! models produce N concurrent `podman run`s.
//!
//! **Every claim is released, including a cancelled one.** The claim is an
//! RAII token: if the acquiring future is dropped mid-start (client hung up),
//! its `Drop` unclaims the entry and wakes the waiters so one of them takes
//! over, instead of leaving a model wedged in `starting` until restart. Same
//! discipline as [`crate::vram::LocalHold`], one level lower.
//!
//! **Boot reconciliation lives here too** (§3.4): [`Registry::reconcile`]
//! rebuilds this map from `podman ps` at startup, adopting containers that
//! still match what lmgw would render for them and removing everything else.
//! The policy *around* it — which models to warm-start, when to reap, the
//! legacy sweep — is [`super::lifecycle`]'s; this module only owns the podman
//! verbs and the map.
//!
//! What is *not* here, on purpose: the scheduler's arbitration around
//! `acquire` (§4) is [`crate::vram`]'s. This module is the machinery it
//! drives.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sdcpp_caps::SdcppCaps;

mod climb;
mod ownership;

use climb::ClimbMark;
pub use climb::{
    ClaimStatus, ClimbRun, ClimbStart, ClimbStatus, ClimbTicket, DrainEnd, Marked, PhaseWait,
    RungStatus, SendGuard, Settled,
};
pub use ownership::{Origin, OwnerDrain};

mod acquire;
mod command;
mod errors;
mod inputs;
mod lease;
mod podman_ops;
mod raii;
mod reconcile;
mod sdcpp;
mod state;
mod stop;

use acquire::Started;
pub use command::{
    ephemeral_port, llama_server_candidates, throwaway_args, without_gpus, CmdOutput,
    CommandRunner, ImageFacts, NoRuntime, PortAllocator, TokioRunner, HIDE_GPUS,
};
pub use errors::RuntimeError;
pub use inputs::{AcquireSpec, StartSpec};
pub use podman_ops::log_excerpt_of;
pub use raii::AcquireGuard;
pub use reconcile::{Presence, PsEntry, ReconcileReport, HOST_PID_FORMAT};
use state::{next_generation, Entry, Key, Phase};
pub use state::{RuntimeState, RuntimeView};

/// A generation for a container this registry does not hold — a benchmark
/// run's (benchmark design §3.4) — drawn from the entries' own counter, so
/// VRAM attribution's per-generation PID cache
/// ([`crate::vram::attribution`]) can key it beside the registry's entries
/// without ever mistaking one for the other.
pub fn outside_generation() -> u64 {
    next_generation()
}

/// How often a starting container's `/health` is probed.
///
/// A sampling rate, not a bound on anything — the poll runs until the
/// caller's `vram.load_timeout_seconds` is spent, this only says how often it
/// looks. Deliberately the same 250 ms `vram::POLL` uses for the same job;
/// that constant is private to `vram`, so it is restated here rather than
/// made public across the seam.
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// How often a starting container's own state is read while its readiness
/// route stays silent.
///
/// Measured 2026-09-24: a model whose context did not fit aborted
/// llama-server five seconds into the load, the container exited with code 1,
/// and the start still waited out all of `vram.load_timeout_seconds` (600 s)
/// before answering 502, because a refused connection reads as "still
/// starting" (§10.3). The port cannot tell a slow load from a dead one; podman
/// can. Also a sampling rate, not a bound: a `podman inspect` costs a process
/// spawn, which the 250 ms health poll should not pay each time, and a crash
/// is then reported at most this late.
const CONTAINER_EXIT_POLL: Duration = Duration::from_secs(2);

/// `podman stop -t` grace, in seconds (§3.6).
///
/// Measured (§10.1): a direct-mode llama-server exits on SIGTERM in ~0.4s
/// even mid-generation, so this is a ceiling that is never normally reached —
/// the SIGTERM-deafness that made the full grace mandatory was router-mode
/// specific. Spelled out rather than left to podman's identical default so
/// that the value is visible in the argv a test (and `podman logs`) can read.
const STOP_GRACE_SECONDS: u64 = 10;

/// How many container log lines a failed start reads back, and how many of
/// them the error carries. Same shape (and same reason) as
/// [`crate::modelinfo`]'s post-mortem tail: with N containers this excerpt is
/// often the only place the actual failure is visible.
pub const LOG_TAIL: usize = 60;
const LOG_EXCERPT_LINES: usize = 12;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-memory map of what lmgw believes is running, one entry per model
/// (§3.2). Never persisted: it is rebuilt from `podman ps` at boot (§3.4).
pub struct Registry {
    runner: Arc<dyn CommandRunner>,
    http: reqwest::Client,
    port: PortAllocator,
    map: Mutex<HashMap<Key, Entry>>,
    /// `llama-server --help` per **image ID** (§3.6, "probe / help").
    ///
    /// Per image, not global, because per-model image overrides (§3.1) mean
    /// there is no single flag vocabulary on the box any more: a model's args
    /// have to validate against the help of the image *that model* runs. Read
    /// through [`Registry::help_text`], which fills it lazily from a throwaway
    /// container.
    ///
    /// Keyed by the ID the reference resolves to *now*
    /// ([`Registry::image_facts`], asked on every lookup), never by the
    /// reference itself: `localhost/llama-server-cuda:official-latest` is a
    /// moving tag that `build.sh` retags under a running lmgw, and a
    /// string key kept validating against the old binary's flags until the
    /// next restart. A retag is a new ID and therefore a miss, with no
    /// invalidation step anyone has to remember; [`Registry::forget_help`]
    /// exists to drop an entry, not to keep one correct.
    help: Mutex<HashMap<String, String>>,
    /// `sd-server --help`, parsed, per **image ID** — the image class's twin
    /// of [`Self::help`], keyed the same way for the same reason, and cached
    /// for one more: the probe needs the GPU device attached (§12.7), so it is
    /// not merely slow to repeat, it can fail on a box whose card is busy. A
    /// parsed entry therefore outlives the condition that produced it, and a
    /// failed probe is not cached at all — the next start tries again.
    sdcpp_help: Mutex<HashMap<String, Arc<SdcppCaps>>>,
    /// Owner admissions waiting for room right now ([`OwnerDrain`],
    /// candidate-aliases design §4.5): while nonzero, every resident model
    /// is draining for the owner.
    owner_waits: AtomicUsize,
    /// A benchmark run's GPU lease, mirrored here from the snapshot
    /// ([`Registry::set_gpu_lease`], `lease.rs`). Only ever read or written
    /// with [`Self::map`] held.
    gpu_lease: Mutex<Option<Arc<crate::bench::lease::GpuLease>>>,
}

fn is_no_such_container(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    low.contains("no such container") || low.contains("no container with name")
}
