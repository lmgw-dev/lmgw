//! Local chat models: the sparse `LocalModelPatch`, its overlay onto
//! `LlamaParams`, and the `local_model_get`/`local_model_check`/
//! `local_model_set` ops themselves.

use serde::Deserialize;
use serde_json::Value;

use lmgw_api_types as dto;

use crate::config::{HopKey, LocalModel};
use crate::detached::detached;
use crate::runtime::argv;
use crate::runtime::descriptor::model_runtime_at;
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::store::{self, NewLocalModel};

use super::*;

/// Sparse patch for a local (llama.cpp) model.
///
/// Unlike the other model patches this one is *complete*: every llama.cpp
/// parameter of the row is reachable, including multimodal and speculative
/// setups, without the freeform `extra_args` escape hatch or knowledge of
/// llama-server's path conventions. An `update` preserves every field not
/// named.
///
/// Because an omitted field means "leave as is", there is no value a caller can
/// send to *unset* a field — hence `clear`, a list of field names to reset.
/// That keeps every argument a flat scalar while still allowing "remove the
/// drafter".
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct LocalModelPatch {
    pub action: String,
    pub id: Option<i64>,
    pub model_id: Option<String>,
    pub gguf_path: Option<String>,

    // -- multimodal --
    pub mmproj_path: Option<String>,
    pub no_mmproj: Option<bool>,

    // -- memory & placement --
    pub ctx_size: Option<i64>,
    pub n_predict: Option<i64>,
    pub n_gpu_layers: Option<i64>,
    /// `--n-cpu-moe`: keep the routed experts of a mixture-of-experts
    /// model's first N layers in host memory. 0 or more.
    pub n_cpu_moe: Option<i64>,
    /// `--moe-cache-mib`: a cache on the card, in MiB, for the most used of
    /// the experts kept in host memory. 0 or more.
    pub moe_cache_mib: Option<i64>,
    pub threads: Option<i64>,
    pub batch_size: Option<i64>,
    pub ubatch_size: Option<i64>,
    pub parallel: Option<i64>,
    /// `kv_unified` tri-state: `true` renders
    /// `--kv-unified`, `false` renders `--no-kv-unified`, absent leaves
    /// llama-server's own default alone.
    pub kv_unified: Option<bool>,
    /// `--kv-unified-per-slot`: per-request cap on a unified row's context,
    /// and (only when `ctx_size` is unset) how the shared pool is sized.
    /// Refused at save time on a split row.
    pub kv_unified_per_slot: Option<i64>,
    pub flash_attn: Option<String>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    pub cache_ram: Option<i64>,
    pub fit: Option<String>,
    pub fit_ctx: Option<i64>,

    // -- chat template & reasoning --
    pub jinja: Option<bool>,
    pub chat_template_file: Option<String>,
    pub reasoning_format: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_budget: Option<i64>,
    pub reasoning_preserve: Option<bool>,
    pub reasoning_effort: Option<String>,
    /// `--chat-template-kwargs` as the JSON object string llama-server takes.
    /// A flat scalar rather than a nested object because the whole schema is;
    /// it is parsed and validated on the way in, so a malformed one fails here and not at model load.
    pub chat_template_kwargs: Option<String>,

    // -- sampling --
    pub temp: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub seed: Option<i64>,

    // -- speculative decoding --
    pub draft_gguf_path: Option<String>,
    pub spec_type: Option<String>,
    pub spec_draft_n_max: Option<i64>,
    pub spec_draft_n_min: Option<i64>,
    pub spec_draft_ngl: Option<String>,

    // -- everything else --
    /// Raw llama-server flags with no dedicated field, one argument per entry.
    /// Absent leaves them unchanged and an empty list clears them. Validated
    /// against the running build; prefer a real field where one exists.
    pub extra_args: Option<Vec<String>>,
    pub idle_seconds: Option<i64>,
    pub enabled: Option<bool>,
    pub public: Option<bool>,

    // -- per-model container overrides (per-model-containers design §3.1) --
    // Resolved against the chat class settings' own image/extra_run_args by
    // `runtime::descriptor`.
    /// Podman image override for this model's own container. Empty (or
    /// absent) leaves it unset; `clear` is what reverts a previously set
    /// override back to inheriting the class image.
    pub image: Option<String>,
    /// `podman run` args override, one entry per argument. Absent leaves it
    /// unchanged; an empty list is no override either, and `clear` reverts a
    /// previously set override back to inheriting the class's
    /// `extra_run_args`. An override is never stored as an empty list.
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch, once the per-model
    /// runtime exists.
    pub warm_start: Option<bool>,

    // -- fallback (openai egress design §14.2) --
    /// The model or alias that answers when this one cannot: the GPU hold,
    /// VRAM outside lmgw, background traffic deferred, or a disabled row. A
    /// local model id, a passthrough id or a plain alias of the same model
    /// type; not a candidate alias. Chains are allowed; a loop is refused.
    /// `clear: "fallback"` removes it.
    pub fallback: Option<String>,

    /// Override of the derived `/v1/models` capability facts: a JSON object,
    /// or a JSON string containing one (MCP arguments are flat scalars).
    /// `null`/empty clears (or name it in `clear`).
    pub capabilities_override: Option<Value>,

    /// Ladder rungs above the base: the row's own `gguf_path` + `ctx_size`
    /// stay rung 1. Empty (or `clear: "ladder"`) means "not a ladder".
    /// Validated at save time.
    pub ladder: Option<Vec<crate::ladder::Rung>>,

    /// Field names to reset to unset, comma- or space-separated.
    pub clear: Option<String>,
}

