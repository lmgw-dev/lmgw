//! An Audio lab hand-off kept as its group's run (labs-store design §5). The
//! page makes the group first (`POST /audio-lab/api/groups`, [`super::groups`])
//! and names it on the hand-off (`?group=<id>`): the request is read whole,
//! an upload's file parts become the group's input artifacts, the route's own
//! handler answers, and its answer is **teed**: every chunk goes on to the
//! page the moment it arrives, a copy is kept, and when the answer ends (its
//! end, or its declared length's last byte) the run is recorded before the
//! page sees it, so a read of the group after the last byte finds it.
//!
//! Speech that succeeded is an output artifact (a WAV, an event stream of
//! PCM, chunked PCM), never text in the database, and it is spooled to its
//! file as it streams rather than held in memory. A task's answer is one
//! JSON document, held whole (the route relays audio.cpp's one buffered
//! body, and its audio is found by reading it as text): each base64 audio
//! string in it becomes an output artifact named `lab-artifact:<id>` in its
//! place ([`super::task_audio`]). Every other answer is its text, an error
//! envelope included. A page that stops the answer (an aborted fetch drops
//! the body) leaves the run as far as it got: `error_code` `stopped`, the
//! bytes so far its artifact. A request the page dropped before the route
//! answered at all leaves the group with no run, which the history shows as
//! stopped once it is no longer in flight.
//!
//! The run is recorded on a task of its own, which holds the cell's claim
//! until it is in: a page that goes away while the run is being written
//! does not take it with it.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use lmgw_api_types::audio_lab::AudioRoute;
use lmgw_api_types::lab_store::{
    artifact_ref, LabKind, LabMultipartField, LabMultipartFile, LabMultipartSummary,
};
use lmgw_api_types::OpenAiError;
use serde_json::Value;

use super::task_audio;
use crate::labs::artifacts::{self, Spool};
use crate::labs::claims::CellClaim;
use crate::labs::record::{self, NewArtifact, NewRun, RunTexts};
use crate::labs::run_facts::{answer_of, failure_of, gateway_headers};
use crate::labs::views;
use crate::state::SharedState;
use crate::store::{self, LabRunData};

/// The lab's own refusal of a kept hand-off, in the envelope the `/v1`
/// route answers in.
fn refused(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(OpenAiError::new(
            message.into(),
            "invalid_request_error",
            code,
        )),
    )
        .into_response()
}

/// The group's cell, claimed for this request: held until its run is
/// recorded, so a second post of the cell is the 409 without a dispatch.
pub(super) struct Claimed {
    group_id: i64,
    route: AudioRoute,
    model: String,
    claim: CellClaim,
}

/// Claim group `id`'s one cell for `route`: the group must be the Audio
/// lab's, made for this route, and not answered yet.
pub(super) async fn claim(
    state: &SharedState,
    id: i64,
    route: AudioRoute,
) -> Result<Claimed, Response> {
    let lab = LabKind::Audio;
    let g = match store::lab_group(&state.db, lab, id).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return Err(refused(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("the audio lab has no group {id}"),
            ))
        }
        Err(e) => {
            return Err(refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "lab_store",
                format!("reading the group failed: {e}"),
            ))
        }
    };
    let Some(cell) = views::plan_of(&g).into_iter().next() else {
        return Err(refused(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("group {id} has no cell"),
        ));
    };
    if cell.variant != route.as_str() {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            "wrong_route",
            format!(
                "group {id} was made for {}, not {}: make a group for this route",
                cell.variant,
                route.as_str()
            ),
        ));
    }
    let answered = || {
        refused(
            StatusCode::CONFLICT,
            "cell_answered",
            format!("group {id} has its run or is being answered; a re-ask is a new group"),
        )
    };
    let Some(claim) = state.lab_cells.claim(id, 0) else {
        return Err(answered());
    };
    match store::lab_run_exists(&state.db, id, 0, false).await {
        Ok(false) => {}
        Ok(true) => return Err(answered()),
        Err(e) => {
            return Err(refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "lab_store",
                format!("reading the group's run failed: {e}"),
            ))
        }
    }
    Ok(Claimed {
        group_id: id,
        route,
        model: cell.model,
        claim,
    })
}

