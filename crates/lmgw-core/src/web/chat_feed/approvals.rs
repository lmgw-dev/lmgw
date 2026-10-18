//! The `approval.*` records as a reader receives them (client-apps design
//! §6.5): rendered from the reply's pending state at delivery — the call's
//! label, name, arguments and id (`call_id`, as the turn's frames carry
//! it), and for a decision its verdict (an approved
//! call whose turn never started reads `approve: false, not_run: true`) —
//! with the record's author as `by`. A reply gone by then (deleted, regenerated, its
//! thread cut back or deleted) renders nothing: there is no call left to
//! decide or to show. Nor does a request whose call was decided by then:
//! its decision follows.
//!
//! **Notification-ready** (device link design §7.4): `approval.requested`
//! carries the thread's title at delivery, so a phone in the background
//! shows "Kai wants to run … in '<title>'" from the event alone.
//!
//! **The open set** (§7.3): `hello.open_approvals` lists the calls waiting
//! at the head the stream opened at, each rendered as `approval.requested`
//! is ([`open`]).

use lmgw_api_types::chat_feed::{ApprovalDecided, ApprovalRequested};
use serde_json::{json, Value};

use crate::agent::PendingCall;
use crate::state::AppState;
use crate::store::feed::{kind, Record, WaitingReply};
use crate::store::{self, AdminThreads};

/// `hello.open_approvals` for a reader that reaches as far as `admin`
/// (L3): every open call of the replies `waiting` at the head in a thread
/// the reader sees, oldest reply first, each reply's calls in the model's
/// order.
pub(super) fn open(waiting: &[WaitingReply], admin: AdminThreads) -> Vec<ApprovalRequested> {
    waiting
        .iter()
        .filter(|w| admin.sees(w.level))
        .flat_map(|w| {
            w.pending
                .open()
                .into_iter()
                .map(|c| requested(w.thread_id, &w.thread_title, w.message_id, c))
        })
        .collect()
}

/// Call `c` of reply `message_id` in thread `thread_id` (titled
/// `thread_title`) as `approval.requested` and `hello` carry it.
fn requested(
    thread_id: i64,
    thread_title: &str,
    message_id: i64,
    c: &PendingCall,
) -> ApprovalRequested {
    let req = store::approval_request_of(c);
    ApprovalRequested {
        thread_id,
        thread_title: thread_title.to_string(),
        message_id,
        approval_request_id: req.approval_request_id,
        server_label: req.server_label,
        name: req.name,
        arguments: req.arguments,
        call_id: req.call_id,
    }
}

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
        // A call decided by delivery is not shown as waiting: its
        // `approval.decided` follows, and a client would raise and withdraw
        // it in one breath (review of WP5, N3).
        kind::APPROVAL_REQUESTED if pending.decision(&id).is_some() => None,
        kind::APPROVAL_REQUESTED => pending
            .call(&id)
            .map(|c| json!(requested(thread_id, &thread.title, message_id, c))),
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
