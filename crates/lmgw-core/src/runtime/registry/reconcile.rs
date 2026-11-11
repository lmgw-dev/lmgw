//! Reconciliation (§3.4)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::gate::facts::GateFacts;

use super::*;
use crate::runtime::argv::EngineArgs;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::image::ImageCapabilities;
use crate::runtime::launcher::podman::decode_ps;
use crate::runtime::launcher::{Found, Listed, Listing};
use crate::runtime::{container_name, Class};

/// One `podman ps` row, for a caller outside this module.
///
/// The launcher's [`Listed`] is reconciliation's reading of a model's
/// containers; this is the same enumeration for the containers outside the
/// launcher: the agent container
/// runtime sweeps its own leftovers by label (container-runtime §6.4) and
/// re-deriving `podman ps --format json` next door would be a second place to
/// get the flags and the JSON casing wrong.
#[derive(Debug, Clone)]
pub struct PsEntry {
    pub name: String,
    pub labels: HashMap<String, String>,
    pub state: String,
    /// When podman created it, in unix seconds. Boot reconciliation compares it
    /// against the moment this process started: a container younger than that
    /// was started by *this* build and is not a leftover to collect
    /// (container-runtime §6.4).
    pub created: i64,
}

/// What one adoption probe learned: whether the container answered at all,
/// and — for an image container, whose readiness route is its capabilities
/// route — the document it answered with.
///
/// Two facts from one request, because that is what the route gives: the
/// alternative is asking a live container the same question twice.
#[derive(Debug, Default)]
struct ProbeAnswer {
    answered: bool,
    capabilities: Option<ImageCapabilities>,
}

/// Why [`Registry::adopt`] would not adopt a container.
#[derive(Debug)]
pub(super) struct Refused {
    /// The sentence the log line prints.
    pub(super) why: String,
    /// It did not answer its readiness probe — as a container that is still
    /// loading does not. Everything else about it may be in order.
    pub(super) silent: bool,
}

impl From<String> for Refused {
    fn from(why: String) -> Self {
        Self { why, silent: false }
    }
}

/// What one [`Registry::reconcile`] pass did (§3.4).
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Container names taken over as live registry entries.
    pub adopted: Vec<String>,
    /// Container names reconciliation refused to adopt, each with the reason.
    /// Every one of them was handed to `podman rm -f`; if that itself failed,
    /// the failure is in `errors` too and the container is still there for the
    /// next pass.
    pub removed: Vec<(String, String)>,
    /// True when the `podman ps` enumeration itself succeeded. False means
    /// lmgw learned *nothing* about what is running — which is why
    /// [`crate::runtime::lifecycle`] refuses to run the legacy sweep on it: with no
    /// adoption list there is no way to honour the "never sweep a name we
    /// just adopted" guard.
    pub listed: bool,
    /// Anything that went wrong along the way, already rendered. Reconciliation
    /// never fails as a whole: one unreadable container must not stop the
    /// other twelve from being recovered.
    pub errors: Vec<String>,
}

impl Registry {
    /// Snapshot of the registry for the status surfaces (§8). Sorted by
    /// class then model id so a rendered list does not reshuffle itself
    /// between polls.
    pub fn list(&self) -> Vec<RuntimeView> {
        let now = Instant::now();
        let draining = self.draining_for_owner();
        let mut out: Vec<RuntimeView> = self
            .map()
            .iter()
            .map(|((class, model_id), e)| RuntimeView {
                class: *class,
                model_id: model_id.clone(),
                container_name: e.container_name.clone(),
                generation: e.generation,
                port: e.host_port,
                state: e.state,
                in_flight: e.in_flight,
                claims: e.claims,
                started_at_age_seconds: now.saturating_duration_since(e.started_at).as_secs(),
                last_used_age_seconds: now.saturating_duration_since(e.last_used).as_secs(),
                // A `/props` read that failed is one of the entry's
                // warnings, kept apart so a refresh can replace it
                // (`llama_props.rs`); so is a partial start's, and what
                // admission said of a whole one, which the entry carries
                // from its claim on, whatever its start sets
                // (`admission_grant.rs`).
                warnings: e
                    .grant
                    .as_ref()
                    .map(|g| &g.warning)
                    .filter(|w| !w.is_empty())
                    .into_iter()
                    .chain(e.admission_note.iter())
                    .chain(e.warnings.iter())
                    .chain(e.llama.as_ref().and_then(|l| l.props.as_ref().err()))
                    .chain(
                        e.llama
                            .as_ref()
                            .and_then(|l| l.decision_route.as_ref())
                            .and_then(DecisionRoute::warning),
                    )
                    .cloned()
                    .collect(),
                image_capabilities: e.capabilities.clone(),
                llama_props: e.llama.as_ref().and_then(|l| l.facts().cloned()),
                rung: e.charge.as_ref().map(RungStatus::from_charge),
                climbing: e.climb.as_ref().map(|m| m.status(now)),
                sends: *e.sends.borrow(),
                charge: e.charge.clone(),
                resident_key: e.resident_key.clone(),
                placement: e.placement,
                owner: e.owner,
                draining_for_owner: draining && e.state != RuntimeState::Stopping,
                grant: e.grant.clone(),
                served: e.served,
                adopted: e.adopted,
                engine_key: e.engine_key.clone(),
            })
            .collect();
        out.sort_by(|a, b| (a.class, &a.model_id).cmp(&(b.class, &b.model_id)));
        out
    }