/// Apply the sparse overlay onto a [`crate::config::LlamaParams`], then honour
/// `clear`. Split out so `create` and `update` cannot drift apart — `create`
/// overlays onto a default, `update` onto the stored row.
fn overlay_params(
    base: &mut crate::config::LlamaParams,
    p: &LocalModelPatch,
) -> Result<(), String> {
    /// Paths are stored relative to the models dir; the preset renderer owns
    /// the `/models/` prefix, so strip one a caller pasted in.
    fn rel(v: Option<String>) -> Option<String> {
        v.map(|s| {
            s.trim_start_matches("/models/")
                .trim_start_matches('/')
                .to_string()
        })
    }

    macro_rules! set {
        ($field:ident) => {
            if p.$field.is_some() {
                base.$field = p.$field.clone();
            }
        };
    }
    set!(ctx_size);
    set!(n_predict);
    set!(n_gpu_layers);
    if p.n_cpu_moe.is_some_and(|n| n < 0) {
        return Err(
            "n_cpu_moe must be 0 or more: the number of layers whose experts stay in host memory"
                .into(),
        );
    }
    set!(n_cpu_moe);
    if p.moe_cache_mib.is_some_and(|n| n < 0) {
        return Err("moe_cache_mib must be 0 or more: a cache size in MiB".into());
    }
    set!(moe_cache_mib);
    set!(threads);
    set!(batch_size);
    set!(ubatch_size);
    set!(parallel);
    set!(kv_unified);
    set!(kv_unified_per_slot);
    set!(flash_attn);
    set!(cache_type_k);
    set!(cache_type_v);
    set!(cache_ram);
    set!(fit);
    set!(fit_ctx);
    set!(reasoning_format);
    set!(reasoning);
    set!(reasoning_budget);
    set!(reasoning_effort);
    set!(temp);
    set!(top_p);
    set!(top_k);
    set!(min_p);
    set!(repeat_penalty);
    set!(presence_penalty);
    set!(seed);
    set!(spec_type);
    set!(spec_draft_n_max);
    set!(spec_draft_n_min);
    set!(spec_draft_ngl);

    if let Some(v) = opt(&p.mmproj_path) {
        base.mmproj_path = rel(Some(v));
    }
    if let Some(v) = opt(&p.draft_gguf_path) {
        base.draft_gguf_path = rel(Some(v));
    }
    if let Some(v) = opt(&p.chat_template_file) {
        base.chat_template_file = rel(Some(v));
    }
    if let Some(j) = p.jinja {
        base.jinja = j;
    }
    if let Some(n) = p.no_mmproj {
        base.no_mmproj = n;
    }
    if let Some(v) = p.reasoning_preserve {
        base.reasoning_preserve = Some(v);
    }
    if let Some(v) = p.chat_template_kwargs.as_deref() {
        base.chat_template_kwargs = crate::config::parse_chat_template_kwargs(v)?;
    }
    // Before `clear`, so clearing an effort level cannot be undone by a
    // `reasoning_effort` still sitting in the freeform kwargs.
    base.fold_reasoning_effort();

    for name in clear_names(p.clear.as_deref()) {
        match name {
            "mmproj_path" => base.mmproj_path = None,
            "no_mmproj" => base.no_mmproj = false,
            "ctx_size" => base.ctx_size = None,
            "n_predict" => base.n_predict = None,
            "n_gpu_layers" => base.n_gpu_layers = None,
            "n_cpu_moe" => base.n_cpu_moe = None,
            "moe_cache_mib" => base.moe_cache_mib = None,
            "threads" => base.threads = None,
            "batch_size" => base.batch_size = None,
            "ubatch_size" => base.ubatch_size = None,
            "parallel" => base.parallel = None,
            "kv_unified" => base.kv_unified = None,
            "kv_unified_per_slot" => base.kv_unified_per_slot = None,
            "flash_attn" => base.flash_attn = None,
            "cache_type_k" => base.cache_type_k = None,
            "cache_type_v" => base.cache_type_v = None,
            "cache_ram" => base.cache_ram = None,
            "fit" => base.fit = None,
            "fit_ctx" => base.fit_ctx = None,
            "jinja" => base.jinja = false,
            "chat_template_file" => base.chat_template_file = None,
            "reasoning_format" => base.reasoning_format = None,
            "reasoning" => base.reasoning = None,
            "reasoning_budget" => base.reasoning_budget = None,
            "reasoning_preserve" => base.reasoning_preserve = None,
            "reasoning_effort" => base.reasoning_effort = None,
            "chat_template_kwargs" => base.chat_template_kwargs.clear(),
            "temp" => base.temp = None,
            "top_p" => base.top_p = None,
            "top_k" => base.top_k = None,
            "min_p" => base.min_p = None,
            "repeat_penalty" => base.repeat_penalty = None,
            "presence_penalty" => base.presence_penalty = None,
            "seed" => base.seed = None,
            "draft_gguf_path" => base.draft_gguf_path = None,
            "spec_type" => base.spec_type = None,
            "spec_draft_n_max" => base.spec_draft_n_max = None,
            "spec_draft_n_min" => base.spec_draft_n_min = None,
            "spec_draft_ngl" => base.spec_draft_ngl = None,
            "extra_args" => {} // handled by the caller, which owns `args`
            "image" | "extra_run_args" => {} // handled by the caller, same reason
            "fallback" => {}   // handled by the caller, same reason
            "capabilities_override" => {} // handled by the caller, same reason
            "ladder" => {}     // handled by the caller, same reason
            other => return Err(format!("clear: '{other}' is not a clearable field name")),
        }
    }
    Ok(())
}

/// The image a chat model actually runs and the `podman run` args it runs
/// with: its own overrides, else the chat class defaults (§3.1). What
/// `config_warnings` validates against — with per-model image overrides there
/// is no single flag vocabulary to check against any more (§3.6) — and what
/// the `--help` read needs to run that image's binary at all (see
/// [`crate::runtime::registry::Registry::help_text`]).
fn effective_chat_image(
    state: &SharedState,
    override_image: &Option<String>,
    override_run_args: &Option<Vec<String>>,
) -> (String, Vec<String>) {
    let snap = state.snapshot();
    let router = &snap.settings.router;
    (
        override_image
            .clone()
            .unwrap_or_else(|| router.image.clone()),
        override_run_args
            .clone()
            .unwrap_or_else(|| router.extra_run_args.clone()),
    )
}

