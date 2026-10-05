//! The world the fake driver, the fake podman and the fake containers share

use super::*;

pub(super) const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Default)]
pub(super) struct World {
    /// model id -> bytes it occupies while its container runs.
    pub(super) size: HashMap<String, u64>,
    /// Models whose container is up right now.
    pub(super) loaded: HashSet<String>,
    /// Container name -> model id, learned from the `--label lmgw.model=` the
    /// registry puts on every `podman run` (§3.3).
    pub(super) names: HashMap<String, String>,
    /// Host port -> model id, learned from `-p <port>:8080`. This is what lets
    /// one wiremock answer `/slots` *about the model actually running on it*
    /// without the test having to pin ports to models.
    pub(super) ports: HashMap<u16, String>,
    /// Models whose `/slots` reports a generation in progress — traffic that
    /// arrived on the container port directly, which lmgw's own in-flight
    /// count cannot see (§4).
    pub(super) busy: HashSet<String>,
    /// Model ids started / stopped, in order.
    pub(super) runs: Vec<String>,
    /// Model id -> the argv of its last `podman run`: what flags its
    /// container was given (a CPU row's GPU passthrough left out).
    pub(super) argv: HashMap<String, Vec<String>>,
    pub(super) stops: Vec<String>,
    /// How many times each port's mock answered `/apply-template` /
    /// `/tokenize` — so a test can assert "no /apply-template call" on a row
    /// nothing gates yet.
    pub(super) apply_template_calls: HashMap<u16, u64>,
    pub(super) tokenize_calls: HashMap<u16, u64>,
    /// Every chat body a port's mock actually received, in order — so a test
    /// can assert the `max_tokens` llama-server was sent.
    pub(super) chat_bodies: HashMap<u16, Vec<Value>>,
    /// Unified-pool mode (ladder design §2.1 fact 3 / unified-KV design §2.1
    /// fact 3): a port with a capacity here fails an overlapping request that
    /// would push the pool's reservations over it, the same way a real
    /// shared KV pool aborts every processing slot. Absent = the plain,
    /// ungated `/v1/chat/completions` mock every existing test already relies
    /// on.
    pub(super) pool_capacity: HashMap<u16, u64>,
    /// How long a served request "holds" its reservation, per port — the
    /// window a later, overlapping request is checked against. Configurable
    /// per port so a concurrency test can make two requests overlap
    /// deterministically. Defaults to 200ms when a pool is active and the
    /// test never set one.
    pub(super) pool_delay: HashMap<u16, Duration>,
    /// `[start, start + delay)` reservation windows still on record, per
    /// port, each carrying the need (words + max_tokens) it reserved. The
    /// responder is synchronous, so this — not a real wait — is how it knows
    /// what else was "in flight" when a new request arrived.
    pub(super) pool_windows: HashMap<u16, Vec<(std::time::Instant, std::time::Instant, u64)>>,
    /// Every window a pool-mode port served, in arrival order, never pruned —
    /// so a test can assert *when* a request reached the model relative to
    /// another one's end (second review, finding 12), rather than how long it took by
    /// the wall clock.
    pub(super) pool_served: HashMap<u16, Vec<(std::time::Instant, std::time::Instant, u64)>>,
    /// How many times a pool-mode port answered with the shared-pool
    /// overflow error (fact 3) instead of forwarding.
    pub(super) pool_overflows: u64,
    /// How many times each port's `/slots` was read — so a test can wait for
    /// the pool's deferred release to have *looked* several times, rather
    /// than sleep and hope it did.
    pub(super) slots_calls: HashMap<u16, u64>,
    /// Per-process attribution (candidate-aliases design §4.7). On: the fake
    /// driver lists one process per loaded model — its container's host PID,
    /// handed out at `run` and answered by `inspect … {{.State.Pid}}` — plus
    /// one `outside` process. Off (the default, and every test written
    /// before §4.7): the driver lists no processes at all.
    pub(super) attribution: bool,
    /// Model id -> the host PID of its current container. A fresh one per
    /// `run`, like a real re-run container. Above any real `pid_max`, so the
    /// real `/proc` walk finds nothing under it.
    pub(super) pids: HashMap<String, u32>,
    /// Bytes held by a process lmgw does not own — a game, the desktop.
    pub(super) outside: u64,
    /// Models whose process the fake driver leaves out of its list: a
    /// CPU-only row, or a driver that has not caught up with a start.
    pub(super) unlisted: HashSet<String>,
    /// Models whose process stays on the card — and in the driver's list —
    /// after their container was stopped: the moment right after an
    /// eviction, before the driver lets go.
    pub(super) lingering: HashSet<String>,
    /// Every `podman inspect` for container PIDs, as the names it asked
    /// about — so a test can assert that a path asked nothing.
    pub(super) pid_inspects: Vec<Vec<String>>,
    /// Weights file name -> bytes it occupies once loaded. A `run` whose `-m`
    /// names one of these sets its model's [`Self::size`] to it, so the fake
    /// driver's `used` follows the rung a ladder model runs (ladder design
    /// §5).
    pub(super) files: HashMap<String, u64>,
    /// Every `podman run` attempt as (model, `-m` file name, `--ctx-size`),
    /// failed ones included — which rung each start rendered.
    pub(super) run_files: Vec<(String, String, Option<String>)>,
    /// A weights file whose `podman run` fails: a rung that will not start.
    pub(super) fail_run_file: Option<String>,
    /// Whether starts that set a context play llama-server's context rules
    /// ([`Self::slot_ctx`]) — on in [`ladder_fixture`], off for every test
    /// written before it (the pool tests set a context too, and keep their
    /// own mock).
    pub(super) context_rules: bool,
    /// Host port -> the per-slot context its container was started with
    /// (`--ctx-size / --parallel`), for starts that set a context while
    /// [`Self::context_rules`] is on. A port here plays llama-server's own
    /// context rules on `/v1/chat/completions` and `/v1/completions` (ladder
    /// design §2.1 facts 2–3): a prompt longer than the slot is refused with
    /// `exceed_context_size_error` before any work, and one whose prompt plus
    /// `max_tokens` overruns the slot is answered truncated. Its answers name
    /// the port, so a test can tell which rung served. Other ports answer as
    /// they always have.
    pub(super) slot_ctx: HashMap<u16, u64>,
    /// Answers the mock truncated because the slot ran out mid-answer — the
    /// failure a ladder exists to prevent.
    pub(super) truncations: u64,
    /// `exceed_context_size_error` refusals the mock gave.
    pub(super) exceeded: u64,
    /// Subtracted from every `/tokenize` count: a count that undercounts, so
    /// only llama-server's own refusal (the backstop) catches the request.
    pub(super) tokenize_undercount: u64,
    /// Per port: how long `/apply-template` takes — a count that finishes
    /// after the send has already answered.
    pub(super) apply_template_delay: HashMap<u16, Duration>,
    /// Per port: how long `/v1/chat/completions` takes — a send in flight.
    pub(super) chat_delay: HashMap<u16, Duration>,
    /// Per prompt: how long `/apply-template` takes for a prompt carrying a
    /// word that starts with the tag ([`words`] makes such prompts) — so two
    /// requests on one port reach their verdicts in a set order.
    pub(super) template_delay_by_tag: Vec<(String, Duration)>,
    /// Audio model id -> the bytes its container holds once audio.cpp has
    /// loaded the model, which it does lazily, on the first inference request
    /// (realtime design §9.4): an audio route's answer sets the model's
    /// [`Self::size`] to this. Until then the container holds whatever
    /// `size` the test gave it — its CUDA context.
    pub(super) loaded_bytes: HashMap<String, u64>,
    /// Audio inference requests the containers answered, as the model each
    /// was for.
    pub(super) audio_calls: Vec<String>,
    /// Audio models whose speech route answers as a streaming-mode model
    /// does: `text/event-stream`, this many bytes of events (WP7 review M1)
    /// — or, for a request with `stream_format: audio`, this many bytes of
    /// raw PCM as `application/octet-stream`.
    pub(super) sse_bytes: HashMap<String, usize>,
    /// Audio model id -> what its container holds while a request runs
    /// ([`Transient`]): then it drops back to [`Self::loaded_bytes`] —
    /// before it answers, the way audio.cpp frees compute buffers per
    /// request (the WP7 live gate).
    pub(super) transient: HashMap<String, Transient>,
    /// How many times the fake driver listed processes: what a sampler
    /// reads ([`Until::Sampled`]).
    pub(super) process_reads: u64,
}

