//! MCP approvals in a session bound to a chat thread (client-apps design
//! §6.4): OpenAI's items, on the binding's opt-in.
//!
//! - **A bound turn's gated call** — its `tool {event: "approval"}` frame —
//!   becomes an `mcp_approval_request` item (`conversation.item.added` and
//!   `.done`, `{id, type, server_label, name, arguments}`; its `id` is the
//!   approval id, as OpenAI's), and the response ends as the turn did.
//! - **The client answers** with `conversation.item.create` of
//!   `mcp_approval_response` (echoed as items too), then `response.create`:
//!   that response resumes the thread's turn through the Chat's own
//!   approvals (`web::chat_approvals`), the session's principal the
//!   approver, and the resumed turn — run as the principal that started it —
//!   runs the approved calls and answers in the same response. It writes no
//!   user turn, so the journal has no part in it: the reply it appends to is
//!   the gated turn's, already saved.
//! - **A decision made elsewhere** while one of the session's request items
//!   is open: the session says `lmgw.approval.decided {approval_request_id,
//!   approve, by}`, and drops the item from what it waits for. The deciding
//!   client's request runs the turn on. A call whose reply went or lost its
//!   calls meanwhile (edited, deleted, the thread cut back) is said the same
//!   way with `approve: false` and `by: null`: nobody decided it, and it
//!   never runs.
//!
//! An answer is checked when its response runs (the decision is the
//! Chat's, one write): an id nothing waits under, a call decided already,
//! or a waiting call left without an answer fails that response with the
//! route's code. A committed turn with words takes precedence: it is a new
//! message, which declines what waits, so a response that answers one runs
//! as ever and the queued answers go.

use lmgw_api_types::chat_approvals::ApprovalRequest;

use super::super::protocol::{ErrorObject, Item, McpApprovalRequestItem, ServerEvent};
use super::super::session::Core;
use crate::store::Verdict;

/// What a bound session keeps of its approvals.
#[derive(Debug, Default)]
pub(crate) struct Approvals {
    /// The request items it showed and nobody decided yet: their ids, which
    /// are their approval ids.
    open: Vec<String>,
    /// The client's answers, for the next `response.create`.
    answers: Vec<Verdict>,
}

impl Approvals {
    /// The answers queued for the next response, taken.
    pub(crate) fn take_answers(&mut self) -> Vec<Verdict> {
        std::mem::take(&mut self.answers)
    }

    pub(crate) fn has_answers(&self) -> bool {
        !self.answers.is_empty()
    }
}

impl Core {
    /// A bound turn's `tool {event: "approval"}` frame (module doc).
    pub(in crate::realtime) fn approval_requested(&mut self, data: &serde_json::Value) {
        let Ok(req) = serde_json::from_value::<ApprovalRequest>(data.clone()) else {
            return;
        };
        // The item's id is the approval id, as OpenAI's is: the client's
        // `mcp_approval_response` quotes it as its `approval_request_id`.
        let item = Item::McpApprovalRequest(McpApprovalRequestItem {
            id: Some(req.approval_request_id.clone()),
            server_label: req.server_label,
            name: req.name,
            arguments: req.arguments,
        });
        let previous_item_id = self.conversation.append(item.clone());
        self.ob.send(ServerEvent::ItemAdded {
            previous_item_id: previous_item_id.clone(),
            item: item.clone(),
        });
        self.ob.send(ServerEvent::ItemDone {
            previous_item_id,
            item,
        });
        if let Some(b) = self.bound.as_mut() {
            b.approvals.open.push(req.approval_request_id);
        }
    }

    /// A client's `mcp_approval_response` in a bound session (module doc):
    /// echoed, and queued for the next response.
    pub(in crate::realtime) fn approval_answered(
        &mut self,
        event_id: Option<&str>,
        mut item: Item,
    ) {
        let Item::McpApprovalResponse(answer) = &item else {
            return;
        };
        if answer.approval_request_id.trim().is_empty() {
            return self.error(
                ErrorObject::invalid(
                    "missing_required_parameter",
                    "an mcp_approval_response names the approval_request_id it answers",
                )
                .with_param("item.approval_request_id")
                .for_event(event_id),
            );
        }
        let verdict = Verdict {
            approval_request_id: answer.approval_request_id.clone(),
            approve: answer.approve,
            reason: answer.reason.clone(),
        };
        let ids = self.ids.clone();
        item.set_id_if_missing(|| self.conversation.fresh_item_id(&ids));
        let previous_item_id = self.conversation.append(item.clone());
        self.ob.send(ServerEvent::ItemAdded {
            previous_item_id: previous_item_id.clone(),
            item: item.clone(),
        });
        self.ob.send(ServerEvent::ItemDone {
            previous_item_id,
            item,
        });
        if let Some(b) = self.bound.as_mut() {
            b.approvals
                .open
                .retain(|id| *id != verdict.approval_request_id);
            b.approvals
                .answers
                .retain(|v| v.approval_request_id != verdict.approval_request_id);
            b.approvals.answers.push(verdict);
        }
    }

    /// The thread's approvals were decided somewhere (`chat_live`'s wake,
    /// or a lag of it): each request item this session still holds open
    /// and a client decided is said decided (module doc).
    pub(in crate::realtime) async fn approvals_woken(&mut self) {
        let Some(b) = self.bound.as_ref() else {
            return;
        };
        if b.approvals.open.is_empty() {
            return;
        }
        let thread_id = b.thread_id;
        let messages = crate::web::chat_voice::bound::messages(&self.state, thread_id).await;
        let holder = |id: &str| {
            messages
                .iter()
                .rev()
                .filter_map(|m| m.pending_approvals.as_ref())
                .find(|p| p.call(id).is_some())
        };
        let mut said = Vec::new();
        if let Some(b) = self.bound.as_mut() {
            b.approvals.open.retain(|id| match holder(id) {
                // Still waiting.
                Some(p) if p.decision(id).is_none() => true,
                Some(p) => {
                    let d = p.decision(id).cloned();
                    said.push(ServerEvent::LmgwApprovalDecided {
                        approval_request_id: id.clone(),
                        approve: d.as_ref().is_some_and(|d| d.runs()),
                        by: d.map(|d| d.named),
                    });
                    false
                }
                // Its reply went, or lost its calls (edited, deleted, the
                // thread cut back): nobody decided it, and it never runs.
                None => {
                    said.push(ServerEvent::LmgwApprovalDecided {
                        approval_request_id: id.clone(),
                        approve: false,
                        by: None,
                    });
                    false
                }
            });
        }
        for ev in said {
            self.ob.send(ev);
        }
    }
}
