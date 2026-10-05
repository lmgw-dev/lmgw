//! A temporary thread's spoken-reply writes (chat-voice design §8.3), as
//! `store::set_chat_message_voice` and `store::cut_chat_reply` make them
//! for a stored one: not activity, so no `updated_at` bump.

use super::TempChats;
use crate::store::MessageVoice;

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

    /// Cut reply `id` to `content`, its `voice` with it; `false` when it is
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
            true
        })
        .unwrap_or(false)
    }
}
