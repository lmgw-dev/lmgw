//! OpenAI audio endpoints (§6): byte-level passthrough to the resolved
//! upstream. audio.cpp's `audiocpp_server` speaks `/v1/audio/*` natively, so
//! there is no IR translation here — only alias resolution, model rewrite,
//! auth, and telemetry. Upstream *errors* are still normalized through the
//! openai egress adapter, so a failed audio call carries the same error shape
//! as every other route and its provider message survives into `request_logs`.
//!
//! The live-ingest routes (`/v1/audio/transcriptions/live`, `/v1/audio/speech/live`)
//! are not proxied yet. Their model comes from a `?model=` query parameter
//! (trivial to rewrite), but the request body is an incrementally delivered PCM
//! stream whose response interleaves with the upload — that needs a
//! duplex-capable client path we don't have here, so clients use the
//! container's own port for it.

use std::time::Instant;

use axum::body::Body;
use axum::extract::FromRequest;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Route;
use crate::egress::{apply_bearer_auth, for_protocol};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// The route check of `/v1/audio/*` and `/v1/tasks/*`
/// ([`crate::gate::RouteCheck::Audio`]).
///
/// These routes are byte-level passthroughs of OpenAI's audio shape, so the
/// upstream has to actually speak that shape. An anthropic or gemini upstream
/// has neither the path nor the auth header, and would silently receive
/// nonsense (`https://api.anthropic.com/audio/speech` with a bearer token) —
/// same guard as legacy `/v1/completions`. Real OpenAI TTS/Whisper upstreams
/// stay usable; only the protocol is constrained, not the upstream kind.
///
/// Run by the gate on the route it settled on, so under a GPU hold it judges
/// the fallback's upstream, which is the only order that keeps it honest
/// (gpu-hold design §4).
pub(crate) fn require_openai_audio(route: &Route, alias: &str) -> Result<(), GatewayError> {
    if route.upstream.protocol != crate::config::Protocol::Openai {
        return Err(GatewayError::Unsupported(format!(
            "/v1/audio/* requires an openai-protocol upstream; alias '{alias}' resolves to \
             '{}' ({})",
            route.upstream.name,
            route.upstream.protocol.as_str(),
        )));
    }
    Ok(())
}

/// Send an audio request, bounding only the wait for response *headers* — the
/// body then streams unbounded (TTS and music generation legitimately take
/// minutes). A non-2xx is turned into a [`GatewayError`] carrying the
/// upstream's own message; error bodies are small, so buffering one is worth
/// it to avoid logging a bare "HTTP 500" and relaying a foreign error shape.
async fn audio_send<F>(
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    build: F,
) -> Result<reqwest::Response, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    // Headers only, through the shared dead-container retry (§3.2) — the body
    // that follows is unbounded and unreplayable, and this is the moment a
    // dead container shows itself.
    let resp =
        crate::vram::send_local(hold, route, route.upstream.request_timeout(), build).await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    Err(for_protocol(crate::config::Protocol::Openai).map_error(status.as_u16(), &bytes))
}

/// A passthrough call that reached its upstream's response headers — the
/// audio family's and, since the image class, the `/v1/images/*` routes'.
pub(super) struct MediaOutcome {
    pub(super) resp: reqwest::Response,
    pub(super) route: Route,
    /// The gate's headers — the fallback alias, when the request was
    /// re-routed to one (gpu-hold design §4). `finish_media` stamps them.
    pub(super) headers: GateHeaders,
    pub(super) ttfb_ms: i64,
    /// The GPU claim, travelling with the response instead of dying with the
    /// function that opened it. audio.cpp streams for as long as the audio
    /// lasts — TTS and music generation legitimately take minutes — and it is
    /// holding the model for every second of that, so releasing at the headers
    /// would tell the scheduler an actively generating model is idle. Same rule
    /// as the chat path, whose guard rides into `stream_chat`'s relay task.
    pub(super) admission: Option<crate::vram::LocalHold>,
}

/// Response headers worth carrying back from the upstream. `content-length`
/// is safe to forward because nothing decompresses in between (the client is
/// built without gzip/brotli), and without it a 300 KB WAV (or a base64 PNG)
/// arrives chunked with no size for the client to show or seek against.
const MEDIA_PASSTHROUGH_HEADERS: [header::HeaderName; 3] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_DISPOSITION,
];

