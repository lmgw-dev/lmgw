//! The refusal of a `response.create` while a response generates (§4.3:
//! `conversation_already_has_active_response`) — and its words when that
//! response already answers what the create is for (realtime-server-tools
//! §2.5, final review #10).
//!
//! The case: the user's turn ends while a server-side call runs, so the
//! turn's automatic response starts right at the `response.done` that
//! carries the call, and `@openai/agents` sends its follow-up for the call
//! a moment later, into a generating response. The refusal stays: absorbing
//! a create into a response already running could swallow a deliberate
//! second one (§6.4's "an answer can end while a check is in flight" is the
//! precedent). But a bare "still in progress" reads like a lost answer, so
//! when the running response is the automatic one that started first after
//! the calls' results were in, it says that this response answers with them.

use super::super::protocol::ErrorObject;
use super::Core;

impl Core {
    /// The client's `response.create` (its `event_id`) while the active
    /// response generates: refused (module doc).
    pub(super) fn refuse_second(&mut self, event_id: Option<&str>) {
        let Some(active) = &self.active else {
            return;
        };
        let id = &active.output.id;
        let message = if active.answers_tools {
            format!(
                "response {id} is still in progress: it is the automatic response to the user's \
                 turn, and it already answers with the results of this session's last MCP calls — \
                 no second response is needed for them. Wait for its response.done, or send \
                 response.cancel"
            )
        } else {
            format!(
                "response {id} is still in progress; wait for its response.done, or send \
                 response.cancel"
            )
        };
        self.error(
            ErrorObject::invalid("conversation_already_has_active_response", message)
                .for_event(event_id),
        );
    }
}
