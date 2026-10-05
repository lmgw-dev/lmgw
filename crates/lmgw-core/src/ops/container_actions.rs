//! Container lifecycle: per-model actions, group actions, and the
//! `lmgw__container` dispatch -- plus the GPU-hold helpers
//! (`hold_refusal`, `start_model`, ...) they share with `hold::hold_set`.

use serde_json::{json, Value};

use crate::config::{AudioModel, ImageModel, LocalModel, Snapshot};
use crate::runtime::descriptor::{model_runtime, model_runtimes};
use crate::runtime::registry::RuntimeError;
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::store::NewLocalModel;
use crate::vram::Fit;

use super::*;

/// The refusal `container` gives `start` and `restart` while the hold is on
/// (gpu-hold design §2).
///
/// Returned **before** the verb's own stop step, which is the point for
/// `restart`: a restart under a hold would stop the model and then fail to
/// start it, i.e. do exactly the destructive half of the operation the caller
/// asked for. `apply` is not refused — it stops on purpose, and says so.
fn hold_refusal(action: &str) -> String {
    format!(
        "GPU hold is active, so '{action}' is refused before anything is stopped — nothing new \
         starts on the GPU while lmgw is holding it. Release the hold (tray, dashboard \
         titlebar, or lmgw__hold_set active=false), or use action=stop, status or logs."
    )
}

/// Canonical group target (per-model-containers design §8): `target` now
/// names a **group**, never a model — `all` spans every class, `embed` is
/// accepted as the pre-rename spelling of `aux`. `None` means "every class"
/// (`all`), so a caller filters the registry/vram views with
/// `Iterator::is_none_or` instead of a second branch per call site.
fn normalize_group_target(target: &str) -> Result<Option<Class>, String> {
    match target.trim() {
        "all" => Ok(None),
        "chat" => Ok(Some(Class::Chat)),
        "embed" | "aux" => Ok(Some(Class::Aux)),
        "audio" => Ok(Some(Class::Audio)),
        "image" => Ok(Some(Class::Image)),
        other => Err(format!(
            "unknown target '{other}' (all|chat|aux|audio|image)"
        )),
    }
}

fn target_label(class_filter: Option<Class>) -> &'static str {
    class_filter.map_or("all", Class::as_str)
}

/// Every class a model id is configured under. Usually exactly one — but
/// `model_id` is unique per table only (design §3.3), so a chat and an aux
/// model (say) can share one, and `model` alone would be ambiguous between
/// them without checking every table.
pub(super) fn find_model_classes(snap: &Snapshot, model_id: &str) -> Vec<Class> {
    let mut out = Vec::new();
    if snap.local_models.iter().any(|m| m.model_id == model_id) {
        out.push(Class::Chat);
    }
    if snap.aux_models.iter().any(|m| m.model_id == model_id) {
        out.push(Class::Aux);
    }
    if snap.audio_models.iter().any(|m| m.model_id == model_id) {
        out.push(Class::Audio);
    }
    if snap.image_models.iter().any(|m| m.model_id == model_id) {
        out.push(Class::Image);
    }
    out
}

/// Start this model's container, or join a start already in flight, and drop
/// the claim immediately.
///
/// The `lmgw__container` surface's own primitive for "make this model warm":
/// [`crate::runtime::registry::Registry::acquire`] is what every inference
/// request already goes through (directly when admission is inactive,
/// through [`crate::vram::admit`] when it is active) — this calls it the same
/// way and then lets the guard drop, which is what makes the container
/// resident-but-unclaimed instead of pinned against eviction/the idle reaper
/// forever (design §3.2: "admit-less registry acquire→drop guard").
async fn start_model(state: &SharedState, class: Class, model_id: &str) -> Result<u16, String> {
    let snap = state.snapshot();
    let runtime = model_runtime(&snap, class, model_id)
        .ok_or_else(|| format!("no {class} model '{model_id}' is configured"))?;
    // An operator start is not a request: nothing is waiting on it, so it is
    // refused rather than allowed to evict a model that is in use (§4). When
    // admission is inactive this is a no-op and the start proceeds exactly as
    // it did before. The permit holds the ledger reservation until the
    // acquire settles, so two concurrent starts (group start, group apply)
    // cannot both be told the same free bytes.
    let permit = match state
        .vram
        .check_background_start(state, &snap, class, model_id)
        .await
    {
        Fit::Full(why) => {
            return Err(format!("not enough GPU memory to start it — {why}"));
        }
        // Deliberately not the "not enough GPU memory" wording (gpu-hold
        // design §4): the card may be completely empty, and telling the owner
        // to free memory would send them looking for a victim that does not
        // exist. `container start`/`restart` are refused earlier, before
        // anything is stopped; this catches the paths that reach a start
        // another way — group apply, and a hold or a benchmark's lease that
        // came on mid-operation. The sentence names which of the two it is
        // and how it ends, so it is passed on as it is: under a lease,
        // "GPU hold is active" in front of it was wrong.
        Fit::Held(why) => {
            return Err(why);
        }
        Fit::Go(permit) => Some(permit),
        Fit::Unchecked => None,
    };
    let spec = lifecycle::acquire_spec(state, &snap, &runtime);
    let started = state.runtime().acquire(&spec).await;
    drop(permit);
    let port = started.map_err(|e| e.to_string())?.port();
    // Up now: learn its PID while it is known to be here (§4.7).
    state.vram.cache_pids(state);
    Ok(port)
}

