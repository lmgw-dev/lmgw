//! A heard response that waits for its user row — a refused attempt before
//! its retry, a skipped one before its transcript turn (`super`'s module
//! doc): how the journal settled the row, and the frames the response ends
//! with when the row ended it.

use tokio::sync::mpsc;

use super::Started;
use crate::error::GatewayError;
use crate::proxy::StopSignal;
use crate::web::chat_voice::bound::{RowWatch, TurnFrame, UserRow};

/// How the user row was settled, or why the wait ended.
pub(super) enum Settled {
    /// Written, or none to write: the transcript turn can go.
    Row,
    /// The transcription failed, and no turn had words. (Only a row a model
    /// heard says so, and a waiting attempt carried nothing: kept for what
    /// the journal may still say.)
    Failed,
    /// A veto, or the response's stop: end quietly.
    Quiet,
    /// The store refused the row, or the thread went (WP3 review #8).
    Unwritten,
    /// The journal went without saying.
    Lost,
}

impl Settled {
    /// The frames a response that waited for its row ends with, when the
    /// row ended it.
    pub fn frames(self) -> Option<Vec<TurnFrame>> {
        match self {
            Self::Row => None,
            Self::Failed => Some(refused(&transcription_failed())),
            Self::Quiet => Some(vec![aborted()]),
            Self::Unwritten => Some(refused(&row_unwritten())),
            Self::Lost => Some(refused(&journal_gone())),
        }
    }
}

/// Wait for the journal's answer about the user row (`row`), or the stop.
pub(super) async fn settled(row: Option<RowWatch>, stop: &StopSignal) -> Settled {
    let Some(mut row) = row else {
        return Settled::Lost;
    };
    tokio::select! {
        biased;
        () = stop.raised() => Settled::Quiet,
        said = row.wait_for(Option::is_some) => match said.map(|r| *r) {
            Ok(Some(UserRow::Written(_) | UserRow::NoRow)) => Settled::Row,
            Ok(Some(UserRow::Failed)) => Settled::Failed,
            Ok(Some(UserRow::Veto)) => Settled::Quiet,
            Ok(Some(UserRow::Unwritten)) => Settled::Unwritten,
            Ok(None) | Err(_) => Settled::Lost,
        },
    }
}

/// A refused turn's frames: its error, then `done {aborted}`
/// (`chat_turn::refuse`'s).
pub(super) fn refused(e: &GatewayError) -> Vec<TurnFrame> {
    vec![TurnFrame::error(e), aborted()]
}

/// A turn that only says `frames`: how a skipped attempt's response ended,
/// relayed as a turn's own frames are.
pub(super) fn said(frames: Vec<TurnFrame>) -> Started {
    let (tx, rx) = mpsc::channel(frames.len().max(1));
    for f in frames {
        let _ = tx.try_send(f);
    }
    (rx, None)
}

fn aborted() -> TurnFrame {
    TurnFrame::new("done", serde_json::json!({ "aborted": true }).to_string())
}

/// A refused attempt whose turns all failed to transcribe: no words to send
/// again (worded as the session's own `transcription_failed`).
fn transcription_failed() -> GatewayError {
    GatewayError::Refused {
        status: 500,
        code: "transcription_failed",
        message: "the model could not take the speech as audio, and it could not be transcribed: \
                  say it again, or send the text"
            .into(),
    }
}

/// The journal could not write the turn's user row (the store refused it,
/// or the thread went; its WARN says why): the turn is not answered, as a
/// user entry the store refused is not (`journal`'s
/// `chat_history_write_failed`, WP3 review #8).
fn row_unwritten() -> GatewayError {
    GatewayError::Refused {
        status: 500,
        code: "chat_history_write_failed",
        message: "the spoken turn could not be written to the chat thread, so it is not answered"
            .into(),
    }
}

fn journal_gone() -> GatewayError {
    GatewayError::Internal("the voice session's journal ended before it wrote the turn".into())
}
