//! `conversation.item.truncate` (realtime design §2.3, §4.3, §7.3).
//!
//! The client says how much of an assistant audio item it played; the item
//! is cut to that — its heard table, and with it the transcript the next
//! response renders (`conversation::audio`) — and the answer is
//! `conversation.item.truncated`. A cut beyond the item's audio is an
//! `error`, as with OpenAI, and changes nothing.
//!
//! **The item still being spoken** is a client-side stop of the rest of its
//! response (§4.3): its unplayed audio is purged and the response closes as
//! on a cancel — open items `incomplete`, `response.done {cancelled}` —
//! after the `truncated` answer. The stock client truncates after a
//! barge-in, when the response is already over; a client that stops
//! playback on its own lands here.

use super::Core;

impl Core {
    pub(in crate::realtime) fn item_truncate(
        &mut self,
        event_id: Option<&str>,
        item_id: &str,
        content_index: u32,
        audio_end_ms: u64,
    ) {
        let answer = self
            .conversation
            .truncate(item_id, content_index, audio_end_ms);
        match answer {
            Ok(ev) => self.ob.send(ev),
            Err(e) => return self.error(e.for_event(event_id)),
        }
        // A bound session's reply is cut to it in the thread too
        // (chat-voice §8.3).
        self.bound_truncated(item_id);
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.output.producing(item_id))
        {
            tracing::debug!(
                "realtime {}: {item_id} was truncated while it played; the rest of its response \
                 is cancelled",
                self.id()
            );
            self.cancel_active("client_cancelled");
        }
    }
}