/// The full stored record for one local model, including every llama-server
/// parameter and the exact preset section it renders to.
///
/// The list view deliberately returns only names and paths, which left a
/// caller unable to read back what it had just written — it had to open the
/// generated INI on disk. This closes that loop.
pub async fn local_model_get(
    state: &SharedState,
    id: Option<i64>,
    model_id: Option<&str>,
    target: Option<&str>,
) -> Result<dto::LocalModelRead, String> {
    let class = parse_class_target(target)?;
    let all = store::list_local_models(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let model_id = model_id.map(str::trim).filter(|s| !s.is_empty());
    // A row id is per table, so it only selects within the class named (chat
    // when none is); a model id is looked up in chat first, then aux, then
    // image, then audio, so an embedding model, a diffusion pipeline or an
    // audio.cpp model is found without the caller knowing which table holds
    // it. `target=` narrows the lookup to that one table.
    let elsewhere = matches!(
        class,
        Some(Class::Aux) | Some(Class::Image) | Some(Class::Audio)
    );
    let found = if elsewhere {
        None
    } else {
        match (id, model_id) {
            (Some(id), _) => all.iter().find(|m| m.id == id),
            (None, Some(mid)) => all.iter().find(|m| m.model_id == mid),
            (None, None) => return Err("pass id or model_id".into()),
        }
    };
    let Some(m) = found else {
        if matches!(class, None | Some(Class::Aux)) {
            if let Some(v) = aux_model_get(state, id, model_id).await? {
                return Ok(dto::LocalModelRead::Aux(v));
            }
        }
        let snap = state.snapshot();
        if matches!(class, None | Some(Class::Image)) {
            if let Some(v) = image_model_get(&snap, state, id, model_id).await? {
                return Ok(dto::LocalModelRead::Image(v));
            }
        }
        if matches!(class, None | Some(Class::Audio)) {
            if let Some(v) = audio_model_get(state, id, model_id).await? {
                return Ok(dto::LocalModelRead::Audio(v));
            }
        }
        let mut known: Vec<String> = all.iter().map(|m| m.model_id.clone()).collect();
        if class != Some(Class::Chat) {
            known.extend(
                snap.aux_models
                    .iter()
                    .map(|m| format!("{} (aux)", m.model_id)),
            );
            known.extend(
                snap.image_models
                    .iter()
                    .map(|m| format!("{} (image)", m.model_id)),
            );
            known.extend(
                snap.audio_models
                    .iter()
                    .map(|m| format!("{} (audio)", m.model_id)),
            );
        }
        return Err(format!(
            "no such local model; configured ids: {}",
            if known.is_empty() {
                "(none)".into()
            } else {
                known.join(", ")
            }
        ));
    };

    let models_dir = state.snapshot().settings.router.models_dir.clone();
    let exists = |rel: &Option<String>| -> Option<bool> {
        rel.as_deref()
            .filter(|r| !r.is_empty())
            .map(|r| std::path::Path::new(&models_dir).join(r).is_file())
    };

    // Provenance: whether this GGUF came from the HF downloader or was put
    // there by hand. It decides whether `lmgw__hf_set action=check_updates`
    // can ever see a new revision of it, so it is worth stating rather than
    // leaving the caller to infer from the path shape.
    let downloads = store::list_hf_models(&state.db).await.unwrap_or_default();
    let origin = downloads
        .iter()
        .find(|d| d.dest_path == m.gguf_path)
        .map(|d| dto::DownloadedFrom {
            repo: d.repo.clone(),
            file: d.file.clone(),
            download_id: d.id,
        });
    // The same checks `apply` runs, per model. Without them a row whose
    // spec_type its GGUF cannot support reads as perfectly healthy — every
    // path present, a clean preset — right up until it fails to load. A read
    // tool that shows a config without saying whether it can work is the gap
    // that made those failures invisible in the first place.
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
        fallback: m.fallback.clone(),
        capabilities_override: m.capabilities_override.clone(),
        ladder: m.ladder.clone(),
    };
    let ModelChecks {
        mut problems,
        advisories,
    } = model_warnings(&models_dir, &as_new).await;
    for (what, rel) in [
        ("gguf_path", Some(&m.gguf_path)),
        ("mmproj_path", m.params.mmproj_path.as_ref()),
        ("draft_gguf_path", m.params.draft_gguf_path.as_ref()),
    ] {
        if let Some(rel) = rel.filter(|r| !r.is_empty()) {
            if !models_dir.trim().is_empty()
                && !std::path::Path::new(&models_dir).join(rel).is_file()
            {
                problems.push(format!("{what} '{rel}' is missing from the models dir"));
            }
        }
    }
    let (image, run_args) = effective_chat_image(state, &m.image, &m.extra_run_args);
    problems.extend(config_warnings(state, &image, &run_args, &m.params, &m.args).await);

    // The exact command line this model renders to (§8: local_edit's
    // "Rendered preset" panel became "Rendered command line"). Rendered as if
    // enabled, so a disabled entry still shows what enabling it would
    // produce, and with a placeholder port because the real one is allocated
    // per start (§3.5).
    // Where VRAM admission fits it in llama.cpp's `--fit` place, the
    // placement is rendered too, and said: the running container's, else
    // what a start on this card as last read gets (`vram::partial::llama`).
    let (fitted, fit_said) = super::local_model_fit::fitted(state, &m.model_id).await;
    let mut advisories = advisories;
    advisories.extend(fit_said);
    let command_line = command_line_preview_at(state, Class::Chat, &m.model_id, 0, fitted);
    let rungs_detail = ladder_rungs_detail(state, &models_dir, m).await;

    let licence = crate::modelinfo::licence::of_file(state, &models_dir, &m.gguf_path).await;
    Ok(dto::LocalModelRead::Chat(dto::LocalModelDetail {
        id: m.id,
        model_id: m.model_id.clone(),
        gguf_path: m.gguf_path.clone(),
        gguf_present: std::path::Path::new(&models_dir)
            .join(&m.gguf_path)
            .is_file(),
        licence,
        source: if origin.is_some() { "hf" } else { "manual" }.to_string(),
        downloaded_from: origin,
        mmproj_present: exists(&m.params.mmproj_path),
        draft_present: exists(&m.params.draft_gguf_path),
        params: mirror(&m.params)?,
        extra_args: m.args.clone(),
        idle_seconds: m.idle_seconds,
        enabled: m.enabled,
        public: m.public,
        // Per-model container overrides (§3.1); `null` means "inherit the
        // chat class settings".
        image: m.image.clone(),
        extra_run_args: m.extra_run_args.clone(),
        warm_start: m.warm_start,
        // The row's one fallback (openai egress design §14.2); `null` = none.
        fallback: m.fallback.clone(),
        // Owner override of the derived `/v1/models` capability facts
        // (model-capabilities design §7); `null` means none is set.
        capabilities_override: m.capabilities_override.clone(),
        // Ladder rungs above the base (ladder design §4.1, §6): empty means
        // "not a ladder", the same convention `LocalModel::is_ladder` reads.
        ladder: m.ladder.iter().map(dto::Rung::from).collect(),
        // What llama-server will actually be told, which is the thing a caller
        // is really trying to verify.
        command_line,
        // Every rung's own command line, per-slot context and switchover
        // (ladder design §6, §8 WP6); `null` on a row without a ladder.
        rungs: rungs_detail,
        // Empty means the static checks found nothing, not that the model
        // loads — only lmgw__local_model_test proves that.
        problems,
        // Configuration that loads and serves but misbehaves under some
        // input (a large image, say). Kept apart from `problems` because the
        // model does start.
        advisories,
        // The note of a context fitted to the card (find-a-model D26), so
        // the editor reads it with the row.
        fit_note: crate::modelinfo::refit::note_of(state, Class::Chat, m.id).await,
    }))
}

/// The `podman run …` command line one model renders to, for display (§8).
///
/// A *preview*, and honest about it: the host port is allocated per start
/// (§3.5), so a placeholder stands in for it. Everything else is what a start
/// would really execute.
///
/// Chat, aux and image. Rendering an audio model's spec writes its
/// `server.json` (§3.6 — the config dir *is* audio's configuration), and a
/// read path must not have that side effect; audio models say so instead. The
/// image arm renders through [`ModelRuntime::preview_spec`] for the same
/// reason: a start creates the two directories sd-server's capabilities route
/// throws without (image-generation design §12.2), but a *preview* that did
/// would answer "could not render the command line: Read-only file system"
/// to a question about flags.
pub(super) fn command_line_preview(state: &SharedState, class: Class, model_id: &str) -> String {
    command_line_preview_at(state, class, model_id, 0, None)
}

