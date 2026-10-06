//! What an upstream write stores for its protocol, kind and native
//! `/v1/responses` (llama.cpp egress design §5).
//!
//! `llama_cpp` implies kind `llama_server`, and `llama_server` implies
//! `llama_cpp` or `anthropic` (decision 11); a `llama_cpp` row never forwards
//! `/v1/responses` natively (decision 19). The table's CHECKs enforce both, and
//! [`settle`] keeps a write from ever reaching them: it decides from what the
//! caller **sent**, not from the merged row, so a caller who switches the
//! protocol alone is not held to the kind the row happened to have. Both
//! write paths call it — the tool plane's `upstream_set` and the dashboard's
//! `upstream_set_full`.

use crate::config::{Protocol, UpstreamKind};

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
    /// native `/v1/responses` — the defaults of both write paths.
    pub const NEW_ROW: Self = Self {
        protocol: Protocol::Openai,
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
}

/// A sent `protocol`: absent or blank is "not sent".
pub fn sent_protocol(s: Option<&str>) -> Result<Option<Protocol>, String> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => Protocol::parse(v)
            .map(Some)
            .ok_or_else(|| format!("invalid protocol '{v}' (openai|anthropic|gemini|llama_cpp)")),
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

/// The rule table of design §5, plus decision 19. `protocol`, `kind` and
/// `supports_responses` are what the caller sent (`None`: not sent);
/// `current` is the stored row, or [`UpstreamShape::NEW_ROW`] on a create.
///
/// | protocol sent | kind sent | result |
/// |---|---|---|
/// | `llama_cpp` | none or `llama_server` | `llama_cpp` + `llama_server` |
/// | `llama_cpp` | anything else | refused, by name |
/// | `openai` | `llama_server` | `llama_cpp` (the old spelling), said in the response |
/// | `anthropic` | `llama_server` | as sent |
/// | `gemini` | `llama_server` | refused |
/// | `openai`/`gemini` | none, current kind `llama_server` | kind `generic` (leaving llama) |
/// | `anthropic` | none, current kind `llama_server` | kind kept |
/// | none | `generic`/`audio_cpp`, current `llama_cpp` | refused: change the protocol |
/// | none | `llama_server`, current protocol `openai`/`gemini` | as the `openai`/`gemini` rows above |
/// | anything else | | as sent |
///
/// On a `llama_cpp` result `supports_responses` is refused when sent `true`,
/// and switched off (said in the response) when the row had it on.
pub fn settle(
    protocol: Option<Protocol>,
    kind: Option<UpstreamKind>,
    supports_responses: Option<bool>,
    current: UpstreamShape,
) -> Result<Settled, String> {
    use Protocol::{Anthropic, Gemini, LlamaCpp, Openai};
    use UpstreamKind::{Generic, LlamaServer};

    let mut notes = Vec::new();
    // "kind llama_server" alone asks for a llama-server on the row's protocol.
    let protocol = match (protocol, kind) {
        (None, Some(LlamaServer)) if matches!(current.protocol, Openai | Gemini) => {
            Some(current.protocol)
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
        (Some(Openai), Some(LlamaServer)) => {
            notes.push(
                "kind llama_server on protocol openai is the old spelling of protocol llama_cpp, \
                 which was stored"
                    .into(),
            );
            (LlamaCpp, LlamaServer)
        }
        (Some(Anthropic), Some(LlamaServer)) => (Anthropic, LlamaServer),
        (Some(Gemini), Some(LlamaServer)) => {
            return Err(
                "kind llama_server cannot go with protocol gemini: llama-server does not serve \
                 the Gemini API — use protocol llama_cpp (or anthropic, for its /v1/messages)"
                    .into(),
            )
        }
        (Some(p @ (Openai | Gemini)), None) if current.kind == LlamaServer => {
            notes.push(format!(
                "kind set to generic: a llama_server is reached over llama_cpp or anthropic, \
                 not {}",
                p.as_str()
            ));
            (p, Generic)
        }
        (None, Some(k @ (Generic | UpstreamKind::AudioCpp))) if current.protocol == LlamaCpp => {
            return Err(format!(
                "this upstream speaks protocol llama_cpp, which is always kind llama_server — to \
                 make it kind '{}', send its new protocol as well (openai, anthropic or gemini)",
                k.as_str()
            ))
        }
        (p, k) => (p.unwrap_or(current.protocol), k.unwrap_or(current.kind)),
    };

    let supports_responses = if protocol == LlamaCpp {
        match supports_responses {
            Some(true) => {
                return Err(
                    "supports_responses cannot be on for a llama_cpp upstream: lmgw serves \
                     /v1/responses on it from /v1/chat/completions, so every request goes \
                     through its llama.cpp egress — leave it off"
                        .into(),
                )
            }
            None if current.supports_responses => {
                notes.push(
                    "native /v1/responses switched off: a llama_cpp upstream gets it \
                     synthesized from /v1/chat/completions"
                        .into(),
                );
                false
            }
            _ => false,
        }
    } else {
        supports_responses.unwrap_or(current.supports_responses)
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
