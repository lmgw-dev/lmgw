//! From a request to a run (benchmark design §3, §8.1 `bench_plan`): the
//! row with its overrides applied (§3.5), the container it renders (§3.4),
//! the identity it will be stored under (§6), the provisional points and
//! probes (§4.2, §5), what a start stops (§3.2 step 3) — and whether it can
//! start at all.
//!
//! `bench_plan` answers all of it without starting anything; `bench_start`
//! and the run itself go through the same [`prepare`] and [`render`], so the
//! confirmation shows exactly what runs.

use std::path::Path;

use lmgw_api_types::bench::{PointPlan, ProbeKind, ServerFacts, SuiteParams};
use lmgw_api_types::bench_ops::{
    BenchArgs, BenchBlocked, BenchOverrides, BenchPlan, BenchSettings, BenchStop, BlockedReason,
    PlannedProbe,
};

use super::identity::{build_identity, model_identity, settings_hash};
use super::launcher::{self, container_name, run_argv};
use super::probes::{skip_reason, Caps};
use super::{points, run};
use crate::config::{LocalModel, Snapshot};
use crate::jobs::JobKind;
use crate::runtime::argv::{LlamaArgs, RenderSpec};
use crate::runtime::descriptor::{model_runtime_at, ModelRuntime};
use crate::runtime::registry::RuntimeState;
use crate::runtime::Class;
use crate::state::SharedState;

/// Everything a run of one request needs, derived once.
pub struct Prepared {
    pub row: LocalModel,
    /// The row at the rung, with the overrides applied.
    pub runtime: ModelRuntime,
    pub overrides: BenchOverrides,
    pub settings: BenchSettings,
    pub settings_hash: String,
    pub model: lmgw_api_types::bench::ModelIdentity,
    pub params: SuiteParams,
    pub models_dir: String,
    pub caps: Caps,
    /// The row's rungs, base included.
    pub rungs: u32,
}

impl Prepared {
    /// The effective `LlamaParams`.
    pub fn llama_params(&self) -> Option<&crate::config::LlamaParams> {
        match &self.runtime.llama {
            Some(LlamaArgs::Chat { params, .. }) => Some(params),
            _ => None,
        }
    }
}

/// Freeform flags that load a drafter, dropped with `no_draft` (§3.5) next to
/// the typed drafter fields. Each takes one value.
const DRAFT_FLAGS: [&str; 3] = ["--model-draft", "-md", "--spec-type"];

/// §3.5: the overrides, applied to the bench container's descriptor only —
/// never to the row.
pub fn apply_overrides(rt: &mut ModelRuntime, ov: &BenchOverrides) {
    if let Some(image) = &ov.image {
        rt.image = image.clone();
    }
    let Some(LlamaArgs::Chat { params, args, .. }) = rt.llama.as_mut() else {
        return;
    };
    if let Some(v) = ov.ctx_size {
        params.ctx_size = Some(v);
    }
    if let Some(v) = ov.parallel {
        params.parallel = Some(v);
    }
    if let Some(v) = ov.ubatch_size {
        params.ubatch_size = Some(v);
    }
    if let Some(v) = ov.batch_size {
        params.batch_size = Some(v);
    }
    if let Some(v) = &ov.cache_type_k {
        params.cache_type_k = Some(v.clone());
    }
    if let Some(v) = &ov.cache_type_v {
        params.cache_type_v = Some(v.clone());
    }
    if let Some(v) = &ov.flash_attn {
        params.flash_attn = Some(v.clone());
    }
    if let Some(v) = ov.kv_unified {
        params.kv_unified = Some(v);
    }
    if let Some(v) = ov.n_gpu_layers {
        params.n_gpu_layers = Some(v);
    }
    if ov.no_draft {
        params.draft_gguf_path = None;
        params.spec_type = None;
        params.spec_draft_n_max = None;
        params.spec_draft_n_min = None;
        params.spec_draft_ngl = None;
        let mut kept = Vec::with_capacity(args.len());
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let flag = a.split_once('=').map_or(a.as_str(), |(f, _)| f);
            if DRAFT_FLAGS.contains(&flag) {
                if !a.contains('=') {
                    it.next();
                }
                continue;
            }
            kept.push(a.clone());
        }
        *args = kept;
    }
}

