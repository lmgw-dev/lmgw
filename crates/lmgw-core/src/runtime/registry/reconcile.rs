//! Reconciliation (§3.4)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::watch;

use crate::gate::facts::GateFacts;

use super::*;
use crate::runtime::argv::{render_engine_args, run_args_digest, EngineArgs, RUN_ARGS_LABEL};
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::image::ImageCapabilities;
use crate::runtime::{container_name, Class};

/// The subset of `podman ps --format json` reconciliation reads.
///
/// Only these three fields: the enumeration answers *which* containers carry
/// our instance label and what they claim to be; everything a decision
/// actually turns on (the command, the published port) comes from `podman
/// inspect`, which is the authoritative record for both.
#[derive(Debug, Deserialize)]
pub(super) struct PsRow {
    #[serde(default, rename = "Names")]
    pub(super) names: Vec<String>,
    #[serde(default, rename = "Labels")]
    pub(super) labels: HashMap<String, String>,
    #[serde(default, rename = "State")]
    pub(super) state: String,
    /// Unix seconds, podman's own `Created` (not `CreatedAt`, which is the
    /// human "6 minutes ago"). `0` when the field is absent, which reads as
    /// "long ago" everywhere it is compared — the safe direction for a
    /// reconciliation that removes things.
    #[serde(default, rename = "Created")]
    pub(super) created: i64,
}

/// One `podman ps` row, for a caller outside this module.
///
/// [`PsRow`]'s fields stay private — reconciliation's reading of them is this
/// module's business — but the *enumeration* is not: the agent container
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

/// The subset of `podman inspect <name>` reconciliation reads.
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

#[derive(Debug, Deserialize)]
struct InspectRow {
    #[serde(default, rename = "Config")]
    config: InspectConfig,
    #[serde(default, rename = "NetworkSettings")]
    network_settings: InspectNetwork,
}

/// The part of `podman inspect <name>` [`Registry::exited`] reads: the
/// container's run state. Its own row type rather than a field on
/// [`InspectRow`], so adoption's decoding does not change.
#[derive(Debug, Deserialize)]
pub(super) struct InspectStateRow {
    #[serde(default, rename = "State")]
    pub(super) state: Option<InspectState>,
}

#[derive(Debug, Deserialize)]
pub(super) struct InspectState {
    #[serde(default, rename = "Status")]
    status: String,
    #[serde(default, rename = "Running")]
    pub(super) running: bool,
    #[serde(default, rename = "ExitCode")]
    exit_code: i64,
}