/// [`command_line_preview`], for a chosen rung (ladder design §6, §8 WP6) —
/// `rung` is ignored for every class but chat, exactly like
/// [`model_runtime_at`] itself; `command_line_preview` is just this at rung 0
/// (the base), which renders byte-identical to before this parameter existed.
fn command_line_preview_at(
    state: &SharedState,
    class: Class,
    model_id: &str,
    rung: usize,
    fitted: Option<argv::Fitted>,
) -> String {
    if class == Class::Audio {
        return "audio.cpp is configured by the server.json mounted at /config, not by \
                command-line flags"
            .to_string();
    }
    /// Stands in for the port `podman run` would really publish. Any value
    /// would be a lie; this one is at least obviously not a real allocation.
    const PLACEHOLDER_PORT: u16 = 0;
    let snap = state.snapshot();
    let Some(mut rt) = model_runtime_at(&snap, class, model_id, rung) else {
        return String::new();
    };
    // Rendered as if enabled, so a disabled entry still shows what enabling
    // it would produce.
    rt.enabled = true;
    let models_dir = match class {
        Class::Chat => &snap.settings.router.models_dir,
        Class::Aux => &snap.settings.aux_router.models_dir,
        Class::Audio => &snap.settings.audio.models_dir,
        Class::Image => &snap.settings.image.models_dir,
    };
    match rt.preview_spec(
        &snap.settings.container_prefix,
        PLACEHOLDER_PORT,
        models_dir,
        &state.data_dir,
    ) {
        Ok(mut spec) => {
            // With the option a start would pass for this row: lmgw's own
            // CDI spec dir for a GPU row (machine-check design §3.6.4), none
            // while Podman's default dirs are used (D26). Showing it writes
            // nothing: the dir holds a spec only once a GPU start or probe of
            // this instance wrote it, and under `$XDG_RUNTIME_DIR` only until
            // the login session ends.
            let globals = crate::runtime::cdi::podman_globals(state, &spec.extra_run_args);
            spec.set_globals(globals);
            spec.fitted = fitted;
            argv::command_line(&spec)
        }
        Err(e) => format!("could not render the command line: {e}"),
    }
}

/// Every rung's command line, per-slot context and switchover (ladder design
/// §4.2, §6, §8 WP6), base included, for `lmgw__local_model_get`'s reader —
/// the base's own numbers this way match how a higher rung's are shown,
/// rather than only being visible through the row's top-level `command_line`.
/// `Value::Null` when `m` is not a ladder ([`LocalModel::is_ladder`]), the
/// same "empty means not a ladder" convention `m.ladder` itself reads.
///
/// `per_slot_ctx`/`switchover` are capped at each rung's own GGUF's trained
/// context, the same [`crate::ladder::slot_ctx`] the gate judges and
/// publishes a running rung on (review third pass, T4) — showing the
/// configured (uncapped) number here would disagree with what a request
/// against this rung is actually judged against for a row saved before §4.3
/// rule 3b started refusing the mismatch at save time.
async fn ladder_rungs_detail(
    state: &SharedState,
    models_dir: &str,
    m: &LocalModel,
) -> Option<Vec<dto::RungDetail>> {
    if !m.is_ladder() {
        return None;
    }
    // `None` only for a row saved before §4.3 rule 3 existed, whose base
    // ctx_size cannot derive a per-slot number at all — the rungs still
    // exist, so this reports what it can (no per_slot_ctx/switchover) rather
    // than an empty array.
    let rungs = m.all_rungs().unwrap_or_default();
    let trained = crate::gate::ladder::trained_contexts(state, models_dir, m).await;
    let n_predict = m.params.n_predict.filter(|&n| n > 0);
    Some(
        rungs
            .iter()
            .map(|r| {
                let per_slot_ctx = m
                    .per_slot_ctx(r.index)
                    .map(|c| crate::ladder::slot_ctx(c, trained.get(r.index).copied().flatten()));
                let switchover = per_slot_ctx.zip(n_predict).map(|(c, n)| c - n);
                dto::RungDetail {
                    rung: r.index + 1,
                    of: m.top_rung() + 1,
                    gguf_path: r.gguf_path.to_string(),
                    ctx_size: r.ctx_size,
                    per_slot_ctx,
                    switchover,
                    command_line: command_line_preview_at(
                        state,
                        Class::Chat,
                        &m.model_id,
                        r.index,
                        None,
                    ),
                }
            })
            .collect(),
    )
}

/// A core row type as its API mirror, through the shared JSON shape. The
/// mirror is what the document describes, so it has to be the row whole:
/// the row is read into the mirror and written back, and a key the mirror
/// drops (a field added on the core side only) is an error rather than a
/// silent omission. `config_mirrors_match` in the tests compares the two
/// types' key sets ahead of any row. Every row of `GET /api/models/full` goes
/// through it too (`models_full_wire` sets every field of one of each).
pub(crate) fn mirror<T: serde::Serialize, U: serde::Serialize + serde::de::DeserializeOwned>(
    row: &T,
) -> Result<U, String> {
    let err = |e: serde_json::Error| format!("reading a row as its API shape: {e}");
    let written = serde_json::to_value(row).map_err(err)?;
    let mirrored: U = serde_json::from_value(written.clone()).map_err(err)?;
    if serde_json::to_value(&mirrored).map_err(err)? != written {
        return Err(
            "reading a row as its API shape: the API type drops or invents a field \
                    the row has"
                .into(),
        );
    }
    Ok(mirrored)
}

/// The API shape of a ladder rung. Destructured without `..`: a field added
/// to the row's rung stops compiling here until the API type has it too.
impl From<&crate::ladder::Rung> for dto::Rung {
    fn from(r: &crate::ladder::Rung) -> Self {
        let crate::ladder::Rung {
            gguf_path,
            ctx_size,
        } = r.clone();
        Self {
            gguf_path,
            ctx_size,
        }
    }
}