/// Shared tail for the audio handlers: stream the upstream body back and write
/// the log row when the transfer *ends*, so `total_ms` covers the whole
/// response and a mid-stream failure or client disconnect is recorded (same
/// shape as the legacy-completions passthrough). Errors get an OpenAI-shaped
/// body and a log row, mirroring the chat/embedding paths.
async fn finish_audio(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: String,
    started: Instant,
    result: Result<MediaOutcome, Failed>,
) -> Response {
    finish_media(state, ctx, alias, started, RequestClass::Audio, result).await
}

/// The body of both, parameterised by the class the row is logged under. One
/// implementation on purpose: "the guard lives until the last byte" is the
/// rule that keeps the GPU ledger honest on every long synchronous route, and
/// a second copy of it is a second place for it to rot.
pub(super) async fn finish_media(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: String,
    started: Instant,
    class: RequestClass,
    result: Result<MediaOutcome, Failed>,
) -> Response {
    const PROTO: ClientProto = ClientProto::OpenaiChat;
    let MediaOutcome {
        resp,
        route,
        headers,
        ttfb_ms,
        admission,
    } = match result {
        Ok(o) => o,
        Err((route, headers, e)) => {
            // Attributed even though it failed — see [`Failed`]. An audio
            // request the hold sent to a cloud TTS that then refused it is
            // still a request the fallback answered.
            let out = headers.stamp(error_response(PROTO, &e));
            record(
                LogParams {
                    state,
                    proto: PROTO,
                    ctx,
                    alias,
                    route: route.as_ref(),
                    started,
                    streamed: false,
                    class,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            return out;
        }
    };

    let status = resp.status();
    // Streaming-mode audio models answer with SSE; everything else (audio/wav,
    // a JSON transcript) is one buffered body.
    let streamed = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    let mut builder = Response::builder().status(status);
    for name in MEDIA_PASSTHROUGH_HEADERS {
        if let Some(value) = resp.headers().get(&name) {
            builder = builder.header(name, value.clone());
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let state2 = state.clone();
    let ctx2 = ctx.clone();
    let fallback = headers.fallback_reason();
    tokio::spawn(async move {
        let mut upstream = resp.bytes_stream();
        let mut error: Option<(String, String)> = None;
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => {
                    if tx.send(Ok(b)).await.is_err() {
                        error = Some(("canceled".into(), "client disconnected".into()));
                        break;
                    }
                }
                Err(e) => {
                    error = Some(("transport".into(), e.to_string()));
                    break;
                }
            }
        }
        record(
            LogParams {
                state: &state2,
                proto: PROTO,
                ctx: &ctx2,
                alias,
                route: Some(&route),
                started,
                streamed,
                class,
                timings: None,
                max_tokens_clamped: None,
                fallback,
                rung: None,
            },
            status.as_u16(),
            Some(ttfb_ms),
            Usage::default(),
            error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        )
        .await;
        // The model stops counting as in use when the last byte of audio has
        // been relayed, not when its headers arrived.
        drop(admission);
    });

    headers.stamp(
        builder
            .body(Body::from_stream(ReceiverStream::new(rx)))
            .unwrap_or_else(|e| error_response(PROTO, &GatewayError::Internal(e.to_string()))),
    )
}

/// Resolve the alias, rewrite `model` to the concrete upstream id, and send —
/// the shared JSON path behind both audio endpoints.
async fn audio_json_call(
    state: &SharedState,
    endpoint: &str,
    body: &Value,
    started: Instant,
) -> Result<MediaOutcome, Failed> {
    let alias = body.get("model").and_then(Value::as_str).ok_or((
        None,
        GateHeaders::default(),
        GatewayError::BadRequest("missing 'model'".into()),
    ))?;
    // The gate's per-request half: the hold swap, the protocol check
    // ([`crate::gate::RouteCheck::Audio`]), then admission — audio.cpp shares
    // the GPU with both llama routers and cannot unload a model on request, so
    // its share is arbitrated on the way in like theirs. The guard travels out
    // with the outcome (see [`MediaOutcome::admission`]), and a local route
    // comes back on the port its container answers on (§5).
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = crate::gate::open(state, alias, crate::gate::RouteCheck::Audio)
        .await
        .map_err(|f| (f.route, f.headers, f.error))?;
    let mut out = body.clone();
    out["model"] = Value::String(route.upstream_model.clone());
    let sent = audio_send(admission.as_ref(), &route, |r| {
        let url = format!("{}{endpoint}", r.upstream.base());
        Ok(apply_bearer_auth(
            state.http.post(url).json(&out),
            &r.upstream,
        ))
    })
    .await;
    match sent {
        Ok(resp) => Ok(MediaOutcome {
            ttfb_ms: started.elapsed().as_millis() as i64,
            resp,
            route,
            headers,
            admission,
        }),
        Err(e) => Err((Some(route), headers, e)),
    }
}

/// The requested alias for logging, before anything is resolved.
pub(super) fn body_alias(body: &Value) -> String {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

/// `POST /v1/audio/speech` — OpenAI TTS shape (JSON in, audio/JSON/SSE out).
pub async fn handle_audio_speech(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let alias = body_alias(&body);
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Audio,
    )
    .await
    {
        return r;
    }
    let result = audio_json_call(&state, "/audio/speech", &body, started).await;
    finish_audio(&state, &ctx, alias, started, result).await
}

/// `POST /v1/tasks/run` — audio.cpp's generic task route, the only way to
/// reach the tasks with no OpenAI-shaped equivalent (separation, VAD,
/// diarization, voice/singing conversion, speech-to-speech, music generation,
/// music transcription, voice description, speaker embedding, cloning).
///
/// The body is `{"model": <alias>, "request": {...}}`: `model` sits at the top
/// level exactly like the speech route, so alias rewriting is the shared path,
/// and the nested `request` object is relayed untouched — its fields are the
/// CLI's request-sequence format, which the gateway has no reason to model.
/// Answers are one JSON document (base64 audio, named tracks, segments,
/// speaker turns, word timings — whatever the task produced).
pub async fn handle_task_run(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let alias = body_alias(&body);
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Audio,
    )
    .await
    {
        return r;
    }
    let result = audio_json_call(&state, "/tasks/run", &body, started).await;
    finish_audio(&state, &ctx, alias, started, result).await
}

/// `POST /v1/tasks/stream` — the streaming-mode sibling of [`handle_task_run`].
/// Despite the name it is not an SSE route: audio.cpp buffers the run's stream
/// events and answers `{"events": [...], "result": {...}}` in one document, so
/// this is the same byte-level passthrough.
pub async fn handle_task_stream(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let alias = body_alias(&body);
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Audio,
    )
    .await
    {
        return r;
    }
    let result = audio_json_call(&state, "/tasks/stream", &body, started).await;
    finish_audio(&state, &ctx, alias, started, result).await
}

