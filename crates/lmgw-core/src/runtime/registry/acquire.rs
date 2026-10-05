//! Registry::acquire and the start sequence.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::gate::facts::GateFacts;

use super::raii::StartClaim;
use super::*;
use crate::runtime::argv::{podman_run_argv, render_image_args, EngineArgs};
use crate::runtime::container_name;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::image::ImageCapabilities;

impl Registry {
    pub fn new(runner: Arc<dyn CommandRunner>, http: reqwest::Client) -> Self {
        Self::with_ports(runner, http, Arc::new(ephemeral_port))
    }

    /// As [`new`](Self::new), with an explicit port source — see
    /// [`PortAllocator`].
    pub fn with_ports(
        runner: Arc<dyn CommandRunner>,
        http: reqwest::Client,
        port: PortAllocator,
    ) -> Self {
        Self {
            runner,
            http,
            port,
            map: Mutex::new(HashMap::new()),
            help: Mutex::new(HashMap::new()),
            sdcpp_help: Mutex::new(HashMap::new()),
            owner_waits: AtomicUsize::new(0),
            gpu_lease: Mutex::new(None),
        }
    }

    /// The runner every verb of this registry goes through. Container builds
    /// (container-builds §5) shell `podman` verbs that are not the model
    /// lifecycle's — `tag`, `rmi`, `images`, `system df`, the build's leftover
    /// sweep — and take them through this same seam, so one fake stands in for
    /// podman in a test and [`AppState::init_for_tests`]'s refusing runner
    /// covers them too.
    ///
    /// [`AppState::init_for_tests`]: crate::state::AppState::init_for_tests
    pub fn runner(&self) -> Arc<dyn CommandRunner> {
        self.runner.clone()
    }

    /// The map lock. Poisoning is recovered from rather than propagated: the
    /// only code that panics while holding it would be a panicking `Drop`,
    /// and letting that wedge every model on the box until restart is a far
    /// worse failure than continuing with a map that is structurally intact.
    pub(super) fn map(&self) -> MutexGuard<'_, HashMap<Key, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Residency + claim, atomically (§3.2).
    ///
    /// Ready → the in-flight count goes up under the same lock that answered
    /// "it is up", so no reaper or eviction can slip between the two.
    /// Starting → park on the claiming task's channel and re-decide when it
    /// lands. Absent → claim it and run the start sequence with the lock
    /// released.
    ///
    /// The owner's claim ([`Origin::Owner`]) — every caller but a background
    /// candidate alias's start, which is [`Self::acquire_as`].
    pub async fn acquire(
        self: &Arc<Self>,
        spec: &AcquireSpec<'_>,
    ) -> Result<AcquireGuard, RuntimeError> {
        self.acquire_as(spec, Origin::Owner).await
    }