/// Static health of every configured local model, or of one named model.
///
/// The same checks `lmgw__container action=apply` reports, available without
/// mutating anything. Answering "which of my models are broken?" previously
/// meant either calling apply — which hot-reloads a live inference container
/// to answer a read-only question — or reading each model back one at a time
/// and eyeballing it.
pub async fn local_model_check(
    state: &SharedState,
    model_id: Option<&str>,
    target: Option<&str>,
) -> Result<dto::LocalModelCheck, String> {
    let class = parse_class_target(target)?;
    let all = store::list_local_models(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let snap = state.snapshot();
    let models_dir = snap.settings.router.models_dir.clone();
    let wanted = model_id.map(str::trim).filter(|s| !s.is_empty());
    if let Some(mid) = wanted {
        let wanted_class = |c: Class| class.is_none_or(|k| k == c);
        let in_chat = wanted_class(Class::Chat) && all.iter().any(|m| m.model_id == mid);
        let in_aux = wanted_class(Class::Aux) && snap.aux_models.iter().any(|m| m.model_id == mid);
        let in_image =
            wanted_class(Class::Image) && snap.image_models.iter().any(|m| m.model_id == mid);
        if !in_chat && !in_aux && !in_image {
            let mut known: Vec<String> = all.iter().map(|m| m.model_id.clone()).collect();
            known.extend(
                snap.aux_models
                    .iter()
                    .map(|m| format!("{} (aux)", m.model_id)),
            );
            known.extend(
                snap.image_models
                    .iter()
                    .map(|m| format!("{} (image)", m.model_id)),
            );
            return Err(format!(
                "no local model '{mid}'; configured: {}",
                known.join(", ")
            ));
        }
    }

    let mut checked = 0usize;
    let mut results: Vec<dto::ModelCheck> = Vec::new();
    for m in all
        .iter()
        .filter(|_| class.is_none_or(|c| c == Class::Chat))
        .filter(|m| wanted.is_none_or(|w| m.model_id == w))
    {
        checked += 1;
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
            fallback: m.fallback.clone(),
            capabilities_override: m.capabilities_override.clone(),
            ladder: m.ladder.clone(),
        };
        let ModelChecks {
            mut problems,
            advisories,
        } = model_warnings(&models_dir, &as_new).await;
        for (what, rel) in [
            ("gguf_path", Some(&m.gguf_path)),
            ("mmproj_path", m.params.mmproj_path.as_ref()),
            ("draft_gguf_path", m.params.draft_gguf_path.as_ref()),
        ] {
            if let Some(rel) = rel.filter(|r| !r.is_empty()) {
                if !models_dir.trim().is_empty()
                    && !std::path::Path::new(&models_dir).join(rel).is_file()
                {
                    problems.push(format!("{what} '{rel}' is missing from the models dir"));
                }
            }
        }
        // Only problems decide `ok`: an advisory describes a model that
        // starts, and counting it as broken would say the opposite.
        if !problems.is_empty() || !advisories.is_empty() || wanted.is_some() {
            results.push(dto::ModelCheck::Chat {
                model_id: m.model_id.clone(),
                enabled: m.enabled,
                ok: problems.is_empty(),
                problems,
                advisories,
            });
        }
    }

    // The aux class (embedders + rerankers), the same way: a missing file, a
    // projector or drafter passed as weights, and the kind/pooling rules the
    // argv renderer would otherwise only enforce silently.
    if class.is_none_or(|c| c == Class::Aux) {
        let aux_dir = snap.settings.aux_router.models_dir.clone();
        for m in snap
            .aux_models
            .iter()
            .filter(|m| wanted.is_none_or(|w| m.model_id == w))
        {
            checked += 1;
            let problems = aux_model_problems(&aux_dir, m).await;
            if !problems.is_empty() || wanted.is_some() {
                results.push(dto::ModelCheck::Aux {
                    model_id: m.model_id.clone(),
                    kind: m.kind.as_str().to_string(),
                    enabled: m.enabled,
                    ok: problems.is_empty(),
                    problems,
                });
            }
        }
    }

    // The image class, through the same pre-flight group apply runs
    // (image-generation design §4): a files value that is not on disk, a key
    // outside sd-server's vocabulary, and a row that names neither of the two
    // ways to load a pipeline.
    if class.is_none_or(|c| c == Class::Image) {
        let image_dir = snap.settings.image.models_dir.clone();
        for m in snap
            .image_models
            .iter()
            .filter(|m| wanted.is_none_or(|w| m.model_id == w))
        {
            checked += 1;
            let problems = image_model_problems(&image_dir, m);
            if !problems.is_empty() || wanted.is_some() {
                results.push(dto::ModelCheck::Image {
                    model_id: m.model_id.clone(),
                    modes: m.modes(),
                    edit: m.edit,
                    enabled: m.enabled,
                    ok: problems.is_empty(),
                    problems,
                });
            }
        }
    }

    Ok(dto::LocalModelCheck {
        checked,
        broken: results.iter().filter(|r| !r.ok()).count(),
        with_advisories: results
            .iter()
            .filter(
                |r| matches!(r, dto::ModelCheck::Chat { advisories, .. } if !advisories.is_empty()),
            )
            .count(),
        models: results,
        note: "Static checks only — paths, file roles, speculative-decoding \
                 consistency, the kind flag rules of aux models (embed, rerank, \
                 decision), and for \
                 image models the files/args key vocabulary. A model with no \
                 problems here can still fail to load; lmgw__local_model_test is \
                 the only proof. 'advisories' are not problems: the model starts, \
                 but misbehaves under some input (a chat model's batch sizes too \
                 small for its projector's largest image, for one, or an aux GGUF on \
                 a chat row that still loads and serves), so they never \
                 set ok=false or count as broken. Models with nothing to report are \
                 omitted unless you asked for one by name."
            .to_string(),
    })
}

/// First free `<base>-copy`, `<base>-copy-2`, … — `model_id` is UNIQUE, so a
/// clone needs an id no existing model already holds.
fn unique_copy_id(base: &str, existing: &[String]) -> String {
    let taken = |id: &str| existing.iter().any(|e| e == id);
    let first = format!("{base}-copy");
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| format!("{base}-copy-{n}"))
        .find(|id| !taken(id))
        .expect("infinite range always yields a free id")
}

pub async fn local_model_set(
    state: &SharedState,
    p: LocalModelPatch,
) -> Result<dto::LocalModelSaved, String> {
    // From the rows the checks read to the commit (§14.2).
    let routing = state.routing_write().await;
    local_model_set_holding(state, routing, p).await
}

