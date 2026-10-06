//! Whether a model takes a user turn as audio (voice-audio-input design
//! §2.2, changed 2026-10-06: capability, not locality). The one predicate
//! the verdict (`web::chat_voice::audio_input`), the send
//! (`web::chat_turn::spoken::may_hear`) and an attachment's native audio
//! (`web::chat_attach_gate`, through [`hears_from`]) all ask, of the model
//! that answers and the route it answers on. The verdict reads the running
//! container's `/props` and what a candidate alias publishes, the send the
//! claimed container and the picked row: they agree in the common case, and
//! a mismatch is refused at the send and retried as the transcript.
//!
//! Three questions, in order ([`hears`]):
//!
//! 1. **The egress.** Can lmgw's egress to the route's upstream encode an
//!    audio part? The OpenAI-compatible, llama.cpp and Gemini egresses can
//!    (`input_audio`, `inlineData`); Anthropic's API has no audio block.
//! 2. **The model.** Its capabilities as `/v1/models` publishes them
//!    ([`super::exposed::exposed_entry`], the owner's override merged in): a
//!    chat model whose `input_modalities` include `audio`. A speech-to-text
//!    row publishes audio input too, hence the task. A candidate a walk
//!    picked that is not public is read by its row.
//! 3. **The server.** What a llama-server said about itself in `GET /props`
//!    (`LlamaFacts::audio`, llama egress design §4.1): a server with no audio
//!    projector loaded vetoes the audio whatever was published; one that
//!    loaded one lifts an unknown (a projector lmgw could not read). Not
//!    known — every other upstream, an external server's first request —
//!    leaves the answer of step 2 (egress decision 14: unknown means today).
//!
//! **Where the model runs plays no role** (the owner's ruling, 2026-10-06):
//! a cloud model, a llama-server elsewhere on the network and one this lmgw
//! runs hear alike once they can take the audio. **Unknown is no**: lmgw
//! sends audio only to a model it knows takes it, since a server that drops
//! a content part it does not know answers an empty turn. The owner makes a
//! model hear by listing `audio` in its capabilities override.

use std::sync::Arc;

use crate::config::{Protocol, Route};
use crate::egress::llama_cpp::props::LlamaFacts;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::ModelCapabilities;

#[cfg(test)]
mod tests;

/// The predicate's answer (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hears {
    /// The model takes the turn as audio, and lmgw can send it.
    Yes,
    /// It cannot, and why.
    No(NotHeard),
    /// lmgw cannot tell: the turn goes as its transcript.
    Unknown,
}

/// Why a model does not take a turn as audio ([`Hears::No`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotHeard {
    /// Its upstream speaks the Anthropic API, which has no audio part.
    NoAudioPart,
    /// It is no chat model (a speech-to-text row takes audio, and answers
    /// with a transcript).
    NotChat,
    /// Its input modalities leave audio out.
    NoAudioInput,
    /// Its llama-server loaded no audio projector (`/props`).
    NoProjector,
}

impl Hears {
    /// The turn goes as audio.
    pub fn yes(self) -> bool {
        self == Self::Yes
    }

    /// Why `name` does not take the turn as audio, as a sentence; `None` when
    /// it does.
    pub fn why(self, name: &str) -> Option<String> {
        Some(match self {
            Self::Yes => return None,
            Self::Unknown => format!("lmgw cannot tell whether {name} takes audio"),
            Self::No(NotHeard::NoAudioPart) => {
                format!("{name} is served over the Anthropic API, which has no audio input part")
            }
            Self::No(NotHeard::NotChat) => format!("{name} is not a chat model"),
            Self::No(NotHeard::NoAudioInput) => format!("{name} does not take audio input"),
            Self::No(NotHeard::NoProjector) => {
                format!("{name}'s server loaded no audio projector")
            }
        })
    }

    /// [`Self::why`] of a model the turn is handed to: "this goes to
    /// {name}, which …" — what follows a swap's lead ("under the GPU hold").
    pub fn goes_to(self, name: &str) -> Option<String> {
        let tail = match self {
            Self::Yes => return None,
            Self::Unknown => "and lmgw cannot tell whether it takes audio",
            Self::No(NotHeard::NoAudioPart) => {
                "served over the Anthropic API, which has no audio input part"
            }
            Self::No(NotHeard::NotChat) => "which is not a chat model",
            Self::No(NotHeard::NoAudioInput) => "which does not take audio input",
            Self::No(NotHeard::NoProjector) => "whose server loaded no audio projector",
        };
        Some(format!("this goes to {name}, {tail}"))
    }
}

