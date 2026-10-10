//! `GET /chat/api/threads/rows?ids=3,7`: some of the thread list's rows,
//! for the owner (review CL-11, 2026-10-08).
//!
//! The Chat page follows other writers through `/api/events`' `chat` frame
//! (client-apps design §3.6). A frame that names only messages moved those
//! threads' `updated_at` and `last_message_at` and nothing else the list
//! shows, so the page reads only the named rows here and sorts its list
//! again itself, rather than the whole list with every thread's settings
//! once per turn of a device's voice session.
//!
//! - **The list's rows**: each row is what `GET /chat/api/threads` lists for
//!   it, byte for byte, active or archived, in the active list's order.
//! - **Stored threads only**: a temporary id, one that is not there, or one
//!   out of the caller's reach is left out. A reader takes a missing id for
//!   "read the whole list".
//! - **No cap of lmgw's on `ids`**: one query binds them as one JSON array.
//!   The HTTP layer answers a URI over 64 KiB (roughly 9,000 ids) with
//!   414, and the page then reads the whole list instead, as after any
//!   failed rows read (`chat_sync/rows.rs` in lmgw-ui): slower, nothing
//!   lost.
//! - **The owner's** (`Cap::Admin`): a device's own feed carries whole rows
//!   already. In the admin API document only (`openapi/planes/chat_threads.rs`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat::ThreadRows;
use serde::Deserialize;

use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatQuery;
use super::super::chat_folders::retention::PurgeDays;
use super::super::chat_repo::ChatRepo;
use super::super::chat_wire;
use super::{err_json, read_failed};
use crate::state::SharedState;

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub struct RowsQuery {
    /// The thread ids, comma-separated (`3,7`). A temporary id, one that is
    /// not there and one out of the caller's reach are left out of the
    /// answer; a part that is not a number is a 400.
    #[serde(default)]
    ids: String,
}

/// `ids` as thread ids; the first part that is not one, as the refusal.
fn parse_ids(ids: &str) -> Result<Vec<i64>, String> {
    ids.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i64>().map_err(|_| s.to_string()))
        .collect()
}

/// The handler (module doc).
pub async fn thread_rows(
    State(state): State<SharedState>,
    caller: Caller,
    ChatQuery(q): ChatQuery<RowsQuery>,
) -> Response {
    let ids = match parse_ids(&q.ids) {
        Ok(ids) => ids,
        Err(bad) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("ids: '{bad}' is not a thread id — give them comma-separated, as 3,7"),
            )
        }
    };
    let stored: Vec<i64> = ids.into_iter().filter(|id| *id > 0).collect();
    let threads = match ChatRepo::stored_rows(&state, &caller, &stored).await {
        Ok(t) => t,
        Err(e) => return read_failed("the threads", e),
    };
    let purge = PurgeDays::load(&state).await;
    let all: Vec<&_> = threads.iter().collect();
    let last = chat_wire::last_messages(&state, &all).await;
    let rows = threads
        .iter()
        .map(|t| chat_wire::thread_row(t, &purge, last.get(&t.id).copied()))
        .collect();
    Json(chat_wire::wire(&ThreadRows { threads: rows })).into_response()
}

#[cfg(test)]
mod tests {
    use super::parse_ids;

    #[test]
    fn ids_are_comma_separated_and_a_part_that_is_none_is_named() {
        assert_eq!(parse_ids("3,7"), Ok(vec![3, 7]));
        assert_eq!(parse_ids(" 3 , 7 ,"), Ok(vec![3, 7]));
        assert_eq!(parse_ids(""), Ok(vec![]));
        assert_eq!(parse_ids("-2,5"), Ok(vec![-2, 5]));
        assert_eq!(parse_ids("3,x7"), Err("x7".to_string()));
    }
}