    /// [`Self::acquire`] for a claim of `origin` (candidate-aliases design
    /// §4.4): an entry this call creates records it as its owner, and an
    /// owner's claim — the hit, or the park on a start or climb in flight —
    /// makes an existing entry the owner's ([`ownership`]).
    pub async fn acquire_as(
        self: &Arc<Self>,
        spec: &AcquireSpec<'_>,
        origin: Origin,
    ) -> Result<AcquireGuard, RuntimeError> {
        let class = spec.runtime.class;
        let key: Key = (class, spec.runtime.model_id.clone());
        loop {
            // Decide under the lock, act outside it. Everything this block
            // yields is owned, so the guard is gone before the first await.
            let decision = {
                let mut map = self.map();
                match map.get_mut(&key) {
                    Some(e) if e.state == RuntimeState::Ready && e.climb.is_none() => {
                        e.in_flight += 1;
                        e.last_used = Instant::now();
                        ownership::claimed_by(e, origin);
                        Decision::Hit {
                            port: e.host_port,
                            phase: e.phase.clone(),
                            gate: e.gate.clone(),
                            generation: e.generation,
                        }
                    }
                    // A climb is replacing the container (ladder design
                    // §3.1): park on it like on a start — the claim belongs
                    // on the rung it is bringing up, not on the one it is
                    // draining. Parked as on `starting`, so a stop that wins
                    // against the climb fails the waiter instead of letting
                    // it start the model again (§12 races). Not once a stop
                    // has taken the entry: an arrival after that wants the
                    // key free, like after any other stop (§12 entry 48).
                    Some(e) if e.climb.is_some() && e.state != RuntimeState::Stopping => {
                        ownership::claimed_by(e, origin);
                        Decision::Wait(RuntimeState::Starting, e.phase.subscribe())
                    }
                    // `starting`: somebody else is already doing the work.
                    // `stopping`: the container is on its way out and its
                    // name is still taken, so a start now would race
                    // `podman stop` — wait for the entry to go, then start
                    // fresh on the next turn of the loop.
                    Some(e) => {
                        ownership::claimed_by(e, origin);
                        Decision::Wait(e.state, e.phase.subscribe())
                    }
                    None => {
                        // A benchmark holds the card: nothing new starts on
                        // it. Asked here, under the map lock, so a start
                        // decided before the lease was taken is either in
                        // the map when the run's drain looks, or refused
                        // (`lease.rs`).
                        if let Some(lease) = self.leased_under(&map) {
                            return Err(RuntimeError::GpuBenchmark {
                                class,
                                model_id: key.1.clone(),
                                run_id: lease.run_id,
                                holder: lease.model_id.clone(),
                            });
                        }
                        let (tx, _rx) = watch::channel(Phase::Starting);
                        let phase = Arc::new(tx);
                        let name =
                            container_name(spec.container_prefix, class, &spec.runtime.model_id);
                        let now = Instant::now();
                        let generation = next_generation();
                        map.insert(
                            key.clone(),
                            Entry {
                                generation,
                                container_name: name.clone(),
                                host_port: 0,
                                state: RuntimeState::Starting,
                                started_at: now,
                                in_flight: 0,
                                last_used: now,
                                stop_timeout: spec.stop_timeout,
                                phase: phase.clone(),
                                warnings: Vec::new(),
                                capabilities: None,
                                gate: None,
                                charge: spec.runtime.rung_charge(),
                                resident_key: spec.runtime.resident_key(),
                                placement: spec.runtime.placement(),
                                sends: watch::channel(0).0,
                                climb: None,
                                owner: origin,
                            },
                        );
                        Decision::Start(StartClaim {
                            reg: Arc::clone(self),
                            key: key.clone(),
                            container_name: name,
                            phase,
                            generation,
                            settled: false,
                        })
                    }
                }
            };

            match decision {
                Decision::Hit {
                    port,
                    phase,
                    gate,
                    generation,
                } => {
                    return Ok(AcquireGuard {
                        reg: Arc::clone(self),
                        key,
                        port,
                        phase,
                        generation,
                        failed: AtomicBool::new(false),
                        gate,
                    })
                }
                Decision::Wait(parked_on, rx) => self.await_phase(&key, parked_on, rx).await?,
                Decision::Start(claim) => {
                    return match self.start_container(spec, &claim.container_name).await {
                        Ok(started) => claim.ready(started).await,
                        Err(err) => {
                            claim.fail(&err);
                            Err(err)
                        }
                    }
                }
            }
        }
    }

    /// Park until the entry we found reaches a verdict. `Ok(())` means
    /// "look again" — the caller re-decides under the lock, because whatever
    /// we were told may already be stale by the time we get there.
    ///
    /// `parked_on` is what the entry was when we subscribed, and it is what
    /// makes a `Gone(Some(_))` verdict readable. Waiting on a **starting**
    /// entry means we wanted that start: its failure is ours, and quietly
    /// launching a second attempt would turn one broken model into an endless
    /// series of doomed containers. Waiting on a **stopping** entry means we
    /// only wanted the key to become free: how the previous occupant ended is
    /// none of our business, and a fresh start is exactly what we came for.
    pub(super) async fn await_phase(
        &self,
        key: &Key,
        parked_on: RuntimeState,
        mut rx: watch::Receiver<Phase>,
    ) -> Result<(), RuntimeError> {
        loop {
            // Read the current value *before* awaiting a change: the verdict
            // may have landed between `subscribe()` and here, and a `watch`
            // receiver is created already-seen.
            let phase = rx.borrow_and_update().clone();
            match phase {
                Phase::Starting | Phase::Climbing => {}
                Phase::Ready | Phase::Gone(None) => return Ok(()),
                Phase::ClimbFailed { rung, cause } => {
                    return Err(RuntimeError::ClimbFailed {
                        class: key.0,
                        model_id: key.1.clone(),
                        message: match rung {
                            Some(r) => {
                                format!("climbing to rung {}/{} failed: {cause}", r.index + 1, r.of)
                            }
                            None => format!("its climb failed: {cause}"),
                        },
                    })
                }
                Phase::Gone(Some(_)) if parked_on == RuntimeState::Stopping => return Ok(()),
                Phase::Gone(Some(message)) => {
                    // Re-wrapped rather than forwarded: `message` is already
                    // the claiming task's rendered error, log excerpt and
                    // all, so it is carried as the message and the structured
                    // fields a waiter cannot know (which container, which log
                    // lines) are left empty instead of guessed at.
                    return Err(RuntimeError::Start {
                        class: key.0,
                        model_id: key.1.clone(),
                        container_name: String::new(),
                        message,
                        logs: Vec::new(),
                    });
                }
            }
            if rx.changed().await.is_err() {
                // Every sender is gone without a verdict — the claiming task
                // was dropped between unclaiming and sending. Its `Drop`
                // already removed the entry, so looping is what takes over.
                return Ok(());
            }
        }
    }

