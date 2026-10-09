//! The `approval.*` records as a reader receives them (client-apps design
//! §6.5): rendered from the reply's pending state at delivery — the call's
//! label, name, arguments and id (`call_id`, as the turn's frames carry
//! it), and for a decision its verdict (an approved
//! call whose turn never started reads `approve: false, not_run: true`) —
//! with the record's author as `by`. A reply gone by then (deleted, regenerated, its
//! thread cut back or deleted) renders nothing: there is no call left to
//! decide or to show.

use lmgw_api_types::chat_feed::{ApprovalDecided, ApprovalRequested};
use serde_json::{json, Value};

use crate::state::AppState;
use crate::store::feed::{kind, Record};
use crate::store::{self, AdminThreads};

/// `(message_id, approval_request_id)` of an `approval.*` record.
fn ids(r: &Record) -> Option<(i64, String)> {
    let detail: Value = serde_json::from_str(r.detail.as_deref()?).ok()?;
    Some((
        detail.get("message_id")?.as_i64()?,
        detail.get("approval_request_id")?.as_str()?.to_string(),
    ))
}

/// The data of `r` (an `approval.*` record) for a reader that reaches as
/// far as `admin`; `None` when there is nothing to render, or the reader
/// does not see the thread now.
pub(super) async fn render(
    state: &AppState,
    r: &Record,
    admin: AdminThreads,
) -> Result<Option<Value>, crate::error::GatewayError> {
    let (Some(thread_id), Some((message_id, id))) = (r.thread_id, ids(r)) else {
        return Ok(None);
    };
    // As the thread is now: one a later write took out of a device's reach
    // is not shown (that write's own record says it went).
    let Some(thread) = store::get_chat_thread(&state.db, thread_id).await? else {
        return Ok(None);
    };
    if !admin.sees(thread.reach_level()) {
        return Ok(None);
    }
    let Some(pending) = store::get_chat_message(&state.db, thread_id, message_id)
        .await?
        .and_then(|m| m.pending_approvals)
    else {
        return Ok(None);
    };
    Ok(match r.kind.as_str() {
        kind::APPROVAL_REQUESTED => pending.call(&id).map(|c| {
            let req = store::approval_request_of(c);
            json!(ApprovalRequested {
                thread_id,
                message_id,
                approval_request_id: req.approval_request_id,
                server_label: req.server_label,
                name: req.name,
                arguments: req.arguments,
                call_id: req.call_id,
            })
        }),
        kind::APPROVAL_DECIDED => pending.decision(&id).map(|d| {
            json!(ApprovalDecided {
                thread_id,
                message_id,
                approval_request_id: id.clone(),
                approve: d.runs(),
                by: r.by.clone(),
                not_run: d.approve && d.not_run,
                call_id: Some(d.call.call_id.clone()),
            })
        }),
        _ => None,
    })
}
