//! Which chat alias answers a session (realtime design §5.1).
//!
//! The name a client sends is resolved in this order:
//! 1. a name in `realtime.model_map`, the mapped alias — the owner's explicit
//!    intent, so it wins over everything, a same-named alias included;
//! 2. an OpenAI Realtime name (`gpt-realtime*`, `gpt-4o*-realtime*`),
//!    `realtime.default_model` — **never** resolved as a chat alias: a
//!    catch-all upstream (`expose_all`) would otherwise take the name and
//!    route a voice session to a chat endpoint that has no such model, and
//!    the stock SDKs send exactly these names;
//! 3. a lmgw chat alias, used as is;
//! 4. anything else is unknown: a 404 at the handshake, an `error` on a
//!    `session.update`.
//!
//! No name at all is allowed — `@openai/agents` given an explicit URL sends no
//! `?model=` — and starts on `realtime.default_model`. **Every substitution is
//! visible**: the session echoes the result in `lmgw.resolved.chat`, and the
//! log line names the rule that produced it.
//!
//! "A chat alias" is exactly what `POST /v1/chat/completions` accepts at the
//! gate (`gate::RouteCheck::Text`): a name the snapshot resolves, or a
//! candidate alias, that is not an image or audio model. Resolving does not
//! start anything and ignores the GPU hold — a held local model is still a
//! valid choice, and each model call routes around the hold itself (§9.2).

use crate::config::Snapshot;
use crate::error::GatewayError;

/// What a session's chat model resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResolution {
    /// The chat alias that answers. `None` only for a model-less session on a
    /// gateway with no `realtime.default_model`: the session still opens, and
    /// the first response reports the missing model.
    pub alias: Option<String>,
    /// Which rule produced it.
    pub via: Via,
}

/// The rule of §5.1 that resolved a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// `realtime.model_map`.
    ModelMap,
    /// An OpenAI Realtime name, answered by `realtime.default_model`.
    RealtimeName,
    /// The name is itself a chat alias.
    Alias,
    /// No name was sent: `realtime.default_model`.
    Default,
}

/// Resolve the client's chat model name — `None` when it sent none.
pub fn resolve_chat(
    snap: &Snapshot,
    requested: Option<&str>,
) -> Result<ChatResolution, GatewayError> {
    let rt = &snap.settings.realtime;
    let default = || {
        let d = rt.default_model.trim();
        (!d.is_empty()).then(|| d.to_string())
    };
    let Some(name) = requested.map(str::trim).filter(|n| !n.is_empty()) else {
        let alias = default();
        if let Some(a) = &alias {
            check_setting(snap, "realtime.default_model", a)?;
        }
        return Ok(ChatResolution {
            alias,
            via: Via::Default,
        });
    };

    if let Some(mapped) = rt.model_map.get(name) {
        check_setting(snap, &format!("realtime.model_map[\"{name}\"]"), mapped)?;
        return Ok(ChatResolution {
            alias: Some(mapped.clone()),
            via: Via::ModelMap,
        });
    }
    if is_openai_realtime_name(name) {
        let Some(alias) = default() else {
            return Err(GatewayError::UnknownAlias(format!(
                "{name} (an OpenAI Realtime model name, which lmgw answers with the setting \
                 realtime.default_model — and that is not set)"
            )));
        };
        check_setting(snap, "realtime.default_model", &alias)?;
        return Ok(ChatResolution {
            alias: Some(alias),
            via: Via::RealtimeName,
        });
    }
    if is_chat_alias(snap, name) {
        return Ok(ChatResolution {
            alias: Some(name.to_string()),
            via: Via::Alias,
        });
    }
    Err(GatewayError::UnknownAlias(name.to_string()))
}

/// `gpt-realtime*` and `gpt-4o*-realtime*` (§5.1) — the names the stock SDKs
/// default to (`@openai/agents` sends `gpt-realtime-2.1`).
pub fn is_openai_realtime_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("gpt-realtime") || (n.starts_with("gpt-4o") && n.contains("-realtime"))
}

/// Whether `name` is a chat alias in the sense `/v1/chat/completions` uses.
pub fn is_chat_alias(snap: &Snapshot, name: &str) -> bool {
    if snap.candidate_alias(name).is_some() {
        return true;
    }
    snap.resolve(name)
        .is_ok_and(|route| crate::proxy::refuse_media_route(&route, name, super::PATH).is_ok())
}

/// A setting that names an alias must name a chat alias — and when it does
/// not, the refusal names the *setting*, because that is what the owner has
/// to fix, not the name the client sent.
fn check_setting(snap: &Snapshot, setting: &str, alias: &str) -> Result<(), GatewayError> {
    if is_chat_alias(snap, alias) {
        return Ok(());
    }
    Err(GatewayError::UnknownAlias(format!(
        "{alias} (named by the setting {setting}, and not a chat alias this gateway serves)"
    )))
}

/// The log line for a resolution (§5.1: every substitution is logged).
pub fn log_resolution(session_id: &str, requested: Option<&str>, r: &ChatResolution) {
    let alias = r.alias.as_deref().unwrap_or("(none)");
    match (r.via, requested) {
        (Via::Alias, _) => {
            tracing::info!("realtime {session_id}: chat model '{alias}'");
        }
        (Via::ModelMap, Some(name)) => tracing::info!(
            "realtime {session_id}: chat model '{name}' → '{alias}' (realtime.model_map)"
        ),
        (Via::RealtimeName, Some(name)) => tracing::info!(
            "realtime {session_id}: chat model '{name}' is an OpenAI Realtime name → '{alias}' \
             (realtime.default_model)"
        ),
        _ if r.alias.is_none() => tracing::info!(
            "realtime {session_id}: no chat model named and realtime.default_model is not set — \
             the session opens, and a response will say so"
        ),
        _ => tracing::info!(
            "realtime {session_id}: no chat model named → '{alias}' (realtime.default_model)"
        ),
    }
}
