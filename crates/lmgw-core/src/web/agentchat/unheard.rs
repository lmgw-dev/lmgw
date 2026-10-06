//! A heard tool turn handed, mid-loop, to a model that cannot take its
//! audio (review V2): the call goes with the turn's transcript instead.
//!
//! Every model call of a heard turn's loop carries the user's audio, and
//! each route it lands on is judged (`spoken::may_hear`). The first call's
//! refusal goes back to the bound responder, which answers the turn again
//! from its transcript before anything was said. A later call is past that
//! point — its tool frames are out — and the gate may still hand it to a
//! model that cannot hear: a candidate alias's re-pick, or the fallback a
//! GPU hold switched on meanwhile hands it to (`regain`). Refusing it there
//! would end the turn on an error and leave the configured fallback unused,
//! which the owner's ruling (2026-10-06) forbids. By then the journal has
//! written the turn's user row, so the call goes as text: the spoken parts
//! give way to the row's words. While the row is not settled yet, the
//! refusal stands (and the responder retries it).

use crate::error::GatewayError;
use crate::ir::{ChatRequest, ContentPart};
use crate::state::SharedState;
use crate::web::chat_turn::{RowWatch, UserRow, NOT_TRANSCRIBED};

#[cfg(test)]
mod tests;

/// A heard turn's spoken parts in its loop's request (module doc).
pub(super) struct Spoken {
    /// The request's message that holds them: the last before the loop's
    /// record.
    at: usize,
    /// The new turns that went as their transcript, beside the audio.
    texts: Vec<String>,
    thread_id: i64,
    row: RowWatch,
}

impl Spoken {
    pub(super) fn new(at: usize, texts: Vec<String>, thread_id: i64, row: RowWatch) -> Self {
        Self {
            at,
            texts,
            thread_id,
            row,
        }
    }

    /// `ir` with the spoken parts replaced by the words of the turn's user
    /// row, once the journal said what it wrote; `None` while it has not, or
    /// when the turn ends without a reply (a veto, a row not written).
    pub(super) async fn as_transcript(
        &self,
        state: &SharedState,
        ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        let said = *self.row.borrow();
        let words = match said {
            Some(UserRow::Written(id)) => {
                let row = crate::store::get_chat_message(&state.db, self.thread_id, id).await?;
                row.map(|r| crate::web::chat_turn::row_text(&r).to_string())
            }
            Some(UserRow::Failed) => Some(NOT_TRANSCRIBED.to_string()),
            Some(UserRow::NoRow) => None,
            Some(UserRow::Veto | UserRow::Unwritten) | None => return Ok(None),
        };
        Ok(Some(substitute(ir, self.at, &self.texts, words.as_deref())))
    }
}

/// `ir` with message `at`'s spoken parts — every audio part, and the
/// `texts` that went beside it — replaced by `words` as a text of its own.
/// A spoken text the request joined onto a trailing user row's text (by a
/// blank line, `merge`) is taken off that text's end.
pub(super) fn substitute(
    ir: &ChatRequest,
    at: usize,
    texts: &[String],
    words: Option<&str>,
) -> ChatRequest {
    let mut out = ir.clone();
    let Some(m) = out.messages.get_mut(at) else {
        return out;
    };
    m.content
        .retain(|p| !matches!(p, ContentPart::Audio { .. }));
    for t in texts.iter().filter(|t| !t.is_empty()) {
        let whole = m
            .content
            .iter()
            .position(|p| matches!(p, ContentPart::Text { text } if text == t));
        if let Some(i) = whole {
            m.content.remove(i);
            continue;
        }
        for p in m.content.iter_mut() {
            if let ContentPart::Text { text } = p {
                if let Some(head) = text.strip_suffix(t.as_str()) {
                    *text = head.strip_suffix("\n\n").unwrap_or(head).to_string();
                    break;
                }
            }
        }
    }
    if let Some(w) = words.filter(|w| !w.is_empty()) {
        m.content.push(ContentPart::text(w));
    }
    if m.content.is_empty() {
        m.content.push(ContentPart::text(NOT_TRANSCRIBED));
    }
    out
}
