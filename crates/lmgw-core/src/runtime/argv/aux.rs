//! The aux class's own flags: the kind flag (+ pooling for embedders), and the
//! fields an aux row carries beside it (decision-models design §3.3, §3.5).
//!
//! Every flag rendered from a field is claimed, so [`super::push_freeform_args`]
//! drops a stale copy of it from the row's freeform args. A field that is unset
//! claims nothing: a row whose placement still rides in its args
//! (`--n-gpu-layers 0`, `--threads 8`) renders exactly what it rendered before
//! the field existed, and boot adoption keeps its container.

use std::collections::BTreeSet;

use crate::config::{AuxBackend, AuxKind};

use super::mount_path;

/// One aux row's flag inputs, borrowed out of [`super::LlamaArgs::Aux`].
pub(super) struct AuxFlags<'a> {
    pub kind: AuxKind,
    pub pooling: Option<&'a str>,
    pub ctx_size: Option<i64>,
    pub ubatch_size: Option<i64>,
    pub batch_size: Option<i64>,
    pub mmproj_path: Option<&'a str>,
    pub backend: Option<AuxBackend>,
    pub threads: Option<i64>,
}

/// Emit the aux kind flag (+ pooling for embedders), mirroring the deleted
/// `render_aux_preset`'s claims exactly, then the row's fields.
pub(super) fn push_aux_flags(out: &mut Vec<String>, f: AuxFlags<'_>, taken: &mut BTreeSet<String>) {
    // Both kind flags are claimed in every section, not just this section's
    // own: a stale `--reranking` left in an embedder's freeform args would
    // otherwise flip it into a reranker, and llama-server answers
    // `/v1/embeddings` against a reranker with HTTP 200 and an all-zero
    // vector (spike-verified) — a corpus of noise that looks healthy.
    taken.insert("embeddings".to_string());
    taken.insert("reranking".to_string());
    match f.kind {
        AuxKind::Embed => {
            out.push("--embeddings".to_string());
            if let Some(p) = f.pooling.filter(|p| !p.is_empty()) {
                out.push("--pooling".to_string());
                out.push(p.to_string());
                taken.insert("pooling".to_string());
            }
        }
        AuxKind::Rerank => {
            out.push("--reranking".to_string());
            // `--reranking` selects rank pooling internally; a `--pooling`
            // flag from *any* source breaks reranking outright (llama-server
            // quirk, spike-verified). Claimed unconditionally — even a
            // structured `pooling` value on a rerank row (which should never
            // be set, but nothing stops it structurally) must not leak
            // through, and neither may a freeform `--pooling`.
            taken.insert("pooling".to_string());
        }
        // No kind flag (§3.5): llama-server turns on embedding mode itself
        // for the decision types that read embeddings, and `--reranking`
        // crashes it at load (Appendix A.10). Both kind flags are claimed
        // above and `--pooling` here, so neither a stale copy in the freeform
        // args nor a structured `pooling` reaches the decision head.
        AuxKind::Decision => {
            taken.insert("pooling".to_string());
        }
    }
    let mut value = |key: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("--{key}"));
            out.push(v);
            taken.insert(key.to_string());
        }
    };
    value("ctx-size", f.ctx_size.map(|c| c.to_string()));
    value("ubatch-size", f.ubatch_size.map(|n| n.to_string()));
    value("batch-size", f.batch_size.map(|n| n.to_string()));
    let mmproj = f.mmproj_path.filter(|p| !p.is_empty()).map(mount_path);
    let projector = mmproj.is_some();
    value("mmproj", mmproj);
    value(
        "n-gpu-layers",
        f.backend.map(|b| match b {
            AuxBackend::Cpu => "0".to_string(),
        }),
    );
    value("threads", f.threads.map(|n| n.to_string()));
    // A projector named by the field wins over a freeform `--no-mmproj`, as
    // the chat renderer has it.
    if projector {
        taken.insert("no-mmproj".to_string());
    }
}
