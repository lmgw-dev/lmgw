//! Voice turns the model hears, the turn's half (voice-audio-input design
//! §3.4): a bound session's response hands its turn the **spoken parts** —
//! its new turns, not yet in the history: an audio turn as its WAV, any
//! other as its transcript — and the **pre-save barrier**, the journal's
//! answer about the user row its reply follows.
//!
//! - **The request** appends the spoken parts as a user message after the
//!   history (`build_messages`); a trailing user row merges with them
//!   (`merge`), so a turn owed again (as text) and the new audio go as one
//!   message. A stored row whose transcription failed and that has no words
//!   goes as [`NOT_TRANSCRIBED`]. Only the turn being answered goes as
//!   audio: earlier spoken turns are rows, and replay as their transcript.
//! - **Local only.** A turn whose spoken parts carry audio goes only to a
//!   route this lmgw runs ([`local_only`], decision 5): `fit_route` refuses
//!   any other with `audio_not_local` before a byte is sent — the admitted
//!   route and every one the gate re-routes the send to (a fallback at
//!   admission, a ladder climb's, a candidate alias's next pick). An
//!   attachment's audio is not local only: attaching is an explicit act.
//! - **The barrier** ([`barrier`]): the reply is saved only once the journal
//!   has said what became of the user row, so the reply row follows it; on
//!   a veto, a row the journal could not write, or a journal gone, nothing
//!   is saved. A reply with nothing in it is no row to wait for: the turn
//!   gives up its save at once, so its wait can never be the one the row's
//!   write waits on (the row waits for the attempt's fate, which the
//!   responder learns from the turn's frames). The tool loop waits for the
//!   same answer before its first tool call ([`tools_may_run`]).
//!
//! **Nothing is stored** (§4): the parts live in the request alone. The
//! tool loop's record starts after the request's messages, so they never
//! reach `ir_messages`; no log line or request row carries a request body.

use std::future::Future;

use tokio::sync::watch;

use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::ContentPart;
use crate::store::ChatMessageRow;

mod seam;
#[cfg(test)]
mod tests;

pub use seam::{spoken_turn_for_tests, spoken_turn_held_for_tests, HeldTurnForTests};

/// The code a route the audio may not go to is refused with (§3.4).
pub(crate) const AUDIO_NOT_LOCAL: &str = "audio_not_local";

/// What a spoken user row with no words goes to a model as: its
/// transcription failed, and the model heard it once (§3.4).
pub(crate) const NOT_TRANSCRIBED: &str = "[spoken turn, not transcribed]";

/// The journal's answer about a heard response's user row (§3.3): the
/// pre-save barrier ([`barrier`]), which the responder reads too (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserRow {
    /// The row was written: its id. The reply is saved after it.
    Written(i64),
    /// The transcription failed and no turn had words: the row is written
    /// with `transcript_error`, and the reply is saved after it. A retry
    /// would have no words to send.
    Failed,
    /// No new row: the new turns had no words, but a turn the response
    /// answers did (one owed again after a cut). The reply is saved.
    NoRow,
    /// Every turn the response answers came back without words: no row,
    /// and nothing is saved.
    Veto,
    /// The journal could not write the row — the store refused it, or the
    /// thread went (its WARN says why): nothing is saved (WP3 review #8).
    Unwritten,
}

/// [`UserRow`] as the journal sets it: `None` until it said.
pub(crate) type RowWatch = watch::Receiver<Option<UserRow>>;

/// Whether `spoken` carries audio: such a turn is local only (§3.4).
pub(crate) fn hears(spoken: Option<&[ContentPart]>) -> bool {
    spoken.is_some_and(|parts| parts.iter().any(|p| matches!(p, ContentPart::Audio { .. })))
}

/// Refuse `route` for a turn whose spoken parts carry audio, unless this
/// lmgw runs it (`vram::classify`, decision 5): a llama-server elsewhere on
/// the network is no more local here than a cloud API. `audio_not_local`,
/// a 400, before anything is sent.
pub(crate) fn local_only(route: &Route) -> Result<(), GatewayError> {
    if crate::vram::classify(route).is_some() {
        return Ok(());
    }
    Err(GatewayError::InvalidRequest {
        code: AUDIO_NOT_LOCAL,
        message: format!(
            "this turn carries the user's speech as audio, which goes only to a model this lmgw \
             runs, and '{}' (upstream '{}') is not one: the audio was not sent",
            route.upstream_model, route.upstream.name
        ),
    })
}