/// The model the predicate is asked about (review V8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model<'a> {
    /// A name a turn routes by — an alias, a candidate alias, a public
    /// local row, a passthrough model — read as `/v1/models` publishes it.
    Named(&'a str),
    /// A candidate alias's pick, by its public name: read from its row,
    /// through the route the walk gave the turn — never by the name, which
    /// an alias of the same name would shadow.
    Pick(&'a str),
}

impl<'a> Model<'a> {
    /// The name a sentence says it by.
    pub fn name(self) -> &'a str {
        match self {
            Self::Named(n) | Self::Pick(n) => n,
        }
    }
}

/// Whether `model`, answering on `route`, takes a user turn as audio
/// (module doc). `facts`: what the route's llama-server said about itself
/// ([`server_facts`]); `None` while not known, and for every other upstream.
pub async fn hears(
    state: &SharedState,
    model: Model<'_>,
    route: &Route,
    facts: Option<&LlamaFacts>,
) -> Hears {
    // An egress with no audio part needs no lookup.
    if !egress_takes_audio(route.upstream.protocol) {
        return Hears::No(NotHeard::NoAudioPart);
    }
    let caps = capabilities(state, model, route).await;
    hears_from(caps.as_ref(), route, facts)
}

/// Whether the egress for `protocol` encodes an audio part. Exhaustive on
/// purpose: a new protocol has to say whether its egress encodes
/// `ContentPart::Audio`.
pub fn egress_takes_audio(protocol: Protocol) -> bool {
    match protocol {
        Protocol::Openai | Protocol::LlamaCpp | Protocol::Gemini => true,
        Protocol::Anthropic => false,
    }
}

/// [`hears`] on capabilities already read (`caps`, as `/v1/models`
/// publishes them; `None`: none could be read) — the attachments' check
/// asks it too (`web::chat_attach_gate`), with what it read for vision.
pub fn hears_from(
    caps: Option<&ModelCapabilities>,
    route: &Route,
    facts: Option<&LlamaFacts>,
) -> Hears {
    // 1. The egress.
    if !egress_takes_audio(route.upstream.protocol) {
        return Hears::No(NotHeard::NoAudioPart);
    }
    // 2. The model, as published.
    let published = match caps {
        None => Hears::Unknown,
        Some(c) if c.task != "chat" => Hears::No(NotHeard::NotChat),
        Some(c) => match &c.input_modalities {
            None => Hears::Unknown,
            Some(m) if m.iter().any(|x| x == "audio") => Hears::Yes,
            Some(_) => Hears::No(NotHeard::NoAudioInput),
        },
    };
    // 3. The server's own word. The one place facts about the running
    // server meet the published answer: a veto, or an unknown lifted; a
    // published "no" (the owner's override among them) stands.
    match (published, facts.and_then(|f| f.audio)) {
        (Hears::No(n), _) => Hears::No(n),
        (_, Some(false)) => Hears::No(NotHeard::NoProjector),
        (Hears::Unknown, Some(true)) => Hears::Yes,
        (answer, _) => answer,
    }
}

/// `model`'s capabilities: a name's as `/v1/models` publishes them, and a
/// candidate's pick — or a name `exposed_entry` does not list, a row that
/// is not public — its row's, read through the route the turn goes on.
async fn capabilities(
    state: &SharedState,
    model: Model<'_>,
    route: &Route,
) -> Option<ModelCapabilities> {
    if let Model::Named(name) = model {
        if let Some(entry) = super::exposed::exposed_entry(state, name).await {
            return entry.capabilities;
        }
    }
    let target = crate::vram::classify(route).filter(|t| t.class == Class::Chat)?;
    let snap = state.snapshot();
    let row = snap
        .local_models
        .iter()
        .find(|m| m.model_id == target.model_id)?;
    super::exposed::derived_for_local(state, row)
        .await
        .capabilities
}

/// What the llama-server `route` goes to said about itself, never waiting:
/// with a claim (`hold`), the claimed container's facts
/// ([`crate::llama_facts::resolve`]); without one, a managed row's container
/// that is up now (the verdict claims nothing), and an external `llama_cpp`
/// row's cached facts. `None` while unknown, and for every other upstream.
pub fn server_facts(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
) -> Option<Arc<LlamaFacts>> {
    if hold.is_none() {
        if let Some(t) = crate::vram::classify(route) {
            return state
                .runtime()
                .llama_entry_now(t.class, &t.model_id)
                .and_then(|e| e.facts().cloned());
        }
    }
    crate::llama_facts::resolve(state, hold, route).facts
}
