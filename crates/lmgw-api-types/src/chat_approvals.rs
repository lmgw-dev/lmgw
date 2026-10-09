//! MCP approvals in the Chat (client-apps design §6): a thread's tools may
//! wait for an approval (`ThreadMcp::require_approval`), a gated turn stops
//! on its calls, and any Chat client decides them with
//! `POST /chat/api/threads/{id}/approvals`.
//!
//! **A gated turn** streams, for each call that waits, a `tool` frame
//! `{event: "approval", approval_request_id, server_label, name, arguments,
//! call_id}` ([`ApprovalRequest`] with `event`), then `done` with
//! `pending_approvals: [ApprovalRequest]`. The reply is saved with the
//! calls; the thread's message JSON lists the open ones as its
//! `pending_approvals`. `arguments` is a JSON string, as OpenAI's, and
//! `name` the tool's own name, without its label's `<prefix>__`. `call_id`
//! is the call's id as its `ready` and `result` frames carry it
//! ([`crate::mcp_apps::ToolReadyFrame`]), so a client matches an approval
//! to its call by id.
//!
//! **Deciding** ([`ApprovalsRequest`]) resumes the turn and streams the
//! frames a send streams. Every waiting call needs a verdict; the first
//! decision wins; a new message in the thread declines what still waits
//! ([`MOVED_ON`]).
//!
//! **A bound voice session** sees OpenAI's `mcp_approval_request` item and
//! answers with `mcp_approval_response` and `response.create`; a decision
//! made elsewhere while its item is open is [`ApprovalDecidedEvent`]
//! (`lmgw.approval.decided`). The item stays OpenAI's shape, which has no
//! call id: the `lmgw.chat.frame` relaying the turn's `approval` frame,
//! sent right after the item, carries `call_id` beside the same
//! `approval_request_id`.
//!
//! **The feed** records [`crate::chat_feed::ApprovalRequested`] and
//! [`crate::chat_feed::ApprovalDecided`], each with the call's `call_id`.

use serde::{Deserialize, Serialize};

/// The route's path, `{id}` the thread's.
pub const PATH: &str = "/chat/api/threads/{id}/approvals";

/// One call that waits for an approval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalRequest {
    /// The id a decision quotes (`mcpr_…`), unique in the gateway.
    pub approval_request_id: String,
    /// The thread's label the tool came from.
    pub server_label: String,
    /// The tool's own name: the name the model called, without its
    /// label's `<prefix>__`.
    pub name: String,
    /// The arguments as the model wrote them: a JSON string.
    pub arguments: String,
    /// The call's id, as its `ready` and `result` frames carry it
    /// ([`crate::mcp_apps::ToolReadyFrame::call_id`]). Absent from an lmgw
    /// before 2026-10-09, and from an [`ApprovalRequest`] read off a bound
    /// session's `mcp_approval_request` item (OpenAI's shape has none):
    /// there the `lmgw.chat.frame` relaying the `approval` frame has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// One verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalDecision {
    pub approval_request_id: String,
    pub approve: bool,
    /// Why, for a declined call: the model reads "The user declined this
    /// tool call: <reason>".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST /chat/api/threads/{id}/approvals`: a verdict for every call the
/// thread's last reply waits on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalsRequest {
    pub decisions: Vec<ApprovalDecision>,
    /// Read the resumed reply aloud as it streams, as a send's `speak`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub speak: bool,
}

/// The refusal codes of the route.
pub mod code {
    /// `409`: a call was decided already; the message names who decided.
    pub const APPROVAL_DECIDED: &str = "approval_decided";
    /// `404`: no call of this thread waits under that id.
    pub const APPROVAL_NOT_FOUND: &str = "approval_not_found";
    /// `400`: a waiting call has no verdict; the message names each.
    pub const APPROVAL_MISSING: &str = "approval_missing";
    /// `409`: the key that started the turn is gone or disabled; the
    /// resumed turn runs as its starter, so it cannot run. The message names
    /// the key.
    pub const APPROVAL_STARTER_UNAVAILABLE: &str = "approval_starter_unavailable";
    /// `409`: the reply the calls belong to is no longer the thread's last
    /// message (a new message declined them, or it was edited or deleted).
    pub const APPROVAL_MOVED_ON: &str = "approval_moved_on";
    /// `403`: a device approved a call beyond its own reach — a tool its
    /// key's tool scope does not admit, or one of lmgw's admin tools while
    /// its admin tools may not do everything (`full`). The message names
    /// each. It may still decline them. Nothing is decided.
    pub const APPROVAL_OUT_OF_SCOPE: &str = "approval_out_of_scope";
}

/// What a call that still waited reads when a new message came: it is
/// declined, with this reason.
pub const MOVED_ON: &str = "the user moved on without deciding";

/// `lmgw.approval.decided` (a bound session's own event): a call whose
/// `mcp_approval_request` item the session showed was decided by another
/// client. `by` names who, as the feed names authors ("the dashboard",
/// "device 'phone'"). `approve` is `false` for a call approved whose turn
/// could not start, so it never ran. A call whose reply was edited or
/// deleted meanwhile is said with `approve: false` and `by: None`: nobody
/// decided it, and it never runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalDecidedEvent {
    pub approval_request_id: String,
    pub approve: bool,
    pub by: Option<String>,
}

/// The `type` of [`ApprovalDecidedEvent`].
pub const APPROVAL_DECIDED_EVENT: &str = "lmgw.approval.decided";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_reads_with_and_without_a_reason() {
        let r: ApprovalsRequest = serde_json::from_str(
            r#"{"decisions": [{"approval_request_id": "mcpr_1", "approve": true},
                              {"approval_request_id": "mcpr_2", "approve": false,
                               "reason": "not now"}]}"#,
        )
        .unwrap();
        assert_eq!(r.decisions.len(), 2);
        assert!(!r.speak);
        assert_eq!(r.decisions[1].reason.as_deref(), Some("not now"));
        let back = serde_json::to_value(&r.decisions[0]).unwrap();
        assert!(back.get("reason").is_none());
    }

    #[test]
    fn a_request_reads_with_and_without_its_call_id() {
        let r: ApprovalRequest = serde_json::from_str(
            r#"{"approval_request_id": "mcpr_1", "server_label": "wx", "name": "show",
                "arguments": "{}", "call_id": "call_7"}"#,
        )
        .unwrap();
        assert_eq!(r.call_id.as_deref(), Some("call_7"));
        let old: ApprovalRequest = serde_json::from_str(
            r#"{"approval_request_id": "mcpr_1", "server_label": "wx", "name": "show",
                "arguments": "{}"}"#,
        )
        .unwrap();
        assert_eq!(old.call_id, None);
        assert!(serde_json::to_value(&old).unwrap().get("call_id").is_none());
    }
}
