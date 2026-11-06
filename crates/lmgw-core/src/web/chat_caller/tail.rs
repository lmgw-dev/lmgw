//! A Chat SSE stream's tail flush (chat-media-tools design §14 item 54).
//!
//! WebKitGTK (the app window's engine; measured on 2.52) hands a page the tail of a
//! frame that straddles one of its reads only once more bytes arrive: a tool
//! call's 285-byte `ready` frame reached the page 2.6 s late, with the call's
//! result, while a cloud picture was drawn, so the call's Cancel had no id to
//! name until then. Chrome reads every frame whole at once. One comment (`:`)
//! a quarter second after the last frame of a burst pushes such a tail
//! through; a stream that stays quiet longer gets nothing more from here, and
//! axum's keep-alive (15 s) keeps it open as it keeps any SSE stream.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::Event as SseEvent;
use futures::stream::{BoxStream, StreamExt};

/// How long after a burst's last frame the flush goes out: the rate at which
/// a person reads a stopped clock as instant, the one cancels are polled at
/// ([`crate::agent::CANCEL_POLL`]).
pub(crate) const TAIL_FLUSH: Duration = crate::agent::CANCEL_POLL;

/// `events` with one `: ` comment [`TAIL_FLUSH`] after each burst of frames:
/// after a frame, the next one is awaited that long; if none came, the
/// comment goes out and the stream waits for the next frame without a
/// clock. A frame that comes in time is passed on and owes the flush in its
/// place, so a steady stream (tokens) sends none.
pub(crate) fn tail_flushed(
    events: BoxStream<'static, Result<SseEvent, Infallible>>,
) -> BoxStream<'static, Result<SseEvent, Infallible>> {
    futures::stream::unfold((events, false), |(mut events, owed)| async move {
        if !owed {
            let ev = events.next().await?;
            return Some((ev, (events, true)));
        }
        match tokio::time::timeout(TAIL_FLUSH, events.next()).await {
            Ok(Some(ev)) => Some((ev, (events, true))),
            Ok(None) => None,
            Err(_) => Some((Ok(SseEvent::default().comment("")), (events, false))),
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    /// The body a stream of `data` frames makes, read as it is written.
    async fn body_of(rx: mpsc::Receiver<&'static str>) -> String {
        let events = ReceiverStream::new(rx)
            .map(|d| Ok::<_, Infallible>(SseEvent::default().data(d)))
            .boxed();
        let resp = axum::response::sse::Sse::new(tail_flushed(events)).into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn one_comment_follows_each_burst_and_none_a_steady_stream() {
        let (tx, rx) = mpsc::channel(8);
        let feed = tokio::spawn(async move {
            // A burst: three frames closer than the flush, then quiet for
            // ten flushes.
            for d in ["a", "b", "c"] {
                tx.send(d).await.unwrap();
                tokio::time::sleep(TAIL_FLUSH / 2).await;
            }
            tokio::time::sleep(TAIL_FLUSH * 10).await;
            // A lone frame, quiet again, then the end.
            tx.send("d").await.unwrap();
            tokio::time::sleep(TAIL_FLUSH * 3).await;
        });
        let body = body_of(rx).await;
        feed.await.unwrap();
        assert_eq!(
            body, "data: a\n\ndata: b\n\ndata: c\n\n: \n\ndata: d\n\n: \n\n",
            "one flush per quiet spell, never a second in the same one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ends_at_once_sends_no_flush() {
        let (tx, rx) = mpsc::channel(8);
        tx.send("a").await.unwrap();
        drop(tx);
        assert_eq!(body_of(rx).await, "data: a\n\n");
    }
}
