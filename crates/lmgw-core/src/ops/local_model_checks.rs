//! Non-fatal, advisory checks for a local model's configuration: read
//! llama-server's own `--help` vocabulary and check a model's config
//! against it -- `config_warnings`/`model_warnings` (used by
//! `local_model_get`/`local_model_set`) and `lmgw__llama_flags`, which
//! answers the same question for a caller directly.

use serde_json::{json, Map, Value};

use crate::config::Snapshot;
use crate::runtime::descriptor::model_runtime;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::NewLocalModel;

/// The llama-server flag vocabulary `image` validates against — or, as the
/// `Err`, the warning that says it could not be read and so nothing was
/// checked.
///
/// That warning is the point. The read used to fail silently ("no podman, no
/// vocabulary, say nothing"), which made an image the read could not handle —
/// ik_llama.cpp's, whose binary was on no path the read tried — look exactly
/// like a config with nothing wrong in it. A check that did not run is now
/// reported as not having run, with podman's reason, never as a pass.
pub(super) async fn llama_vocabulary(
    state: &SharedState,
    image: &str,
    run_args: &[String],
) -> Result<crate::llama_caps::LlamaCaps, String> {
    let snap = state.snapshot();
    state
        .runtime()
        .help_text(&snap.settings.container_prefix, image, run_args)
        .await
        .map(|help| crate::llama_caps::parse_help(&help))
        .map_err(|e| format!("{e} — the extra args and flag values were not validated"))
}

/// Non-fatal problems with the freeform extra args and the enum-ish fields,
/// checked against the `--help` of the image the model runs.
///
/// Advisory rather than fatal on purpose: the vocabulary comes from a
/// throwaway container, and a gateway that refuses to save a config because
/// that container could not run (image not pulled yet, card busy) would be
/// worse than one that saves it with a note — which is also what a read that
/// failed comes back as. A caller that wants certainty runs
/// `lmgw__local_model_test`.
pub(super) async fn config_warnings(
    state: &SharedState,
    image: &str,
    run_args: &[String],
    params: &crate::config::LlamaParams,
    args: &[String],
) -> Vec<String> {
    let caps = match llama_vocabulary(state, image, run_args).await {
        Ok(caps) => caps,
        Err(unchecked) => return vec![unchecked],
    };
    let mut out = caps.validate_args(args);

    // A flag with a dedicated field must not also come through the escape
    // hatch: the renderer drops the duplicate, so the arg would silently do
    // nothing rather than fail loudly.
    for tok in args.iter().filter(|t| t.starts_with('-')) {
        // `--ctx-size=4096` is one token but renders as the `ctx-size` key, so
        // match on the flag half — otherwise the `=` form slips past and
        // silently loses to the structured field.
        let flag = tok.split('=').next().unwrap_or(tok);
        if crate::llama_caps::is_managed(flag) {
            out.push(format!(
                "`{flag}` has a dedicated field — set it there instead; \
                 passed as an extra arg it is ignored"
            ));
        }
        // Deprecated-but-working flags are reported separately from removed
        // ones: llama-server still accepts these, so this is advice, not a
        // defect, and refusing them would block configs that load fine.
        if caps.is_deprecated(flag) {
            let note = caps
                .note_for(flag)
                .map(|n| format!(" — {}", n.split_once("(env:").map_or(n, |(a, _)| a).trim()))
                .unwrap_or_default();
            out.push(format!("`{flag}` is deprecated in this build{note}"));
        }
    }

    // The structured enum-ish fields, checked against the *running* build.
    // This is the check that catches `spec_type: "draft"` (a real value from a
    // published model card) against a build whose actual value is
    // `draft-dflash` — the schema's enum can only ever be a snapshot of some
    // llama.cpp, whereas this is the one that is going to load the model.
    for (flag, value) in [
        ("spec-type", &params.spec_type),
        ("flash-attn", &params.flash_attn),
        ("cache-type-k", &params.cache_type_k),
        ("cache-type-v", &params.cache_type_v),
        ("reasoning-format", &params.reasoning_format),
        ("reasoning", &params.reasoning),
        ("fit", &params.fit),
    ] {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            if let Some(problem) = caps.validate_pair(flag, Some(v)) {
                out.push(problem);
            }
        }
    }

    // Both of these are recent llama.cpp additions, so "does this build have
    // the flag at all" is the check that matters — there is no enum to police.
    if params.reasoning_preserve.is_some() {
        if let Some(problem) = caps.validate_pair("reasoning-preserve", None) {
            out.push(problem);
        }
    }
    // Same treatment for the unified-KV toggle (candidate-aliases/unified-KV
    // design §3.1): recent enough that a build that predates it should say so
    // rather than silently ignore the flag.
    if params.kv_unified.is_some() {
        if let Some(problem) = caps.validate_pair("kv-unified", None) {
            out.push(problem);
        }
    }
    if let Some(n) = params.kv_unified_per_slot {
        if let Some(problem) = caps.validate_pair("kv-unified-per-slot", Some(&n.to_string())) {
            out.push(problem);
        }
    }
    if !params.chat_template_kwargs.is_empty() {
        if let Some(problem) = caps.validate_pair("chat-template-kwargs", Some("{}")) {
            out.push(problem);
        }
    }
    // Deliberately a "does this build have the flag" check and not a value
    // check: llama-server hands the effort level to the chat template
    // verbatim without validating it, so which levels mean something is the
    // template's business. The old route still works when the flag does not.
    if params.reasoning_effort.is_some() && !caps.supports("reasoning-effort") {
        out.push(
            "`--reasoning-effort` is not in this llama.cpp build — update the image, or \
             set the level through chat_template_kwargs, which reaches the same \
             template variable"
                .to_string(),
        );
    }
    out
}

