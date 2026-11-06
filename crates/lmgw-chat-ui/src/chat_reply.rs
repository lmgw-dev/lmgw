//! What a finished reply says about itself: who answered, and whether it was
//! stored. The pure half, so it is unit-testable off the DOM.
//!
//! A reply's `done` frame carries `saved`, `model` (the thread's alias the
//! turn asked for) and `answered_by` (the alias that answered in its place);
//! a stored row carries the last two. `message_id` 0 or `saved: false` means
//! the server refused the save — the bubble is not a row, whatever it shows.

use lmgw_api_types::chat_frames::DoneFrame;

/// The note under a reply that was not stored.
pub(super) const UNSAVED_NOTE: &str =
    "not saved — the conversation changed while this reply was being written; copy it if you \
     want it";

/// What a `done` frame says of the reply's storage and author.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Finished {
    /// The stored row's id; `None` when nothing was saved.
    pub id: Option<i64>,
    pub model: Option<String>,
    pub answered_by: Option<String>,
    /// A fallback that cannot see answered: what it got in the images'
    /// place (`images_note`, only on such a reply).
    pub images_note: Option<String>,
}

impl Finished {
    pub fn from_done(done: &DoneFrame) -> Self {
        // A turn that saved nothing (stopped, refused, a warm-up) says none
        // of it.
        let Some(saved) = &done.saved else {
            return Self::default();
        };
        let text = |t: Option<&str>| t.filter(|s| !s.is_empty()).map(str::to_string);
        Self {
            id: Some(saved.message_id).filter(|id| *id > 0 && saved.saved),
            model: text(Some(&saved.model)),
            answered_by: text(saved.answered_by.as_deref()),
            images_note: text(saved.images_note.as_deref()),
        }
    }

    /// The frame described a refused save: a `done` that carries a row id of 0
    /// or `saved: false`. A bare `{aborted: true}` (a stopped turn) says
    /// nothing either way.
    pub fn refused(done: &DoneFrame) -> bool {
        done.saved
            .as_ref()
            .is_some_and(|s| !s.saved || s.message_id == 0)
    }
}

/// "qwen3-8b", or "gpt-5.2 (fallback for qwen3-8b)" when another alias
/// answered; `None` for a reply with no recorded model.
pub(super) fn answer_label(model: Option<&str>, answered_by: Option<&str>) -> Option<String> {
    let model = model.filter(|m| !m.is_empty());
    let by = answered_by.filter(|m| !m.is_empty());
    match (model, by) {
        (Some(m), Some(b)) if b != m => Some(format!("{b} (fallback for {m})")),
        (Some(m), _) => Some(m.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_frames::DoneSaved;

    #[test]
    fn labels_the_answering_model() {
        assert_eq!(
            answer_label(Some("qwen3-8b"), None).as_deref(),
            Some("qwen3-8b")
        );
        assert_eq!(
            answer_label(Some("qwen3-8b"), Some("qwen3-8b")).as_deref(),
            Some("qwen3-8b")
        );
        assert_eq!(
            answer_label(Some("qwen3-8b"), Some("gpt-5.2")).as_deref(),
            Some("gpt-5.2 (fallback for qwen3-8b)")
        );
        assert_eq!(answer_label(Some(""), None), None);
        assert_eq!(answer_label(None, None), None);
    }

    /// A `done` that saved something: the reply's row id, and whether it is
    /// stored.
    fn done(message_id: i64, saved: bool) -> DoneFrame {
        DoneFrame {
            aborted: false,
            saved: Some(DoneSaved {
                message_id,
                saved,
                model: "m".into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn a_done_frame_says_whether_the_reply_is_a_row() {
        let ok = Finished::from_done(&done(7, true));
        assert_eq!(ok.id, Some(7));
        assert_eq!(ok.model.as_deref(), Some("m"));
        assert_eq!(ok.answered_by, None);
        assert!(!Finished::refused(&done(7, true)));
        // Zero or `saved: false` is no row, and is a refusal.
        for d in [done(0, false), done(0, true), done(9, false)] {
            assert_eq!(Finished::from_done(&d).id, None, "{d:?}");
            assert!(Finished::refused(&d), "{d:?}");
        }
        // A stopped turn's bare done, and the warm-up's, are neither.
        for d in [DoneFrame::aborted(), DoneFrame::default()] {
            assert_eq!(Finished::from_done(&d), Finished::default());
            assert!(!Finished::refused(&d), "{d:?}");
        }
    }

    #[test]
    fn a_done_frame_names_who_answered_and_what_the_images_became() {
        let mut d = done(3, true);
        let saved = d.saved.as_mut().unwrap();
        saved.answered_by = Some("gpt-5.2".into());
        saved.images_note = Some("placeholders".into());
        let f = Finished::from_done(&d);
        assert_eq!(f.answered_by.as_deref(), Some("gpt-5.2"));
        assert_eq!(f.images_note.as_deref(), Some("placeholders"));
        // An empty text is none.
        let saved = d.saved.as_mut().unwrap();
        saved.answered_by = Some(String::new());
        saved.model = String::new();
        let f = Finished::from_done(&d);
        assert_eq!((f.answered_by, f.model), (None, None));
    }
}
