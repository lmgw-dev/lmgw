//! The session's memory of a model that refused a heard turn's audio, or
//! failed on it (voice-audio-input design §3.5) — what the responder keeps
//! ([`Refused`]) and the core's half: the note that a response goes as its
//! transcript, and the memory the verdict's session row reads
//! (`Bound.refused`, `AudioInput::after_refusal`).
//!
//! **Keyed by the model that refused**, as a turn names who answered: a
//! candidate alias's pick by its public name, else the thread's model. A
//! thread switched to another model, or a candidate alias that may still
//! pick a candidate that did not refuse, hears again.

use crate::realtime::protocol::ServerEvent;
use crate::realtime::session::Core;
use crate::store::InputPath;

/// A model whose server refused a heard turn's audio, or failed on it: the
/// model, as a turn names who answered, and the verdict's `why` for it
/// from then on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub model: String,
    pub why: String,
}

impl Core {
    /// A bound response goes as its transcript after all (`Msg::Input`):
    /// logged, said as `lmgw.chat.input` and kept for the timing's
    /// `input_why` (§5).
    pub(in crate::realtime) fn bound_input(&mut self, gen: u64, input: InputPath, why: String) {
        let id = self.id().to_string();
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        let response = b.response_ids.get(&gen).cloned().unwrap_or_default();
        tracing::info!(
            "realtime {id}: response {response} goes to the chat model as its {}: {why}",
            input.as_str()
        );
        if let Some(served) = b.responses.get_mut(&gen) {
            served.input = Some((input, Some(why.clone())));
        }
        self.ob.send(ServerEvent::LmgwChatInput {
            response_id: response,
            input,
            why: Some(why),
        });
    }

    /// A model's server refused a heard turn's audio, or failed on it
    /// (`Msg::Refused`): kept for the session, by model, so its later turns
    /// to that model go as their transcript; `note`, when no transcript
    /// retry said it already, goes to the client as `lmgw.chat.input` —
    /// about the later turns, so the response's own timing keeps its path.
    pub(in crate::realtime) fn bound_refused(
        &mut self,
        gen: u64,
        refused: Refused,
        note: Option<String>,
    ) {
        let id = self.id().to_string();
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        let response = b.response_ids.get(&gen).cloned().unwrap_or_default();
        tracing::info!(
            "realtime {id}: {} ({}): the session's later turns to it go as their transcript",
            refused.model,
            refused.why
        );
        b.refused.insert(refused.model, refused.why);
        if let Some(why) = note {
            self.ob.send(ServerEvent::LmgwChatInput {
                response_id: response,
                input: InputPath::Transcript,
                why: Some(why),
            });
        }
    }
}
