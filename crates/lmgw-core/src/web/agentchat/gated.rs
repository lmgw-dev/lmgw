//! A Chat turn that stopped on a gated call (client-apps design §6.2, L13).
//!
//! The loop hands back the calls it stopped on (`agent::PendingCall`): the
//! gated ones and the siblings that wait with them. Here each gated call
//! gets an id of its own (`mcpr_…`, unique in the gateway, whatever ids the
//! model gave its calls), and the lot is stored with the key of the
//! principal the turn runs as, which the resumed turn runs as again. Once
//! the reply is saved, each gated call is announced as a `tool` frame
//! `{event: "approval", …, call_id}` (`call_id` the model's id for the
//! call, as its `ready` and `result` frames carry it), and `done` lists them
//! as `pending_approvals`.

use serde_json::json;

use super::super::chat_turn::{Events, TurnFrame};
use crate::agent::PendingCall;
use crate::store::{approval_request_of, mint_approval_id, PendingApprovals};

/// The stored state of a turn that stopped on `calls`, started by the key
/// `starter` (id and name; `None`: the gateway's own run).
pub(super) fn pending_of(
    mut calls: Vec<PendingCall>,
    (key_id, key_name): (Option<i64>, Option<String>),
) -> PendingApprovals {
    for c in calls.iter_mut().filter(|c| c.needs_approval) {
        c.approval_id = mint_approval_id();
    }
    PendingApprovals {
        key_id,
        key_name,
        calls,
        decided: Vec::new(),
    }
}

/// One `tool {event: "approval"}` frame per waiting call (module doc), in
/// the model's order.
pub(super) async fn announce(tx: &Events, pending: &PendingApprovals) {
    for c in pending.open() {
        let r = approval_request_of(c);
        let data = json!({
            "event": "approval",
            "approval_request_id": r.approval_request_id,
            "server_label": r.server_label,
            "name": r.name,
            "arguments": r.arguments,
            // The id its `ready` and `result` frames carry.
            "call_id": r.call_id,
        });
        let _ = tx.send(TurnFrame::new("tool", data.to_string())).await;
    }
}