    /// The start sequence (§3.5, §3.6, §10.4). Never runs under the map lock.
    pub(super) async fn start_container(
        &self,
        spec: &AcquireSpec<'_>,
        name: &str,
    ) -> Result<Started, RuntimeError> {
        let class = spec.runtime.class;
        let model_id = spec.runtime.model_id.clone();
        let fail = |message: String, logs: Vec<String>| RuntimeError::Start {
            class,
            model_id: model_id.clone(),
            container_name: name.to_string(),
            message,
            logs,
        };

        if spec.models_dir.trim().is_empty() {
            return Err(fail(
                "models dir is not configured for this class".into(),
                Vec::new(),
            ));
        }

        // The flag vocabulary this model's argv is spelled against
        // (image-generation §2.6), read once per start rather than per
        // attempt. Never fatal: a failed probe falls back to the vocabulary
        // lmgw shipped with and says so on the entry, because "the GPU is
        // busy, so we could not read --help" must not be a reason a model
        // cannot start.
        let (runtime, mut warnings) = self.with_sdcpp_caps(spec).await;
        let runtime: &ModelRuntime = runtime.as_ref().unwrap_or(spec.runtime);
        // The gate's facts come from the descriptor this start renders its
        // argv from — the same value, so the ledger's pool is the pool this
        // container really has (second review, finding 1). Once per start,
        // not per port attempt.
        let gate = GateFacts::of_start(runtime, spec.models_dir).await;
        // A start on the CPU says so once: no VRAM figure will ever show for
        // it, and the thread count is the one setting that matters there.
        if let (Some(m), Some(engine)) = (&runtime.audio, &runtime.audio_settings) {
            if !runtime.placement().is_gpu() {
                tracing::info!(
                    "{}",
                    crate::runtime::audio::cpu_start_line(m, engine, crate::host::cpu())
                );
            }
        }

        // At most two attempts, and the second only for a port conflict:
        // this is not a general retry loop, it is the §10.4 window between
        // releasing the probe listener and podman binding the port.
        let mut retry = PortRetry::default();
        loop {
            let port = (self.port)()
                .map_err(|e| fail(format!("no free host port could be allocated: {e}"), vec![]))?;
            let render = match spec.may_write_models_dir {
                true => {
                    runtime.render_spec(spec.container_prefix, port, spec.models_dir, spec.data_dir)
                }
                false => runtime.render_spec_sparing_models_dir(
                    spec.container_prefix,
                    port,
                    spec.models_dir,
                    spec.data_dir,
                ),
            };
            let render = render.map_err(|e| {
                fail(
                    format!("rendering the container spec failed: {e}"),
                    Vec::new(),
                )
            })?;
            // The image renderer is the one that can refuse (a row naming
            // neither `model` nor `diffusion_model`, a key this image has no
            // flag for). Asked here, before `podman run`, so the answer is the
            // reason rather than a container that exits with a usage dump.
            if matches!(render.engine, EngineArgs::Image(_)) {
                if let Err(e) = render_image_args(&render) {
                    return Err(fail(
                        format!("this row cannot be rendered: {e}"),
                        Vec::new(),
                    ));
                }
            }

            let outcome = self.podman_argv(&podman_run_argv(&render)).await;
            let (status, stderr) = match outcome {
                Ok(out) if out.ok() => {
                    // `podman run -d` returned; the model is still uploading.
                    return match self
                        .await_ready(name, &render, port, spec.load_timeout)
                        .await
                    {
                        Ok(ready) => {
                            warnings.extend(ready.warnings);
                            if let Some(caps) = &ready.capabilities {
                                if let Some(row) = &runtime.image_model {
                                    warnings.extend(caps.warnings(row));
                                }
                            }
                            for w in &warnings {
                                tracing::warn!(
                                    container = %name,
                                    "{class} model '{model_id}' started with a warning: {w}"
                                );
                            }
                            Ok(Started {
                                port,
                                warnings,
                                capabilities: ready.capabilities,
                                gate,
                            })
                        }
                        Err(message) => {
                            // The container is still running — a stuck load, a
                            // model that will never answer — and no entry will
                            // ever refer to it, so nothing else would stop it:
                            // it would hold VRAM until the next boot
                            // reconciliation. Stopped, not removed, because
                            // the whole value of this failure is in
                            // `podman logs`, which `--replace` collects at the
                            // next start (§3.6).
                            self.stop_quietly(name).await;
                            Err(fail(message, self.log_excerpt(name).await))
                        }
                    };
                }
                Ok(out) => (out.status.to_string(), out.stderr),
                // The runner could not even spawn podman (missing binary,
                // fork failure). No container exists, so there is nothing to
                // retry a port against.
                Err(e) => (String::from("n/a"), format!("podman could not be run: {e}")),
            };

            if retry.again(&stderr) {
                // Measured (§10.4): the run fails synchronously but leaves
                // the container object behind in `created`. Collect the husk
                // before rerunning — `--replace` would too, but only if the
                // retry gets that far, and leaving `created` husks around is
                // exactly what makes reconciliation (§3.4) ambiguous later.
                let _ = self.podman(&["rm", "-f", name]).await;
                continue;
            }
            let logs = self.log_excerpt(name).await;
            return Err(fail(
                format!("podman run failed (exit {status}): {}", stderr.trim()),
                logs,
            ));
        }
    }

