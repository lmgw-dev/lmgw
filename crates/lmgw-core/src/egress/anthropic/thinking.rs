//! Anthropic's `thinking` blocks in an answer, as the IR carries them: one
//! block is one [`ContentPart::Reasoning`] part with its `signature`, where
//! it stood among the answer's blocks, so a Messages client gets every block
//! Anthropic sent and can send it back signed. A block can have no text at
//! all: `thinking.display` `omitted`, the default on current Claude models
//! (Claude Haiku 5.5 thinks adaptively when a request names no `thinking`),
//! answers a block with an empty `thinking` and its signature, and the
//! output tokens it billed are that block's.
//!
//! The part carries the signer of the answer (`signature_scope`, the row's
//! scope, [`Signer`]): lmgw's own records send it to a row at that scope
//! only ([`crate::ir::signer::goes_to`]). A client's own block has none and
//! goes as the client wrote it.
//!
//! The streamed twin is the decoder's `signature_delta`
//! ([`StreamDelta::ReasoningSignature`](crate::ir::StreamDelta)).

use serde_json::Value;

use crate::ir::{ContentPart, Signer};

/// A `thinking` block's part from the provider `scope` names: its text and its
/// signature (an empty one reads as none). A block with neither carries
/// nothing and has none.
pub(super) fn part(block: &Value, scope: &Signer) -> Option<ContentPart> {
    let text = block
        .get("thinking")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let signature = block
        .get("signature")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match signature {
        Some(sig) => Some(ContentPart::signed_reasoning(
            text,
            sig,
            Some(scope.clone()),
        )),
        None => (!text.is_empty()).then(|| ContentPart::reasoning(text)),
    }
}

/// Whether any of `content`'s reasoning parts is signed. An answer without
/// one keeps the shape it always had: its trace in `Completion::reasoning`
/// alone, no reasoning parts in place (as the OpenAI platform's decoder
/// keeps an answer without a seal).
pub(super) fn signed(content: &[ContentPart]) -> bool {
    content.iter().any(|p| {
        matches!(
            p,
            ContentPart::Reasoning {
                signature: Some(_),
                ..
            }
        )
    })
}
