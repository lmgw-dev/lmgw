//! What a llama-server says about itself: `GET /props` (llama egress design
//! §4).
//!
//! One tolerant reader for both servers lmgw talks to, read at their sources:
//!
//! | | official (0c6a6a7) | ik_llama.cpp (7ff619c) |
//! |---|---|---|
//! | `modalities` | `vision`, `audio`, `video` | `vision`, `audio` |
//! | `chat_template_caps` | 9 booleans | `{}` |
//! | context | `default_generation_settings.n_ctx` (per slot) | that, plus a top-level `n_ctx` (the whole context) |
//! | `build_info` | `b<number>-<commit>` | absent |
//! | router mode | without `?model=`: `role: "router"`, no model facts | none |
//!
//! **Unknown means today** (decision 14). Every field of [`LlamaFacts`] is
//! optional, and one the server did not send is unknown, never `false`: ik
//! sends no `video` and an empty `chat_template_caps`, and an older build may
//! send less still. A caller that decides anything on a fact decides only on
//! one that is there.
//!
//! **A router states no model facts.** Asked without `?model=`, a router-mode
//! llama-server answers `role: "router"` with a dummy
//! `default_generation_settings.n_ctx` of 0 (`server-models.cpp:1931-1953`).
//! Read as facts, that would be a model with no context and no modalities, so
//! it is its own answer, [`Props::Router`], and the caller asks about one
//! model with [`request`]'s `model` (`?model=<m>&autoload=false`, which never
//! loads one).
//!
//! **Nothing is keyed on `build_info`** (decision 15): it is kept for a
//! person to read, and ik does not send it.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::config::Upstream;
use crate::egress::{apply_bearer_auth, with_timeout};

/// What one llama-server said about the model it serves (§4.1). Every field
/// is optional, and a missing one is unknown (module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LlamaFacts {
    /// `modalities.vision`: the server loaded a projector that sees.
    pub vision: Option<bool>,
    /// `modalities.audio`: the server loaded a projector that hears.
    pub audio: Option<bool>,
    /// `modalities.video`, official builds only: exactly
    /// `mtmd_helper_support_video`, which is also what decides whether a webp
    /// image decodes (§8.2). ik sends none.
    pub video: Option<bool>,
    /// `chat_template_caps`, the template's own booleans (nine at 0c6a6a7).
    /// `None` when the server sends none; ik sends `{}`, so every cap is
    /// unknown there. Read one with [`Self::cap`].
    pub caps: Option<BTreeMap<String, bool>>,
    /// `default_generation_settings.n_ctx`: one slot's context, in tokens.
    /// Never ik's top-level `n_ctx`, which is the whole context across slots.
    pub n_ctx_slot: Option<u64>,
    /// `b<number>-<commit>` on official builds; ik sends none. For diagnosis
    /// only (module doc).
    pub build_info: Option<String>,
    /// The body as the server sent it. Not serialized: it carries the chat
    /// template, kilobytes that a status frame has no use for. Only a live
    /// read keeps it (the local model test shows it); what is kept — an
    /// external row's cache, a container's registry entry — is `Null`.
    #[serde(skip)]
    pub raw: Value,
}

impl LlamaFacts {
    /// One `chat_template_caps` boolean, `None` when the server did not say.
    pub fn cap(&self, name: &str) -> Option<bool> {
        self.caps.as_ref()?.get(name).copied()
    }
}

/// What a `/props` body says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Props {
    /// The server's facts about the model it runs.
    Model(LlamaFacts),
    /// A router-mode llama-server asked without `?model=` (module doc): no
    /// model facts. Its own `build_info` is kept.
    Router { build_info: Option<String> },
}

