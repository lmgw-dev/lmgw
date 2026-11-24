//! Whether a conversion's key may still call a model now (document-conversion
//! design §7.5, D6): the one check every model call a conversion makes asks
//! before it goes out: the page-reading and the picture-description
//! model's (`model_help/call.rs::allowed`, which `model_help/turn.rs` asks
//! for a page's read and a picture's description) and the speech-to-text
//! model's (`media/asr.rs`).
//!
//! The key is read again by its identity (a job outlives the request that
//! started it: a key disabled, rotated or deleted since stops its calls),
//! then its expiry, its scope and its budget for `alias`
//! ([`crate::policy::check_alias`]), and its tokens-per-minute window
//! ([`crate::policy::PolicyGate::check_call`]). The call is not counted
//! against the key's requests-per-minute window, nor asked about it: the
//! conversion is the request the key made, and its model calls are that
//! request's work, as a `/v1/responses` run's turns are.

use crate::error::GatewayError;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// Whether `ctx`'s key may call `alias` now (module doc).
pub(crate) async fn may_call(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
) -> Result<(), GatewayError> {
    let snap = state.snapshot();
    let key = match ctx.principal.key_id() {
        None => None,
        Some(id) => match snap.api_keys.iter().find(|k| k.id == id) {
            Some(k) if k.enabled && ctx.principal.presents(k) => Some(k),
            _ => {
                return Err(GatewayError::Unauthorized(
                    "the API key this conversion was started with has since been disabled, \
                     rotated or deleted",
                ))
            }
        },
    };
    let name = key.map(|k| k.name.as_str()).or(ctx.client_key.as_deref());
    crate::policy::check_alias(&state.policy, &state.db, &snap, name, alias).await?;
    match key {
        Some(key) => state.policy.check_call(key, chrono::Utc::now()),
        None => Ok(()),
    }
}