    /// Rebuild the map from what the launcher lists as actually there (§3.4;
    /// Podman: `podman ps -a` under this instance's label). A launcher whose
    /// processes never outlive lmgw lists nothing, and this adopts nothing.
    ///
    /// Runs once at boot, before anything else touches the runtime, and is the
    /// crash-recovery path: an lmgw that died leaves its containers behind, and
    /// this is what makes them lmgw's again instead of orphans holding VRAM
    /// nobody accounts for.
    ///
    /// `candidates` is one [`AcquireSpec`] per **enabled** configured model —
    /// the exact same value `acquire` would build for it, so "would we start
    /// this container the same way today?" is asked against the real renderer
    /// rather than a reconstruction of it. A container whose `(class, model)`
    /// labels do not appear in `candidates` is by definition unknown or
    /// disabled and gets removed.
    ///
    /// A ladder row has one candidate per rung, base first (ladder design
    /// §3.1): a container lmgw had climbed before it went away is adopted at
    /// the rung its command line matches, and that rung is what the entry
    /// records and the ledger charges. A crash is not a container stop, so it
    /// does not reset the ladder; a container that matches no rung is removed
    /// like any other stale one, and the next start is the base.
    ///
    /// **What is compared, and what deliberately is not.** The test is the
    /// *container command* — `Config.Cmd`, everything after the image name in
    /// `podman run`, which for a managed container is exactly
    /// [`crate::runtime::argv::render_engine_args`]'s output: the llama-server flag list for
    /// chat/aux, the fixed `server --config …` for audio. For audio that argv
    /// says nothing about *which* model is loaded (the selection is in the
    /// mounted `server.json`), so audio additionally compares the config last
    /// written for this model against what would be written now.
    ///
    /// `extra_run_args` are compared through the label a start records them
    /// in ([`crate::runtime::argv::RUN_ARGS_LABEL`], a digest): a container whose flags are not the
    /// ones its row renders now — the GPU passthrough and `label=disable` an
    /// empty override used to drop, or a row switched to the CPU — is
    /// removed, and so is one from an lmgw that did not record them yet (it
    /// starts again once, on the flags in effect).
    ///
    /// Not compared: the image, the mounts, `-p`, and the other labels beyond
    /// `lmgw.class`/`lmgw.model`. Those are podman-level flags that podman
    /// normalizes (a short image ref comes back fully qualified, a mount path
    /// comes back resolved), so a textual comparison would tear down healthy
    /// warm containers on every boot for a difference that is not one.
    /// Changing them takes effect through the explicit apply path (§3.6: stop
    /// + start), not through adoption.
    pub async fn reconcile(
        &self,
        container_prefix: &str,
        candidates: &[AcquireSpec<'_>],
    ) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let rows = match self.launcher.list(container_prefix, Listing::All).await {
            Ok(rows) => rows,
            Err(e) => {
                report.errors.push(e);
                return report;
            }
        };
        report.listed = true;

        for row in rows {
            let Some(name) = row.name().map(str::to_string) else {
                report
                    .errors
                    .push("podman ps returned a container with no name".into());
                continue;
            };
            // A benchmark run's container (benchmark design §3.4) carries this
            // instance's label and a chat model's, but it is no registry entry:
            // adopting it would hand a run's container to the reaper, and
            // removing it here would cut a run short. The boot sweep
            // (`bench::boot_sweep`) is what collects a leftover one.
            if row.labels.contains_key(crate::bench::BENCH_LABEL) {
                continue;
            }
            // An agent's container (container-runtime §6.1) carries this
            // instance's label too, and is no model's either: adoption would
            // fail on its missing `lmgw.class` and the container would be
            // force-removed — a service an early App-tab request started
            // while this `podman ps` was on its way, or an exited one whose
            // logs a 503 quotes. The agents' own reconcile
            // (`agents::container`) is what collects their leftovers.
            if row
                .labels
                .get(crate::agents::container::LABEL_KIND)
                .map(String::as_str)
                == Some(crate::agents::container::KIND_AGENT)
            {
                continue;
            }
            match self.adopt(container_prefix, candidates, &row, &name).await {
                Ok(_) => {
                    tracing::info!(container = %name, "adopted running container");
                    report.adopted.push(name);
                }
                Err(Refused { why: reason, .. }) => {
                    let removed = self.launcher.remove(&name).await;
                    match removed {
                        Ok(()) => tracing::info!(container = %name, "removed container: {reason}"),
                        Err(e) => {
                            tracing::warn!(container = %name, "removing container ({reason}): {e}");
                            report.errors.push(e);
                        }
                    }
                    report.removed.push((name, reason));
                }
            }
        }
        report
    }