    /// Poll `GET <path>` until it answers 200, or the caller's
    /// `vram.load_timeout_seconds` is spent. `path` is
    /// [`RenderSpec::health_path`] — `/health` for llama, `/v1/models` for
    /// audio (audio.cpp has no `/health` route, §3.6).
    ///
    /// Measured (§10.3): the llama image has **no 503-loading phase**. The
    /// port is unreachable until the model is up and then answers 200
    /// directly, and because nginx fronts llama-server an early probe can see
    /// a recv error rather than a refused connection. So every non-200
    /// outcome — refused, reset, timed out, 404, 502 — means the same thing
    /// here: still starting. Nothing is retried differently and nothing is
    /// treated as fatal by the port; the one earlier verdict comes from podman,
    /// when the container itself has exited ([`Self::exited`], every
    /// [`CONTAINER_EXIT_POLL`]). Applied unchanged to the audio probe: a
    /// `GET /v1/models` that 200s is exactly as meaningful a readiness signal
    /// as `/health` is for llama.
    async fn await_health(
        &self,
        name: &str,
        port: u16,
        path: &str,
        load_timeout: Duration,
    ) -> Result<(), String> {
        let url = format!("http://127.0.0.1:{port}{path}");
        let deadline = Instant::now() + load_timeout;
        let mut next_state_check = Instant::now() + CONTAINER_EXIT_POLL;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no HTTP 200 from {url} within vram.load_timeout_seconds ({load_timeout:?})"
                ));
            }
            // The remaining budget *is* the per-probe timeout: a hung probe
            // then costs exactly the wall clock the poll was allowed anyway,
            // so there is no second, invented bound to explain.
            if let Ok(resp) = self.http.get(&url).timeout(left).send().await {
                if resp.status().as_u16() == 200 {
                    return Ok(());
                }
            }
            if let Some(dead) = self.exited_since(name, &mut next_state_check).await {
                return Err(format!("{dead} before {url} answered 200"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no HTTP 200 from {url} within vram.load_timeout_seconds ({load_timeout:?})"
                ));
            }
            tokio::time::sleep(HEALTH_POLL.min(left)).await;
        }
    }

    /// The readiness poll, per engine (§3.6, image-generation §3).
    ///
    /// llama and audio keep [`Self::await_health`] byte for byte: poll until a
    /// 200, and every other outcome means "still starting" until the deadline.
    /// sd-server needs its own rule, for a measured reason (§12.2): its
    /// readiness route is `GET /sdcpp/v1/capabilities`, and that route *throws*
    /// — 500 with an `EXCEPTION_WHAT` header — when the LoRA/upscaler scan
    /// trips over a directory it cannot walk, while generation works perfectly.
    /// Treating that as "not ready" would burn the whole load timeout and then
    /// stop a container that was answering all along.
    async fn await_ready(
        &self,
        name: &str,
        render: &crate::runtime::argv::RenderSpec,
        port: u16,
        load_timeout: Duration,
    ) -> Result<Ready, String> {
        if !matches!(render.engine, EngineArgs::Image(_)) {
            return self
                .await_health(name, port, render.health_path, load_timeout)
                .await
                .map(|()| Ready::default());
        }
        let url = format!("http://127.0.0.1:{port}{}", render.health_path);
        // What this poll is waiting for, said in full: for this engine a 5xx
        // is an answer too, so "no HTTP 200" would name the wrong condition.
        let timed_out = || {
            format!(
                "no HTTP 200 (ready) and no 5xx (ready, with a broken capabilities scan) from \
                 {url} within vram.load_timeout_seconds ({load_timeout:?})"
            )
        };
        let deadline = Instant::now() + load_timeout;
        let mut next_state_check = Instant::now() + CONTAINER_EXIT_POLL;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out());
            }
            if let Ok(resp) = self.http.get(&url).timeout(left).send().await {
                let status = resp.status().as_u16();
                // The server sets this header on the 500 it throws, and it is
                // the only description of what went wrong that exists — the
                // body is a generic `server_error`.
                let what = resp
                    .headers()
                    .get("EXCEPTION_WHAT")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                if status == 200 {
                    // One request, two answers: the body that proves it is up
                    // is also the capabilities document (§3's probe).
                    return Ok(match resp.text().await {
                        Ok(body) => match ImageCapabilities::parse(&body) {
                            Ok(caps) => Ready {
                                capabilities: Some(caps),
                                warnings: Vec::new(),
                            },
                            Err(e) => Ready {
                                capabilities: None,
                                warnings: vec![format!(
                                    "the capabilities body could not be read ({e}) — this \
                                     model's samplers, limits and supported modes are unknown"
                                )],
                            },
                        },
                        Err(e) => Ready {
                            capabilities: None,
                            warnings: vec![format!("the capabilities body could not be read: {e}")],
                        },
                    });
                }
                if (500..600).contains(&status) {
                    return Ok(Ready {
                        capabilities: None,
                        warnings: vec![format!(
                            "{} answered HTTP {status}: {} — the model generates, but its \
                             capabilities (samplers, limits, supported modes) could not be read",
                            render.health_path,
                            if what.is_empty() {
                                "no EXCEPTION_WHAT header"
                            } else {
                                &what
                            }
                        )],
                    });
                }
                // Anything else (a connection reset for the first ~0.6 s, a
                // 404 from something else on the port) is "not yet".
            }
            if let Some(dead) = self.exited_since(name, &mut next_state_check).await {
                return Err(format!("{dead} before {url} answered"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out());
            }
            tokio::time::sleep(HEALTH_POLL.min(left)).await;
        }
    }
}

