//! A temporary thread's spoken-reply writes (chat-voice design §8.3), as
//! `store::set_chat_message_voice` and `store::cut_chat_reply` make them
//! for a stored one: not activity, so no `updated_at` bump.

use super::TempChats;
use crate::store::{MessageVoice, ReplySeal};

impl TempChats {
    /// Set message `id`'s `voice` alone; `false` when it is not in the
    /// thread.
    pub fn set_message_voice(&self, thread_id: i64, id: i64, voice: &MessageVoice) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(row) = t.messages.iter_mut().find(|r| r.id == id) else {
                return false;
            };
            row.voice = Some(voice.clone());
            true
        })
        .unwrap_or(false)
    }

    /// Cut reply `id` to `content`, its `voice` with it and its reasoning
    /// seals to the heard ones (`ReplySeal::heard`); `false` when it is
    /// not a reply of the thread.
    pub fn cut_reply(&self, thread_id: i64, id: i64, content: &str, voice: &MessageVoice) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(row) = t
                .messages
                .iter_mut()
                .find(|r| r.id == id && r.role == "assistant")
            else {
                return false;
            };
            row.content = content.to_string();
            row.voice = Some(voice.clone());
            row.reasoning_seals =
                ReplySeal::heard(std::mem::take(&mut row.reasoning_seals), content.len());
            true
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Seal, SealScope};
    use crate::store::ChatReply;

    /// A cut reply keeps the seals of the items answered within what was
    /// heard and loses those past the cut, as a stored one does (WP8
    /// verify V-F2).
    #[test]
    fn a_cut_reply_keeps_only_the_heard_seals() {
        let chats = TempChats::default();
        let t = chats.create("m", "chat", "");
        let seal = |id: &str| Seal::openai(&SealScope::new(1, "cfg"), id, "enc");
        let reply = ChatReply {
            content: "It is late. Very late.".into(),
            reasoning: "Hm.".into(),
            reasoning_seals: vec![
                ReplySeal::new(0, seal("rs_1")),
                ReplySeal::new(11, seal("rs_2")),
                ReplySeal::new(12, seal("rs_3")),
            ],
            ..Default::default()
        };
        let id = chats.append_reply(t.id, &reply).unwrap();
        assert!(chats.cut_reply(t.id, id, "It is late.", &MessageVoice::default()));
        let row = chats.message(t.id, id).unwrap();
        assert_eq!(row.content, "It is late.");
        assert_eq!(row.reasoning, "Hm.", "the trace stays");
        assert_eq!(row.reasoning_seals, reply.reasoning_seals[..2]);
    }
}
