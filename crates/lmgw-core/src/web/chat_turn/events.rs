//! Where a turn's frames go ([`Events`]): its caller's channel, with the
//! change feed reading along (client-apps design §2.2). The code of the
//! last `error` frame that carried one becomes the turn's `turn.done`
//! `code` — whichever path of the plain stream or the tool loop said it.

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};

use super::TurnFrame;
use crate::web::chat_feed::TurnObserver;

/// Room for one frame on a turn's channel ([`Events::reserve`]).
pub(crate) struct Slot<'a> {
    permit: mpsc::Permit<'a, TurnFrame>,
    seen: &'a TurnObserver,
}

impl Slot<'_> {
    pub(crate) fn send(self, frame: TurnFrame) {
        self.seen.frame(frame.event, &frame.data);
        self.permit.send(frame);
    }
}

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

    /// A slot for one frame, once the reader has room: a caller that must
    /// not stop for a slow reader waits for it beside its own work
    /// (`tokio::select!`) and fills it with whatever is newest by then.
    /// `Err` once the reader is gone.
    pub(crate) async fn reserve(&self) -> Result<Slot<'_>, SendError<()>> {
        let permit = self.tx.reserve().await?;
        Ok(Slot {
            permit,
            seen: &self.seen,
        })
    }

    /// Resolves once the reader is gone.
    pub(crate) async fn closed(&self) {
        self.tx.closed().await
    }
}
