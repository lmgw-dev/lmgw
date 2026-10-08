//! How a feed opens (client-apps design §2.1): at once, or, for a device
//! whose levels the published snapshot does not say yet, after a wait that
//! runs inside the response.
//!
//! **A device's feed opens at the levels the routes have** (the desktop
//! client's live check, 2026-10-07). A level's write commits its feed
//! record and the key's row, then publishes the snapshot every route a
//! device reads decides with. A feed opened in between would say in `hello`
//! a level those routes do not have yet. So it reads its levels again at
//! each wake of the table until the published snapshot says the same; the
//! publish wakes the table (`AppState::publish_over`).
//!
//! **The wait is inside the response** (the branch review's N-1). It used
//! to hold the request itself: a client saw no byte, no status even, until
//! the levels agreed, and a client that gave up first cancelled the wait
//! before anything said why. Now the response starts at once with a
//! keep-alive comment, the wait is said in the log as it starts, and
//! `hello` follows once the snapshot says the levels — for up to one
//! keep-alive (Settings → Chat → Change feed → Keep-alive), the longest a
//! stream is silent anyway. Still apart then, the log says so, and the feed
//! opens at the store's levels: the store is what the feed follows. A
//! read that fails meanwhile is said in the log and ends the response; the
//! client reconnects. The gateway going away ends it without a word.

use axum::response::sse::Event as SseEvent;
use futures::stream::BoxStream;
use futures::StreamExt;
use lmgw_api_types::chat_feed::Cursor;

use super::super::chat_caller::Caller;
use super::stream::FeedStream;
use super::{key_of, open, Read};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// One client's feed as the frames it sends: opened now, or after the wait
/// (module doc). An `Err` is a read that failed before the response
/// started: the route's 500.
pub(super) async fn frames(
    state: &SharedState,
    caller: &Caller,
    ctx: &RequestCtx,
    cursor: Option<Cursor>,
) -> Result<BoxStream<'static, SseEvent>, crate::error::GatewayError> {
    let key_id = key_of(caller);
    let read = Read::now(state, key_id).await?;
    match key_id {
        Some(id) if !read.published(state, id) => Ok(Waiting::start(
            state.clone(),
            caller.clone(),
            ctx.clone(),
            cursor,
            read,
            id,
        )),
        _ => Ok(opened(open(state, caller, ctx, cursor, read).await?)),
    }
}

/// An open stream's frames.
fn opened(s: FeedStream) -> BoxStream<'static, SseEvent> {
    futures::stream::unfold(s, |mut s| async move { s.next().await.map(|f| (f, s)) }).boxed()
}

/// A device's feed waiting for the published snapshot to say its levels
/// (module doc).
struct Waiting {
    state: SharedState,
    caller: Caller,
    ctx: RequestCtx,
    cursor: Option<Cursor>,
    read: Read,
    key_id: i64,
    /// When it started.
    since: tokio::time::Instant,
    /// One keep-alive, in seconds: how long it waits at most.
    keepalive: u32,
}

impl Waiting {
    /// The response: a keep-alive comment at once, then the wait, then the
    /// stream opened at the levels it came to.
    fn start(
        state: SharedState,
        caller: Caller,
        ctx: RequestCtx,
        cursor: Option<Cursor>,
        read: Read,
        key_id: i64,
    ) -> BoxStream<'static, SseEvent> {
        let keepalive = state.snapshot().settings.chat_feed_keepalive_s.max(1);
        tracing::info!(
            "chat feed: device key {key_id}'s admin-tools levels in the store are not the \
             published snapshot's yet; its feed waits for the publish behind a keep-alive \
             comment, for up to one keep-alive ({keepalive} s, Settings → Chat → Change feed → \
             Keep-alive)"
        );
        let waiting = Self {
            state,
            caller,
            ctx,
            cursor,
            read,
            key_id,
            since: tokio::time::Instant::now(),
            keepalive,
        };
        let first = SseEvent::default().comment("keep-alive");
        futures::stream::once(async move { first })
            .chain(futures::stream::once(waiting.opened()).flatten())
            .boxed()
    }

    /// The stream once the wait is over; none when it ended the response.
    async fn opened(mut self) -> BoxStream<'static, SseEvent> {
        if !self.wait().await {
            return futures::stream::empty().boxed();
        }
        let Self {
            state,
            caller,
            ctx,
            cursor,
            read,
            key_id,
            ..
        } = self;
        match open(&state, &caller, &ctx, cursor, read).await {
            Ok(s) => opened(s),
            Err(e) => {
                tracing::warn!(
                    "chat feed: device key {key_id}'s feed could not open after its wait: {e}; \
                     the response ends, and the client reconnects"
                );
                futures::stream::empty().boxed()
            }
        }
    }

    /// Wait for a wake after which the published snapshot says the levels
    /// read with the head, for up to one keep-alive (module doc): whether
    /// the feed opens, at the levels `read` holds then — the snapshot's, or
    /// the store's once the wait ran out. `false`: a read failed, said in
    /// the log, or the gateway is going away; the response ends.
    async fn wait(&mut self) -> bool {
        let key_id = self.key_id;
        let deadline = self.since + std::time::Duration::from_secs(self.keepalive.into());
        loop {
            let woken = tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => false,
                changed = self.read.stored.changed() => {
                    if changed.is_err() {
                        tracing::debug!(
                            "chat feed: device key {key_id}'s feed ends in its wait: the \
                             gateway is going away"
                        );
                        return false;
                    }
                    true
                }
            };
            if let Err(e) = self.read.again(&self.state, key_id).await {
                tracing::warn!(
                    "chat feed: reading device key {key_id}'s levels in its feed's wait failed: \
                     {e}; the response ends, and the client reconnects"
                );
                return false;
            }
            if self.read.published(&self.state, key_id) {
                tracing::debug!(
                    "chat feed: device key {key_id}'s feed opens at the published levels after \
                     {} ms",
                    self.since.elapsed().as_millis()
                );
                return true;
            }
            if !woken {
                tracing::warn!(
                    "chat feed: device key {key_id}'s admin-tools levels in the store and in the \
                     published snapshot still differed after one keep-alive ({} s, Settings → \
                     Chat → Change feed → Keep-alive); the feed opens at the store's",
                    self.keepalive
                );
                return true;
            }
        }
    }
}
