//! The dashboard's view of the change feed: the `chat` frame of
//! `/api/events` ([`lmgw_api_types::ChatChanged`]), one reader per open
//! `/api/events` stream.
//!
//! It reads what a device's feed reads, for the owner: the feed's table,
//! on the feed's wake ([`super::Feed::wake`]), from the head it found when
//! the stream opened. The owner sees every thread, so no record is left out
//! for its level (L3 is a device's rule), and the admin-level records are
//! skipped: they move nothing the owner sees. The threads whose messages
//! changed come from the message marks (`super::marks`), which the table
//! does not hold yet.
//!
//! A frame names ids; it renders nothing. The dashboard reads the list or
//! the thread again, so a frame is small however much changed, and what the
//! page shows is always what the Chat API answers.
//!
//! - **Coalesced**: the first change of a burst waits [`COALESCE`] (a fixed
//!   200 ms, not a setting) for the rest, then one read of the marks and the
//!   table names them all in one frame. A folder deleted with its threads,
//!   or a turn's user row and its title, is one frame. So a stream sends at
//!   most one frame per [`COALESCE`] plus a read, but for one the backstop
//!   finds, which goes out at once.
//! - **Nothing missed**: the wake and the marks are subscribed before the
//!   head is read, and the route sends its `resync` frame after both, so a
//!   change is either before what the client reads on that frame or named
//!   by a later one. Retention pruning records this reader had not read
//!   yet, or a head it could not read at the start, is a `resync`.
//! - **The backstop**: the table is read at every
//!   `chat_feed_keepalive_s` too, as a device's feed reads it, so a write
//!   that recorded and forgot to wake the feed is late by one interval at
//!   most.

use std::collections::BTreeSet;
use std::time::Duration;

use futures::Stream;
use lmgw_api_types::ChatChanged;
use tokio::sync::watch;

use super::marks::Marks;
use super::stream::FeedStream;
use crate::state::SharedState;
use crate::store::feed::{self as table, kind};

/// How long the first change of a burst waits for the rest before its
/// frame goes out: a latency floor, not a cap on what a frame names. Fixed;
/// the `ChatChanged` doc and the OpenAPI text state it as 200 ms.
pub(crate) const COALESCE: Duration = Duration::from_millis(200);

/// One `/api/events` stream's reader.
pub(crate) struct Changes {
    state: SharedState,
    stored: watch::Receiver<u64>,
    messages: watch::Receiver<Marks>,
    /// The message marks' generation read up to.
    seen: u64,
    /// The newest record read; `None` while the head could not be read.
    after: Option<i64>,
    /// `chat_feed_page_size` when the stream opened: what one read holds.
    page_size: u32,
    /// The table's read at every `chat_feed_keepalive_s`.
    backstop: tokio::time::Interval,
}

impl Changes {
    /// Subscribe to the wake and the marks, then read the table's head:
    /// every change committed after this moment is named by a frame.
    pub(crate) async fn open(state: &SharedState) -> Self {
        let mut stored = state.chat_feed.stored.subscribe();
        stored.borrow_and_update();
        let mut messages = state.chat_feed.messages.subscribe();
        let seen = messages.borrow_and_update().now();
        let after = match table::bounds(&state.db).await {
            Ok(b) => Some(b.head),
            Err(e) => {
                tracing::warn!(
                    "/api/events chat frame: reading the change feed's head failed: {e} — the \
                     first change after it reads as a resync"
                );
                None
            }
        };
        let snap = state.snapshot();
        Self {
            state: state.clone(),
            stored,
            messages,
            seen,
            after,
            page_size: snap.settings.chat_feed_page_size.max(1),
            backstop: FeedStream::keepalive(snap.settings.chat_feed_keepalive_s),
        }
    }

    /// The next frame; `None` once the gateway is going away.
    pub(crate) async fn next(&mut self) -> Option<ChatChanged> {
        loop {
            let backstop = tokio::select! {
                changed = self.stored.changed() => {
                    changed.ok()?;
                    false
                }
                changed = self.messages.changed() => {
                    changed.ok()?;
                    false
                }
                _ = self.backstop.tick() => true,
            };
            if !backstop {
                tokio::time::sleep(COALESCE).await;
            }
            let out = self.read().await;
            if !out.is_empty() {
                return Some(out);
            }
        }
    }

    /// Everything that changed since the last read.
    async fn read(&mut self) -> ChatChanged {
        self.stored.borrow_and_update();
        let (messages, seen) = {
            let marks = self.messages.borrow_and_update();
            marks.since(self.seen)
        };
        self.seen = seen;
        let mut out = ChatChanged {
            messages,
            ..ChatChanged::default()
        };
        self.read_table(&mut out).await;
        out
    }

