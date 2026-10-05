//! What a run's record says for the calls it ended without a real result for
//! (chat-voice design §7.3), and the error a run fails with, which keeps the
//! record of what ran before it.
//!
//! Every call in a record must have its result. Strict upstreams refuse a
//! request that replays a tool call with none, so a record that stops short
//! wedges every later request on its conversation.

use serde_json::Value;

use super::{LoopEvent, ToolOutcome};
use crate::error::GatewayError;
use crate::ir::{ContentPart, Message, Role, Usage};

/// What a call's record says when a cancel landed while it was in flight.
/// Dropping the batch does not un-send what it sent, so whether the far side
/// ran it is not knowable from here, and saying so is the honest report.
pub(crate) const ABANDONED_CALL: &str =
    "abandoned when the run was cancelled; it had already been sent, so it may still have run";

/// What a call's record says when the run ended before making it: a cancel
/// raised before its batch started, a tool-call budget that ran out, a
/// client gone while the calls were announced.
pub(crate) const UNMADE_CALL: &str = "not run: the turn ended before this call was made";

/// A call the run closes with a result of its own rather than the tool's:
/// its run-global index, who it was, and what its result says.
pub(super) struct Closed {
    pub index: usize,
    pub call_id: String,
    pub name: String,
    pub server_label: String,
    pub says: String,
}

impl Closed {
    /// Each of `calls`, from run-global index `first`, closed with `says`.
    pub fn all(
        calls: &[(String, String, Value)],
        first: usize,
        says: &str,
        label_of: impl Fn(&str) -> Option<String>,
    ) -> Vec<Self> {
        calls
            .iter()
            .enumerate()
            .map(|(i, (id, name, _))| Self {
                index: first + i,
                call_id: id.clone(),
                name: name.clone(),
                server_label: label_of(name).unwrap_or_default(),
                says: says.to_string(),
            })
            .collect()
    }

    /// The call's result as the record keeps it.
    fn result(&self) -> ContentPart {
        ContentPart::ToolResult {
            id: self.call_id.clone(),
            name: Some(self.name.clone()),
            content: ToolOutcome::error(self.says.as_str()).blocks,
            is_error: true,
        }
    }

    /// The call's result as the run reports it.
    pub fn event(&self) -> LoopEvent {
        LoopEvent::CallResult {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            server_label: self.server_label.clone(),
            blocks: ToolOutcome::error(self.says.as_str()).blocks,
            is_error: true,
            ms: 0,
        }
    }
}

/// The tool message that answers `closed`, `None` when there is nothing to
/// answer.
pub(super) fn results(closed: &[Closed]) -> Option<Message> {
    (!closed.is_empty()).then(|| Message {
        role: Role::Tool,
        content: closed.iter().map(Closed::result).collect(),
    })
}

/// Give results to the calls of `record`'s trailing assistant message when
/// none follow it. Each call `says(name)` has a text for gets a result
/// saying it; a call it has none for is left open, for its owner to answer
/// (a client's call on `/v1/responses`, which the client runs).
///
/// A chat thread's loop never hands a call back, so every trailing call
/// there was never made; `/v1/responses` closes only the server-side ones
/// (chat-voice design §7.3).
pub(crate) fn close_trailing_calls(
    record: &mut Vec<Message>,
    says: impl Fn(&str) -> Option<String>,
) {
    let Some(last) = record.last().filter(|m| m.role == Role::Assistant) else {
        return;
    };
    let results: Vec<ContentPart> = last
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, name, .. } => {
                says(name).map(|text| ContentPart::ToolResult {
                    id: id.clone(),
                    name: Some(name.clone()),
                    content: crate::ir::ToolResultBlock::one(text),
                    is_error: true,
                })
            }
            _ => None,
        })
        .collect();
    if !results.is_empty() {
        record.push(Message {
            role: Role::Tool,
            content: results,
        });
    }
}

/// A run that failed: one of its model turns returned an error. What the run
/// did before that is kept, so a caller that stores the conversation keeps
/// the tools that ran, and the model learns that they ran. Otherwise a
/// side-effecting call is made again on the next request.
#[derive(Debug)]
pub struct RunError {
    pub error: GatewayError,
    /// The conversation up to the turn that failed: the request's messages,
    /// then every turn the loop finished, each with its calls' results. The
    /// failed turn is not in it.
    pub messages: Vec<Message>,
    /// What the finished turns spent (the failed turn's own row carries what
    /// it spent).
    pub usage: Usage,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl From<RunError> for GatewayError {
    fn from(e: RunError) -> Self {
        e.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolUse {
            id: id.into(),
            name: name.into(),
            args: serde_json::json!({}),
        }
    }

    #[test]
    fn trailing_calls_get_the_result_their_closer_gives_and_others_stay_open() {
        let mut record = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("both"),
                call("c1", "server"),
                call("c2", "client"),
            ],
        }];
        close_trailing_calls(&mut record, |name| {
            (name == "server").then(|| UNMADE_CALL.to_string())
        });
        assert_eq!(record.len(), 2);
        assert_eq!(record[1].role, Role::Tool);
        assert!(matches!(
            record[1].content.as_slice(),
            [ContentPart::ToolResult { id, is_error: true, .. }] if id == "c1"
        ));

        // A record that already ends in results is left alone.
        close_trailing_calls(&mut record, |_| Some("x".into()));
        assert_eq!(record.len(), 2);
    }
}
