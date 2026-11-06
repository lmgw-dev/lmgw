//! A load test that did not pass (`local_model_test` on a chat or aux
//! row): the error, the container log lines that look like its reason, and
//! what to try. Shared by a start that failed at admission (lmgw's own error,
//! with its code) and a probe that did not answer as it should.

use lmgw_api_types as dto;

use super::{aux_file_on_chat_row, load_failure_hint, models_dir_of, summarize, Probe};
use crate::config::Snapshot;
use crate::runtime::Class;
use crate::state::SharedState;

/// What failed, as the test answers it.
pub(super) struct Failure {
    /// What went wrong, in the gateway's or the model's words.
    pub detail: String,
    /// The gateway's error code, when one of lmgw's own errors said it
    /// ([`crate::error::GatewayError::kind`]): `gpu_setup` names a start the
    /// machine check blamed on this computer (machine-check design D179).
    pub code: Option<String>,
    pub latency_ms: u64,
}

/// The failed test of `model_id` (`class`, sent `probe`): `failure` with the
/// reason's lines from the model's own container log and a hint.
pub(super) async fn failed(
    state: &SharedState,
    snap: &Snapshot,
    class: Class,
    model_id: &str,
    probe: Probe,
    failure: Failure,
) -> dto::ModelTest {
    let Failure {
        detail,
        code,
        latency_ms,
    } = failure;
    // The HTTP error is usually a generic "failed to load"; the reason is in
    // this model's own container log (§3.6's `logs` verb, the same tail
    // `lmgw__container action=logs` reads — there is no shared container log
    // to grep any more, and that is an improvement: these lines are about
    // this model and nothing else).
    let container =
        crate::runtime::container_name(&snap.settings.container_prefix, class, model_id);
    let log_lines: Vec<String> = match state.runtime().logs_tail(&container, 60).await {
        Ok(text) => text
            .lines()
            .filter(|l| {
                let low = l.to_ascii_lowercase();
                ["error", "failed", "unknown model", "exiting"]
                    .iter()
                    .any(|n| low.contains(n))
            })
            .map(|l| l.trim().to_string())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        Err(_) => Vec::new(),
    };

    // A chat row whose file is really an aux model (an encoder, a reranker,
    // a decision model): the classic misfiling, and worth naming over any
    // generic hint, because no amount of parameter tuning makes it a chat
    // model.
    let misfiled = if class == Class::Chat && probe == Probe::Generate {
        let dir = models_dir_of(snap, Class::Chat);
        let rel = snap
            .local_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| m.gguf_path.clone())
            .unwrap_or_default();
        summarize(std::path::Path::new(&dir).join(&rel))
            .await
            .ok()
            .and_then(|s| aux_file_on_chat_row("this GGUF", &s))
    } else {
        None
    };

    let haystack = format!("{detail}\n{}", log_lines.join("\n")).to_ascii_lowercase();
    let hint: &str = if let Some(misfiled) = misfiled.as_deref() {
        misfiled
    } else if let Some(hint) = load_failure_hint(&haystack) {
        hint
    } else if probe == Probe::Embed && haystack.contains("all-zero") {
        "an all-zero vector is what llama-server returns when an embedder is served with \
         --reranking, or when pooling is wrong for the model — check kind and pooling with \
         lmgw__local_model_get, and lmgw__local_model_plan target=aux for what the header says"
    } else if probe == Probe::Embed && haystack.contains("pooling") {
        "llama-server rejected the pooling — lmgw__local_model_plan target=aux reports the \
         value the GGUF header declares"
    } else if probe != Probe::Generate
        && (haystack.contains("not supported") || haystack.contains("only supported"))
    {
        "the server refused this endpoint — the row's kind (embed / rerank / decision) decides which \
         flag it starts with; check it with lmgw__local_model_get"
    } else {
        "see container_log below, and lmgw__local_model_get for the exact command \
         line this model renders to"
    };

    dto::ModelTest::Failed(dto::ModelTestFailed {
        ok: false,
        loaded: false,
        model_id: model_id.to_string(),
        class: class.as_str().to_string(),
        public_name: None,
        probe: probe.as_str().to_string(),
        endpoint: None,
        latency_ms,
        error: detail,
        code,
        container_log: log_lines,
        hint: hint.to_string(),
    })
}
