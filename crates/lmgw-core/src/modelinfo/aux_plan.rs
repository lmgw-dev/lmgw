//! The aux class's half of `local_model_plan`: the plan for an embedding,
//! rerank or decision GGUF, read from its header, and its comparison with an
//! aux row already serving that file (`configured_as`). The decision kind's
//! own planning rules are [`super::decision`]'s; the ubatch of an encoder
//! whose inputs llama-server cannot split is [`super::aux_batch`]'s.

use serde_json::{json, Map, Value};

use crate::config::{AuxKind, Snapshot};
use crate::gguf::{self, ModelSummary};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store;

use super::{
    aux_batch, decision, image_attention, models_dir_of, plan_fit, projector_floor, role_of,
    suggest_model_id, summarize,
};

/// Why an embed or rerank plan's `ctx_size` is the trained context `c`, and
/// that it bounds every input: llama-server refuses a longer one. `split`
/// says whether the plan also set `ubatch_size` = `batch_size` = `c`, because
/// llama-server evaluates a whole input of this model in one ubatch
/// ([`aux_batch`]), or it splits an input across batches (a last-token
/// embedder, a causal reranker) and the context alone bounds it — what
/// `knowledge::limit` checks a chunk against — or the row's pooling decides.
fn ctx_rationale(c: u64, kind: AuxKind, split: &aux_batch::Split) -> String {
    let refuses = "llama-server refuses an input longer than this with an error and does not \
                   truncate it, so a smaller ctx_size lowers what one input may hold";
    match split {
        aux_batch::Split::OneUbatch(_) => format!(
            "the model's trained context ({c}) — its real ceiling per input, with ubatch_size \
             and batch_size planned at it too, since llama-server evaluates a whole input of \
             this model in one ubatch; {refuses}"
        ),
        aux_batch::Split::DependsOnPooling => format!(
            "the model's trained context ({c}) — its real ceiling per input when llama-server \
             splits an input across batches, which it does with 'last' pooling; with any other \
             the ubatch bounds an input (see the warnings); {refuses}"
        ),
        aux_batch::Split::Splits if kind == AuxKind::Rerank => format!(
            "the model's trained context ({c}) — its real ceiling, since llama-server splits a \
             causal reranker's input across batches, so llama.cpp's default ubatch and batch \
             are kept. That split arrived in llama.cpp b{} (2026-09-27); an older image \
             evaluates a rerank input in one ubatch, so there set ubatch_size and batch_size \
             to ctx_size; {refuses}",
            crate::runtime::aux::CAUSAL_RERANK_SPLIT_BUILD
        ),
        aux_batch::Split::Splits => format!(
            "the model's trained context ({c}) — its real ceiling, since llama-server splits a \
             last-token pooling embedder's input across batches, so llama.cpp's default ubatch \
             and batch are kept; {refuses}"
        ),
    }
}

/// The plan under construction: each planned field with its reason.
#[derive(Default)]
struct Planned {
    params: Map<String, Value>,
    why: Map<String, Value>,
}

impl Planned {
    fn put(&mut self, k: &str, v: Value, reason: String) {
        self.params.insert(k.to_string(), v);
        self.why.insert(k.to_string(), Value::String(reason));
    }

    fn int(&self, k: &str) -> Option<i64> {
        self.params.get(k).and_then(Value::as_i64)
    }
}

