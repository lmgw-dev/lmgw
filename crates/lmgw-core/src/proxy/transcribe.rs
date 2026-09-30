//! Speech-to-text as an in-process call: the same gate, upload and log row as
//! `POST /v1/audio/transcriptions`, for callers inside the gateway (the Chat
//! tab turning an audio attachment into a transcript, chat-complete design
//! §8) that have bytes and want a string.

use std::time::Instant;

use bytes::Bytes;
use serde_json::Value;

use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::audio::{multipart_call, AudioUpload};
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
    let started = Instant::now();
    // Dropped unfinished (the caller went away mid-call), the guard abandons the
    // request instead of leaking the active gauge.
    let in_flight = super::count::InFlight::start(&state.telemetry);
    let ctx = RequestCtx::default();
    let fields = vec![
        MultipartField::Text("model".into(), alias.to_string()),
        MultipartField::File(
            "file".into(),
            filename.to_string(),
            (!mime.is_empty()).then(|| mime.to_string()),
            bytes,
        ),
    ];
    let outcome =
        match multipart_call(state, alias, &fields, AudioUpload::Transcription, started).await {
            Ok(o) => o,
            Err((route, headers, e)) => {
                record(
                    log(
                        state,
                        &ctx,
                        alias,
                        started,
                        route.as_ref(),
                        headers.fallback_reason(),
                    ),
                    e.http_status().as_u16(),
                    None,
                    Usage::default(),
                    Some((e.kind(), e.to_string())),
                )
                .await;
                in_flight.finished();
                return Err(e);
            }
        };
    let status = outcome.resp.status().as_u16();
    let fallback = outcome.headers.fallback_reason();
    let body = outcome
        .resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Transport(e.to_string()));
    let result = body.and_then(|b| transcript_text(&b));
    let error = result.as_ref().err().map(|e| (e.kind(), e.to_string()));
    record(
        log(state, &ctx, alias, started, Some(&outcome.route), fallback),
        status,
        Some(outcome.ttfb_ms),
        Usage::default(),
        error,
    )
    .await;
    in_flight.finished();
    // The model stops counting as in use once the transcript is in hand.
    drop(outcome.admission);
    result
}

/// The log row's parameters: the audio route's own, minus everything that only
/// a chat call has.
fn log<'a>(
    state: &'a SharedState,
    ctx: &'a RequestCtx,
    alias: &str,
    started: Instant,
    route: Option<&'a crate::config::Route>,
    fallback: Option<crate::gate::FallbackReason>,
) -> LogParams<'a> {
    LogParams {
        state,
        proto: ClientProto::OpenaiChat,
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

/// The text of a transcription reply: OpenAI's `{"text": …}`, or a plain-text
/// body (`response_format=text` upstreams).
fn transcript_text(body: &[u8]) -> Result<String, GatewayError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => match v.get("text").and_then(Value::as_str) {
            Some(t) => Ok(t.trim().to_string()),
            None => Err(GatewayError::Internal(
                "the transcription reply has no 'text' field".into(),
            )),
        },
        Err(_) => Ok(String::from_utf8_lossy(body).trim().to_string()),
    }
}
