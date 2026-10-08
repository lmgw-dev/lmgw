//! What a session's refusals say on the panel's line (chat-voice §4.3,
//! §9.1): a bound response fails with the chat turn's own gateway error, and
//! a turn's transcription with its code (the WP11 server batch), so each is
//! worded by its code (`state::turn_refusal`).
//!
//! **One note per refusal** (WP11 UI review m6): a hold refuses the chat
//! model at its resolve, and the turn says `lmgw.model.state {stage: chat,
//! state: held}` first and then the `error {gpu_hold}`; a VRAM wait says
//! `failed` and then `vram_queue_timeout`. The refusal is said under the
//! chat stage's key, so it replaces that note: one amber chip for a hold,
//! never two. An ASR the hold refuses replaces the `asr` stage's note the
//! same way.

use std::rc::Rc;

use super::super::super::state::{refusal_link, turn_refusal, Note, NoteKind, Surface, TURN_KEY};
use super::Live;
use lmgw_client::realtime::ErrorFacts;

/// The key of a turn's own notes (a transcription that failed, a reply that
/// stopped): the next response clears it.
pub(super) const TURN_NOTE: &str = "turn";

impl Live {
    pub(super) fn on_error(self: &Rc<Self>, e: ErrorFacts) {
        let code = e.code.clone().unwrap_or_default();
        *self.last_error.borrow_mut() = Some(e.clone());
        if self.machine.borrow().current().is_none() {
            self.machine.borrow_mut().turn_failed();
        }
        match code.as_str() {
            // The close 4000 follows and says it.
            "chat_thread_taken_over" => {}
            // A stop's cancel met a response that had just ended by itself:
            // nothing was left to stop.
            "response_cancel_not_active" => {}
            "chat_thread_admin" | "chat_thread_not_found" => self.rt.refused(e.message),
            "empty_turn" => self.nothing_said(),
            "superseded" | "not_saved" => self.rt.status.set(Note::new(
                TURN_NOTE,
                NoteKind::Info,
                format!("the reply stopped: {}", e.message),
            )),
            c => {
                // The open turn went with the detector: no response comes
                // for it (review m4).
                if c == "turn_detection_unavailable" {
                    self.machine.borrow_mut().turn_failed();
                }
                // An unbound session refuses a commit with no ASR as an
                // `error`; it is the turn's, as a failed transcription is.
                let key = if c == "asr_not_configured" {
                    TURN_NOTE
                } else {
                    TURN_KEY
                };
                self.rt
                    .status
                    .set(match turn_refusal(c, Surface::Voice, &e.message) {
                        Some((kind, text)) => Note::new(key, kind, text).linked(refusal_link(c)),
                        None => Note::new("error", NoteKind::Error, e.message.clone()),
                    });
            }
        }
    }

    /// A response that answers nothing said: `empty_turn`, or a heard
    /// push-to-talk turn the transcript found no words in (`response.done
    /// {cancelled, no_words}`, voice-audio-input §3.2) — the same note
    /// either way.
    pub(super) fn nothing_said(&self) {
        self.rt.status.set(Note::new(
            TURN_NOTE,
            NoteKind::Info,
            "nothing new was said: no reply".to_string(),
        ));
    }

    /// A turn's transcription failed (`….input_audio_transcription.failed`):
    /// no reply comes for it. A bound turn whose thread names no ASR says
    /// `asr_not_configured`; an ASR the hold refuses, `gpu_hold`.
    pub(super) fn heard_failed(&self, code: Option<&str>, message: &str) {
        self.machine.borrow_mut().turn_failed();
        let note = match turn_refusal(code.unwrap_or_default(), Surface::Voice, message) {
            // In place of the `asr` stage's `held` note: one chip.
            Some((NoteKind::Hold, text)) => Note::new("asr", NoteKind::Hold, text),
            Some((kind, text)) => Note::new(TURN_NOTE, kind, text),
            None => Note::new(
                TURN_NOTE,
                NoteKind::Warn,
                format!("what you said could not be transcribed: {message}"),
            ),
        };
        self.rt.status.set(note);
    }
}