impl Claimed {
    /// The request names the group's model, or it is refused before it is
    /// sent: a run is kept under the model its plan asks.
    pub(super) fn check_model(&self, named: Option<&str>) -> Result<(), Response> {
        let named = named.map(str::trim).unwrap_or_default();
        if named == self.model {
            return Ok(());
        }
        let said = if named.is_empty() {
            "names no model".to_string()
        } else {
            format!("names the model '{named}'")
        };
        Err(refused(
            StatusCode::BAD_REQUEST,
            "wrong_model",
            format!(
                "the request {said}, and group {} was made for '{}': make a group for this model",
                self.group_id, self.model
            ),
        ))
    }
}

/// The request as the run keeps it: its text, or an upload's summary, whose
/// files name the group's new inputs once they have ids.
pub(super) enum Sent {
    Text(String),
    Upload(LabMultipartSummary),
}

impl Sent {
    fn text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Upload(s) => serde_json::to_string(s).unwrap_or_else(|_| "{}".into()),
        }
    }

    /// The model the request names: the body's `model`, or the upload's
    /// `model` field.
    pub(super) fn model(&self) -> Option<String> {
        match self {
            Self::Text(t) => {
                #[derive(serde::Deserialize)]
                struct Named {
                    model: Option<String>,
                }
                serde_json::from_str::<Named>(t).ok()?.model
            }
            Self::Upload(s) => s
                .fields
                .iter()
                .find(|f| f.name == "model")
                .map(|f| f.value.clone()),
        }
    }
}

/// Is a part of an upload one of its text fields? It has no filename, no
/// type of its own that is not text, and its bytes are UTF-8. Any other is
/// a file, kept as an input artifact: a filename-less recording is not
/// text in the database.
fn is_text_part(file_name: Option<&str>, content_type: Option<&str>, bytes: &[u8]) -> bool {
    let texty = content_type.is_none_or(|ct| {
        let ct = ct
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        ct.starts_with("text/") || ct == "application/json" || ct.ends_with("+json")
    });
    file_name.is_none() && texty && std::str::from_utf8(bytes).is_ok()
}

/// A request read whole: its JSON body for the handler and its text for the
/// run, refused as the `/v1` route refuses a body it cannot read.
pub(super) async fn json_body(
    state: &SharedState,
    req: Request,
) -> Result<(Value, String), Response> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| refused(StatusCode::BAD_REQUEST, "bad_request", e.to_string()))?;
    let again = Request::from_parts(parts, Body::from(bytes.clone()));
    match Json::<Value>::from_request(again, state).await {
        Ok(Json(v)) => Ok((v, String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) => {
            let err = crate::error::GatewayError::BadRequest(e.to_string());
            Err((e.status(), Json(err.to_openai_json())).into_response())
        }
    }
}

/// An upload's text fields and file parts, read from its buffered body:
/// the summary the run keeps and each file as a new input artifact. A body
/// that does not read as multipart keeps what was read before the break;
/// the route answers it for itself. `parts` are the route's request's: its
/// extensions carry the route's "no body limit", without which axum's
/// multipart reader stops at 2 MB.
pub(super) async fn summarize(
    parts: &axum::http::request::Parts,
    bytes: Bytes,
) -> (Sent, Vec<NewArtifact>) {
    let mut req = Request::new(Body::from(bytes));
    *req.headers_mut() = parts.headers.clone();
    *req.extensions_mut() = parts.extensions.clone();
    let mut summary = LabMultipartSummary::default();
    let mut inputs = Vec::new();
    let Ok(mut mp) = Multipart::from_request(req, &()).await else {
        return (Sent::Upload(summary), inputs);
    };
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        let file_name = field.file_name().map(String::from);
        let ct = field.content_type().map(String::from);
        let Ok(b) = field.bytes().await else {
            break;
        };
        if is_text_part(file_name.as_deref(), ct.as_deref(), &b) {
            summary.fields.push(LabMultipartField {
                name,
                value: String::from_utf8_lossy(&b).into_owned(),
            });
            continue;
        }
        let filename = file_name.unwrap_or_else(|| name.clone());
        summary.files.push(LabMultipartFile {
            name,
            filename: filename.clone(),
            content_type: ct.clone(),
            bytes: b.len(),
            artifact: None,
        });
        inputs.push(NewArtifact::Bytes {
            name: filename,
            bytes: b,
            declared_mime: ct,
            data_url_head: None,
        });
    }
    (Sent::Upload(summary), inputs)
}

/// What a kept hand-off carries to its recording.
pub(super) struct Kept {
    pub state: SharedState,
    pub claimed: Claimed,
    pub sent: Sent,
    /// The upload's files, the group's new inputs.
    pub inputs: Vec<NewArtifact>,
    pub started: Instant,
}

