//! `/v1/models`' word for this route (realtime design §12): once a session
//! could speak — an ASR alias and a TTS alias are configured — every chat
//! model lists `/v1/realtime` among its `capabilities.endpoints`, since any
//! chat alias can answer a voice session.
//!
//! Applied where the list is rendered (`server::list_models`,
//! `server::model_by_id`), not inside `capabilities::exposed`: the alias
//! checks below go through `exposed_entry` themselves.

use crate::server::ExposedEntry;
use crate::state::SharedState;

/// Whether a session could transcribe and speak: the ASR alias a session
/// would take (`realtime.asr_alias`, else the Chat's transcription model —
/// the first one set decides, as `asr::resolve` has it) is an ASR alias, and
/// `realtime.tts_alias` is a TTS alias.
pub async fn speaks(state: &SharedState) -> bool {
    let snap = state.snapshot();
    let rt = &snap.settings.realtime;
    let asr = [rt.asr_alias.trim(), snap.settings.chat_stt_alias.trim()]
        .into_iter()
        .find(|a| !a.is_empty());
    let tts = rt.tts_alias.trim();
    match asr {
        Some(asr) if !tts.is_empty() => {
            super::asr::is_asr_alias(state, asr).await && super::tts::is_tts_alias(state, tts).await
        }
        _ => false,
    }
}

/// Add the route to a chat model's endpoints.
pub fn advertise(e: &mut ExposedEntry) {
    if let Some(c) = e.capabilities.as_mut().filter(|c| c.task == "chat") {
        if !c.endpoints.iter().any(|p| p == super::PATH) {
            c.endpoints.push(super::PATH.to_string());
        }
    }
}