impl Props {
    /// Read a `/props` body in either server's shape (module doc). `Err` only
    /// for a body that is not a JSON object; any object reads, with whatever
    /// it does not say left unknown.
    pub fn read(body: Value) -> Result<Self, String> {
        let Some(obj) = body.as_object() else {
            return Err(format!(
                "the body is not a JSON object but {}",
                shape(&body)
            ));
        };
        let build_info = obj
            .get("build_info")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if obj.get("role").and_then(Value::as_str) == Some("router") {
            return Ok(Self::Router { build_info });
        }
        let modality = |k: &str| {
            obj.get("modalities")
                .and_then(|m| m.get(k))
                .and_then(Value::as_bool)
        };
        let caps = obj
            .get("chat_template_caps")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                    .collect()
            });
        // A slot of no context is not a fact about any model: a router's
        // dummy, or a server that does not know yet.
        let n_ctx_slot = obj
            .get("default_generation_settings")
            .and_then(|d| d.get("n_ctx"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0);
        Ok(Self::Model(LlamaFacts {
            vision: modality("vision"),
            audio: modality("audio"),
            video: modality("video"),
            caps,
            n_ctx_slot,
            build_info,
            raw: body,
        }))
    }
}

fn shape(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Why a `/props` probe produced no facts. The three are kept apart because
/// they mean different things to whoever remembers the answer (§4.2): a probe
/// that reached nothing says nothing about the server, one the server
/// answered otherwise (an old build's 404, a 401, a router's "model is not
/// loaded") is the server's own answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropsFailure {
    /// No whole answer: refused, reset, or timed out before the body ended.
    Unreachable(String),
    /// An answer other than a 2xx, with its body as sent.
    Status { status: u16, body: String },
    /// A 2xx whose body is not a JSON object.
    Unreadable(String),
}

impl std::fmt::Display for PropsFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "no answer: {e}"),
            Self::Status { status, body } if body.is_empty() => write!(f, "HTTP {status}"),
            Self::Status { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Unreadable(e) => write!(f, "an unreadable answer: {e}"),
        }
    }
}

/// The `/props` request for an upstream row (§4.2): `GET <root>/props` with
/// the row's bearer and extra headers, the root and the auth as
/// `tokenize_request` derives them for the count (`/props` is not public on a
/// server with a key). `model` asks a router about one model without loading
/// it: `?model=<m>&autoload=false`.
pub fn request(
    http: &reqwest::Client,
    up: &Upstream,
    model: Option<&str>,
) -> reqwest::RequestBuilder {
    let root = up.base().trim_end_matches("/v1");
    let url = format!("{root}/props");
    let rb = match (model, reqwest::Url::parse(&url)) {
        (Some(m), Ok(mut u)) => {
            u.query_pairs_mut()
                .append_pair("model", m)
                .append_pair("autoload", "false");
            http.get(u)
        }
        // A base that is no URL fails at the send, saying so.
        _ => http.get(url),
    };
    apply_bearer_auth(rb, up)
}

/// The `/props` request for the managed container published on `port`: its
/// root on the loopback, with no bearer, as a managed row sets none.
pub fn container_request(http: &reqwest::Client, port: u16) -> reqwest::RequestBuilder {
    http.get(format!("http://127.0.0.1:{port}/props"))
}

/// Send a `/props` request and read the answer. `timeout` bounds the whole
/// exchange, body included; `None` is no bound of lmgw's own, as
/// `timeout_ms = 0` means for a row's requests.
pub async fn probe(
    rb: reqwest::RequestBuilder,
    timeout: Option<Duration>,
) -> Result<Props, PropsFailure> {
    let resp = with_timeout(rb, timeout)
        .send()
        .await
        .map_err(|e| PropsFailure::Unreachable(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| PropsFailure::Unreachable(e.to_string()))?;
    if !status.is_success() {
        return Err(PropsFailure::Status {
            status: status.as_u16(),
            body: text.trim().to_string(),
        });
    }
    let body: Value =
        serde_json::from_str(&text).map_err(|e| PropsFailure::Unreadable(e.to_string()))?;
    Props::read(body).map_err(PropsFailure::Unreadable)
}

#[cfg(test)]
mod tests;
