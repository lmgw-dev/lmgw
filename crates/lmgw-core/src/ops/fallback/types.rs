//! A model's **type**, as the fallback save check compares it (openai egress
//! design §14.2 item 4, decision 29): a fallback must be the same model
//! type; chat models that differ only in modalities are allowed, with a
//! warning.
//!
//! The type is the `task` `/v1/models` publishes ([`ModelCapabilities::task`],
//! [`crate::capabilities::exposed`]), the one place lmgw says what a model is:
//! `chat`, `embedding`, `rerank`, `decision`, an image model (`image_edit` is
//! one with `image_generation`: an image row that edits also generates),
//! `video_generation`, each audio task on its own, and `realtime`. A local
//! row's type is its class's, read from the row; every other name's comes
//! from its published capabilities, which a cloud model only has when its
//! upstream catalog lists it.

use crate::candidates::{supports, Facet, FacetSet};
use crate::capabilities::ModelCapabilities;
use crate::config::{AuxKind, HopKey, Snapshot};
use crate::runtime::Class;
use crate::state::SharedState;

/// One model type: a published task, image tasks folded into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelType(String);

impl ModelType {
    pub(crate) fn from_task(task: &str) -> Self {
        Self(
            match task {
                "image_edit" => "image_generation",
                t => t,
            }
            .to_string(),
        )
    }

    pub(crate) fn chat() -> Self {
        Self("chat".into())
    }

    pub(crate) fn image() -> Self {
        Self("image_generation".into())
    }

    /// An aux row of `kind`: the task its `/v1/models` entry publishes.
    pub(crate) fn aux(kind: AuxKind) -> Self {
        Self(
            match kind {
                AuxKind::Embed => "embedding",
                AuxKind::Rerank => "rerank",
                AuxKind::Decision => "decision",
            }
            .into(),
        )
    }

    /// An audio row of `task` (`tts`, `asr`, …).
    pub(crate) fn audio(task: &str) -> Self {
        Self::from_task(task.trim())
    }

    pub(crate) fn is_chat(&self) -> bool {
        self.0 == "chat"
    }

    /// The type as a noun phrase: "a chat model", "a speech-to-text model".
    pub(crate) fn describe(&self) -> String {
        match self.0.as_str() {
            "chat" => "a chat model".into(),
            "embedding" => "an embedding model".into(),
            "rerank" => "a reranker".into(),
            "decision" => "a decision model".into(),
            "image_generation" => "an image model".into(),
            "video_generation" => "a video model".into(),
            "tts" => "a text-to-speech model".into(),
            "asr" => "a speech-to-text model".into(),
            "vdes" => "a voice-design model".into(),
            "align" => "an alignment model".into(),
            "realtime" => "an OpenAI Realtime model".into(),
            other => format!("a '{other}' model"),
        }
    }
}

/// What lmgw knows about one model or alias, for the type check and the
/// facet warnings: its type (`None` when it cannot know it) and, for a model
/// with published capabilities, the facets it takes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts {
    pub(crate) ty: Option<ModelType>,
    pub(crate) facets: Option<FacetSet>,
}

impl Facts {
    pub(crate) fn of_type(ty: ModelType) -> Self {
        Self {
            ty: Some(ty),
            facets: None,
        }
    }

    /// From published capabilities, `None` being "nothing could be read".
    ///
    /// A `chat` that nothing stated is no type either: `/v1/models` calls a
    /// model `chat` when its catalog lists it bare (OpenAI's own lists
    /// `{id, object, created, owned_by}`, its embedding models included) and
    /// its name says nothing else (`capabilities::task::by_name`), which is a
    /// default for routing, not a fact to refuse a save on.
    pub(crate) fn from_capabilities(caps: Option<&ModelCapabilities>) -> Self {
        let Some(caps) = caps else {
            return Self::default();
        };
        let unstated = caps.task == "chat"
            && caps.input_modalities.is_none()
            && caps.output_modalities.is_none()
            && caps.source != "owner";
        Self {
            ty: (!unstated).then(|| ModelType::from_task(&caps.task)),
            facets: Some(
                Facet::ALL
                    .into_iter()
                    .filter(|f| supports(caps, *f))
                    .fold(FacetSet::EMPTY, FacetSet::insert),
            ),
        }
    }
}

/// The type of a local row, read from the row itself: no I/O, and known for
/// a disabled row too.
pub(crate) fn local_type(snap: &Snapshot, class: Class, model_id: &str) -> Option<ModelType> {
    match class {
        Class::Chat => Some(ModelType::chat()),
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| ModelType::aux(m.kind)),
        Class::Audio => snap
            .audio_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| ModelType::audio(&m.task)),
        Class::Image => Some(ModelType::image()),
    }
}

/// What lmgw knows about the model or alias a client reaches by `name`: a
/// local row's type from the row, a candidate alias's (its candidates are
/// chat rows), and everything else's from what `/v1/models` publishes for
/// the name, fetching its upstream's catalog as that does. The facets come
/// from the published capabilities either way.
pub(crate) async fn facts_of(state: &SharedState, snap: &Snapshot, name: &str) -> Facts {
    let published = crate::capabilities::exposed::exposed_entry(state, name)
        .await
        .and_then(|e| e.capabilities);
    let mut facts = Facts::from_capabilities(published.as_ref());
    // A disabled row is typed from the row too (§14.3, decision 28).
    match snap.any_hop_key(name) {
        Some(HopKey::Local(class, id)) => {
            if let Some(ty) = local_type(snap, class, &id) {
                facts.ty = Some(ty);
            }
        }
        Some(HopKey::Candidate(_)) => facts.ty = Some(ModelType::chat()),
        _ => {}
    }
    facts
}
