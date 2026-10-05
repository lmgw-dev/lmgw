//! The voice a response speaks with, settled at its first clause on the
//! route it holds (realtime design §5.3, §9.1, §9.2; WP3 review M1, M2).
//!
//! - **A fallback answers** (the GPU hold, or VRAM lmgw cannot free): the
//!   voice is resolved once more, against the fallback
//!   ([`voice::for_fallback`]) — for an audio.cpp fallback over its own
//!   voice list, read here on the held route; an OpenAI one by OpenAI's
//!   names; by what its upstream's protocol and kind say it is
//!   ([`voice::Engine`], package B review 7) — or the response fails with
//!   `voice_not_configured` naming the fallback. Logged either way.
//! - **The voice is provisional** (nothing known without a request showed
//!   the model has it, `voice`'s module doc): an audio.cpp model's list is
//!   read on the held route — the model is up and held, so this starts
//!   nothing — and a voice it does not have fails the response with
//!   `voice_not_found` (`voice_not_configured` for one the owner's settings
//!   named, B2 review 4); the session stays open. The list goes back to the
//!   session core, so later updates are judged against it. A list that
//!   cannot be read decides nothing: the voice is spoken and the engine is
//!   the judge, as for a cloud TTS, which has no list to read.
//!
//! Every read is raced against the response's stop, which the end of the
//! session raises too: a container that never answers holds nobody.

use super::super::super::voice::{self, VoiceFacts, VoiceVia};
use super::Speech;
use crate::config::UpstreamKind;
use crate::error::GatewayError;
use crate::proxy::synthesize::Synthesis;
use crate::proxy::{is_canceled, StopSignal};
use crate::state::SharedState;

/// The `voice` this response's clauses are sent with (module doc). `report`
/// gets the model's list when it was read for the alias the session named.
pub(super) async fn settle(
    state: &SharedState,
    synth: &Synthesis,
    speech: &Speech,
    stop: &StopSignal,
    report: impl FnOnce(Vec<String>),
) -> Result<Option<String>, GatewayError> {
    let alias = speech.alias.as_str();
    let label = speech.label.as_str();
    let settings = state.snapshot().settings.realtime.clone();
    if let Some(fallback) = synth.fallback() {
        let facts = match voice::Engine::of(synth.protocol(), synth.kind()) {
            voice::Engine::AudioCpp => {
                let mut f = voice::facts(state, fallback, Some(label)).await;
                f.listed = read(synth, stop, label, fallback).await?;
                Some(f)
            }
            voice::Engine::OpenAi => None,
            voice::Engine::Other(what) => {
                let e = voice::not_configured(format!(
                    "TTS alias '{alias}' is answered by its fallback '{fallback}' (the GPU hold, \
                     or VRAM lmgw cannot free), {what}, which is neither audio.cpp nor an OpenAI \
                     TTS: lmgw knows no voice to name for it"
                ));
                tracing::info!("{label}: {e}");
                return Err(e);
            }
        };
        let v = voice::for_fallback(
            alias,
            fallback,
            &speech.requested,
            facts.as_ref(),
            &settings,
        )
        .inspect_err(|e| tracing::info!("{label}: {e}"))?;
        tracing::info!(
            "{label}: TTS '{alias}' is answered by its fallback '{fallback}', speaking '{}' for \
             '{}' ({})",
            v.name,
            voice::echo(&speech.requested),
            v.via.knob()
        );
        return Ok(v.send);
    }
    let v = &speech.voice;
    let Some(send) = v.send.as_deref().filter(|_| !v.verified) else {
        return Ok(v.send.clone());
    };
    if synth.kind() != UpstreamKind::AudioCpp {
        // No list to check against: the engine is the judge.
        return Ok(Some(send.to_string()));
    }
    let Some(listed) = read(synth, stop, label, alias).await? else {
        return Ok(Some(send.to_string()));
    };
    let facts = VoiceFacts {
        listed: Some(listed.clone()),
        ..speech.facts.clone()
    };
    report(listed);
    if facts.knows(send) {
        tracing::info!("{label}: voice '{send}' verified for TTS '{alias}'");
        return Ok(Some(send.to_string()));
    }
    // A name the owner's settings gave is the owner's to fix: not
    // configured, not "not found" (B2 review 4).
    let why = voice::lacks(alias, send, v.via);
    Err(match v.via {
        VoiceVia::Model | VoiceVia::Library => voice::not_found(why),
        _ => voice::not_configured(why),
    })
}

/// The model's voice list on the held route — `None` when it cannot be
/// read, which is logged under `label`; a stop is the response's end.
async fn read(
    synth: &Synthesis,
    stop: &StopSignal,
    label: &str,
    alias: &str,
) -> Result<Option<Vec<String>>, GatewayError> {
    match synth.voice_names(Some(stop)).await {
        Ok(names) => Ok(Some(names)),
        Err(e) if is_canceled(&e) => Err(e),
        Err(e) => {
            tracing::warn!("{label}: the voice list of TTS model '{alias}' could not be read: {e}");
            Ok(None)
        }
    }
}
