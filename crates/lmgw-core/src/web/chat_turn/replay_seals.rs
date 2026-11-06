//! A plain reply's reasoning, replayed (openai egress design §7.3.2): its
//! trace (`chat_messages.reasoning`) and its seals (`reasoning_seals`,
//! migration 0096) as reasoning parts among its text, each item where it was
//! answered ([`ReplySeal::at`]).
//!
//! The column keeps the trace as one text, so the parts cannot be split back
//! into each item's own summary: the first sealed part carries the whole
//! trace, every later one none. The platform reads an item by its
//! `encrypted_content` (the summary is what the model wrote about it), and
//! every other upstream replays the parts' texts joined, which is the whole
//! trace again. A reply answered as `[reasoning, text, reasoning, text]` (a
//! GPT-5.3-codex commentary, say) replays in that order, its text split
//! where the second item stood (WP8 review W-N4); an upstream that joins a
//! message's texts (`Message::joined_text`) joins the two pieces with a line
//! break, as it joins a tool turn's.

use crate::ir::ContentPart;
use crate::store::ReplySeal;

/// The parts of a plain reply with trace `reasoning`, seals `seals` and text
/// `text` that come before its last stretch of text, and that last stretch,
/// which the caller adds as its text part (module doc). A reply without
/// seals is one unsealed part (none for no trace) ahead of the whole text.
pub(super) fn replayed_reasoning<'a>(
    reasoning: &str,
    seals: &[ReplySeal],
    text: &'a str,
) -> (Vec<ContentPart>, &'a str) {
    if seals.is_empty() {
        let lead = match reasoning.is_empty() {
            true => Vec::new(),
            false => vec![ContentPart::reasoning(reasoning)],
        };
        return (lead, text);
    }
    let mut parts = Vec::with_capacity(2 * seals.len());
    let mut from = 0usize;
    for (i, s) in seals.iter().enumerate() {
        // An offset past the text (a continued row's text was trimmed of its
        // trailing whitespace) or inside a character is where it can stand.
        let mut at = s.at.clamp(from, text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        if at > from {
            parts.push(ContentPart::text(&text[from..at]));
            from = at;
        }
        let trace = if i == 0 { reasoning } else { "" };
        parts.push(ContentPart::sealed_reasoning(trace, s.seal.clone()));
    }
    (parts, &text[from..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Seal;

    #[test]
    fn a_reply_s_seals_replay_as_its_reasoning_parts() {
        assert_eq!(
            replayed_reasoning("", &[], "Hi."),
            (Vec::<ContentPart>::new(), "Hi.")
        );
        assert_eq!(
            replayed_reasoning("Hm.", &[], "Hi."),
            (vec![ContentPart::reasoning("Hm.")], "Hi.")
        );
        let (s1, s2) = (
            Seal::openai(&crate::ir::SealScope::new(4, "cfg"), "rs_1", "e1"),
            Seal::openai(&crate::ir::SealScope::new(4, "cfg"), "rs_2", "e2"),
        );
        let ahead = [ReplySeal::new(0, s1.clone()), ReplySeal::new(0, s2.clone())];
        assert_eq!(
            replayed_reasoning("First.\n\nSecond.", &ahead, "Hi."),
            (
                vec![
                    ContentPart::sealed_reasoning("First.\n\nSecond.", s1.clone()),
                    ContentPart::sealed_reasoning("", s2.clone()),
                ],
                "Hi."
            )
        );
        // An item that came with no summary keeps its seal.
        assert_eq!(
            replayed_reasoning("", &ahead[..1], ""),
            (vec![ContentPart::sealed_reasoning("", s1.clone())], "")
        );
    }

    /// `[reasoning, text, reasoning, text]` replays in that order (W-N4);
    /// an offset past the text or inside a character stands where it can.
    #[test]
    fn items_between_messages_replay_where_they_were_answered() {
        let (s1, s2) = (
            Seal::openai(&crate::ir::SealScope::new(4, "cfg"), "rs_1", "e1"),
            Seal::openai(&crate::ir::SealScope::new(4, "cfg"), "rs_2", "e2"),
        );
        let seals = [ReplySeal::new(0, s1.clone()), ReplySeal::new(4, s2.clone())];
        assert_eq!(
            replayed_reasoning("Trace.", &seals, "Hm. Done."),
            (
                vec![
                    ContentPart::sealed_reasoning("Trace.", s1.clone()),
                    ContentPart::text("Hm. "),
                    ContentPart::sealed_reasoning("", s2.clone()),
                ],
                "Done."
            )
        );
        let past = [
            ReplySeal::new(0, s1.clone()),
            ReplySeal::new(40, s2.clone()),
        ];
        assert_eq!(replayed_reasoning("", &past, "Hm.").1, "");
        let inside = [ReplySeal::new(1, s1.clone())];
        assert_eq!(
            replayed_reasoning("", &inside, "äb"),
            (vec![ContentPart::sealed_reasoning("", s1)], "äb")
        );
    }
}
