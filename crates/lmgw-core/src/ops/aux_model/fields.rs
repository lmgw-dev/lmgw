//! `aux_model_set`'s five row fields (decision-models design §3.3) —
//! `ubatch_size`, `batch_size`, `mmproj_path`, `backend` and `threads` —
//! what a patch may set them to, and what a save and a read say about them;
//! and the decision kind's flag refusals (§3.5).

use crate::config::{AuxBackend, AuxKind, AuxModel};
use crate::gguf::ModelSummary;
use crate::host::HostCpu;
use crate::runtime::argv::canonical_key;
use crate::runtime::aux::{self as aux_rt, CountFrom};

use super::super::{check_rel_path, clear_has, strip_models_prefix};
use super::AuxModelPatch;

/// The five fields after a patch, over the current row's (`None` on create).
pub(super) struct Fields {
    pub ubatch_size: Option<i64>,
    pub batch_size: Option<i64>,
    pub mmproj_path: Option<String>,
    pub backend: Option<AuxBackend>,
    pub threads: Option<i64>,
}

/// Each field of `p` through its rule below, a name in `p.clear` unsetting
/// it, over `cur`'s value where `p` leaves it out.
pub(super) fn patched(p: &AuxModelPatch, cur: Option<&AuxModel>) -> Result<Fields, String> {
    let cleared = |f: &str| clear_has(p.clear.as_deref(), f);
    Ok(Fields {
        ubatch_size: count(
            "ubatch_size",
            p.ubatch_size,
            cleared("ubatch_size"),
            cur.and_then(|c| c.ubatch_size),
        )?,
        batch_size: count(
            "batch_size",
            p.batch_size,
            cleared("batch_size"),
            cur.and_then(|c| c.batch_size),
        )?,
        mmproj_path: mmproj_path(
            p.mmproj_path.as_deref(),
            cleared("mmproj_path"),
            cur.and_then(|c| c.mmproj_path.clone()),
        )?,
        backend: backend(
            p.backend.as_deref(),
            cleared("backend"),
            cur.and_then(|c| c.backend),
        )?,
        threads: count(
            "threads",
            p.threads,
            cleared("threads"),
            cur.and_then(|c| c.threads),
        )?,
    })
}

/// The row's `backend` after a patch: `value` when supplied (blank runs on
/// the GPU), else `current`; `cleared` (named in `clear`) runs on the GPU
/// too. Only `cpu` is accepted: the GPU backend is the image's, not the
/// row's.
pub(super) fn backend(
    value: Option<&str>,
    cleared: bool,
    current: Option<AuxBackend>,
) -> Result<Option<AuxBackend>, String> {
    if cleared {
        return Ok(None);
    }
    let Some(v) = value.map(str::trim) else {
        return Ok(current);
    };
    match v {
        "" => Ok(None),
        v => AuxBackend::parse(v).map(Some).ok_or_else(|| {
            format!(
                "backend: '{v}' is not a backend an aux row can set — only 'cpu' runs the row on \
                 the CPU; leave it out (or clear it) to run on the GPU, which backend the image \
                 decides"
            )
        }),
    }
}

/// A count field (`ubatch_size`, `batch_size`, `threads`) after a patch:
/// `value` when supplied, else `current`; `cleared` takes llama-server's
/// default. A count is positive, with no upper cap.
pub(super) fn count(
    field: &str,
    value: Option<i64>,
    cleared: bool,
    current: Option<i64>,
) -> Result<Option<i64>, String> {
    if cleared {
        return Ok(None);
    }
    match value {
        Some(n) if n <= 0 => Err(format!(
            "{field}: {n} is not a count — give 1 or more, or clear it to take llama-server's \
             default"
        )),
        Some(n) => Ok(Some(n)),
        None => Ok(current),
    }
}

/// The row's `mmproj_path` after a patch: `value` when supplied (blank is
/// no projector), else `current`; `cleared` is no projector. Stored relative
/// to the aux models dir, like `gguf_path`: the in-container `/models/`
/// prefix a caller copies off a command line is dropped.
pub(super) fn mmproj_path(
    value: Option<&str>,
    cleared: bool,
    current: Option<String>,
) -> Result<Option<String>, String> {
    if cleared {
        return Ok(None);
    }
    let Some(v) = value.map(str::trim) else {
        return Ok(current);
    };
    if v.is_empty() {
        return Ok(None);
    }
    let rel = strip_models_prefix(v);
    check_rel_path(&rel, "aux models dir").map_err(|e| format!("mmproj_path: {e}"))?;
    Ok(Some(rel))
}