/// A request's transient on an audio container ([`World::transient`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Transient {
    pub bytes: u64,
    pub until: Until,
}

/// How long a [`Transient`] lasts.
#[derive(Debug, Clone, Copy)]
pub(super) enum Until {
    /// Until the driver has been read while it lasted — the sampler's read
    /// — however long that takes: a test of what the sampler catches does
    /// not race the wall clock on a loaded box (it failed 3 of 64 runs
    /// with 32 copies at once while it lasted 150 ms).
    Sampled,
    /// This long, on the wall clock: for a request nothing should sample.
    For(Duration),
}

impl Transient {
    /// Held until the sampler read it.
    pub(super) fn sampled(bytes: u64) -> Self {
        Self {
            bytes,
            until: Until::Sampled,
        }
    }

    /// Held for `lasts`.
    pub(super) fn lasting(bytes: u64, lasts: Duration) -> Self {
        Self {
            bytes,
            until: Until::For(lasts),
        }
    }
}

/// The fake host PID of the process outside lmgw.
const OUTSIDE_PID: u32 = 7_000_000;

impl World {
    fn used(&self) -> u64 {
        let own: u64 = self
            .loaded
            .iter()
            .chain(self.lingering.iter().filter(|m| !self.loaded.contains(*m)))
            .filter_map(|id| self.size.get(id))
            .sum();
        own + self.outside
    }
}

