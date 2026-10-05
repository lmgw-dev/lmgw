//! Speech-to-text as an in-process call: the same gate, upload and log row as
//! `POST /v1/audio/transcriptions`, for callers inside the gateway (the Chat
//! tab turning an audio attachment into a transcript, chat-complete design
//! §8) that have bytes and want a string.

use std::time::Instant;

use bytes::Bytes;
use serde_json::Value;

use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::audio::{multipart_call, multipart_send, AudioUpload};
use super::stop::{canceled, is_canceled, row_status, stopped};
use super::*;

/// Transcribe `bytes` with the speech-to-text alias `alias`.
///
/// Goes through [`crate::gate::open`] (GPU hold, admission, protocol check) and
/// the audio route's own send, so a local model is started and held exactly as
/// for an HTTP client, and the call writes one `request_logs` row of class
/// `audio` — under an in-process (owner) context, no client key. The reply is
/// read whole: the transcript from the JSON `text` field, or the body itself
/// when the upstream answered plain text.
///
/// `filename` and `mime` describe the upload to the upstream (containers are
/// often recognised by extension). Errors carry the upstream's own message.
pub async fn transcribe(
    state: &SharedState,
    alias: &str,
    bytes: Bytes,
    filename: &str,
    mime: &str,
) -> Result<String, GatewayError> {
    transcribe_as(state, &RequestCtx::default(), alias, bytes, filename, mime).await
}

/// [`transcribe`] on behalf of a client: the row carries `ctx`'s client key,
/// so per-key Usage and budgets see the call. The key's policy is the
/// caller's to check first (`policy_checked_call`); this only records.
pub async fn transcribe_as(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    bytes: Bytes,
    filename: &str,
    mime: &str,
) -> Result<String, GatewayError> {
    let upload = Upload {
        alias,
        bytes,
        filename,
        mime,
        stop: None,
        language: None,
        local_only: false,
    };
    transcribe_labelled(state, ctx, ClientProto::OpenaiChat, upload)
        .await
        .map(|t| t.text)
        .map_err(|u| u.error)
}

/// [`transcribe`] for the Chat, its row labelled `proto` — `chat`, or
/// `admin` for an Admin Chat thread (`web::chat_voice::speech_proto`), as
/// its dictation and speech rows are: an attachment's transcript is the
/// thread's own traffic (WP11 server review n5), not an API client's.
pub(crate) async fn transcribe_for(
    state: &SharedState,
    proto: ClientProto,
    alias: &str,
    bytes: Bytes,
    filename: &str,
    mime: &str,
) -> Result<String, GatewayError> {
    let upload = Upload {
        alias,
        bytes,
        filename,
        mime,
        stop: None,
        language: None,
        local_only: false,
    };
    transcribe_labelled(state, &RequestCtx::default(), proto, upload)
        .await
        .map(|t| t.text)
        .map_err(|u| u.error)
}

/// What a transcription came to: the text, the language the upstream
/// reported, and the gate's headers — who answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transcript {
    pub text: String,
    /// The `language` field of the upstream's JSON answer, when it has one
    /// (a model that detects the language says which it heard).
    pub language: Option<String>,
    /// `x-lmgw-fallback` and its reason when a fallback answered, for the
    /// caller's response.
    pub headers: GateHeaders,
}

impl Transcript {
    /// The alias that answered in place of the one asked for — the GPU
    /// hold's or the outside-VRAM verdict's fallback; `None` when the alias
    /// asked for answered itself (chat-voice design §4.4: mic audio takes
    /// the fallbacks exactly as configured, and what served is shown).
    pub(crate) fn answered_by(&self) -> Option<&str> {
        self.headers.fallback()
    }
}

/// A transcription that failed, and the gate's headers: when a fallback had
/// the audio, the error goes out naming it — the audio may have left this
/// machine although nothing came back (chat-voice design §4.4).
#[derive(Debug)]
pub(crate) struct Untranscribed {
    pub error: GatewayError,
    pub headers: GateHeaders,
}

