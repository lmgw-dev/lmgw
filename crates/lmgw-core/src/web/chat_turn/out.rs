//! What a Chat turn emits, and how a route hands it to the page (chat-voice
//! design §7.1).
//!
//! A turn writes [`TurnFrame`]s — an event name and its JSON — into the
//! channel its caller gave it. The SSE routes (send, edit, regenerate,
//! continue) map each frame to one SSE event with [`sse`], so the wire is
//! what it always was; a caller that is not a page (a realtime session bound
//! to the thread) reads the same frames itself.
//!
//! The channel's receiver is the turn's reader: when it is dropped — the
//! page stopped reading, so its response body went away — the turn sees
//! [`Stopped::ClientGone`](super::Stopped::ClientGone), exactly as it did
//! when the channel carried SSE events.

use std::convert::Infallible;

use axum::response::sse::{Event as SseFrame, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::GatewayError;
use crate::proxy::StopSignal;

/// One thing a turn says: `turn`, `retrieval`, `delta`, `reasoning`, `tool`,
/// `usage`, `stats`, `stop`, `error` or `done`, with its JSON payload.
#[derive(Debug, Clone)]
pub(crate) struct TurnFrame {
    pub event: &'static str,
    pub data: String,
    /// The gateway error an `error` frame says ([`Self::error`]), for a
    /// reader in-process: a bound realtime session fails its response with
    /// this error itself, so its client gets the code, type and message the
    /// stock session would have sent (WP11 server review M1). Never on the
    /// wire; the JSON carries the message and the code.
    pub failure: Option<GatewayError>,
    /// What the request an `error` frame refuses or fails went out as, when
    /// it had a route ([`Self::error_sent`]): for the same in-process reader
    /// — a heard response keeps a server's refusal of the audio by the
    /// model that refused it (voice-audio-input design §3.5). Never on the
    /// wire.
    pub sent: Option<SentAs>,
}

/// Who a routed request went to and what it carried ([`TurnFrame::sent`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SentAs {
    /// Who it went to in the thread model's place — a fallback's alias, a
    /// candidate alias's pick by its public name (`chat_turn::answered_by`);
    /// `None`: the thread's model itself.
    pub answered_by: Option<String>,
    /// It carried an image.
    pub images: bool,
}

impl SentAs {
    /// `answered_by` for request `ir`.
    pub fn of(answered_by: Option<String>, ir: &crate::ir::ChatRequest) -> Self {
        Self {
            answered_by,
            images: crate::gate::media_parts(ir).images > 0,
        }
    }
}

impl TurnFrame {
    pub fn new(event: &'static str, data: String) -> Self {
        Self {
            event,
            data,
            failure: None,
            sent: None,
        }
    }

    /// The `error` frame of a gateway error: `{message, code}`, the code
    /// being the one a client branches on (`GatewayError::code`, e.g.
    /// `gpu_hold`, `context_length_exceeded`, `vram_queue_timeout`).
    pub fn error(e: &GatewayError) -> Self {
        let data = serde_json::json!({ "message": e.to_string(), "code": e.code() });
        Self {
            event: "error",
            data: data.to_string(),
            failure: Some(e.clone()),
            sent: None,
        }
    }

    /// [`Self::error`] of a request that was routed, and what it went out
    /// as — the same bytes on the wire.
    pub fn error_sent(e: &GatewayError, sent: SentAs) -> Self {
        Self {
            sent: Some(sent),
            ..Self::error(e)
        }
    }

    /// The frame as the one SSE event it is on the wire.
    pub(crate) fn into_sse(self) -> SseFrame {
        SseFrame::default().event(self.event).data(self.data)
    }
}

