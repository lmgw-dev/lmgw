//! What an Anthropic-protocol row is sent of one part (gateway design §6.1,
//! "Where a signature goes", changed 2026-11-06): the one predicate the
//! egress renders by (`egress/anthropic.rs`), the request's
//! `x-lmgw-params-ignored` names by ([`super::left_out_for`]) and lmgw's own
//! estimates count by (`proxy/count_messages/estimate.rs`), so the three
//! cannot disagree.
//!
//! What lmgw read from an answer at another scope ([`Signer`]) is not sent:
//! a signed thinking block, a verbatim block, a text's citations (the text
//! goes without them), a compaction block (the egress folds its summary in,
//! as every other egress does, [`super::super::compaction::folded_for`]). A text with no text is not sent either, as
//! Anthropic's `TextBlockParam.text` has `minLength: 1`
//! (<https://platform.claude.com/docs/en/api/messages/create>); when it had
//! citations, they are named. An unsigned thinking trace is not sent, as
//! Anthropic refuses one without a signature, and not named: it is none of
//! Anthropic's.

use serde_json::Value;

use super::super::{signer, ContentPart, Signer};

/// What the row is sent of a part ([`AnthropicSent::of`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnthropicSent<'a> {
    /// The part as it is (every part this module has no rule for).
    Whole,
    /// A text without its citations, which another scope answered with.
    TextOnly(&'a str),
    /// Nothing of it.
    Nothing,
}

impl<'a> AnthropicSent<'a> {
    /// What a row at `row` is sent of `p` (module doc).
    pub fn of(p: &'a ContentPart, row: &Signer) -> Self {
        match p {
            ContentPart::Text { text, .. } if text.is_empty() => Self::Nothing,
            ContentPart::Text {
                text,
                citations,
                citations_scope,
            } if !citations.is_empty() && !signer::goes_to(citations_scope.as_ref(), row) => {
                Self::TextOnly(text)
            }
            ContentPart::Reasoning {
                signature: Some(_),
                signature_scope,
                ..
            } if signer::goes_to(signature_scope.as_ref(), row) => Self::Whole,
            ContentPart::Reasoning { .. } => Self::Nothing,
            ContentPart::AnthropicBlock(b) if !b.goes_to(row) => Self::Nothing,
            ContentPart::Compaction(c) if !c.goes_to(row) => Self::Nothing,
            _ => Self::Whole,
        }
    }

    /// The citations of `p` the row is sent: a text's, where it goes whole.
    pub fn citations(p: &'a ContentPart, row: &Signer) -> &'a [Value] {
        match (p, Self::of(p, row)) {
            (ContentPart::Text { citations, .. }, Self::Whole) => citations,
            _ => &[],
        }
    }

    /// The name `x-lmgw-params-ignored` gives what the row is not sent of
    /// `p` ([`super::ignored_names`]' spelling), `None` where it leaves out
    /// nothing a client would miss: `content.citations` for a text's
    /// citations, `content.thinking` for a signed thinking block,
    /// `content.compaction` for a compaction block, `content.<type>` for a
    /// verbatim block.
    pub fn left_out_name(p: &'a ContentPart, row: &Signer) -> Option<String> {
        let sent = Self::of(p, row);
        match p {
            ContentPart::Text { citations, .. } if !citations.is_empty() && sent != Self::Whole => {
                Some("content.citations".into())
            }
            ContentPart::Reasoning {
                signature: Some(_), ..
            } if sent == Self::Nothing => Some("content.thinking".into()),
            ContentPart::AnthropicBlock(_) if sent == Self::Nothing => {
                super::ignored_names("content", [p]).into_iter().next()
            }
            ContentPart::Compaction(_) if sent == Self::Nothing => {
                Some("content.compaction".into())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cite() -> Value {
        json!({"type": "web_search_result_location", "encrypted_index": "Eo8B"})
    }

    #[test]
    fn a_text_s_citations_go_to_their_scope_and_a_client_s_everywhere() {
        let here = Signer::new("https://api.anthropic.com");
        let there = Signer::new("https://gateway.ai.cloudflare.com/v1/a/g/anthropic");
        let recorded = ContentPart::cited("KIWI.", vec![cite()], Some(here.clone()));
        assert_eq!(AnthropicSent::of(&recorded, &here), AnthropicSent::Whole);
        assert_eq!(AnthropicSent::citations(&recorded, &here), [cite()]);
        assert_eq!(AnthropicSent::left_out_name(&recorded, &here), None);
        assert_eq!(
            AnthropicSent::of(&recorded, &there),
            AnthropicSent::TextOnly("KIWI.")
        );
        assert!(AnthropicSent::citations(&recorded, &there).is_empty());
        assert_eq!(
            AnthropicSent::left_out_name(&recorded, &there).as_deref(),
            Some("content.citations")
        );
        let client = ContentPart::cited("KIWI.", vec![cite()], None);
        assert_eq!(AnthropicSent::of(&client, &there), AnthropicSent::Whole);
    }

    /// A compaction block lmgw read from an answer goes to its scope only;
    /// a client's goes everywhere.
    #[test]
    fn a_recorded_compaction_goes_to_its_scope_and_a_client_s_everywhere() {
        let here = Signer::new("https://api.anthropic.com");
        let there = Signer::new("https://gateway.ai.cloudflare.com/v1/a/g/anthropic");
        let block = json!({"type": "compaction", "content": "S", "encrypted_content": "E"});
        let recorded = ContentPart::Compaction(crate::ir::Compaction::answered(&block, &here));
        assert_eq!(AnthropicSent::of(&recorded, &here), AnthropicSent::Whole);
        assert_eq!(AnthropicSent::left_out_name(&recorded, &here), None);
        assert_eq!(AnthropicSent::of(&recorded, &there), AnthropicSent::Nothing);
        assert_eq!(
            AnthropicSent::left_out_name(&recorded, &there).as_deref(),
            Some("content.compaction")
        );
        let client = ContentPart::Compaction(crate::ir::Compaction::from_block(&block));
        assert_eq!(AnthropicSent::of(&client, &there), AnthropicSent::Whole);
    }

    /// Anthropic refuses an empty text block, so a citation without text
    /// is not sent, and named.
    #[test]
    fn an_empty_cited_text_is_not_sent_and_named() {
        let row = Signer::new("https://api.anthropic.com");
        let empty = ContentPart::cited("", vec![cite()], None);
        assert_eq!(AnthropicSent::of(&empty, &row), AnthropicSent::Nothing);
        assert_eq!(
            AnthropicSent::left_out_name(&empty, &row).as_deref(),
            Some("content.citations")
        );
        let plain = ContentPart::text("");
        assert_eq!(AnthropicSent::left_out_name(&plain, &row), None);
        let unsigned = ContentPart::reasoning("hmm");
        assert_eq!(AnthropicSent::of(&unsigned, &row), AnthropicSent::Nothing);
        assert_eq!(AnthropicSent::left_out_name(&unsigned, &row), None);
    }
}
