//! The web-source fetches in flight gateway-wide (document-conversion
//! design ruling 9, §7.4.1, §9): a pool of their own, of the tool pool's
//! type and size (D19: derived from the cores and memory, or "Parallel tool
//! runs" set by hand), separate from it, so slow downloads never hold a
//! slot a conversion needs.
//!
//! - Each source's fetch takes one slot for its whole walk (redirects and
//!   body); the request's sources are fetched one after the other, so one
//!   request holds at most one slot.
//! - Waiting goes round robin over the callers (a key, or each keyless
//!   request on its own), as the tool pool's does.
//! - A fetch waits for its slot before `SOURCE_FETCH_TIMEOUT` starts, and
//!   the wait ends when the request goes away. Meanwhile the caller sees its
//!   request still open: the sync answer, and an async route's task answer
//!   (which waits for its downloads), come once the fetches have run.
//! - The most fetched bytes held at once is the pool's size times
//!   `max_body_mb`, shown with the size and the counts under Settings ›
//!   Documents (expert view, [`facts`]).

use std::sync::OnceLock;

use lmgw_api_types::documents::WebFetchPool;

use crate::extract::convert::pool::{self, ToolPool};

/// The pool every web-source fetch shares.
pub fn global() -> &'static ToolPool {
    static POOL: OnceLock<ToolPool> = OnceLock::new();
    POOL.get_or_init(|| pool::global().twin())
}

/// Its size and what runs and waits now, with the bytes one fetch may hold
/// (`max_body_mb`; 0 while that is 0, when web sources are refused).
pub fn facts(max_body_mb: u32) -> WebFetchPool {
    let f = global().facts();
    WebFetchPool {
        slots: f.slots,
        running: f.running,
        waiting: f.waiting,
        per_fetch_bytes: u64::from(max_body_mb) << 20,
    }
}
