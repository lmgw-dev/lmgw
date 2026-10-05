//! `POST /chat/api/threads/{id}/transcribe` (chat-voice design §5): the
//! dictation's recording, transcribed by the thread's ASR alias.
//!
//! The body is the recording, its container named by the `Content-Type`:
//! `audio/wav` (also `audio/wave`, `audio/x-wav`, `audio/vnd.wave`, and a
//! body with no type or `application/octet-stream` — the page's WAV),
//! `audio/webm`, `audio/ogg` (`audio/opus`), `audio/mpeg` (`audio/mp3`),
//! `audio/mp4` (`audio/m4a`, `audio/x-m4a`) or `audio/flac`
//! (`audio/x-flac`). It goes up under that type with a matching file name
//! (`dictation.webm`, …): OpenAI's Whisper reads the format from the
//! extension. Any other type is `415 unsupported_media_type`, naming those.
//! The answer is `{text, alias, asr_answered_by, asr_ms, audio_ms,
//! language}`:
//! - `alias` — the ASR alias the thread resolves to (thread → Settings →
//!   Chat → Voice → realtime's);
//! - `asr_answered_by` — the alias that answered in its place (the GPU
//!   hold's or the outside-VRAM verdict's fallback, taken exactly as
//!   configured, §4.4), `null` when it answered itself;
//! - `asr_ms` — the transcription call, admission included;
//! - `audio_ms` — the recording's length, from its WAV header and data size
//!   (nothing is decoded; `null` for any other container, or a body that is
//!   not a WAV this reads — it is sent on all the same);
//! - `language` — what the upstream says it heard, in its own spelling (the
//!   thread's language is ISO 639-1, `de`; OpenAI's `verbose_json` names
//!   it, `german`), else the language the thread's user speaks (never the
//!   reply language), else `null`.
//!
//! **A page that aborts the upload** (Esc, the thread left) stops the call
//! rather than dropping it: it runs in its own task with a stop
//! (`proxy::stop_pair`) this handler holds, so it ends at its next await and
//! still writes its request row, `canceled` — the audio may already be with
//! a provider, and that is traffic the Logs and Usage pages show.
//!
//! **The body limit is the configured one.** The route runs without axum's
//! 2 MiB default (`DefaultBodyLimit::disable()` where it is mounted) and
//! reads its body through `chat::read_upload_body`, bounded by
//! `max_body_mb` (0 = no bound) like an attachment: without that, dictation
//! would be cut at about 65 s. A body over the bound is `413 body_limit`,
//! whose message names the setting and its value.
//!
//! Errors: `404` for a thread that is not there, `400 empty_audio`, `422
//! asr_not_configured` (the message names Settings → Chat → Voice), and the
//! transcription's own — `503 gpu_hold` for a GPU row with no usable
//! fallback under the hold — as `{code, message, asr_answered_by}`: a
//! fallback that had the audio is named even when it failed, since the
//! audio may have left this machine. Every answer carries the gate's
//! `x-lmgw-fallback` headers when a fallback answered.

use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::proxy::stop_pair;
use crate::state::SharedState;

use super::super::chat::{err_json, read_upload_body};
use super::super::chat_extract::ChatPath;
use super::super::chat_repo::ChatRepo;
use super::resolve::resolve;

/// `POST /chat/api/threads/{id}/transcribe` (module doc).
pub(crate) async fn transcribe(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(thread) = ChatRepo::of(id).thread(&state, id).await.ok().flatten() else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    let snap = state.snapshot();
    let cfg = resolve(&snap, &thread);
    let Some(alias) = cfg.asr.alias.clone() else {
        let message = cfg
            .problems
            .iter()
            .find(|p| p.stage == "asr")
            .map(|p| p.message.clone())
            .unwrap_or_else(|| "no speech-to-text model is set (Settings → Chat → Voice)".into());
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            "asr_not_configured",
            message,
        );
    };
    let (mime, filename) = match container(&headers) {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let body = match read_upload_body(body, &headers, snap.settings.max_body_mb).await {
        Ok(b) => b,
        Err(refused) => return refused,
    };
    if body.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_audio",
            "the recording is empty",
        );
    }
    let audio_ms = wav_ms(&body);
    let language = cfg.language.value.clone();
    let started = Instant::now();
    // Its own task, stopped when this handler is dropped (the page aborted
    // the upload): the call ends at its next await and still writes its row,
    // `canceled` — the audio may already be with a provider.
    let (_stop, signal) = stop_pair();
    let call = {
        let (state, alias, language) = (state.clone(), alias.clone(), language.clone());
        let proto = super::speech_proto(&thread);
        tokio::spawn(async move {
            let recording = (body, filename, mime);
            let language = language.as_deref();
            crate::proxy::transcribe_dictation(&state, proto, &alias, recording, language, &signal)
                .await
        })
    };
    let transcribed = match call.await {
        Ok(t) => t,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("the transcription ended without an answer: {e}"),
            )
        }
    };
    match transcribed {
        Ok(t) => {
            let answer = Json(json!({
                "text": t.text,
                "alias": alias,
                "asr_answered_by": t.answered_by(),
                "asr_ms": started.elapsed().as_millis() as u64,
                "audio_ms": audio_ms,
                "language": t.language.clone().or(language),
            }));
            t.headers.stamp(answer.into_response())
        }
        // The fallback that had the audio is named even when it failed: the
        // audio may have left this machine (§4.4).
        Err(u) => {
            let e = &u.error;
            let answer = Json(json!({
                "code": e.kind(),
                "message": e.to_string(),
                "asr_answered_by": u.answered_by(),
            }));
            u.headers.stamp((e.http_status(), answer).into_response())
        }
    }
}

