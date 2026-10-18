//! The head of an upstream answer that a relaying route passes on.
//!
//! `/v1/systemone` and `/tokenize` hand the backend's answer to the client as
//! it came: its status and body, its `Content-Type`, and its `Retry-After`.
//! The last one is what tells a client how long to wait after the 429 or 503
//! of a rate-limited or loading upstream (TypeSafe, OpenRouter); no other
//! header is relayed.

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// The relayed headers of one upstream answer, taken before its body is read.
pub(super) struct RelayedHead {
    content_type: Option<HeaderValue>,
    retry_after: Option<HeaderValue>,
}

impl RelayedHead {
    pub(super) fn of(headers: &HeaderMap) -> Self {
        Self {
            content_type: headers.get(header::CONTENT_TYPE).cloned(),
            retry_after: headers.get(header::RETRY_AFTER).cloned(),
        }
    }

    /// The client's response: `status`, the kept headers and the body as read.
    pub(super) fn response(self, status: StatusCode, body: Bytes) -> Response {
        let mut out = Response::builder().status(status.as_u16());
        if let Some(ct) = self.content_type {
            out = out.header(header::CONTENT_TYPE, ct);
        }
        if let Some(ra) = self.retry_after {
            out = out.header(header::RETRY_AFTER, ra);
        }
        out.body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
    }
}