/// What a successful start produced (image-generation design §3): the port,
/// plus the two things that used to have nowhere to go — the warnings a start
/// can carry without failing, and the capabilities an image container
/// reported.
pub(super) struct Started {
    pub(super) port: u16,
    pub(super) warnings: Vec<String>,
    pub(super) capabilities: Option<ImageCapabilities>,
    pub(super) gate: Option<Arc<GateFacts>>,
}

/// The readiness poll's verdict, for a probe that can succeed *with* something
/// to say.
#[derive(Default)]
struct Ready {
    capabilities: Option<ImageCapabilities>,
    warnings: Vec<String>,
}

enum Decision {
    /// Already `ready`: the port to forward to, and the identity of the entry
    /// the claim was taken on (see [`AcquireGuard::phase`]) and of its
    /// container. The entry's gate facts travel with the claim (see
    /// [`Entry::gate`]).
    Hit {
        port: u16,
        phase: Arc<watch::Sender<Phase>>,
        gate: Option<Arc<GateFacts>>,
        generation: u64,
    },
    /// Park on somebody else's entry. The state we found it in is carried
    /// along because it decides what a failure verdict *means* to us — see
    /// [`Registry::await_phase`].
    Wait(RuntimeState, watch::Receiver<Phase>),
    Start(StartClaim),
}
