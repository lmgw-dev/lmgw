//! The cloning check (`POST /voice-lab/api/clone-check`): a cloning model
//! speaks a short test line with a saved sample as its reference, so the
//! owner hears whether the clip clones well.
//!
//! The request is the one a client sends, through the real
//! `POST /v1/audio/speech` handler in process — resolution, the hold and its
//! fallback, shaping, preflight, the log row — and its answer (audio, or the
//! route's error envelope) is relayed as it came, `x-lmgw-fallback`
//! included. With the class's voice directory on the library the sample is
//! named as its `voice`, exactly as a realtime session names it, and
//! audio.cpp hands the engine its transcript; otherwise the clip goes as a
//! `voice_ref` path with its transcript as `reference_text`.

use axum::response::Response;
use lmgw_api_types::voice_lab::VoiceCloneCheck;
use serde::Serialize;

use super::{sentences, LabFail};
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::web::audio_lab::{clip_entries, voices_dir, CONTAINER_VOICES_DIR};

/// The speech request the check sends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CheckBody {
    pub model: String,
    pub input: String,
    pub response_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// The request for `check` (module doc), from the library as it is now.
pub async fn body(state: &SharedState, check: &VoiceCloneCheck) -> Result<CheckBody, LabFail> {
    let alias = check.alias.trim();
    if alias.is_empty() {
        return Err(LabFail::bad("pick the cloning model (alias)"));
    }
    let language = check
        .language
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(sentences::normalize)
        .transpose()
        .map_err(LabFail::bad)?;
    let input = match check
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => t.to_string(),
        None => language
            .as_deref()
            .and_then(sentences::check_line_for)
            .map(str::to_string)
            .ok_or_else(|| LabFail::bad("write the line the model says"))?,
    };
    let dir = voices_dir(state)
        .ok_or_else(|| LabFail::bad(crate::web::audio_lab::no_audio_models_dir()))?;
    let wanted = check.sample.trim().to_string();
    // The library's scan reads the directory and its index: off the runtime.
    let clip = tokio::task::spawn_blocking(move || {
        clip_entries(&dir)
            .into_iter()
            .find(|c| c.voice == wanted || c.name == wanted)
            .ok_or_else(|| LabFail::not_found(format!("no sample '{wanted}' in the voice library")))
    })
    .await
    .map_err(|e| LabFail::internal(e.to_string()))??;
    let library = state.snapshot().settings.audio.voice_dir.trim() == CONTAINER_VOICES_DIR;
    let mut body = CheckBody {
        model: alias.to_string(),
        input,
        response_format: "wav",
        language,
        ..CheckBody::default()
    };
    if library {
        body.voice = Some(clip.voice);
    } else {
        body.voice_ref = Some(format!("{CONTAINER_VOICES_DIR}/{}", clip.name));
        body.reference_text = Some(clip.transcript).filter(|t| !t.trim().is_empty());
    }
    Ok(body)
}

/// Send the check; the speech route's own answer.
pub async fn run(state: &SharedState, check: &VoiceCloneCheck) -> Result<Response, LabFail> {
    let body = body(state, check).await?;
    let value = serde_json::to_value(&body).map_err(|e| LabFail::internal(e.to_string()))?;
    Ok(proxy::handle_audio_speech(state.clone(), RequestCtx::default(), value).await)
}
