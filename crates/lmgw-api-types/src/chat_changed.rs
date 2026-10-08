use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Something in the Chat changed: the `chat` frame of `/api/events`. The
/// frame names what changed and the dashboard reads it again; it carries no
/// rendering of its own.
///
/// It covers every writer: the dashboard (this window or another), a paired
/// device, an owner key, the self-admin tools and the gateway's own sweep.
/// `/api/events` is the owner's, so every thread is named, Admin Chat
/// included. Temporary chats are never named: they live in the memory of
/// the gateway, outside the change feed.
///
/// - **On connect** the stream sends one with `resync: true`: what a client
///   read before it connected may be stale, so it reads everything it shows
///   again. A reconnect is a connect, so frames missed while the stream was
///   down are covered by that one.
/// - **Bursts are coalesced**: the first change of a burst waits a fixed
///   200 ms (not a setting) for the rest, then the stream reads what changed
///   once and sends one frame naming everything changed up to that read. A
///   frame therefore comes 200 ms plus one read of the change feed after the
///   first change it names, and a stream sends at most one frame per 200 ms,
///   except one its keep-alive read finds (`chat_feed_keepalive_s`, a write
///   that did not wake the feed). An id is named once however often it
///   changed in a frame. A client that reads frames more slowly than they
///   come may merge them ([`Self::merge`]): a frame is a delta, never a
///   state.
// The client-apps design record, §3.6 (2026-10-08).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatChanged {
    /// The threads whose row changed, ascending: created (a rollover's new
    /// current thread too), renamed, settings, pinned, archived or
    /// restored, moved between folders, hidden from devices, or deleted.
    pub threads: Vec<i64>,
    /// The folders created, changed (name, defaults, ongoing, retention) or
    /// deleted, and those whose current thread moved, ascending.
    pub folders: Vec<i64>,
    /// The threads whose messages changed, ascending: a message added (a
    /// user turn, typed or spoken, as it is stored; a reply as it is saved),
    /// edited, continued, cut to what was heard, or deleted. A reply still
    /// being generated is not named until it is saved.
    pub messages: Vec<i64>,
    /// What changed is not known: read everything shown again. Sent on
    /// connect, and when the stream could not tell what changed.
    pub resync: bool,
}

impl ChatChanged {
    /// The frame that says "read everything again".
    pub fn resync() -> Self {
        Self {
            resync: true,
            ..Self::default()
        }
    }

    /// Nothing is named, and no resync.
    pub fn is_empty(&self) -> bool {
        !self.resync
            && self.threads.is_empty()
            && self.folders.is_empty()
            && self.messages.is_empty()
    }

    /// Whether thread `id` has to be read again: its row or its messages
    /// changed, or everything has to be.
    pub fn names_thread(&self, id: i64) -> bool {
        self.resync || self.threads.contains(&id) || self.messages.contains(&id)
    }

    /// `other` folded into this one: every id named by either, once,
    /// ascending; a resync when either is one.
    pub fn merge(&mut self, other: Self) {
        fn union(a: &mut Vec<i64>, b: Vec<i64>) {
            if b.is_empty() {
                return;
            }
            let all: BTreeSet<i64> = a.drain(..).chain(b).collect();
            *a = all.into_iter().collect();
        }
        union(&mut self.threads, other.threads);
        union(&mut self.folders, other.folders);
        union(&mut self.messages, other.messages);
        self.resync |= other.resync;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_merge_into_one_naming_each_id_once() {
        let mut a = ChatChanged {
            threads: vec![3, 7],
            messages: vec![7],
            ..Default::default()
        };
        a.merge(ChatChanged {
            threads: vec![1, 7],
            folders: vec![2],
            messages: vec![9],
            resync: false,
        });
        assert_eq!(a.threads, vec![1, 3, 7]);
        assert_eq!(a.folders, vec![2]);
        assert_eq!(a.messages, vec![7, 9]);
        assert!(!a.resync);
        a.merge(ChatChanged::resync());
        assert!(a.resync);
        assert_eq!(a.threads, vec![1, 3, 7], "a resync keeps what was named");
    }

    #[test]
    fn a_thread_is_named_by_its_row_its_messages_or_a_resync() {
        let c = ChatChanged {
            threads: vec![1],
            messages: vec![2],
            ..Default::default()
        };
        assert!(c.names_thread(1) && c.names_thread(2) && !c.names_thread(3));
        assert!(ChatChanged::resync().names_thread(3));
        assert!(ChatChanged::default().is_empty());
        assert!(!ChatChanged::resync().is_empty());
    }
}
