//! A Chat turn on a fallback that cannot see (the owner's ruling,
//! 2026-10-06: a configured fallback is always used, with no exception by
//! content; `gate::fallback_images`).
//!
//! The thread's attachments are rendered for the thread's own model
//! (`model_caps`): a PDF sent as page images goes as images. When the route
//! a send goes out on is a fallback whose capabilities say `vision: false` —
//! the GPU hold's or a benchmark lease's swap, §4.7's outside-VRAM swap, a
//! ladder climb's — the send carries ([`Blind::on_route`]):
//!
//! - **each such PDF as its text form**: its extracted text, and a note for
//!   pages without text (`chat_attach::Rendered::text_form`), from the
//!   parallel message list the turn's request was built with
//!   ([`super::request`]); adjacent user messages merge by role alone, so
//!   both lists have the same messages;
//! - **every other image as a placeholder**, the one `gate::fit_chat`
//!   would put there, named in the WARN once per turn however many calls
//!   the tool loop makes (`fallback_images::Announced`).
//!
//! The plain stream (`chat::relay`) and the tool loop (`agentchat`'s
//! `ChatRunner`) ask on every route they send on, re-routes included, before
//! anything else is fitted to it. The reply says what happened
//! ([`Blind::note`], `done.images_note`, stored with it): once a call of the
//! turn went to a fallback that cannot see, the note stays for the turn.

use std::sync::Mutex;

use crate::config::Route;
use crate::gate::fallback_images::{blind_fallback, carries_images, Announced, Unseen};
use crate::ir::{ChatRequest, Message};
use crate::state::SharedState;

/// What one turn holds for a fallback that cannot see (module doc).
#[derive(Debug, Default)]
pub(crate) struct Blind {
    /// The request's messages with every PDF that went as page images in
    /// its text form; `None` when no attachment has one.
    text_form: Option<Vec<Message>>,
    /// The images the turn has named in its WARN.
    announced: Announced,
    /// A PDF's text form went out: its WARN line is said once per turn too.
    swapped: Mutex<bool>,
    /// The turn's note, once a send of it went to a fallback that cannot
    /// see.
    note: Mutex<Option<String>>,
    /// What the last send [`Blind::on_route`] was asked about lost, as its
    /// request row says it (`request_logs.degraded`); `None` when it went
    /// as the turn's request is.
    last: Mutex<Option<String>>,
}

impl Blind {
    pub(crate) fn new(text_form: Option<Vec<Message>>) -> Self {
        Self {
            text_form,
            ..Self::default()
        }
    }

    /// `ir` as a send on `route` carries it: `None` when that is `ir` as it
    /// is — `route` is no fallback, or one that sees, or `ir` has no image
    /// to give it. Otherwise its PDFs in their text form and its other
    /// images as placeholders (module doc). `ir` is the turn's request, a
    /// tool loop's record after it: the text form replaces the messages the
    /// request was built with, and only those.
    pub(crate) async fn on_route(
        &self,
        state: &SharedState,
        route: &Route,
        ir: &ChatRequest,
    ) -> Option<ChatRequest> {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if self.text_form.is_none() && !carries_images(ir) {
            return None;
        }
        let unseen = blind_fallback(state, route, &ir.model_alias).await?;
        let mut out = ir.clone();
        let swapped = match &self.text_form {
            Some(form) => {
                let n = form.len().min(out.messages.len());
                out.messages.splice(..n, form[..n].iter().cloned());
                true
            }
            None => false,
        };
        if swapped {
            let mut said = self.swapped.lock().unwrap_or_else(|e| e.into_inner());
            if !*said {
                *said = true;
                tracing::warn!(
                    fallback = %unseen.fallback,
                    "a PDF's page images not sent to the fallback '{}' answering for '{}', \
                     which cannot see images — its text sent instead",
                    unseen.fallback,
                    unseen.requested
                );
            }
        }
        let (out, n) = if carries_images(&out) {
            self.announced.without_images(&out, &unseen)
        } else {
            (out, 0)
        };
        let placeholders = self.announced.count() > 0;
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(note(&unseen, swapped, placeholders));
        let pdf = swapped.then(|| {
            crate::degraded::lacks(&unseen.fallback, true, "vision", "PDF pages sent as text")
        });
        let images = (n > 0).then(|| unseen.marker(n));
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = crate::degraded::join([pdf, images]);
        Some(out)
    }

    /// What the last send [`Self::on_route`] was asked about lost, for its
    /// row (`request_logs.degraded`): `None` when it went as it was.
    pub(crate) fn marker(&self) -> Option<String> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The turn's note, once a send of it went to a fallback that cannot
    /// see; it stays for the rest of the turn.
    pub(crate) fn note(&self) -> Option<String> {
        self.note.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The reply's note (module doc): who answered for which model, and what
/// went to it in the images' place.
fn note(unseen: &Unseen, text_form: bool, placeholders: bool) -> String {
    let what = match (text_form, placeholders) {
        (true, true) => "PDF pages went to it as the PDF's text, other images as placeholders",
        (true, false) => "PDF pages went to it as the PDF's text",
        (false, _) => "the images went to it as placeholders",
    };
    format!(
        "'{}' answered in place of '{}' and cannot see images: {what}",
        unseen.fallback, unseen.requested
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unseen() -> Unseen {
        Unseen {
            fallback: "cloud-blind".into(),
            requested: "chat-model".into(),
        }
    }

    #[test]
    fn the_note_says_who_answered_and_what_went_in_the_images_place() {
        assert_eq!(
            note(&unseen(), false, true),
            "'cloud-blind' answered in place of 'chat-model' and cannot see images: the images \
             went to it as placeholders"
        );
        assert_eq!(
            note(&unseen(), true, false),
            "'cloud-blind' answered in place of 'chat-model' and cannot see images: PDF pages \
             went to it as the PDF's text"
        );
        assert_eq!(
            note(&unseen(), true, true),
            "'cloud-blind' answered in place of 'chat-model' and cannot see images: PDF pages \
             went to it as the PDF's text, other images as placeholders"
        );
    }
}
