//! The device listener's own routes, `/devices/*` (device link design §4.6):
//! on its face whatever their capability (`server::DEVICE_ROUTES`), a 404 on
//! the plain listener's, and bounded to [`BODY_LIMIT_BYTES`] instead of
//! `max_body_mb`.
//!
//! Enrolment (`POST /devices/enrol`, §3.4) is here; renewal (`POST
//! /devices/renew`, §5.4) comes with WP8. Each route sits beside its row in
//! `CAPABILITY_TABLE` (`Public` for enrolment, whose handler checks the
//! token), its `DocRoute`, and its entry in `server::DEVICE_ROUTES`. The
//! body limit is the route's own layer, inside the route's gate: the face
//! answers the plain listener 404 before a byte of the body counts, and a
//! handler reads its body with [`read_body`].

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use lmgw_api_types::device_link::{code, BODY_LIMIT_BYTES};

use crate::principal::{Cap, Refusal};
use crate::state::SharedState;

/// The `/devices/*` routes, merged into both listeners' routers: the gate
/// (`require(Public)`, whose face check answers the plain listener 404)
/// outside, the body limit inside it, then the handler.
pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route(
            // `ENROL_PATH`, as a literal: the route walk reads the paths from
            // the source (`tests/it/route_walk.rs`).
            "/devices/enrol",
            with_body_limit(post(super::enrol::enrol)),
        )
        .route_layer(crate::server::require(state, Cap::Public))
}

/// The `/devices/*` routes' body limit as a layer on one route: a declared
/// length past [`BODY_LIMIT_BYTES`] is refused before a byte of it is read,
/// and a body that grows past it while read fails [`read_body`] with the
/// same 413.
pub fn with_body_limit(
    route: axum::routing::MethodRouter<SharedState>,
) -> axum::routing::MethodRouter<SharedState> {
    route
        .layer::<_, std::convert::Infallible>(axum::middleware::from_fn(body_limit_mw))
        .layer::<_, std::convert::Infallible>(axum::extract::DefaultBodyLimit::disable())
}

/// 413 [`code::BODY_TOO_LARGE`], naming the limit.
pub fn too_large() -> Refusal {
    Refusal {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        code: code::BODY_TOO_LARGE,
        message: format!(
            "a /devices/* request body is at most {BODY_LIMIT_BYTES} bytes (16 KiB); an \
             enrolment is under 1 KiB"
        ),
    }
}

async fn body_limit_mw(req: Request, next: Next) -> Response {
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > BODY_LIMIT_BYTES as u64) {
        return too_large().into_response();
    }
    let (parts, body) = req.into_parts();
    let body = Body::new(http_body_util::Limited::new(body, BODY_LIMIT_BYTES));
    next.run(Request::from_parts(parts, body)).await
}

/// A `/devices/*` body, whole: the 413 when it is longer than
/// [`BODY_LIMIT_BYTES`], a 400 when the client went away mid-body.
pub async fn read_body(body: Body) -> Result<Bytes, Refusal> {
    axum::body::to_bytes(body, BODY_LIMIT_BYTES)
        .await
        .map_err(|e| {
            let limited = std::error::Error::source(&e)
                .is_some_and(|s| s.is::<http_body_util::LengthLimitError>())
                || e.to_string().contains("length limit");
            if limited {
                too_large()
            } else {
                Refusal {
                    status: StatusCode::BAD_REQUEST,
                    code: "bad_request",
                    message: format!("the request body could not be read: {e}"),
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use tower::ServiceExt;

    async fn echo_len(req: Request) -> Response {
        match read_body(req.into_body()).await {
            Ok(b) => b.len().to_string().into_response(),
            Err(r) => r.into_response(),
        }
    }

    fn app() -> Router {
        Router::new()
            .route("/devices/probe", post(echo_len))
            .layer(axum::middleware::from_fn(body_limit_mw))
            .layer(axum::extract::DefaultBodyLimit::disable())
    }

    async fn send(body: Body, declared: Option<usize>) -> (StatusCode, String) {
        let mut req = Request::post("/devices/probe");
        if let Some(n) = declared {
            req = req.header(header::CONTENT_LENGTH, n);
        }
        let resp = app().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_body_past_sixteen_kib_is_a_413_naming_the_limit() {
        let fits = vec![b'x'; BODY_LIMIT_BYTES];
        let (status, body) = send(Body::from(fits), Some(BODY_LIMIT_BYTES)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "16384"));
        // Declared too long: refused before it is read.
        let (status, body) = send(Body::empty(), Some(17 * 1024)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            body.contains(code::BODY_TOO_LARGE) && body.contains("16384"),
            "{body}"
        );
        // Chunked, no length declared: refused as it grows past it.
        let chunks = futures::stream::iter(
            (0..17).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 1024]))),
        );
        let (status, body) = send(Body::from_stream(chunks), None).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
        assert!(body.contains(code::BODY_TOO_LARGE), "{body}");
    }
}