/// The recording's MIME type and the file name it goes up as, from the
/// request's `Content-Type` (module doc), or the `415`.
fn container(headers: &HeaderMap) -> Result<(&'static str, &'static str), Response> {
    let Some(ct) = headers.get(header::CONTENT_TYPE) else {
        return Ok(("audio/wav", "dictation.wav"));
    };
    let essence = ct
        .to_str()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Ok(match essence.as_str() {
        "audio/wav"
        | "audio/wave"
        | "audio/x-wav"
        | "audio/vnd.wave"
        | "application/octet-stream"
        | "" => ("audio/wav", "dictation.wav"),
        "audio/webm" => ("audio/webm", "dictation.webm"),
        "audio/ogg" | "audio/opus" => ("audio/ogg", "dictation.ogg"),
        "audio/mpeg" | "audio/mp3" => ("audio/mpeg", "dictation.mp3"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => ("audio/mp4", "dictation.m4a"),
        "audio/flac" | "audio/x-flac" => ("audio/flac", "dictation.flac"),
        _ => {
            return Err(err_json(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                format!(
                    "the recording's Content-Type is '{}': dictation sends WAV (audio/wav), \
                     WebM (audio/webm), Ogg (audio/ogg), MP3 (audio/mpeg), M4A (audio/mp4) or \
                     FLAC (audio/flac)",
                    ct.to_str().unwrap_or("not text")
                ),
            ))
        }
    })
}

/// The recording's length in milliseconds, from its WAV header and data
/// size; `None` for a body this does not read as a PCM WAV.
fn wav_ms(body: &[u8]) -> Option<u64> {
    crate::realtime::audio::pcm::wav_duration_ms(body).ok()
}

#[cfg(test)]
mod tests {
    use super::{container, wav_ms};

    #[test]
    fn a_wav_s_length_comes_from_its_samples_and_rate() {
        let samples = vec![0i16; 24_000];
        let wav = crate::realtime::audio::pcm::write_wav_pcm16_mono(&samples, 16_000).unwrap();
        assert_eq!(wav_ms(&wav), Some(1_500));
        assert_eq!(wav_ms(b"not a wav"), None);
    }

    #[test]
    fn the_content_type_names_the_upload_and_anything_else_is_415() {
        use axum::http::{header, HeaderMap, HeaderValue};
        let of = |ct: Option<&'static str>| {
            let mut h = HeaderMap::new();
            if let Some(ct) = ct {
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            }
            container(&h).map_err(|r| r.status().as_u16())
        };
        assert_eq!(of(None), Ok(("audio/wav", "dictation.wav")));
        assert_eq!(of(Some("audio/x-wav")), Ok(("audio/wav", "dictation.wav")));
        assert_eq!(
            of(Some("audio/webm;codecs=opus")),
            Ok(("audio/webm", "dictation.webm"))
        );
        assert_eq!(of(Some("Audio/Ogg")), Ok(("audio/ogg", "dictation.ogg")));
        assert_eq!(of(Some("audio/mpeg")), Ok(("audio/mpeg", "dictation.mp3")));
        assert_eq!(of(Some("audio/x-m4a")), Ok(("audio/mp4", "dictation.m4a")));
        assert_eq!(of(Some("audio/flac")), Ok(("audio/flac", "dictation.flac")));
        assert_eq!(of(Some("multipart/form-data; boundary=x")), Err(415));
        assert_eq!(of(Some("video/webm")), Err(415));
    }
}
