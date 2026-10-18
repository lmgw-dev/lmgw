//! A call's stops (realtime calls design §6.2, C12): a bound client's
//! `lmgw.response.stop`, and OpenAI's `output_audio_buffer.clear`.
//!
//! **`lmgw.response.stop {playout_ntp_ms?}`** is the user's stop on a bound
//! call, and the first half of the End (§8.1). It is idempotent and cancels
//! whatever is open:
//! - the active response, in any phase a cancel changes (thinking,
//!   generating, speaking, waiting on a server-side tool or an approval),
//!   with a cancel's events: the cancelled `response.done`
//!   (`client_cancelled`), then `output_audio_buffer.cleared` for a
//!   response that spoke (the writer's). Its unsent audio is purged, and
//!   heard is where the phone muted, `playout_ntp_ms` mapped through lmgw's
//!   sender reports (`writer::Cut::At`, §5.4), or what was sent without it.
//!   A response past its generation with every item closed (a text response
//!   whose tool call the client may have run) finishes as generated, by
//!   `response.cancel`'s own rule ([`Active::cancellable`]);
//! - the response owed to committed turns and a client's held
//!   `response.create` (the Pending phase), dropped, so nothing starts
//!   speaking after the tap; the turns stay in the conversation, unanswered.
//!   *(Changed 2026-10-15: a `response.create` queued behind the active
//!   response — sent while it closed or played — is dropped too, for the
//!   same reason: it would start speaking right after the cancel.)*
//!
//! With nothing open it sends nothing (a DEBUG line), and never
//! `response_cancel_not_active`: a stop that crossed the response's end is
//! no error.
//!
//! **`output_audio_buffer.clear`** on a call: while a response's audio is in
//! the call's output buffer — its `output_audio_buffer.started` went out,
//! and its `response.done`, which waits for drained, has not — it cancels
//! that response as `response.cancel` does (`client_cancelled`), and
//! `cleared` follows its `response.done`. With no audio playing (right after
//! the `response.cancel` that already cut it, or nothing spoken at all) it
//! sends nothing, and a DEBUG line notes it. A WebSocket answers it with
//! `not_implemented_yet` (`session::events`).
//!
//! [`Active::cancellable`]: super::Active

use super::super::writer::Cut;
use super::Core;

impl Core {
    /// `lmgw.response.stop` (module doc), with the phone's playout position
    /// at the tap when it had one.
    pub(in crate::realtime) fn response_stop(&mut self, playout_ntp_ms: Option<f64>) {
        let owed = self.pending_stop();
        let queued = self.active.as_mut().and_then(|a| a.queued.take()).is_some();
        let cancel = self
            .active
            .as_ref()
            .filter(|a| a.cancellable())
            .map(|a| (a.output.gen, a.output.id.clone()));
        let mut said = Vec::new();
        if let Some(owed) = owed {
            said.push(owed);
        }
        if queued {
            said.push("the response.create queued behind the active response".to_string());
        }
        if let Some((gen, rid)) = cancel {
            let cut = playout_ntp_ms.map_or(Cut::Sent, Cut::At);
            let sent = self.out.purge_cut(gen, cut);
            self.cancel_purged("client_cancelled", &sent);
            said.push(format!("response {rid}"));
        } else if let Some(a) = &self.active {
            tracing::debug!(
                "realtime {}: lmgw.response.stop after the end of generation; {} finishes as \
                 generated",
                self.id(),
                a.output.id
            );
        }
        if said.is_empty() {
            tracing::debug!(
                "realtime {}: lmgw.response.stop with nothing open; nothing to stop",
                self.id()
            );
        } else {
            tracing::debug!(
                "realtime {}: lmgw.response.stop stopped {}",
                self.id(),
                said.join(", ")
            );
        }
    }

    /// `output_audio_buffer.clear` on a call (module doc).
    pub(in crate::realtime) fn output_clear(&mut self) {
        let playing = self
            .active
            .as_ref()
            .filter(|a| self.out.speaking(&a.output.id) && a.cancellable())
            .is_some();
        if playing {
            self.cancel_active("client_cancelled");
        } else {
            tracing::debug!(
                "realtime {}: output_audio_buffer.clear with no audio playing; nothing to clear",
                self.id()
            );
        }
    }
}
