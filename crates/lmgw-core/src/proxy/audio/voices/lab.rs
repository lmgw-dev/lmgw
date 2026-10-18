//! `GET /audio-lab/api/voices?model=<alias>` — the voice list the Audio lab's
//! picker shows, read the moment a model is selected.
//!
//! **It never starts anything.** A local row is answered from lmgw's own
//! catalog (presets, the voices its package ships, `embeddings/`, the voice
//! library: [`crate::audio::voices`]), which is complete, needs no container
//! and works under the GPU hold or a benchmark's lease (then `held: true`).
//! audio.cpp's own list lacks the shipped voices, so reading only that one
//! showed just the clips recorded into the voice library.
//!
//! When the row's container happens to be up, its list is read the
//! no-admission way ([`super::super::voices_if_running`]: no claim, so a look
//! at the lab does not reset the idle timer, and no restart of a dead one)
//! and the names the catalog lacks are added as [`LabVoiceKind::Engine`] — a
//! row whose `extra_run_args` mount another voice directory has clips only
//! the engine sees. A remote route has no container and no catalog: it is
//! asked, like the public route does, through the same no-admission reader
//! (this path makes no admission call at all). A local row the GPU hold (or a
//! busy card) re-routes to a fallback alias answers its own catalog with
//! `held: true` and names the fallback; the fallback's provider is not called.
//! A local route with no audio row behind it is refused.

use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::audio_lab::{LabVoice, LabVoiceKind, LabVoices, LabVoicesSource};

use super::super::error_response;
use crate::audio::voices::{row_of_route, row_speech, VoiceKind};
use crate::config::AudioModel;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::state::SharedState;

const PROTO: ClientProto = ClientProto::OpenaiChat;

/// `GET /audio-lab/api/voices` (module doc).
pub async fn lab_voices(state: SharedState, alias: &str) -> Response {
    let routed =
        match crate::gate::resolve(&state, alias, crate::gate::RouteCheck::AudioVoices).await {
            Ok(r) => r,
            Err(f) => {
                // The hold is a zero VRAM allowance, not a model that is
                // gone: a config read takes none (the public route's rule).
                let held = matches!(
                    f.error,
                    GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }
                );
                if held {
                    if let Some(row) = held_row(&state, alias) {
                        return Json(catalog(&state, &row, true).await).into_response();
                    }
                }
                return f.headers.stamp(error_response(PROTO, &f.error));
            }
        };
    // The gate re-routed the request (the hold's or a busy card's fallback):
    // the alias itself is a local row, whose catalog is the answer. The
    // fallback is only named; asking its provider for a list would be a call
    // made because a page was opened.
    if let Some(fallback) = routed.headers().fallback().map(str::to_owned) {
        return match held_row(&state, alias) {
            Some(row) => {
                let mut answer = catalog(&state, &row, true).await;
                answer.fallback = Some(fallback);
                routed.headers().stamp(Json(answer).into_response())
            }
            None => refuse_not_local_audio(&routed, alias),
        };
    }
    let local = crate::vram::classify(routed.resolved()).is_some();
    let row = {
        let snap = state.snapshot();
        row_of_route(&snap, routed.resolved()).cloned()
    };
    match row {
        Some(row) => {
            let mut answer = catalog(&state, &row, false).await;
            // The row's container, only if it is already up.
            if let Some((_, Ok(bytes))) = super::super::voices_if_running(&state, alias).await {
                answer.engine_asked = true;
                add_engine_names(&mut answer, &bytes);
            }
            routed.headers().stamp(Json(answer).into_response())
        }
        // A local route with no audio row behind it (a snapshot reloaded
        // between the gate and this read, say) has no catalog: refused.
        None if local => refuse_not_local_audio(&routed, alias),
        None => {
            // Remote: asked through the no-admission reader, which takes no
            // claim and starts nothing. This path makes no admission call.
            match super::super::voices_if_running(&state, alias).await {
                Some((headers, Ok(bytes))) => {
                    let mut answer = LabVoices {
                        source: LabVoicesSource::Upstream,
                        engine_asked: true,
                        ..LabVoices::default()
                    };
                    add_engine_names(&mut answer, &bytes);
                    headers.stamp(Json(answer).into_response())
                }
                Some((headers, Err(e))) => headers.stamp(error_response(PROTO, &e)),
                None => routed.headers().stamp(error_response(
                    PROTO,
                    &GatewayError::Unsupported(format!("model '{alias}' did not answer")),
                )),
            }
        }
    }
}

fn refuse_not_local_audio(routed: &crate::gate::Routed, alias: &str) -> Response {
    let e = GatewayError::Unsupported(format!(
        "model '{alias}' is not a local audio model with a voice catalog"
    ));
    routed.headers().stamp(error_response(PROTO, &e))
}

/// The local audio row `alias` lands on, resolved past the hold, when it
/// passes the route's own checks.
fn held_row(state: &SharedState, alias: &str) -> Option<AudioModel> {
    let snap = state.snapshot();
    let route = snap.resolve(alias).ok()?;
    crate::proxy::require_openai_audio(&route, alias).ok()?;
    crate::proxy::refuse_non_audio_voices(&route, alias).ok()?;
    row_of_route(&snap, &route).cloned()
}

/// The row's own list: lmgw's catalog (module doc).
async fn catalog(state: &SharedState, row: &AudioModel, held: bool) -> LabVoices {
    let speech = row_speech(state, row).await;
    let v = &speech.voices;
    LabVoices {
        voices: v
            .entries
            .iter()
            .map(|e| LabVoice {
                id: e.id.clone(),
                kind: match e.kind {
                    VoiceKind::Preset => LabVoiceKind::Preset,
                    VoiceKind::Library => LabVoiceKind::Library,
                    VoiceKind::Native => LabVoiceKind::Native,
                    VoiceKind::Embedding => LabVoiceKind::Embedding,
                },
                send: e.send.to_string(),
                transcript: e.transcript,
            })
            .collect(),
        default: v.default.clone(),
        missing: v.missing.clone(),
        needs_transcript: crate::audio::transcript::refuses_untranscribed(row, &speech.profile),
        held,
        source: LabVoicesSource::Catalog,
        engine_asked: false,
        fallback: None,
    }
}

/// Add the names in an engine's body the list lacks, as engine-only.
fn add_engine_names(answer: &mut LabVoices, body: &[u8]) {
    for name in crate::proxy::synthesize::parse_voice_names(body) {
        // Native voices match in any case (`audio/profile.rs`), so a native
        // entry already answers to the engine's spelling; an empty name would
        // collide with the picker's "(model default)".
        let known = answer.voices.iter().any(|v| {
            if v.kind == LabVoiceKind::Native {
                v.id.eq_ignore_ascii_case(&name)
            } else {
                v.id == name
            }
        });
        if !name.is_empty() && !known {
            answer.voices.push(LabVoice {
                id: name,
                kind: LabVoiceKind::Engine,
                send: "voice".into(),
                transcript: None,
            });
        }
    }
    answer.voices.sort_by(|a, b| a.id.cmp(&b.id));
}