    /// The records after the last one read, every page of them, as the
    /// threads and folders they name. A read that fails keeps where it was:
    /// the next wake or backstop tick reads the same records again.
    async fn read_table(&mut self, out: &mut ChatChanged) {
        let db = &self.state.db;
        let Some(mut after) = self.after else {
            match table::bounds(db).await {
                Ok(b) => {
                    self.after = Some(b.head);
                    out.resync = true;
                }
                Err(e) => tracing::warn!(
                    "/api/events chat frame: reading the change feed's head failed: {e}"
                ),
            }
            return;
        };
        let mut threads = BTreeSet::new();
        let mut folders = BTreeSet::new();
        loop {
            let (page, bounds) = match table::page_with_bounds(db, after, self.page_size).await {
                Ok(read) => read,
                Err(e) => {
                    tracing::warn!(
                        "/api/events chat frame: reading the change feed after {after} failed: {e}"
                    );
                    break;
                }
            };
            if bounds.pruned_through > after {
                // Retention took records this reader had not read: what
                // they named is not known.
                out.resync = true;
                after = bounds.head;
                break;
            }
            let full = page.len() >= self.page_size as usize;
            for r in &page {
                after = r.seq;
                match r.kind.as_str() {
                    kind::THREAD_CREATED | kind::THREAD_UPDATED | kind::THREAD_DELETED => {
                        threads.extend(r.thread_id);
                    }
                    kind::FOLDER_CREATED
                    | kind::FOLDER_UPDATED
                    | kind::FOLDER_DELETED
                    | kind::FOLDER_CURRENT => {
                        folders.extend(r.folder_id);
                    }
                    // A device's or the gateway's admin level: what a
                    // device reaches moved, nothing the owner sees.
                    _ => {}
                }
            }
            if !full {
                break;
            }
        }
        self.after = Some(after);
        out.threads = threads.into_iter().collect();
        out.folders = folders.into_iter().collect();
    }
}

/// The frames of `changes`, as they come.
pub(crate) fn frames(changes: Changes) -> impl Stream<Item = ChatChanged> {
    futures::stream::unfold(changes, |mut c| async move {
        let frame = c.next().await?;
        Some((frame, c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page is read, however many records a burst left: the page
    /// size bounds what one read holds, not what the frame names.
    #[tokio::test]
    async fn a_frame_names_every_record_across_pages() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let mut c = Changes::open(&state).await;
        c.page_size = 2;
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(
                crate::store::create_chat_thread(&state.db, "m", "chat")
                    .await
                    .unwrap(),
            );
        }
        state.chat_feed.messages_changed(ids[0]);
        // A temporary thread is never named (L7).
        state.chat_feed.messages_changed(-3);
        let out = c.read().await;
        assert_eq!(out.threads, ids);
        assert_eq!(out.messages, vec![ids[0]]);
        assert!(!out.resync);
        assert!(c.read().await.is_empty(), "nothing named twice");
    }

    /// Retention that prunes records this reader had not read is a resync,
    /// and it goes on from the newest.
    #[tokio::test]
    async fn a_prune_past_the_reader_is_a_resync() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let mut c = Changes::open(&state).await;
        for _ in 0..3 {
            crate::store::create_chat_thread(&state.db, "m", "chat")
                .await
                .unwrap();
        }
        sqlx::query("UPDATE chat_feed SET at = datetime('now', '-30 days')")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(table::prune(&state.db, 7).await.unwrap(), 3);
        let out = c.read().await;
        assert!(out.resync && out.threads.is_empty(), "{out:?}");
        let id = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        assert_eq!(c.read().await.threads, vec![id], "on from the newest");
    }

    /// A head that could not be read at the start: the first read after
    /// it is a resync from wherever the table is then.
    #[tokio::test]
    async fn a_start_without_the_head_reads_as_a_resync() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let mut c = Changes::open(&state).await;
        c.after = None;
        crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let out = c.read().await;
        assert!(out.resync && out.threads.is_empty(), "{out:?}");
        let id = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        assert_eq!(c.read().await.threads, vec![id]);
    }

    /// A frame waits for the rest of its burst, and the backstop finds a
    /// record whose writer forgot to wake the feed.
    #[tokio::test]
    async fn a_burst_is_one_frame_and_a_missed_wake_is_found() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let mut c = Changes::open(&state).await;
        // The keep-alive interval at its least, so the backstop comes soon.
        c.backstop = FeedStream::keepalive(1);
        let a = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        state.chat_feed.wake();
        let b = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        state.chat_feed.messages_changed(b);
        let out = c.next().await.unwrap();
        assert_eq!((out.threads, out.messages), (vec![a, b], vec![b]));
        // No wake: the keep-alive interval's read finds it.
        let d = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        assert_eq!(c.next().await.unwrap().threads, vec![d]);
    }
}