/// `GET /v1/audio/voices?model=<alias>` — the cached voice ids and configured
/// server presets of a TTS model, so a client can populate a voice picker
/// instead of guessing names like "alloy".
///
/// This is metadata about a model rather than an inference call, so — like
/// `GET /v1/models`, which also fans out to upstreams — it resolves and
/// forwards without opening a `request_logs` row.
pub async fn handle_audio_voices(state: SharedState, alias: &str) -> Response {
    const PROTO: ClientProto = ClientProto::OpenaiChat;
    // Resolved exactly like the inference routes it describes — the same
    // gate and the same protocol check, so "which model does `audio/x` mean"
    // has one answer on this endpoint and the speech/task ones — plus the
    // voices check ([`crate::gate::RouteCheck::AudioVoices`]). Metadata or
    // not, the answer comes out of the model's own container, and with
    // per-model containers there is no always-on port to read it from (§5).
    // So this takes admission like everything else: the container is started
    // if it is down, and the hold is held for the read.
    let opened = match crate::gate::open(&state, alias, crate::gate::RouteCheck::AudioVoices).await
    {
        Ok(o) => o,
        Err(f) => return f.headers.stamp(error_response(PROTO, &f.error)),
    };
    let resp = match read_voices(&state, opened.hold.as_ref(), &opened.route).await {
        Ok(resp) => resp,
        Err(e) => error_response(PROTO, &e),
    };
    opened.headers.stamp(resp)
}