/// What the header-reading checks found on one chat row, split by whether
/// the model can start at all.
#[derive(Debug, Default)]
pub(super) struct ModelChecks {
    /// Configuration that stops the model loading. These are what make a row
    /// "broken" in `lmgw__local_model_check`.
    pub(super) problems: Vec<String>,
    /// Configuration that loads and serves, but misbehaves under some input.
    /// Reported next to the problems and labelled as advisories; they never
    /// mark a row broken.
    pub(super) advisories: Vec<String>,
}

/// Configuration that renders cleanly but cannot load, caught by reading the
/// weights GGUF's header.
///
/// Two families of mistake, both of which otherwise surface only as an opaque
/// llama-server error minutes later:
///
/// - **The wrong file as `gguf_path`.** A projector or a drafter passed as
///   weights passes every path check, because it *is* a real file.
/// - **Speculation the model cannot do.** `spec-type = draft-mtp` with no
///   `model-draft` requires MTP heads inside the weights themselves; the other
///   `draft-*` types require an actual drafter file. Neither is checked by
///   llama-server until load.
///
/// One header read covers both, which is why they share a function.
///
/// One advisory reads the projector's header too: a projector whose images
/// llama.cpp decodes with non-causal attention, under batch sizes too small
/// for a whole image. That model loads fine, and then aborts llama-server (or
/// splits the image) on the first large enough image — see
/// [`crate::modelinfo::projector_ubatch_advisory`].
///
/// A second, unrelated advisory shares the same projector read: a
/// [`crate::config::LlamaParams::pool_guarded`] row whose projector has no
/// known per-image token bound ([`crate::gate::count::image_token_bound`]
/// returns `Err`). That model loads fine too, but every image request to it
/// is refused at the gate until `--image-max-tokens` gives the reservation
/// something to bound.
pub(super) async fn model_warnings(models_dir: &str, m: &NewLocalModel) -> ModelChecks {
    let mut out = ModelChecks::default();
    let spec = m.params.spec_type.as_deref().unwrap_or("");

    // Every `draft-*` type except `draft-mtp` drafts from a separate file.
    if spec.starts_with("draft-") && spec != "draft-mtp" && m.params.draft_gguf_path.is_none() {
        out.problems.push(format!(
            "spec_type '{spec}' drafts from a separate model, but \
             draft_gguf_path is not set — it will fail to load. Set the \
             drafter, or clear spec_type."
        ));
    }

    let summary = if models_dir.trim().is_empty() {
        None
    } else {
        let path = std::path::Path::new(models_dir).join(&m.gguf_path);
        tokio::task::spawn_blocking(move || crate::gguf::summarize(&path))
            .await
            .ok()
            .and_then(Result::ok)
    };

    // Runs even when the weights could not be read: the projector check
    // then says what it could not tell rather than staying silent. A
    // projector file that is not there is skipped; the missing file is the
    // problem, reported by the callers' path check.
    if crate::modelinfo::row_loads_projector(&m.params, &m.args) {
        let attention = crate::modelinfo::row_image_attention(
            models_dir,
            &m.params,
            &m.args,
            summary.as_ref().and_then(|s| s.embedding_length),
        )
        .await;
        if let Some(attention) = attention {
            out.advisories
                .extend(crate::modelinfo::projector_ubatch_advisory(
                    &m.params, &m.args, &attention,
                ));

            // A pool-guarded row (unified-KV design §12) reserves against a
            // per-image token bound before it will admit an image request —
            // see `crate::gate::count::image_token_bound`. A projector this
            // gateway cannot bound (no measured ceiling and no
            // `--image-max-tokens`) means every image request to this row is
            // refused at the gate rather than risk an unbounded reservation
            // against the shared pool. Say so here rather than let it
            // surface only as a live refusal on the first image.
            if m.params.pool_guarded() {
                let row = crate::gate::count::ProjectorRow {
                    model_id: &m.model_id,
                    gguf_path: &m.gguf_path,
                    params: &m.params,
                    args: &m.args,
                };
                if crate::gate::count::image_token_bound(models_dir, row)
                    .await
                    .is_err()
                {
                    out.advisories.push(format!(
                        "'{}' is pool-guarded (unified KV, n_predict set) and lmgw has no \
                         per-image token ceiling for its projector — image requests to it will \
                         be refused until --image-max-tokens is set in the row's args",
                        m.model_id
                    ));
                }
            }
        }
    }

    let Some(summary) = summary else {
        return out;
    };

    match crate::modelinfo::role_of(&summary) {
        "mmproj" => out.problems.push(format!(
            "gguf_path '{}' is a multimodal projector, not weights — pass it \
             as mmproj_path and point gguf_path at the weights file.",
            m.gguf_path
        )),
        "drafter" => out.problems.push(format!(
            "gguf_path '{}' looks like a speculative drafter, not weights — \
             pass it as draft_gguf_path and point gguf_path at the weights.",
            m.gguf_path
        )),
        _ => {}
    }

    if spec == "draft-mtp" && m.params.draft_gguf_path.is_none() && !summary.has_mtp_layers {
        out.problems.push(format!(
            "spec_type 'draft-mtp' needs MTP layers, but {} has none — it will \
             fail to load. Clear spec_type, or set draft_gguf_path to a real \
             drafter GGUF.",
            m.gguf_path
        ));
    }
    out
}

