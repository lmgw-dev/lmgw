//! What a chat send to a llama-server carries about that server (llama egress
//! design §3.2): its `/props` facts and the tool-image decision made on them,
//! **decided once** per send in `gate::fit_chat` and never refilled
//! mid-request.
//!
//! Lives on [`Upstream::llama`](super::Upstream::llama), runtime only. The
//! single writer of a held endpoint, `LocalHold::point_at`, rewrites
//! `base_url` and copies this unchanged. A ladder climb and a dead-container
//! retry land on another container, so each attempt holds a decision that
//! lets tool images go against the container it goes to
//! (`gate::tool_images::recheck`): one that does not see, carries a
//! projector advisory or says nothing gets the placeholder (or today's
//! bytes) instead. Only ever downward, so a send posts at most what was
//! counted — the one deliberate case where it posts less. A candidate
//! re-pick lands on another model and is fitted for it, so it decides for
//! itself and never inherits another model's yes.

use std::sync::Arc;

use crate::egress::llama_cpp::props::LlamaFacts;

/// The facts a llama-server route was fitted on, and what was decided on
/// them (§3.2). Only ever built when facts are known: an unknown server is
/// `Upstream::llama == None`, and unknown means today's bytes (decision 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaRoute {
    /// What the server said about itself (`GET /props`): a managed
    /// container's from its registry entry, an external row's from the
    /// background cache (`crate::llama_facts`).
    pub facts: Arc<LlamaFacts>,
    /// Whether this send's tool-result images go to the server as images.
    pub tool_images: ToolImages,
}

/// The frozen tool-image decision (§8.2): whether a tool result's image is
/// sent as an `image_url` part, decided once per send by a conservative
/// predicate. Nothing is ever refused because of it: a tool image that may
/// not go is sent as today's placeholder, naming this reason, with a WARN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolImages {
    /// Every condition of the predicate held (vision, no projector ubatch
    /// advisory, a per-image bound on a guarded row, Vision on a candidate
    /// alias). Each image still has to pass the format check.
    Allowed,
    /// A condition failed; the sentence names it ("this model's server has
    /// no vision", "its projector can abort above the batch size", "no
    /// per-image bound", …) and is what the placeholder and the WARN say.
    Refused(String),
    /// The server did not say whether it sees (`/props` without
    /// `modalities.vision`): tool images do not go, and the tool results are
    /// today's bytes, placeholders included, with no reason of their own
    /// (decision 14: a fact that is absent means today).
    Unknown,
}

impl ToolImages {
    /// Whether tool images may go as images on this send.
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Why they may not, when a condition said so. `None` when they may, and
    /// when nothing was known to decide on ([`Self::Unknown`]).
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Allowed | Self::Unknown => None,
            Self::Refused(why) => Some(why),
        }
    }
}
