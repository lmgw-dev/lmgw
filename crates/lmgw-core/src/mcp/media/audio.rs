//! `audio__speak` and `audio__transcribe` (chat-media-tools design §4.2):
//! the in-process speech calls the Chat's voice already makes, as tools.
//!
//! - **speak**: the key's policy, then `synthesize::open` on the alias (the
//!   gate, the hold swap, admission — one route for the whole text), the
//!   text cut into clauses as a spoken reply is (`realtime::clauses`), one
//!   request per clause, the WAVs joined into one file, one TTS row.
//! - **transcribe**: the key's policy, then the dictation's call with the
//!   ref's bytes and the optional language; one ASR row.
//!
//! Before either opens its route, the caller's [`Hooks::before_admit`] sees
//! the route the gate settled on, so the Chat's tool loop lets its GPU claim
//! go for a GPU row and keeps it for a CPU row or a cloud route (§4.3).
//! Both race the call's stop as the voice paths do.

use bytes::Bytes;
use lmgw_api_types::chat_media::{MediaFacts, MediaPhase};
use serde_json::{Map, Value};

use crate::audio::language::SpeechLanguage;
use crate::error::GatewayError;
use crate::gate::RouteCheck;
use crate::proxy::image_call::MediaProgress;
use crate::proxy::synthesize::{self, ClauseSpeech};
use crate::realtime::audio::pcm;
use crate::realtime::clauses::ClauseAggregator;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::args::{language, model, required, str_arg};
use super::list::Offer;
use super::result::{secs, Outcome, Produced};
use super::{refs, CallCx, AUDIO_SPEAK, AUDIO_TRANSCRIBE};

/// The gate's route for `alias`, shown to the caller's hook before the call
/// opens its own (the same resolve, nothing started).
/// Then the call is at work, `phase` (design §6.2).
async fn before_admit(state: &SharedState, alias: &str, cx: &CallCx<'_>, phase: MediaPhase) {
    if let Ok(routed) = crate::gate::resolve(state, alias, RouteCheck::Audio, cx.ctx.origin()).await
    {
        cx.hooks.before_admit(routed.resolved()).await;
    }
    cx.hooks.progress(&MediaProgress::Stage {
        phase,
        model: alias.to_string(),
        room: None,
    });
}