/// What a decision row may not carry (§3.5): a pooling, or a kind flag or
/// `--pooling` in its freeform args. The renderer claims all of them, so none
/// would reach the server; this is the same refusal said out loud, because
/// the one that would matters most — `--reranking` crashes llama-server at
/// load on a decision model (Appendix A.10).
pub(super) fn check_decision_flags(pooling: Option<&str>, args: &[String]) -> Result<(), String> {
    if pooling.is_some_and(|p| !p.is_empty()) {
        return Err(
            "decision models take no pooling — llama-server reads the decision head the file \
             declares, and lmgw renders no --pooling for them"
                .into(),
        );
    }
    let named = |a: &String| {
        let key = a.trim_start_matches('-').split('=').next().unwrap_or("");
        a.starts_with('-')
            && matches!(
                key,
                "embedding" | "embeddings" | "reranking" | "rerank" | "pooling"
            )
    };
    if let Some(flag) = args.iter().find(|a| named(a)) {
        return Err(format!(
            "remove {flag} from the extra flags: a decision model takes no kind flag and no \
             pooling — llama-server turns on what it needs itself, and --reranking crashes it \
             at load"
        ));
    }
    Ok(())
}

/// The freeform flag each field renders, by the long name the alias table
/// maps every spelling to (`runtime::argv::canonical_key`: `-ub`, `-mm`,
/// `-ngl`, `--gpu-layers`, `-t`, …), and how a finding names the field.
const FIELD_FLAGS: &[(&str, &str)] = &[
    ("ubatch-size", "ubatch_size"),
    ("batch-size", "batch_size"),
    ("mmproj", "mmproj_path"),
    ("n-gpu-layers", "backend"),
    ("threads", "threads"),
];

/// Whether `m`'s field named `field` is set.
fn field_set(m: &AuxModel, field: &str) -> bool {
    match field {
        "ubatch_size" => m.ubatch_size.is_some(),
        "batch_size" => m.batch_size.is_some(),
        "mmproj_path" => m.mmproj_path.is_some(),
        "backend" => m.backend.is_some(),
        "threads" => m.threads.is_some(),
        _ => false,
    }
}

/// Advisory findings on the freeform args against the fields: a flag the
/// row's field renders is dropped from the args, and a flag whose field is
/// unset still renders but has a field now — which is what placement, the
/// GPU hold and the VRAM charge read.
pub(super) fn arg_warnings(m: &AuxModel) -> Vec<String> {
    let mut out = Vec::new();
    for tok in m.args.iter().filter(|t| t.starts_with('-')) {
        let flag = tok.split('=').next().unwrap_or(tok);
        let key = canonical_key(flag.trim_start_matches('-'));
        let Some((_, field)) = FIELD_FLAGS.iter().find(|(long, _)| *long == key) else {
            continue;
        };
        out.push(if field_set(m, field) {
            format!(
                "`{flag}` is rendered from the row's {field} field — passed as an extra arg it \
                 is ignored"
            )
        } else if *field == "backend" {
            // Decision-models design §3.3: on a CUDA image with the device,
            // `-ngl 0` still keeps compute buffers on the card and batches
            // far faster than without it, so this names the trade, not a fix.
            format!(
                "`{flag}` in the extra args still renders, but the GPU hold reads the row's \
                 backend field: with backend unset this row is a GPU row for the hold, and on a \
                 GPU image its container keeps the device for compute buffers. backend=cpu \
                 runs it without the device and serves it under the hold, at CPU batch speed"
            )
        } else {
            format!(
                "`{flag}` in the extra args still renders, but it has a field now — set \
                 {field} instead"
            )
        });
    }
    out
}