/// §6 `settings` of a descriptor.
pub fn settings_of(rt: &ModelRuntime, ov: &BenchOverrides) -> BenchSettings {
    let (gguf_path, params, args) = match &rt.llama {
        Some(LlamaArgs::Chat {
            gguf_path,
            params,
            args,
        }) => (
            gguf_path.clone(),
            serde_json::to_value(params).unwrap_or_default(),
            args.clone(),
        ),
        _ => Default::default(),
    };
    BenchSettings {
        image: rt.image.clone(),
        gguf_path,
        rung: ov.rung,
        params,
        args,
        extra_run_args: rt.extra_run_args.clone(),
        overrides: ov.clone(),
    }
}

/// The bench container's render and argv (§3.4): the row's own spec with the
/// bench name, `port` published, and the bench label.
pub fn render(
    prep: &Prepared,
    prefix: &str,
    port: u16,
    data_dir: &Path,
    run_id: i64,
) -> Result<(RenderSpec, Vec<String>), String> {
    let mut spec = prep
        .runtime
        .preview_spec(prefix, port, &prep.models_dir, data_dir)
        .map_err(|e| format!("rendering the container spec failed: {e}"))?;
    spec.container_name = container_name(prefix, run_id);
    let argv = run_argv(&spec, run_id);
    Ok((spec, argv))
}

fn blocked(reason: BlockedReason, message: String) -> BenchBlocked {
    BenchBlocked {
        reason,
        message,
        run_id: None,
    }
}

/// Resolve a request against the snapshot: the row, its rung, the overrides,
/// the weights on disk. `Err` is the row-level reason a run cannot start.
pub async fn prepare(
    state: &SharedState,
    snap: &Snapshot,
    args: &BenchArgs,
) -> Result<Prepared, BenchBlocked> {
    let id = args.model_id.trim();
    let Some(row) = snap.local_models.iter().find(|m| m.model_id == id).cloned() else {
        let other = if snap.aux_models.iter().any(|m| m.model_id == id) {
            Some("an aux (embedding/rerank) model")
        } else if snap.audio_models.iter().any(|m| m.model_id == id) {
            Some("an audio model")
        } else if snap.image_models.iter().any(|m| m.model_id == id) {
            Some("an image model")
        } else {
            None
        };
        return Err(match other {
            Some(what) => blocked(
                BlockedReason::NotChat,
                format!("'{id}' is {what}; a benchmark runs local chat models only"),
            ),
            None => blocked(
                BlockedReason::RowMissing,
                format!("no local chat model '{id}' is configured"),
            ),
        });
    };
    if !row.enabled {
        return Err(blocked(
            BlockedReason::RowDisabled,
            format!("'{id}' is disabled — enable it first"),
        ));
    }
    let overrides = args.overrides();
    if let Some(image) = &overrides.image {
        check_image(image).map_err(|e| blocked(BlockedReason::InvalidImage, e))?;
    }
    let top = row.top_rung();
    if overrides.rung as usize > top {
        return Err(blocked(
            BlockedReason::RungOutOfRange,
            format!(
                "'{id}' has {} rung(s) (0 = base, up to {top}); rung {} does not exist",
                top + 1,
                overrides.rung
            ),
        ));
    }
    let models_dir = snap.settings.router.models_dir.clone();
    if models_dir.trim().is_empty() {
        return Err(blocked(
            BlockedReason::NoWeights,
            "the chat models dir is not configured (Settings → Local models)".into(),
        ));
    }
    let mut runtime = model_runtime_at(snap, Class::Chat, id, overrides.rung as usize)
        .expect("the row was found above");
    apply_overrides(&mut runtime, &overrides);
    let settings = settings_of(&runtime, &overrides);
    let model = model_identity(state, &models_dir, id, &settings.gguf_path, overrides.rung)
        .await
        .map_err(|e| blocked(BlockedReason::NoWeights, e))?;
    let mut params = args.params();
    if let Some(tune) = state.bench.tuning().params {
        tune(&mut params);
    }
    let caps = caps_of(state, &row).await;
    Ok(Prepared {
        settings_hash: settings_hash(&settings),
        rungs: top as u32 + 1,
        row,
        runtime,
        overrides,
        settings,
        model,
        params,
        models_dir,
        caps,
    })
}

