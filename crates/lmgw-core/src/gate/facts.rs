//! What the gate knows about one **running** chat container — captured when
//! the container was started (or adopted), never re-read from the row
//! (unified-KV design §3.3, second review, finding 1).
//!
//! **Why not the row.** The ledger's arithmetic is the container's: the pool
//! it guards is the `--ctx-size` / `--kv-unified` / `--parallel` llama-server
//! was *launched* with. When the owner edits a busy guarded row — `ctx_size`
//! 65536 → 131072, or `kv_unified` off — `local_model_set` asks
//! `lifecycle::stop_for_apply` to recreate the container, which refuses while
//! requests are in flight (`RuntimeError::Busy`): the container keeps running
//! its old configuration until it is idle. A gate that read the current row
//! would, from the moment of the save, admit against a pool the container does
//! not have — and the first pair of requests that fits only the new numbers
//! overflows the old pool, which aborts **every** in-flight request on that
//! model (design §2.1 fact 3). So the facts live on the registry entry
//! ([`crate::runtime::registry`]), set where the start happens from the very
//! [`ModelRuntime`] the container's argv was rendered from, and reach the gate
//! through [`crate::vram::LocalHold::gate_facts`]. A restart — an apply once
//! the model is idle, the idle reaper, [`crate::vram::LocalHold`]'s
//! dead-container recovery — is a new entry with the new start's facts.
//!
//! **Adoption** (boot reconciliation, design §3.4) records the facts of the
//! row as it is at boot: a container is only adopted when its command line is
//! the one that row renders now (`Registry::adopt` compares the argv), so the
//! row and the running container agree by construction there.
//!
//! **Ladders (phase 3)** record the rung the entry was started with in the
//! same place: a ladder start renders one rung's params, so its facts are that
//! rung's `params` (and its own trained-context read), plus which rung it
//! was. The gate's fit check then reads "the running rung" from here, and a
//! climb is a restart that records the next rung's facts on the new entry —
//! no second source of truth to keep in step with the first.

use std::path::Path;
use std::sync::Arc;

use crate::config::LlamaParams;
use crate::runtime::argv::LlamaArgs;
use crate::runtime::descriptor::{ModelRuntime, RungPos};
use crate::runtime::Class;

use super::count::ProjectorRow;

/// The gate's facts about one running chat container (module doc).
///
/// Holds the start row's own inputs rather than a handful of derived numbers,
/// so every derivation — the guard, the clamp ceiling, the pool, the
/// per-request limit, the per-image bound — is the same method the row itself
/// uses ([`LlamaParams`]), applied to the params the container was launched
/// with, and cannot drift from it.
#[derive(Debug, Clone, PartialEq)]
pub struct GateFacts {
    pub model_id: String,
    /// The chat class's models dir the container mounted at `/models` — what
    /// the start row's relative paths (weights, projector) resolve against.
    pub models_dir: String,
    /// The weights, relative to [`Self::models_dir`].
    pub gguf_path: String,
    /// The start row's params: `kv_unified`, `parallel`, `ctx_size`,
    /// `kv_unified_per_slot`, `n_predict`, the projector.
    pub params: LlamaParams,
    /// The start row's extra flags (`--image-max-tokens`, `--mmproj`).
    pub args: Vec<String>,
    /// The weights' trained context (`<arch>.context_length`), read at start
    /// — one of the terms of the per-request limit (design §3.2), and on a
    /// ladder rung the cap llama-server puts on every slot. `None` when the
    /// header could not be read or does not say, and for a start that is
    /// neither guarded nor a ladder, which never needs it (the read is
    /// skipped).
    pub trained_context: Option<i64>,
    /// The ladder rung this container runs (ladder design §5), from the
    /// descriptor it was rendered from ([`ModelRuntime::rung`]). `None` for a
    /// row without a ladder. On a ladder row [`Self::params`] are that rung's
    /// (its `--ctx-size`), so [`Self::per_request_ctx`] is the rung's per-slot
    /// context — ladder rows always run split slots (§4.3 rule 2).
    pub rung: Option<RungPos>,
}

