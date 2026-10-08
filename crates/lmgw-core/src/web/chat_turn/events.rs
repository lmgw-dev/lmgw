//! Where a turn's frames go ([`Events`]): its caller's channel, with the
//! change feed reading along (client-apps design §2.2). The code of the
//! last `error` frame that carried one becomes the turn's `turn.done`
//! `code` — whichever path of the plain stream or the tool loop said it.

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};

use super::TurnFrame;
use crate::web::chat_feed::TurnObserver;

/// A turn's frame channel ([`super::out`]), observed by the feed.
#[derive(Clone)]
pub(crate) struct Events {
    tx: mpsc::Sender<TurnFrame>,
    seen: TurnObserver,
}

impl Events {
    /// `tx`, read along by `seen`.
    pub(crate) fn new(tx: mpsc::Sender<TurnFrame>, seen: TurnObserver) -> Self {
        Self { tx, seen }
    }

    pub(crate) async fn send(&self, frame: TurnFrame) -> Result<(), SendError<TurnFrame>> {
        self.seen.frame(frame.event, &frame.data);
        self.tx.send(frame).await
    }

    pub(crate) fn try_send(&self, frame: TurnFrame) -> Result<(), TrySendError<TurnFrame>> {
        self.seen.frame(frame.event, &frame.data);
        self.tx.try_send(frame)
    }

    /// Resolves once the reader is gone.
    pub(crate) async fn closed(&self) {
        self.tx.closed().await
    }
}
