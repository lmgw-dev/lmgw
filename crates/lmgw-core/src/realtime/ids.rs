//! Id minting for one session (realtime design §7.1): `event_…`, `item_…`,
//! `resp_…`, `call_…` and `sess_…`.
//!
//! **Session-unique by construction**, not by luck: every id is the session's
//! random tag plus one counter shared by every kind, so no two ids a session
//! hands out are ever equal, whatever their prefix. The tag keeps ids of
//! different sessions apart too, which matters for `call_id`:
//! `@openai/agents` refuses one reused across invocations (§2.3), and some
//! local models number their calls per request (§7.4).
//!
//! Shared between the session core and the writer (which mints every
//! `event_id`), hence the atomic.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct Ids {
    tag: String,
    next: AtomicU64,
}

impl Default for Ids {
    fn default() -> Self {
        Self::new()
    }
}

impl Ids {
    pub fn new() -> Self {
        // 48 random bits, as 12 hex digits: plenty to keep sessions apart in
        // one gateway's logs, short enough to read in them.
        let tag: u64 = rand::random::<u64>() & 0xffff_ffff_ffff;
        Self {
            tag: format!("{tag:012x}"),
            next: AtomicU64::new(1),
        }
    }

    fn mint(&self, prefix: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{}{n:x}", self.tag)
    }

    /// A server event's own id (§2.3: every server event carries one).
    pub fn event(&self) -> String {
        self.mint("event")
    }

    pub fn item(&self) -> String {
        self.mint("item")
    }

    pub fn response(&self) -> String {
        self.mint("resp")
    }

    pub fn call(&self) -> String {
        self.mint("call")
    }

    pub fn session(&self) -> String {
        self.mint("sess")
    }
}
