//! The speech tee (chat-voice design §6.4): a turn sent with `speak: true` —
//! a send, an edited user message, a regenerate or a continue — is read
//! aloud while its text streams.
//!
//! The tee sits between the turn's worker and the page's SSE, as a stream
//! adapter with no task of its own; the read-aloud runs in its own
//! (`speech::start`), so the text's first byte waits on none of its
//! planning (review m4):
//! - **every frame of the turn is forwarded** as it comes, the wire
//!   unchanged; a `delta`'s text is also fed to the read-aloud, and a tool
//!   call's `start` flushes the clause in progress, so a preamble is heard
//!   while the tool runs;
//! - **the read-aloud's frames are interleaved** (`state`, `voice`,
//!   `speech`, `speech_error`, `speech_done`): speech starts at the first
//!   clause, while the text still streams, and goes on after the text's
//!   `done` until `speech_done`. A voice that fails sends `speech_error`
//!   and the text carries on. These turns get no voice prompt: they are
//!   text turns that are also read;
//! - **the TTS is warmed beside the prefill** in the background — never
//!   evicting, since the chat turn beside it is the real request (§4.2) —
//!   and its `state` frames are interleaved too (a model that was up says
//!   nothing; one `loading` and one end per stage);
//! - **a continue is read from the clause it finishes** ([`ReadAloud`]):
//!   the stored reply's unfinished clause is fed first, so the first clause
//!   said is whole, not "n vor." (review m5; `speech::lead`);
//! - **closure passes upstream**: the stream holds the worker's receiver
//!   and the read-aloud's. When the page goes (Stop, a reload, leaving the
//!   thread), both are dropped: the worker sees `ClientGone` and saves its
//!   partial reply, as without the tee, and the read-aloud sees its reader
//!   gone and stops, so synthesis ends too. `speech/stop` stops only the
//!   latter.
//!
//! The opening `turn` frame stays first: the turn's frame is read before
//! the speech's for the first frame; after it, whichever is there (review
//! n6), so a burst of deltas does not hold the speech back.

use std::convert::Infallible;
use std::time::Instant;

use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::state::SharedState;
use crate::store::ChatThread;

use super::super::chat_caller::Caller;
use super::super::chat_repo::ChatRepo;
use super::super::chat_turn::TurnFrame;
use super::speech::{self, Feed, SpeechRx};

/// A turn read aloud as it streams (module doc): `lead` is what its
/// read-aloud is fed before the turn's text.
#[derive(Debug, Default)]
pub(crate) struct ReadAloud {
    lead: String,
}

impl ReadAloud {
    /// A continue of the stored reply `content`: its unfinished clause
    /// first (`speech::lead`).
    pub(crate) fn continuing(content: &str) -> Self {
        Self {
            lead: speech::lead(content).to_string(),
        }
    }
}

/// The SSE answer of a turn sent with `speak: true` (module doc): the
/// worker writes into `turn`; `started` is the request's start, which
/// `first_audio_ms` counts from; `planned` is told whether the speech plan
/// stands, which the turn's language sentence waits for
/// (`TurnOpts::heard`).
pub(crate) fn speaking_turn(
    state: &SharedState,
    caller: &Caller,
    repo: ChatRepo,
    thread: &ChatThread,
    turn: mpsc::Receiver<TurnFrame>,
    (started, planned): (Instant, Option<oneshot::Sender<bool>>),
    read: ReadAloud,
) -> Response {
    let (feed, fed) = mpsc::unbounded_channel();
    if !read.lead.is_empty() {
        let _ = feed.send(Feed::Text(read.lead));
    }
    let speech = speech::start(state, caller, repo, thread, (started, planned), true, fed);
    let tee = Tee {
        turn: Some(turn),
        speech: Some(speech),
        feed: Some(feed),
        first: true,
    };
    let events = futures::stream::unfold(tee, |mut tee| async move {
        let f = tee.next().await?;
        Some((Ok::<_, Infallible>(f.into_sse()), tee))
    });
    Sse::new(caller.sse(state, events))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The tee's state (module doc).
struct Tee {
    /// The worker's frames; `None` once it is done.
    turn: Option<mpsc::Receiver<TurnFrame>>,
    /// The read-aloud's frames; `None` once it is done. Dropped with the
    /// stream: that stops the speech.
    speech: Option<SpeechRx>,
    /// The read-aloud's text; `None` once the turn is done (which ends the
    /// read-aloud's input) or the read-aloud is gone.
    feed: Option<mpsc::UnboundedSender<Feed>>,
    /// No frame has gone to the page yet: the turn's `turn` goes first.
    first: bool,
}

/// Which side a frame came from.
enum Next {
    Turn(Option<TurnFrame>),
    Speech(Option<TurnFrame>),
}

impl Tee {
    /// The next frame for the page; `None` once both the turn and the
    /// speech are done.
    async fn next(&mut self) -> Option<TurnFrame> {
        loop {
            if self.turn.is_none() && self.speech.is_none() {
                return None;
            }
            let next = if self.first {
                tokio::select! {
                    biased;
                    f = next_of(&mut self.turn) => Next::Turn(f),
                    f = next_speech(&mut self.speech) => Next::Speech(f),
                }
            } else {
                tokio::select! {
                    f = next_of(&mut self.turn) => Next::Turn(f),
                    f = next_speech(&mut self.speech) => Next::Speech(f),
                }
            };
            match next {
                Next::Turn(Some(f)) => {
                    self.first = false;
                    self.feed(&f);
                    return Some(f);
                }
                Next::Turn(None) => {
                    self.turn = None;
                    // The end of the text: the last clause is said.
                    self.feed = None;
                }
                Next::Speech(Some(f)) => {
                    self.first = false;
                    return Some(f);
                }
                Next::Speech(None) => self.speech = None,
            }
        }
    }

    /// Feed the read-aloud what `f` says: a `delta`'s text, or a flush at a
    /// tool call's start.
    fn feed(&mut self, f: &TurnFrame) {
        let Some(feed) = &self.feed else {
            return;
        };
        let fed = match f.event {
            "delta" => serde_json::from_str::<Value>(&f.data)
                .ok()
                .and_then(|v| v["text"].as_str().map(|t| Feed::Text(t.to_string()))),
            "tool" => serde_json::from_str::<Value>(&f.data)
                .ok()
                .filter(|v| v["event"] == "start")
                .map(|_| Feed::Flush),
            _ => None,
        };
        if let Some(fed) = fed {
            if feed.send(fed).is_err() {
                self.feed = None;
            }
        }
    }
}

async fn next_of(rx: &mut Option<mpsc::Receiver<TurnFrame>>) -> Option<TurnFrame> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_speech(rx: &mut Option<SpeechRx>) -> Option<TurnFrame> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