pub(super) async fn speak(
    state: &SharedState,
    args: &Map<String, Value>,
    offer: &Offer,
    cx: &CallCx<'_>,
) -> Result<Outcome, String> {
    let alias = model(args, offer, AUDIO_SPEAK)?;
    let text = required(args, "text")?;
    // Left out, the default voice when it belongs to this model (§3.8).
    let voice = str_arg(args, "voice")?.or_else(|| offer.voice_for(alias));
    let instructions = str_arg(args, "instructions")?;
    let lang = language(args)?.map(SpeechLanguage::Request);
    crate::proxy::policy_checked_call(state, cx.proto, cx.ctx, alias, RequestClass::Audio)
        .await
        .map_err(|e| e.to_string())?;
    before_admit(state, alias, cx, MediaPhase::Speaking).await;
    let started = std::time::Instant::now();
    let mut syn = synthesize::open(
        state,
        cx.ctx,
        cx.proto,
        &format!("tool {AUDIO_SPEAK}"),
        alias,
        instructions,
        voice,
        cx.stop,
    )
    .await
    .map_err(|e| e.to_string())?;
    let answered_by = syn.fallback().map(str::to_string);

    let mut clauses = ClauseAggregator::new();
    let mut parts = clauses.push(text);
    parts.extend(clauses.flush());
    let mut wavs: Vec<Bytes> = Vec::new();
    let mut failed: Option<GatewayError> = None;
    for clause in &parts {
        let speech = ClauseSpeech {
            input: clause,
            voice,
            language: lang.as_ref(),
            instructions,
            ..Default::default()
        };
        match syn.speak(&speech, cx.stop).await {
            Ok(spoken) => wavs.push(spoken.audio),
            // A clause with nothing a voice can say (a lone emoji) is
            // skipped, as a spoken reply skips it.
            Err(GatewayError::InvalidRequest {
                code: "empty_input",
                ..
            }) => {}
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    let joined = match (&failed, wavs.is_empty()) {
        (Some(_), _) => None,
        (None, true) => {
            failed = Some(GatewayError::InvalidRequest {
                code: "empty_input",
                message: "the text has nothing a voice can say".into(),
            });
            None
        }
        (None, false) => match join_wavs(&wavs) {
            Ok(j) => Some(j),
            Err(e) => {
                failed = Some(GatewayError::Upstream {
                    status: 502,
                    provider_type: None,
                    message: e,
                });
                None
            }
        },
    };
    let audio_ms = joined
        .as_ref()
        .and_then(|j| pcm::wav_duration_ms(j).ok())
        .unwrap_or(0);
    syn.finish(failed.as_ref(), audio_ms).await;
    if let Some(e) = failed {
        return Err(e.to_string());
    }
    let wav = joined.unwrap_or_default();
    let by = answered_by.as_deref().unwrap_or(alias);
    let stem = crate::web::model_stem(by);
    let name = match voice {
        Some(v) => format!("{stem}-{}.wav", crate::web::model_stem(v)),
        None => format!("{stem}.wav"),
    };
    let fallback = answered_by
        .as_deref()
        .map(|_| format!(", the fallback for {alias}"))
        .unwrap_or_default();
    let voice_note = voice.map(|v| format!(", voice {v}")).unwrap_or_default();
    let detail = format!(
        " ({}) with {by}{fallback}{voice_note}, {} in all.",
        secs(audio_ms),
        secs(started.elapsed().as_millis() as u64)
    );
    let file = Produced {
        kind: "audio",
        mime: "audio/wav".into(),
        name,
        bytes: wav,
        facts: MediaFacts {
            tool: Some(AUDIO_SPEAK.to_string()),
            model: Some(alias.to_string()),
            answered_by,
            voice: voice.map(str::to_string),
            duration_ms: Some(audio_ms),
            language: lang.as_ref().map(|l| l.code().to_string()),
            wall_ms: Some(started.elapsed().as_millis() as u64),
            ..Default::default()
        },
    };
    Ok(Outcome::made(
        AUDIO_SPEAK,
        vec![file],
        (
            "Spoke recording ".to_string(),
            format!("{detail} The user can play it."),
        ),
        format!("Spoke a recording{detail} It is in this result."),
    ))
}

/// The clauses' WAVs as one file, at their common rate. Clauses of one
/// route differing in rate is the engine's error, said as such.
fn join_wavs(wavs: &[Bytes]) -> Result<Vec<u8>, String> {
    let mut samples: Vec<f32> = Vec::new();
    let mut rate = None;
    for w in wavs {
        let wav = pcm::parse_wav(w).map_err(|e| format!("a clause's audio is unreadable: {e}"))?;
        match rate {
            None => rate = Some(wav.rate),
            Some(r) if r != wav.rate => {
                return Err(format!(
                    "the clauses came back at different sample rates ({r} and {} Hz)",
                    wav.rate
                ))
            }
            Some(_) => {}
        }
        samples.extend(wav.samples);
    }
    let rate = rate.ok_or("no audio came back")?;
    pcm::write_wav_pcm16_mono(&pcm::f32_to_pcm16(&samples), rate).map_err(|e| e.to_string())
}

pub(super) async fn transcribe(
    state: &SharedState,
    args: &Map<String, Value>,
    offer: &Offer,
    cx: &CallCx<'_>,
) -> Result<Outcome, String> {
    let alias = model(args, offer, AUDIO_TRANSCRIBE)?;
    let raw = required(args, "audio")?;
    let lang = language(args)?;
    let src = refs::resolve(cx.refs, raw, "audio", "audio").await?;
    crate::proxy::policy_checked_call(state, cx.proto, cx.ctx, alias, RequestClass::Audio)
        .await
        .map_err(|e| e.to_string())?;
    before_admit(state, alias, cx, MediaPhase::Transcribing).await;
    // The dictation's call takes a stop; a caller with none gets one that is
    // never raised.
    let (_keep, never) = crate::proxy::stop_pair();
    let stop = cx.stop.unwrap_or(&never);
    let t = crate::proxy::transcribe_dictation(
        state,
        cx.ctx,
        cx.proto,
        alias,
        (Bytes::from(src.bytes), &src.name, &src.mime),
        lang.as_deref(),
        stop,
    )
    .await
    .map_err(|u| u.error.to_string())?;
    let of = src.id.map(|i| format!(" of att:{i}")).unwrap_or_default();
    let by = match t.answered_by() {
        Some(fb) => format!("{fb}, the fallback for {alias}"),
        None => alias.to_string(),
    };
    let heard = t
        .language
        .as_deref()
        .map(|l| format!(" (language {l})"))
        .unwrap_or_default();
    Ok(Outcome::text(
        AUDIO_TRANSCRIBE,
        format!("Transcript{of} by {by}{heard}:\n\n{}", t.text),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clauses_join_into_one_file_at_their_rate() {
        let a = pcm::write_wav_pcm16_mono(&[0; 2400], 24_000).unwrap();
        let b = pcm::write_wav_pcm16_mono(&[0; 4800], 24_000).unwrap();
        let joined = join_wavs(&[Bytes::from(a), Bytes::from(b)]).unwrap();
        assert_eq!(pcm::wav_duration_ms(&joined).unwrap(), 300);
        let c = pcm::write_wav_pcm16_mono(&[0; 1600], 16_000).unwrap();
        let mixed = join_wavs(&[Bytes::from(joined), Bytes::from(c)]).unwrap_err();
        assert!(mixed.contains("different sample rates"), "{mixed}");
    }
}