/// The aux-class plan: what `lmgw__aux_model_set action=create` takes for an
/// embedding, rerank or decision GGUF, read from its header.
///
/// `found_in` is the directory the file was resolved against. An aux row can
/// only address the aux models dir, so a file found in the chat dir gets the
/// parameters *and* a warning that says where the file has to be — the exact
/// trap that had an agent hard-linking GGUFs between the two trees.
#[allow(clippy::too_many_arguments)]
pub(super) async fn aux_plan(
    state: &SharedState,
    snap: &Snapshot,
    found_in: Class,
    dir: &str,
    rel: &str,
    s: &ModelSummary,
    kind: Option<AuxKind>,
) -> Result<Value, String> {
    let mut warnings: Vec<String> = Vec::new();
    let kind = kind.unwrap_or_else(|| {
        warnings.push(format!(
            "{rel} looks like a chat model (no pooling_type or classifier head in its \
             header{}) — served with --embeddings it pools its hidden states, which works \
             but is not what a trained embedder produces; fine only if that is the intent",
            if s.has_chat_template {
                ", and a chat template"
            } else {
                ""
            }
        ));
        AuxKind::Embed
    });

    let mut plan = Planned::default();
    plan.put(
        "gguf_path",
        json!(rel),
        "the file this plan was made for".into(),
    );
    let pooling = s.pooling_type.and_then(gguf::pooling_name);
    // The decision branch plans its own ctx, ubatch, batch and placement
    // (decision-models design §3.4); its shape says whether a projector
    // belongs on the row.
    let mut decision_shape = None;
    match kind {
        AuxKind::Rerank => plan.put(
            "kind",
            json!("rerank"),
            if s.has_classifier_head {
                "the header carries a classifier head (cls.output), a cross-encoder that \
                 scores query/document pairs — served with --reranking on /v1/rerank"
                    .into()
            } else {
                "the header declares pooling_type 'rank' — served with --reranking on \
                 /v1/rerank; never set pooling on a rerank model"
                    .into()
            },
        ),
        AuxKind::Embed => {
            plan.put(
                "kind",
                json!("embed"),
                match pooling {
                    Some(p) => format!(
                        "the header declares pooling_type '{p}': an encoder with no LM head, \
                         served with --embeddings on /v1/embeddings"
                    ),
                    None => "an encoder architecture, served with --embeddings on \
                             /v1/embeddings"
                        .into(),
                },
            );
            match pooling {
                Some(p) if p != "none" => plan.put(
                    "pooling",
                    json!(p),
                    format!(
                        "the pooling the converter recorded ('{p}'); the model card's vectors \
                         assume it, and a different one retrieves noise while looking fine"
                    ),
                ),
                Some(_) => warnings.push(
                    "the header declares pooling 'none' — leave pooling unset and llama-server \
                     picks the model's default"
                        .into(),
                ),
                None => warnings.push(
                    "the header declares no pooling_type — leave pooling unset unless the \
                     model card names one (mean for most BERT-style encoders)"
                        .into(),
                ),
            }
        }
        // Decision-models design §3.2, §3.4.
        AuxKind::Decision => {
            decision_shape = Some(decision::plan(
                s,
                rel,
                &snap.settings,
                &mut |k: &str, v: Value, r: String| plan.put(k, v, r),
                &mut warnings,
            ))
        }
    }
    // Whether the ubatch and batch below are the encoder rule's.
    let mut one_ubatch = false;
    if decision_shape.is_none() {
        // Owner item a (decision-models design §7): an embedder or reranker
        // whose inputs llama-server cannot split gets its context as ubatch
        // and batch, as a non-causal decision model does.
        let split = aux_batch::split_of(s, kind, pooling);
        match s.context_length {
            Some(c) => plan.put("ctx_size", json!(c), ctx_rationale(c, kind, &split)),
            None => warnings.push(
                "the GGUF declares no context length; leave ctx_size unset to take llama.cpp's \
                 default"
                    .into(),
            ),
        }
        one_ubatch = aux_batch::plan(
            s,
            kind,
            pooling,
            &mut |k: &str, v: Value, r: String| plan.put(k, v, r),
            &mut warnings,
        );
    }

    // A sibling projector (multimodal embedders such as Qwen3-VL-Embedding,
    // image-capable decision models such as Clef): the row's `mmproj_path`,
    // relative to the aux models dir like `gguf_path` (decision-models design
    // §3.3).
    let parent = rel
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let siblings = {
        let dir = dir.to_string();
        tokio::task::spawn_blocking(move || crate::hf::scan_gguf_files(&dir))
            .await
            .map_err(|e| format!("scan panicked: {e}"))?
    };
    for cand in siblings {
        if cand == rel || cand.rsplit_once('/').map(|(d, _)| d).unwrap_or("") != parent {
            continue;
        }
        let Ok(cs) = summarize(std::path::Path::new(dir).join(&cand)).await else {
            continue;
        };
        if role_of(&cs) == "mmproj" {
            // Never on a non-causal decision type (decision-models design §3.4).
            if let Some(w) = decision_shape.and_then(|d| d.unproposed_projector(&cand)) {
                warnings.push(w);
                break;
            }
            plan.put(
                "mmproj_path",
                json!(cand),
                format!(
                    "sibling projector ({}), so the model can read images; it is loaded \
                     beside the weights and charged with them",
                    cs.projector_type.as_deref().unwrap_or("vision")
                ),
            );
            // The chat plan's rule: a projector whose images llama.cpp
            // decodes non-causally needs a whole image in one ubatch.
            let image = image_attention(Some(&cs), s.embedding_length);
            let planned = (plan.int("ubatch_size"), plan.int("batch_size"));
            projector_floor::plan(
                Some(&image),
                planned,
                &mut |k: &str, v: Value, r: String| plan.put(k, v, r),
            );
            break;
        }
    }

    // The location rule, said before anything else the caller might act on.
    let mut apply_ready = true;
    if found_in != Class::Aux {
        apply_ready = false;
        let aux_dir = models_dir_of(snap, Class::Aux);
        let in_aux =
            !aux_dir.trim().is_empty() && std::path::Path::new(&aux_dir).join(rel).is_file();
        let downloads = store::list_hf_models(&state.db).await.unwrap_or_default();
        let origin = downloads
            .iter()
            .find(|d| d.dest_path == rel && d.target == found_in.as_str());
        let fetch = match origin {
            Some(d) => format!(
                "lmgw__hf_add repo={} file={} target=aux (tracked there and updatable by \
                 lmgw__hf_set check_updates)",
                d.repo, d.file
            ),
            None => "lmgw__hf_add … target=aux (tracked and updatable), or move the file into \
                     the aux models dir"
                .to_string(),
        };
        warnings.push(format!(
            "{rel} is a {} model, which the aux class serves — but it was found in the {} \
             models dir, and an aux row can only address files under the aux models dir \
             ({}). {} Do not hard-link or copy it across, and do not create a chat-class \
             row for it: a chat row cannot embed, and the file would fall out of the \
             updater's view.",
            kind.as_str(),
            found_in.as_str(),
            if aux_dir.trim().is_empty() {
                "not configured — Settings → Runtimes → Aux".to_string()
            } else {
                aux_dir.clone()
            },
            if in_aux {
                "The same path already exists there: plan again with target=aux and create \
                 the row from that."
                    .to_string()
            } else {
                format!("Get it there with {fetch}, then plan again with target=aux.")
            }
        ));
    }

    // Owner finding 3: a GPU plan that cannot fit the card says so, with the
    // largest context that does; the trained context stays the plan.
    if let Some(w) = fit_warning(state, snap, dir, rel, s, &plan).await {
        warnings.push(w);
    }
    let Planned { params, why } = plan;

    // Already configured as an aux model? Then this is a repair, and the
    // delta is what the caller needs — same contract as the chat plan.
    let configured: Vec<Value> = if found_in == Class::Aux {
        snap.aux_models
            .iter()
            .filter(|m| m.gguf_path == rel)
            .map(|row| aux_plan_diff(row, s, one_ubatch, &params, &why, decision_shape))
            .collect()
    } else {
        Vec::new()
    };
    let repairs = configured
        .iter()
        .filter(|c| c["in_sync"] == Value::Bool(false))
        .count();
    let model_id = suggest_model_id(rel);
    let next_step = if !apply_ready {
        "get the file into the aux models dir (see warnings), then run lmgw__local_model_plan \
         again with target=aux"
            .to_string()
    } else if configured.is_empty() {
        format!(
            "pass these to lmgw__aux_model_set action=create (adding model_id, e.g. \
             '{model_id}'), then lmgw__local_model_test model_id=<id> target=aux — the test \
             {}, since this model cannot generate",
            match kind {
                AuxKind::Embed => "embeds a probe input",
                AuxKind::Rerank => "reranks two probe documents",
                AuxKind::Decision => "asks one System One choice question on /v1/systemone",
            }
        )
    } else if repairs == 0 {
        "every aux model already using this GGUF matches this plan; nothing to do".to_string()
    } else {
        format!(
            "this GGUF is already configured — pass the ready-made configured_as[].apply \
             object ({repairs} of them) to lmgw__aux_model_set, then lmgw__local_model_test \
             with target=aux"
        )
    };

    Ok(json!({
        "class": "aux",
        "target": found_in.as_str(),
        "apply_ready": apply_ready,
        "model_id": model_id,
        "params": params,
        "rationale": why,
        "warnings": warnings,
        "configured_as": configured,
        "configured_note": "Aux models already serving this GGUF, and how they differ from \
                            the plan. Placement (backend, threads) is never compared \
                            here, so a deliberate CPU or GPU variant is not a difference.",
        "next_step": next_step,
    }))
}

