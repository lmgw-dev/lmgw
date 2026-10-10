//! `GET /chat/api/search` (chat-complete design §4): full-text search over
//! thread titles, message text and sent attachment names. Temporary threads
//! live in memory only and are never indexed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use super::chat::err_json;
use super::chat_caller::Caller;
use super::chat_extract::ChatQuery;
use crate::state::SharedState;
use crate::store::{self, SearchArchived, SEARCH_MIN_CHARS};

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub struct SearchQuery {
    /// The words to find: at least two characters. Words match as prefixes
    /// and all must match; a `"quoted phrase"` matches as a phrase.
    #[serde(default)]
    q: String,
    /// `0` (default) active threads, `1` archived only, `all` both.
    #[serde(default)]
    archived: String,
    /// Only threads in this folder.
    #[serde(default)]
    folder: Option<i64>,
    /// How many threads to skip: the previous page's `next_offset`.
    #[serde(default)]
    offset: Option<i64>,
}

/// A device's search leaves Admin Chat threads out (client-apps design L3).
pub async fn search(
    State(state): State<SharedState>,
    caller: Caller,
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
    let offset = q.offset.unwrap_or(0);
    let admin = caller.reach(&state.snapshot());
    // A folder a device cannot see is one that is not there (review W4-7):
    // nothing in it is found, and no thread is said to be in it.
    let hidden = if caller.is_device() {
        match store::self_admin_folder_ids(&state.db, admin).await {
            Ok(h) => h,
            Err(e) => {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
            }
        }
    } else {
        Default::default()
    };
    let folder = match q.folder {
        Some(f) if hidden.contains(&f) => Some(-1),
        other => other,
    };
    match store::search_chat(&state.db, &q.q, archived, folder, offset, admin).await {
        Ok(mut page) => {
            for t in &mut page.threads {
                if t.folder_id.is_some_and(|f| hidden.contains(&f)) {
                    t.folder_id = None;
                }
            }
            Json(page).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