impl Untranscribed {
    /// The fallback the audio went to, as [`Transcript::answered_by`].
    pub(crate) fn answered_by(&self) -> Option<&str> {
        self.headers.fallback()
    }
}

/// The Chat's dictation (chat-voice design §5): a recording transcribed by
/// the thread's ASR alias, with the language the thread's user speaks, through
/// the same gate as any request — so its GPU-hold and outside-VRAM fallbacks
/// answer as configured, and say so in [`Transcript::answered_by`] — or,
/// when it fails, in [`Untranscribed::answered_by`]. The row carries
/// `proto`, the label the thread's turns carry: `chat`
/// (`ClientProto::Chat`), or `admin` for an Admin Chat thread.
/// `filename` and `mime` say its container to the upstream (OpenAI's
/// Whisper reads the format from the extension). `stop` ends it at its next
/// await, as [`transcribe_turn`]'s does: the page aborted the upload, and
/// the stopped call still writes its row, `canceled`.
pub(crate) async fn transcribe_dictation(
    state: &SharedState,
    proto: ClientProto,
    alias: &str,
    (bytes, filename, mime): (Bytes, &str, &str),
    language: Option<&str>,
    stop: &StopSignal,
) -> Result<Transcript, Untranscribed> {
    let upload = Upload {
        alias,
        bytes,
        filename,
        mime,
        stop: Some(stop),
        language,
        local_only: false,
    };
    transcribe_labelled(state, &RequestCtx::default(), proto, upload).await
}

/// A realtime session's ASR of one committed turn (realtime design §10.3,
/// §11): [`transcribe_as`], its row labelled `realtime`. The upload is the
/// turn as a 16 kHz mono WAV. The answer names the fallback that answered,
/// when one did (chat-voice design §4.4).
///
/// `stop` ends it at its next await — the gate, the upstream's answer, its
/// body — the way a response's model call ends (`stream_once_on`): a
/// session that is gone does not keep the ASR model held for a transcript
/// nobody reads, for as long as the upstream's `request_timeout` allows, or
/// for good when that is 0 (WP1c review #4). The stopped call still writes
/// its row, `canceled`.
///
/// `language` goes up as the `language` field when given — the two-letter
/// ISO 639-1 code the session's `audio.input.transcription.language` names
/// (`realtime::input::word_check`), for every turn and word check.
pub(crate) async fn transcribe_turn(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    wav: Bytes,
    stop: &StopSignal,
    language: Option<&str>,
) -> Result<Transcript, GatewayError> {
    let upload = Upload {
        alias,
        bytes: wav,
        filename: "input.wav",
        mime: "audio/wav",
        stop: Some(stop),
        language,
        local_only: false,
    };
    transcribe_labelled(state, ctx, ClientProto::Realtime, upload)
        .await
        .map_err(|u| u.error)
}

/// What goes up, to which alias, and the caller's stop.
struct Upload<'a> {
    alias: &'a str,
    bytes: Bytes,
    filename: &'a str,
    mime: &'a str,
    stop: Option<&'a StopSignal>,
    /// The spoken language, an ISO-639-1 code, when the caller knows it.
    language: Option<&'a str>,
    /// Only a local speech-to-text row may hear it ([`local`]).
    local_only: bool,
}

