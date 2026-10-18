//! Aux models — embedders, rerankers and decision models (§8,
//! per-model-containers §3.1, decision-models design §3)

use serde::Deserialize;

use lmgw_api_types as dto;

use crate::config::{AuxKind, AuxModel, HoldFallbackMode, Snapshot};
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::store::{self, NewAuxModel};

use super::*;

mod fallback;
mod fields;

/// The `--pooling` values llama-server accepts. Checked at write time rather
/// than left to the container, because the wrong one does not fail: it
/// produces vectors that look fine and retrieve noise.
pub const POOLING_VALUES: &[&str] = &["none", "mean", "cls", "last", "rank"];

/// A list-valued argument as every surface sends it: one entry per argument.
/// Entries are taken verbatim apart from surrounding whitespace and empty ones
/// — re-parsing them through shlex would eat the backslashes in an
/// `--override-tensor` regex. An entry that is a flag followed by a value
/// (`--cpus 2`) is refused, since it would reach the command line as one
/// argument that no program understands; `--key=value` is fine.
pub fn arg_list(v: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for t in v {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        let space = t.find(char::is_whitespace);
        let equals = t.find('=');
        if t.starts_with('-') && space.is_some_and(|s| equals.is_none_or(|e| s < e)) {
            let (flag, value) = t.split_at(space.unwrap_or_default());
            return Err(format!(
                "'{t}' is two arguments in one entry: put '{flag}' and '{}' on separate entries \
                 (one argument per entry, or per line in the dashboard)",
                value.trim()
            ));
        }
        out.push(t.to_string());
    }
    Ok(out)
}

/// Sparse patch for an aux (embedding / rerank / decision) model — a slimmer
/// sibling of the local model patch: no sampling, no template, no
/// speculation. What an aux row *does* need beyond a path is its kind and,
/// for embedders, the pooling the model was trained with; both are read out
/// of the GGUF by `lmgw__local_model_plan target=aux`.
///
/// Placement, the batch sizes and a projector are fields
/// (decision-models design §3.3): `backend`, `threads`, `ubatch_size`,
/// `batch_size`, `mmproj_path`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuxModelPatch {
    pub action: String,
    pub id: Option<i64>,
    pub model_id: Option<String>,
    pub gguf_path: Option<String>,
    /// `embed` (default on create) | `rerank` | `decision`.
    pub kind: Option<String>,
    /// `none` | `mean` | `cls` | `last` | `rank`; empty clears it back to
    /// llama-server's choice. Never set on a rerank or decision model.
    pub pooling: Option<String>,
    /// `--ctx-size`; `ctx_size` in `clear` takes llama-server's default.
    pub ctx_size: Option<i64>,
    /// `--ubatch-size`, 1 or more; `ubatch_size` in `clear` takes
    /// llama-server's default. An encoder whose input llama-server cannot
    /// split (any but a last-token embedder or a causal reranker) evaluates
    /// a whole input in one ubatch, so this bounds every input it answers;
    /// the plan sets it to the context there.
    pub ubatch_size: Option<i64>,
    /// `--batch-size`, 1 or more; `batch_size` in `clear` takes the default.
    /// llama.cpp lowers the ubatch to the batch, so raise both together.
    pub batch_size: Option<i64>,
    /// A projector (`--mmproj`), relative to the aux models dir; empty (or
    /// `mmproj_path` in `clear`) is none. Charged with the weights.
    pub mmproj_path: Option<String>,
    /// `cpu` runs the row on the CPU: `--n-gpu-layers 0`, the class's run
    /// args without the GPU passthrough, no VRAM charged, served while the
    /// GPU hold is on. Blank (or `backend` in `clear`) runs it on the GPU.
    /// No other value is accepted.
    pub backend: Option<String>,
    /// `--threads`, 1 or more (no upper cap; above this machine's CPUs the
    /// save says so). `threads` in `clear` renders none on the GPU, and on
    /// the CPU this machine's physical cores (the logical CPU count where
    /// the core topology cannot be read) unless the extra args name a
    /// count.
    pub threads: Option<i64>,
    /// Freeform llama-server flags. `args` is the dashboard's spelling.
    #[serde(alias = "args")]
    pub extra_args: Option<Vec<String>>,
    pub idle_seconds: Option<i64>,
    pub enabled: Option<bool>,
    /// Per-model image override; empty clears back to the class image.
    pub image: Option<String>,
    /// `podman run` args override; name it in `clear` to revert to
    /// inheriting the class setting. An empty list inherits too (it is never
    /// stored as one); absent leaves the current value.
    pub extra_run_args: Option<Vec<String>>,
    pub warm_start: Option<bool>,
    /// `inherit` (default — which for this class means *no* fallback) |
    /// `none` | `alias`.
    pub hold_fallback_mode: Option<String>,
    pub hold_fallback: Option<String>,
    /// Field names to reset: `extra_run_args`, `hold_fallback`, `extra_args`,
    /// `ctx_size`, `ubatch_size`, `batch_size`, `mmproj_path`, `backend`,
    /// `threads`.
    pub clear: Option<String>,
}