/// The `image` override goes onto `podman image inspect` and `podman run` as
/// one argument: a leading `-` would be read as a flag, and whitespace or a
/// control character is no image reference (the check `backends/pull.rs`
/// makes of a reference to pull).
pub fn check_image(image: &str) -> Result<(), String> {
    if image.starts_with('-') {
        return Err(format!(
            "the image override '{image}' starts with '-', which podman would read as a flag —              name an image reference"
        ));
    }
    if image.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "the image override '{image}' holds whitespace — an image reference is one word"
        ));
    }
    Ok(())
}

/// The row's derived capabilities as the probes read them (§5): `None` is
/// "unknown", and an unknown capability runs its probe (decision 15).
pub async fn caps_of(state: &SharedState, row: &LocalModel) -> Caps {
    let derived = crate::capabilities::exposed::derived_for_local(state, row).await;
    let caps = derived.capabilities;
    let reasoning = caps.as_ref().and_then(|c| {
        c.reasoning
            .as_ref()
            .map(|r| !(r.kind == "fixed" && r.enabled == Some(false)))
    });
    let tools = caps
        .as_ref()
        .and_then(|c| c.tool_calls.as_ref())
        .map(|t| t.kind != "none");
    let projector = (row.params.mmproj_path.is_some() && !row.params.no_mmproj)
        || caps.as_ref().and_then(|c| c.vision) == Some(true);
    Caps {
        reasoning,
        tools,
        projector,
    }
}

/// §4.2's points from the row's own numbers (`provisional`): the per-slot
/// context the row asks for, or — when it sets none — nothing yet, said so.
pub async fn provisional_points(state: &SharedState, prep: &Prepared) -> PointPlan {
    let Some(params) = prep.llama_params() else {
        return PointPlan::default();
    };
    let path = Path::new(&prep.models_dir).join(&prep.settings.gguf_path);
    let trained = state
        .gguf_cache
        .summarize_cached(&path)
        .await
        .ok()
        .and_then(|s| s.context_length)
        .map(|c| c as i64);
    let n_slots = params.effective_slots().max(1) as u32;
    // A shared pool (unified KV): its size, as llama-server sizes it.
    let pool = params
        .effective_kv_unified()
        .then(|| params.pool_tokens())
        .flatten()
        .filter(|p| *p > 0)
        .map(|p| p as u64);
    let mut plan = match params.per_request_ctx(trained).filter(|s| *s > 0) {
        Some(s) => points::derive(s as u64, n_slots, pool, &prep.params),
        None => PointPlan {
            n_slots,
            generate_tokens: prep.params.generate_tokens,
            repetitions: prep.params.repetitions,
            notes: vec![
                "the row sets no context the per-slot size can be derived from, so the points \
                 are only known once the server is up (they are read from its /slots)"
                    .into(),
            ],
            ..Default::default()
        },
    };
    plan.provisional = true;
    plan
}

/// The probes the plan would run (§5), judged on the row's capabilities and
/// the provisional context.
pub fn planned_probes(prep: &Prepared, plan: &PointPlan) -> Vec<PlannedProbe> {
    let facts = ServerFacts {
        n_slots: plan.n_slots,
        per_slot_ctx: plan.per_slot_ctx,
        ..Default::default()
    };
    ProbeKind::ALL
        .into_iter()
        .map(|probe| PlannedProbe {
            probe,
            skip: if probe == ProbeKind::Needle && plan.per_slot_ctx == 0 {
                None
            } else {
                skip_reason(probe, prep.caps, &facts, plan.needle_tokens)
            },
        })
        .collect()
}