/// A driver whose numbers follow what the containers actually hold — which is
/// what makes an eviction observably free memory rather than only look like it.
pub(super) struct FakeGpu {
    pub(super) total: u64,
    pub(super) world: Arc<Mutex<World>>,
}

impl GpuProbe for FakeGpu {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let used = self.world.lock().unwrap().used();
        Ok(vec![GpuMemory {
            index: 0,
            name: "FakeGPU".into(),
            total_bytes: self.total,
            used_bytes: used,
            free_bytes: self.total.saturating_sub(used),
        }])
    }

    fn source(&self) -> String {
        "FakeGPU".into()
    }

    fn process_support(&self) -> Result<(), String> {
        if self.world.lock().unwrap().attribution {
            Ok(())
        } else {
            Err("FakeGPU lists no processes".into())
        }
    }

    fn processes(&self, _own: &[u32], _retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        let mut w = self.world.lock().unwrap();
        w.process_reads += 1;
        if !w.attribution {
            return Err("FakeGPU lists no processes".into());
        }
        let mut out: Vec<ProcessMemory> = w
            .loaded
            .iter()
            .chain(w.lingering.iter())
            .filter(|m| !w.unlisted.contains(*m))
            .filter_map(|m| {
                Some(ProcessMemory {
                    pid: *w.pids.get(m)?,
                    bytes: w.size.get(m).copied(),
                })
            })
            .collect();
        if w.outside > 0 {
            out.push(ProcessMemory {
                pid: OUTSIDE_PID,
                bytes: Some(w.outside),
            });
        }
        out.sort_by_key(|p| p.pid);
        out.dedup_by_key(|p| p.pid);
        Ok(out)
    }
}
