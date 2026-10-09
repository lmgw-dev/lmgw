//! A temporary thread's approval writes (client-apps design §6), as
//! `store::chat_approvals` makes them for a stored one — under the temporary
//! store's one lock, so a decision and a new message never interleave. A
//! temporary thread is in no feed, so nothing is recorded there.

use super::TempChats;
use crate::store::{
    close_record, record_open, unrun_record, ApprovalRefusal, ChatMessageRow, ChatReply, Claimed,
    Decider, Verdict,
};

/// Decline what the last reply of `messages` still waits on, as `by`
/// moving on (as a stored thread's new message does).
pub(super) fn decline_on_message(messages: &mut [ChatMessageRow], by: &Decider) {
    let Some(last) = messages.last_mut().filter(|m| m.role == "assistant") else {
        return;
    };
    let Some(pending) = last.pending_approvals.as_mut() else {
        return;
    };
    if !pending.is_open() && !record_open(last.ir_messages.as_deref()) {
        return;
    }
    pending.move_on(by);
    if let Some(record) = close_record(last.ir_messages.as_deref(), pending) {
        last.ir_messages = Some(record);
    }
}

impl TempChats {
    /// Decide calls of thread `thread_id`, as a stored thread's are;
    /// `None` when the thread is gone.
    pub fn decide_approvals(
        &self,
        thread_id: i64,
        verdicts: &[Verdict],
        by: &Decider,
    ) -> Option<Result<Claimed, ApprovalRefusal>> {
        self.with_thread(thread_id, |t| {
            let row = t.messages.iter().rev().find(|m| {
                m.pending_approvals.as_ref().is_some_and(|p| {
                    verdicts
                        .iter()
                        .any(|v| p.call(&v.approval_request_id).is_some())
                })
            });
            let Some(row) = row else {
                return Err(if verdicts.is_empty() {
                    ApprovalRefusal::Empty
                } else {
                    ApprovalRefusal::Unknown(
                        verdicts
                            .iter()
                            .map(|v| v.approval_request_id.clone())
                            .collect(),
                    )
                });
            };
            let message_id = row.id;
            let mut pending = row.pending_approvals.clone().unwrap_or_default();
            let decided = pending.decide(verdicts, by)?;
            // Only the thread's last message can be resumed (as a stored
            // thread's claim checks in its write).
            if t.messages.last().map(|m| m.id) != Some(message_id) {
                return Err(ApprovalRefusal::MovedOn);
            }
            if let Some(row) = t.messages.iter_mut().find(|m| m.id == message_id) {
                row.pending_approvals = Some(pending.clone());
            }
            Ok(Claimed {
                message_id,
                pending,
                decided,
            })
        })
    }

    /// Close the approved calls `approved` of reply `id` whose turn never
    /// started, as a stored reply's are (as the store closes a stored reply's); the ids
    /// marked.
    pub fn close_never_run(&self, thread_id: i64, id: i64, approved: &[String]) -> Vec<String> {
        self.with_thread(thread_id, |t| {
            let Some(row) = t
                .messages
                .iter_mut()
                .find(|m| m.id == id && m.role == "assistant")
            else {
                return Vec::new();
            };
            let Some(pending) = row.pending_approvals.as_mut() else {
                return Vec::new();
            };
            let marked = pending.mark_unrun(approved);
            if !marked.is_empty() {
                if let Some(record) = unrun_record(row.ir_messages.as_deref(), pending) {
                    row.ir_messages = Some(record);
                }
            }
            marked
        })
        .unwrap_or_default()
    }

    /// Save a resumed turn onto reply `id` (as a stored reply is);
    /// `false` when it is gone.
    pub fn resume_reply(&self, thread_id: i64, id: i64, r: &ChatReply) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(row) = t
                .messages
                .iter_mut()
                .find(|m| m.id == id && m.role == "assistant")
            else {
                return false;
            };
            row.content = r.content.clone();
            row.reasoning = r.reasoning.clone();
            row.prompt_tokens = r.prompt_tokens;
            row.completion_tokens = r.completion_tokens;
            row.ir_messages = r.ir_messages.clone();
            row.model = r.model.clone();
            row.answered_by = r.answered_by.clone();
            row.images_note = r.images_note.clone();
            row.pending_approvals = r.pending_approvals.clone();
            t.touch();
            true
        })
        .unwrap_or(false)
    }
}