/// Every lmgw container on the card (§3.2 step 3), each with whether it is
/// busy: serving lmgw requests or starting, or — asked of its `/slots`, as
/// the hold's sweep asks — generating for a client on its own port.
pub async fn stops(state: &SharedState) -> Vec<BenchStop> {
    let mut out = Vec::new();
    for v in state.runtime().list() {
        let mut busy = v.state != RuntimeState::Ready || v.in_flight > 0;
        if !busy && v.port != 0 {
            busy = crate::vram::busy_slots(&state.http, v.port, crate::vram::CONTROL_TIMEOUT)
                .await
                .is_some_and(|n| n > 0);
        }
        out.push(BenchStop {
            class: v.class.as_str().to_string(),
            model_id: v.model_id.clone(),
            container_name: v.container_name.clone(),
            state: v.state.as_str().to_string(),
            busy,
            in_flight: v.in_flight as usize,
        });
    }
    out
}

/// Why nothing can start *now*, apart from the row: the hold, boot still
/// adopting what a previous lmgw left running, or another run holding the
/// card (§3.2 step 1, §3.1).
pub fn blocked_now(state: &SharedState, snap: &Snapshot) -> Option<BenchBlocked> {
    if snap.settings.hold.active {
        return Some(blocked(
            BlockedReason::Hold,
            "the GPU hold is on — a benchmark is lmgw using VRAM, which the hold rules out. \
             Release it (tray, dashboard titlebar, or lmgw__hold_set active=false) first."
                .into(),
        ));
    }
    // Boot runs unawaited: until reconciliation has adopted what podman
    // still runs, those containers are not in the registry, so the drain
    // would not stop them and the run would measure beside them (§13
    // decision 45).
    if !state.vram.boot_settled() {
        return Some(blocked(
            BlockedReason::Booting,
            "lmgw has just started and is still adopting the containers a previous lmgw left \
             running — until it has, a run could not stop them. Try again in a moment."
                .into(),
        ));
    }
    let live = state
        .jobs
        .live_by_key(JobKind::Benchmark, lmgw_api_types::bench_ops::JOB_KEY);
    let current = state.bench.current();
    if live.is_some() || current.is_some() {
        let run_id = current.map(|c| c.run_id);
        return Some(BenchBlocked {
            reason: BlockedReason::RunGoing,
            message: format!(
                "benchmark run {} is going — one run at a time; wait for it, or cancel it",
                run_id.map_or("?".to_string(), |i| i.to_string())
            ),
            run_id,
        });
    }
    None
}

