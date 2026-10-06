//! What the registry keeps about a llama-server container beyond its port
//! (llama egress design §4.2, §8.2): what the server said about itself in
//! `GET /props`, and what the row it was started with says about its
//! projector.
//!
//! **Facts belong to a server, not to a request** (decision 13). They are read
//! once per start and climb, right after `/health` answers
//! (`Registry::await_ready`), and once per adoption, after its probe and before
//! its insert (`Registry::adopt`); each read is bounded by what is left of
//! `vram.load_timeout_seconds`, the readiness budget. They live on the entry
//! and go with it, so a restart, a climb, an image update or an adoption reads
//! them again. The local model test reads them live and refreshes the entry
//! ([`Registry::refresh_llama_props`]), so a read that failed once does not
//! stay failed for the container's life — and a live read that fails does
//! not take facts read before away.
//!
//! **A failed read is a warning, never a failed start.** A model that answers
//! `/health` serves; what it could not say about itself is unknown, and
//! unknown means today (decision 14). The reason is shown with the entry's
//! warnings ([`Registry::list`]).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serializer;

use super::*;
use crate::egress::llama_cpp::props::{self, LlamaFacts, Props, PropsFailure};
use crate::runtime::argv::LlamaArgs;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::Class;

/// What the registry knows about one llama-server container (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaEntry {
    /// The container's `GET /props`, as read at its start, climb or adoption,
    /// or since refreshed by a local model test — without the body they were
    /// read from (`bodiless`). `Err` says why there are no facts, in a
    /// sentence the entry's warnings show.
    pub props: Result<Arc<LlamaFacts>, String>,
    /// The started row's projector ubatch advisory
    /// (`modelinfo::projector_ubatch_advisory`): `Some` when the row
    /// loads a projector whose images llama.cpp decodes non-causally (or lmgw
    /// could not tell) under batch sizes too small for a whole image. Such a
    /// server aborts on a large enough image and takes every request in
    /// flight with it, which is why a tool image is never sent to it (§8.2).
    /// The row as it was **started**, like [`GateFacts`](crate::gate::facts::GateFacts):
    /// an edit since does not change the container that runs.
    pub ubatch_advisory: Option<String>,
}

impl LlamaEntry {
    /// The facts, when they could be read.
    pub fn facts(&self) -> Option<&Arc<LlamaFacts>> {
        self.props.as_ref().ok()
    }
}

/// [`RuntimeView::llama_props`]'s form on the wire: the facts without their
/// raw body (`LlamaFacts` skips it).
pub(super) fn serialize_facts<S: Serializer>(
    facts: &Option<Arc<LlamaFacts>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&facts.as_deref(), s)
}

/// The projector ubatch advisory of a start from `runtime` under
/// `models_dir` ([`LlamaEntry::ubatch_advisory`]). `None` for every row
/// without a projector, and for every class but chat. Reads the weights' and
/// the projector's headers, once per start, as the local model checks do
/// (`ops::local_model_checks::model_warnings`).
///
/// A projector file that is not where the row says counts as one lmgw could
/// not read (unknown attention), unlike in the model checks, which report
/// the missing file instead: a container that runs anyway loaded it from
/// somewhere lmgw does not see, and a tool image must not go to it on a
/// guess (llama egress design §8.2). A row that names no projector lmgw
/// reads at all is `gate::tool_images::unread_projector_short`'s.
pub(super) async fn ubatch_advisory(runtime: &ModelRuntime, models_dir: &str) -> Option<String> {
    let Some(LlamaArgs::Chat {
        gguf_path,
        params,
        args,
    }) = &runtime.llama
    else {
        return None;
    };
    if !crate::modelinfo::row_loads_projector(params, args) {
        return None;
    }
    let weights = Path::new(models_dir).join(gguf_path);
    let text_width = tokio::task::spawn_blocking(move || crate::gguf::summarize(&weights))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|s| s.embedding_length);
    let attention = crate::modelinfo::row_image_attention(models_dir, params, args, text_width)
        .await
        .unwrap_or_else(|| {
            crate::modelinfo::ImageAttention::Unknown(
                "its projector file is not where the row says, in the models dir".into(),
            )
        });
    crate::modelinfo::projector_ubatch_advisory(params, args, &attention)
}

/// A `/props` answer read live: the facts, or why there are none.
fn as_read(answer: Result<Props, PropsFailure>) -> Result<Arc<LlamaFacts>, String> {
    match answer {
        Ok(Props::Model(facts)) => Ok(Arc::new(facts)),
        Ok(Props::Router { .. }) => Err("GET /props answered as a llama-server router, which \
                                         states no facts about one model"
            .into()),
        Err(e) => Err(format!("GET /props could not be read ({e})")),
    }
}

