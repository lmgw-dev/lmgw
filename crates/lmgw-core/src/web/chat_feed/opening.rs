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
//! keep-alive (Settings → Chat → Change feed → Keep-alive, the gateway's
//! own, whatever the stream asked: device link design §7.1). Still apart
//! then, the log says so, and the feed opens at the store's levels: the
//! store is what the feed follows. A read that fails meanwhile is said in
//! the log and ends the response; the client reconnects. The gateway going
//! away ends it without a word.
//!
//! **Comments during the wait come at the stream's own interval** (review
//! of WP5, N2): a client that asked for a short keep-alive takes two of
//! them without a byte as a dead link, and the wait may last the gateway's
//! longer one.

use axum::response::sse::Event as SseEvent;
use futures::stream::BoxStream;
use futures::StreamExt;

use super::super::chat_caller::Caller;
use super::stream::FeedStream;
use super::{key_of, open, Asked, Read};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// One client's feed as the frames it sends, keeping a comment every
/// `keepalive_s` seconds: opened now, or after the wait (module doc). An
/// `Err` is a read that failed before the response started: the route's
/// 500.
pub(super) async fn frames(
    state: &SharedState,
    caller: &Caller,
    ctx: &RequestCtx,
    asked: Asked,
) -> Result<BoxStream<'static, SseEvent>, crate::error::GatewayError> {
    let key_id = key_of(caller);
    let read = Read::now(state, key_id).await?;
    match key_id {
        Some(id) if !read.published(state, id) => Ok(Waiting::start(
            state.clone(),
            caller.clone(),
            ctx.clone(),
            asked,
            read,
            id,
        )),
        _ => Ok(opened(open(state, caller, ctx, asked, read).await?)),
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
    /// Where the stream resumes, and the keep-alive it keeps.
    asked: Asked,
    read: Read,
    key_id: i64,
    /// When it started.
    since: tokio::time::Instant,
    /// One of the gateway's keep-alives, in seconds: how long it waits at
    /// most.
    keepalive: u32,
    /// The comments meanwhile, at the stream's own interval (module doc).
    comments: tokio::time::Interval,
}

/// How a step of the wait ended.
enum Waited {
    /// The stream's keep-alive came round: a comment, and the wait goes on.
    Comment,
    /// The levels agree, or the wait ran out: the stream opens.
    Open,
    /// A read failed, or the gateway is going away: the response ends.
    End,
}

/// A keep-alive comment.
fn comment() -> SseEvent {
    SseEvent::default().comment("keep-alive")
}

impl Waiting {
    /// The response: a keep-alive comment at once, then the wait, then the
    /// stream opened at the levels it came to.
    fn start(
        state: SharedState,
        caller: Caller,
        ctx: RequestCtx,
        asked: Asked,
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
            comments: FeedStream::keepalive(asked.keepalive_s),
            state,
            caller,
            ctx,
            asked,
            read,
            key_id,
            since: tokio::time::Instant::now(),
            keepalive,
        };
        // A comment at once, one at each of the stream's keep-alives while
        // it waits, then the stream.
        let steps = futures::stream::unfold(Some(waiting), |w| async move {
            let mut w = w?;
            match w.wait().await {
                Waited::Comment => {
                    let one = futures::stream::once(async { comment() }).boxed();
                    Some((one, Some(w)))
                }
                Waited::Open => Some((w.opened().await, None)),
                Waited::End => None,
            }
        });
        futures::stream::once(async { comment() })
            .chain(steps.flatten())
            .boxed()
    }

    /// The stream once the wait is over; none when it could not open.
    async fn opened(self) -> BoxStream<'static, SseEvent> {
        let Self {
            state,
            caller,
            ctx,
            asked,
            read,
            key_id,
            ..
        } = self;
        match open(&state, &caller, &ctx, asked, read).await {
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
    /// read with the head, for up to one of the gateway's keep-alives
    /// (module doc), or for the stream's own keep-alive to come round
    /// first: the feed opens, at the levels `read` holds then — the
    /// snapshot's, or the store's once the wait ran out — or a comment goes
    /// out and the wait goes on, or the response ends (a read failed, said
    /// in the log, or the gateway is going away).
    async fn wait(&mut self) -> Waited {
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
                        return Waited::End;
                    }
                    true
                }
                _ = self.comments.tick() => return Waited::Comment,
            };
            if let Err(e) = self.read.again(&self.state, key_id).await {
                tracing::warn!(
                    "chat feed: reading device key {key_id}'s levels in its feed's wait failed: \
                     {e}; the response ends, and the client reconnects"
                );
                return Waited::End;
            }
            if self.read.published(&self.state, key_id) {
                tracing::debug!(
                    "chat feed: device key {key_id}'s feed opens at the published levels after \
                     {} ms",
                    self.since.elapsed().as_millis()
                );
                return Waited::Open;
            }
            if !woken {
                tracing::warn!(
                    "chat feed: device key {key_id}'s admin-tools levels in the store and in the \
                     published snapshot still differed after one keep-alive ({} s, Settings → \
                     Chat → Change feed → Keep-alive); the feed opens at the store's",
                    self.keepalive
                );
                return Waited::Open;
            }
        }
    }
}
