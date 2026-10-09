//! A turn resumed after its calls were decided (client-apps design §6.3):
//! what it continues and how its reply is saved.
//!
//! The reply it resumes is the thread's last message: a gated turn's reply,
//! its tool record ending in the calls it stopped on. The resumed turn's
//! history replays that record as it stands, the tool loop settles the
//! decided calls first (their results are the next thing the model sees),
//! and the turn goes on as any tool turn. Its reply is **appended** to the
//! one it resumes: the text grows by what the turn said, the record by what
//! it did, and the pending state takes the calls of a new stop, should the
//! model call a gated tool again — its decisions so far kept, so a second
//! decision on an old call is still told who made the first.

use axum::http::StatusCode;
use axum::response::Response;

use crate::agent::DecidedCall;
use crate::ir::Message;
use crate::store::{ChatMessageRow, ChatReply, PendingApprovals};

/// What a resumed turn is handed ([`super::TurnOpts::resume`]).
#[derive(Debug, Clone)]
pub(crate) struct Resume {
    /// The reply it resumes.
    pub message_id: i64,
    /// The decided calls, in the model's order: settled before the first
    /// model call.
    pub decided: Vec<DecidedCall>,
}

/// The reply a resumed turn appends to, as it stood when the turn began.
#[derive(Debug, Clone)]
pub(crate) struct Prior {
    pub message_id: i64,
    content: String,
    reasoning: String,
    record: Vec<Message>,
    /// With the decisions that resumed it.
    pub pending: PendingApprovals,
}

/// Why a resume is refused when its reply is no longer the thread's last
/// message.
pub(crate) const MOVED_ON_WHY: &str = "the reply these calls belong to is no longer the thread's \
     last message (a new message declined them, or the reply was edited or deleted), so its turn \
     cannot be resumed";

/// The refusal of a resume whose reply is no longer the thread's last
/// message: a new message declined its calls, or it was edited or deleted.
pub(crate) fn moved_on() -> Response {
    super::super::chat::err_json(
        StatusCode::CONFLICT,
        lmgw_api_types::chat_approvals::code::APPROVAL_MOVED_ON,
        MOVED_ON_WHY,
    )
}

/// The reply `message_id` of `history`, when it is the last message and a
/// gated turn's reply.
pub(super) fn prior(history: &[ChatMessageRow], message_id: i64) -> Option<Prior> {
    let last = history
        .last()
        .filter(|m| m.id == message_id && m.role == "assistant")?;
    let pending = last.pending_approvals.clone()?;
    let record = last
        .ir_messages
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or_default();
    Some(Prior {
        message_id,
        content: last.content.clone(),
        reasoning: last.reasoning.clone(),
        record,
        pending,
    })
}

impl Prior {
    /// The reply as saved: `added` (what the resumed turn said and did, its
    /// record the loop's new messages) after what was there, and the
    /// pending state with a new stop's calls, if the turn stopped again.
    pub(super) fn merged(&self, added: ChatReply, stopped: Option<PendingApprovals>) -> ChatReply {
        let new_record: Vec<Message> = added
            .ir_messages
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok())
            .unwrap_or_default();
        let mut record = self.record.clone();
        record.extend(new_record);
        let mut pending = self.pending.clone();
        if let Some(stop) = stopped {
            pending.calls = stop.calls;
        }
        ChatReply {
            content: format!("{}{}", self.content, added.content),
            reasoning: format!("{}{}", self.reasoning, added.reasoning),
            ir_messages: serde_json::to_string(&record).ok(),
            pending_approvals: Some(pending),
            ..added
        }
    }
}
