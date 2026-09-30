//! The op dispatcher's body extractor (review R2 #2): `POST /api/op/{name}`
//! with no body at all is the same as `{}`.
//!
//! The ops that take no arguments (`builds`, `build_env`, `update_check`,
//! `agents_restore`, …) are documented with no `requestBody` (api-docs design
//! §4.7 `OpArgs::NoArgs`), so the API reference's tester — and a curl copied
//! from it — sends none, and neither sends a `Content-Type`. `Json<_>` answers
//! that with a `415` before the op ever runs. An empty body is taken as the
//! empty object here instead, whatever its `Content-Type`; anything else goes
//! through `Json<_>` exactly as before, so a malformed body, a wrong
//! `Content-Type` on a non-empty one, or a non-object still gets the same
//! rejection it always did.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Map, Value};

/// The op's arguments: the JSON object in the body, or an empty one when
/// there is no body.
pub(super) struct OpBody(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for OpBody {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let bytes = Bytes::from_request(Request::from_parts(parts.clone(), body), state)
            .await
            .map_err(IntoResponse::into_response)?;
        if bytes.is_empty() {
            return Ok(OpBody(Map::new()));
        }
        let req = Request::from_parts(parts, Body::from(bytes));
        Json::<Map<String, Value>>::from_request(req, state)
            .await
            .map(|Json(args)| OpBody(args))
            .map_err(IntoResponse::into_response)
    }
}
