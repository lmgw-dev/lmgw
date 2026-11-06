//! What an upstream write stores for its protocol, kind and native
//! `/v1/responses` (llama.cpp egress design §5, openai egress design §4.3).
//!
//! `llama_cpp` implies kind `llama_server`, and `llama_server` implies
//! `llama_cpp` or `anthropic` (llama decision 11). `openai` is the OpenAI
//! platform and always kind `generic`; `openai_compatible` is `generic` or
//! `audio_cpp` (openai decision 7). Native `/v1/responses` is a choice on
//! `openai_compatible` rows only (openai decision 8). The table's CHECKs
//! enforce all of it, and [`settle`] keeps a write from ever reaching them:
//! it decides from what the caller **sent**, not from the merged row, so a
//! caller who switches the protocol alone is not held to the kind the row
//! happened to have. Both write paths call it — the tool plane's
//! `upstream_set` and the dashboard's `upstream_set_full` — and both add
//! [`Settled::with_host_note`] for the address they store.

use crate::config::{openai_host_note, Protocol, UpstreamKind};

/// The three fields [`settle`] decides: a stored row's, or a create's
/// defaults ([`UpstreamShape::NEW_ROW`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamShape {
    pub protocol: Protocol,
    pub kind: UpstreamKind,
    pub supports_responses: bool,
}

impl UpstreamShape {
    /// What a create starts from: an OpenAI-compatible generic row without
    /// native `/v1/responses` — the defaults of both write paths, and the
    /// UI's.
    pub const NEW_ROW: Self = Self {
        protocol: Protocol::OpenaiCompatible,
        kind: UpstreamKind::Generic,
        supports_responses: false,
    };

    pub fn of(u: &crate::config::Upstream) -> Self {
        Self {
            protocol: u.protocol,
            kind: u.kind,
            supports_responses: u.supports_responses,
        }
    }
}

/// What the write stores, and what it has to say about the difference to
/// what was sent (`notes`, for the response message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub shape: UpstreamShape,
    pub notes: Vec<String>,
}

impl Settled {
    /// `message`, followed by the notes when there are any.
    pub fn message(&self, message: impl Into<String>) -> String {
        let message = message.into();
        if self.notes.is_empty() {
            message
        } else {
            format!("{message} ({})", self.notes.join("; "))
        }
    }

    /// With the note an `openai` row on a host that is not OpenAI's carries
    /// (openai egress design, decision 13): accepted, and said. `base_url` is
    /// the address the write stores.
    pub fn with_host_note(mut self, base_url: &str) -> Self {
        self.notes
            .extend(openai_host_note(self.shape.protocol, base_url));
        self
    }
}

/// The protocols a write may send, for its error texts.
pub const PROTOCOLS: &str = "openai|openai_compatible|anthropic|gemini|llama_cpp";

/// A sent `protocol`: absent or blank is "not sent".
pub fn sent_protocol(s: Option<&str>) -> Result<Option<Protocol>, String> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => Protocol::parse(v)
            .map(Some)
            .ok_or_else(|| format!("invalid protocol '{v}' ({PROTOCOLS})")),
    }
}

/// A sent `kind`: absent or blank is "not sent"; otherwise every spelling an
/// owner may store (`ops::common::parse_upstream_kind` — `sd_cpp` is refused
/// by name rather than reaching the table's CHECK).
pub fn sent_kind(s: Option<&str>) -> Result<Option<UpstreamKind>, String> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => super::parse_upstream_kind(v).map(Some),
    }
}

