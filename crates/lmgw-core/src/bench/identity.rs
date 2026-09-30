//! What a run was run on (benchmark design §6): the model file, the build,
//! the settings and their hash. The GPU's half is the sampler's
//! ([`super::Sampler::gpu_identity`]).

use std::collections::BTreeMap;
use std::path::Path;

use lmgw_api_types::bench::{BuildIdentity, ModelIdentity};
use lmgw_api_types::bench_ops::BenchSettings;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::state::SharedState;

/// §6 `build` from the image's reference, ID and labels (§2.2): lmgw's own
/// `dev.lmgw.*` provenance where the image has it, the OCI labels where it
/// does not. `/props` `build_info` is added once the server answers.
pub fn build_identity(
    image_ref: &str,
    image_id: Option<String>,
    labels: BTreeMap<String, String>,
) -> BuildIdentity {
    let get = |k: &str| labels.get(k).filter(|v| !v.trim().is_empty()).cloned();
    BuildIdentity {
        image_ref: image_ref.to_string(),
        image_id,
        engine_slug: get("dev.lmgw.slug"),
        repo: get("dev.lmgw.repo"),
        commit: get("dev.lmgw.base").or_else(|| get("org.opencontainers.image.revision")),
        git_ref: get("dev.lmgw.ref"),
        version: get("org.opencontainers.image.version"),
        build_info: None,
        labels,
    }
}

/// The settings hash (§6): SHA-256 over the canonical JSON of what the hash
/// covers ([`BenchSettings`]' doc, decision 22) — `params`, `args` and
/// `extra_run_args`. Canonical because `serde_json`'s maps are sorted here
/// (no `preserve_order`), so the same values always serialize the same way.
/// Port and container name are not settings at all, so they cannot move it.
pub fn settings_hash(s: &BenchSettings) -> String {
    let canonical = json!({
        "params": s.params,
        "args": s.args,
        "extra_run_args": s.extra_run_args,
    });
    let text = serde_json::to_string(&canonical).unwrap_or_default();
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// §6 `model`: the weights file as it is on disk now — size, mtime and the
/// quant its GGUF header names (`general.file_type`, which every engine
/// agrees on). A file that cannot be read is `Err`, naming it.
pub async fn model_identity(
    state: &SharedState,
    models_dir: &str,
    model_id: &str,
    gguf_path: &str,
    rung: u32,
) -> Result<ModelIdentity, String> {
    let path = Path::new(models_dir).join(gguf_path);
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("the weights file {} cannot be read: {e}", path.display()))?;
    let mtime = meta.modified().ok().map(|t| {
        chrono::DateTime::<chrono::Utc>::from(t)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    });
    let quant = state
        .gguf_cache
        .summarize_cached(&path)
        .await
        .ok()
        .and_then(|s| s.quant.clone());
    Ok(ModelIdentity {
        model_id: model_id.to_string(),
        gguf_path: gguf_path.to_string(),
        gguf_size: meta.len(),
        gguf_mtime: mtime,
        quant,
        rung,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::bench_ops::BenchOverrides;

    fn settings(image: &str, ctx: i64) -> BenchSettings {
        BenchSettings {
            image: image.into(),
            gguf_path: "m.gguf".into(),
            rung: 0,
            params: json!({"ctx_size": ctx, "parallel": 2}),
            args: vec!["--jinja".into()],
            extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
            overrides: BenchOverrides::default(),
        }
    }

    #[test]
    fn the_hash_follows_the_flags_and_nothing_else() {
        let a = settings("localhost/lmgw-llama-server:official-master", 8192);
        // Another build of the same settings is comparable: the image is the
        // build identity, not a setting.
        let b = settings("localhost/lmgw-llama-server:ik-main", 8192);
        assert_eq!(settings_hash(&a), settings_hash(&b));
        // Reaching the same flags through an override is the same settings.
        let mut c = a.clone();
        c.overrides.ctx_size = Some(8192);
        assert_eq!(settings_hash(&a), settings_hash(&c));
        // Another context is not.
        assert_ne!(settings_hash(&a), settings_hash(&settings("x", 16384)));
        assert_eq!(settings_hash(&a).len(), 64);
    }

    #[test]
    fn build_identity_reads_lmgw_and_oci_labels() {
        let mut labels = BTreeMap::new();
        labels.insert("dev.lmgw.slug".into(), "official-master".into());
        labels.insert("dev.lmgw.base".into(), "0c6a6a7abc".into());
        labels.insert("org.opencontainers.image.revision".into(), "zzz".into());
        labels.insert("org.opencontainers.image.version".into(), "b11226".into());
        let b = build_identity("img", Some("abc".into()), labels);
        assert_eq!(b.engine_slug.as_deref(), Some("official-master"));
        assert_eq!(b.commit.as_deref(), Some("0c6a6a7abc"), "lmgw's base wins");
        assert_eq!(b.version.as_deref(), Some("b11226"));

        let mut pulled = BTreeMap::new();
        pulled.insert("org.opencontainers.image.revision".into(), "fff".into());
        let p = build_identity("ghcr.io/ggml-org/llama.cpp:server", None, pulled);
        assert_eq!(p.commit.as_deref(), Some("fff"));
        assert_eq!(p.engine_slug, None);
    }
}
