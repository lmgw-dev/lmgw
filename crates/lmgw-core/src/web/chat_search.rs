//! `GET /chat/api/search` (chat-complete design §4): full-text search over
//! thread titles, message text and sent attachment names. Temporary threads
//! live in memory only and are never indexed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use super::chat::err_json;
use super::chat_extract::ChatQuery;
use crate::state::SharedState;
use crate::store::{self, SearchArchived, SEARCH_MIN_CHARS};

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    /// `0` (default) active threads, `1` archived only, `all` both.
    #[serde(default)]
    archived: String,
    #[serde(default)]
    folder: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

pub async fn search(
    State(state): State<SharedState>,
    ChatQuery(q): ChatQuery<SearchQuery>,
) -> Response {
    let archived = match q.archived.as_str() {
        "" | "0" => SearchArchived::Active,
        "1" => SearchArchived::Archived,
        "all" => SearchArchived::All,
        other => {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("archived must be 0, 1 or all, not {other:?}"),
            )
        }
    };
    if q.q.trim().chars().count() < SEARCH_MIN_CHARS {
        return err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("a search needs at least {SEARCH_MIN_CHARS} characters"),
        );
    }
    match store::search_chat(&state.db, &q.q, archived, q.folder, q.offset.unwrap_or(0)).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
