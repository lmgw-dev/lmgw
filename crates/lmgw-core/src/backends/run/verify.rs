//! Phase 6, verify (container-builds §5 step 6 as superseded by §14.3): the
//! image is started twice through `Registry::run_throwaway`, always with the
//! owning class's GPU run args, and judged by its **output** — exit codes
//! say nothing here (official and audio exit 0 without a GPU, ik's `--help`
//! exits 1).
//!
//! - `--help` (or the engine's equivalent) must print a real help text;
//! - the device probe must report a device of the build's backend — the
//!   check that catches an image whose CUDA backend cannot load, which
//!   `--help` alone never would under `GGML_BACKEND_DL`.
//!
//! Both pass: **succeeded**. A check that fails: **broken**. Under the GPU
//! hold, on a CUDA OOM while probing, or when podman itself cannot start the
//! probe (a missing CDI device, say — the machine's problem, not the
//! image's): **unverified**, with **Verify now** ([`verify_run`]) for later.

use lmgw_api_types::builds::VerifyReport;

use super::log::RunLog;
use super::podman::{short_id, Podman};
use super::promote;
use crate::backends::model::{BuildRun, BuildRunPatch, BuildRunStatus, BuildSpec, Engine};
use crate::backends::presets::{self, Flavor, VerifySpec};
use crate::config::Settings;
use crate::state::SharedState;
use crate::store;

/// Lowercased substrings of a device-memory exhaustion — the probe found the
/// card full (another model resident), which says nothing about the image.
const OOM_MARKERS: [&str; 4] = [
    "out of memory",
    "cudamalloc",
    "cudaerrormemoryallocation",
    "erroroutofdevicememory",
];

/// How a verify came out.
pub(crate) struct Verdict {
    pub status: BuildRunStatus,
    pub report: VerifyReport,
    /// Why it is not `succeeded` — the run's `error`.
    pub error: Option<String>,
}

impl Verdict {
    fn unverified(mut report: VerifyReport, why: String) -> Self {
        report.gpu_verified = false;
        report.notes.push(why.clone());
        Self {
            status: BuildRunStatus::Unverified,
            report,
            error: Some(why),
        }
    }

    fn broken(mut report: VerifyReport, why: String) -> Self {
        report.notes.push(why.clone());
        Self {
            status: BuildRunStatus::Broken,
            report,
            error: Some(why),
        }
    }
}

/// The `podman run` args of the class an engine's images run in (§14.3: the
/// probes need the GPU attached exactly as a real start would): llama → the
/// chat class, audio → audio, sd.cpp → image.
pub(crate) fn class_run_args(settings: &Settings, engine: Engine) -> Vec<String> {
    match engine {
        Engine::Llama => settings.router.extra_run_args.clone(),
        Engine::Audio => settings.audio.extra_run_args.clone(),
        Engine::Sdcpp => settings.image.extra_run_args.clone(),
    }
}

/// The probes for images built from `dockerfile` — the preset's when one
/// knows it, else by the repository (as `presets::choose_dockerfile` picks
/// them), so **Verify now** asks exactly what the run asked.
pub(crate) fn verify_spec_for(spec: &BuildSpec, dockerfile: &str) -> VerifySpec {
    match presets::profile_for(spec.engine, spec.backend, dockerfile) {
        Some(p) => p.verify(),
        None => {
            let flavor = presets::repo_preset_for_url(&spec.repo_url)
                .filter(|p| p.engine == spec.engine)
                .map_or(Flavor::default_for(spec.engine), |p| p.flavor);
            presets::verify_spec(flavor, spec.backend)
        }
    }
}

fn is_oom(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    OOM_MARKERS.iter().any(|m| low.contains(m))
}

struct Probe {
    status: i32,
    text: String,
}

/// One throwaway run, its whole output into the log.
async fn probe(
    state: &SharedState,
    name: &str,
    image: &str,
    entrypoint: &str,
    run_args: &[String],
    args: &[&str],
    log: &RunLog,
) -> Result<Probe, String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    log.line(&format!(
        "$ podman run --rm {} --entrypoint {entrypoint} {} {}",
        run_args.join(" "),
        short_id(image),
        args.join(" ")
    ));
    let out = state
        .runtime()
        .run_throwaway(name, image, entrypoint, run_args, &args)
        .await?;
    log.lines(&out.stdout);
    log.lines(&out.stderr);
    log.line(&format!("(exit {})", out.status));
    Ok(Probe {
        status: out.status,
        text: format!("{}\n{}", out.stdout, out.stderr),
    })
}

