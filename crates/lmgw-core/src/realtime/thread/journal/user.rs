//! The journal's user entry (chat-voice design §8.3): one user message for
//! a response's turns that have none yet, written as a send writes one.

use serde_json::Value;

use super::super::super::protocol::{ErrorObject, ServerEvent};
use super::super::super::transcribe::Facts;
use super::{Task, UserTurn};
use crate::store::{MessageVoice, VIA_REALTIME};
use crate::web::chat_voice::bound;

impl Task {
    /// The user entry (module doc).
    ///
    /// A write the store refused (WP8 review m11) is
    /// `chat_history_write_failed`, not a thread that went: its turns are
    /// kept and lead the next user entry — this session's next response, or
    /// the turns written as it ends — and the drain's end logs any still
    /// unwritten, so the words are never lost silently.
    pub(super) async fn user(&mut self, turns: Vec<UserTurn>) -> Result<Option<i64>, ErrorObject> {
        let new: Vec<UserTurn> = std::mem::take(&mut self.unwritten)
            .into_iter()
            .chain(turns)
            .filter(|t| !self.written.contains_key(&t.item_id))
            .collect();
        let spoken: Vec<&UserTurn> = new.iter().filter(|t| !t.text.trim().is_empty()).collect();
        if spoken.is_empty() {
            return Ok(None);
        }
        let device = self.caller.is_device();
        let thread = match bound::thread_checked(&self.state, self.thread_id).await {
            Ok(Some(thread)) => thread,
            Ok(None) => return Err(self.dropped(GONE, device)),
            Err(why) => return Err(self.not_written(new, &why)),
        };
        // A device's thread out of its reach is not there for it, in the
        // words a deleted one gets (review W4-18): its session is being
        // closed, and the turn is not written.
        if !self.caller.sees(&self.state.snapshot(), &thread) {
            return Err(self.dropped(OUT_OF_REACH, true));
        }
        let content = spoken
            .iter()
            .map(|t| t.text.trim())
            .collect::<Vec<_>>()
            .join("\n");
        let voice = user_voice(&spoken);
        let id = match bound::write_user(&self.state, &thread, &content, &voice, &self.caller).await
        {
            Ok(id) => id,
            // The write checks a device's reach again under the thread's
            // lock: a thread deleted or hidden from it since the read above
            // is not there, as above, rather than a write the store refused.
            Err(why) => match self.missing().await {
                Some(cause) if device => return Err(self.dropped(cause, true)),
                _ => return Err(self.not_written(new, &why)),
            },
        };
        for t in &new {
            self.written.insert(t.item_id.clone(), id);
        }
        // The write moved the thread's generation, so no reply finalized
        // before it can be re-cut any more (§8.3): its text is let go, and
        // a late truncate of it is said skipped (WP8 review NIT 7).
        for done in self.done.values_mut() {
            done.original = String::new();
            done.record = None;
            done.moved = true;
        }
        self.event(ServerEvent::LmgwChatUser {
            message_id: id,
            content,
            voice: serde_json::to_value(&voice).unwrap_or(Value::Null),
            response_id: None,
        });
        Ok(Some(id))
    }

    /// Why the thread is not there for the session's binder now — gone, or
    /// out of a device's reach — or `None` when it is. A read that fails
    /// says `None`: the write's own error is then said.
    async fn missing(&self) -> Option<&'static str> {
        match bound::thread_checked(&self.state, self.thread_id).await {
            Ok(Some(t)) if self.caller.sees(&self.state.snapshot(), &t) => None,
            Ok(Some(_)) => Some(OUT_OF_REACH),
            Ok(None) => Some(GONE),
            Err(_) => None,
        }
    }

    /// The user entry is not written: the thread is not there for the
    /// binder (`cause`, for the log). The log names the cause — it is the
    /// owner's — and the binder is told [`not_there`]'s words, which for a
    /// device are the same either way (review W4-18).
    fn dropped(&self, cause: &str, device: bool) -> ErrorObject {
        tracing::warn!(
            "{}: chat thread {} {cause}; the spoken turn is not written",
            self.label,
            self.thread_id
        );
        not_there(self.thread_id, device)
    }

    /// The user entry of `turns` could not be written (`why`): kept for
    /// the next one, and the response's error.
    pub(super) fn not_written(&mut self, turns: Vec<UserTurn>, why: &str) -> ErrorObject {
        tracing::warn!(
            "{}: the spoken turn was not written to chat thread {}: {why}; its words lead the \
             next user message",
            self.label,
            self.thread_id
        );
        self.unwritten = turns;
        ErrorObject {
            kind: "server_error".into(),
            ..ErrorObject::invalid(
                "chat_history_write_failed",
                format!(
                    "the spoken turn could not be written to chat thread {}: {why}",
                    self.thread_id
                ),
            )
        }
    }
}

/// How a response's turns were spoken (§3): the last turn's ASR alias, the
/// first fallback that answered, the times summed — unknown once any part's
/// is.
pub(super) fn user_voice(turns: &[&UserTurn]) -> MessageVoice {
    let facts: Vec<Option<&Facts>> = turns.iter().map(|t| t.asr.as_ref()).collect();
    let sum = |f: fn(&Facts) -> u64| -> Option<u64> {
        facts
            .iter()
            .map(|x| x.map(f))
            .try_fold(0u64, |acc, x| x.map(|v| acc + v))
    };
    MessageVoice {
        via: VIA_REALTIME.into(),
        asr: facts.iter().rev().find_map(|f| f.map(|f| f.alias.clone())),
        asr_answered_by: facts
            .iter()
            .find_map(|f| f.and_then(|f| f.answered_by.clone())),
        asr_ms: sum(|f| f.ms),
        audio_ms: sum(|f| f.audio_ms),
        ..Default::default()
    }
}

/// Causes [`Task::dropped`] logs, as `row` logs them.
const GONE: &str = "is gone";
const OUT_OF_REACH: &str = "is out of this device's reach now";

/// The thread is not there for the binder (`thread::not_there`): deleted,
/// or out of a device's reach.
fn not_there(id: i64, device: bool) -> ErrorObject {
    ErrorObject::invalid(
        "chat_thread_not_found",
        crate::realtime::thread::not_there(id, device),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_s_turns_add_up_in_its_user_message_s_voice() {
        let facts = |alias: &str, by: Option<&str>, ms: u64| Facts {
            alias: alias.into(),
            answered_by: by.map(str::to_string),
            ms,
            audio_ms: ms * 10,
            cold: false,
        };
        let turns = [
            UserTurn {
                item_id: "a".into(),
                text: "Eins".into(),
                asr: Some(facts("hear", Some("cloud"), 30)),
                error: None,
                heard: false,
            },
            UserTurn {
                item_id: "b".into(),
                text: "Zwei".into(),
                asr: Some(facts("other", None, 20)),
                error: None,
                heard: false,
            },
        ];
        let v = user_voice(&turns.iter().collect::<Vec<_>>());
        assert_eq!(v.via, VIA_REALTIME);
        assert_eq!(v.asr.as_deref(), Some("other"), "the last turn's alias");
        assert_eq!(v.asr_answered_by.as_deref(), Some("cloud"));
        assert_eq!((v.asr_ms, v.audio_ms), (Some(50), Some(500)));
        let unknown = UserTurn {
            item_id: "c".into(),
            text: "Drei".into(),
            asr: None,
            error: None,
            heard: false,
        };
        let v = user_voice(&[&turns[0], &unknown]);
        assert_eq!((v.asr_ms, v.audio_ms), (None, None), "unknown once any is");
    }
}