/// [`handle_audio_voices`] for a reader that must not start anything — the
/// dashboard's Audio lab, where opening the page is not asking for the model.
/// `None` when the alias lands on a local model with no live container: none
/// in the registry as `ready`, or one that is `ready` there and does not
/// answer. The second case is real: a container that dies outside lmgw (an
/// OOM kill, audio.cpp crashing, a `podman stop` from a shell) stays `ready`
/// until something tries to start it, because nothing sweeps ready entries.
///
/// So this bypasses admission entirely rather than going through it with a
/// flag: an admitted read takes a claim (which stamps `last_used`, so a look
/// at the lab would reset the model's idle timer), and a dead container
/// behind the claim triggers the recovery that stops it and admits it afresh
/// — a `podman run`, possibly evicting other residents to fit it. Here it is
/// one GET to the entry's port, and a refused connection is the answer "not
/// running". A remote route has no container and is read as usual.
pub async fn audio_voices_if_running(state: SharedState, alias: &str) -> Option<Response> {
    const PROTO: ClientProto = ClientProto::OpenaiChat;
    // The gate's routing stages only — never admission, which is the point.
    let routed =
        match crate::gate::resolve(&state, alias, crate::gate::RouteCheck::AudioVoices).await {
            Ok(r) => r,
            Err(f) => return Some(f.headers.stamp(error_response(PROTO, &f.error))),
        };
    let mut route = routed.resolved().clone();
    let local = crate::vram::classify(&route);
    if let Some(target) = &local {
        // `starting` counts as not running: a read would park on it, and the
        // panel refetches once the runtime frame says it is up.
        let port = state.runtime().list().into_iter().find_map(|e| {
            (e.class == target.class
                && e.model_id == target.model_id
                && e.state == crate::runtime::registry::RuntimeState::Ready)
                .then_some(e.port)
        })?;
        route.upstream.base_url = format!("http://127.0.0.1:{port}/v1");
    }
    let resp = match read_voices(&state, None, &route).await {
        Ok(resp) => resp,
        Err(GatewayError::Transport(_)) if local.is_some() => return None,
        Err(e) => error_response(PROTO, &e),
    };
    Some(routed.headers().stamp(resp))
}

/// A voice list is an audio model's to give. A chat or image model resolves
/// here too (llama-server speaks the OpenAI protocol `resolve_audio` checks
/// for), and admitting it started its container for a question it answers
/// with a 404 — before admission, like [`refuse_media_route`] the other way.
pub(crate) fn refuse_non_audio_voices(route: &Route, alias: &str) -> Result<(), GatewayError> {
    use crate::config::UpstreamKind;
    let what = match route.upstream.kind {
        UpstreamKind::LlamaServer => "a llama-server (chat, embedding or rerank)",
        UpstreamKind::SdCpp => "an image",
        UpstreamKind::AudioCpp | UpstreamKind::Generic => return Ok(()),
    };
    Err(GatewayError::Unsupported(format!(
        "model '{alias}' is {what} model; /v1/audio/voices lists the voices of an audio model"
    )))
}

/// The one GET both voice readers send, relayed as the upstream's JSON.
async fn read_voices(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
) -> Result<Response, GatewayError> {
    let resp = audio_send(hold, route, |r| {
        // `RequestBuilder::query` is behind a reqwest feature this build does
        // not enable, so the concrete model id is encoded with the shared
        // helper.
        let url = format!(
            "{}/audio/voices?model={}",
            r.upstream.base(),
            crate::web::urlencode(&r.upstream_model)
        );
        Ok(apply_bearer_auth(state.http.get(url), &r.upstream))
    })
    .await?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Transport(e.to_string()))?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(bytes),
    )
        .into_response())
}

/// The three audio routes whose request is an *upload* rather than a JSON
/// document: one relay, three paths.
///
/// They differ in exactly two ways — the upstream path, and whether the JSON
/// variant exists at all — so they are one handler parameterised by this
/// rather than three copies of the multipart buffering, the admission guard
/// and the "held until the last byte" bookkeeping.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AudioUpload {
    /// `/v1/audio/transcriptions` — text and timing.
    Transcription,
    /// `/v1/audio/transcriptions/details` — the same request, with the word
    /// timings, segments and speaker turns the plain route drops on the way
    /// out. audio.cpp added it so a model that aligned every word or
    /// separated speakers does not have that work discarded.
    TranscriptionDetails,
    /// `/v1/audio/alignments` — forced alignment of a known transcript
    /// against uploaded audio (a `task: "align"` row). Multipart only, which
    /// is the point of it: the client's audio need not exist on the
    /// container's filesystem.
    Alignment,
}

impl AudioUpload {
    /// The upstream path, without the `/v1` the base URL already carries.
    fn path(self) -> &'static str {
        match self {
            Self::Transcription => "/audio/transcriptions",
            Self::TranscriptionDetails => "/audio/transcriptions/details",
            Self::Alignment => "/audio/alignments",
        }
    }

    /// Whether the route also takes the JSON variant (`{"model", "audio":
    /// path}`). audio.cpp answers a JSON alignment request with a 400, so
    /// lmgw says the same thing here instead of starting a container to be
    /// told it.
    fn accepts_json(self) -> bool {
        !matches!(self, Self::Alignment)
    }

    /// When upstream added this route, for the message below.
    fn added_upstream(self) -> Option<&'static str> {
        match self {
            Self::Transcription => None,
            Self::TranscriptionDetails => Some("2026-09-04"),
            Self::Alignment => Some("2026-09-02"),
        }
    }
}