/// What the route's answer said before its body: what the run records
/// besides the bytes.
struct Facts {
    status: StatusCode,
    headers: std::collections::BTreeMap<String, String>,
    response_type: Option<String>,
    /// Speech that succeeded: the bytes are an output artifact.
    artifact: bool,
}

/// How the answer ended.
enum Ended {
    Done,
    /// The page went away: the body was dropped before its end.
    Stopped,
    /// The answer broke off.
    Failed(String),
}

/// Where the answer's bytes go on their way to the run.
enum Sink {
    /// Held until the run is recorded: a transcription, a task's one
    /// document, an error envelope.
    Memory(Vec<u8>),
    /// Speech that succeeded, onto its file as it arrives. `spool` is made
    /// at the first byte; `error` says why spooling stopped (the answer
    /// still goes on to the page, and its run is not kept).
    Spool {
        spool: Option<Spool>,
        error: Option<String>,
    },
}

impl Sink {
    async fn take(&mut self, state: &SharedState, chunk: &[u8]) {
        match self {
            Self::Memory(buf) => buf.extend_from_slice(chunk),
            Self::Spool { spool, error } => {
                if error.is_some() {
                    return;
                }
                if spool.is_none() {
                    match Spool::create(&state.data_dir).await {
                        Ok(s) => *spool = Some(s),
                        Err(e) => {
                            *error = Some(e);
                            return;
                        }
                    }
                }
                if let Some(s) = spool.as_mut() {
                    if let Err(e) = s.write(chunk).await {
                        *spool = None;
                        *error = Some(e);
                    }
                }
            }
        }
    }
}

/// The answer's bytes as they reached the run.
enum Got {
    Memory(Vec<u8>),
    Spooled(artifacts::Spooled),
    /// Spooling failed: the run is not kept.
    Lost(String),
}

/// The teed body's state: what to record, and where the bytes went.
struct Recorder {
    pending: Option<(Kept, Facts)>,
    sink: Sink,
    received: usize,
    /// The answer's `Content-Length`: a body that declares one is done at
    /// its last byte, and the server may drop it there without asking for
    /// its end, so the run is recorded before that byte goes on.
    length: Option<usize>,
}

impl Recorder {
    /// The pending record with its bytes, once: what [`keep`] takes.
    fn take(&mut self) -> Option<(Kept, Facts, Sink, usize)> {
        let (kept, facts) = self.pending.take()?;
        let sink = std::mem::replace(&mut self.sink, Sink::Memory(Vec::new()));
        Some((kept, facts, sink, self.received))
    }

    /// Record the run on a task of its own and wait for it: a page that
    /// goes away meanwhile drops this future, not the record.
    async fn finish(&mut self, ended: Ended) {
        if let Some((kept, facts, sink, received)) = self.take() {
            let _ = tokio::spawn(keep(kept, facts, sink, received, ended)).await;
        }
    }
}

impl Drop for Recorder {
    /// The body was dropped before its end: the page stopped it. The run is
    /// recorded as far as it got, on a task of its own.
    fn drop(&mut self) {
        if let Some((kept, facts, sink, received)) = self.take() {
            tokio::spawn(keep(kept, facts, sink, received, Ended::Stopped));
        }
    }
}