/// The llama-server flag vocabulary of one image (§3.6, "probe / help").
///
/// Read from the image rather than compiled in, because it is a property of
/// the image: a build that gained `--spec-type draft-dflash` can say so
/// without an lmgw release, and a caller told "that value is not allowed" can
/// find out what *is*.
///
/// `model_or_image` selects which vocabulary: a configured model id (any
/// class) answers with the image *that* model runs, anything else is taken as
/// an image reference verbatim, and `None` falls back to the chat class
/// default. Per-model image overrides (§3.1) are exactly why this is a
/// parameter — with three models on three images there is no single answer.
pub async fn llama_flags(
    state: &SharedState,
    search: Option<&str>,
    model_or_image: Option<&str>,
) -> Result<Value, String> {
    let snap = state.snapshot();
    let (image, run_args) = resolve_flag_image(&snap, model_or_image)?;
    let help = state
        .runtime()
        .help_text(&snap.settings.container_prefix, &image, &run_args)
        .await?;
    let caps = crate::llama_caps::parse_help(&help);
    let needle = search.map(str::to_ascii_lowercase);
    let keep = |f: &str| {
        needle
            .as_deref()
            .is_none_or(|n| f.to_ascii_lowercase().contains(n))
    };

    let flags: Vec<&String> = caps.flags.iter().filter(|f| keep(f)).collect();
    let enums: Map<String, Value> = caps
        .enums
        .iter()
        .filter(|(f, _)| keep(f))
        .map(|(f, v)| (f.clone(), json!(v)))
        .collect();
    let removed: Vec<&String> = caps.removed.iter().filter(|f| keep(f)).collect();
    Ok(json!({
        "image": image,
        "flag_count": caps.flags.len(),
        "flags": flags,
        "allowed_values": enums,
        "removed": removed,
        "note": "The vocabulary is the image's, not lmgw's: a model on a different \
                 image can accept different flags — pass model=<id> to ask about that \
                 one. Flags with a dedicated lmgw__local_model_set field should be set \
                 there, not through extra_args.",
    }))
}

/// `model_or_image` → an image reference, and the `podman run` args its
/// `--help` read runs with. A configured model id wins over an image
/// spelling, because "qwen3" is a model id and never a pullable image, and
/// brings that model's resolved args; anything unrecognised is passed through
/// as an image so a caller can ask about one they have not deployed yet, with
/// the chat class's args (the ones a model on it would inherit).
///
/// An **image-class** model id is refused rather than resolved: its container
/// runs sd-server, whose `--help` this verb's parser
/// ([`crate::llama_caps`]) would read as llama-server flags and answer with a
/// vocabulary that is wrong in every line. Falling through to "treat it as an
/// image reference" was no better — it pulls `z-image-turbo` as if it were a
/// registry name.
fn resolve_flag_image(
    snap: &Snapshot,
    model_or_image: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let class_args = || snap.settings.router.extra_run_args.clone();
    let Some(arg) = model_or_image.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok((snap.settings.router.image.clone(), class_args()));
    };
    for class in [Class::Chat, Class::Aux, Class::Audio] {
        if let Some(rt) = model_runtime(snap, class, arg) {
            return Ok((rt.image, rt.extra_run_args));
        }
    }
    if snap.image_models.iter().any(|m| m.model_id == arg) {
        return Err(format!(
            "'{arg}' is an image (stable-diffusion.cpp) model, and this verb reads \
             llama-server's flag vocabulary — sd-server's `--help` is a different language. \
             Use lmgw__local_model_get model_id={arg} target=image for the row, or the image \
             editor on the Models page; its keys are checked against that image's own --help \
             when the row is saved."
        ));
    }
    Ok((arg.to_string(), class_args()))
}
