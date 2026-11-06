//! What a request's render left out because another scope signed it
//! (gateway design §6.1, "Where a signature goes", changed 2026-11-06): a
//! thinking block, verbatim block, compaction block (its summary goes as
//! text, folded) or text's citations lmgw read from an answer at another
//! scope, which this row's server cannot read ([`crate::ir::signer`],
//! [`crate::ir::AnthropicSent`]), and a citation without text, which
//! Anthropic refuses as an empty text block. Logged once per request at
//! INFO ([`LeftOut::log`]); the handler names the same parts in
//! `x-lmgw-params-ignored` ([`crate::ir::ChatRequest::blocks_ignored`]).
//!
//! The final assistant message's leading thinking can be among them, in a
//! trailing tool loop: the request still goes out as it is, thinking control
//! unchanged. Anthropic accepts a tool loop whose assistant turn has no
//! thinking block (probed 2026-11-06 on Claude Haiku 5.5 and Claude Sonnet
//! 5.5 with adaptive thinking, and on Claude Haiku 4.5 with a manual
//! `budget_tokens`, where it answers that turn with thinking off instead of
//! refusing), while `thinking: {"type": "disabled"}` is itself refused by
//! Claude Opus 5.5 and Claude Sonnet 5.5. The log line says so.

use crate::ir::{AnthropicBlock, Signer};

/// The tally (module doc).
#[derive(Debug, Default)]
pub(super) struct LeftOut {
    thinking: usize,
    /// Texts sent without their citations.
    citations: usize,
    /// Compaction blocks folded in as their summary.
    compactions: usize,
    /// Cited texts with no text, not sent with their citations (whoever
    /// answered them).
    textless: usize,
    /// The final assistant message's leading thinking was among them.
    trailing: bool,
    /// The message being rendered is the request's last assistant message.
    pub(super) in_last_assistant: bool,
    blocks: Vec<String>,
    scopes: Vec<String>,
}

impl LeftOut {
    /// A thinking block signed at `signer`, `leading` when no block of its
    /// message went before it.
    pub(super) fn thinking(&mut self, signer: Option<&Signer>, leading: bool) {
        self.thinking += 1;
        self.trailing |= self.in_last_assistant && leading;
        self.scope(signer);
    }

    /// A verbatim block another scope answered with.
    pub(super) fn block(&mut self, b: &AnthropicBlock) {
        let kind = b.kind().to_string();
        if !self.blocks.contains(&kind) {
            self.blocks.push(kind);
        }
        self.scope(b.scope.as_ref());
    }

    /// A text's citations another scope answered with; the text was sent.
    pub(super) fn citations(&mut self, signer: Option<&Signer>) {
        self.citations += 1;
        self.scope(signer);
    }

    /// A compaction block another scope answered with, folded in.
    pub(super) fn compaction(&mut self, signer: Option<&Signer>) {
        self.compactions += 1;
        self.scope(signer);
    }

    /// A cited text with no text, left out with its citations.
    pub(super) fn textless(&mut self) {
        self.textless += 1;
    }

    fn scope(&mut self, signer: Option<&Signer>) {
        let o = signer.map(Signer::as_str).unwrap_or_default().to_string();
        if !self.scopes.contains(&o) {
            self.scopes.push(o);
        }
    }

    /// The request's one INFO line, when anything was left out.
    pub(super) fn log(&self, row: &Signer) {
        let signed = self.thinking + self.citations + self.compactions + self.blocks.len();
        if signed == 0 {
            if self.textless > 0 {
                tracing::info!(
                    textless_citations = self.textless,
                    row = %row.as_str(),
                    "left out citations whose text is empty: Anthropic refuses an empty text \
                     block"
                );
            }
            return;
        }
        if self.trailing {
            tracing::info!(
                thinking = self.thinking,
                citations = self.citations,
                compactions = self.compactions,
                textless_citations = self.textless,
                blocks = %self.blocks.join(","),
                signed_at = %self.scopes.join(","),
                row = %row.as_str(),
                "left out Anthropic blocks another scope signed, the trailing tool loop's \
                 thinking among them: this row cannot read them, so the model goes on with the \
                 tool loop without that reasoning (with a manual budget, Anthropic runs this \
                 turn with thinking off)"
            );
        } else {
            tracing::info!(
                thinking = self.thinking,
                citations = self.citations,
                compactions = self.compactions,
                textless_citations = self.textless,
                blocks = %self.blocks.join(","),
                signed_at = %self.scopes.join(","),
                row = %row.as_str(),
                "left out Anthropic blocks another scope signed: this row cannot read them, so \
                 the model answers without that reasoning, the texts go without their \
                 citations and a compaction as its summary"
            );
        }
    }
}