/// The text row `m` goes to a model with: a spoken user row whose
/// transcription failed and that has no words says so ([`NOT_TRANSCRIBED`]);
/// every other row its content, as stored.
pub(crate) fn row_text(m: &ChatMessageRow) -> &str {
    let failed = m
        .voice
        .as_ref()
        .is_some_and(|v| v.transcript_error.is_some());
    if m.role == "user" && failed && m.content.trim().is_empty() {
        NOT_TRANSCRIBED
    } else {
        &m.content
    }
}

/// What the pre-save barrier came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Barrier {
    /// The row is settled (written, failed, or none to write): save the
    /// reply. Also a turn superseded meanwhile, whose save is refused by its
    /// own check.
    Passed,
    /// Every turn the response answers had no words: save nothing.
    Veto,
    /// The journal could not write the row: save nothing.
    Unwritten,
    /// The journal went without saying: save nothing.
    Lost,
}

/// Wait for the journal's answer about the user row a reply follows — or
/// for `superseded`, the turn's own end, whichever is first.
pub(crate) async fn barrier(mut row: RowWatch, superseded: impl Future<Output = ()>) -> Barrier {
    tokio::select! {
        biased;
        said = row.wait_for(Option::is_some) => match said.map(|r| *r) {
            Ok(Some(UserRow::Veto)) => Barrier::Veto,
            Ok(Some(UserRow::Unwritten)) => Barrier::Unwritten,
            Ok(_) => Barrier::Passed,
            Err(_) => Barrier::Lost,
        },
        () = superseded => Barrier::Passed,
    }
}

/// Whether a heard turn's reply may be saved now (`Turn::persist`): `None`
/// once the barrier passed, else `Some(refused)` — it is not saved, and
/// `refused` says whether the turn reports that (`not_saved`). A reply with
/// `nothing` in it is saved by no one (module doc); a veto saves nothing
/// quietly; a row the journal could not write, or a journal gone, is said
/// (WARN, for thread `thread`) as what it is (WP3 review #8).
pub(crate) async fn save_after(
    row: RowWatch,
    nothing: bool,
    superseded: impl Future<Output = ()>,
    thread: i64,
) -> Option<bool> {
    if nothing {
        return Some(false);
    }
    let why = match barrier(row, superseded).await {
        Barrier::Passed => return None,
        Barrier::Veto => return Some(false),
        Barrier::Unwritten => {
            "the spoken turn's user message could not be written (the voice journal's warning \
             says why)"
        }
        Barrier::Lost => {
            "the voice session's journal ended before it wrote the spoken turn's user message"
        }
    };
    tracing::warn!(thread, "chat: {why}; the reply was not saved");
    Some(true)
}

/// Why a heard turn's tools do not run ([`tools_may_run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolsHeld {
    /// Every turn the response answers came back without words.
    Veto,
    /// The transcription failed: the reply plays, and the user hears it,
    /// but nobody confirmed the turn had words, so nothing acts on it.
    Failed,
    /// The turn had words, and the journal could not write its row: the
    /// reply is not saved.
    Unwritten,
    /// The journal went without saying.
    Lost,
}

impl ToolsHeld {
    /// Whether the loop stops here: its reply would not be saved anyway.
    /// A failed transcription's reply goes on, its calls not run.
    pub(crate) fn stops(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

/// Whether a heard turn's tools may run (WP3 review #1): once the journal
/// wrote the turn's user row, or had none to write (a turn owed again had
/// the words). Never on a veto (noise the model answered), a failed
/// transcription (a reply that plays is heard by the user; a tool run is
/// a side effect nobody confirmed had words), a row the journal could not
/// write, or a journal gone. The tool loop asks before its first tool
/// call, so nothing a tool does happens for a turn that turns out to be
/// noise.
pub(crate) async fn tools_may_run(mut row: RowWatch) -> Result<(), ToolsHeld> {
    match row.wait_for(Option::is_some).await.map(|r| *r) {
        Ok(Some(UserRow::Written(_) | UserRow::NoRow)) => Ok(()),
        Ok(Some(UserRow::Failed)) => Err(ToolsHeld::Failed),
        Ok(Some(UserRow::Veto)) => Err(ToolsHeld::Veto),
        Ok(Some(UserRow::Unwritten)) => Err(ToolsHeld::Unwritten),
        Ok(None) | Err(_) => Err(ToolsHeld::Lost),
    }
}