/// [`local_model_set`] for a caller that already holds the routing-write
/// lock (`routing`), so what it read under the lock before stays true
/// through the commit: Get it settles a new row's id under it (D28,
/// `jobs::model_get::set_up`). Let go once the snapshot is published.
pub async fn local_model_set_holding(
    state: &SharedState,
    routing: tokio::sync::OwnedMutexGuard<()>,
    p: LocalModelPatch,
) -> Result<dto::LocalModelSaved, String> {
    let all = store::list_local_models(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let find = || -> Result<crate::config::LocalModel, String> {
        if let Some(id) = p.id {
            return all
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| format!("no local model with id {id}"));
        }
        let mid = opt(&p.model_id).ok_or("this action requires id or model_id")?;
        all.iter()
            .find(|m| m.model_id == mid)
            .cloned()
            .ok_or_else(|| format!("no local model with model_id '{mid}'"))
    };

    let snap = state.snapshot();
    // A path that resolves to nothing is the single most common way to
    // configure an unloadable model, and the caller usually has no shell to
    // check with. Name the miss, and point at the tools that fix it.
    let models_dir = snap.settings.router.models_dir.clone();
    let check_path = |rel: Option<&str>, what: &str| -> Result<(), String> {
        let Some(rel) = rel.filter(|r| !r.is_empty()) else {
            return Ok(());
        };
        // The models dir is the boundary for every path field. `..` is
        // refused outright rather than normalized — matching
        // `modelinfo::resolve`, which guards the same boundary — so a caller
        // at self_admin=full cannot use these fields as a host-filesystem
        // existence oracle or point --chat-template-file at an arbitrary file.
        if rel.split('/').any(|c| c == ".." || c == ".") {
            return Err(format!(
                "{what} '{rel}' must be a plain path under the models directory"
            ));
        }
        // With no models dir configured there is nothing to resolve against;
        // refusing here would block configuration on a check we cannot make.
        if models_dir.trim().is_empty() {
            return Ok(());
        }
        if std::path::Path::new(&models_dir).join(rel).is_file() {
            return Ok(());
        }
        Err(format!(
            "{what} '{rel}' is not under the models directory ({models_dir}). \
             Use lmgw__gguf_files to see what is there, or lmgw__hf_repo / \
             lmgw__hf_add to download it from Hugging Face."
        ))
    };

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("model_id", p.model_id.is_some()),
                ("gguf_path", p.gguf_path.is_some()),
            ])?;
            let model_id = opt(&p.model_id).ok_or("model_id must not be empty")?;
            refuse_if_candidate_alias_name(&snap, &snap.local_public_name(&model_id))?;
            let gguf_path = opt(&p.gguf_path)
                .map(|g| {
                    g.trim_start_matches("/models/")
                        .trim_start_matches('/')
                        .to_string()
                })
                .ok_or("gguf_path must not be empty")?;
            let mut params = crate::config::LlamaParams::default();
            overlay_params(&mut params, &p)?;
            let mut args = p
                .extra_args
                .as_deref()
                .map(arg_list)
                .transpose()?
                .unwrap_or_default();
            // A freeform `--kv-unified`/`-kvu` (or any other promoted flag)
            // must not bypass the refusal below — fold it into `params`
            // first, exactly as this row's next load would
            // (`store::list_local_models` → `LocalModel::hoist_promoted_args`,
            // review finding 3).
            crate::config::hoist_promoted_args_into(&mut params, &mut args);
            validate_kv_unified(&params)?;
            check_path(Some(&gguf_path), "gguf_path")?;
            check_path(params.mmproj_path.as_deref(), "mmproj_path")?;
            check_path(params.draft_gguf_path.as_deref(), "draft_gguf_path")?;
            // `clear: "ladder"` and an absent `ladder` both mean "not a
            // ladder" on create — there is no prior row to preserve one from.
            let ladder = if clear_has(p.clear.as_deref(), "ladder") {
                Vec::new()
            } else {
                p.ladder.clone().unwrap_or_default()
            };
            for (i, r) in ladder.iter().enumerate() {
                check_path(
                    Some(&r.gguf_path),
                    &format!("ladder rung {} gguf_path", i + 2),
                )?;
            }
            validate_ladder(&models_dir, &model_id, &gguf_path, &params, &args, &ladder).await?;
            let fallback = fallback::patched(p.clear.as_deref(), &p.fallback, None)?;
            let mut fallback_notes = Vec::new();
            if let Some(f) = &fallback {
                let owner = fallback::Owner::typed(
                    None,
                    snap.local_public_name(&model_id),
                    fallback::ModelType::chat(),
                );
                fallback_notes = fallback::validate(state, &owner, f).await?;
            }
            let capabilities_override = match p.capabilities_override.as_ref() {
                Some(v) => parse_capabilities_override(v)?,
                None => None,
            };
            let new = NewLocalModel {
                model_id: model_id.clone(),
                gguf_path,
                params,
                args,
                idle_seconds: p.idle_seconds.unwrap_or(300),
                enabled: p.enabled.unwrap_or(true),
                public: p.public.unwrap_or(true),
                image: opt(&p.image),
                // A named clear wins over the value on create as on update,
                // and text that holds no args is no override either.
                extra_run_args: match &p.extra_run_args {
                    _ if clear_has(p.clear.as_deref(), "extra_run_args") => None,
                    Some(list) => run_args_override(Some(arg_list(list)?)),
                    None => None,
                },
                warm_start: p.warm_start.unwrap_or(false),
                fallback,
                capabilities_override,
                ladder,
            };
            let (image, run_args) = effective_chat_image(state, &new.image, &new.extra_run_args);
            let mut warnings =
                config_warnings(state, &image, &run_args, &new.params, &new.args).await;
            let checks = model_warnings(&models_dir, &new).await;
            warnings.extend(checks.problems);
            let mut advisories = checks.advisories;
            advisories.extend(ladder_footprint_advisories(state, &models_dir, &new).await);
            // A fallback that holds the new name reaches this row from now
            // on: checked in a dry run (§14.2, `fallback::referrers`).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let dry = store::insert_local_model_in(&mut tx, &new)
                .await
                .map_err(|e| e.to_string())?;
            let row = (store::FallbackTable::LocalModels, dry);
            fallback::dry_run(
                state,
                &snap,
                tx,
                &[fallback::Touch::Row(row.0, dry)],
                Some(row),
            )
            .await?;
            // The write and the reload are a task of their own (*changed
            // 2026-10-24*, see [`crate::detached::detached`]): a dropped
            // request would leave the snapshot without the new model.
            let st = state.clone();
            let public_name = snap.local_public_name(&model_id);
            let (id, fallback_notes) = detached("creating the local model", async move {
                let id = store::insert_local_model(&st.db, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut notes = fallback_notes;
                if let Some(f) = &new.fallback {
                    notes.extend(fallback::facet_notes(&st, &public_name, f).await);
                }
                Ok::<_, String>((id, notes))
            })
            .await
            .map_err(|r| r.message)??;
            Ok(dto::LocalModelSaved {
                ok: true,
                id,
                warnings: Some(warnings),
                // Saved and loadable, but misbehaving under some input; kept
                // apart from `warnings`, which are reasons it may not load.
                advisories: Some(advisories),
                message: format!(
                    "local model '{model_id}' created — run lmgw__container \
                     model={model_id} action=start to load it into llama-server, \
                     then lmgw__local_model_test to confirm it loads"
                ),
                fallback_notes,
                ..Default::default()
            })
        }
        "update" | "enable" | "disable" => {
            let cur = find()?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let mut params = cur.params.clone();
            overlay_params(&mut params, &p)?;
            let mut args = match p.extra_args.as_deref() {
                Some(list) => arg_list(list)?,
                None if clear_has(p.clear.as_deref(), "extra_args") => vec![],
                None => cur.args,
            };
            // Same reason as the create arm above: fold any promoted flag out
            // of `args` before judging `params`, so a short alias in
            // freeform text cannot bypass this refusal (review finding 3).
            // The fold itself always runs, disabled row included — only the
            // refusal below is gated on the *resulting* `enabled` state
            // (review finding X3), the same exemption `validate_ladder`
            // already gets a few lines down: turning a row off is
            // maintenance on a row the owner already owns and wants off, not a
            // chance to fix a kv_unified/parallel/n_predict combination they
            // never typed into a typed field themself — a row saved before
            // this validation existed (freeform `--kv-unified`/`-kvu`
            // hoisted on load) must still be disable-able.
            crate::config::hoist_promoted_args_into(&mut params, &mut args);
            if enabled {
                validate_kv_unified(&params)?;
            }
            let gguf_path = opt(&p.gguf_path)
                .map(|g| {
                    g.trim_start_matches("/models/")
                        .trim_start_matches('/')
                        .to_string()
                })
                .unwrap_or(cur.gguf_path);
            check_path(Some(&gguf_path), "gguf_path")?;
            check_path(params.mmproj_path.as_deref(), "mmproj_path")?;
            check_path(params.draft_gguf_path.as_deref(), "draft_gguf_path")?;
            let ladder = if clear_has(p.clear.as_deref(), "ladder") {
                Vec::new()
            } else {
                p.ladder.clone().unwrap_or_else(|| cur.ladder.clone())
            };
            // Turning a row off is a maintenance action on a row the owner
            // already owns and wants off, not a chance to fix its ladder —
            // refusing it because a rung file went missing or unreadable
            // after save would leave them unable to turn a broken model off
            // (review finding 10). The exemption is on the *resulting*
            // `enabled` state, not the action name (second-pass review
            // finding S3): the editor's Save always sends `action:
            // "update"`, `enabled: false` included, when the owner just
            // unticks the checkbox, so checking only for `action ==
            // "disable"` left that path — the one an owner actually uses —
            // still refusing. A stale rung path surfaces instead as a
            // problem in `chat_model_problems` (`lmgw__local_model_check`).
            // Every save that leaves (or makes) the row enabled still
            // validates: `enable` always does, since `enabled` is forced
            // `true` above, and a plain `update` that leaves it enabled has
            // no missing-rung amnesty to give.
            if enabled {
                for (i, r) in ladder.iter().enumerate() {
                    check_path(
                        Some(&r.gguf_path),
                        &format!("ladder rung {} gguf_path", i + 2),
                    )?;
                }
                validate_ladder(
                    &models_dir,
                    &cur.model_id,
                    &gguf_path,
                    &params,
                    &args,
                    &ladder,
                )
                .await?;
            }
            let image = if clear_has(p.clear.as_deref(), "image") {
                None
            } else {
                opt(&p.image).or(cur.image)
            };
            let extra_run_args = if clear_has(p.clear.as_deref(), "extra_run_args") {
                None
            } else {
                match &p.extra_run_args {
                    Some(list) => Some(arg_list(list)?),
                    None => cur.extra_run_args,
                }
            };
            let extra_run_args = run_args_override(extra_run_args);
            let fallback =
                fallback::patched(p.clear.as_deref(), &p.fallback, cur.fallback.clone())?;
            let capabilities_override = if clear_has(p.clear.as_deref(), "capabilities_override") {
                None
            } else {
                match p.capabilities_override.as_ref() {
                    Some(v) => parse_capabilities_override(v)?,
                    None => cur.capabilities_override.clone(),
                }
            };
            let new_model_id = match (p.id, opt(&p.model_id)) {
                (Some(_), Some(m)) => m,
                _ => cur.model_id.clone(),
            };
            // Same reasoning as `model_set`'s own rename guard (review
            // finding X4): only check when the id is actually changing, not
            // on every save of a row a later candidate alias happens to
            // shadow.
            if new_model_id != cur.model_id {
                refuse_if_candidate_alias_name(&snap, &snap.local_public_name(&new_model_id))?;
            }
            let key = HopKey::Local(Class::Chat, cur.model_id.clone());
            let (old_name, new_name) = (
                snap.local_public_name(&cur.model_id),
                snap.local_public_name(&new_model_id),
            );
            let mut fallback_notes = Vec::new();
            let fallback_changed = fallback != cur.fallback;
            if let (Some(f), true) = (&fallback, fallback_changed) {
                let owner = fallback::Owner::typed(
                    Some(key.clone()),
                    new_name.clone(),
                    fallback::ModelType::chat(),
                )
                .renamed_from(Some(old_name.clone()));
                fallback_notes = fallback::validate(state, &owner, f).await?;
            }
            // A rename never breaks a chain (decision 27): every other
            // fallback naming this row follows it, in the same transaction.
            let rewrites = match new_name != old_name {
                true => {
                    fallback::rename_rewrites(&snap, &[(key.clone(), new_name.clone())], Some(&key))
                }
                false => fallback::Rewrites::default(),
            };
            fallback_notes.extend(rewrites.notes.iter().cloned());
            let new = NewLocalModel {
                model_id: new_model_id,
                gguf_path,
                params,
                args,
                idle_seconds: p.idle_seconds.unwrap_or(cur.idle_seconds),
                enabled,
                public: p.public.unwrap_or(cur.public),
                image,
                extra_run_args,
                warm_start: p.warm_start.unwrap_or(cur.warm_start),
                fallback,
                capabilities_override,
                ladder,
            };
            let (image, run_args) = effective_chat_image(state, &new.image, &new.extra_run_args);
            let mut warnings =
                config_warnings(state, &image, &run_args, &new.params, &new.args).await;
            let checks = model_warnings(&models_dir, &new).await;
            warnings.extend(checks.problems);
            let mut advisories = checks.advisories;
            advisories.extend(ladder_footprint_advisories(state, &models_dir, &new).await);
            // What a rename, an enable or a disable does to the fallbacks
            // naming this row's names, in a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            store::update_local_model_in(&mut tx, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                .await
                .map_err(|e| e.to_string())?;
            let row = (store::FallbackTable::LocalModels, cur.id);
            fallback::dry_run(
                state,
                &snap,
                tx,
                &[fallback::Touch::Row(row.0, row.1)],
                fallback_changed.then_some(row),
            )
            .await?;
            let verb = saved_verb(&p.action);
            // The write and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the old row
            // and the container on the old configuration (or, renamed or
            // disabled, running).
            let st = state.clone();
            detached("updating the local model", async move {
                let mut tx = store::begin_write(&st.db)
                    .await
                    .map_err(|e| e.to_string())?;
                store::update_local_model_in(&mut tx, cur.id, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                    .await
                    .map_err(|e| e.to_string())?;
                tx.commit().await.map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut fallback_notes = fallback_notes;
                if let (Some(f), true) = (&new.fallback, fallback_changed) {
                    fallback_notes.extend(fallback::facet_notes(&st, &new_name, f).await);
                }
                // Container hygiene (per-model-containers §3.4/§3.6). Keyed on
                // `cur.model_id`, never the new one: a rename leaves the container
                // running under the *old* name, and that is the one that has to go.
                let renamed = new.model_id != cur.model_id;
                // Whether a container is still up on the configuration before
                // this change (mid-request, or a stop that failed): the
                // dashboard's "Save and test" does not test that one.
                let mut kept_previous = false;
                let note = if renamed {
                    // A rename is a delete of the old model as far as the runtime
                    // is concerned: the row it was started from no longer exists,
                    // its container name (which carries the model id, §3.3) is one
                    // nothing will ever render again, and `--replace` therefore
                    // never collects it. A busy-refusal here would protect a model
                    // the configuration no longer admits to having, and would
                    // leave that container running forever — so it goes now,
                    // forced and removed, exactly like `drop_model`'s delete path.
                    // The requests it was serving fail visibly, which is the
                    // honest outcome of renaming a model mid-flight.
                    lifecycle::drop_model(&st, Class::Chat, &cur.model_id).await;
                    Some(format!(
                        "it was renamed from '{}', so that container was stopped and removed — the \
                         next request starts '{}' fresh",
                        cur.model_id, new.model_id
                    ))
                } else if enabled {
                    let stop =
                        lifecycle::stop_for_apply_outcome(&st, Class::Chat, &cur.model_id).await;
                    kept_previous = stop.kept_previous();
                    stop.note(&cur.model_id)
                } else {
                    lifecycle::drop_model(&st, Class::Chat, &cur.model_id).await;
                    Some(format!(
                        "'{}' is disabled — its container was stopped and removed",
                        cur.model_id
                    ))
                };
                Ok(dto::LocalModelSaved {
                    ok: true,
                    id: cur.id,
                    warnings: Some(warnings),
                    advisories: Some(advisories),
                    container_kept_previous_config: Some(kept_previous),
                    fallback_notes,
                    message: match note {
                        Some(note) => format!("local model '{}' {} — {note}", new.model_id, verb),
                        None => format!(
                            "local model '{}' {} — it is not running, so the next request \
                             starts it with the new configuration",
                            new.model_id, verb
                        ),
                    },
                })
            })
            .await
            .map_err(|r| r.message)?
        }
        "delete" => {
            let cur = find()?;
            let fallback_notes =
                fallback::delete_notes(&snap, &HopKey::Local(Class::Chat, cur.model_id.clone()));
            // The delete and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the deleted
            // row and its container running.
            let st = state.clone();
            detached("deleting the local model", async move {
                store::delete_local_model(&st.db, cur.id)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                // §3.4: the container goes now, not at the next boot —
                // `--replace` only ever collects a same-name container on a
                // *next start*, and for a deleted model there is none.
                lifecycle::drop_model(&st, Class::Chat, &cur.model_id).await;
                Ok(dto::LocalModelSaved {
                    ok: true,
                    id: cur.id,
                    message: format!(
                        "local model '{}' deleted — its container was stopped and removed",
                        cur.model_id
                    ),
                    fallback_notes,
                    ..Default::default()
                })
            })
            .await
            .map_err(|r| r.message)?
        }
        // Clone a local model under a fresh id (same GGUF, params, args, and
        // flags), so the same GGUF can back several presets (reasoning vs
        // not, different slot/context splits) without hand-copying every
        // field. Applying the preset stays an explicit step, so a duplicate
        // never silently reloads the container.
        "duplicate" => {
            let cur = find()?;
            let existing: Vec<String> = all.iter().map(|m| m.model_id.clone()).collect();
            let model_id = unique_copy_id(&cur.model_id, &existing);
            // The copy's fallback is checked as any save's (§14.2): the row's
            // own may predate the checks (migration 0095 kept a fallback of
            // another type as it was). One that would be refused is left off
            // the copy, and the answer says why.
            let mut fallback_notes = Vec::new();
            let copied_fallback = match cur.fallback.clone() {
                Some(f) => {
                    let owner = fallback::Owner::typed(
                        None,
                        snap.local_public_name(&model_id),
                        fallback::ModelType::chat(),
                    );
                    match fallback::validate(state, &owner, &f).await {
                        Ok(notes) => {
                            fallback_notes = notes;
                            Some(f)
                        }
                        Err(why) => {
                            fallback_notes.push(format!(
                                "the copy has no fallback: '{}''s own would be refused on a \
                                 save — {why}",
                                cur.model_id
                            ));
                            None
                        }
                    }
                }
                None => None,
            };
            let new = NewLocalModel {
                model_id: model_id.clone(),
                gguf_path: cur.gguf_path.clone(),
                params: cur.params.clone(),
                args: cur.args.clone(),
                idle_seconds: cur.idle_seconds,
                enabled: cur.enabled,
                public: cur.public,
                image: cur.image.clone(),
                extra_run_args: cur.extra_run_args.clone(),
                warm_start: cur.warm_start,
                fallback: copied_fallback,
                capabilities_override: cur.capabilities_override.clone(),
                ladder: cur.ladder.clone(),
            };
            // The copy's name is new: a dangling fallback holding it would
            // reach the copy (§14.2, the dry run).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let dry = store::insert_local_model_in(&mut tx, &new)
                .await
                .map_err(|e| e.to_string())?;
            let row = (store::FallbackTable::LocalModels, dry);
            fallback::dry_run(
                state,
                &snap,
                tx,
                &[fallback::Touch::Row(row.0, dry)],
                Some(row),
            )
            .await?;
            // The write and the reload are a task of their own (*changed
            // 2026-10-24*, see [`crate::detached::detached`]): a dropped
            // request would leave the snapshot without the copy.
            let st = state.clone();
            let id = detached("duplicating the local model", async move {
                let id = store::insert_local_model(&st.db, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>(id)
            })
            .await
            .map_err(|r| r.message)??;
            Ok(dto::LocalModelSaved {
                ok: true,
                id,
                message: format!(
                    "local model '{}' duplicated as '{model_id}' — adjust parameters, \
                     then start it",
                    cur.model_id
                ),
                fallback_notes,
                ..Default::default()
            })
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable|duplicate)"
        )),
    }
}
