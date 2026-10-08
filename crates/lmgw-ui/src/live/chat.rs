//! The `chat` frames of `/api/events`, kept until the Chat page takes them.
//!
//! A frame names what any writer changed (this window too); the page reads
//! those again. A signal holds only its last value, and the bridge can set
//! it twice before an effect runs (two frames already buffered in the
//! EventSource), so the frames are merged into one pending change instead
//! ([`ChatChanged::merge`]): nothing named is lost however the effects are
//! scheduled. The pending change is bounded by the ids it names, never a
//! backlog of frames, and the page takes it whole.

use leptos::prelude::*;
use lmgw_api_types::ChatChanged;

/// The read end, on [`super::LiveBus`]. `Copy`.
#[derive(Clone, Copy)]
pub struct ChatInbox {
    arrived: ReadSignal<u64>,
    pending: StoredValue<ChatChanged>,
}

/// The write end, the bridge's.
#[derive(Clone, Copy)]
pub(super) struct ChatPost {
    arrived: WriteSignal<u64>,
    pending: StoredValue<ChatChanged>,
}

/// A fresh inbox and its write end.
pub(super) fn inbox() -> (ChatInbox, ChatPost) {
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
    /// How many frames have arrived (tracked): an effect reading it runs
    /// again after each, or after several at once.
    pub fn arrived(&self) -> u64 {
        self.arrived.get()
    }

    /// Every change the frames named since the last take, merged; empty
    /// when there was none.
    pub fn take(&self) -> ChatChanged {
        self.pending
            .try_update_value(std::mem::take)
            .unwrap_or_default()
    }
}

impl ChatPost {
    /// One frame arrived.
    pub(super) fn post(&self, c: ChatChanged) {
        self.pending.update_value(|p| p.merge(c));
        self.arrived.update(|n| *n += 1);
    }
}
