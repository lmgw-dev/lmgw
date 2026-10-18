//! The four hand-offs with `?group=<id>` (labs-store design §5): each
//! claims its group's cell, reads the request whole, checks that it names
//! the group's model, hands it to the route's handler as it came and tees
//! the answer into the run ([`super::record`]).

use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use lmgw_api_types::audio_lab::AudioRoute;

use super::record::{self, Kept, Sent};
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;

/// `POST /audio-lab/api/speech?group=<id>`.
pub(super) async fn speech(state: SharedState, group: i64, req: Request) -> Response {
    let claimed = match record::claim(&state, group, AudioRoute::Speech).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (body, text) = match record::json_body(&state, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let sent = Sent::Text(text);
    if let Err(r) = claimed.check_model(sent.model().as_deref()) {
        return r;
    }
    let started = Instant::now();
    let resp = proxy::handle_audio_speech(state.clone(), RequestCtx::default(), body).await;
    record::tee(
        Kept {
            state,
            claimed,
            sent,
            inputs: Vec::new(),
            started,
        },
        resp,
    )
}

/// `POST /audio-lab/api/tasks/run?group=<id>` (`stream=1`: `/v1/tasks/stream`).
pub(super) async fn task(state: SharedState, group: i64, stream: bool, req: Request) -> Response {
    let route = if stream {
        AudioRoute::TaskStream
    } else {
        AudioRoute::TaskRun
    };
    let claimed = match record::claim(&state, group, route).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (body, text) = match record::json_body(&state, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let sent = Sent::Text(text);
    if let Err(r) = claimed.check_model(sent.model().as_deref()) {
        return r;
    }
    let started = Instant::now();
    let ctx = RequestCtx::default();
    let resp = if stream {
        proxy::handle_task_stream(state.clone(), ctx, body).await
    } else {
        proxy::handle_task_run(state.clone(), ctx, body).await
    };
    record::tee(
        Kept {
            state,
            claimed,
            sent,
            inputs: Vec::new(),
            started,
        },
        resp,
    )
}

/// `POST /audio-lab/api/transcriptions?group=<id>` and `/alignments`: a
/// multipart upload (its file parts the group's inputs) or the JSON naming
/// a clip on the model server.
pub(super) async fn upload(
    state: SharedState,
    group: i64,
    route: AudioRoute,
    which: proxy::AudioUpload,
    req: Request,
) -> Response {
    let claimed = match record::claim(&state, group, route).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            let err = crate::error::GatewayError::BadRequest(format!("reading the upload: {e}"));
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(err.to_openai_json()),
            )
                .into_response();
        }
    };
    let multipart = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        });
    let (sent, inputs) = if multipart {
        record::summarize(&parts, bytes.clone()).await
    } else {
        (
            Sent::Text(String::from_utf8_lossy(&bytes).into_owned()),
            Vec::new(),
        )
    };
    if let Err(r) = claimed.check_model(sent.model().as_deref()) {
        return r;
    }
    let again = Request::from_parts(parts, Body::from(bytes));
    let started = Instant::now();
    let resp = proxy::handle_audio_upload(state.clone(), RequestCtx::default(), again, which).await;
    record::tee(
        Kept {
            state,
            claimed,
            sent,
            inputs,
            started,
        },
        resp,
    )
}