    /// Decide one container's fate and, if it survives, insert it as `ready`.
    /// `Err` means "remove it, because …" — every rejection carries the
    /// sentence the log line prints ([`Refused`]). `Ok(true)`: this call inserted the
    /// entry; `Ok(false)`: the key was already the registry's, or a start
    /// claimed it meanwhile, and nothing was touched. Shared with the pass
    /// after boot (`unheld.rs`), which reports only what it inserted.
    pub(super) async fn adopt(
        &self,
        container_prefix: &str,
        candidates: &[AcquireSpec<'_>],
        row: &Listed,
        name: &str,
    ) -> Result<bool, Refused> {
        if row.state != "running" {
            return Err(format!("not running (state '{}')", row.state).into());
        }
        let class = row
            .labels
            .get("lmgw.class")
            .and_then(|c| Class::parse(c))
            .ok_or_else(|| {
                format!(
                    "unreadable lmgw.class label ({:?})",
                    row.labels.get("lmgw.class")
                )
            })?;
        let model_id = row
            .labels
            .get("lmgw.model")
            .cloned()
            .ok_or_else(|| "no lmgw.model label".to_string())?;
        // Every rung this model could be running, base first. One for every
        // row without a ladder, which is then exactly the old lookup.
        let specs: Vec<&AcquireSpec<'_>> = candidates
            .iter()
            .filter(|s| s.runtime.class == class && s.runtime.model_id == model_id)
            .collect();
        if specs.is_empty() {
            return Err(format!("no enabled {class} model '{model_id}' is configured").into());
        }
        // A name that is not the one this model renders today can never be
        // collected by a future `podman run --replace`, so it would leak for
        // as long as the box runs. Reject it while we still have it in hand.
        let expected_name = container_name(container_prefix, class, &model_id);
        if name != expected_name {
            return Err(
                format!("name is not the one '{model_id}' renders ({expected_name})").into(),
            );
        }
        // Already ours (a second reconcile pass, or a start that raced it):
        // leave the live entry exactly as it is, and report the name as
        // adopted so the legacy sweep's guard still sees it.
        if self.map().contains_key(&(class, model_id.clone())) {
            return Ok(false);
        }

        let inspected = self.launcher.inspect(name).await?;
        let port = inspected
            .host_port
            .ok_or_else(|| "no published host port for the container's 8080/tcp".to_string())?;

        let mut refusal = None;
        let mut matched = None;
        for spec in &specs {
            match self
                .adoptable(container_prefix, spec, &inspected, &row.labels, port)
                .await
            {
                Ok(found) => {
                    matched = Some((*spec, found));
                    break;
                }
                Err(why) => refusal = refusal.or(Some(why)),
            }
        }
        let Some((spec, (probed, render))) = matched else {
            let why = refusal.unwrap_or_default();
            return Err(if specs.len() > 1 {
                format!("{why}, at none of its {} rungs", specs.len())
            } else {
                why
            }
            .into());
        };
        let runtime: &ModelRuntime = probed.as_ref().unwrap_or(spec.runtime);

        // One probe, not a poll: an adopted container is claimed to be up
        // already, so this is a verification, not a wait for a load. Bounded
        // as one readiness probe is ([`Self::probe_once`]).
        let deadline = Instant::now() + spec.load_timeout;
        let probe = self.probe_once(&render, port, spec.load_timeout).await;
        if !probe.answered {
            // Worded per engine, because what counts as an answer differs:
            // llama and audio must say 200, an image container may say 5xx
            // (its capabilities scan can be broken while it generates).
            let why = if matches!(render.engine, EngineArgs::Image(_)) {
                format!(
                    "no HTTP response from its {} on the recovered port {port}",
                    render.health_path
                )
            } else {
                format!(
                    "no HTTP 200 from its {} on the recovered port {port}",
                    render.health_path
                )
            };
            // Silent, which a container still loading is too: the pass after
            // boot leaves a young one be (`unheld.rs`).
            return Err(Refused { why, silent: true });
        }

        // The gate's facts for an adopted container are the row's as it is
        // now — which is what the container runs: the argv comparison above
        // only lets through a command line this very descriptor renders.
        let gate = GateFacts::of_start(runtime, spec.models_dir).await;
        // What a llama-server says about itself, read as a start reads it
        // (llama egress design §4.2): after the probe, before the insert, in
        // what is left of the same budget. A pass cut short here has inserted
        // nothing, and the next one reads it again. A failed read is a
        // warning on the entry, never a reason not to adopt.
        let llama = match render.engine {
            EngineArgs::Llama(_) => {
                let props = self.read_llama_props_by(port, deadline).await;
                // A decision row's route, after `/props` and in the same
                // budget (decision-models design §3.6).
                let decision_route = self
                    .decision_route_of(&render, port, deadline, Some(&props))
                    .await;
                let route_warning = decision_route.as_ref().and_then(DecisionRoute::warning);
                for why in props.as_ref().err().into_iter().chain(route_warning) {
                    tracing::warn!(
                        container = %name,
                        "adopting {class} model '{model_id}', with a warning: {why}"
                    );
                }
                Some(LlamaEntry {
                    props,
                    ubatch_advisory: llama_props::ubatch_advisory(runtime, spec.models_dir).await,
                    decision_route,
                })
            }
            _ => None,
        };

        // The decisions above (`inspect`, the config read, the `/health`
        // probe) all happened with the map lock released, so the "not ours
        // yet" answer this adoption started from is now several awaits old.
        // The insert therefore re-asks under the lock it inserts in, and
        // yields to whoever got there first: an `acquire` that claimed this
        // key while we were probing owns the name (its `podman run --replace`
        // has already taken the container object over), and overwriting its
        // entry would strand a live start with no way back into the map.
        //
        // Abandoning is *not* a rejection: nothing is removed. This is what
        // makes boot reconciliation safe to run concurrently with the
        // listener (`server.rs` spawns it that way on purpose — a gateway
        // that refuses traffic until podman has answered is a worse failure
        // than a container adopted a second late).
        let now = Instant::now();
        let mut map = self.map();
        match map.entry((class, model_id.clone())) {
            std::collections::hash_map::Entry::Occupied(_) => {
                tracing::info!(
                    container = %name,
                    "not adopting: a concurrent start already claimed {class} \
                     model '{model_id}'"
                );
                Ok(false)
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                let (tx, _rx) = watch::channel(Phase::Ready);
                self.remember_image_caps(class, &model_id, probe.capabilities.as_ref());
                // What admission gave its start, from the label it carries,
                // and the note that it is not held to it where it is not.
                let (grant, unheld) = adopted_grant(&render, class, &model_id).unzip();
                slot.insert(Entry {
                    generation: next_generation(),
                    container_name: name.to_string(),
                    host_port: port,
                    state: RuntimeState::Ready,
                    started_at: now,
                    in_flight: 0,
                    claims: 0,
                    // An adopted container has been up for an unknown time and
                    // has served an unknown number of requests. Stamping "now"
                    // gives it one full idle period before the reaper (§3.7)
                    // may take it, which is the conservative direction: the
                    // alternative would be inventing a `last_used` in the past
                    // out of nothing.
                    last_used: now,
                    stop_timeout: spec.stop_timeout,
                    phase: Arc::new(tx),
                    // An adopted container was started by an earlier process
                    // (or an earlier run of this one), so its start's warnings
                    // are gone with it — "unknown" rather than a stale copy,
                    // and the next start fills them. Its capabilities are not
                    // lost the same way: the probe that just verified the
                    // container *is* the capabilities request for this class,
                    // so what it read is kept. One warning is restored from
                    // its label and the row: that it is not held to its
                    // grant, which keeps it from teaching a working budget.
                    warnings: unheld.flatten().into_iter().collect(),
                    capabilities: probe.capabilities,
                    llama,
                    gate,
                    // The rung whose command line it runs — the one the
                    // ledger charges from now on.
                    charge: runtime.rung_charge(),
                    resident_key: runtime.resident_key(),
                    placement: runtime.placement(),
                    sends: watch::channel(0).0,
                    climb: None,
                    // Whoever started it, a previous lmgw adopts it for the
                    // owner (candidate-aliases §12 entry 48): a guest's claim
                    // on it did not survive that lmgw.
                    owner: Origin::Owner,
                    // Charged what admission gave its start, as before.
                    grant,
                    admission_note: None,
                    // A container is adopted from Podman only, whose image
                    // names the build.
                    engine_key: runtime.engine_key(None),
                    // What it served under an earlier lmgw is not known:
                    // the ledger keeps the first-prompt margin only where
                    // nothing of it was measured either (`vram::headroom`).
                    served: false,
                    adopted: true,
                });
                Ok(true)
            }
        }
    }

    /// Would `spec` start exactly this container? `Ok` carries the descriptor
    /// it was compared with (the image class's probed copy, when there is
    /// one) and the render; `Err` says why not.
    ///
    /// **What is compared** is [`Self::reconcile`]'s: for audio the mounted
    /// `server.json`, here, and the launcher's own test of the instance
    /// (`Launcher::same_start` — Podman: the container command and its run
    /// args' label).
    async fn adoptable(
        &self,
        container_prefix: &str,
        spec: &AcquireSpec<'_>,
        inspected: &Found,
        labels: &HashMap<String, String>,
        port: u16,
    ) -> Result<(Option<ModelRuntime>, crate::runtime::argv::RenderSpec), String> {
        // Audio first, and before `render_spec`: the comparison is against the
        // config we *last wrote*, and `render_spec` overwrites it.
        if let (Some(model), Some(settings)) = (&spec.runtime.audio, &spec.runtime.audio_settings) {
            let path = crate::runtime::audio::config_dir(spec.data_dir, &spec.runtime.model_id)
                .join(crate::runtime::audio::CONFIG_FILE_NAME);
            let on_disk = std::fs::read_to_string(&path).ok();
            if on_disk.as_deref()
                != Some(crate::runtime::audio::render_single_model_config(settings, model).as_str())
            {
                return Err("its mounted server.json is not the one this model renders".into());
            }
        }

        // The same vocabulary the start would use: an adopted container's
        // `Cmd` was spelled against the image's own `--help`, so comparing it
        // against argv rendered from the embedded one would evict a healthy
        // container for using a flag newer than lmgw. A probe that fails here
        // falls back exactly as a start does — and then the comparison is the
        // one that decides, not the probe.
        let (probed, _) = self.with_sdcpp_caps(spec).await;
        let runtime: &ModelRuntime = probed.as_ref().unwrap_or(spec.runtime);
        // `preview_spec`, not `render_spec`: this renders in order to *compare*
        // argv, and the container it is comparing against is already running —
        // its directories were made by the start that created it. Creating
        // them again here would let an unwritable models dir refuse to adopt a
        // healthy container (image-generation §12.2 is why they exist at all).
        let mut render = runtime
            .preview_spec(container_prefix, port, spec.models_dir, spec.data_dir)
            .map_err(|e| format!("rendering the container spec failed: {e}"))?;
        // A partial start's container carries its grant (`lmgw.grant`): its
        // command has the `--max-vram` rendered from it, and so must the one
        // it is compared with.
        if let (EngineArgs::Image(img), Some(held)) = (
            &mut render.engine,
            labels
                .get(crate::runtime::argv::GRANT_LABEL)
                .and_then(|l| crate::runtime::argv::HeldTo::parse(l)),
        ) {
            img.held = Some(held);
        }
        // A fitted llama-server start's container carries its placement
        // (`lmgw.llama-fit`): its command has the values rendered from it
        // with `--fit off`, and so must the one it is compared with — only
        // those the row still leaves to fitting, so a row since edited to
        // its own value is not adopted with the stale one.
        if let (
            EngineArgs::Llama(crate::runtime::argv::LlamaArgs::Chat { params, args, .. }),
            Some(f),
        ) = (
            &render.engine,
            labels
                .get(crate::runtime::argv::FITTED_LABEL)
                .and_then(|l| crate::runtime::argv::Fitted::parse(l)),
        ) {
            let left = crate::vram::plan::chat::fit::left(params, args);
            let placement = f.placement.only(left);
            render.fitted = (!placement.is_whole()).then_some(crate::runtime::argv::Fitted {
                bytes: f.bytes,
                placement,
            });
        }
        self.launcher
            .same_start(container_prefix, &render, inspected, labels)?;
        Ok((probed, render))
    }

    /// [`Launcher::exited`](crate::runtime::launcher::Launcher::exited), at
    /// most once per [`CONTAINER_EXIT_POLL`]: `next` is when the next look is
    /// due, and moves on each time one is taken.
    pub(super) async fn exited_since(
        &self,
        name: &str,
        next: &mut Instant,
    ) -> Option<crate::runtime::launcher::Exited> {
        if Instant::now() < *next {
            return None;
        }
        *next = Instant::now() + CONTAINER_EXIT_POLL;
        self.launcher.exited(name).await
    }

    /// The host PID of each named container's root process, through the
    /// launcher (candidate-aliases design §4.7's per-process attribution;
    /// Podman: `State.Pid`, in one `podman inspect` for all of them). A name
    /// that is not running is absent from the map; `Err` only when nothing
    /// could be learned.
    pub async fn host_pids(&self, names: &[String]) -> Result<HashMap<String, u32>, String> {
        self.launcher.host_pids(names).await
    }

    /// One `GET http://127.0.0.1:<port><path>`; true when the container
    /// answers the way its engine's readiness rule accepts. Shares
    /// [`Self::await_health`]'s reading of a non-200 (refused, reset, 404,
    /// 502) as "not ready" — for an adoption that verdict is final rather than
    /// worth another poll, because a container that is genuinely up answers
    /// now — and [`Self::await_ready`]'s reading of a 5xx from an image
    /// container as "up, with a broken capabilities scan". Refusing to adopt
    /// on that would remove a container that is generating images.
    async fn probe_once(
        &self,
        render: &crate::runtime::argv::RenderSpec,
        port: u16,
        timeout: Duration,
    ) -> ProbeAnswer {
        let url = format!("http://127.0.0.1:{port}{}", render.health_path);
        let is_image = matches!(render.engine, EngineArgs::Image(_));
        // Bounded as one readiness probe of a start is (`await_health`,
        // `await_ready`): llama's `/health` and audio's `/v1/models` answer at
        // once when the container is up, so [`CONTAINER_EXIT_POLL`]; an image
        // container's capabilities scan may take its time, so the load budget.
        let timeout = if is_image {
            timeout
        } else {
            CONTAINER_EXIT_POLL.min(timeout)
        };
        let Ok(resp) = self.http.get(&url).timeout(timeout).send().await else {
            return ProbeAnswer::default();
        };
        let status = resp.status().as_u16();
        let answered = status == 200 || (is_image && (500..600).contains(&status));
        // The body is kept, not discarded: for an image container the health
        // route *is* `GET /sdcpp/v1/capabilities` (§3), so this one request
        // already carries the samplers, limits and supported modes. Throwing
        // them away left every warm-started container publishing nothing until
        // someone restarted it — the one case adoption exists to avoid.
        let capabilities = match (is_image, status) {
            (true, 200) => resp
                .text()
                .await
                .ok()
                .and_then(|body| ImageCapabilities::parse(&body).ok()),
            _ => None,
        };
        ProbeAnswer {
            answered,
            capabilities,
        }
    }

    /// `podman ps -a --filter … --format json`, reduced to [`PsEntry`] — for
    /// the containers outside the launcher (the agents', MCP's), on Podman
    /// directly.
    ///
    /// Every filter is passed as its own `--filter`, which podman ANDs. An
    /// empty stdout is podman's answer for "nothing matches", not a malformed
    /// one — the same reading [`Self::reconcile`] makes of it.
    pub async fn ps_filtered(&self, filters: &[String]) -> Result<Vec<PsEntry>, String> {
        let mut argv: Vec<String> =
            vec!["ps".into(), "-a".into(), "--format".into(), "json".into()];
        for f in filters {
            argv.push("--filter".into());
            argv.push(f.clone());
        }
        let rows = decode_ps(self.podman_argv(&argv).await)?;
        Ok(rows
            .into_iter()
            .map(|r| PsEntry {
                name: r.name().unwrap_or_default().to_string(),
                labels: r.labels,
                state: r.state,
                created: r.created,
            })
            .collect())
    }
}

/// The grant an adopted image container was started with, read back from its
/// `lmgw.grant` label into the render it was compared against
/// (`admission_grant.rs`): what the ledger charges it, with a warning worded
/// by what actually holds sd-server — the grant's `--max-vram`, the row's own
/// `max_vram`, or nothing in a build without the flag — and, where it is not
/// held to the grant, the same note a start leaves on its entry
/// ([`super::admission_grant::unheld_note`]), so nothing it does teaches the
/// row a working budget. `None` for every other container.
fn adopted_grant(
    render: &crate::runtime::argv::RenderSpec,
    class: Class,
    model_id: &str,
) -> Option<(PartialGrant, Option<String>)> {
    use super::admission_grant::{held_by, unheld_note, HeldBy};
    if let Some(f) = render.fitted {
        return Some((adopted_fit(f, class, model_id), None));
    }
    let EngineArgs::Image(img) = &render.engine else {
        return None;
    };
    let held = img.held?;
    let mut grant = PartialGrant {
        bytes: held.granted,
        max_vram_bytes: held.max_vram,
        warning: String::new(),
        llama: None,
    };
    let budget = crate::runtime::argv::max_vram_arg(held.max_vram);
    let how = match held_by(img, &grant) {
        HeldBy::Grant => format!("held sd-server to {budget} GiB (--max-vram)"),
        HeldBy::OwnAsGranted => {
            format!("with the max_vram in its args holding sd-server to {budget} GiB (--max-vram)")
        }
        HeldBy::Own(k, v) => format!("but {k} {v} in its args is what holds sd-server"),
        HeldBy::NoFlag => "but this sd-server build has no --max-vram, so sd-server sizes \
                           itself to the driver's free memory"
            .to_string(),
    };
    grant.warning = format!(
        "{}: {}/{model_id} was adopted from an earlier run of lmgw, which gave it {} on the \
         card, {how}; it keeps that until its container stops",
        crate::vram::partial::PARTIAL_FIT,
        class.as_str(),
        crate::hf::fmt_bytes(held.granted),
    );
    let note = unheld_note(img, &grant);
    Some((grant, note))
}

/// The grant of an adopted llama-server container lmgw fitted in `--fit`'s
/// place, from its `lmgw.llama-fit` label as compared: what the ledger
/// charges it, with the warning a start leaves — a lowered context, or
/// layers off the card — worded for the adoption.
fn adopted_fit(f: crate::runtime::argv::Fitted, class: Class, model_id: &str) -> PartialGrant {
    let p = f.placement;
    let lead = match (p.n_gpu_layers, p.n_cpu_moe) {
        (None, None) => crate::vram::partial::llama::CONTEXT_FITTED,
        _ => crate::vram::partial::PARTIAL_FIT,
    };
    PartialGrant {
        bytes: f.bytes,
        max_vram_bytes: 0,
        warning: format!(
            "{lead}: {}/{model_id} was adopted from an earlier run of lmgw, which gave it {} \
             on the card and told llama-server {}; it keeps that until its container stops",
            class.as_str(),
            crate::hf::fmt_bytes(f.bytes),
            p.flags()
        ),
        llama: Some(p),
    }
}
