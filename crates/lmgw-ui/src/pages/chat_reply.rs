//! What a finished reply says about itself: who answered, and whether it was
//! stored. The pure half, so it is unit-testable off the DOM.
//!
//! A reply's `done` frame carries `saved`, `model` (the thread's alias the
//! turn asked for) and `answered_by` (the alias that answered in its place);
//! a stored row carries the last two. `message_id` 0 or `saved: false` means
//! the server refused the save — the bubble is not a row, whatever it shows.

use serde_json::Value;

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
}

impl Finished {
    pub fn from_done(done: &Value) -> Self {
        let text = |k: &str| {
            done[k]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let saved = done["saved"].as_bool().unwrap_or(true);
        Self {
            id: done["message_id"].as_i64().filter(|id| *id > 0 && saved),
            model: text("model"),
            answered_by: text("answered_by"),
        }
    }

    /// The frame described a refused save: a `done` that carries a row id of 0
    /// or `saved: false`. A bare `{aborted: true}` (a stopped turn) says
    /// nothing either way.
    pub fn refused(done: &Value) -> bool {
        done["saved"].as_bool() == Some(false) || done["message_id"].as_i64() == Some(0)
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
    use serde_json::json;

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

    #[test]
    fn a_done_frame_says_whether_the_reply_is_a_row() {
        let ok = Finished::from_done(
            &json!({"message_id": 7, "saved": true, "model": "m", "answered_by": null}),
        );
        assert_eq!(ok.id, Some(7));
        assert_eq!(ok.model.as_deref(), Some("m"));
        assert_eq!(ok.answered_by, None);
        // Zero or `saved: false` is no row, and is a refusal; an older server
        // without `saved` still reads by the id.
        for d in [
            json!({"message_id": 0, "saved": false}),
            json!({"message_id": 0}),
            json!({"message_id": 9, "saved": false}),
        ] {
            assert_eq!(Finished::from_done(&d).id, None, "{d}");
            assert!(Finished::refused(&d), "{d}");
        }
        assert_eq!(Finished::from_done(&json!({"message_id": 3})).id, Some(3));
        // A stopped turn's bare done is neither.
        assert!(!Finished::refused(&json!({"aborted": true})));
    }
}
