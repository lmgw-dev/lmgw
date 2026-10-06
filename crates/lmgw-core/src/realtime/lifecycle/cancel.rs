//! `response.cancel` (realtime design §4.3 "Cancel").
//!
//! A cancel is **cooperative**: it raises the response's stop, and the model
//! call ends at its next await — never aborted, so it still writes its
//! `request_logs` row with status `canceled` and what it cost so far, which
//! counts against the key's tokens and spend (§11). A client cannot take an
//! answer budget-free by cancelling at its end, and a barge-in does not
//! vanish from Usage. The call is detached: what it says afterwards is
//! dropped by its generation.
//!
//! Then, in order: the response's queued **audio** is purged — text already
//! sent stays, and a spoken item keeps the audio the writer sent, cut in its
//! heard table (§7.3: "a cancel keeps what was sent"); its open items close
//! `incomplete` (`output_item.done`, `conversation.item.done`,
//! without a generation, since `@openai/agents` updates its history only from
//! item events — a spoken message's audio part first, with its heard
//! transcript, since the SDK resets its audio counters only on
//! `output_audio.done`); and `response.done {cancelled, reason: client_cancelled}`
//! goes out, with the usage the upstream reported so far.
//!
//! **After generation has ended, in text mode**, its message and function
//! call items are closed `completed` — the client may have run its tool —
//! and stay so. With nothing else open a cancel changes nothing: the
//! response finishes as `completed` when its call returns and its output has
//! drained. A server-side call still open keeps the response cancellable
//! (realtime-server-tools §2.5): the cancel closes it as abandoned — as never
//! made, if it was not sent yet — and the response ends `cancelled`. A
//! cancel with nothing active, or for a response that is not the active one,
//! is `error {code: "response_cancel_not_active"}`, as with OpenAI.

use super::super::output;
use super::super::protocol::{ErrorObject, ResponseStatus, StatusDetails};
use super::super::writer::Sent;
use super::{Active, Core, Phase};

impl Core {
    /// `response.cancel`.
    pub(in crate::realtime) fn response_cancel(
        &mut self,
        event_id: Option<&str>,
        response_id: Option<&str>,
    ) {
        let not_active = |why: String| {
            ErrorObject::invalid("response_cancel_not_active", why).for_event(event_id)
        };
        let Some(active) = &self.active else {
            return self.error(not_active(
                "there is no active response to cancel".to_string(),
            ));
        };
        if let Some(rid) = response_id.filter(|r| *r != active.output.id) {
            let why = format!(
                "response {rid} is not active (the active response is {})",
                active.output.id
            );
            return self.error(not_active(why));
        }
        if !active.cancellable() {
            tracing::debug!(
                "realtime {}: response.cancel after the end of generation; {} finishes as \
                 generated",
                self.id(),
                active.output.id
            );
            return;
        }
        self.cancel_active("client_cancelled");
    }

    /// Cancel the active response now (module doc), `reason` being the
    /// `status_details.reason` — `client_cancelled`, or a barge-in's
    /// `turn_detected` (`interrupt`).
    pub(in crate::realtime) fn cancel_active(&mut self, reason: &str) {
        let Some(gen) = self.active.as_ref().map(|a| a.output.gen) else {
            return;
        };
        // Ahead of every event still waiting for the socket; what of its
        // audio left is what the items keep.
        let sent = self.out.purge(gen);
        self.cancel_purged(reason, &sent);
    }

    /// [`Self::cancel_active`] once its output was purged, and `sent` is
    /// what of it had left — a barge-in decides what it owes again on that
    /// first (`interrupt`).
    pub(in crate::realtime) fn cancel_purged(&mut self, reason: &str, sent: &Sent) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(stop) = active.call.take() {
            stop.stop();
        }
        let (conv, ids, ob) = (&mut self.conversation, &self.ids, &mut self.ob);
        active.output.keep_sent(conv, &sent.audio);
        active.output.abandon(conv, ids, ob);
        active.output.done(
            conv,
            ob,
            ResponseStatus::Cancelled,
            Some(StatusDetails {
                kind: ResponseStatus::Cancelled,
                reason: Some(reason.into()),
                error: None,
            }),
            output::known_usage(&active.usage),
        );
        // A response cut while held was heard by nobody (`held`).
        self.held_over(&active);
        self.log_timing(&active, ResponseStatus::Cancelled);
        self.next_response(active);
    }
}

impl Active {
    /// Whether a cancel still changes anything (module doc): not after the
    /// end of generation once every item is closed — a text response whose
    /// tool call the client may already have run. The one predicate for
    /// `response.cancel` and a barge-in alike (WP3 review H4), so a barge-in
    /// never marks a completed call cancelled.
    pub(super) fn cancellable(&self) -> bool {
        let generated = matches!(self.phase, Phase::Closing | Phase::Playing);
        !(generated && self.output.all_closed())
    }
}