/// Owner finding 3 for an aux plan: the planned row's footprint on the GPU,
/// as admission will charge it ([`crate::vram::plan::aux`]), against the card
/// ([`plan_fit`]). `None` for a row planned on the CPU, while admission has
/// nothing to measure against, and when it fits.
async fn fit_warning(
    state: &SharedState,
    snap: &Snapshot,
    dir: &str,
    rel: &str,
    s: &ModelSummary,
    plan: &Planned,
) -> Option<String> {
    use crate::vram::plan::aux::{footprint, Batches, Placement};
    use crate::vram::plan::GgufFiles;

    if plan.params.get("backend").and_then(Value::as_str) == Some("cpu") {
        return None;
    }
    let card = plan_fit::Card::of(state, snap).await?;
    let ctx = plan
        .int("ctx_size")
        .and_then(|c| u64::try_from(c).ok())
        .or(s.context_length)?;
    let companion_bytes = match plan.params.get("mmproj_path").and_then(Value::as_str) {
        Some(p) => tokio::fs::metadata(std::path::Path::new(dir).join(p))
            .await
            .map(|m| m.len())
            .unwrap_or(0),
        None => 0,
    };
    let files = GgufFiles {
        main_bytes: s.file_size,
        companion_bytes,
        summary: Ok(s.clone()),
    };
    let (ubatch, batch) = (plan.int("ubatch_size"), plan.int("batch_size"));
    // A plan that ties the ubatch to the context (a whole input in one
    // ubatch) keeps the tie at a smaller context.
    let tied = ubatch.is_some() && ubatch == plan.int("ctx_size");
    let count = |n: Option<i64>, default: i64| n.unwrap_or(default).unsigned_abs();
    let at = |c: u64| {
        let batches = Batches {
            ubatch: if tied {
                c
            } else {
                count(ubatch, crate::runtime::aux::LLAMA_DEFAULT_UBATCH)
            },
            batch: if tied {
                c
            } else {
                count(batch, crate::runtime::aux::LLAMA_DEFAULT_BATCH)
            },
            flash_attn_off: false,
        };
        // The plan renders no cache type, so the KV cache is f16.
        let ctx = i64::try_from(c).unwrap_or(i64::MAX);
        footprint(&files, rel, Some(ctx), (16, 16), Placement::Gpu, batches)
    };
    plan_fit::over_card(
        &card,
        ctx,
        "run it on the CPU (backend=cpu), where it takes no VRAM",
        at,
    )
}