async fn transcribe_labelled(
    state: &SharedState,
    ctx: &RequestCtx,
    proto: ClientProto,
    upload: Upload<'_>,
) -> Result<Transcript, Untranscribed> {
    let Upload {
        alias,
        bytes,
        filename,
        mime,
        stop,
        language,
        local_only,
    } = upload;
    let started = Instant::now();
    // Dropped unfinished (the caller went away mid-call), the guard abandons the
    // request instead of leaking the active gauge.
    let in_flight = super::count::InFlight::start(&state.telemetry);
    let mut fields = vec![
        MultipartField::Text("model".into(), alias.to_string()),
        MultipartField::File(
            "file".into(),
            filename.to_string(),
            (!mime.is_empty()).then(|| mime.to_string()),
            bytes,
        ),
    ];
    if let Some(language) = language.map(str::trim).filter(|l| !l.is_empty()) {
        fields.push(MultipartField::Text(
            "language".into(),
            language.to_string(),
        ));
    }
    // The stop is raced inside, so a call stopped once the gate opened keeps
    // its route on the row (`multipart_call`).
    let called = if local_only {
        match local::open(state, alias).await {
            Ok(opened) => {
                multipart_send(
                    state,
                    opened,
                    &fields,
                    AudioUpload::Transcription,
                    started,
                    stop,
                )
                .await
            }
            Err(f) => Err((f.route, f.headers, f.error)),
        }
    } else {
        multipart_call(
            state,
            alias,
            &fields,
            AudioUpload::Transcription,
            started,
            stop,
        )
        .await
    };
    let outcome = match called {
        Ok(o) => o,
        Err((route, headers, e)) => {
            record(
                log(
                    state,
                    ctx,
                    proto,
                    alias,
                    started,
                    route.as_deref(),
                    headers.fallback_reason(),
                ),
                row_status(&e),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            in_flight.finished();
            return Err(Untranscribed { error: e, headers });
        }
    };
    let status = outcome.resp.status().as_u16();
    let fallback = outcome.headers.fallback_reason();
    let body = tokio::select! {
        biased;
        () = stopped(stop) => Err(canceled("stopped by the caller while the transcript was read")),
        b = outcome.resp.bytes() => b.map_err(|e| GatewayError::Transport(e.to_string())),
    };
    let result = body.and_then(|b| transcript_of(&b, outcome.headers.clone()));
    // A stop is a 200 `canceled` row, whatever the upstream had answered.
    let status = match &result {
        Err(e) if is_canceled(e) => row_status(e),
        _ => status,
    };
    let error = result.as_ref().err().map(|e| (e.kind(), e.to_string()));
    record(
        log(
            state,
            ctx,
            proto,
            alias,
            started,
            Some(&outcome.route),
            fallback,
        ),
        status,
        Some(outcome.ttfb_ms),
        Usage::default(),
        error,
    )
    .await;
    in_flight.finished();
    // The model stops counting as in use once the transcript is in hand.
    drop(outcome.admission);
    result.map_err(|error| Untranscribed {
        error,
        headers: outcome.headers,
    })
}

mod local;
pub use local::{local_asr_row, transcribe_local_only};
pub(crate) mod warm;

/// The log row's parameters: the audio route's own, minus everything that only
/// a chat call has.
fn log<'a>(
    state: &'a SharedState,
    ctx: &'a RequestCtx,
    proto: ClientProto,
    alias: &str,
    started: Instant,
    route: Option<&'a crate::config::Route>,
    fallback: Option<crate::gate::FallbackReason>,
) -> LogParams<'a> {
    LogParams {
        state,
        proto,
        ctx,
        alias: alias.to_string(),
        route,
        started,
        streamed: false,
        class: RequestClass::Audio,
        timings: None,
        max_tokens_clamped: None,
        fallback,
        rung: None,
    }
}

/// A transcription reply: OpenAI's `{"text": …}` (with its `language`, when
/// the upstream says one), or a plain-text body (`response_format=text`
/// upstreams).
fn transcript_of(body: &[u8], headers: GateHeaders) -> Result<Transcript, GatewayError> {
    let (text, language) = match serde_json::from_slice::<Value>(body) {
        Ok(v) => match v.get("text").and_then(Value::as_str) {
            Some(t) => (
                t.trim().to_string(),
                v.get("language")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string),
            ),
            None => {
                return Err(GatewayError::Internal(
                    "the transcription reply has no 'text' field".into(),
                ))
            }
        },
        Err(_) => (String::from_utf8_lossy(body).trim().to_string(), None),
    };
    Ok(Transcript {
        text,
        language,
        headers,
    })
}
