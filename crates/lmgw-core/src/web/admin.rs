//! Config-plane internals shared by the `/api` handlers and [`crate::ops`]:
//! the upstream reachability probe and the on-disk GGUF orphan scan. The
//! chat container's lifecycle left with router mode (§7) — per-model
//! lifecycle is [`crate::ops::container`] over
//! [`crate::runtime::registry`]. The askama pages these grew up next to went
//! away at the P8 cutover; the logic did not.

use crate::config::{self, Protocol, Upstream};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Probe an upstream's model list with its stored credentials. `Ok("")` means
/// reachable; the error carries the upstream's own complaint, truncated.
pub(crate) async fn test_upstream(state: &SharedState, u: &Upstream) -> Result<String, String> {
    // A llama.cpp row's `/props` facts are asked again, now (llama egress
    // §4.2): of its server, and of a router about what its aliases name.
    let aliased: Vec<String> = state
        .snapshot()
        .aliases
        .values()
        .filter(|a| a.enabled && a.upstream_id == u.id)
        .map(|a| a.upstream_model_id.clone())
        .collect();
    state.llama_facts.retest(&state.http, u, aliased);
    let base = u.base();
    let rb = match u.protocol {
        Protocol::Openai | Protocol::LlamaCpp => {
            let mut rb = state.http.get(format!("{base}/models"));
            if let Some(k) = u.api_key.as_deref().filter(|k| !k.is_empty()) {
                rb = rb.bearer_auth(k);
            }
            rb
        }
        Protocol::Anthropic => {
            let b = base.trim_end_matches("/v1");
            let mut rb = state
                .http
                .get(format!("{b}/v1/models"))
                .header("anthropic-version", "2023-06-01");
            if let Some(k) = u.api_key.as_deref().filter(|k| !k.is_empty()) {
                rb = rb.header("x-api-key", k);
            }
            rb
        }
        Protocol::Gemini => {
            let b = base.trim_end_matches("/v1beta");
            let mut rb = state.http.get(format!("{b}/v1beta/models"));
            if let Some(k) = u.api_key.as_deref().filter(|k| !k.is_empty()) {
                rb = rb.header("x-goog-api-key", k);
            }
            rb
        }
    };
    let resp = rb
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(String::new())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "{status}: {}",
            body.chars().take(200).collect::<String>()
        ))
    }
}

// ---------------------------------------------------------------------------
// Local models
// ---------------------------------------------------------------------------

pub struct OrphanView {
    pub gguf_path: String,
    pub suggested_id: String,
    /// `weights | mmproj | drafter | imatrix`: see [`orphan_role`].
    pub role_guess: &'static str,
}

/// On-disk GGUFs no local model references — not as its weights, and not as
/// the projector or drafter it loads beside them (split parts 2+ belong to
/// their part-1 entry, so they don't count as orphans either).
pub(crate) fn orphan_ggufs(files: &[String], models: &[config::LocalModel]) -> Vec<OrphanView> {
    files
        .iter()
        .filter(|f| {
            crate::hf::split_part_name(f.rsplit('/').next().unwrap_or(f))
                .map(|(_, n, _)| n == 1)
                .unwrap_or(true)
        })
        .filter(|f| {
            !models.iter().any(|m| {
                &m.gguf_path == *f
                    || m.params.mmproj_path.as_deref() == Some(f)
                    || m.params.draft_gguf_path.as_deref() == Some(f)
            })
        })
        .map(|f| OrphanView {
            gguf_path: f.clone(),
            suggested_id: crate::hf::suggest_model_id(f),
            role_guess: orphan_role(f),
        })
        .collect()
}

/// What an unreferenced GGUF most likely is, from its name: the guess
/// `/api/gguf-files` gives, plus the one kind that guess files under weights —
/// an importance matrix (`imatrix.gguf`) is quantization input that ships in
/// quant repos, not a model anyone can serve. Only `weights` can be wired up
/// as a model; the others are companions a model's editor attaches.
pub(crate) fn orphan_role(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name.contains("imatrix") {
        "imatrix"
    } else {
        crate::ops::classify_repo_file(path, "chat")
    }
}