/// The delta between an aux plan and a configured aux row on the same GGUF,
/// as a patch for `lmgw__aux_model_set`. Only the fields the plan proposes
/// are compared; a projector the row already names in its args (`--mmproj`
/// or `-mm`, as [`crate::capabilities::aux_projector`] reads it) counts as
/// named, since those args still render it. `decision` is the plan's
/// decision shape, whose tuning the owner may take further than the plan
/// ([`decision::within_tuning`]).
///
/// `one_ubatch` says the plan's ubatch and batch are [`aux_batch`]'s (the
/// context, for an input in one ubatch). They are no difference for an
/// embed or rerank row that splits its input after all, by its own pooling
/// or else the header's ([`crate::runtime::aux::splits_input`]): a row set
/// to `last` on a header without a pooling would otherwise be offered a
/// ubatch the size of its context, and charged its logits at that size
/// (review finding 1).
fn aux_plan_diff(
    row: &crate::config::AuxModel,
    s: &ModelSummary,
    one_ubatch: bool,
    params: &serde_json::Map<String, Value>,
    why: &serde_json::Map<String, Value>,
    decision: Option<decision::Shape>,
) -> Value {
    let row_splits = row.kind != AuxKind::Decision && {
        let pooling = row
            .pooling
            .as_deref()
            .filter(|p| !p.is_empty())
            .or_else(|| s.pooling_type.and_then(gguf::pooling_name));
        crate::runtime::aux::splits_input(row.kind, pooling, s.attention_causal != Some(false))
    };
    let current = |field: &str| -> Value {
        match field {
            "gguf_path" => json!(row.gguf_path),
            "kind" => json!(row.kind.as_str()),
            "pooling" => json!(row.pooling),
            "ctx_size" => json!(row.ctx_size),
            "ubatch_size" => json!(row.ubatch_size),
            "batch_size" => json!(row.batch_size),
            "mmproj_path" => json!(row.mmproj_path),
            _ => Value::Null,
        }
    };
    let mut changes: Vec<Value> = Vec::new();
    let mut patch = serde_json::Map::new();
    for (field, planned) in params {
        // Placement is the owner's to choose: a non-causal decision plan
        // proposes the CPU for a new row, and a row kept on the GPU is a
        // deliberate variant, not a difference.
        if field == "backend" {
            continue;
        }
        let cur = current(field);
        let differs = match field.as_str() {
            "mmproj_path" => crate::capabilities::aux_projector(row).is_none(),
            "ubatch_size" | "batch_size" if one_ubatch && row_splits => false,
            _ if decision
                .filter(|_| row.kind == AuxKind::Decision)
                .is_some_and(|d| decision::within_tuning(d, field, &cur, planned)) =>
            {
                false
            }
            _ => &cur != planned,
        };
        if !differs {
            continue;
        }
        changes.push(json!({
            "field": field,
            "current": cur,
            "planned": planned,
            "why": why.get(field).cloned().unwrap_or(Value::Null),
        }));
        patch.insert(field.clone(), planned.clone());
    }
    let in_sync = changes.is_empty();
    let mut out = serde_json::Map::new();
    out.insert("model_id".into(), json!(row.model_id));
    out.insert("id".into(), json!(row.id));
    out.insert("enabled".into(), json!(row.enabled));
    out.insert("in_sync".into(), json!(in_sync));
    out.insert("change".into(), json!(changes));
    if !in_sync {
        patch.insert("action".into(), json!("update"));
        patch.insert("model_id".into(), json!(row.model_id));
        patch.remove("gguf_path");
        out.insert("apply".into(), Value::Object(patch));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner item a: an encoder that reads a whole input in one ubatch gets
    /// its context as ubatch and batch, and the context is then the ceiling
    /// per input either way.
    #[test]
    fn an_encoder_s_context_is_its_ceiling_with_its_ubatch_planned_at_it() {
        for kind in [AuxKind::Embed, AuxKind::Rerank] {
            let one = aux_batch::Split::OneUbatch("it is non-causal".into());
            let why = ctx_rationale(8192, kind, &one);
            assert!(
                why.contains("its real ceiling per input")
                    && why.contains("ubatch_size and batch_size planned at it"),
                "{kind:?}: {why}"
            );
            assert!(why.contains("does not truncate"), "{why}");
        }
    }

    #[test]
    fn a_split_input_keeps_llama_cpps_batch_defaults() {
        let splits = aux_batch::Split::Splits;
        let why = ctx_rationale(32768, AuxKind::Embed, &splits);
        assert!(
            why.contains("its real ceiling") && why.contains("last-token pooling embedder"),
            "{why}"
        );
        let why = ctx_rationale(40960, AuxKind::Rerank, &splits);
        assert!(why.contains("a causal reranker's"), "{why}");
        assert!(why.contains("default ubatch and batch are kept"), "{why}");
        // Review finding 5: the build that split a causal reranker's input.
        assert!(
            why.contains("b11223") && why.contains("older image"),
            "{why}"
        );
        let why = ctx_rationale(40960, AuxKind::Embed, &aux_batch::Split::DependsOnPooling);
        assert!(why.contains("'last' pooling"), "{why}");
    }
}