/// The rule table of the llama design's §5 and the openai design's §4.3.
/// `protocol`, `kind` and `supports_responses` are what the caller sent
/// (`None`: not sent); `current` is the stored row, or
/// [`UpstreamShape::NEW_ROW`] on a create.
///
/// | protocol sent | kind sent | result |
/// |---|---|---|
/// | `llama_cpp` | none or `llama_server` | `llama_cpp` + `llama_server` |
/// | `llama_cpp` | anything else | refused, by name |
/// | `openai` | none or `generic` | `openai` + `generic` |
/// | `openai`/`anthropic`/`gemini` | `audio_cpp` | refused: an audio.cpp server is `openai_compatible` |
/// | `anthropic`/`gemini` | none, current kind `audio_cpp` | kind `generic`, said |
/// | `openai`/`openai_compatible` | `llama_server` | refused, by name: use protocol `llama_cpp` |
/// | `anthropic` | `llama_server` | as sent |
/// | `gemini` | `llama_server` | refused, by name |
/// | `openai` | none, current kind `audio_cpp` or `llama_server` | kind `generic`, said |
/// | `openai_compatible`/`gemini` | none, current kind `llama_server` | kind `generic` (leaving llama), said |
/// | `anthropic` | none, current kind `llama_server` | kind kept |
/// | none | `generic`/`audio_cpp`, current `llama_cpp` | refused: change the protocol |
/// | none | `audio_cpp`, current `openai`/`anthropic`/`gemini` | refused: send `openai_compatible` as well |
/// | none | `llama_server`, current `openai`/`openai_compatible`/`gemini` | refused, as the rows above |
/// | anything else | | as sent |
///
/// `supports_responses` stands only on an `openai_compatible` result. On any
/// other it is refused when sent `true`, by protocol, and switched off (said
/// in the response) when the row had it on.
pub fn settle(
    protocol: Option<Protocol>,
    kind: Option<UpstreamKind>,
    supports_responses: Option<bool>,
    current: UpstreamShape,
) -> Result<Settled, String> {
    use Protocol::{Anthropic, Gemini, LlamaCpp, Openai, OpenaiCompatible};
    use UpstreamKind::{AudioCpp, Generic, LlamaServer};

    let mut notes = Vec::new();
    // A kind alone asks for it on the row's protocol, which then answers
    // for it as if it had been sent.
    let protocol = match (protocol, kind) {
        (None, Some(LlamaServer))
            if matches!(current.protocol, Openai | OpenaiCompatible | Gemini) =>
        {
            Some(current.protocol)
        }
        (None, Some(AudioCpp)) if current.protocol == Openai => {
            return Err(
                "this upstream speaks protocol openai, the OpenAI platform, which is always \
                 kind generic — an audio.cpp server is openai_compatible: send protocol \
                 openai_compatible as well"
                    .into(),
            )
        }
        (None, Some(AudioCpp)) if matches!(current.protocol, Anthropic | Gemini) => {
            return Err(format!(
                "this upstream speaks protocol {}; kind audio_cpp goes with protocol \
                 openai_compatible only — send protocol openai_compatible as well",
                current.protocol.as_str()
            ))
        }
        _ => protocol,
    };
    let (protocol, kind) = match (protocol, kind) {
        (Some(LlamaCpp), None | Some(LlamaServer)) => {
            if kind.is_none() && current.kind != LlamaServer {
                notes.push(
                    "kind set to llama_server, the only kind protocol llama_cpp goes with".into(),
                );
            }
            (LlamaCpp, LlamaServer)
        }
        (Some(LlamaCpp), Some(other)) => {
            return Err(format!(
                "protocol llama_cpp is always kind llama_server (llama.cpp's own server, official \
                 or ik_llama.cpp), not '{}' — leave the kind out or send llama_server",
                other.as_str()
            ))
        }
        (Some(p @ (Openai | OpenaiCompatible)), Some(LlamaServer)) => {
            return Err(format!(
                "kind llama_server cannot go with protocol {}: a llama-server is reached over \
                 protocol llama_cpp — use protocol llama_cpp (or anthropic, for llama-server's \
                 /v1/messages)",
                p.as_str()
            ))
        }
        (Some(p @ (Anthropic | Gemini)), Some(AudioCpp)) => {
            return Err(format!(
                "kind audio_cpp cannot go with protocol {}: an audio.cpp server is reached over \
                 protocol openai_compatible",
                p.as_str()
            ))
        }
        (Some(p @ (Anthropic | Gemini)), None) if current.kind == AudioCpp => {
            notes.push(format!(
                "kind set to generic: an audio.cpp server is reached over openai_compatible, \
                 not {}",
                p.as_str()
            ));
            (p, Generic)
        }
        (Some(Openai), Some(AudioCpp)) => {
            return Err(
                "protocol openai is the OpenAI platform; an audio.cpp server is \
                 openai_compatible"
                    .into(),
            )
        }
        (Some(Openai), None) if current.kind != Generic => {
            notes.push(format!(
                "kind set to generic: protocol openai is the OpenAI platform, which is always \
                 generic, not {}",
                current.kind.as_str()
            ));
            (Openai, Generic)
        }
        (Some(Anthropic), Some(LlamaServer)) => (Anthropic, LlamaServer),
        (Some(Gemini), Some(LlamaServer)) => {
            return Err(
                "kind llama_server cannot go with protocol gemini: llama-server does not serve \
                 the Gemini API — use protocol llama_cpp (or anthropic, for its /v1/messages)"
                    .into(),
            )
        }
        (Some(p @ (OpenaiCompatible | Gemini)), None) if current.kind == LlamaServer => {
            notes.push(format!(
                "kind set to generic: a llama_server is reached over llama_cpp or anthropic, \
                 not {}",
                p.as_str()
            ));
            (p, Generic)
        }
        (None, Some(k @ (Generic | AudioCpp))) if current.protocol == LlamaCpp => {
            return Err(format!(
                "this upstream speaks protocol llama_cpp, which is always kind llama_server — to \
                 make it kind '{}', send its new protocol as well (openai_compatible, \
                 anthropic or gemini)",
                k.as_str()
            ))
        }
        (p, k) => (p.unwrap_or(current.protocol), k.unwrap_or(current.kind)),
    };

    let supports_responses = if protocol == OpenaiCompatible {
        supports_responses.unwrap_or(current.supports_responses)
    } else {
        let why = match protocol {
            Openai => {
                "an openai upstream, the OpenAI platform, has no flag for it: /v1/responses goes \
                 native there, except a request only lmgw's own loop can serve"
            }
            LlamaCpp => {
                "lmgw serves /v1/responses on a llama_cpp upstream from /v1/chat/completions, \
                 so every request goes through its llama.cpp egress"
            }
            Anthropic | Gemini => {
                "neither Anthropic's nor Gemini's API serves /v1/responses, so lmgw \
                 synthesizes it"
            }
            OpenaiCompatible => unreachable!("the branch above"),
        };
        match supports_responses {
            Some(true) => {
                return Err(format!(
                    "supports_responses cannot be on for a {} upstream: {why} — leave it off \
                     (it is a choice on openai_compatible upstreams only)",
                    protocol.as_str()
                ))
            }
            None if current.supports_responses => {
                notes.push(format!("native /v1/responses switched off: {why}"));
                false
            }
            _ => false,
        }
    };

    Ok(Settled {
        shape: UpstreamShape {
            protocol,
            kind,
            supports_responses,
        },
        notes,
    })
}

#[cfg(test)]
mod tests;