// Per-model actions

async fn model_status(state: &SharedState, class: Class, model_id: &str) -> Result<Value, String> {
    let snap = state.snapshot();
    let rt = model_runtime(&snap, class, model_id);
    let runtime_view = state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == class && v.model_id == model_id);
    Ok(json!({
        "ok": true,
        "class": class.as_str(),
        "model_id": model_id,
        "engine": class.engine(),
        "enabled": rt.as_ref().map(|r| r.enabled),
        "warm_start": rt.as_ref().map(|r| r.warm_start),
        "idle_seconds": rt.as_ref().map(|r| r.idle_seconds),
        "image": rt.as_ref().map(|r| r.image.clone()),
        // `null` when nothing has ever started this model in this process —
        // distinct from a `stopped` entry, which the registry never keeps
        // around (§3.2: absent means "not running", not "unknown").
        "runtime": runtime_view,
    }))
}

async fn model_start(state: &SharedState, class: Class, model_id: &str) -> Result<Value, String> {
    match start_model(state, class, model_id).await {
        Ok(port) => Ok(json!({
            "ok": true, "class": class.as_str(), "model_id": model_id, "port": port,
            "message": format!("'{model_id}' ({class}) is up on port {port}"),
        })),
        Err(e) => Err(format!("starting '{model_id}' ({class}) failed: {e}")),
    }
}