/// The two things an aux row's *kind* decides, refused where they are typed
/// rather than only dropped where the argv is rendered. A decision row takes
/// neither kind flag and no pooling (decision-models design §3.5).
///
/// - A model's argv carries its own kind flag and never the other one. An
///   `--reranking` left in an embedding model's extra args would make
///   llama-server answer `/v1/embeddings` with HTTP 200 and an all-zero vector
///   (§9a, verified live) — a corpus of noise that looks healthy.
/// - A reranker gets no `--pooling`: `--reranking` selects rank pooling
///   itself, and a second pooling flag breaks reranking outright.
///
/// The argv renderer claims all three flags so the escape hatch cannot emit
/// them either way; this is the same refusal said out loud.
pub fn check_aux_flags(
    kind: AuxKind,
    pooling: Option<&str>,
    args: &[String],
) -> Result<(), String> {
    let has = |name: &str| {
        args.iter()
            .any(|a| a.trim_start_matches('-').split('=').next() == Some(name))
    };
    let (own, other) = match kind {
        AuxKind::Embed => ("embedding", "reranking"),
        AuxKind::Rerank => ("reranking", "embedding"),
        AuxKind::Decision => return fields::check_decision_flags(pooling, args),
    };
    if has(other) {
        return Err(format!(
            "remove --{other} from the extra flags: this is a '{}' model, so lmgw writes \
             `{own} = true` for it and the two flags contradict each other. A section serving \
             embeddings with reranking on answers every request with an all-zero vector.",
            kind.as_str()
        ));
    }
    if let Some(p) = pooling.filter(|p| !p.is_empty()) {
        if !POOLING_VALUES.contains(&p) {
            return Err(format!(
                "unknown pooling '{p}' (expected one of {})",
                POOLING_VALUES.join(", ")
            ));
        }
    }
    if kind != AuxKind::Rerank {
        return Ok(());
    }
    if pooling.is_some_and(|p| !p.is_empty()) {
        return Err(
            "rerank models take no pooling — `reranking = true` selects rank pooling itself, \
             and setting pooling as well stops reranking working"
                .into(),
        );
    }
    if has("pooling") {
        return Err(
            "remove --pooling from the extra flags: a rerank section must carry no pooling key"
                .into(),
        );
    }
    Ok(())
}

