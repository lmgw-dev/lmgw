//! What the Chat's live updates named, kept until the Chat takes it: the
//! frames of a stream of its host's own (the dashboard shell's `chat` frames,
//! [`crate::ChatLive::Host`]), or the events of the Chat's own feed where the
//! host hands it no such stream (`chat_live`, chat-plane design D1). A host
//! with a stream makes the inbox with [`inbox`], keeps the [`ChatPost`] and
//! hands the [`ChatInbox`] to the Chat.
//!
//! A frame or an event names what any writer changed (this window too); the
//! page reads those again. A signal holds only its last value, and the
//! stream can post twice before an effect runs (two frames already buffered
//! in the EventSource), so they are merged into one pending change instead
//! ([`ChatChanged::merge`]): nothing named is lost however the effects are
//! scheduled. The pending change is bounded by the ids it names, never a
//! backlog, and the page takes it whole.

use leptos::prelude::*;
use lmgw_api_types::ChatChanged;

/// The read end. `Copy`.
#[derive(Clone, Copy)]
pub struct ChatInbox {
    arrived: ReadSignal<u64>,
    pending: StoredValue<ChatChanged>,
}

/// The write end, the stream's.
#[derive(Clone, Copy)]
pub struct ChatPost {
    arrived: WriteSignal<u64>,
    pending: StoredValue<ChatChanged>,
}

/// A fresh inbox and its write end.
pub fn inbox() -> (ChatInbox, ChatPost) {
    let (arrived, set_arrived) = signal(0u64);
    let pending = StoredValue::new(ChatChanged::default());
    (
        ChatInbox { arrived, pending },
        ChatPost {
            arrived: set_arrived,
            pending,
        },
    )
}

impl ChatInbox {
    /// How many changes have arrived (tracked): an effect reading it runs
    /// again after each, or after several at once.
    pub fn arrived(&self) -> u64 {
        self.arrived.get()
    }

    /// Every change named since the last take, merged; empty when there was
    /// none.
    pub fn take(&self) -> ChatChanged {
        self.pending
            .try_update_value(std::mem::take)
            .unwrap_or_default()
    }
}

impl ChatPost {
    /// One change arrived. A no-op once its owner is gone.
    pub fn post(&self, c: ChatChanged) {
        if self.pending.try_update_value(|p| p.merge(c)).is_some() {
            self.arrived.try_update(|n| *n += 1);
        }
    }
}