/// `stop` refuses a model that is still serving requests unless the caller
/// passes `override=true` (design §3.6: "today `ops::container` stops
/// unconditionally, so this is new work, not an existing property").
async fn model_stop(
    state: &SharedState,
    class: Class,
    model_id: &str,
    force: bool,
) -> Result<Value, String> {
    match state.runtime().stop(class, model_id, force).await {
        Ok(()) => Ok(json!({
            "ok": true, "class": class.as_str(), "model_id": model_id,
            "message": format!("'{model_id}' ({class}) stopped"),
        })),
        Err(RuntimeError::Busy { in_flight, .. }) => Err(format!(
            "'{model_id}' ({class}) is still serving {in_flight} request(s) — not stopping it; \
             pass override=true to force"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// `restart` and `apply` are the same operation on a single model (design
/// §3.6: argv is rendered fresh at every start, so there is no drift concept
/// to distinguish "reapply config" from "bounce it") — stop it if running,
/// then start it with freshly rendered arguments. A refusal to stop (still
/// serving requests) leaves the container exactly as it was, running the
/// previous configuration, rather than being forced.
async fn model_apply(state: &SharedState, class: Class, model_id: &str) -> Result<Value, String> {
    if !state.runtime().contains(class, model_id) {
        // §3.6: "running → stop + start (fresh argv); **not running → no-op**".
        // There is no drift to correct — argv is rendered from the current
        // configuration at every start — so starting it here would silently
        // turn "apply my edit" into "and also put this model on the GPU",
        // which is a VRAM decision the caller did not ask for. The model is
        // named as configured, and nothing is started.
        let snap = state.snapshot();
        if model_runtime(&snap, class, model_id).is_none() {
            return Err(format!("no {class} model '{model_id}' is configured"));
        }
        return Ok(json!({
            "ok": true, "class": class.as_str(), "model_id": model_id,
            "applied": false, "running": false,
            "message": format!(
                "'{model_id}' ({class}) is not running — nothing to apply; its arguments are \
                 rendered fresh at every start, so the next start uses the current \
                 configuration. Use action=start to bring it up now."
            ),
        }));
    }
    let stopped = state.runtime().stop(class, model_id, false).await;
    if let Err(RuntimeError::Busy { in_flight, .. }) = &stopped {
        return Ok(json!({
            "ok": false, "class": class.as_str(), "model_id": model_id,
            "message": format!(
                "'{model_id}' ({class}) is still serving {in_flight} request(s) — its \
                 container keeps running the previous configuration; stop it (with \
                 override=true if needed) to apply the change"
            ),
        }));
    }
    // Per model: a row on the CPU is not held, and starts again (§2 of the
    // gpu-hold design, by `gpu_block_for`). The dispatch refuses `apply`
    // under a benchmark's lease before this runs, so this is the hold.
    let held = state.snapshot().gpu_block_for(class, model_id).is_some();
    // A stop that *failed* while held is the one outcome that must not be
    // rounded up to the sentence below. Without a hold the `start_model` after
    // it is the recovery — the next start replaces the container by name — but
    // under a hold nothing starts, nothing retries, and the container is still
    // holding GPU memory the owner asked for back. Saying "was stopped" here
    // would be simply false.
    if held {
        if let Err(e) = &stopped {
            return Ok(json!({
                "ok": false, "class": class.as_str(), "model_id": model_id,
                "applied": true, "running": false, "held": true,
                "message": format!(
                    "'{model_id}' ({class}) could NOT be stopped ({e}), and lmgw is holding the \
                     GPU so it is not started again either — its container may still be holding \
                     GPU memory, and nothing will retry it. The new configuration takes effect \
                     at the first start after the hold is released."
                ),
            }));
        }
    }
    // Stopped, and deliberately not started again (gpu-hold design §2): the
    // hold wants this container down anyway, so apply does its useful half and
    // says plainly when the edit takes effect. Reported as `applied: true`,
    // because it was — the row is what a start renders argv from, and the next
    // start is the one that will.
    if held {
        return Ok(json!({
            "ok": true, "class": class.as_str(), "model_id": model_id,
            "applied": true, "running": false, "held": true,
            "message": format!(
                "'{model_id}' ({class}) was stopped, and not started again — lmgw is holding \
                 the GPU. The new configuration takes effect at the first start after the hold \
                 is released."
            ),
        }));
    }
    match start_model(state, class, model_id).await {
        Ok(port) => Ok(json!({
            "ok": true, "class": class.as_str(), "model_id": model_id, "port": port,
            "message": format!(
                "'{model_id}' ({class}) is up on port {port} with the current configuration"
            ),
        })),
        Err(e) => Err(format!("starting '{model_id}' ({class}) failed: {e}")),
    }
}

async fn model_logs(
    state: &SharedState,
    class: Class,
    model_id: &str,
    tail: i64,
) -> Result<Value, String> {
    let tail = if tail <= 0 { 60usize } else { tail as usize };
    let snap = state.snapshot();
    // The live entry's own name when there is one; otherwise the name this
    // model would render today, so a container that was stopped (but not
    // removed — a plain `stop` never removes it, §3.6) still has its logs
    // read back by the exact same name a fresh start would reuse.
    let name = state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == class && v.model_id == model_id)
        .map(|v| v.container_name)
        .unwrap_or_else(|| {
            crate::runtime::container_name(&snap.settings.container_prefix, class, model_id)
        });
    let text = state.runtime().logs_tail(&name, tail).await?;
    Ok(json!({
        "ok": true, "class": class.as_str(), "model_id": model_id,
        "container": name, "tail": tail, "logs": text,
    }))
}

// Group actions

/// The same static pre-flight the old chat-only `apply` ran, generalized to
/// every class (design §8: "Group responses keep the per-model pre-flight
/// shape the old chat apply had"). Problems and advisories come back apart,
/// as [`model_warnings`] finds them: the advisories describe a model that
/// does start.
async fn chat_model_problems(dir: &str, m: &LocalModel) -> ModelChecks {
    let mut issues: Vec<String> = Vec::new();
    for (what, rel) in [
        ("gguf_path", Some(&m.gguf_path)),
        ("mmproj_path", m.params.mmproj_path.as_ref()),
        ("draft_gguf_path", m.params.draft_gguf_path.as_ref()),
    ] {
        if let Some(rel) = rel.filter(|r| !r.is_empty()) {
            if !dir.trim().is_empty() && !std::path::Path::new(dir).join(rel).is_file() {
                issues.push(format!("{what} '{rel}' is missing from the models dir"));
            }
        }
    }
    // A ladder rung's own gguf, same check (review findings 10 and S3): a
    // save that leaves (or makes) the row disabled skips `validate_ladder`
    // at save time precisely so a stale rung path never blocks turning a
    // model off, so this read-only pre-flight is where it has to surface
    // instead.
    for (i, r) in m.ladder.iter().enumerate() {
        if !dir.trim().is_empty() && !std::path::Path::new(dir).join(&r.gguf_path).is_file() {
            issues.push(format!(
                "ladder rung {} gguf_path '{}' is missing from the models dir",
                i + 2,
                r.gguf_path
            ));
        }
    }
    let as_new = NewLocalModel {
        model_id: m.model_id.clone(),
        gguf_path: m.gguf_path.clone(),
        params: m.params.clone(),
        args: m.args.clone(),
        idle_seconds: m.idle_seconds,
        enabled: m.enabled,
        public: m.public,
        image: m.image.clone(),
        extra_run_args: m.extra_run_args.clone(),
        warm_start: m.warm_start,
        hold_fallback_mode: m.hold_fallback_mode,
        hold_fallback: m.hold_fallback.clone(),
        capabilities_override: m.capabilities_override.clone(),
        ladder: m.ladder.clone(),
    };
    let checks = model_warnings(dir, &as_new).await;
    issues.extend(checks.problems);
    ModelChecks {
        problems: issues,
        advisories: checks.advisories,
    }
}

/// Audio models point at a model *directory*, not a single GGUF (§3.6).
fn audio_model_problems(dir: &str, m: &AudioModel) -> Vec<String> {
    let mut out = Vec::new();
    // A directory, or the GGUF file itself: audio.cpp loads either.
    if !dir.trim().is_empty() && !std::path::Path::new(dir).join(&m.path).exists() {
        out.push(format!("path '{}' is missing from the models dir", m.path));
    }
    out
}

/// An image row names a *set* of files and two maps of flags
/// (image-generation design §4), so its pre-flight has three questions the
/// other classes do not: does every file exist, is every key a flag this
/// engine has, and does the row name exactly one of the two ways to load a
/// pipeline.
///
/// Checked against the **embedded** vocabulary rather than a probe of the
/// row's image: this runs on a save and on every group apply, and neither may
/// depend on the GPU being free enough to run a throwaway container (§12.7).
/// A row using a flag newer than lmgw is therefore reported here and still
/// starts, because the start re-renders against the image's own `--help`.
pub(super) fn image_model_problems(dir: &str, m: &ImageModel) -> Vec<String> {
    use crate::runtime::image as image_cfg;

    let caps = crate::sdcpp_caps::SdcppCaps::embedded();
    let mut out = image_key_problems(&caps, &m.files, &m.args);

    let named = |k: &str| image_names(&caps, &m.files, k);
    match (named("model"), named("diffusion_model")) {
        (true, true) => out.push(
            "files names both 'model' and 'diffusion_model' — an all-in-one checkpoint and a \
             standalone diffusion model are alternatives, not a pair"
                .into(),
        ),
        (false, false) => out.push(
            "files names neither 'model' (an all-in-one checkpoint) nor 'diffusion_model' (a \
             standalone diffusion model) — one of the two is what loads the pipeline"
                .into(),
        ),
        _ => {}
    }

    let defaults: Vec<&str> = image_cfg::DEFAULT_DIRS.iter().map(|(k, _)| *k).collect();
    for (key, value) in &m.files {
        let resolved = caps.resolve_key(key);
        let Some(rel) = value.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
            out.push(format!(
                "files key '{key}' must be a non-empty path relative to the image models dir"
            ));
            continue;
        };
        // Checked for *every* key, the two default directories included: their
        // value is the one `files` path lmgw itself joins onto the models dir
        // and creates (`ensure_dirs`), so a `..` in it escapes the tree on the
        // host rather than inside the container.
        if let Err(e) = check_rel_path(rel, "image models dir") {
            out.push(format!("files key '{key}': {e}"));
            continue;
        }
        // The two directories lmgw renders unconditionally are created at
        // every start (`runtime::image::ensure_dirs`), so their absence now is
        // not a problem the owner has to do anything about. An unconfigured
        // models dir resolves nothing, so nothing is missing from it either —
        // the start refuses on the directory instead.
        if defaults.contains(&resolved.as_str()) || dir.trim().is_empty() {
            continue;
        }
        let path = image_cfg::file_target(dir, rel);
        let ok = if image_cfg::is_dir_key(&resolved) {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !ok {
            out.push(format!(
                "{key} '{rel}' is missing from the image models dir"
            ));
        }
    }
    out
}

/// Outcome of recreating one running model's container (group `apply`/
/// `restart`).
enum Recreated {
    Ok(u16),
    Busy(u32),
    Failed(String),
    /// Stopped and left down because the GPU hold is on (gpu-hold design §2).
    /// Group `apply`'s counterpart to the per-model `apply` message — group
    /// `restart` never reaches it, being refused before any stop is issued.
    Held,
}

/// Stop one running model and start it again with freshly rendered
/// arguments. A busy refusal leaves it running untouched — group apply/restart
/// never forces a stop, the same restraint the per-model actions take.
async fn recreate_one(state: &SharedState, class: Class, model_id: &str, held: bool) -> Recreated {
    match state.runtime().stop(class, model_id, false).await {
        // Under a hold the stop *is* the whole operation — the model stays
        // down, and the caller is told the configuration lands at the next
        // start. Restarting here would fail on `Fit::Held` anyway, but it
        // would be reported as a failed apply rather than as the hold doing
        // its job.
        Ok(()) if held => Recreated::Held,
        Ok(()) => match start_model(state, class, model_id).await {
            Ok(port) => Recreated::Ok(port),
            Err(e) => Recreated::Failed(format!("stopped, but restarting it failed: {e}")),
        },
        Err(RuntimeError::Busy { in_flight, .. }) => Recreated::Busy(in_flight),
        // A *failed* stop under a hold is not the hold doing its job: the
        // container is still on the card, the registry has already dropped its
        // entry, and nothing retries it. Reporting it as `Held` — "stopped,
        // starts again at release" — would be the exact opposite of what
        // happened, so the error is reported as an error (gpu-hold design §2:
        // hold refuses and stops, it never invents a success). No restart is
        // attempted either; `start_model` would only answer `Fit::Held`.
        Err(e) if held => Recreated::Failed(format!("stopping it failed: {e}")),
        // Once a stop has failed, lmgw's belief about the container is
        // worthless (runtime/registry/stop.rs's own reasoning for dropping the
        // entry regardless) — starting fresh is the recovery, not a second failure.
        Err(e) => match start_model(state, class, model_id).await {
            Ok(port) => Recreated::Ok(port),
            Err(e2) => Recreated::Failed(format!("{e}; then restarting it failed: {e2}")),
        },
    }
}

/// Recreate every currently running member of `class_filter` (`None` = every
/// class), concurrently. Returns `(recreated, busy, errors, held)`, each a
/// `Value` list ready to embed in a response — `held` is populated only while
/// the GPU hold is on, with the members it holds (a row on the CPU is not
/// held, and is recreated): for `apply` "recreate" means "stop, and start at
/// release"; a `restart` (`keep_held`) leaves them running untouched, since
/// stopping one it cannot start again is the destructive half of a restart.
async fn recreate_running(
    state: &SharedState,
    class_filter: Option<Class>,
    keep_held: bool,
) -> (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) {
    let snap = state.snapshot();
    type Members = Vec<(Class, String)>;
    let (running, kept): (Members, Members) = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| class_filter.is_none_or(|c| v.class == c))
        .map(|v| (v.class, v.model_id))
        .partition(|(c, m)| !keep_held || snap.gpu_block_for(*c, m).is_none());
    let outcomes = futures::future::join_all(running.iter().map(|(c, m)| {
        let held = snap.gpu_block_for(*c, m).is_some();
        recreate_one(state, *c, m, held)
    }))
    .await;

    let mut recreated = Vec::new();
    let mut busy = Vec::new();
    let mut errors = Vec::new();
    let mut stopped_held = Vec::new();
    for ((class, model_id), outcome) in running.iter().zip(outcomes) {
        match outcome {
            Recreated::Ok(port) => recreated
                .push(json!({ "class": class.as_str(), "model_id": model_id, "port": port })),
            Recreated::Busy(in_flight) => busy.push(
                json!({ "class": class.as_str(), "model_id": model_id, "in_flight": in_flight }),
            ),
            Recreated::Failed(message) => errors
                .push(json!({ "class": class.as_str(), "model_id": model_id, "error": message })),
            Recreated::Held => {
                stopped_held.push(json!({ "class": class.as_str(), "model_id": model_id }))
            }
        }
    }
    for (class, model_id) in kept {
        stopped_held.push(json!({ "class": class.as_str(), "model_id": model_id }));
    }
    (recreated, busy, errors, stopped_held)
}

async fn group_status(state: &SharedState, class_filter: Option<Class>) -> Result<Value, String> {
    let runtime: Vec<_> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| class_filter.is_none_or(|c| v.class == c))
        .collect();
    Ok(json!({
        "ok": true,
        "target": target_label(class_filter),
        "runtime": runtime,
        // One GPU behind every class, so the ledger is reported whole even
        // when the caller asked about a single group (design §8: "one
        // scheduler view, no third shape").
        "vram": state.vram.view(state).await,
    }))
}

/// Group `start` only starts that group's `warm_start`-flagged models — never
/// every configured model, which could ask for more VRAM than the box has
/// (design §8's documented restraint; a caller wanting a specific *other*
/// model up front uses `model=<id>`).
async fn group_start(state: &SharedState, class_filter: Option<Class>) -> Result<Value, String> {
    let snap = state.snapshot();
    let registry = state.runtime();
    let candidates: Vec<_> = model_runtimes(&snap)
        .into_iter()
        .filter(|r| r.enabled && r.warm_start)
        .filter(|r| class_filter.is_none_or(|c| r.class == c))
        .collect();
    let already_running: Vec<Value> = candidates
        .iter()
        .filter(|r| registry.contains(r.class, &r.model_id))
        .map(|r| json!({ "class": r.class.as_str(), "model_id": r.model_id }))
        .collect();
    // Under the GPU hold only the models on the CPU start; the rest are
    // named, not attempted (the dispatch refuses a group start under a
    // benchmark's lease before this runs).
    let (to_start, held): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .filter(|r| !registry.contains(r.class, &r.model_id))
        .partition(|r| snap.gpu_block_for(r.class, &r.model_id).is_none());
    let held: Vec<Value> = held
        .iter()
        .map(|r| json!({ "class": r.class.as_str(), "model_id": r.model_id }))
        .collect();

    let outcomes = futures::future::join_all(
        to_start
            .iter()
            .map(|r| start_model(state, r.class, &r.model_id)),
    )
    .await;

    let mut started = Vec::new();
    let mut errors = Vec::new();
    for (r, outcome) in to_start.iter().zip(outcomes) {
        match outcome {
            Ok(port) => started
                .push(json!({ "class": r.class.as_str(), "model_id": r.model_id, "port": port })),
            Err(e) => errors
                .push(json!({ "class": r.class.as_str(), "model_id": r.model_id, "error": e })),
        }
    }
    Ok(json!({
        "ok": errors.is_empty(),
        "target": target_label(class_filter),
        "started": started,
        "already_running": already_running,
        "held": held,
        "errors": errors,
        "note": "Group start only starts models flagged warm_start — starting every configured \
                 model in a class could ask for more VRAM than the box has. Start any other \
                 model on demand with model=<id> action=start.",
    }))
}

/// Stop every running member of `class_filter`. `force` (the `override`
/// input) is honored per model exactly as the per-model `stop` action honors
/// it — a group stop is N per-model stops, not a new refusal rule.
async fn group_stop(
    state: &SharedState,
    class_filter: Option<Class>,
    force: bool,
) -> Result<Value, String> {
    let running: Vec<(Class, String)> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| class_filter.is_none_or(|c| v.class == c))
        .map(|v| (v.class, v.model_id))
        .collect();
    let registry = state.runtime();
    let outcomes =
        futures::future::join_all(running.iter().map(|(c, m)| registry.stop(*c, m, force))).await;

    let mut stopped = Vec::new();
    let mut busy = Vec::new();
    let mut errors = Vec::new();
    for ((class, model_id), outcome) in running.iter().zip(outcomes) {
        match outcome {
            Ok(()) => stopped.push(json!({ "class": class.as_str(), "model_id": model_id })),
            Err(RuntimeError::Busy { in_flight, .. }) => busy.push(
                json!({ "class": class.as_str(), "model_id": model_id, "in_flight": in_flight }),
            ),
            Err(e) => errors.push(
                json!({ "class": class.as_str(), "model_id": model_id, "error": e.to_string() }),
            ),
        }
    }
    let message = if busy.is_empty() {
        format!("stopped {} container(s)", stopped.len())
    } else {
        format!(
            "stopped {} container(s); {} still serving requests (pass override=true to force)",
            stopped.len(),
            busy.len()
        )
    };
    Ok(json!({
        "ok": errors.is_empty(),
        "target": target_label(class_filter),
        "stopped": stopped,
        "busy": busy,
        "errors": errors,
        "message": message,
    }))
}