/// A `404 unknown endpoint` from audiocpp_server means one thing here: the
/// container image is older than the route.
///
/// The container image is pinned by tag and an install can sit on one for
/// months, so this is the *likely* outcome of using a route lmgw grew
/// before the image did — and relayed verbatim it reads as a gateway bug
/// ("lmgw sent it somewhere that does not exist"). The upstream message stays
/// in the text; what is added is what to do about it.
fn explain_missing_route(which: AudioUpload, e: GatewayError) -> GatewayError {
    let Some(added) = which.added_upstream() else {
        return e;
    };
    match &e {
        GatewayError::Upstream {
            status: 404,
            message,
            ..
        } if message.contains("unknown endpoint") => GatewayError::Unsupported(format!(
            "this audio.cpp image has no handler for /v1{} — the route landed upstream on \
                 {added}, so the image predates it. Pull a newer one (the image under Settings → \
                 Runtimes → Audio) and restart the model's container. Upstream said: {message}",
            which.path()
        )),
        _ => e,
    }
}

/// `POST /v1/audio/transcriptions` and its two siblings — OpenAI Whisper
/// shape. Accepts both the JSON variant (`{"model", "audio": path}`) and
/// `multipart/form-data` uploads; multipart fields are re-encoded with
/// `model` rewritten.
pub async fn handle_audio_upload(
    state: SharedState,
    ctx: RequestCtx,
    req: axum::extract::Request,
    which: AudioUpload,
) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let is_multipart = is_multipart_content_type(req.headers());

    if !is_multipart && !which.accepts_json() {
        let err = GatewayError::BadRequest(
            "/v1/audio/alignments takes multipart/form-data only — the fields file, model and \
             text (language optional)"
                .into(),
        );
        return finish_audio(
            &state,
            &ctx,
            "?".into(),
            started,
            Err((None, GateHeaders::default(), err)),
        )
        .await;
    }

    if !is_multipart {
        let body = match axum::Json::<Value>::from_request(req, &state).await {
            Ok(axum::Json(v)) => v,
            Err(e) => {
                let err = GatewayError::BadRequest(e.to_string());
                return finish_audio(
                    &state,
                    &ctx,
                    "?".into(),
                    started,
                    Err((None, GateHeaders::default(), err)),
                )
                .await;
            }
        };
        let alias = body_alias(&body);
        if let Some(r) = policy_or_refuse(
            &state,
            ClientProto::OpenaiChat,
            &ctx,
            &alias,
            started,
            RequestClass::Audio,
        )
        .await
        {
            return r;
        }
        let result = audio_json_call(&state, which.path(), &body, started)
            .await
            .map_err(|(r, f, e)| (r, f, explain_missing_route(which, e)));
        return finish_audio(&state, &ctx, alias, started, result).await;
    }

    let (fields, alias) = match buffer_multipart(&state, req).await {
        Ok(v) => v,
        Err((alias, err)) => {
            return finish_audio(
                &state,
                &ctx,
                alias,
                started,
                Err((None, GateHeaders::default(), err)),
            )
            .await
        }
    };
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Audio,
    )
    .await
    {
        return r;
    }
    let result = multipart_call(&state, &alias, &fields, which, started).await;
    finish_audio(&state, &ctx, alias, started, result).await
}

/// The gate, the re-encode and the send of one buffered multipart upload —
/// what `/v1/audio/transcriptions` (and its two siblings) do once the body is
/// in memory, and what [`transcribe`](super::transcribe) does for an
/// in-process caller. The claim is held until the response has finished
/// streaming back, not just until the upload has been sent — see
/// [`MediaOutcome::admission`].
pub(super) async fn multipart_call(
    state: &SharedState,
    alias: &str,
    fields: &[MultipartField],
    which: AudioUpload,
    started: Instant,
) -> Result<MediaOutcome, Failed> {
    // The gate, as in `audio_json_call`.
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = crate::gate::open(state, alias, crate::gate::RouteCheck::Audio)
        .await
        .map_err(|f| (f.route, f.headers, f.error))?;
    let encode = |r: &Route| {
        let url = format!("{}{}", r.upstream.base(), which.path());
        Ok(apply_bearer_auth(
            state
                .http
                .post(url)
                .multipart(reencode_multipart(fields, r)),
            &r.upstream,
        ))
    };
    match audio_send(admission.as_ref(), &route, encode).await {
        Ok(resp) => Ok(MediaOutcome {
            ttfb_ms: started.elapsed().as_millis() as i64,
            resp,
            route,
            headers,
            admission,
        }),
        Err(e) => Err((Some(route), headers, explain_missing_route(which, e))),
    }
}