/// What a caller other than a page brings to a turn
/// ([`start_turn_into`](super::start_turn_into)). The SSE routes pass the
/// default.
#[derive(Default)]
pub(crate) struct TurnOpts {
    /// The caller's own stop (a realtime response's): once raised, the turn
    /// ends as [`Stopped::Interrupted`](super::Stopped::Interrupted) — its
    /// partial reply saved, and `done` still sent.
    pub stop: Option<StopSignal>,
    /// A voice turn: a turn of a realtime session bound to the thread, with
    /// audio output (chat-voice design §8.5).
    pub voice: Option<VoiceTurn>,
    /// The language the reply is asked to be in, with the one the user
    /// speaks: any turn of a bound session and a `speak: true` turn, when
    /// the thread has a reply language (§8.5, changed 2026-10-04, split
    /// 2026-10-05).
    pub language: Option<TurnLanguage>,
    /// A `speak: true` turn's read-aloud says here whether its speech plan
    /// stands: the language sentence waits for it, and says the reply is
    /// heard only when it is ([`heard`]). `None` when nothing is read aloud,
    /// or there is no language to say.
    pub heard: Option<tokio::sync::oneshot::Receiver<bool>>,
    /// Told the thread's generation the turn starts at, once it has started
    /// — what its reply is saved against, and what a bound session's
    /// journal guards its later cut with (§8.3).
    pub began: Option<tokio::sync::oneshot::Sender<u64>>,
    /// A heard response's new turns, not yet in the history, in order
    /// (voice-audio-input design §3.4): an audio turn as its
    /// `ContentPart::Audio`, any other as its transcript. They follow the
    /// history as a user message; a turn whose parts carry audio goes only
    /// to a route this lmgw runs (`spoken::local_only`). Never stored.
    pub spoken: Option<Vec<crate::ir::ContentPart>>,
    /// The pre-save barrier (§3.3, §3.4): the journal's answer about the
    /// user row the reply follows. The reply is saved once it said, and
    /// never on a veto.
    pub user_row: Option<super::RowWatch>,
}

/// What makes a turn a voice turn (chat-voice design §8.5): its system
/// message gets the spoken-style block after the thread's prompt, and its
/// reasoning is off unless the thread sets it.
#[derive(Debug, Clone, Default)]
pub(crate) struct VoiceTurn {
    /// The tag/cue hint of the TTS that speaks the reply, when
    /// `realtime.tag_hint` is on.
    pub hint: Option<String>,
}

/// The thread's languages for one turn (chat-voice design §8.5, changed
/// 2026-10-04, split 2026-10-05): the system message says the reply is in
/// `reply`, and — when it is set — that the user speaks `speaks`; inside a
/// voice turn's block, or alone after the thread's prompt
/// (`chat_voice::prompt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnLanguage {
    /// The reply language, ISO 639-1: the model answers in it, and the TTS
    /// speaks it.
    pub reply: String,
    /// The language the user speaks, ISO 639-1; `None` when only a reply
    /// language is set — the prompt then claims nothing about the user.
    pub speaks: Option<String>,
    /// The reply is heard: a voice turn's, or a `speak: true` turn's.
    pub spoken: bool,
}

/// `language` once the read-aloud said whether its speech plan stands
/// (`heard`, [`TurnOpts::heard`]): a refused plan — no text-to-speech
/// model, a voice not found — reads nothing aloud, so the sentence is the
/// text-output one (chat-voice design §8.5). A read-aloud gone before it
/// said counts as refused.
pub(crate) async fn heard(
    language: Option<TurnLanguage>,
    heard: Option<tokio::sync::oneshot::Receiver<bool>>,
) -> Option<TurnLanguage> {
    let Some(heard) = heard else {
        return language;
    };
    let planned = heard.await.unwrap_or(false);
    language.map(|l| TurnLanguage {
        spoken: l.spoken && planned,
        ..l
    })
}

/// End a turn that was refused before it said anything: the refusal's
/// `error` frame ([`TurnFrame::error`]), then `done {aborted}` with no
/// message id — nothing was saved.
pub(crate) async fn refuse(tx: &mpsc::Sender<TurnFrame>, e: &GatewayError) {
    refuse_frame(tx, TurnFrame::error(e)).await;
}

/// [`refuse`] for a request that was routed: its `error` frame says what
/// the request went out as ([`TurnFrame::error_sent`]).
pub(crate) async fn refuse_sent(tx: &mpsc::Sender<TurnFrame>, e: &GatewayError, sent: SentAs) {
    refuse_frame(tx, TurnFrame::error_sent(e, sent)).await;
}

async fn refuse_frame(tx: &mpsc::Sender<TurnFrame>, error: TurnFrame) {
    let _ = tx.send(error).await;
    let done = serde_json::json!({ "aborted": true }).to_string();
    let _ = tx.send(TurnFrame::new("done", done)).await;
}

/// A turn's frames as its SSE response, one event per frame, in order.
pub(super) fn sse(rx: mpsc::Receiver<TurnFrame>) -> Response {
    let events = ReceiverStream::new(rx).map(|f| Ok::<_, Infallible>(f.into_sse()));
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}
