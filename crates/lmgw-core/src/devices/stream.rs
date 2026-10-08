//! A stream that ends when its key is revoked (client-apps design §1.6,
//! review W2-2): what a request that is not a connection of its own — a
//! Chat turn's SSE, a read-aloud, a warm, `/mcp`'s notification stream —
//! watches while it runs, so a Disable, Rotate, Delete or expiry ends it now
//! rather than at its next model call.

use futures::stream::{BoxStream, StreamExt};
use futures::Stream;

use super::{Revocation, Watched};

/// `events` until `watch` resolves; then `last(revocation)` — the sentence
/// `RevokeReason::message` gives and the kind (`Watched::said`), when the
/// stream has a frame to say them in — and the end. Dropping the stream then drops what produced `events`: a
/// Chat turn hears its reader go and stops at its next await, saving what it
/// had. `None` watches nothing: `events` as they are.
pub fn until_revoked<T, S>(
    events: S,
    watch: Option<Watched>,
    last: fn(Revocation) -> Option<T>,
) -> BoxStream<'static, T>
where
    T: Send + 'static,
    S: Stream<Item = T> + Send + 'static,
{
    let Some(mut watch) = watch else {
        return events.boxed();
    };
    let revoked: futures::future::BoxFuture<'static, Revocation> = Box::pin(async move {
        let reason = watch.revoked().await;
        let said = watch.said(reason);
        tracing::info!("{}", said.message);
        said
    });
    futures::stream::unfold(
        (events.boxed(), Some(revoked)),
        move |(mut events, revoked)| async move {
            let mut revoked = revoked?;
            tokio::select! {
                biased;
                why = &mut revoked => last(why).map(|frame| (frame, (events, None))),
                item = events.next() => item.map(|item| (item, (events, Some(revoked)))),
            }
        },
    )
    .boxed()
}
