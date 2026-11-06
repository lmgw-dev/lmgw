//! Seals are the upstream's to read, never a page's, a device's or an
//! export's (WP8 review S-R2, decided): a stored tool turn's IR
//! (`chat_messages.ir_messages`) carries each reasoning part's `sealed`,
//! which goes back to the platform on the next turn, and every surface that
//! shows that record (`GET /chat/api/threads/{id}`, the chat feed, both
//! exports) shows it without them. The part's text stays.
//!
//! So do Anthropic's own opaque blobs, which the record keeps for the same
//! replay: a thinking part's `signature` (with its `signature_scope`) and a
//! verbatim block ([`crate::ir::AnthropicBlock`]: `redacted_thinking`'s
//! encrypted `data`, a block type lmgw does not read), which a surface
//! shows not at all, and a text's Anthropic `citations` with their
//! `citations_scope` (changed 2026-11-06: a web search citation's
//! `encrypted_index` is the provider's), the text kept, and a compaction
//! block's `signature`, `encrypted_content` and `scope` (changed
//! 2026-11-06), its summary kept.

use serde_json::Value;

/// The `type` of a verbatim Anthropic block's part in a record.
const ANTHROPIC_BLOCK_PART: &str = "anthropic_block";

/// `v` (an IR record as JSON) without any reasoning part's `sealed`,
/// `signature` and `signature_scope`, without any text part's `citations`
/// and `citations_scope`, without any compaction part's `signature`,
/// `encrypted_content` and `scope`, and without its verbatim Anthropic
/// blocks (module doc).
pub fn strip_seals(v: &mut Value) {
    match v {
        Value::Object(o) => {
            match o.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    o.remove("sealed");
                    o.remove("signature");
                    o.remove("signature_scope");
                }
                Some("text") => {
                    o.remove("citations");
                    o.remove("citations_scope");
                }
                Some(crate::ir::compaction::BLOCK_TYPE) => {
                    o.remove("scope");
                    if let Some(f) = o.get_mut("fields").and_then(Value::as_object_mut) {
                        f.remove("signature");
                        f.remove("encrypted_content");
                    }
                }
                _ => {}
            }
            o.values_mut().for_each(strip_seals);
        }
        Value::Array(a) => {
            a.retain(|p| p.get("type").and_then(Value::as_str) != Some(ANTHROPIC_BLOCK_PART));
            a.iter_mut().for_each(strip_seals);
        }
        _ => {}
    }
}

/// A stored record's text without its seals. A record that does not parse
/// (this process wrote every one) is none of lmgw's IR, so none of its
/// seals either: it is kept as it is, as the surfaces keep it.
pub fn unsealed_record(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(mut v) => {
            strip_seals(&mut v);
            v.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{AnthropicBlock, ContentPart, Message, Role, Seal, Signer};

    #[test]
    fn a_record_shows_its_reasoning_text_and_no_seal() {
        let record = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::sealed_reasoning(
                    "Use the tool.",
                    Seal::openai(&crate::ir::SealScope::new(4, "cfg"), "rs_1", "enc"),
                ),
                ContentPart::text("Checking."),
            ],
        }];
        let raw = serde_json::to_string(&record).unwrap();
        let shown = unsealed_record(&raw);
        assert!(
            !shown.contains("enc") && !shown.contains("sealed"),
            "{shown}"
        );
        let back: Vec<Message> = serde_json::from_str(&shown).unwrap();
        assert_eq!(back[0].content[0], ContentPart::reasoning("Use the tool."));
        assert_eq!(back[0].content[1], ContentPart::text("Checking."));
        assert_eq!(unsealed_record("not json"), "not json");
    }

    /// An Anthropic answer's signature and its verbatim blocks are the
    /// upstream's too, and so are its citations: the record keeps them for
    /// the replay, a surface shows the thinking text and the cited text
    /// alone.
    #[test]
    fn a_record_shows_no_signature_and_no_verbatim_block() {
        let row = Signer::new("https://api.anthropic.com");
        let record = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::signed_reasoning("Look it up.", "SIG-anthropic", Some(row.clone())),
                ContentPart::AnthropicBlock(AnthropicBlock::answered(
                    serde_json::json!({"type": "redacted_thinking", "data": "ENC-redacted"}),
                    &row,
                )),
                ContentPart::text("Checking."),
                ContentPart::cited(
                    "KIWI.",
                    vec![serde_json::json!({"type": "web_search_result_location",
                                            "encrypted_index": "ENC-index"})],
                    Some(row.clone()),
                ),
                ContentPart::Compaction(crate::ir::Compaction::answered(
                    &serde_json::json!({"type": "compaction", "content": "Summary.",
                                        "signature": "SIG-compaction",
                                        "encrypted_content": "ENC-compaction"}),
                    &row,
                )),
            ],
        }];
        let raw = serde_json::to_string(&record).unwrap();
        assert!(raw.contains("SIG-anthropic") && raw.contains("ENC-redacted"));
        assert!(raw.contains("ENC-index") && raw.contains("citations_scope"));
        assert!(raw.contains("SIG-compaction") && raw.contains("ENC-compaction"));
        let shown = unsealed_record(&raw);
        for blob in [
            "SIG-anthropic",
            "SIG-compaction",
            "ENC-compaction",
            "ENC-redacted",
            "ENC-index",
            "signature",
            "anthropic_block",
            "citations",
            "api.anthropic.com",
            "cfg5",
        ] {
            assert!(!shown.contains(blob), "{blob} in {shown}");
        }
        let back: Vec<Message> = serde_json::from_str(&shown).unwrap();
        assert_eq!(
            back[0].content,
            [
                ContentPart::reasoning("Look it up."),
                ContentPart::text("Checking."),
                ContentPart::text("KIWI."),
                ContentPart::Compaction(crate::ir::Compaction::from_block(
                    &serde_json::json!({"type": "compaction", "content": "Summary."})
                )),
            ]
        );
    }
}