/// The route's answer `resp` with its body teed into the run of `kept`.
pub(super) fn tee(kept: Kept, resp: Response) -> Response {
    let status = resp.status();
    let (parts, body) = resp.into_parts();
    let facts = Facts {
        status,
        headers: gateway_headers(&parts.headers),
        response_type: parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        artifact: kept.claimed.route == AudioRoute::Speech && status.is_success(),
    };
    let sink = if facts.artifact {
        Sink::Spool {
            spool: None,
            error: None,
        }
    } else {
        Sink::Memory(Vec::new())
    };
    let state = kept.state.clone();
    let rec = Recorder {
        pending: Some((kept, facts)),
        sink,
        received: 0,
        length: parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok()),
    };
    let stream = futures::stream::unfold(
        (body.into_data_stream(), rec, state),
        |(mut body, mut rec, state)| async move {
            match body.next().await {
                Some(Ok(chunk)) => {
                    if rec.pending.is_some() {
                        rec.sink.take(&state, &chunk).await;
                        rec.received += chunk.len();
                        if rec.length.is_some_and(|n| rec.received >= n) {
                            // The last byte: recorded before it goes on.
                            rec.finish(Ended::Done).await;
                        }
                    }
                    Some((Ok(chunk), (body, rec, state)))
                }
                Some(Err(e)) => {
                    rec.finish(Ended::Failed(format!("the answer broke off: {e}")))
                        .await;
                    Some((Err(e), (body, rec, state)))
                }
                None => {
                    // Recorded before the page sees the end.
                    rec.finish(Ended::Done).await;
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}

/// A file name for speech of `response_type`: `<model>.<ext>`.
fn speech_name(model: &str, response_type: Option<&str>) -> String {
    let ty = response_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    let ext = match ty {
        "text/event-stream" => "sse",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" => "aac",
        "application/json" => "json",
        "audio/pcm" | "audio/l16" | "application/octet-stream" => "pcm",
        _ => "bin",
    };
    format!("{}.{ext}", file_stem(model, "speech"))
}

/// `s` as a file name's stem: anything but ASCII letters, digits, `.`, `_`
/// and `-` a dash; `fallback` for nothing.
fn file_stem(s: &str, fallback: &str) -> String {
    let stem: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if stem.is_empty() {
        fallback.into()
    } else {
        stem
    }
}

/// The sink's bytes as they reached the run: a spool finished on the disk.
async fn got(sink: Sink) -> Got {
    match sink {
        Sink::Memory(buf) => Got::Memory(buf),
        Sink::Spool { error: Some(e), .. } => Got::Lost(e),
        Sink::Spool { spool: None, .. } => Got::Memory(Vec::new()),
        Sink::Spool { spool: Some(s), .. } => match s.finish().await {
            Ok(f) => Got::Spooled(f),
            Err(e) => Got::Lost(e),
        },
    }
}

/// A task answer's audio file name: `<model>[-<id>].<ext>`, the extension
/// the bytes' own type names.
fn clip_name(model: &str, clip: &task_audio::Clip, k: usize) -> String {
    let head = &clip.bytes[..clip.bytes.len().min(4096)];
    let ext = crate::extract::sniff::sniff_artifact(head)
        .map(|s| artifacts::ext_of(s.mime))
        .unwrap_or("bin");
    let stem = file_stem(model, "audio");
    match &clip.id {
        Some(id) => format!("{stem}-{}.{ext}", file_stem(id, "output")),
        None if k == 0 => format!("{stem}.{ext}"),
        None => format!("{stem}-{k}.{ext}"),
    }
}

/// Record the run: the answer's bytes as an artifact or as text (a task's
/// audio as artifacts in its text), the upload's files as the group's
/// inputs, the summary naming them. Releases the cell's claim once done.
async fn keep(kept: Kept, facts: Facts, sink: Sink, received: usize, ended: Ended) {
    let Kept {
        state,
        claimed,
        sent,
        inputs,
        started,
    } = kept;
    let latency_ms = started.elapsed().as_millis() as i64;
    let mut outputs = Vec::new();
    let mut clips = Vec::new();
    let response = match got(sink).await {
        Got::Lost(e) => {
            tracing::warn!("audio lab: group {} not kept: {e}", claimed.group_id);
            return;
        }
        Got::Spooled(file) => {
            outputs.push(NewArtifact::Spooled {
                name: speech_name(&claimed.model, facts.response_type.as_deref()),
                file,
                declared_mime: facts.response_type.clone(),
            });
            None
        }
        Got::Memory(buf) if facts.artifact => {
            // Speech that came with no byte: nothing to keep as its file.
            debug_assert!(buf.is_empty());
            None
        }
        Got::Memory(buf) => match String::from_utf8(buf) {
            Ok(t) => {
                let task = matches!(claimed.route, AudioRoute::TaskRun | AudioRoute::TaskStream);
                if task && facts.status.is_success() {
                    clips = task_audio::clips(&t);
                    for (k, c) in clips.iter_mut().enumerate() {
                        outputs.push(NewArtifact::Bytes {
                            name: clip_name(&claimed.model, c, k),
                            bytes: std::mem::take(&mut c.bytes),
                            declared_mime: None,
                            data_url_head: None,
                        });
                    }
                }
                Some(t)
            }
            Err(e) => {
                outputs.push(NewArtifact::Bytes {
                    name: "response".into(),
                    bytes: e.into_bytes().into(),
                    declared_mime: facts.response_type.clone(),
                    data_url_head: None,
                });
                None
            }
        },
    };
    let (mut error_code, mut error_message) = if facts.status.is_success() {
        (None, None)
    } else {
        failure_of(response.as_deref())
    };
    match ended {
        Ended::Done => {}
        Ended::Stopped => {
            error_code = Some("stopped".into());
            error_message = Some(format!(
                "the page stopped the answer after {received} bytes"
            ));
        }
        Ended::Failed(e) => {
            error_code.get_or_insert_with(|| "read_failed".into());
            error_message = Some(e);
        }
    }
    let (upstream_model, input_tokens, output_tokens) = if facts.status.is_success() {
        answer_of(response.as_deref())
    } else {
        (None, None, None)
    };
    let request = sent.text();
    let data = LabRunData {
        group_id: claimed.group_id,
        cell: 0,
        variant: claimed.route.as_str().into(),
        endpoint: claimed.route.endpoint().into(),
        model_asked: claimed.model.clone(),
        answered_by: facts
            .headers
            .get("x-lmgw-fallback")
            .cloned()
            .unwrap_or_else(|| claimed.model.clone()),
        fallback_reason: facts.headers.get("x-lmgw-fallback-reason").cloned(),
        upstream_model,
        http_status: i64::from(facts.status.as_u16()),
        error_code,
        error_message,
        request,
        // A task's audio is written once it has ids, below.
        response: if clips.is_empty() {
            response.clone()
        } else {
            None
        },
        response_type: facts.response_type,
        headers: serde_json::to_string(&facts.headers).unwrap_or_else(|_| "{}".into()),
        input_tokens,
        output_tokens,
        latency_ms,
        readable: None,
    };
    let stored = data.request.clone();
    let recorded = record::run_with(
        &state.db,
        &state.data_dir,
        NewRun {
            data,
            outputs,
            replace_failed: false,
        },
        inputs,
        move |in_ids, out_ids| {
            let request = match sent {
                Sent::Text(_) => None,
                Sent::Upload(mut s) => {
                    for (f, id) in s.files.iter_mut().zip(in_ids) {
                        f.artifact = Some(artifact_ref(*id));
                    }
                    Some(serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()))
                }
            };
            let spliced = (!clips.is_empty()).then(|| {
                let refs: Vec<String> = out_ids.iter().map(|id| artifact_ref(*id)).collect();
                task_audio::splice(response.as_deref().unwrap_or_default(), &clips, &refs)
            });
            if request.is_none() && spliced.is_none() {
                return None;
            }
            Some(RunTexts {
                request: request.unwrap_or(stored),
                response: spliced.or(response),
            })
        },
    )
    .await;
    if let Err(e) = recorded {
        tracing::warn!("audio lab: group {} not kept: {e}", claimed.group_id);
    }
    // Released once the run is recorded (or could not be).
    drop(claimed.claim);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_is_named_after_its_model_and_type() {
        assert_eq!(speech_name("audio/tts", Some("audio/wav")), "audio-tts.wav");
        assert_eq!(
            speech_name("audio/live", Some("text/event-stream; charset=utf-8")),
            "audio-live.sse"
        );
        assert_eq!(speech_name("", None), "speech.bin");
    }

    #[test]
    fn a_tasks_audio_is_named_after_its_model_its_output_and_its_type() {
        let clip = |bytes: &'static [u8], id: Option<&str>| task_audio::Clip {
            start: 0,
            end: 0,
            bytes: Bytes::from_static(bytes),
            id: id.map(String::from),
        };
        let wav = b"RIFF\x24\0\0\0WAVEfmt ";
        assert_eq!(clip_name("audio/sep", &clip(wav, None), 0), "audio-sep.wav");
        assert_eq!(
            clip_name("audio/sep", &clip(wav, Some("vocals")), 1),
            "audio-sep-vocals.wav"
        );
        assert_eq!(
            clip_name("audio/sep", &clip(&[0, 1, 2], None), 2),
            "audio-sep-2.bin"
        );
    }

    #[test]
    fn only_a_nameless_utf8_text_part_is_a_field() {
        assert!(is_text_part(None, None, b"audio/asr"));
        assert!(is_text_part(None, Some("text/plain; charset=utf-8"), b"x"));
        assert!(is_text_part(None, Some("application/json"), b"{}"));
        assert!(!is_text_part(Some("a.wav"), None, b"x"), "a file name");
        assert!(
            !is_text_part(None, Some("audio/wav"), b"x"),
            "a type of its own"
        );
        assert!(!is_text_part(None, None, &[0xff, 0xfe, 0]), "no UTF-8");
    }
}