#[derive(Debug, Default, Deserialize)]
struct InspectConfig {
    /// The container command — everything after the image name in `podman
    /// run` (the image's own `ENTRYPOINT` is *not* part of it). For a managed
    /// container that is exactly [`render_engine_args`]'s output, which is
    /// what makes the two comparable at all.
    #[serde(default, rename = "Cmd")]
    cmd: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectNetwork {
    /// `{"8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": "39311"}]}`. The
    /// value is nullable for an unpublished port.
    #[serde(default, rename = "Ports")]
    ports: HashMap<String, Option<Vec<PortBinding>>>,
}

#[derive(Debug, Deserialize)]
struct PortBinding {
    #[serde(default, rename = "HostPort")]
    host_port: String,
}

/// What [`Registry::container_exists`] could establish about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// podman answered, and the container is there.
    Present,
    /// podman answered "no such container".
    Absent,
    /// podman did not answer at all — carries the rendered reason.
    Unknown(String),
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
                started_at_age_seconds: now.saturating_duration_since(e.started_at).as_secs(),
                last_used_age_seconds: now.saturating_duration_since(e.last_used).as_secs(),
                // A `/props` read that failed is one of the entry's
                // warnings, kept apart so a refresh can replace it
                // (`llama_props.rs`).
                warnings: e
                    .warnings
                    .iter()
                    .chain(e.llama.as_ref().and_then(|l| l.props.as_ref().err()))
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
            })
            .collect();
        out.sort_by(|a, b| (a.class, &a.model_id).cmp(&(b.class, &b.model_id)));
        out
    }

    /// Rebuild the map from what podman is actually running (§3.4).
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
    /// [`render_engine_args`]'s output: the llama-server flag list for
    /// chat/aux, the fixed `server --config …` for audio. For audio that argv
    /// says nothing about *which* model is loaded (the selection is in the
    /// mounted `server.json`), so audio additionally compares the config last
    /// written for this model against what would be written now.
    ///
    /// `extra_run_args` are compared through the label a start records them
    /// in ([`RUN_ARGS_LABEL`], a digest): a container whose flags are not the
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
        let filter = format!("label=lmgw.instance={container_prefix}");
        let out = self
            .podman(&["ps", "-a", "--format", "json", "--filter", &filter])
            .await;
        let stdout = match out {
            Ok(o) if o.ok() => o.stdout,
            Ok(o) => {
                report.errors.push(format!(
                    "podman ps failed (exit {}): {}",
                    o.status,
                    o.stderr.trim()
                ));
                return report;
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("podman ps could not be run: {e}"));
                return report;
            }
        };
        // An empty stdout is podman's answer for "no containers match", not a
        // malformed one.
        let rows: Vec<PsRow> = if stdout.trim().is_empty() {
            Vec::new()
        } else {
            match serde_json::from_str(&stdout) {
                Ok(rows) => rows,
                Err(e) => {
                    report
                        .errors
                        .push(format!("podman ps returned unreadable JSON: {e}"));
                    return report;
                }
            }
        };
        report.listed = true;

        for row in rows {
            let Some(name) = row.names.first().cloned() else {
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
                    let removed = self.rm_force(&name).await;
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
        row: &PsRow,
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

        let inspected = self.inspect(name).await?;
        let port = host_port(&inspected)
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
                if let Err(why) = &props {
                    tracing::warn!(
                        container = %name,
                        "adopting {class} model '{model_id}', with a warning: {why}"
                    );
                }
                Some(LlamaEntry {
                    props,
                    ubatch_advisory: llama_props::ubatch_advisory(runtime, spec.models_dir).await,
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
                slot.insert(Entry {
                    generation: next_generation(),
                    container_name: name.to_string(),
                    host_port: port,
                    state: RuntimeState::Ready,
                    started_at: now,
                    in_flight: 0,
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
                    // so what it read is kept.
                    warnings: Vec::new(),
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
                });
                Ok(true)
            }
        }
    }

    /// Would `spec` start exactly this container? `Ok` carries the descriptor
    /// it was compared with (the image class's probed copy, when there is
    /// one) and the render; `Err` says why not.
    ///
    /// **What is compared** is [`Self::reconcile`]'s: the container command,
    /// its run args' label, and for audio the mounted `server.json`.
    async fn adoptable(
        &self,
        container_prefix: &str,
        spec: &AcquireSpec<'_>,
        inspected: &InspectRow,
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
        let render = runtime
            .preview_spec(container_prefix, port, spec.models_dir, spec.data_dir)
            .map_err(|e| format!("rendering the container spec failed: {e}"))?;
        let expected = render_engine_args(&render);
        if inspected.config.cmd != expected {
            return Err(format!(
                "its command is not the one this model renders now ({} vs {} args)",
                inspected.config.cmd.len(),
                expected.len()
            ));
        }
        match labels.get(RUN_ARGS_LABEL) {
            Some(got) if *got == run_args_digest(&render.extra_run_args) => {}
            Some(_) => return Err("its run args are not the ones this model renders now".into()),
            None => {
                return Err(
                    "it carries no record of its run args (started by an older lmgw), so \
                     they cannot be told to be the ones this model renders now"
                        .into(),
                )
            }
        }
        Ok((probed, render))
    }

    /// [`Self::exited`], at most once per [`CONTAINER_EXIT_POLL`]: `next` is
    /// when the next look is due, and moves on each time one is taken.
    pub(super) async fn exited_since(&self, name: &str, next: &mut Instant) -> Option<String> {
        if Instant::now() < *next {
            return None;
        }
        *next = Instant::now() + CONTAINER_EXIT_POLL;
        self.exited(name).await
    }

    /// `Some("the container exited with code N")` when podman says a starting
    /// container is no longer running, so the readiness poll can stop at once
    /// instead of waiting out the load timeout on a port nothing will ever
    /// open again — and so is "no such container", the one failing inspect
    /// with a clear answer. Anything else podman cannot answer clearly —
    /// inspect failing otherwise, JSON it cannot read, a state it does not
    /// name — is `None`: "keep waiting", so a doubt never fails a start that
    /// might still come up.
    pub(super) async fn exited(&self, name: &str) -> Option<String> {
        let out = self
            .podman(&["inspect", "--format", "json", name])
            .await
            .ok()?;
        // A start only asks after its own `podman run` succeeded, so a name
        // podman no longer knows is a container somebody removed while it
        // loaded — dead, not "keep waiting" until the load budget is spent.
        if !out.ok() {
            return is_no_such_container(&out.stderr)
                .then(|| "the container was removed while the model was loading".to_string());
        }
        let rows: Vec<InspectStateRow> = serde_json::from_str(&out.stdout).ok()?;
        let state = rows.into_iter().next()?.state?;
        match state.status.as_str() {
            "exited" | "stopped" | "dead" if !state.running => Some(format!(
                "the container exited with code {} ({}) while the model was loading",
                state.exit_code, state.status
            )),
            _ => None,
        }
    }

    /// The host PID of each named container's init process — `State.Pid` —
    /// in one `podman inspect` for all of them (candidate-aliases design
    /// §4.7's per-process attribution).
    ///
    /// Measured 2026-09-27 under rootless podman: for a chat container this
    /// is llama-server itself, and the PID NVML lists. A name podman does not
    /// know, or a container that is not running (`State.Pid` 0), is simply
    /// absent from the map — podman prints the containers it found and names
    /// the rest on stderr, exiting non-zero. `Err` only when podman could not
    /// be run, or failed without naming a single container.
    pub async fn host_pids(&self, names: &[String]) -> Result<HashMap<String, u32>, String> {
        if names.is_empty() {
            return Ok(HashMap::new());
        }
        let mut argv: Vec<String> =
            vec!["inspect".into(), "--format".into(), HOST_PID_FORMAT.into()];
        argv.extend(names.iter().cloned());
        let out = self
            .podman_argv(&argv)
            .await
            .map_err(|e| format!("podman inspect could not be run: {e}"))?;
        let pids = parse_host_pids(&out.stdout);
        if pids.is_empty() && !out.ok() {
            return Err(format!(
                "podman inspect failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(pids)
    }

    /// `podman inspect` one container, decoded down to the two fields
    /// adoption reads.
    async fn inspect(&self, name: &str) -> Result<InspectRow, String> {
        let out = self
            .podman(&["inspect", "--format", "json", name])
            .await
            .map_err(|e| format!("podman inspect could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman inspect failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        let rows: Vec<InspectRow> = serde_json::from_str(&out.stdout)
            .map_err(|e| format!("podman inspect returned unreadable JSON: {e}"))?;
        rows.into_iter()
            .next()
            .ok_or_else(|| "podman inspect returned no container".to_string())
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

    /// `podman ps -a --filter … --format json`, reduced to [`PsEntry`].
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
        let out = self
            .podman_argv(&argv)
            .await
            .map_err(|e| format!("podman ps could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman ps failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        if out.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<PsRow> = serde_json::from_str(&out.stdout)
            .map_err(|e| format!("podman ps returned unreadable JSON: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| PsEntry {
                name: r.names.first().cloned().unwrap_or_default(),
                labels: r.labels,
                state: r.state,
                created: r.created,
            })
            .collect())
    }
}

/// The `--format` of [`Registry::host_pids`]: one `<name> <pid>` line per
/// container. A space is a safe separator — podman refuses a container name
/// with one.
pub const HOST_PID_FORMAT: &str = "{{.Name}} {{.State.Pid}}";

/// [`HOST_PID_FORMAT`]'s output, as `name -> pid` for every running
/// container. A line that does not parse — or names PID 0, a container that
/// is not running — is left out.
fn parse_host_pids(stdout: &str) -> HashMap<String, u32> {
    stdout
        .lines()
        .filter_map(|l| {
            let (name, pid) = l.trim().rsplit_once(' ')?;
            let pid: u32 = pid.trim().parse().ok()?;
            (pid > 0 && !name.is_empty()).then(|| (name.trim().to_string(), pid))
        })
        .collect()
}

/// Recover the published host port from `podman inspect` (§3.4/§3.5): the
/// binding for the container's own `8080/tcp`, which is the only port a
/// managed container ever publishes ([`podman_run_argv`](crate::runtime::argv::podman_run_argv) writes `-p
/// <host>:8080`). Any other key is somebody else's container or a hand-edited
/// one, and is not guessed at.
fn host_port(row: &InspectRow) -> Option<u16> {
    row.network_settings
        .ports
        .get("8080/tcp")?
        .as_ref()?
        .iter()
        .find_map(|b| b.host_port.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_host_pids;

    /// What podman printed for `inspect --format '{{.Name}} {{.State.Pid}}'`
    /// over a running container, an exited one and a name it did not know
    /// (measured 2026-09-27: found containers on stdout, the unknown one on
    /// stderr, exit 125).
    #[test]
    fn host_pids_keep_running_containers_only() {
        let pids = parse_host_pids("lmgw-chat-a-1a2b3c 1213087\nlmgw-aux-b-4d5e6f 0\n\ngarbage\n");
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert_eq!(pids["lmgw-chat-a-1a2b3c"], 1213087);
    }
}