/// Configuration an aux row renders cleanly but cannot serve, from the files
/// it names and the header of its weights — the aux counterpart of
/// [`model_warnings`], shared by `local_model_check`, `local_model_get` and
/// the set path's warnings.
pub(super) async fn aux_model_problems(models_dir: &str, m: &AuxModel) -> Vec<String> {
    let mut out = Vec::new();
    if let Err(e) = check_aux_flags(m.kind, m.pooling.as_deref(), &m.args) {
        out.push(e);
    }
    if models_dir.trim().is_empty() {
        return out;
    }
    let root = std::path::Path::new(models_dir);
    if !root.join(&m.gguf_path).is_file() {
        out.push(format!(
            "gguf_path '{}' is missing from the aux models dir",
            m.gguf_path
        ));
    }
    // A projector named in the escape hatch is a path like any other, and a
    // relative one is the usual mistake: llama-server resolves it against
    // its working directory, not /models.
    if let Some(mm) = crate::runtime::argv::flag_value(&m.args, "mmproj") {
        let rel = strip_models_prefix(mm);
        if mm.starts_with("/models/") || !mm.starts_with('/') {
            if !root.join(&rel).is_file() {
                out.push(format!(
                    "--mmproj '{mm}' is not a file under the aux models dir"
                ));
            } else if !mm.starts_with("/models/") {
                out.push(format!(
                    "--mmproj '{mm}' is relative; llama-server resolves it against its own \
                     working directory, not /models — write it as '/models/{rel}'"
                ));
            }
        }
    }

    let read = |rel: &str| {
        let path = root.join(rel);
        async move {
            tokio::task::spawn_blocking(move || crate::gguf::summarize(&path))
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    let summary = read(&m.gguf_path).await;
    let projector = match &m.mmproj_path {
        Some(mm) => read(mm).await,
        None => None,
    };
    out.extend(fields::problems(
        m,
        root,
        summary.as_ref(),
        projector.as_ref(),
    ));
    let Some(summary) = summary else {
        return out;
    };
    match crate::modelinfo::role_of(&summary) {
        "mmproj" => out.push(format!(
            "gguf_path '{}' is a multimodal projector, not weights",
            m.gguf_path
        )),
        "drafter" => out.push(format!(
            "gguf_path '{}' looks like a speculative drafter, not weights",
            m.gguf_path
        )),
        _ => {}
    }
    let pooling = summary.pooling_type.and_then(crate::gguf::pooling_name);
    match m.kind {
        AuxKind::Rerank if pooling != Some("rank") && !summary.has_classifier_head => {
            out.push(format!(
                "'{}' has no classifier head and its header {} — it does not look like a \
                 reranker; served with --reranking it would score with pooled hidden states",
                m.gguf_path,
                match pooling {
                    Some(p) => format!("declares pooling '{p}'"),
                    None => "declares no pooling".to_string(),
                }
            ));
        }
        AuxKind::Embed if pooling == Some("rank") || summary.has_classifier_head => {
            out.push(format!(
                "'{}' carries a classifier head (a cross-encoder reranker) — set kind=rerank; \
                 as an embedding model it would answer with meaningless vectors",
                m.gguf_path
            ));
        }
        AuxKind::Embed => {
            if let (Some(declared), Some(set)) = (pooling, m.pooling.as_deref()) {
                if declared != set && declared != "none" {
                    out.push(format!(
                        "pooling '{set}' differs from the '{declared}' the GGUF header declares \
                         it was trained with — vectors will not match the model card's"
                    ));
                }
            }
            if pooling.is_none() && summary.has_chat_template {
                out.push(format!(
                    "'{}' looks like a chat model (no pooling_type in its header, a chat \
                     template) — serving it with --embeddings pools its hidden states, which \
                     works but is not what a trained embedder produces",
                    m.gguf_path
                ));
            }
        }
        // `fields::problems` reads the decision header.
        AuxKind::Rerank | AuxKind::Decision => {}
    }
    out
}

/// Advisory problems with an aux row's freeform args, checked against the
/// running build of the image it would use — the aux counterpart of
/// [`config_warnings`], minus the chat-only structured fields — and against
/// the row's own fields (`fields::arg_warnings`).
async fn aux_config_warnings(
    state: &SharedState,
    image: &str,
    run_args: &[String],
    m: &AuxModel,
) -> Vec<String> {
    let args = &m.args;
    // Read from the row alone, so an image whose `--help` cannot be read
    // does not hide them.
    let against_fields = fields::arg_warnings(m);
    let caps = match llama_vocabulary(state, image, run_args).await {
        Ok(caps) => caps,
        Err(unchecked) => return [vec![unchecked], against_fields].concat(),
    };
    let mut out = caps.validate_args(args);
    out.extend(against_fields);
    for tok in args.iter().filter(|t| t.starts_with('-')) {
        let flag = tok.split('=').next().unwrap_or(tok);
        let key = flag.trim_start_matches('-');
        if matches!(
            key,
            "embeddings" | "embedding" | "reranking" | "pooling" | "ctx-size" | "c"
        ) {
            out.push(format!(
                "`{flag}` is rendered from the row's kind / pooling / ctx_size fields — set it \
                 there; passed as an extra arg it is ignored"
            ));
        }
    }
    out
}

/// The image an aux model runs and its `podman run` args: its own overrides,
/// else the aux class defaults — the aux half of
/// [`effective_chat_image`](crate::ops::local_model).
fn effective_aux_image(
    state: &SharedState,
    override_image: &Option<String>,
    override_run_args: &Option<Vec<String>>,
) -> (String, Vec<String>) {
    let snap = state.snapshot();
    let aux = &snap.settings.aux_router;
    (
        override_image.clone().unwrap_or_else(|| aux.image.clone()),
        override_run_args
            .clone()
            .unwrap_or_else(|| aux.extra_run_args.clone()),
    )
}

/// Read back one aux model, or `None` when nothing matches — the aux half of
/// [`local_model_get`], same shape where the fields overlap.
pub(super) async fn aux_model_get(
    state: &SharedState,
    id: Option<i64>,
    model_id: Option<&str>,
) -> Result<Option<dto::AuxModelDetail>, String> {
    let snap = state.snapshot();
    let found = match (id, model_id) {
        (Some(id), _) => snap.aux_models.iter().find(|m| m.id == id),
        (None, Some(mid)) => snap.aux_models.iter().find(|m| m.model_id == mid),
        (None, None) => return Err("pass id or model_id".into()),
    };
    let Some(m) = found else {
        return Ok(None);
    };
    let models_dir = snap.settings.aux_router.models_dir.clone();
    let downloads = store::list_hf_models(&state.db).await.unwrap_or_default();
    let origin = downloads
        .iter()
        .find(|d| d.dest_path == m.gguf_path && d.target == "aux")
        .map(|d| dto::DownloadedFrom {
            repo: d.repo.clone(),
            file: d.file.clone(),
            download_id: d.id,
        });
    let mut problems = aux_model_problems(&models_dir, m).await;
    let (image, run_args) = effective_aux_image(state, &m.image, &m.extra_run_args);
    problems.extend(aux_config_warnings(state, &image, &run_args, m).await);
    let command_line = command_line_preview(state, Class::Aux, &m.model_id);
    let (threads_in_effect, threads_source) =
        crate::runtime::aux::threads_in_effect(m, crate::host::cpu());
    let licence = crate::modelinfo::licence::of_file(state, &models_dir, &m.gguf_path).await;
    Ok(Some(dto::AuxModelDetail {
        id: m.id,
        model_id: m.model_id.clone(),
        public_name: snap.aux_public_name(m),
        kind: m.kind.as_str().to_string(),
        endpoint: m.kind.endpoint().to_string(),
        pooling: m.pooling.clone(),
        ctx_size: m.ctx_size,
        ubatch_size: m.ubatch_size,
        batch_size: m.batch_size,
        mmproj_path: m.mmproj_path.clone(),
        backend: m.backend.map(|b| b.as_str().to_string()),
        threads: m.threads,
        threads_in_effect,
        threads_source,
        gguf_path: m.gguf_path.clone(),
        gguf_present: !models_dir.trim().is_empty()
            && std::path::Path::new(&models_dir)
                .join(&m.gguf_path)
                .is_file(),
        licence,
        source: if origin.is_some() { "hf" } else { "manual" }.to_string(),
        downloaded_from: origin,
        extra_args: m.args.clone(),
        idle_seconds: m.idle_seconds,
        enabled: m.enabled,
        image: m.image.clone(),
        extra_run_args: m.extra_run_args.clone(),
        warm_start: m.warm_start,
        hold_fallback_mode: m.hold_fallback_mode.as_str().to_string(),
        hold_fallback: m.hold_fallback.clone(),
        command_line,
        problems,
    }))
}

/// Resolve the hold fallback for an aux create/update from string-typed
/// fields (the dashboard and the tool plane both send the mode as text).
fn resolve_aux_hold_fallback(
    snap: &Snapshot,
    p: &AuxModelPatch,
    current: (HoldFallbackMode, Option<String>),
) -> Result<(HoldFallbackMode, Option<String>), String> {
    resolve_hold_fallback_text(
        snap,
        p.hold_fallback_mode.as_deref(),
        p.hold_fallback.as_deref(),
        p.clear.as_deref(),
        current,
    )
}

/// `hold_fallback_mode`/`hold_fallback` resolution for the patches that carry
/// the mode as raw text (aux here, audio in `web::api`) rather than as
/// [`LocalModelPatch`]'s typed enum (gpu-hold design §2/§3.2).
///
/// `clear: "hold_fallback"` wins over everything else and resets both to
/// inherit/NULL — same convention as `extra_run_args`. Otherwise mode `alias`
/// needs a non-empty alias, from the patch or the current row, and it is
/// validated (must resolve, must not be local); `inherit`/`none` always clear
/// the alias column.
pub fn resolve_hold_fallback_text(
    snap: &Snapshot,
    mode: Option<&str>,
    fallback: Option<&str>,
    clear: Option<&str>,
    current: (HoldFallbackMode, Option<String>),
) -> Result<(HoldFallbackMode, Option<String>), String> {
    if clear_has(clear, "hold_fallback") {
        return Ok((HoldFallbackMode::Inherit, None));
    }
    let (cur_mode, cur_alias) = current;
    let mode = match mode.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse::<HoldFallbackMode>()
            .map_err(|_| format!("unknown hold_fallback_mode '{s}' (inherit|none|alias)"))?,
        None => cur_mode,
    };
    match mode {
        HoldFallbackMode::Alias => {
            let alias = fallback
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or(cur_alias)
                .ok_or(
                    "hold_fallback_mode 'alias' requires hold_fallback (the alias to route to \
                     while the GPU is held)",
                )?;
            validate_fallback_alias(snap, &alias).map_err(|e| format!("hold_fallback: {e}"))?;
            Ok((mode, Some(alias)))
        }
        HoldFallbackMode::Inherit | HoldFallbackMode::None => Ok((mode, None)),
    }
}

/// Create, update, delete, enable or disable an aux (embedding / rerank)
/// model — the class the tool plane could not write until now, which is how
/// embedding models ended up as chat-class rows with `embed/` in their ids
/// and their GGUFs hard-linked into the chat models dir.
///
/// Same conventions as [`local_model_set`]: sparse patch, the aux models dir
/// as the path boundary (a path that resolves to nothing is refused, naming
/// the tools that find or fetch one), `reload_snapshot()` after every write,
/// container hygiene keyed on the *current* model id.
pub async fn aux_model_set(
    state: &SharedState,
    p: AuxModelPatch,
) -> Result<dto::AuxModelSaved, String> {
    let snap = state.snapshot();
    let all = snap.aux_models.clone();
    let find = || -> Result<AuxModel, String> {
        if let Some(id) = p.id {
            return all
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| format!("no aux model with id {id}"));
        }
        let mid = opt(&p.model_id).ok_or("this action requires id or model_id")?;
        all.iter()
            .find(|m| m.model_id == mid)
            .cloned()
            .ok_or_else(|| format!("no aux model with model_id '{mid}'"))
    };
    let models_dir = snap.settings.aux_router.models_dir.clone();
    let check_path = |rel: &str| -> Result<(), String> {
        check_rel_path(rel, "aux models dir")?;
        if models_dir.trim().is_empty() {
            return Ok(());
        }
        if std::path::Path::new(&models_dir).join(rel).is_file() {
            return Ok(());
        }
        Err(format!(
            "gguf_path '{rel}' is not under the aux models directory ({models_dir}). Use \
             lmgw__gguf_files target=aux to see what is there, or lmgw__hf_add target=aux to \
             download it from Hugging Face. (The chat models directory is a different tree — \
             an aux row cannot address a file there.)"
        ))
    };
    let parse_kind = |s: &str| {
        AuxKind::parse(s.trim())
            .ok_or_else(|| format!("unknown kind '{s}' (embed|rerank|decision)"))
    };
    let reload = || async { state.reload_snapshot().await.map_err(|e| e.to_string()) };

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("model_id", opt(&p.model_id).is_some()),
                ("gguf_path", opt(&p.gguf_path).is_some()),
            ])?;
            let model_id = opt(&p.model_id).expect("checked above");
            let kind = match opt(&p.kind) {
                Some(k) => parse_kind(&k)?,
                None => AuxKind::Embed,
            };
            // The kind picks the prefix (decision-models design §3.10).
            let public_name = snap.aux_name_for(kind, &model_id);
            refuse_if_candidate_alias_name(&snap, &public_name)?;
            let gguf_path = strip_models_prefix(&opt(&p.gguf_path).expect("checked above"));
            check_path(&gguf_path)?;
            let pooling = opt(&p.pooling);
            let args = match &p.extra_args {
                Some(a) => arg_list(a)?,
                None => vec![],
            };
            check_aux_flags(kind, pooling.as_deref(), &args)?;
            let (hold_fallback_mode, hold_fallback) =
                resolve_aux_hold_fallback(&snap, &p, (HoldFallbackMode::Inherit, None))?;
            fallback::check_decision_fallback(
                state,
                &snap,
                kind,
                hold_fallback_mode,
                hold_fallback.as_deref(),
            )
            .await?;
            let f = fields::patched(&p, None)?;
            let new = NewAuxModel {
                model_id: model_id.clone(),
                gguf_path,
                kind,
                pooling,
                ctx_size: p.ctx_size,
                ubatch_size: f.ubatch_size,
                batch_size: f.batch_size,
                mmproj_path: f.mmproj_path,
                backend: f.backend,
                threads: f.threads,
                args,
                idle_seconds: p.idle_seconds.unwrap_or(300),
                enabled: p.enabled.unwrap_or(true),
                image: opt(&p.image),
                // A named clear wins over the value on create as on update:
                // the editor sends a blank field as `[]` *and* names it. An
                // empty list without the clear is no override either.
                extra_run_args: match &p.extra_run_args {
                    _ if clear_has(p.clear.as_deref(), "extra_run_args") => None,
                    Some(a) => run_args_override(Some(arg_list(a)?)),
                    None => None,
                },
                warm_start: p.warm_start.unwrap_or(false),
                hold_fallback_mode,
                hold_fallback,
            };
            let (image, run_args) = effective_aux_image(state, &new.image, &new.extra_run_args);
            let row = as_aux_model(&new);
            let mut warnings = aux_config_warnings(state, &image, &run_args, &row).await;
            warnings.extend(aux_model_problems(&models_dir, &row).await);
            warnings.extend(fields::threads_note(new.threads, crate::host::cpu()));
            let id = store::insert_aux_model(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            Ok(dto::AuxModelSaved {
                ok: true,
                id,
                warnings: Some(warnings),
                message: format!(
                    "{} model '{model_id}' created — clients request it as '{public_name}' on \
                     {}; run lmgw__local_model_test model_id={model_id} target=aux to prove it \
                     loads (its container starts on the first request)",
                    kind.as_str(),
                    kind.endpoint()
                ),
                public_name: Some(public_name),
            })
        }
        "update" | "enable" | "disable" => {
            let cur = find()?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let gguf_path = match opt(&p.gguf_path) {
                Some(g) => strip_models_prefix(&g),
                None => cur.gguf_path.clone(),
            };
            check_path(&gguf_path)?;
            let kind = match opt(&p.kind) {
                Some(k) => parse_kind(&k)?,
                None => cur.kind,
            };
            // An empty string clears, like the dashboard's editor sends it.
            let pooling = match p.pooling.as_deref().map(str::trim) {
                Some("") => None,
                Some(s) => Some(s.to_string()),
                None => cur.pooling.clone(),
            };
            let args = match &p.extra_args {
                Some(a) => arg_list(a)?,
                None if clear_has(p.clear.as_deref(), "extra_args") => vec![],
                None => cur.args.clone(),
            };
            check_aux_flags(kind, pooling.as_deref(), &args)?;
            let image = match p.image.as_deref().map(str::trim) {
                _ if clear_has(p.clear.as_deref(), "image") => None,
                Some("") => None,
                Some(s) => Some(s.to_string()),
                None => cur.image.clone(),
            };
            // An empty list inherits the class; absent leaves the current value.
            let extra_run_args = if clear_has(p.clear.as_deref(), "extra_run_args") {
                None
            } else {
                match &p.extra_run_args {
                    Some(a) => Some(arg_list(a)?),
                    None => cur.extra_run_args.clone(),
                }
            };
            let extra_run_args = run_args_override(extra_run_args);
            let (hold_fallback_mode, hold_fallback) = resolve_aux_hold_fallback(
                &snap,
                &p,
                (cur.hold_fallback_mode, cur.hold_fallback.clone()),
            )?;
            // A row turned into a decision row keeps no fallback that cannot
            // answer System One, nor does one whose fallback changed (§2.4).
            // Only then: an enable or a rename needs no catalog fetch, and
            // the gate checks the fallback on every request anyway.
            if kind != cur.kind
                || hold_fallback_mode != cur.hold_fallback_mode
                || hold_fallback != cur.hold_fallback
            {
                fallback::check_decision_fallback(
                    state,
                    &snap,
                    kind,
                    hold_fallback_mode,
                    hold_fallback.as_deref(),
                )
                .await?;
            }
            let new_aux_model_id = match (p.id, opt(&p.model_id)) {
                (Some(_), Some(m)) => m,
                _ => cur.model_id.clone(),
            };
            // Same rename guard as `model_set`/`local_model_set` (review
            // finding X4). A kind change across the prefixes renames the row
            // too (decision-models design §3.10).
            let new_public_name = snap.aux_name_for(kind, &new_aux_model_id);
            if new_public_name != snap.aux_public_name(&cur) {
                refuse_if_candidate_alias_name(&snap, &new_public_name)?;
            }
            let f = fields::patched(&p, Some(&cur))?;
            let new = NewAuxModel {
                model_id: new_aux_model_id,
                gguf_path,
                kind,
                pooling,
                ctx_size: if clear_has(p.clear.as_deref(), "ctx_size") {
                    None
                } else {
                    p.ctx_size.or(cur.ctx_size)
                },
                ubatch_size: f.ubatch_size,
                batch_size: f.batch_size,
                mmproj_path: f.mmproj_path,
                backend: f.backend,
                threads: f.threads,
                args,
                idle_seconds: p.idle_seconds.unwrap_or(cur.idle_seconds),
                enabled,
                image,
                extra_run_args,
                warm_start: p.warm_start.unwrap_or(cur.warm_start),
                hold_fallback_mode,
                hold_fallback,
            };
            let (image, run_args) = effective_aux_image(state, &new.image, &new.extra_run_args);
            let row = as_aux_model(&new);
            let mut warnings = aux_config_warnings(state, &image, &run_args, &row).await;
            warnings.extend(aux_model_problems(&models_dir, &row).await);
            warnings.extend(fields::threads_note(new.threads, crate::host::cpu()));
            store::update_aux_model(&state.db, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // Container hygiene (per-model-containers §3.4/§3.6), keyed on
            // the *current* model id so a rename stops the container that
            // exists — see the chat path in `local_model_set` for why a
            // rename is a drop rather than a stop.
            let note = if new.model_id != cur.model_id {
                lifecycle::drop_model(state, Class::Aux, &cur.model_id).await;
                Some(format!(
                    "it was renamed from '{}', so that container was stopped and removed — the \
                     next request starts '{}' fresh",
                    cur.model_id, new.model_id
                ))
            } else if enabled {
                lifecycle::stop_for_apply(state, Class::Aux, &cur.model_id).await
            } else {
                lifecycle::drop_model(state, Class::Aux, &cur.model_id).await;
                Some(format!(
                    "'{}' is disabled — its container was stopped and removed",
                    cur.model_id
                ))
            };
            Ok(dto::AuxModelSaved {
                ok: true,
                id: cur.id,
                warnings: Some(warnings),
                message: match note {
                    Some(note) => format!(
                        "aux model '{}' {} — {note}",
                        new.model_id,
                        saved_verb(&p.action)
                    ),
                    None => format!(
                        "aux model '{}' {} — it is not running, so the next request \
                         starts it with the new configuration",
                        new.model_id,
                        saved_verb(&p.action)
                    ),
                },
                ..Default::default()
            })
        }
        "delete" => {
            let cur = find()?;
            store::delete_aux_model(&state.db, cur.id)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // §3.4: immediately, not at the next boot.
            lifecycle::drop_model(state, Class::Aux, &cur.model_id).await;
            Ok(dto::AuxModelSaved {
                ok: true,
                id: cur.id,
                message: format!(
                    "aux model '{}' deleted — its container was stopped and removed",
                    cur.model_id
                ),
                ..Default::default()
            })
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable)"
        )),
    }
}

/// A [`NewAuxModel`] as the row it would become, for the checks that read
/// rows. The id is a placeholder — nothing in the checks reads it.
fn as_aux_model(n: &NewAuxModel) -> AuxModel {
    AuxModel {
        id: 0,
        model_id: n.model_id.clone(),
        gguf_path: n.gguf_path.clone(),
        kind: n.kind,
        pooling: n.pooling.clone(),
        ctx_size: n.ctx_size,
        ubatch_size: n.ubatch_size,
        batch_size: n.batch_size,
        mmproj_path: n.mmproj_path.clone(),
        backend: n.backend,
        threads: n.threads,
        args: n.args.clone(),
        idle_seconds: n.idle_seconds,
        enabled: n.enabled,
        image: n.image.clone(),
        extra_run_args: n.extra_run_args.clone(),
        warm_start: n.warm_start,
        hold_fallback_mode: n.hold_fallback_mode,
        hold_fallback: n.hold_fallback.clone(),
    }
}