impl GateFacts {
    /// The facts of a start from `runtime` (a chat descriptor) under
    /// `models_dir`. `None` for every other class — nothing gates it.
    ///
    /// The GGUF header is read only for a guarded or a ladder start: it is
    /// the one input that is not already in the descriptor, and only the
    /// per-request limit uses it. Once per start, off the async runtime.
    pub async fn of_start(runtime: &ModelRuntime, models_dir: &str) -> Option<Arc<Self>> {
        if runtime.class != Class::Chat {
            return None;
        }
        let Some(LlamaArgs::Chat {
            gguf_path,
            params,
            args,
        }) = &runtime.llama
        else {
            return None;
        };
        let trained_context = if params.pool_guarded() || runtime.rung.is_some() {
            let path = Path::new(models_dir).join(gguf_path);
            tokio::task::spawn_blocking(move || crate::gguf::summarize(&path))
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|s| s.context_length)
                .and_then(|v| i64::try_from(v).ok())
        } else {
            None
        };
        Some(Arc::new(Self {
            model_id: runtime.model_id.clone(),
            models_dir: models_dir.to_string(),
            gguf_path: gguf_path.clone(),
            params: params.clone(),
            args: args.clone(),
            trained_context,
            rung: runtime.rung,
        }))
    }

    /// The weights' file name — what `x-lmgw-rung` and the dashboard show
    /// for the rung that runs.
    pub fn gguf_file(&self) -> &str {
        crate::runtime::descriptor::file_name(&self.gguf_path)
    }

    /// Whether the pool ledger guards this container
    /// ([`LlamaParams::pool_guarded`], decision D1) — as it was started.
    pub fn pool_guarded(&self) -> bool {
        self.params.pool_guarded()
    }

    /// The max-output ceiling every send is clamped to (`n_predict`).
    pub fn n_predict(&self) -> Option<i64> {
        self.params.n_predict
    }

    /// The shared pool's size in tokens ([`LlamaParams::pool_tokens`]).
    pub fn pool_tokens(&self) -> Option<u64> {
        self.params
            .pool_tokens()
            .and_then(|v| u64::try_from(v).ok())
    }

    /// The per-request limit ([`LlamaParams::per_request_ctx`]) with the
    /// trained context read at start.
    ///
    /// On a ladder rung that is the slot llama-server really runs: `ctx /
    /// parallel`, capped at the weights' trained context, the way the server
    /// caps every slot ([`crate::ladder::slot_ctx`]). Rows without a ladder
    /// keep [`LlamaParams::per_request_ctx`]'s own answer, split or unified.
    pub fn per_request_ctx(&self) -> Option<u64> {
        let ctx = self.params.per_request_ctx(self.trained_context)?;
        let ctx = match self.rung {
            Some(_) => crate::ladder::slot_ctx(ctx, self.trained_context),
            None => ctx,
        };
        u64::try_from(ctx).ok()
    }

    /// What the per-image bound is read from ([`super::count::image_token_bound`]).
    pub fn projector_row(&self) -> ProjectorRow<'_> {
        ProjectorRow {
            model_id: &self.model_id,
            gguf_path: &self.gguf_path,
            params: &self.params,
            args: &self.args,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_runtime(params: LlamaParams) -> ModelRuntime {
        ModelRuntime {
            class: Class::Chat,
            model_id: "m".into(),
            image: "img".into(),
            extra_run_args: Vec::new(),
            idle_seconds: 0,
            warm_start: false,
            enabled: true,
            llama: Some(LlamaArgs::Chat {
                gguf_path: "m.gguf".into(),
                params,
                args: vec!["--image-max-tokens".into(), "300".into()],
            }),
            audio: None,
            audio_settings: None,
            image_model: None,
            sdcpp_caps: None,
            rung: None,
        }
    }

    #[tokio::test]
    async fn the_facts_are_the_descriptors_own_numbers() {
        let params = LlamaParams {
            kv_unified: Some(true),
            parallel: Some(2),
            ctx_size: Some(4096),
            n_predict: Some(256),
            ..Default::default()
        };
        let facts = GateFacts::of_start(&chat_runtime(params), "/nowhere")
            .await
            .unwrap();
        assert!(facts.pool_guarded());
        assert_eq!(facts.n_predict(), Some(256));
        assert_eq!(facts.pool_tokens(), Some(4096));
        // No readable GGUF: the trained context is unknown, never guessed.
        assert_eq!(facts.trained_context, None);
        assert_eq!(facts.per_request_ctx(), Some(4096));
        assert_eq!(facts.projector_row().args.len(), 2);
    }

    #[tokio::test]
    async fn a_ladder_rungs_slot_is_capped_at_its_trained_context() {
        // Review finding 1: llama-server caps every slot at the trained
        // context, so a rung started at -c 512 on weights trained at 100
        // holds 100 per slot — and that is what the gate judges it by.
        let dir = tempfile::tempdir().unwrap();
        crate::gguf::synth::chat("qwen3", 100).write_to(&dir.path().join("m.gguf"));
        let models_dir = dir.path().display().to_string();
        let params = LlamaParams {
            parallel: Some(1),
            ctx_size: Some(512),
            n_predict: Some(16),
            ..Default::default()
        };
        let mut rt = chat_runtime(params.clone());
        rt.rung = Some(RungPos { index: 2, of: 3 });
        let facts = GateFacts::of_start(&rt, &models_dir).await.unwrap();
        assert_eq!(facts.trained_context, Some(100));
        assert_eq!(facts.per_request_ctx(), Some(100));
        assert_eq!(
            crate::gate::RungTag::of(&facts).map(|t| t.per_slot),
            Some(100)
        );

        // A row without a ladder is published and judged as before.
        let plain = GateFacts::of_start(&chat_runtime(params), &models_dir)
            .await
            .unwrap();
        assert_eq!(plain.trained_context, None, "not read: nothing needs it");
        assert_eq!(plain.per_request_ctx(), Some(512));
    }

    #[tokio::test]
    async fn only_a_chat_start_has_facts() {
        let mut rt = chat_runtime(LlamaParams::default());
        rt.class = Class::Aux;
        assert!(GateFacts::of_start(&rt, "/nowhere").await.is_none());
    }
}