/// Why a read found no facts, as an entry with none keeps it: the warning
/// says how long they stay unknown.
pub fn unknown_until(why: &str) -> String {
    format!(
        "{why} — this llama-server's modalities, slot context and build are unknown until it \
         starts again or a local model test reads them"
    )
}

/// Facts as an entry keeps them: without the body they were read from
/// (`LlamaFacts::raw` is `Null`). It carries the chat template, and nothing
/// reads it off an entry — the local model test shows the body of its own
/// live read.
fn bodiless(facts: Arc<LlamaFacts>) -> Arc<LlamaFacts> {
    Arc::new(LlamaFacts {
        raw: serde_json::Value::Null,
        ..Arc::unwrap_or_clone(facts)
    })
}

/// What [`Registry::refresh_llama_props`] did with a newer read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// The entry holds the newer read: its facts, or its failure where the
    /// entry had no facts either.
    Stored,
    /// The read failed and the facts the entry had are kept: a failure says
    /// nothing about what the server said before (the server busy, the
    /// budget short), and the facts stay true until the container changes.
    Kept,
    /// That container's entry is gone, or holds no llama facts.
    Gone,
}

impl Registry {
    /// Read `GET /props` from the managed container published on `port`,
    /// `timeout` bounding the whole exchange: the facts, or why there are
    /// none.
    pub async fn read_llama_props(
        &self,
        port: u16,
        timeout: Duration,
    ) -> Result<Arc<LlamaFacts>, String> {
        as_read(props::probe(props::container_request(&self.http, port), Some(timeout)).await)
    }

    /// [`Self::read_llama_props`] bounded by what is left until `deadline` —
    /// the readiness budget a start or an adoption began with — as the new
    /// entry keeps it: the facts, or the warning that says why there are
    /// none. Not asked at all once the budget is spent: a request with no
    /// time is no answer.
    pub(super) async fn read_llama_props_by(
        &self,
        port: u16,
        deadline: Instant,
    ) -> Result<Arc<LlamaFacts>, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(
                "GET /props was not asked: vram.load_timeout_seconds was spent by the time \
                 the server was up — this llama-server's modalities, slot context and build \
                 are unknown until it starts again or a local model test reads them"
                    .into(),
            );
        }
        self.read_llama_props(port, left)
            .await
            .map(bodiless)
            .map_err(|why| unknown_until(&why))
    }

    /// What the registry knows about container `generation` of `(class,
    /// model_id)` (module doc). `None` when that container's entry is gone —
    /// stopped, or replaced by a newer one — and for every engine but
    /// llama-server. By generation, so a caller that holds a claim never reads
    /// the facts of another container than the one its claim is on.
    pub fn llama_entry(&self, class: Class, model_id: &str, generation: u64) -> Option<LlamaEntry> {
        let map = self.map();
        map.get(&(class, model_id.to_string()))
            .filter(|e| e.generation == generation)
            .and_then(|e| e.llama.clone())
    }

    /// [`Self::llama_entry`] of whichever container of `(class, model_id)`
    /// the registry holds now, for a caller that holds no claim and only
    /// predicts (the voice-audio verdict, `capabilities::hears::server_facts`):
    /// its facts as last read. `None` while there is none, and while a start
    /// or a climb has not read its `/props` yet.
    pub fn llama_entry_now(&self, class: Class, model_id: &str) -> Option<LlamaEntry> {
        self.map()
            .get(&(class, model_id.to_string()))
            .and_then(|e| e.llama.clone())
    }

    /// Refresh what container `generation` of `(class, model_id)` said about
    /// itself with a newer read (the local model test's, [`Self::read_llama_props`]):
    /// facts replace what the entry had; a failure replaces only a failure,
    /// and beside facts read before is logged as a warning and dropped
    /// ([`Refresh`]). Nothing is changed when that container's entry is gone
    /// or holds no llama facts.
    pub fn refresh_llama_props(
        &self,
        class: Class,
        model_id: &str,
        generation: u64,
        props: Result<Arc<LlamaFacts>, String>,
    ) -> Refresh {
        let done = {
            let mut map = self.map();
            let Some(llama) = map
                .get_mut(&(class, model_id.to_string()))
                .filter(|e| e.generation == generation)
                .and_then(|e| e.llama.as_mut())
            else {
                return Refresh::Gone;
            };
            match (&llama.props, &props) {
                (Ok(_), Err(_)) => Refresh::Kept,
                _ => {
                    llama.props = props
                        .clone()
                        .map(bodiless)
                        .map_err(|why| unknown_until(&why));
                    Refresh::Stored
                }
            }
        };
        if let (Refresh::Kept, Err(why)) = (done, &props) {
            tracing::warn!(
                "{class} model '{model_id}': {why}; the facts read before stay on its entry"
            );
        }
        done
    }
}
