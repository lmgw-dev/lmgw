//! A TTS alias's fallback speaks its own voice (realtime design §5.3, §9.2;
//! WP3 review M2).
//!
//! When the GPU hold — or admission, when VRAM lmgw cannot free is short —
//! answers a response's first clause with the TTS alias's fallback, the
//! voice resolved for the local model means nothing to it: a cloud TTS
//! refuses "alba", and every spoken response under the hold failed. So the
//! voice is resolved once more, against the fallback, before the first
//! clause is sent, by what its upstream is ([`Engine`]):
//! - an **OpenAI** one — the OpenAI protocol on a generic upstream, a cloud
//!   TTS — has no voice list to read: it speaks the OpenAI voice name the
//!   session asked for as it is, else the name `realtime.voice_map` maps the
//!   requested one to;
//! - an **audio.cpp** one: the chain over the fallback's own facts — its
//!   voice list, read on the route the response holds — and every rule falls
//!   through to the next when the fallback lacks its voice (package B
//!   review 7): the name asked for, its `voice_map` target, and for an
//!   OpenAI name `realtime.default_voice`, then the fallback row's
//!   `default_voice_preset`. The first voice the fallback has speaks. A list
//!   that cannot be read does not refuse (B2 review 8): what the row's
//!   presets and the voice library show still answers, then its inline
//!   preset, and otherwise the voice the primary's chain would send unseen
//!   — the name's `voice_map` target or the name itself, for an OpenAI name
//!   `default_voice` or the default preset — goes unverified, and the
//!   engine judges it, as for the primary;
//! - anything else — a llama.cpp or sd.cpp upstream, another protocol — is
//!   `voice_not_configured`, naming the fallback: an engine is never called
//!   with a voice it was not shown to have.

use serde_json::Value;

use super::{
    echo, inline_preset, not_configured, SpeakVoice, VoiceFacts, VoiceVia, BUILTIN_VOICES,
};
use crate::config::{Protocol, RealtimeSettings, UpstreamKind};
use crate::error::GatewayError;
use crate::realtime::protocol::Voice;

/// What a fallback's upstream is, for naming its voice (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Engine {
    /// audio.cpp: its voice list says.
    AudioCpp,
    /// The OpenAI protocol on a generic upstream: OpenAI's voice names.
    OpenAi,
    /// Anything else, described.
    Other(String),
}

impl Engine {
    pub fn of(protocol: Protocol, kind: UpstreamKind) -> Self {
        match (protocol, kind) {
            (_, UpstreamKind::AudioCpp) => Self::AudioCpp,
            (Protocol::Openai, UpstreamKind::Generic) => Self::OpenAi,
            (p, k) => Self::Other(format!(
                "a {} upstream of the {} protocol",
                k.as_str(),
                p.as_str()
            )),
        }
    }
}

/// The voice `fallback` speaks for `requested` (module doc). `facts`: the
/// fallback's own, for an audio.cpp upstream — `listed` the list read on
/// the held route, `None` if it could not be — and `None` for an OpenAI
/// one. `alias` is the TTS alias the session named.
pub(crate) fn for_fallback(
    alias: &str,
    fallback: &str,
    requested: &Voice,
    facts: Option<&VoiceFacts>,
    settings: &RealtimeSettings,
) -> Result<SpeakVoice, GatewayError> {
    let asked = echo(requested);
    let speak = |send: &str, via| SpeakVoice {
        name: send.to_string(),
        send: Some(send.to_string()),
        via,
        verified: true,
    };
    let refused = |why: &str| {
        not_configured(format!(
            "TTS alias '{alias}' is answered by its fallback '{fallback}' (the GPU hold, or VRAM \
             lmgw cannot free), which has no voice for '{asked}': {why}"
        ))
    };
    let mapped = |n: &str| {
        settings
            .voice_map
            .get(n)
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
    };
    let Some(facts) = facts else {
        // OpenAI: the built-in names are its own.
        if let Voice::Name(n) = requested {
            let lower = n.to_ascii_lowercase();
            if BUILTIN_VOICES.contains(&lower.as_str()) {
                return Ok(speak(&lower, VoiceVia::Model));
            }
            if let Some(m) = mapped(n) {
                return Ok(speak(&m, VoiceVia::VoiceMap));
            }
        }
        return Err(refused(
            "an OpenAI fallback speaks the OpenAI voice name the session asks for (alloy, \
             marin, …) or the name realtime.voice_map maps it to — ask for one of those, or map \
             this name",
        ));
    };
    // The chain, each rule falling through when the fallback lacks its
    // voice (module doc); the row's inline preset, which has no name to
    // check, comes last and always speaks.
    let mut chain: Vec<(String, VoiceVia)> = Vec::new();
    let mut inline = None;
    match requested {
        Voice::Id { id } => chain.push((id.clone(), VoiceVia::Library)),
        Voice::Name(n) => {
            chain.push((n.clone(), VoiceVia::Model));
            chain.extend(mapped(n).map(|m| (m, VoiceVia::VoiceMap)));
            if BUILTIN_VOICES.contains(&n.to_ascii_lowercase().as_str()) {
                let configured = settings.default_voice.trim();
                if !configured.is_empty() {
                    chain.push((configured.to_string(), VoiceVia::DefaultVoice));
                }
                match &facts.default_preset {
                    Some(Value::String(p)) if !p.trim().is_empty() => {
                        chain.push((p.trim().to_string(), VoiceVia::DefaultPreset))
                    }
                    Some(Value::Object(o)) => inline = Some(inline_preset(o)),
                    _ => {}
                }
            }
        }
    }
    if let Some((name, via)) = chain.iter().find(|(n, _)| facts.knows(n)) {
        return Ok(speak(name, *via));
    }
    if let Some(v) = inline {
        return Ok(v);
    }
    if facts.listed.is_none() {
        // Only the list could say (module doc): what the primary's chain
        // would send unseen, unverified — the engine judges.
        // In the primary chain's order (`super::decide`): the name's
        // `voice_map` target first, then for an OpenAI name the configured
        // default, for any other the name itself (B4 review).
        let unseen = match requested {
            Voice::Id { .. } => None,
            Voice::Name(n) => {
                let mapped = chain.iter().find(|(_, via)| *via == VoiceVia::VoiceMap);
                let builtin = BUILTIN_VOICES.contains(&n.to_ascii_lowercase().as_str());
                mapped.or_else(|| {
                    if builtin {
                        chain.iter().find(|(_, via)| {
                            matches!(via, VoiceVia::DefaultVoice | VoiceVia::DefaultPreset)
                        })
                    } else {
                        chain.first()
                    }
                })
            }
        };
        if let Some((name, via)) = unseen {
            tracing::info!(
                "realtime: the voice list of TTS fallback '{fallback}' could not be read (GET \
                 /v1/audio/voices?model={fallback}); '{name}' ({}) is sent unverified, and the \
                 engine judges it",
                via.knob()
            );
            return Ok(SpeakVoice {
                verified: false,
                ..speak(name, *via)
            });
        }
    }
    let tried: Vec<String> = chain
        .iter()
        .map(|(n, via)| format!("'{n}' ({})", via.knob()))
        .collect();
    Err(refused(&format!(
        "it has none of the voices the chain names — {} — in its voice list, presets or voice \
         library",
        tried.join(", ")
    )))
}