/// Configuration the fields render cleanly but cannot serve as intended,
/// from the row and the header of its weights (`summary`, when it could be
/// read) — part of `aux_model_problems`.
pub(super) fn problems(
    m: &AuxModel,
    root: &std::path::Path,
    summary: Option<&ModelSummary>,
    projector: Option<&ModelSummary>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(mm) = &m.mmproj_path {
        if !root.join(mm).is_file() {
            out.push(format!(
                "mmproj_path '{mm}' is not a file under the aux models dir"
            ));
        } else if projector.is_some_and(|p| !p.is_mmproj) {
            out.push(format!(
                "mmproj_path '{mm}' is not a projector (its header declares no clip projector)"
            ));
        }
    }
    // llama.cpp lowers the ubatch to the batch, so a ubatch above it is
    // silently not the ubatch the row runs; 2048 is llama.cpp's own default
    // batch. Embedding mode lowers the batch to the ubatch the other way.
    // Both are the values in effect: the field, else a copy in the freeform
    // args, which renders while the field is unset.
    if let Some((ub, _)) = aux_rt::ubatch_in_effect(m) {
        let (batch, from) = match aux_rt::batch_in_effect(m) {
            Some((n, CountFrom::Field)) => (n, ""),
            Some((n, CountFrom::Args)) => (n, ", from the extra args"),
            None => (aux_rt::LLAMA_DEFAULT_BATCH, ", llama.cpp's default"),
        };
        if ub > batch {
            out.push(format!(
                "the ubatch ({ub}) exceeds the batch ({batch}{from}) — llama.cpp lowers the \
                 ubatch to the batch, so set batch_size to at least {ub}"
            ));
        }
    }
    if aux_rt::own_args_pass_gpu(m) {
        out.push(aux_rt::OWN_ARGS_PASS_GPU.to_string());
    }
    let Some(s) = summary else {
        return out;
    };
    match (m.kind, s.decision_type.as_deref()) {
        (AuxKind::Decision, None) => out.push(format!(
            "'{}' declares no decision.type in its header — it is not a decision model, and \
             llama-server answers /v1/systemone for it with 501",
            m.gguf_path
        )),
        (AuxKind::Decision, Some(_)) => {
            if !s.has_systemone_template {
                out.push(format!(
                    "'{}' carries no tokenizer.chat_template.systemone — llama-server refuses \
                     to load a decision model without it",
                    m.gguf_path
                ));
            }
            // §3.5: a non-causal decision model reads no images.
            if m.mmproj_path.is_some() && s.attention_causal == Some(false) {
                out.push(format!(
                    "'{}' is a non-causal decision model (attention.causal = false), which \
                     reads no images — drop mmproj_path",
                    m.gguf_path
                ));
            }
        }
        (AuxKind::Embed | AuxKind::Rerank, Some(t)) => out.push(format!(
            "'{}' is a decision model (decision.type '{t}') — set kind=decision; served as \
             '{}' it does not answer, and with --reranking llama-server crashes at load",
            m.gguf_path,
            m.kind.as_str()
        )),
        (AuxKind::Embed | AuxKind::Rerank, None) => {}
    }
    out
}

/// What a save says about a thread count above this machine's online CPUs:
/// it is saved as given (llama-server runs that many threads, and they
/// contend for the cores), but the owner should know.
pub(super) fn threads_note(threads: Option<i64>, host: HostCpu) -> Option<String> {
    let n = threads?;
    let logical = i64::try_from(host.logical_cpus).unwrap_or(i64::MAX);
    (n > logical).then(|| {
        format!(
            "threads {n} is more than this machine's {} online CPUs ({} physical cores) — saved \
             as given; the threads beyond them only contend for the same cores",
            host.logical_cpus, host.physical_cores
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_takes_cpu_or_the_gpu_and_nothing_else() {
        let cpu = Some(AuxBackend::Cpu);
        assert_eq!(backend(Some("cpu"), false, None), Ok(cpu));
        assert_eq!(backend(Some(" cpu "), false, None), Ok(cpu));
        assert_eq!(backend(Some(""), false, cpu), Ok(None));
        assert_eq!(backend(None, false, cpu), Ok(cpu));
        assert_eq!(backend(Some("cpu"), true, None), Ok(None), "clear wins");
        let e = backend(Some("cuda"), false, None).unwrap_err();
        assert!(e.contains("'cuda'") && e.contains("only 'cpu'"), "{e}");
    }

    #[test]
    fn a_count_is_positive_with_no_upper_cap() {
        assert_eq!(count("threads", Some(8), false, None), Ok(Some(8)));
        assert_eq!(count("threads", None, false, Some(8)), Ok(Some(8)));
        assert_eq!(count("threads", Some(8), true, Some(4)), Ok(None));
        assert!(count("ubatch_size", Some(0), false, None).is_err());
        assert!(count("batch_size", Some(-1), false, None).is_err());
        assert_eq!(
            count("ubatch_size", Some(262_144), false, None),
            Ok(Some(262_144))
        );
    }

    #[test]
    fn a_projector_path_is_stored_relative_to_the_aux_dir() {
        assert_eq!(
            mmproj_path(Some("/models/org/m/mmproj.gguf"), false, None),
            Ok(Some("org/m/mmproj.gguf".into()))
        );
        assert_eq!(mmproj_path(Some(" "), false, Some("x".into())), Ok(None));
        assert_eq!(
            mmproj_path(None, false, Some("x".into())),
            Ok(Some("x".into()))
        );
        assert_eq!(mmproj_path(Some("y"), true, None), Ok(None));
        assert!(mmproj_path(Some("../etc/passwd"), false, None).is_err());
    }

    #[test]
    fn a_decision_row_refuses_pooling_and_every_kind_flag() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(check_decision_flags(None, &[]).is_ok());
        assert!(check_decision_flags(Some(""), &args(&["--threads", "8"])).is_ok());
        assert!(check_decision_flags(Some("cls"), &[]).is_err());
        for flag in [
            "--reranking",
            "--rerank",
            "--embedding",
            "--embeddings",
            "--pooling=cls",
        ] {
            let e = check_decision_flags(None, &args(&[flag])).unwrap_err();
            assert!(e.contains(flag), "{e}");
        }
    }
}
