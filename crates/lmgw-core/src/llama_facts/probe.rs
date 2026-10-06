//! One external row's `/props` probe (llama egress design §4.2), and what its
//! answer means to the cache.

use std::sync::Arc;

use crate::config::Upstream;
use crate::egress::llama_cpp::props::{self, LlamaFacts, Props, PropsFailure};

/// What one probe of an external row found, as the cache keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The server's facts about the model, without the body they were read
    /// from (`LlamaFacts::raw` is `Null`): it carries the chat template, and
    /// nothing shown or decided on an external row reads it.
    Facts(Arc<LlamaFacts>),
    /// The server itself, asked without `?model=`, is a llama-server router,
    /// with its `build_info`: it states facts per model, each asked as
    /// `?model=<m>&autoload=false` and kept under a key of its own.
    Router(Option<String>),
    /// The server answered without facts — an old build's 404, a 401, a body
    /// that is no JSON object, a router that would not speak for the model.
    /// The facts are unknown, and that is kept with the server's own answer
    /// until an event says it may be stale.
    Unknown(String),
    /// Nothing worth keeping: a router that has not loaded the model yet, a
    /// server error (llama-server's 503 "Loading model", a reverse proxy's
    /// 502 or 504), or a probe that reached nothing. Never cached — shown
    /// until the next use asks again.
    Retry(String),
}

impl Answer {
    /// The facts, when the server stated them.
    pub fn facts(&self) -> Option<&Arc<LlamaFacts>> {
        match self {
            Self::Facts(f) => Some(f),
            Self::Router(_) | Self::Unknown(_) | Self::Retry(_) => None,
        }
    }

    /// Whether this answer stands until invalidated; a [`Self::Retry`] is
    /// asked again on the next use.
    pub fn cached(&self) -> bool {
        !matches!(self, Self::Retry(_))
    }

    /// Why the facts are unknown, when they are.
    pub fn why_unknown(&self) -> Option<&str> {
        match self {
            Self::Facts(_) | Self::Router(_) => None,
            Self::Unknown(why) | Self::Retry(why) => Some(why),
        }
    }
}

/// Ask the server the row `up` points at about itself: `GET <root>/props`
/// with the row's bearer and headers, bounded by the row's own
/// [`Upstream::request_timeout`] (`None`, the maximum, for `timeout_ms = 0`).
/// A router answers [`Answer::Router`]; its models are asked with
/// [`ask_model`].
pub async fn ask_server(http: &reqwest::Client, up: &Upstream) -> Answer {
    match props::probe(props::request(http, up, None), up.request_timeout()).await {
        Ok(Props::Router { build_info }) => Answer::Router(build_info),
        other => meaning(other),
    }
}

/// Ask the router the row `up` points at about `model`:
/// `GET <root>/props?model=<m>&autoload=false`, which never loads it, sent
/// and bounded as [`ask_server`]'s.
pub async fn ask_model(http: &reqwest::Client, up: &Upstream, model: &str) -> Answer {
    meaning(props::probe(props::request(http, up, Some(model)), up.request_timeout()).await)
}

/// What a probe's answer means to the cache (§4.2).
pub(super) fn meaning(answer: Result<Props, PropsFailure>) -> Answer {
    match answer {
        Ok(Props::Model(facts)) => Answer::Facts(Arc::new(LlamaFacts {
            raw: serde_json::Value::Null,
            ..facts
        })),
        Ok(Props::Router { .. }) => Answer::Unknown(
            "GET /props?model=… answered as a llama-server router again, which states no facts \
             about one model"
                .into(),
        ),
        Err(PropsFailure::Unreachable(e)) => Answer::Retry(format!(
            "GET /props reached no server ({e}); asked again on next use"
        )),
        Err(PropsFailure::Status { body, .. }) if not_loaded(&body) => Answer::Retry(
            "the llama-server router has not loaded this model yet; asked again on next use".into(),
        ),
        // A server error is a moment, not the server's answer: llama-server
        // says 503 "Loading model" on every path but its frontend until the
        // model is ready (a router autoloading on the first send, a server
        // just restarted), and a reverse proxy in front says 502 or 504 while
        // the server behind it is away.
        Err(e @ PropsFailure::Status { status, .. }) if status >= 500 => {
            Answer::Retry(format!("GET /props answered {e}; asked again on next use"))
        }
        Err(e) => Answer::Unknown(format!("GET /props answered {e}")),
    }
}

/// A router's refusal to speak for a model it has not loaded
/// (`server-models.cpp`'s `router_validate_model`, "model is not loaded").
fn not_loaded(body: &str) -> bool {
    body.to_ascii_lowercase().contains("model is not loaded")
}