/// `bench_plan` (§8.1).
pub async fn plan(state: &SharedState, args: &BenchArgs) -> BenchPlan {
    let snap = state.snapshot();
    let mut out = BenchPlan {
        model_id: args.model_id.trim().to_string(),
        rung: args.rung,
        params: args.params(),
        stops: stops(state).await,
        ..Default::default()
    };
    // A container an earlier run could not remove (§13 decision 48): it is
    // not in `stops` — no registry entry — but it is on the card.
    for s in state.bench.stranded() {
        out.warnings.push(format!(
            "benchmark run {}'s container {} could not be removed ({}) and may still hold GPU \
             memory — a run removes it before it measures, and fails if it still cannot; or \
             remove it by hand: podman rm -f {}",
            s.run_id, s.name, s.error, s.name
        ));
    }
    let prep = match prepare(state, &snap, args).await {
        Ok(p) => p,
        Err(b) => {
            out.blocked = Some(b);
            return out;
        }
    };
    out.rungs = prep.rungs;
    out.params = prep.params.clone();
    out.settings = prep.settings.clone();
    out.settings_hash = prep.settings_hash.clone();
    out.model = prep.model.clone();
    let prefix = &snap.settings.container_prefix;
    match render(&prep, prefix, 0, &state.data_dir, 0) {
        Ok((_, argv)) => {
            // Rendered as run 0 on port 0; shown with the two placeholders
            // a start fills in.
            let name = container_name(prefix, 0);
            let label = format!("{}=0", launcher::BENCH_LABEL);
            let shown: Vec<String> = argv
                .into_iter()
                .map(|a| match a {
                    a if a == "127.0.0.1:0:8080" => "127.0.0.1:<port>:8080".to_string(),
                    a if a == name => format!("{prefix}-bench-<run id>"),
                    a if a == label => format!("{}=<run id>", launcher::BENCH_LABEL),
                    a => a,
                })
                .collect();
            out.command_line = launcher::command_line(&shown);
        }
        Err(e) => out.warnings.push(e),
    }
    let launcher = state.bench.launcher(state);
    match launcher::image_facts(launcher.as_ref(), &prep.runtime.image).await {
        Ok((id, labels)) => out.build = build_identity(&prep.runtime.image, Some(id), labels),
        Err(e) => {
            out.build = build_identity(&prep.runtime.image, None, Default::default());
            out.blocked = Some(blocked(
                BlockedReason::ImageMissing,
                format!(
                    "the image '{}' is not on this machine ({e}) — pull or build it first; a \
                     run would otherwise time the download as its load",
                    prep.runtime.image
                ),
            ));
        }
    }
    out.points = provisional_points(state, &prep).await;
    out.total_steps = run::total_steps(&out.points, &out.params);
    out.probes = planned_probes(&prep, &out.points);
    if let Some(b) = blocked_now(state, &snap) {
        // The card-level reasons outrank a missing image: they are what
        // clears first, and what the owner acts on.
        out.blocked = Some(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlamaParams;

    fn chat_runtime() -> ModelRuntime {
        let snap = {
            let mut s = Snapshot::default();
            s.settings.router.image = "localhost/lmgw-llama-server:official-master".into();
            s.local_models.push(LocalModel {
                model_id: "qwen".into(),
                gguf_path: "qwen/q4.gguf".into(),
                params: LlamaParams {
                    ctx_size: Some(16384),
                    parallel: Some(2),
                    draft_gguf_path: Some("qwen/mtp.gguf".into()),
                    spec_type: Some("draft-mtp".into()),
                    ..Default::default()
                },
                args: vec![
                    "--jinja".into(),
                    "--model-draft".into(),
                    "/models/x.gguf".into(),
                    "--spec-type=ngram".into(),
                ],
                id: 1,
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
                capabilities_override: None,
                ladder: vec![],
            });
            s
        };
        model_runtime_at(&snap, Class::Chat, "qwen", 0).unwrap()
    }

    #[test]
    fn overrides_change_the_bench_container_only() {
        let mut rt = chat_runtime();
        let ov = BenchOverrides {
            image: Some("localhost/lmgw-llama-server:ik-main".into()),
            ctx_size: Some(8192),
            cache_type_k: Some("q8_0".into()),
            no_draft: true,
            ..Default::default()
        };
        apply_overrides(&mut rt, &ov);
        assert_eq!(rt.image, "localhost/lmgw-llama-server:ik-main");
        let Some(LlamaArgs::Chat { params, args, .. }) = &rt.llama else {
            panic!("chat")
        };
        assert_eq!(params.ctx_size, Some(8192));
        assert_eq!(params.parallel, Some(2), "untouched");
        assert_eq!(params.cache_type_k.as_deref(), Some("q8_0"));
        assert_eq!(params.draft_gguf_path, None);
        assert_eq!(params.spec_type, None);
        assert_eq!(args, &vec!["--jinja".to_string()]);
        let s = settings_of(&rt, &ov);
        assert_eq!(s.params["ctx_size"], 8192);
        assert_eq!(s.gguf_path, "qwen/q4.gguf");
    }

    #[test]
    fn the_bench_container_is_named_labelled_and_published() {
        let rt = chat_runtime();
        let mut spec = rt
            .preview_spec("lmgw", 41234, "/models-dir", Path::new("/tmp"))
            .unwrap();
        spec.container_name = container_name("lmgw", 9);
        let argv = run_argv(&spec, 9);
        let pos = |t: &str| argv.iter().position(|a| a == t);
        assert_eq!(argv[pos("--name").unwrap() + 1], "lmgw-bench-9");
        assert!(argv.contains(&"lmgw.instance=lmgw".to_string()));
        let bench = pos("lmgw.bench=9").expect("the bench label");
        assert_eq!(argv[bench - 1], "--label");
        assert!(argv.contains(&"127.0.0.1:41234:8080".to_string()));
        // The engine flags are the row's, exactly as a model start renders them.
        let engine = crate::runtime::argv::render_engine_args(&spec);
        assert!(argv.ends_with(&engine));
    }
}
