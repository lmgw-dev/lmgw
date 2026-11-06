//! `chat_messages.reasoning_seals` (migration 0096, openai egress design
//! §7.3.2): a plain reply's reasoning seals as a JSON array of
//! [`ReplySeal`] objects, NULL for none.

use serde::{Deserialize, Serialize};

use crate::ir::Seal;

/// One reasoning item's seal in a plain reply, and where the item stood:
/// `at` is the byte offset into the reply's text (`content`) where the
/// item was answered, so a reply that went `[reasoning, text, reasoning,
/// text]` (a GPT-5.3-codex commentary, say) replays in that order, each
/// item ahead of the text that followed it (WP8 review W-N4,
/// `web/chat_turn/replay_seals.rs`). A continued reply's items count from
/// the start of the whole reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplySeal {
    pub at: usize,
    #[serde(flatten)]
    pub seal: Seal,
}

impl ReplySeal {
    pub fn new(at: usize, seal: Seal) -> Self {
        Self { at, seal }
    }

    /// `seals` moved `by` bytes on: the items of a continuation or a
    /// resumed turn, whose text follows the reply's earlier text.
    pub fn shifted(seals: impl IntoIterator<Item = Self>, by: usize) -> Vec<Self> {
        seals
            .into_iter()
            .map(|s| Self::new(s.at + by, s.seal))
            .collect()
    }

    /// `seals` in a reply whose text was trimmed to `len` bytes, to be
    /// continued: an item answered past the end (in the trailing whitespace
    /// a continue trims off) stands at the end, ahead of the continuation,
    /// and not inside it (WP8 verify V-F2).
    pub fn clamped(seals: impl IntoIterator<Item = Self>, len: usize) -> Vec<Self> {
        seals
            .into_iter()
            .map(|s| Self::new(s.at.min(len), s.seal))
            .collect()
    }

    /// `seals` in a reply cut to its first `len` bytes (a spoken reply heard
    /// in part, chat-voice design §8.3): an item answered past the cut goes
    /// with the unheard rest, as its text does, so the model is not given
    /// reasoning about words it never said (WP8 verify V-F2).
    pub fn heard(seals: impl IntoIterator<Item = Self>, len: usize) -> Vec<Self> {
        seals.into_iter().filter(|s| s.at <= len).collect()
    }
}

/// The column's value for `seals`: NULL for none.
pub(super) fn to_column(seals: &[ReplySeal]) -> Option<String> {
    (!seals.is_empty()).then(|| serde_json::to_string(seals).expect("seals serialize"))
}

/// The seals a column holds. A value that does not parse (this process
/// wrote every one) reads as none, with a WARN: the reply then replays as
/// its text and trace, as one from before seals did.
pub(super) fn from_column(v: Option<String>) -> Vec<ReplySeal> {
    let Some(raw) = v else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!("a chat reply's reasoning_seals do not read ({e}); replayed without them");
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_round_trip_and_none_is_null() {
        assert_eq!(to_column(&[]), None);
        let seals = vec![
            ReplySeal::new(
                0,
                Seal::openai(&crate::ir::SealScope::new(3, "cfg"), "rs_1", "e1"),
            ),
            ReplySeal::new(
                5,
                Seal::openai(&crate::ir::SealScope::new(3, "cfg"), "rs_2", "e2"),
            ),
        ];
        let col = to_column(&seals);
        assert_eq!(
            col.as_deref(),
            Some(concat!(
                r#"[{"at":0,"by":"openai","upstream_id":3,"config":"cfg","item_id":"rs_1","#,
                r#""data":"e1"},{"at":5,"by":"openai","upstream_id":3,"config":"cfg","#,
                r#""item_id":"rs_2","data":"e2"}]"#
            ))
        );
        assert_eq!(from_column(col), seals);
        assert_eq!(from_column(None), Vec::<ReplySeal>::new());
        assert_eq!(
            from_column(Some("not json".into())),
            Vec::<ReplySeal>::new()
        );
        assert_eq!(ReplySeal::shifted(seals.clone(), 3)[1].at, 8);
        let clamped = ReplySeal::clamped(seals.clone(), 4);
        assert_eq!((clamped[0].at, clamped[1].at), (0, 4));
        assert_eq!(ReplySeal::heard(seals.clone(), 4), seals[..1]);
        assert_eq!(ReplySeal::heard(seals.clone(), 5), seals);
    }
}
