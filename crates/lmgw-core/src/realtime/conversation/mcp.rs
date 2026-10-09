//! The `mcp_*` items a client may create (realtime-server-tools design
//! §2.6): an `mcp_call` is history a client replays, and is taken with its
//! `output` or `error`; an `mcp_list_tools` item is the server's record of a
//! listing, and the approval items wait for approvals (§6). Those three are
//! refused with `invalid_value` naming the type.
//!
//! The approval items do not parse as an `Item` at all, so
//! their refusal is decided from the frame (`session::events`); an
//! `mcp_list_tools` item parses, and `check_item` refuses it.
//!
//! **What the session keeps beside an `mcp_call`** ([`McpCallRecord`],
//! §2.2, §2.6): the call id it is rendered with — the item carries none —
//! minted session-unique as a function call's is, for a call the model made
//! and for a client's replayed one alike; the name the model called it by;
//! its result's blocks as the tool gave them, images included, where the
//! item's `output` is their text; and whether the call is still open.
//!
//! **A call that is not done cannot be deleted** (§2.5): an `mcp_call` has
//! no `status` for the conversation's in-progress refusal to read, and its
//! remaining events — its result, `.failed` on a cancel — would name an item
//! the client was told is gone. Cancel the response first.

use serde_json::Value;

use super::super::ids::Ids;
use super::super::protocol::{ErrorObject, Item};
use super::Conversation;
use crate::ir::ToolResultBlock;

/// What the session keeps beside one `mcp_call` (module doc).
#[derive(Debug, Clone, Default)]
pub(crate) struct McpCallRecord {
    pub call_id: String,
    /// The exposed name the model called; `None` for a client's replayed
    /// item, which renders by its label's names.
    pub exposed: Option<String>,
    /// The result as the tool gave it; `None` until it is in, and for a
    /// replayed item.
    pub blocks: Option<Vec<ToolResultBlock>>,
    /// The response that made it has not closed it yet: unmade or running
    /// (module doc). A replayed item never is.
    pub open: bool,
}

impl Conversation {
    /// The model called `exposed` as the `mcp_call` `item_id`, its call id
    /// `call_id` (already noted as used).
    pub fn note_mcp_call(&mut self, item_id: &str, call_id: &str, exposed: &str) {
        self.mcp_calls.insert(
            item_id.to_string(),
            McpCallRecord {
                call_id: call_id.to_string(),
                exposed: Some(exposed.to_string()),
                blocks: None,
                open: true,
            },
        );
    }

    /// The `mcp_call` `item_id` closed: with its result, or unfinished.
    pub fn mcp_call_done(&mut self, item_id: &str) {
        if let Some(r) = self.mcp_calls.get_mut(item_id) {
            r.open = false;
        }
    }

    /// Why `item_id` cannot be deleted now, if it is an `mcp_call` its
    /// response has not closed (module doc).
    pub(super) fn mcp_delete_refusal(&self, item_id: &str) -> Option<ErrorObject> {
        self.mcp_calls.get(item_id).filter(|r| r.open)?;
        Some(
            ErrorObject::invalid(
                "invalid_value",
                format!(
                    "item '{item_id}' is an mcp_call the active response has not finished (not \
                     made yet, or still running); send response.cancel first, or wait for its \
                     conversation.item.done"
                ),
            )
            .with_param("item_id"),
        )
    }

    /// The result of the `mcp_call` `item_id`, as the tool gave it.
    pub fn keep_mcp_result(&mut self, item_id: &str, blocks: Vec<ToolResultBlock>) {
        if let Some(r) = self.mcp_calls.get_mut(item_id) {
            r.blocks = Some(blocks);
        }
    }

    /// What the session keeps beside the `mcp_call` `item_id`.
    pub fn mcp_record(&self, item_id: &str) -> Option<&McpCallRecord> {
        self.mcp_calls.get(item_id)
    }

    /// A client's `item`, about to be inserted: a replayed `mcp_call` gets a
    /// call id of its own.
    pub(super) fn note_replayed(&mut self, item: &Item, ids: &Ids) {
        let Item::McpCall(c) = item else {
            return;
        };
        let call_id = self.fresh_call_id(ids);
        self.note_call_id(&call_id);
        if let Some(id) = c.id.as_deref() {
            self.mcp_calls.insert(
                id.to_string(),
                McpCallRecord {
                    call_id,
                    ..Default::default()
                },
            );
        }
    }
}

const LIST_TOOLS: &str =
    "a session lists an MCP label's tools itself, when the label is in session.tools";
const APPROVAL_REQUEST: &str = "the gateway makes these, when a call waits for an approval";
const APPROVAL_RESPONSE: &str = "approvals are taken on a session bound to a chat thread only \
     (?chat_thread=), whose thread's tools ask for them; a session's own mcp tools never ask for \
     one";

fn refused(kind: &str, why: &str) -> ErrorObject {
    ErrorObject::invalid(
        "invalid_value",
        format!("a client cannot create an {kind} item: {why}"),
    )
    .with_param("item.type")
}

/// A client's `mcp_list_tools` item.
pub(super) fn list_tools_refusal() -> ErrorObject {
    refused("mcp_list_tools", LIST_TOOLS)
}

/// A client's `mcp_approval_request` item.
pub(super) fn approval_request_refusal() -> ErrorObject {
    refused("mcp_approval_request", APPROVAL_REQUEST)
}

/// A client's `mcp_approval_response` item on a session bound to no chat
/// thread (a bound one takes it, `thread::approvals`).
pub(super) fn approval_response_refusal() -> ErrorObject {
    refused("mcp_approval_response", APPROVAL_RESPONSE)
}

/// The refusal for a `conversation.item.create` frame whose item is one of
/// the refused types — asked when the frame did not parse.
pub(in crate::realtime) fn refusal_of_frame(frame: &Value) -> Option<ErrorObject> {
    if frame.get("type").and_then(Value::as_str) != Some("conversation.item.create") {
        return None;
    }
    let kind = frame.pointer("/item/type").and_then(Value::as_str)?;
    let why = match kind {
        "mcp_list_tools" => LIST_TOOLS,
        "mcp_approval_request" => APPROVAL_REQUEST,
        // A malformed answer gets the shape's own error: a bound session
        // takes a well-formed one.
        _ => return None,
    };
    Some(refused(kind, why))
}
