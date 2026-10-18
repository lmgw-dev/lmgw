//! `/v1/models`' word for this route (realtime design §12): once a session
//! could speak — an ASR alias and a TTS alias are configured — every chat
//! model lists `/v1/realtime` among its `capabilities.endpoints`, since any
//! chat alias can answer a voice session; and `/v1/realtime/calls` beside
//! it while the WebRTC socket group of the listener the request came in on
//! is up (realtime calls design §9), so a client is told calls work where
//! it asks — once calls work at all ([`CALLS_READY`]).
//!
//! Applied where the list is rendered (`server::list_models`,
//! `server::model_by_id`), not inside `capabilities::exposed`: the alias
//! checks below go through `exposed_entry` themselves.

use crate::server::{ExposedEntry, Via};
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

/// Whether `/v1/models` lists `/v1/realtime/calls` at all: yes, since a
/// call works end to end — its audio is wired both ways (WP3) and its
/// session reads its liveness (WP5) — so the per-listener rule of
/// [`calls`] decides (calls design §9's dated note). The switch stays, for
/// a release that has to take calls off the list again.
pub const CALLS_READY: bool = true;

/// Whether a call POSTed on `via` could be answered now: calls are ready
/// ([`CALLS_READY`]) and its listener's socket group is bound (calls §3.1):
/// the plain group on the plain listener, the device group, which follows
/// the device listener, on that one.
pub fn calls(state: &SharedState, via: Via) -> bool {
    CALLS_READY && state.calls.serves(super::calls::Group::of(via))
}

/// Add the routes to a chat model's endpoints: `/v1/realtime`, and
/// `/v1/realtime/calls` when `calls` ([`calls`]).
pub fn advertise(e: &mut ExposedEntry, calls: bool) {
    if let Some(c) = e.capabilities.as_mut().filter(|c| c.task == "chat") {
        let paths = [Some(super::PATH), calls.then_some(super::CALLS_PATH)];
        for path in paths.into_iter().flatten() {
            if !c.endpoints.iter().any(|p| p == path) {
                c.endpoints.push(path.to_string());
            }
        }
    }
}