/// Stop and start every running member — no pre-flight, unlike `apply`
/// (design §8: apply and restart differ at group granularity even though
/// they are identical at the per-model one, because apply additionally
/// reports the static checks).
async fn group_restart(state: &SharedState, class_filter: Option<Class>) -> Result<Value, String> {
    // Under the hold only the members on the CPU are restarted; the rest
    // are left running untouched and named in `held`.
    let (recreated, busy, errors, held) = recreate_running(state, class_filter, true).await;
    let held_note = if held.is_empty() {
        String::new()
    } else {
        format!(
            "; {} on the GPU left as they are — lmgw is holding the GPU, so they would not \
             start again",
            held.len()
        )
    };
    Ok(json!({
        "ok": errors.is_empty(),
        "target": target_label(class_filter),
        "restarted": recreated,
        "busy": busy,
        "held": held,
        "errors": errors,
        "message": format!("restarted {} running container(s){held_note}", recreated.len()),
    }))
}

/// Group `apply`: recreate every running member, and report the static
/// pre-flight over every *enabled* model in scope, not just the running ones
/// (design §8, generalizing the old chat-only `models_enabled` /
/// `models_with_problems` / `problems` shape to every class).
///
/// **Staleness.** The registry does not track the argv a running container
/// was started with (§3.2's `Entry` carries no rendered spec), so there is no
/// way to tell "this one's config changed" from "this one didn't" without
/// re-rendering and diffing on every apply. Per design §8's explicit fallback
/// ("if not tracked, recreate all running members — state which you did"),
/// apply recreates **every** running member of the group unconditionally —
/// stated in the response's `message`, not left implicit.
async fn group_apply(state: &SharedState, class_filter: Option<Class>) -> Result<Value, String> {
    let snap = state.snapshot();
    let mut models_enabled = 0usize;
    let mut problems: Vec<Value> = Vec::new();
    let mut advisories: Vec<Value> = Vec::new();

    if class_filter.is_none_or(|c| c == Class::Chat) {
        let dir = snap.settings.router.models_dir.clone();
        for m in snap.local_models.iter().filter(|m| m.enabled) {
            models_enabled += 1;
            let checks = chat_model_problems(&dir, m).await;
            if !checks.problems.is_empty() {
                problems.push(
                    json!({ "class": "chat", "model_id": m.model_id, "issues": checks.problems }),
                );
            }
            if !checks.advisories.is_empty() {
                advisories.push(json!({
                    "class": "chat", "model_id": m.model_id, "advisories": checks.advisories,
                }));
            }
        }
    }
    if class_filter.is_none_or(|c| c == Class::Aux) {
        let dir = snap.settings.aux_router.models_dir.clone();
        for m in snap.aux_models.iter().filter(|m| m.enabled) {
            models_enabled += 1;
            let issues = aux_model_problems(&dir, m).await;
            if !issues.is_empty() {
                problems.push(json!({ "class": "aux", "model_id": m.model_id, "issues": issues }));
            }
        }
    }
    if class_filter.is_none_or(|c| c == Class::Audio) {
        let dir = snap.settings.audio.models_dir.clone();
        for m in snap.audio_models.iter().filter(|m| m.enabled) {
            models_enabled += 1;
            let issues = audio_model_problems(&dir, m);
            if !issues.is_empty() {
                problems
                    .push(json!({ "class": "audio", "model_id": m.model_id, "issues": issues }));
            }
        }
    }
    if class_filter.is_none_or(|c| c == Class::Image) {
        let dir = snap.settings.image.models_dir.clone();
        for m in snap.image_models.iter().filter(|m| m.enabled) {
            models_enabled += 1;
            let issues = image_model_problems(&dir, m);
            if !issues.is_empty() {
                problems
                    .push(json!({ "class": "image", "model_id": m.model_id, "issues": issues }));
            }
        }
    }

    let (recreated, busy, errors, held) = recreate_running(state, class_filter, false).await;
    let held_note = if held.is_empty() {
        String::new()
    } else {
        format!(
            " {} of them were stopped and not started again — lmgw is holding the GPU, so their \
             new configuration takes effect at the first start after the hold is released.",
            held.len()
        )
    };
    Ok(json!({
        "ok": errors.is_empty(),
        "target": target_label(class_filter),
        "held": held,
        "models_enabled": models_enabled,
        "models_with_problems": problems.len(),
        "problems": problems,
        // Models that start but misbehave under some input; never counted as
        // problems.
        "models_with_advisories": advisories.len(),
        "advisories": advisories,
        "recreated": recreated,
        "busy": busy,
        "errors": errors,
        "message": format!(
            "recreated {} running container(s) — argv isn't tracked per running container, so \
             apply recreates every member currently up rather than only the ones whose \
             configuration actually changed.{held_note}",
            recreated.len()
        ),
        "note": "The problems list is static checks only — a clean result does not prove a \
                 model loads. Run lmgw__local_model_test to confirm. 'advisories' are not \
                 problems: those models start, but misbehave under some input.",
    }))
}

