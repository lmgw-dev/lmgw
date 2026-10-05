//! The session warm's load of its voice (realtime design §9.1; live run 3b,
//! D3'): one short clause on a claim the warm already holds.
//!
//! audio.cpp loads a lazy row's weights on its first request, so a warm
//! that only started the container left the first spoken clause to pay the
//! load. The warm sends that first request itself: the clause a response
//! would send — the session's voice, seed, language and speech instructions,
//! shaped and preflighted for the row as [`Synthesis::speak`] shapes it — on
//! the claim the caller took. The claim learns what any answered request
//! teaches (`LocalHold::note_sending`, `LocalHold::note_inference`: the
//! pending load is no longer charged, the residency is read), and it writes
//! **no request row**: nobody asked for it, and the owner's Usage page and
//! costs stay what clients sent.
//!
//! [`Synthesis::speak`]: super::Synthesis::speak

use bytes::Bytes;

use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::super::audio::{audio_send, local_speech};
use super::{clause_body, ClauseSpeech};

/// Synthesize `clause` on `route`, a local audio row's container that
/// `hold` claims, to load its model (module doc). `label` is what the log
/// lines start with (the warm's own), and `alias` its TTS alias, for
/// shaping. The answer's WAV, read whole.
pub(crate) async fn load(
    state: &SharedState,
    label: &str,
    alias: &str,
    route: &Route,
    hold: &LocalHold,
    clause: &ClauseSpeech<'_>,
) -> Result<Bytes, GatewayError> {
    let speech = local_speech(state, route).await;
    let (body, _) = clause_body(state, label, route, speech.as_ref(), alias, clause)?;
    hold.note_sending();
    let resp = audio_send(Some(hold), route, |r| {
        let url = format!("{}/audio/speech", r.upstream.base());
        Ok(apply_bearer_auth(
            state.http.post(url).json(&body),
            &r.upstream,
        ))
    })
    .await
    .map_err(|e| crate::audio::engine_errors::explain_speech(e, alias, clause.voice))?;
    let wav = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Transport(e.to_string()))?;
    if let Some((row, _)) = &speech {
        state.audio_rates.learn(&row.model_id, &wav);
    }
    hold.note_inference();
    Ok(wav)
}
