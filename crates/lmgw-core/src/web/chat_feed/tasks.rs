//! The `task.*` records as a reader receives them (MCP Tasks design §4.1):
//! rendered from the facts the record keeps, so a record reads the same
//! after its task row is gone (delivered, or its thread deleted). A thread
//! that still exists is checked as it is now: one a later write took out
//! of a device's reach is not shown (that write's own record says it went).

use lmgw_api_types::chat_feed::{TaskDone, TaskStarted};
use serde_json::{json, Value};

use crate::state::AppState;
use crate::store::feed::{kind, Record};
use crate::store::{self, AdminThreads};

/// The data of `r` (a `task.*` record) for a reader that reaches as far as
/// `admin`; `None` when there is nothing to render, or the reader does not
/// see the thread now.
pub(super) async fn render(
    state: &AppState,
    r: &Record,
    admin: AdminThreads,
) -> Result<Option<Value>, crate::error::GatewayError> {
    let (Some(thread_id), Some(t)) = (r.thread_id, r.task()) else {
        return Ok(None);
    };
    if store::get_chat_thread(&state.db, thread_id)
        .await?
        .is_some_and(|thread| !admin.sees(thread.reach_level()))
    {
        return Ok(None);
    }
    Ok(match r.kind.as_str() {
        kind::TASK_STARTED => Some(json!(TaskStarted {
            thread_id,
            id: t.id,
            task_id: t.task_id,
            server_label: t.server_label,
            tool: t.tool,
            by: r.by.clone(),
        })),
        kind::TASK_DONE => t.message_id.map(|message_id| {
            json!(TaskDone {
                thread_id,
                message_id,
                id: t.id,
                task_id: t.task_id,
                server_label: t.server_label,
                tool: t.tool,
                status: t.status.unwrap_or_default(),
                by: r.by.clone(),
            })
        }),
        _ => None,
    })
}