// Dispatch

/// Per-model-container lifecycle (design §8): `target` names a **group**
/// (`all | chat | aux | audio`, `embed` accepted for `aux`); the new `model`
/// parameter addresses one model within a group by id, with its class found
/// automatically from the local/aux/audio tables. `target` may be omitted
/// when `model` is given; when both are given they must agree, so a model
/// literally named `all` stays reachable without target's meaning being
/// overloaded.
///
/// `force` is the `override` input: it forces a `stop` (per-model or group)
/// that would otherwise be refused because the model is still serving
/// requests. `tail` bounds the `logs` action's line count (model only).
pub async fn container(
    state: &SharedState,
    target: Option<&str>,
    model: Option<&str>,
    action: &str,
    force: bool,
    tail: Option<i64>,
) -> Result<Value, String> {
    let snap = state.snapshot();
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    let target = target.map(str::trim).filter(|s| !s.is_empty());

    // `None` = no `target` given; `Some(None)` = `target=all`; `Some(Some(c))`
    // = `target=<class>`.
    let group_target: Option<Option<Class>> = target.map(normalize_group_target).transpose()?;

    let model_class: Option<Class> = match model {
        Some(mid) => Some(resolve_model_class(&snap, mid, group_target.flatten())?),
        None => None,
    };

    // The GPU hold's operator gate (gpu-hold design §2), before either
    // dispatch and therefore before any verb's stop step. `stop`, `status` and
    // `logs` are untouched — a hold wants models stopped, and reading is never
    // a GPU decision. Per model: a row on the CPU is not held. A group verb
    // decides per member (`group_start`, `group_restart`) when its group has
    // a model on the CPU, and is refused whole as before when it has none.
    let held = match model_class.zip(model) {
        Some((c, m)) => snap.gpu_block_for(c, m).is_some(),
        None => !model_runtimes(&snap).iter().any(|r| {
            r.enabled
                && group_target.flatten().is_none_or(|c| r.class == c)
                && !r.placement().is_gpu()
        }),
    };
    if snap.settings.hold.active && held && matches!(action, "start" | "restart") {
        return Err(hold_refusal(action));
    }
    // A benchmark run holds the card (benchmark design §3.2): the same gate,
    // for `apply` too — its stop would be followed by a start the lease
    // refuses, and the run has already stopped everything anyway.
    if let Some(l) = &snap.gpu_lease {
        if matches!(action, "start" | "restart" | "apply") {
            return Err(format!(
                "benchmark run {} has the GPU to itself, so '{action}' is refused before \
                 anything is stopped — nothing starts on the GPU until the run ends. Wait for \
                 it, cancel it (Benchmarks page, or lmgw__bench_cancel), or use action=stop, \
                 status or logs.",
                l.run_id
            ));
        }
    }

    if let Some(class) = model_class {
        let mid = model.expect("model_class is only Some when model is Some");
        return match action {
            "status" => model_status(state, class, mid).await,
            "start" => model_start(state, class, mid).await,
            "stop" => model_stop(state, class, mid, force).await,
            "restart" | "apply" => model_apply(state, class, mid).await,
            "logs" => model_logs(state, class, mid, tail.unwrap_or(60)).await,
            other => Err(format!(
                "unknown action '{other}' (status|start|stop|restart|apply|logs)"
            )),
        };
    }

    let class_filter = group_target.ok_or("pass target or model")?;
    match action {
        "status" => group_status(state, class_filter).await,
        "start" => group_start(state, class_filter).await,
        "stop" => group_stop(state, class_filter, force).await,
        "restart" => group_restart(state, class_filter).await,
        "apply" => group_apply(state, class_filter).await,
        "logs" => Err("action 'logs' requires model=<id> — there is no group log stream".into()),
        other => Err(format!(
            "unknown action '{other}' (status|start|stop|restart|apply)"
        )),
    }
}
