//! The transfers API's answers and refusals (device transfers design §9.4):
//! every body an `lmgw-api-types` type, a refusal a [`TransferRefusal`] with
//! the extra field its code names; and the JSON routes' bounded body.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::transfer::{code, TransferRefusal, BODY_LIMIT_BYTES};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// `body` with `status`, as JSON.
pub(super) fn answer<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, Json(body)).into_response()
}

/// 400 `bad_request`, saying why.
pub(super) fn bad_request(message: impl Into<String>) -> Response {
    answer(
        StatusCode::BAD_REQUEST,
        &TransferRefusal::new(code::BAD_REQUEST, message),
    )
}

/// 500 `internal`: a store or file failure, said as it is.
pub(super) fn internal(message: impl Into<String>) -> Response {
    answer(
        StatusCode::INTERNAL_SERVER_ERROR,
        &TransferRefusal::new("internal", message),
    )
}

/// 507 `transfer_storage_full` with the space left for new offers.
pub(super) fn storage_full(free_bytes: u64, message: String) -> Response {
    answer(
        StatusCode::INSUFFICIENT_STORAGE,
        &TransferRefusal {
            free_bytes: Some(free_bytes),
            ..TransferRefusal::new(code::TRANSFER_STORAGE_FULL, message)
        },
    )
}

/// 409 `transfer_offset` with the resume point.
pub(super) fn offset(received: u64, message: String) -> Response {
    answer(
        StatusCode::CONFLICT,
        &TransferRefusal {
            received: Some(received),
            ..TransferRefusal::new(code::TRANSFER_OFFSET, message)
        },
    )
}

/// A JSON route's body as `T`, read up to [`BODY_LIMIT_BYTES`]: 413
/// `body_too_large` past it, 400 `bad_request` for one that is not `T`.
pub(super) async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(body, BODY_LIMIT_BYTES)
        .await
        .map_err(|_| {
            answer(
                StatusCode::PAYLOAD_TOO_LARGE,
                &TransferRefusal::new(
                    code::BODY_TOO_LARGE,
                    format!(
                        "the body is over {BODY_LIMIT_BYTES} bytes (16 KiB), the most a \
                         /transfers/api JSON body takes; a file's bytes go to …/content"
                    ),
                ),
            )
        })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| bad_request(format!("the body is not what this route reads: {e}")))
}
