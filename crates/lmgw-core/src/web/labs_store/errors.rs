//! The store routes' own failures, in the dashboard's flat `ApiError`: a
//! `{lab}` that names no lab, and a store that failed.

use axum::http::StatusCode;
use axum::response::Response;
use lmgw_api_types::lab_store::LabKind;

use super::super::chat::err_json;

/// The lab `{lab}` names, or the 404 for a name that is none.
pub(in crate::web) fn lab_of(name: &str) -> Result<LabKind, Response> {
    LabKind::parse(name).ok_or_else(|| {
        err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("there is no lab '{name}': decision, image or audio"),
        )
    })
}

/// A store failure: a 500 `internal`, logged.
pub(in crate::web) fn store_failed(what: &str, e: impl std::fmt::Display) -> Response {
    tracing::warn!("lab store: {what} failed: {e}");
    err_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        format!("{what} failed: {e}"),
    )
}