/// Why a probe says nothing about the image, if it does not.
fn inconclusive(what: &str, p: &Probe) -> Option<String> {
    if is_oom(&p.text) {
        return Some(format!(
            "the {what} probe ran out of GPU memory (something else holds the card) — not \
             GPU-verified; Verify now once the GPU is free"
        ));
    }
    // 125 is podman's own failure — the run args, a CDI device — never the
    // binary's, and it would be the same for any image.
    if p.status == 125 {
        let said = p.text.lines().rev().find(|l| !l.trim().is_empty());
        return Some(format!(
            "podman could not start the {what} probe (exit 125: {}) — not GPU-verified; check \
             the class's run args, then Verify now",
            said.unwrap_or("no message").trim()
        ));
    }
    None
}

/// Verify `image` (an ID) as an image of `engine` with `spec`'s probes.
/// `reference` is the tag the flag-vocabulary cache is primed under (llama).
pub(crate) async fn verify_image(
    state: &SharedState,
    engine: Engine,
    spec: &VerifySpec,
    image: &str,
    reference: &str,
    run_id: i64,
    log: &RunLog,
) -> Verdict {
    let snap = state.snapshot();
    let report = VerifyReport::default();
    if snap.settings.hold.active {
        return Verdict::unverified(
            report,
            "the GPU hold is on, so the image was not started on the GPU — not GPU-verified; \
             Verify now once the hold is released"
                .into(),
        );
    }
    // A benchmark run has the card to itself (benchmark design §3.2): a
    // verify container beside it would skew what it measures.
    if let Some(l) = &snap.gpu_lease {
        return Verdict::unverified(
            report,
            format!(
                "benchmark run {} has the GPU to itself, so the image was not started on the \
                 GPU — not GPU-verified; Verify now once the run has ended",
                l.run_id
            ),
        );
    }
    let run_args = class_run_args(&snap.settings, engine);
    let prefix = snap.settings.container_prefix.clone();
    // The instance id keeps two instances' run N apart: `--replace` would
    // otherwise take the other one's probe down mid-run.
    let name = format!(
        "{prefix}-buildverify-{}-{run_id}",
        state.builds.instance_id()
    );
    verify_with(
        state, engine, spec, image, reference, &name, &prefix, &run_args, report, log,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn verify_with(
    state: &SharedState,
    engine: Engine,
    spec: &VerifySpec,
    image: &str,
    reference: &str,
    name: &str,
    prefix: &str,
    run_args: &[String],
    mut report: VerifyReport,
    log: &RunLog,
) -> Verdict {
    let help = match probe(
        state,
        name,
        image,
        spec.entrypoint,
        run_args,
        spec.help_args,
        log,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return Verdict::unverified(report, format!("{e} — not GPU-verified")),
    };
    if let Some(why) = inconclusive("help", &help) {
        return Verdict::unverified(report, why);
    }
    if let Err(e) = presets::help_ok(spec, &help.text) {
        return Verdict::broken(report, e);
    }
    report.help_ok = true;

    match spec.device_args {
        None => {
            report.gpu_verified = true;
            report
                .notes
                .push("the CPU backend has no device to probe".into());
        }
        Some(args) => {
            let dev = match probe(state, name, image, spec.entrypoint, run_args, args, log).await {
                Ok(p) => p,
                Err(e) => return Verdict::unverified(report, format!("{e} — not GPU-verified")),
            };
            if let Some(why) = inconclusive("device", &dev) {
                return Verdict::unverified(report, why);
            }
            match presets::devices_found(spec, &dev.text) {
                Ok(devices) => {
                    log.line(&format!("devices: {}", devices.join("; ")));
                    report.devices = devices;
                    report.gpu_verified = true;
                }
                Err(e) => return Verdict::broken(report, e),
            }
        }
    }

    // Prime the flag-vocabulary cache for the new image ID (§5 step 6), so
    // the first model start on it validates its args without another probe.
    if engine == Engine::Llama {
        match state.runtime().help_text(prefix, reference, run_args).await {
            Ok(_) => log.line("the llama-server flag vocabulary of the new image is cached"),
            Err(e) => {
                let note = format!("the flag vocabulary was not cached: {e}");
                log.line(&note);
                report.notes.push(note);
            }
        }
    }
    Verdict {
        status: BuildRunStatus::Succeeded,
        report,
        error: None,
    }
}

/// The run's own image tag: the first of its tags that is not a moving one
/// (in either namespace — a run recorded on a dev copy of the database may
/// carry production's) — its immutable tag, or its `-r<id>` tag
/// ([`tags::run_tag`](crate::backends::tags::run_tag)) when it never took
/// the immutable one.
pub(crate) fn immutable_tag_of(run: &BuildRun) -> Option<String> {
    use crate::backends::tags::moving_tag_for;
    let moving = [
        moving_tag_for(run.engine, &run.slug, false),
        moving_tag_for(run.engine, &run.slug, true),
    ];
    run.tags.iter().find(|t| !moving.contains(t)).cloned()
}

/// `build_verify` (§15, "Verify now"): run the verify checks again for a run
/// that built an image, and record the new verdict on the run (status,
/// verify report, error). A run that turns `succeeded` this way is promoted
/// — moving tag, help cache, retention — unless a newer run of its build is
/// already the current one.
pub async fn verify_run(state: &SharedState, run_id: i64) -> Result<VerifyReport, String> {
    let run = store::get_build_run(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build run with id {run_id}"))?;
    if !run.status.is_terminal() {
        return Err(format!(
            "run {run_id} is still going — it verifies its image itself when the build is done"
        ));
    }
    if !run.status.built() {
        return Err(format!(
            "run {run_id} did not build an image (it ended {}), so there is nothing to verify",
            run.status.as_str()
        ));
    }
    let image = run
        .image_id
        .clone()
        .ok_or_else(|| format!("run {run_id} recorded no image ID"))?;
    if Podman::of(state).image_id(&image).await?.is_none() {
        return Err(format!(
            "the image of run {run_id} ({}) is no longer on this machine — run the build again",
            short_id(&image)
        ));
    }
    let resolved = run
        .inputs
        .resolved
        .clone()
        .ok_or_else(|| format!("run {run_id} recorded no resolved inputs"))?;
    let spec = verify_spec_for(&run.inputs.config, &resolved.dockerfile);
    let reference = immutable_tag_of(&run).unwrap_or_else(|| image.clone());
    let log = RunLog::open(&super::log_path_of(state, &run))?;
    log.header("verify (Verify now)");
    let mut verdict =
        verify_image(state, run.engine, &spec, &image, &reference, run_id, &log).await;
    log.line(&format!("verify: {}", verdict.status.as_str()));
    store::update_build_run(
        &state.db,
        run_id,
        &BuildRunPatch {
            verify: serde_json::to_value(&verdict.report).ok(),
            ..BuildRunPatch::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    // The run finished when it finished; Verify now only changes its verdict.
    store::set_build_run_verdict(&state.db, run_id, verdict.status, verdict.error.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    if verdict.status == BuildRunStatus::Succeeded {
        if let Some(build_id) = run.build_id {
            let runs = store::list_runs_for_build(&state.db, build_id)
                .await
                .map_err(|e| e.to_string())?;
            let newer_current = runs.iter().any(|r| r.promoted && r.id > run_id);
            if newer_current {
                let note = "not made current: a newer run of this build is the current one — \
                            Make current moves the tag here"
                    .to_string();
                log.line(&note);
                verdict.report.notes.push(note);
            } else {
                log.header("promote");
                let fresh = store::get_build_run(&state.db, run_id)
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or(run.clone());
                if let Err(e) = promote::after_verify(state, &fresh, &image, None, &log).await {
                    let note = format!("verified, but not made current: {e}");
                    log.line(&note);
                    verdict.report.notes.push(note);
                }
            }
        }
    }
    Ok(verdict.report)
}
