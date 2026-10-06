//! Transcribing a voice-library clip (audio-class gap 5): the transcript a
//! cloning model takes as `reference_text` ([`transcribe_voice_clip`]).
//!
//! **Through the gate as any transcription** (changed 2026-10-06, the
//! owner's ruling: a configured fallback is always used, with no exception
//! by content). The GPU hold's swap and admission's outside-VRAM verdict
//! hand the clip to the model's fallback as they would any request, and the
//! request row says who answered. Until then a clip was kept off the hold's
//! fallback, pinned at admission and held to a local speech-to-text row, so
//! it never left the machine: a rule by content, now removed. To keep clips
//! on the machine, choose a speech-to-text model with no fallback.
//!
//! **What stays is capability.** The model that answers — the alias named,
//! or the fallback the gate hands the clip to — must be a speech-to-text
//! model (capability task `asr`, [`crate::ops::validate_stt_alias`], the
//! check every speech-to-text setting is saved with). A TTS or chat model is
//! refused `asr_required` before anything is sent: judged on the name the
//! resolve settled on before admission starts anything, and again on the
//! fallback admission hands the clip to, if it does.
//!
//! **Who answered is said** (chat-voice §4: what served is always visible):
//! [`ClipTranscript`] names the fallback that transcribed the clip, which the
//! clip's `transcript_source`, the op's message and the Audio lab's toast
//! repeat, and a refusal's request row keeps the swap's headers.
//!
//! The send and the request row are the in-process transcription's own
//! ([`super::transcribe_labelled`]).

use bytes::Bytes;

use crate::error::GatewayError;
use crate::gate::{FallbackReason, OpenFailed, Opened, RouteCheck};
use crate::ingress::ClientProto;
use crate::state::SharedState;

use super::{transcribe_labelled, RequestCtx, Upload};

/// The code a model that does not transcribe is refused with.
pub(crate) const ASR_REQUIRED: &str = "asr_required";

/// A clip's transcript, and who wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipTranscript {
    pub text: String,
    /// The fallback that transcribed it in the alias's place, and why the
    /// gate handed it there; `None` when the alias answered itself.
    pub answered_by: Option<(String, FallbackReason)>,
}

impl ClipTranscript {
    /// The model that wrote it: the fallback, else `alias`.
    pub fn by<'a>(&'a self, alias: &'a str) -> &'a str {
        self.answered_by.as_ref().map_or(alias, |(f, _)| f.as_str())
    }
}

/// Who wrote a clip's transcript, in a sentence's words: `'audio/asr'`, or
/// `'cloud-asr' (the GPU hold's fallback for 'audio/asr')`.
pub fn answered_line(alias: &str, answered_by: Option<&(String, FallbackReason)>) -> String {
    let Some((fallback, reason)) = answered_by else {
        return format!("'{alias}'");
    };
    let why = match reason {
        FallbackReason::Hold => "the GPU hold's fallback",
        FallbackReason::Benchmark => "a benchmark run's fallback",
        FallbackReason::ExternalVram => "the fallback while VRAM outside lmgw is short",
        FallbackReason::Background | FallbackReason::Unavailable => "the fallback",
    };
    format!("'{fallback}' ({why} for '{alias}')")
}

/// Transcribe the voice-library clip `bytes` with `alias`, whose answering
/// model must be a speech-to-text model (module doc). Writes one
/// `request_logs` row, like [`super::transcribe`].
pub async fn transcribe_voice_clip(
    state: &SharedState,
    alias: &str,
    bytes: Bytes,
    filename: &str,
    mime: &str,
) -> Result<ClipTranscript, GatewayError> {
    let upload = Upload {
        alias,
        bytes,
        filename,
        mime,
        stop: None,
        language: None,
        clip: true,
    };
    transcribe_labelled(
        state,
        &RequestCtx::default(),
        ClientProto::OpenaiChat,
        upload,
    )
    .await
    .map(|t| ClipTranscript {
        answered_by: t
            .headers
            .fallback()
            .zip(t.headers.fallback_reason())
            .map(|(f, r)| (f.to_string(), r)),
        text: t.text,
    })
    .map_err(|u| u.error)
}

/// Refuse `name` unless it is a speech-to-text model (module doc).
async fn transcribes(state: &SharedState, name: &str) -> Result<(), GatewayError> {
    crate::ops::validate_stt_alias(state, name)
        .await
        .map_err(|message| GatewayError::InvalidRequest {
            code: ASR_REQUIRED,
            message: format!(
                "a voice clip is transcribed only by a speech-to-text model: {message}"
            ),
        })
}

/// The gate for [`transcribe_voice_clip`] (module doc).
pub(super) async fn open(state: &SharedState, alias: &str) -> Result<Opened, OpenFailed> {
    let routed = crate::gate::resolve(state, alias, RouteCheck::Audio).await?;
    // The model the resolve settled on — the hold's or a benchmark's
    // fallback included — before admission starts anything for it.
    let settled = routed.headers().fallback().unwrap_or(alias).to_string();
    if let Err(error) = transcribes(state, &settled).await {
        // The swap's headers stay on the refusal's row: it names the
        // fallback the clip would have gone to (review V15).
        return Err(OpenFailed {
            route: None,
            headers: routed.headers().clone(),
            error,
        });
    }
    let opened = routed.admit(state).await?;
    // And the fallback admission handed the clip to, when it did.
    let handed = opened
        .headers
        .fallback()
        .filter(|f| *f != settled)
        .map(str::to_string);
    if let Some(fallback) = handed {
        if let Err(error) = transcribes(state, &fallback).await {
            let Opened { route, headers, .. } = opened;
            return Err(OpenFailed {
                route: Some(Box::new(route)),
                headers,
                error,
            });
        }
    }
    Ok(opened)
}
