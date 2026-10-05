//! A session's key policy (realtime design §10.2, §10.3).
//!
//! Scope and budget are checked with the session's key **before the 101**
//! for every alias the session starts with — chat, ASR and TTS — and again
//! whenever a `session.update` changes one ([`check`]). Every model call —
//! each chat stream, each ASR call, a response's first TTS call — is
//! checked again with [`check_call`], which also counts it against the
//! key's per-minute limits: rate limits count model calls, not the session,
//! while the session's one concurrency slot is held by the session itself
//! (§10.3). A refusal writes its `request_logs` row like any other, because
//! a refusal is traffic.
//!
//! Every row says `realtime` (`ClientProto::Realtime`, §11), like the
//! session's model calls.

use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::asr::AsrResolution;
use super::resolve::ChatResolution;
use super::tts::TtsResolution;

/// The aliases a session must be allowed to use before it opens (§10.2),
/// with the request class each is logged under: the chat alias, and the ASR
/// and TTS aliases (`RequestClass::Audio`).
pub(crate) fn session_aliases<'a>(
    chat: &'a ChatResolution,
    asr: &'a AsrResolution,
    tts: &'a TtsResolution,
) -> Vec<(&'a str, RequestClass)> {
    let chat = chat.alias.as_deref().map(|a| (a, RequestClass::Chat));
    let asr = asr.alias.as_deref().map(|a| (a, RequestClass::Audio));
    let tts = tts.alias.as_deref().map(|a| (a, RequestClass::Audio));
    chat.into_iter().chain(asr).chain(tts).collect()
}

/// Scope + budget for one alias with the session's key.
pub(crate) async fn check(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    crate::proxy::policy_checked(state, ClientProto::Realtime, ctx, alias, class).await
}

/// The per-model-call check (§10.3): scope and budget as [`check`], and the
/// call counted against the key's requests/minute and tokens/minute — never
/// against its concurrency, whose slot the session already holds.
pub(crate) async fn check_call(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    class: RequestClass,
) -> Result<(), GatewayError> {
    crate::proxy::policy_checked_call(state, ClientProto::Realtime, ctx, alias, class).await
}

/// [`check`] for every alias of [`session_aliases`], first refusal wins.
pub(crate) async fn check_session(
    state: &SharedState,
    ctx: &RequestCtx,
    chat: &ChatResolution,
    asr: &AsrResolution,
    tts: &TtsResolution,
) -> Result<(), GatewayError> {
    for (alias, class) in session_aliases(chat, asr, tts) {
        check(state, ctx, alias, class).await?;
    }
    Ok(())
}
